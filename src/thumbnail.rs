use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::{process, thread};

use anyhow::{bail, Context as _};
use niri_config::{BackgroundEffect, BlockOutFrom, Blur, CornerRadius};
use smithay::desktop::PopupManager;
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Resource as _;
use smithay::wayland::compositor::{with_surface_tree_downward, TraversalAction};

use crate::layout::LayoutElement;
use crate::utils::write_png_rgba8;
use crate::window::Mapped;

const THUMBNAIL_WORKER_QUEUE_CAPACITY: usize = 16;

/// Hard cap on pending (not yet captured) thumbnail requests on the main loop.
///
/// The main loop performs at most one capture per event-loop iteration, so the
/// number of captures that can be waiting at any point is bounded by this plus
/// the single in-flight capture. Overflowing requests are rejected with an
/// explicit error so every accepted request is still served.
pub(crate) const MAX_PENDING_THUMBNAIL_REQUESTS: usize = 8;

/// Hard cap on in-memory cached capture results (number of entries).
pub(crate) const MAX_THUMBNAIL_CACHE_ENTRIES: usize = 32;

/// Hard cap on in-memory cached capture results (total pixel bytes).
pub(crate) const MAX_THUMBNAIL_CACHE_BYTES: usize = 64 * 1024 * 1024;

/// Bounds for the per-window content-epoch map: dead roots are pruned only
/// once the map grows past this many entries, keeping per-commit cost O(1)
/// amortized while the map stays memory-bounded.
pub(crate) const MAX_THUMBNAIL_EPOCH_ENTRIES: usize = 2048;

pub(crate) type ThumbnailReply = Result<niri_ipc::WindowThumbnail, String>;

pub(crate) struct ThumbnailCapture {
    pub window_id: u64,
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub pixels: Arc<Vec<u8>>,
}

/// One accepted thumbnail request (after deduplication, possibly carrying the
/// reply channels of several identical requests).
pub(crate) struct ThumbnailRequest {
    pub window_id: u64,
    pub path: PathBuf,
    pub max_width: u32,
    pub max_height: u32,
    pub replies: Vec<async_channel::Sender<ThumbnailReply>>,
}

/// Content version of a window at capture time.
///
/// A fresh capture is pixel-identical to the cached one iff the version is
/// unchanged: the version covers every input of the capture render —
/// [`ThumbnailVersion::content_epoch`] advances on every commit of any
/// surface in the window tree (toplevel, subsurfaces, popups) and on every
/// surface destruction, the surface set and per-popup render positions change
/// on popup/subsurface add/remove and on non-commit repositioning (IME
/// input-method popups), the output fractional scale decides the capture
/// resolution, and the window rules decide the rendered alpha, block-out
/// replacement, popup alpha, popup background effect and blur parameters.
/// Rule-driven inputs change without any buffer commit, which is why they are
/// part of the version. The optional-value folds are single-injective: `None`
/// and an explicit zero fold differently because the render path
/// distinguishes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ThumbnailVersion {
    /// Content epoch of the window's root surface (see
    /// [`crate::niri::Niri::bump_thumbnail_content_epoch`]).
    content_epoch: u64,
    /// FNV-1a-style fold of the wl_surface ids in the captured tree
    /// (toplevel + subsurfaces + popups): changes when a surface is added to
    /// or removed from the tree even if no commit followed.
    surface_set: u64,
    output_scale_bits: u64,
    alpha_bits: u32,
    /// Discriminant of the block-out rule (None/Screencast/ScreenCapture): a
    /// ScreenCapture block-out rule replaces the window with a solid block in
    /// the thumbnail.
    block_out_from: u8,
    /// Popup opacity rule bits: changes the captured popup alpha without any
    /// buffer commit.
    popup_opacity_bits: u64,
    /// Fold of the popup background-effect rules and the popup corner radius:
    /// both are rendered into the thumbnail and change without any commit.
    popup_effect_bits: u64,
    /// Fold of the window blur config: popup glass renders with it and it
    /// changes on config reload without any commit.
    blur_config_bits: u64,
}

/// The alpha the thumbnail capture renders with, mirrored from the render
/// path so the cache version and the render use exactly the same input.
pub(crate) fn thumbnail_alpha(mapped: &Mapped) -> f32 {
    if mapped.sizing_mode().is_fullscreen() || mapped.is_ignoring_opacity_window_rule() {
        1.
    } else {
        mapped.rules().opacity.unwrap_or(1.).clamp(0., 1.)
    }
}

fn fold_surface_id(set: &mut u64, surface: &WlSurface) {
    *set = set
        .wrapping_add(u64::from(surface.id().protocol_id()))
        .wrapping_mul(0x9E37_79B9_7F4A_7C15);
}

fn fold_u64(set: &mut u64, value: u64) {
    *set = set.wrapping_add(value).wrapping_mul(0x9E37_79B9_7F4A_7C15);
}

fn fold_bool(set: &mut u64, value: bool) {
    fold_u64(set, u64::from(value));
}

fn fold_f32(set: &mut u64, value: f32) {
    fold_u64(set, u64::from(value.to_bits()));
}

fn fold_f64(set: &mut u64, value: f64) {
    fold_u64(set, value.to_bits());
}

/// Single-injective fold for an optional float: `None` and `Some(0.0)` must
/// fold differently because the render path distinguishes them (e.g. popup
/// opacity `None` means fully opaque while `Some(0.0)` is fully transparent,
/// and a `contrast`/`saturation` of `Some(0.0)` visibly differs from unset).
fn fold_option_f32(set: &mut u64, value: Option<f32>) {
    fold_u64(
        set,
        value.map_or(0, |v| u64::from(v.to_bits()).wrapping_add(1)),
    );
}

fn fold_option_f64(set: &mut u64, value: Option<f64>) {
    fold_u64(set, value.map_or(0, |v| v.to_bits().wrapping_add(1)));
}

fn fold_background_effect(set: &mut u64, effect: &BackgroundEffect) {
    fold_u64(set, effect.xray.map_or(0, |v| u64::from(v) + 1));
    fold_u64(set, effect.blur.map_or(0, |v| u64::from(v) + 1));
    fold_option_f64(set, effect.noise);
    fold_option_f64(set, effect.saturation);
    fold_option_f64(set, effect.contrast);
    fold_option_f64(set, effect.tint_amount);
    fold_option_f64(set, effect.edge_highlight);
    fold_option_f64(set, effect.refraction);
    fold_option_f64(set, effect.inner_shadow);
    fold_option_f64(set, effect.chromatic);
    fold_option_f64(set, effect.lens_depth);
    if let Some(color) = &effect.tint_color {
        fold_f32(set, color.r);
        fold_f32(set, color.g);
        fold_f32(set, color.b);
        fold_f32(set, color.a);
    }
}

fn fold_corner_radius(set: &mut u64, radius: &CornerRadius) {
    fold_f32(set, radius.top_left);
    fold_f32(set, radius.top_right);
    fold_f32(set, radius.bottom_right);
    fold_f32(set, radius.bottom_left);
}

fn fold_blur_config(set: &mut u64, blur: &Blur) {
    fold_bool(set, blur.off);
    fold_u64(set, u64::from(blur.passes));
    fold_f64(set, blur.offset);
    fold_f64(set, blur.noise);
    fold_f64(set, blur.saturation);
}

/// Compute the content version of a window for thumbnail cache decisions.
pub(crate) fn thumbnail_content_version(
    mapped: &Mapped,
    output: &Output,
    content_epoch: u64,
) -> ThumbnailVersion {
    let mut surface_set = 0;
    let surface = mapped.toplevel().wl_surface();
    fold_surface_id(&mut surface_set, surface);
    with_surface_tree_downward(
        surface,
        (),
        |_, _, _| TraversalAction::DoChildren(()),
        |child, _, _| fold_surface_id(&mut surface_set, child),
        |_, _, _| true,
    );
    // Popups are part of the captured content (the capture renders them), so
    // their presence in the tree is part of the version. The render position
    // of each popup is folded in as well: IME input-method popups reposition
    // through `set_location` without any wl_surface commit (unlike xdg popups,
    // whose moves go through configure+commit and are covered by the epoch),
    // so the position must be its own version input.
    for (popup, offset) in PopupManager::popups_for_surface(surface) {
        fold_surface_id(&mut surface_set, &popup.wl_surface());
        let geometry = popup.geometry();
        fold_u64(&mut surface_set, (offset.x - geometry.loc.x) as u64);
        fold_u64(&mut surface_set, (offset.y - geometry.loc.y) as u64);
    }

    let scale = output.current_scale().fractional_scale();
    let alpha = thumbnail_alpha(mapped);
    let block_out_from = match mapped.rules().block_out_from {
        None => 0,
        Some(BlockOutFrom::Screencast) => 1,
        Some(BlockOutFrom::ScreenCapture) => 2,
    };
    let popup_rules = &mapped.rules().popups;
    let mut popup_effect_bits = 0;
    fold_background_effect(&mut popup_effect_bits, &popup_rules.background_effect);
    if let Some(radius) = popup_rules.geometry_corner_radius {
        fold_corner_radius(&mut popup_effect_bits, &radius);
    }
    let mut blur_config_bits = 0;
    fold_blur_config(&mut blur_config_bits, &mapped.blur_config());
    let mut popup_opacity_bits = 0;
    fold_option_f32(&mut popup_opacity_bits, popup_rules.opacity);

    ThumbnailVersion {
        content_epoch,
        surface_set,
        output_scale_bits: scale.to_bits(),
        alpha_bits: alpha.to_bits(),
        block_out_from,
        popup_opacity_bits,
        popup_effect_bits,
        blur_config_bits,
    }
}

type ThumbnailCacheKey = (u64, u32, u32);

struct ThumbnailCacheEntry {
    version: ThumbnailVersion,
    width: u32,
    height: u32,
    pixels: Arc<Vec<u8>>,
    last_used: u64,
}

/// Main-loop budget for thumbnail captures: bounded latest-wins request queue
/// plus a bounded, content-versioned capture cache.
///
/// Owned by [`crate::niri::Niri`] and only touched on the main loop; the GPU
/// readback and the cache hits are driven one per event-loop iteration by the
/// capture idle (`Niri::run_thumbnail_capture`).
pub(crate) struct ThumbnailRequestQueue {
    pending: VecDeque<ThumbnailRequest>,
    cache: HashMap<ThumbnailCacheKey, ThumbnailCacheEntry>,
    cache_clock: u64,
    capture_scheduled: bool,
    max_pending: usize,
    max_cache_entries: usize,
    max_cache_bytes: usize,
}

impl ThumbnailRequestQueue {
    pub fn new(max_pending: usize, max_cache_entries: usize, max_cache_bytes: usize) -> Self {
        Self {
            pending: VecDeque::new(),
            cache: HashMap::new(),
            cache_clock: 0,
            capture_scheduled: false,
            max_pending,
            max_cache_entries,
            max_cache_bytes,
        }
    }

    /// Submit a request, deduplicating identical requests and enforcing the
    /// bounded latest-wins queue.
    ///
    /// - An identical pending request (same window, path and max size) is merged: its replies are
    ///   appended and only one capture happens.
    /// - A newer request for the same window and path with different max size supersedes the older
    ///   pending one (latest-wins); the superseded request fails with an explicit error.
    /// - When the queue is full, the new request is rejected with an explicit error (fail-fast
    ///   backpressure); every accepted request is served.
    pub fn submit(&mut self, request: ThumbnailRequest) {
        for entry in &mut self.pending {
            if entry.window_id == request.window_id
                && entry.path == request.path
                && entry.max_width == request.max_width
                && entry.max_height == request.max_height
            {
                entry.replies.extend(request.replies);
                return;
            }
        }

        for entry in &mut self.pending {
            if entry.window_id == request.window_id && entry.path == request.path {
                for reply in entry.replies.drain(..) {
                    let _ = reply.try_send(Err(String::from(
                        "thumbnail request superseded by a newer request for the same window",
                    )));
                }
                entry.max_width = request.max_width;
                entry.max_height = request.max_height;
                entry.replies = request.replies;
                return;
            }
        }

        if self.pending.len() >= self.max_pending {
            for reply in request.replies {
                let _ = reply.try_send(Err(String::from("thumbnail request queue is full")));
            }
            return;
        }

        self.pending.push_back(request);
    }

    /// Pop the oldest pending request (FIFO).
    pub fn pop_pending(&mut self) -> Option<ThumbnailRequest> {
        self.pending.pop_front()
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    #[cfg(test)]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn capture_scheduled(&self) -> bool {
        self.capture_scheduled
    }

    pub fn set_capture_scheduled(&mut self, scheduled: bool) {
        self.capture_scheduled = scheduled;
    }

    /// Drop all requests and cache entries for a window (window destroyed or
    /// unmapped); pending requests fail with the legacy "window not found"
    /// error so the request/response contract is preserved.
    pub fn cancel_window(&mut self, window_id: u64) {
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i].window_id == window_id {
                let request = self.pending.remove(i).expect("index in bounds");
                for reply in request.replies {
                    let _ = reply.try_send(Err(format!(
                        "window not found or not on an output: {window_id}"
                    )));
                }
            } else {
                i += 1;
            }
        }
        self.cache.retain(|key, _| key.0 != window_id);
    }

    /// Return a capture from the cache when the content version still matches.
    pub(crate) fn cache_get(
        &mut self,
        window_id: u64,
        max_width: u32,
        max_height: u32,
        version: ThumbnailVersion,
        path: &Path,
    ) -> Option<ThumbnailCapture> {
        let key = (window_id, max_width, max_height);
        let entry = self.cache.get_mut(&key)?;
        if entry.version != version {
            // Stale: the content changed, the cached pixels would be wrong.
            self.cache.remove(&key);
            return None;
        }
        entry.last_used = self.cache_clock;
        self.cache_clock = self.cache_clock.wrapping_add(1);
        Some(ThumbnailCapture {
            window_id,
            path: path.to_owned(),
            width: entry.width,
            height: entry.height,
            pixels: entry.pixels.clone(),
        })
    }

    /// Store a capture in the cache, evicting least-recently-used entries
    /// until the byte and entry caps are satisfied.
    pub(crate) fn cache_put(
        &mut self,
        window_id: u64,
        max_width: u32,
        max_height: u32,
        version: ThumbnailVersion,
        width: u32,
        height: u32,
        pixels: Arc<Vec<u8>>,
    ) {
        self.cache.insert(
            (window_id, max_width, max_height),
            ThumbnailCacheEntry {
                version,
                width,
                height,
                pixels,
                last_used: self.cache_clock,
            },
        );
        self.cache_clock = self.cache_clock.wrapping_add(1);
        self.evict_cache();
    }

    fn evict_cache(&mut self) {
        loop {
            let bytes: usize = self
                .cache
                .values()
                .map(|entry| entry.width as usize * entry.height as usize * 4)
                .sum();
            if self.cache.len() <= self.max_cache_entries && bytes <= self.max_cache_bytes {
                return;
            }
            let Some((key, _)) = self.cache.iter().min_by_key(|(_, entry)| entry.last_used) else {
                return;
            };
            let key = *key;
            self.cache.remove(&key);
        }
    }

    #[cfg(test)]
    pub(crate) fn cache_len(&self) -> usize {
        self.cache.len()
    }

    #[cfg(test)]
    pub(crate) fn cache_bytes(&self) -> usize {
        self.cache
            .values()
            .map(|entry| entry.width as usize * entry.height as usize * 4)
            .sum()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Publication {
    generation: u64,
    window_id: u64,
    cancelled: bool,
}

struct PublishJob {
    capture: ThumbnailCapture,
    publication: Publication,
    previous_publication: Option<Publication>,
    replies: Vec<async_channel::Sender<ThumbnailReply>>,
}

enum WorkerMessage {
    Publish(PublishJob),
    Wake,
}

pub(crate) struct ThumbnailPublisher {
    sender: SyncSender<WorkerMessage>,
    publications: Arc<Mutex<HashMap<PathBuf, Publication>>>,
    next_generation: u64,
}

impl ThumbnailPublisher {
    pub fn new() -> Self {
        let (sender, receiver) = mpsc::sync_channel(THUMBNAIL_WORKER_QUEUE_CAPACITY);
        let publications = Arc::new(Mutex::new(HashMap::new()));
        let worker_publications = publications.clone();

        thread::Builder::new()
            .name(String::from("niri-thumbnail-publisher"))
            .spawn(move || worker_loop(receiver, worker_publications))
            .expect("error starting thumbnail publisher thread");

        Self {
            sender,
            publications,
            next_generation: 0,
        }
    }

    pub fn publish(
        &mut self,
        capture: ThumbnailCapture,
        replies: Vec<async_channel::Sender<ThumbnailReply>>,
    ) {
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let publication = Publication {
            generation: self.next_generation,
            window_id: capture.window_id,
            cancelled: false,
        };
        let previous_publication = self
            .publications
            .lock()
            .unwrap()
            .insert(capture.path.clone(), publication);

        let job = PublishJob {
            capture,
            publication,
            previous_publication,
            replies,
        };
        match self.sender.try_send(WorkerMessage::Publish(job)) {
            Ok(()) => (),
            Err(TrySendError::Full(WorkerMessage::Publish(job))) => {
                restore_previous_publication_if_current(
                    &self.publications,
                    &job.capture.path,
                    job.publication,
                    job.previous_publication,
                );
                for reply in &job.replies {
                    let _ = reply.try_send(Err(String::from("thumbnail publisher queue is full")));
                }
            }
            Err(TrySendError::Disconnected(WorkerMessage::Publish(job))) => {
                restore_previous_publication_if_current(
                    &self.publications,
                    &job.capture.path,
                    job.publication,
                    job.previous_publication,
                );
                for reply in &job.replies {
                    let _ = reply.try_send(Err(String::from("thumbnail publisher is unavailable")));
                }
            }
            Err(
                TrySendError::Full(WorkerMessage::Wake)
                | TrySendError::Disconnected(WorkerMessage::Wake),
            ) => unreachable!(),
        }
    }

    pub fn cancel_window(&self, window_id: u64) {
        let mut publications = self.publications.lock().unwrap();
        for publication in publications.values_mut() {
            if publication.window_id == window_id {
                publication.cancelled = true;
            }
        }
        drop(publications);

        let _ = self.sender.try_send(WorkerMessage::Wake);
    }
}

fn worker_loop(
    receiver: mpsc::Receiver<WorkerMessage>,
    publications: Arc<Mutex<HashMap<PathBuf, Publication>>>,
) {
    while let Ok(message) = receiver.recv() {
        if let WorkerMessage::Publish(job) = message {
            publish_thumbnail(job, &publications);
        }
        cleanup_cancelled_publications(&publications);
    }
}

fn publish_thumbnail(job: PublishJob, publications: &Arc<Mutex<HashMap<PathBuf, Publication>>>) {
    if job.replies.iter().all(|reply| reply.is_closed()) {
        restore_previous_publication_if_current(
            publications,
            &job.capture.path,
            job.publication,
            job.previous_publication,
        );
        return;
    }

    let result = encode_and_publish(&job, publications).map(|()| niri_ipc::WindowThumbnail {
        path: job.capture.path.to_string_lossy().into_owned(),
        width: job.capture.width,
        height: job.capture.height,
    });
    let result = result.map_err(|err| err.to_string());
    let mut any_reply_delivered = false;
    for reply in &job.replies {
        if reply.send_blocking(result.clone()).is_ok() {
            any_reply_delivered = true;
        }
    }
    if !any_reply_delivered {
        remove_published_publication_if_current(publications, &job.capture.path, job.publication);
    }
}

fn encode_and_publish(
    job: &PublishJob,
    publications: &Arc<Mutex<HashMap<PathBuf, Publication>>>,
) -> anyhow::Result<()> {
    let path = &job.capture.path;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .context("thumbnail path has no parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("error creating thumbnail directory {parent:?}"))?;

    let (temporary_path, file) = create_temporary_file(path, job.publication.generation)?;
    let result = (|| {
        let mut writer = BufWriter::new(file);
        write_png_rgba8(
            &mut writer,
            job.capture.width,
            job.capture.height,
            &job.capture.pixels,
        )
        .with_context(|| format!("error encoding thumbnail PNG {path:?}"))?;
        writer
            .flush()
            .with_context(|| format!("error flushing thumbnail PNG {temporary_path:?}"))?;
        let file = writer
            .into_inner()
            .map_err(|err| err.into_error())
            .with_context(|| format!("error finalizing thumbnail PNG {temporary_path:?}"))?;
        file.sync_all()
            .with_context(|| format!("error syncing thumbnail PNG {temporary_path:?}"))?;

        if job.replies.iter().all(|reply| reply.is_closed()) {
            bail!("thumbnail request was cancelled");
        }

        let current = publications.lock().unwrap();
        if current.get(path) != Some(&job.publication) || job.publication.cancelled {
            bail!("thumbnail publication was superseded or cancelled");
        }
        drop(current);
        std::fs::rename(&temporary_path, path).with_context(|| {
            format!("error atomically publishing thumbnail {temporary_path:?} to {path:?}")
        })?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("error syncing thumbnail directory {parent:?}"))?;
        Ok(())
    })();

    if result.is_err() {
        let _ = std::fs::remove_file(&temporary_path);
        restore_previous_publication_if_current(
            publications,
            path,
            job.publication,
            job.previous_publication,
        );
    }
    result
}

fn create_temporary_file(path: &Path, generation: u64) -> anyhow::Result<(PathBuf, File)> {
    let parent = path.parent().context("thumbnail path has no parent")?;
    let file_name = path
        .file_name()
        .context("thumbnail path has no file name")?
        .to_string_lossy();

    for attempt in 0..128_u32 {
        let temporary_path = parent.join(format!(
            ".{file_name}.niri-{}-{generation}-{attempt}.tmp",
            process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
        {
            Ok(file) => return Ok((temporary_path, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err).with_context(|| {
                    format!("error creating thumbnail temporary file {temporary_path:?}")
                });
            }
        }
    }

    bail!("could not allocate a unique temporary file for {path:?}")
}

fn restore_previous_publication_if_current(
    publications: &Arc<Mutex<HashMap<PathBuf, Publication>>>,
    path: &Path,
    publication: Publication,
    previous_publication: Option<Publication>,
) {
    let mut current = publications.lock().unwrap();
    if current.get(path) != Some(&publication) {
        return;
    }
    if let Some(previous_publication) = previous_publication {
        current.insert(path.to_owned(), previous_publication);
    } else {
        current.remove(path);
    }
}

fn remove_published_publication_if_current(
    publications: &Arc<Mutex<HashMap<PathBuf, Publication>>>,
    path: &Path,
    publication: Publication,
) {
    let mut current = publications.lock().unwrap();
    if current.get(path) != Some(&publication) {
        return;
    }
    current.remove(path);
    drop(current);

    match std::fs::remove_file(path) {
        Ok(()) => (),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (),
        Err(err) => warn!("error removing disconnected thumbnail {path:?}: {err:?}"),
    }
}

fn cleanup_cancelled_publications(publications: &Arc<Mutex<HashMap<PathBuf, Publication>>>) {
    let mut current = publications.lock().unwrap();
    let cancelled: Vec<_> = current
        .iter()
        .filter(|(_, publication)| publication.cancelled)
        .map(|(path, _)| path.clone())
        .collect();
    for path in &cancelled {
        current.remove(path);
    }
    drop(current);

    for path in cancelled {
        match std::fs::remove_file(&path) {
            Ok(()) => (),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => (),
            Err(err) => warn!("error removing cancelled thumbnail {path:?}: {err:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::BufReader;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    use super::*;

    static TEST_DIRECTORY_ID: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let id = TEST_DIRECTORY_ID.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("niri-thumbnail-test-{}-{id}", process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn decode_rgba(path: &Path) -> anyhow::Result<Vec<u8>> {
        let decoder = png::Decoder::new(BufReader::new(File::open(path)?));
        let mut reader = decoder.read_info()?;
        let mut buffer = vec![
            0;
            reader
                .output_buffer_size()
                .context("invalid PNG buffer size")?
        ];
        let info = reader.next_frame(&mut buffer)?;
        buffer.truncate(info.buffer_size());
        Ok(buffer)
    }

    fn reply_channel() -> (
        async_channel::Sender<ThumbnailReply>,
        async_channel::Receiver<ThumbnailReply>,
    ) {
        async_channel::bounded(1)
    }

    fn request(window_id: u64, path: &Path, max_width: u32, max_height: u32) -> ThumbnailRequest {
        let (reply, _) = reply_channel();
        ThumbnailRequest {
            window_id,
            path: path.to_owned(),
            max_width,
            max_height,
            replies: vec![reply],
        }
    }

    fn queue_with_small_cache() -> ThumbnailRequestQueue {
        // Small caps so cache eviction is reachable in tests.
        ThumbnailRequestQueue::new(
            MAX_PENDING_THUMBNAIL_REQUESTS,
            MAX_THUMBNAIL_CACHE_ENTRIES,
            8 * 1024,
        )
    }

    fn sample_version() -> ThumbnailVersion {
        ThumbnailVersion {
            content_epoch: 1,
            surface_set: 0xDEAD_BEEF,
            output_scale_bits: 1_f64.to_bits(),
            alpha_bits: 1_f32.to_bits(),
            block_out_from: 0,
            popup_opacity_bits: 0,
            popup_effect_bits: 0,
            blur_config_bits: 0,
        }
    }

    #[test]
    fn option_folds_distinguish_none_from_zero() {
        // The render path distinguishes "unset" from an explicit 0.0 (popup
        // opacity None = fully opaque vs Some(0.0) = fully transparent;
        // contrast/saturation None = no effect vs Some(0.0) = visible
        // effect), so the version fold must too.
        let mut f32_none = 0;
        fold_option_f32(&mut f32_none, None);
        let mut f32_zero = 0;
        fold_option_f32(&mut f32_zero, Some(0.0));
        assert_ne!(
            f32_none, f32_zero,
            "None must fold differently from Some(0.0)"
        );
        let mut f32_one = 0;
        fold_option_f32(&mut f32_one, Some(1.0));
        assert_ne!(f32_none, f32_one);
        assert_ne!(f32_zero, f32_one);

        let mut f64_none = 0;
        fold_option_f64(&mut f64_none, None);
        let mut f64_zero = 0;
        fold_option_f64(&mut f64_zero, Some(0.0));
        assert_ne!(
            f64_none, f64_zero,
            "None must fold differently from Some(0.0)"
        );
        let mut f64_one = 0;
        fold_option_f64(&mut f64_one, Some(1.0));
        assert_ne!(f64_none, f64_one);
        assert_ne!(f64_zero, f64_one);
        // The option-bool fold already distinguishes None/Some(false)/Some(true).
        let mut none = 0;
        fold_u64(&mut none, None.map_or(0, |v: bool| u64::from(v) + 1));
        let mut false_ = 0;
        fold_u64(&mut false_, Some(false).map_or(0, |v| u64::from(v) + 1));
        let mut true_ = 0;
        fold_u64(&mut true_, Some(true).map_or(0, |v| u64::from(v) + 1));
        assert_ne!(none, false_);
        assert_ne!(false_, true_);
    }

    #[test]
    fn atomic_publisher_produces_1000_decodable_replacements() {
        let directory = TestDirectory::new();
        let path = directory.0.join("window-42.png");
        let mut publisher = ThumbnailPublisher::new();
        let reader_path = path.clone();
        let reader_stop = Arc::new(AtomicBool::new(false));
        let reader_stop_ = reader_stop.clone();
        let decode_count = Arc::new(AtomicU64::new(0));
        let decode_count_ = decode_count.clone();
        let decode_failures = Arc::new(AtomicU64::new(0));
        let decode_failures_ = decode_failures.clone();

        let first_pixels = vec![0, 0, 0x7f, 0xff];
        let (first_reply, first_result) = reply_channel();
        publisher.publish(
            ThumbnailCapture {
                window_id: 42,
                path: path.clone(),
                width: 1,
                height: 1,
                pixels: Arc::new(first_pixels.clone()),
            },
            vec![first_reply],
        );
        first_result.recv_blocking().unwrap().unwrap();
        assert_eq!(decode_rgba(&path).unwrap(), first_pixels);

        let reader_thread = thread::spawn(move || {
            while !reader_stop_.load(Ordering::Relaxed) {
                match decode_rgba(&reader_path) {
                    Ok(buffer) if buffer.len() == 4 => {
                        decode_count_.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(_) | Err(_) => {
                        decode_failures_.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        });

        for generation in 1..1000_u32 {
            let pixels = vec![generation as u8, (generation >> 8) as u8, 0x7f, 0xff];
            let (reply, result) = reply_channel();
            publisher.publish(
                ThumbnailCapture {
                    window_id: 42,
                    path: path.clone(),
                    width: 1,
                    height: 1,
                    pixels: Arc::new(pixels.clone()),
                },
                vec![reply],
            );
            result.recv_blocking().unwrap().unwrap();
            assert_eq!(decode_rgba(&path).unwrap(), pixels);
        }

        reader_stop.store(true, Ordering::Relaxed);
        reader_thread.join().unwrap();
        assert!(decode_count.load(Ordering::Relaxed) > 0);
        assert_eq!(decode_failures.load(Ordering::Relaxed), 0);
        assert_eq!(std::fs::read_dir(&directory.0).unwrap().count(), 1);
    }

    #[test]
    fn stale_generation_cannot_replace_current_publication() {
        let directory = TestDirectory::new();
        let path = directory.0.join("window-7.png");
        let publications = Arc::new(Mutex::new(HashMap::new()));
        let old = Publication {
            generation: 1,
            window_id: 7,
            cancelled: false,
        };
        let current = Publication {
            generation: 2,
            window_id: 7,
            cancelled: false,
        };
        publications.lock().unwrap().insert(path.clone(), current);

        let (old_reply, _old_result) = reply_channel();
        let old_job = PublishJob {
            capture: ThumbnailCapture {
                window_id: 7,
                path: path.clone(),
                width: 1,
                height: 1,
                pixels: Arc::new(vec![0xff, 0, 0, 0xff]),
            },
            publication: old,
            previous_publication: None,
            replies: vec![old_reply],
        };
        assert!(encode_and_publish(&old_job, &publications).is_err());
        assert!(!path.exists());

        let (current_reply, _current_result) = reply_channel();
        let current_job = PublishJob {
            capture: ThumbnailCapture {
                window_id: 7,
                path: path.clone(),
                width: 1,
                height: 1,
                pixels: Arc::new(vec![0, 0xff, 0, 0xff]),
            },
            publication: current,
            previous_publication: None,
            replies: vec![current_reply],
        };
        encode_and_publish(&current_job, &publications).unwrap();
        assert_eq!(decode_rgba(&path).unwrap(), vec![0, 0xff, 0, 0xff]);
    }

    #[test]
    fn failed_refresh_restores_previous_publication_token() {
        let directory = TestDirectory::new();
        let path = directory.0.join("window-8.png");
        let publications = Arc::new(Mutex::new(HashMap::new()));
        let previous = Publication {
            generation: 1,
            window_id: 8,
            cancelled: false,
        };
        publications.lock().unwrap().insert(path.clone(), previous);
        std::fs::write(&path, b"previous").unwrap();

        let refresh = Publication {
            generation: 2,
            window_id: 8,
            cancelled: false,
        };
        publications.lock().unwrap().insert(path.clone(), refresh);
        let (reply, _result) = reply_channel();
        let job = PublishJob {
            capture: ThumbnailCapture {
                window_id: 8,
                path: path.clone(),
                width: 2,
                height: 2,
                pixels: Arc::new(vec![0xff]),
            },
            publication: refresh,
            previous_publication: Some(previous),
            replies: vec![reply],
        };

        assert!(encode_and_publish(&job, &publications).is_err());
        assert_eq!(publications.lock().unwrap().get(&path), Some(&previous));
        assert_eq!(std::fs::read(&path).unwrap(), b"previous");
    }

    #[test]
    fn cancelling_window_removes_published_generation() {
        let directory = TestDirectory::new();
        let path = directory.0.join("window-9.png");
        let mut publisher = ThumbnailPublisher::new();
        let (reply, result) = reply_channel();
        publisher.publish(
            ThumbnailCapture {
                window_id: 9,
                path: path.clone(),
                width: 1,
                height: 1,
                pixels: Arc::new(vec![0, 0, 0xff, 0xff]),
            },
            vec![reply],
        );
        result.recv_blocking().unwrap().unwrap();
        assert!(path.exists());

        publisher.cancel_window(9);
        for _ in 0..100 {
            if !path.exists() {
                return;
            }
            thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("cancelled thumbnail was not removed by the worker");
    }

    #[test]
    fn publish_fans_out_one_result_to_all_merged_replies() {
        let directory = TestDirectory::new();
        let path = directory.0.join("window-fanout.png");
        let mut publisher = ThumbnailPublisher::new();

        let mut replies = Vec::new();
        let mut results = Vec::new();
        for _ in 0..5 {
            let (reply, result) = reply_channel();
            replies.push(reply);
            results.push(result);
        }
        publisher.publish(
            ThumbnailCapture {
                window_id: 10,
                path: path.clone(),
                width: 1,
                height: 1,
                pixels: Arc::new(vec![1, 2, 3, 4]),
            },
            replies,
        );

        for result in &results {
            let reply = result.recv_blocking().unwrap().unwrap();
            assert_eq!(reply.width, 1);
            assert_eq!(reply.height, 1);
        }
        assert_eq!(decode_rgba(&path).unwrap(), vec![1, 2, 3, 4]);
    }

    #[test]
    fn publish_with_all_replies_closed_publishes_nothing() {
        let directory = TestDirectory::new();
        let path = directory.0.join("window-closed.png");
        let mut publisher = ThumbnailPublisher::new();

        let (reply, _result) = reply_channel();
        drop(_result);
        publisher.publish(
            ThumbnailCapture {
                window_id: 11,
                path: path.clone(),
                width: 1,
                height: 1,
                pixels: Arc::new(vec![9, 9, 9, 9]),
            },
            vec![reply],
        );

        for _ in 0..100 {
            if path.exists() {
                panic!("no file may be published when every reply is closed");
            }
            thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[test]
    fn identical_requests_merge_and_latest_wins_for_same_window() {
        let directory = TestDirectory::new();
        let path = directory.0.join("window-queue.png");
        let mut queue = ThumbnailRequestQueue::new(4, 4, 1024);

        let (r1, rx1) = reply_channel();
        queue.submit(ThumbnailRequest {
            window_id: 1,
            path: path.clone(),
            max_width: 100,
            max_height: 100,
            replies: vec![r1],
        });
        let (r2, rx2) = reply_channel();
        queue.submit(ThumbnailRequest {
            window_id: 1,
            path: path.clone(),
            max_width: 100,
            max_height: 100,
            replies: vec![r2],
        });
        assert_eq!(queue.pending_len(), 1, "identical requests must merge");

        let (r3, rx3) = reply_channel();
        queue.submit(ThumbnailRequest {
            window_id: 1,
            path: path.clone(),
            max_width: 200,
            max_height: 200,
            replies: vec![r3],
        });
        assert_eq!(
            queue.pending_len(),
            1,
            "same window supersedes with latest-wins"
        );
        assert_eq!(
            rx1.try_recv()
                .expect("superseded request must be answered")
                .expect_err("superseded request must fail"),
            "thumbnail request superseded by a newer request for the same window"
        );

        let request = queue.pop_pending().unwrap();
        assert_eq!(request.max_width, 200);
        assert_eq!(request.replies.len(), 1);
        let _ = (rx2, rx3);
    }

    #[test]
    fn full_queue_rejects_new_requests_fail_fast() {
        let directory = TestDirectory::new();
        let mut queue = ThumbnailRequestQueue::new(2, 4, 1024);

        queue.submit(request(1, &directory.0.join("a.png"), 1, 1));
        queue.submit(request(2, &directory.0.join("b.png"), 1, 1));
        assert_eq!(queue.pending_len(), 2);

        let (reply, result) = reply_channel();
        queue.submit(ThumbnailRequest {
            window_id: 3,
            path: directory.0.join("c.png"),
            max_width: 1,
            max_height: 1,
            replies: vec![reply],
        });
        assert_eq!(queue.pending_len(), 2, "the queue must stay bounded");
        assert_eq!(
            result
                .try_recv()
                .expect("rejected request must be answered")
                .expect_err("rejected request must fail"),
            "thumbnail request queue is full"
        );
    }

    #[test]
    fn cancel_window_answers_pending_requests_and_drops_cache() {
        let directory = TestDirectory::new();
        let mut queue = queue_with_small_cache();

        let (reply, result) = reply_channel();
        queue.submit(ThumbnailRequest {
            window_id: 7,
            path: directory.0.join("w.png"),
            max_width: 10,
            max_height: 10,
            replies: vec![reply],
        });
        queue.cache_put(
            7,
            10,
            10,
            sample_version(),
            10,
            10,
            Arc::new(vec![0; 10 * 10 * 4]),
        );
        assert_eq!(queue.cache_len(), 1);

        queue.cancel_window(7);
        assert_eq!(queue.pending_len(), 0);
        assert_eq!(
            queue.cache_len(),
            0,
            "window destroy must invalidate its cache"
        );
        assert_eq!(
            result
                .try_recv()
                .expect("pending request must be answered")
                .expect_err("pending request must fail"),
            "window not found or not on an output: 7"
        );
    }

    #[test]
    fn cache_hit_requires_matching_version_and_stale_entries_are_evicted() {
        let directory = TestDirectory::new();
        let path = directory.0.join("w.png");
        let mut queue = queue_with_small_cache();
        let v1 = sample_version();

        queue.cache_put(1, 10, 10, v1, 10, 10, Arc::new(vec![0; 10 * 10 * 4]));
        let hit = queue.cache_get(1, 10, 10, v1, &path);
        assert!(hit.is_some(), "matching version must hit");
        assert_eq!(hit.unwrap().pixels.len(), 10 * 10 * 4);

        // Content changed: the version differs and the stale entry is gone.
        let v2 = ThumbnailVersion {
            content_epoch: 2,
            ..v1
        };
        assert!(queue.cache_get(1, 10, 10, v2, &path).is_none());
        assert_eq!(queue.cache_len(), 0, "stale entries must be removed");
    }

    #[test]
    fn cache_evicts_least_recently_used_until_under_caps() {
        // Entry cap 4, byte cap 20 KiB.
        let mut queue = ThumbnailRequestQueue::new(4, 4, 20 * 1024);
        let v = sample_version();
        let path = |window_id: u64| PathBuf::from(format!("/nonexistent/w{window_id}.png"));

        // 3 x 64x64 entries = 3 * 16 KiB = 48 KiB; byte cap is 20 KiB, so the
        // two oldest entries must be evicted immediately.
        for window_id in 1..=3_u64 {
            queue.cache_put(window_id, 64, 64, v, 64, 64, Arc::new(vec![0; 64 * 64 * 4]));
        }
        assert_eq!(queue.cache_len(), 1, "byte cap must evict older entries");
        assert!(queue.cache_bytes() <= 20 * 1024);
        assert!(queue.cache_get(1, 64, 64, v, &path(1)).is_none());
        assert!(queue.cache_get(2, 64, 64, v, &path(2)).is_none());
        assert!(queue.cache_get(3, 64, 64, v, &path(3)).is_some());

        // Four small entries push the entry count past the cap: the
        // least-recently-used entry (3) must be evicted, the others survive.
        for window_id in 4..=7_u64 {
            queue.cache_put(window_id, 16, 16, v, 16, 16, Arc::new(vec![0; 16 * 16 * 4]));
        }
        assert_eq!(queue.cache_len(), 4, "entry cap must hold");
        assert!(
            queue.cache_get(3, 64, 64, v, &path(3)).is_none(),
            "LRU must be evicted"
        );
        for window_id in 4..=7_u64 {
            assert!(
                queue
                    .cache_get(window_id, 16, 16, v, &path(window_id))
                    .is_some(),
                "recently used entries must survive"
            );
        }
    }
}
