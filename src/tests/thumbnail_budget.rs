//! T05 thumbnail main-loop budget integration tests.
//!
//! These tests drive the existing public thumbnail IPC entry
//! (`State::window_thumbnail` + `State::cancel_window_thumbnail`) through the
//! real compositor loop, renderer and publisher worker, and assert the budget
//! properties the old implementation violates:
//!
//! - a burst must not be served synchronously from inside the request entry (old implementation
//!   rendered + read back every request inline, blocking the main loop),
//! - captures are paced to at most one per event-loop iteration while Wayland events keep flowing,
//! - destroyed windows / removed outputs / disconnected clients leave no in-flight or cached state
//!   behind,
//! - cached results stay pixel-identical and invalidate on content change.

use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use wayland_client::protocol::wl_surface::WlSurface;

use super::client::ClientId;
use super::*;
use crate::niri::Niri;
use crate::thumbnail::ThumbnailReply;
use crate::utils::lifecycle_diag;

/// Run the whole test inside the serialized lifecycle-diag window so the
/// thumbnail-render counter is exact (only the current test can be counting).
fn with_diag<R>(f: impl FnOnce() -> R) -> R {
    lifecycle_diag::with_enabled_for_test(f)
}

fn thumbnail_render_count() -> u64 {
    lifecycle_diag::snapshot().thumbnail_render
}

static TEST_DIRECTORY_ID: AtomicU64 = AtomicU64::new(0);

/// Scratch directory for published thumbnails (cleaned on drop).
struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Self {
        let id = TEST_DIRECTORY_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "niri-thumbnail-budget-test-{}-{id}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Create a window filled with a solid ARGB8888 color. `color` is in RGBA
/// order (0-255 per channel).
fn create_colored_window(
    f: &mut Fixture,
    id: ClientId,
    w: u16,
    h: u16,
    color: (u32, u32, u32, u32),
) -> WlSurface {
    let (r, g, b, a) = color;
    let mut pixels = vec![0_u8; usize::from(w) * usize::from(h) * 4];
    // wl_shm ARGB8888 memory order is B, G, R, A.
    for chunk in pixels.chunks_mut(4) {
        chunk.copy_from_slice(&[b as u8, g as u8, r as u8, a as u8]);
    }
    create_window_with_pixels(f, id, w, h, &pixels)
}

fn create_window_with_pixels(
    f: &mut Fixture,
    id: ClientId,
    w: u16,
    h: u16,
    pixels: &[u8],
) -> WlSurface {
    let window = f.client(id).create_window();
    let surface = window.surface.clone();
    window.commit();
    f.roundtrip(id);

    f.client(id)
        .state
        .attach_shm_buffer(&surface, u32::from(w), u32::from(h), pixels);
    let window = f.client(id).window(&surface);
    window.ack_last_and_commit();
    f.double_roundtrip(id);
    // The fixture does not drive the compositor loop, so the window would stay
    // frozen at the start of its open animation; complete animations like the
    // production loop does.
    f.niri_complete_animations();
    surface
}

fn mapped_window_id(niri: &Niri) -> u64 {
    niri.layout
        .windows()
        .next()
        .map(|(_, mapped)| mapped.id().get())
        .expect("mapped window")
}

/// Initialize the headless GL renderer like other render-observing fixtures do.
fn init_renderer(f: &mut Fixture) {
    f.niri_state().backend.headless().add_renderer().unwrap();
}

fn request_thumbnail(
    f: &mut Fixture,
    window_id: u64,
    path: &Path,
    max_width: u32,
    max_height: u32,
) -> async_channel::Receiver<ThumbnailReply> {
    let (tx, rx) = async_channel::bounded(1);
    f.niri_state().window_thumbnail(
        window_id,
        path.to_string_lossy().into_owned(),
        max_width,
        max_height,
        tx,
    );
    rx
}

fn ready_count(receivers: &[async_channel::Receiver<ThumbnailReply>]) -> usize {
    receivers.iter().filter(|rx| !rx.is_empty()).count()
}

/// Drive the compositor (server) loop directly.
///
/// `Fixture::dispatch` only drives the server loop when its poll fd is woken
/// by a client event; the thumbnail capture idle is scheduled from a direct
/// call, so the tests drive the server loop explicitly.
fn dispatch_server(f: &mut Fixture) {
    f.state.server.dispatch();
}

fn collect_replies(
    f: &mut Fixture,
    receivers: &mut Vec<async_channel::Receiver<ThumbnailReply>>,
) -> Vec<ThumbnailReply> {
    let mut results = Vec::new();
    for _ in 0..200_000 {
        dispatch_server(f);
        let mut i = 0;
        while i < receivers.len() {
            match receivers[i].try_recv() {
                Ok(reply) => {
                    results.push(reply);
                    receivers.swap_remove(i);
                }
                Err(async_channel::TryRecvError::Empty) => i += 1,
                Err(async_channel::TryRecvError::Closed) => {
                    panic!("reply channel closed without a reply");
                }
            }
        }
        if receivers.is_empty() {
            return results;
        }
    }
    panic!(
        "thumbnails did not complete in time; pending {}",
        receivers.len()
    );
}

fn decode_rgba(path: &Path) -> anyhow::Result<Vec<u8>> {
    let decoder = png::Decoder::new(BufReader::new(std::fs::File::open(path)?));
    let mut reader = decoder.read_info()?;
    let mut buffer = vec![
        0;
        reader
            .output_buffer_size()
            .expect("valid PNG output buffer size")
    ];
    let info = reader.next_frame(&mut buffer)?;
    buffer.truncate(info.buffer_size());
    Ok(buffer)
}

/// A05.1 + A05.3: a burst of 1,000 identical requests must not be served
/// synchronously from inside the request entry, must coalesce into a single
/// capture, and every request must still receive its response.
#[test]
fn thousand_identical_requests_coalesce_and_do_not_block_the_loop() {
    with_diag(|| {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        init_renderer(&mut f);
        let id = f.add_client();
        create_colored_window(&mut f, id, 320, 220, (255, 0, 0, 255));
        let window_id = mapped_window_id(f.niri());
        let dir = TestDirectory::new();
        let path = dir.0.join("window-1.png");

        let renders_before = thumbnail_render_count();

        let mut receivers = Vec::new();
        for _ in 0..1000 {
            receivers.push(request_thumbnail(&mut f, window_id, &path, 320, 220));
        }

        // The budget is the point: the entry must not block on a synchronous GPU
        // readback. Old implementation rendered all 1,000 requests inline here.
        assert_eq!(
            ready_count(&receivers),
            0,
            "burst must not be served before the event loop runs"
        );

        let replies = collect_replies(&mut f, &mut receivers);
        assert_eq!(replies.len(), 1000, "every request must get its reply");
        for reply in &replies {
            let result = reply.as_ref().expect("burst reply must succeed");
            assert_eq!(result.path, path.to_string_lossy());
            assert_eq!((result.width, result.height), (320, 220));
        }

        // 1,000 identical requests coalesce into one capture.
        let renders = thumbnail_render_count() - renders_before;
        assert_eq!(
            renders, 1,
            "duplicate requests must merge into a single capture"
        );

        let pixels = decode_rgba(&path).expect("published PNG must decode");
        assert_eq!(pixels.len(), 320 * 220 * 4);
        assert!(pixels.iter().enumerate().all(|(i, byte)| {
            match i % 4 {
                0 => *byte == 255, // R
                1 => *byte == 0,   // G
                2 => *byte == 0,   // B
                _ => *byte == 255, // A
            }
        }));
    });
}

/// A05.5: an identical request with unchanged content is served from the
/// cache — no new render, same pixels.
#[test]
fn unchanged_content_is_served_from_cache_without_rerender() {
    with_diag(|| {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        init_renderer(&mut f);
        let id = f.add_client();
        create_colored_window(&mut f, id, 100, 80, (0, 0, 255, 255));
        let window_id = mapped_window_id(f.niri());
        let dir = TestDirectory::new();
        let path = dir.0.join("window-cache.png");

        let mut receivers = vec![request_thumbnail(&mut f, window_id, &path, 200, 160)];
        let replies = collect_replies(&mut f, &mut receivers);
        let renders_after_first = thumbnail_render_count();
        let first = replies[0].as_ref().expect("first request must succeed");
        assert_eq!((first.width, first.height), (100, 80));

        // Same request again: the capture is cached, no render happens.
        let mut receivers = vec![request_thumbnail(&mut f, window_id, &path, 200, 160)];
        let replies = collect_replies(&mut f, &mut receivers);
        assert_eq!(
            thumbnail_render_count() - renders_after_first,
            0,
            "unchanged content must be served from the cache"
        );
        let second = replies[0].as_ref().expect("cached request must succeed");
        assert_eq!((second.width, second.height), (100, 80));

        let pixels = decode_rgba(&path).expect("published PNG must decode");
        assert!(pixels.iter().enumerate().all(|(i, byte)| {
            match i % 4 {
                0 => *byte == 0,   // R
                1 => *byte == 0,   // G
                2 => *byte == 255, // B
                _ => *byte == 255, // A
            }
        }));
    });
}

/// A05.5: when the window content changes, the cache must be invalidated and
/// the next request re-render the current pixels.
#[test]
fn content_change_invalidates_cache_and_refreshes_pixels() {
    with_diag(|| {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        init_renderer(&mut f);
        let id = f.add_client();
        let surface = create_colored_window(&mut f, id, 64, 64, (255, 0, 0, 255));
        let window_id = mapped_window_id(f.niri());
        let dir = TestDirectory::new();
        let path = dir.0.join("window-invalidate.png");

        let mut receivers = vec![request_thumbnail(&mut f, window_id, &path, 64, 64)];
        collect_replies(&mut f, &mut receivers);
        let renders_before = thumbnail_render_count();
        let red = decode_rgba(&path).expect("published PNG must decode");
        assert_eq!(red[0], 255);

        // Change the window content through the real commit path.
        let mut green_pixels = vec![0_u8; 64 * 64 * 4];
        for chunk in green_pixels.chunks_mut(4) {
            chunk.copy_from_slice(&[0, 255, 0, 255]); // B, G, R, A memory order
        }
        f.client(id)
            .state
            .attach_shm_buffer(&surface, 64, 64, &green_pixels);
        f.client(id).window(&surface).ack_last_and_commit();
        f.double_roundtrip(id);

        let mut receivers = vec![request_thumbnail(&mut f, window_id, &path, 64, 64)];
        let replies = collect_replies(&mut f, &mut receivers);
        assert!(replies[0].is_ok(), "post-change request must succeed");
        assert_eq!(
            thumbnail_render_count() - renders_before,
            1,
            "content change must invalidate the cache and force a fresh capture"
        );

        let green = decode_rgba(&path).expect("published PNG must decode");
        assert_eq!(green[0], 0, "fresh capture must reflect the new content");
        assert_eq!(green[1], 255);
        assert_eq!(green[2], 0);
    });
}

/// A05.3: requests are served one capture per event-loop iteration; Wayland
/// events (here: real client roundtrips) keep flowing between captures.
#[test]
fn captures_are_paced_one_per_loop_iteration_while_events_flow() {
    with_diag(|| {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        init_renderer(&mut f);
        let id = f.add_client();
        create_colored_window(&mut f, id, 64, 64, (0, 255, 0, 255));
        let window_id = mapped_window_id(f.niri());
        let dir = TestDirectory::new();

        // Distinct request sizes produce distinct pending entries and cache keys,
        // so every request needs its own capture.
        let mut receivers = Vec::new();
        for i in 0..8_u32 {
            let path = dir.0.join(format!("window-paced-{i}.png"));
            receivers.push(request_thumbnail(&mut f, window_id, &path, 64 + i, 64 + i));
        }

        assert_eq!(
            ready_count(&receivers),
            0,
            "nothing may be served synchronously at submit time"
        );

        let renders_before = thumbnail_render_count();
        // Pacing: while requests remain, every event-loop iteration performs
        // exactly one capture (the re-scheduled idle fires on the next dispatch).
        for step in 1..=4 {
            dispatch_server(&mut f);
            let captures = thumbnail_render_count() - renders_before;
            assert_eq!(
                captures, step,
                "iteration {step}: exactly one capture per event-loop iteration"
            );
        }

        // Interleaving: with captures still pending, real client roundtrips must
        // complete (pointer/frame events keep being processed) and the queue keeps
        // advancing.
        for _ in 0..4 {
            let before_roundtrip = thumbnail_render_count();
            f.roundtrip(id);
            assert!(
                thumbnail_render_count() - before_roundtrip >= 1,
                "client roundtrips must keep the capture queue advancing"
            );
        }

        let replies = collect_replies(&mut f, &mut receivers);
        assert_eq!(replies.len(), 8, "every request must be answered");
        assert_eq!(
            thumbnail_render_count() - renders_before,
            8,
            "each distinct request must be captured exactly once"
        );
    });
}

/// A05.3: a request at the 4096x4096 protocol maximum keeps the measured
/// per-iteration latency bounded; the timing is recorded as evidence.
#[test]
fn max_size_request_keeps_measured_iteration_latency_bounded() {
    with_diag(|| {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        init_renderer(&mut f);
        let id = f.add_client();
        create_colored_window(&mut f, id, 256, 256, (0, 0, 255, 255));
        let window_id = mapped_window_id(f.niri());
        let dir = TestDirectory::new();

        let mut receivers = Vec::new();
        for i in 0..4_u32 {
            let path = dir.0.join(format!("window-max-{i}.png"));
            receivers.push(request_thumbnail(&mut f, window_id, &path, 4096, 4096 - i));
        }

        assert_eq!(
            ready_count(&receivers),
            0,
            "max-size requests must not be served synchronously at submit time"
        );
        // Functional proof that the entry did not block: all four requests are
        // still queued (the old implementation served them synchronously and
        // left the queue empty).
        assert_eq!(
            f.niri().thumbnail_queue_test_pending_len(),
            4,
            "submitting max-size requests must not block the caller"
        );

        let mut max_iteration = std::time::Duration::ZERO;
        for _ in 0..4 {
            let start = std::time::Instant::now();
            dispatch_server(&mut f);
            max_iteration = max_iteration.max(start.elapsed());
        }

        eprintln!("T05 max-size request: worst single-iteration latency {max_iteration:?}");
        // Generous bound: the point of the budget is that a burst cannot stack
        // captures inside one iteration; a single 256x256 capture takes far less
        // than this even on the slowest CI machine.
        assert!(
            max_iteration < std::time::Duration::from_secs(1),
            "a single iteration must not exceed 1s even under max-size requests: {max_iteration:?}"
        );

        let replies = collect_replies(&mut f, &mut receivers);
        assert_eq!(replies.len(), 4);
        for reply in &replies {
            let result = reply.as_ref().expect("max-size request must succeed");
            // The window is 256x256; the capture is clamped to the window size and
            // never upscaled, and the response reports the actual capture size.
            assert_eq!((result.width, result.height), (256, 256));
        }
    });
}

/// A05.2: a window destroyed while its request is still pending must fail
/// that request with the legacy "window not found" error and leave no state.
#[test]
fn window_destroyed_while_request_pending_fails_with_legacy_error() {
    with_diag(|| {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        let id = f.add_client();
        let surface = create_colored_window(&mut f, id, 64, 64, (255, 255, 0, 255));
        let window_id = mapped_window_id(f.niri());
        let dir = TestDirectory::new();
        let path = dir.0.join("window-destroy.png");

        let mut receivers = vec![request_thumbnail(&mut f, window_id, &path, 64, 64)];

        // Destroy the window through the real unmap path.
        f.client(id).window(&surface).attach_null();
        f.client(id).window(&surface).commit();
        f.double_roundtrip(id);

        let replies = collect_replies(&mut f, &mut receivers);
        assert_eq!(replies.len(), 1);
        let err = replies[0].as_ref().expect_err("destroyed window must fail");
        assert_eq!(
            err,
            &format!("window not found or not on an output: {window_id}"),
            "legacy error wording must be preserved"
        );

        assert_eq!(
            thumbnail_render_count(),
            0,
            "a request for a destroyed window must not render"
        );
        assert_eq!(
            f.niri().thumbnail_queue_test_pending_len(),
            0,
            "no pending thumbnail state may survive a window destroy"
        );
    });
}

/// A05.2: a client that disconnects before its request is served leaves no
/// capture, no published file and no queued state.
#[test]
fn disconnected_client_skips_capture_and_leaves_no_state() {
    with_diag(|| {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        let id = f.add_client();
        create_colored_window(&mut f, id, 64, 64, (255, 0, 0, 255));
        let window_id = mapped_window_id(f.niri());
        let dir = TestDirectory::new();
        let path = dir.0.join("window-disconnect.png");

        let renders_before = thumbnail_render_count();

        // Submit a request and immediately drop the receiving side: from the
        // server's perspective the client is gone and nobody is awaiting a reply.
        let (tx, rx) = async_channel::bounded::<ThumbnailReply>(1);
        drop(rx);
        f.niri_state()
            .window_thumbnail(window_id, path.to_string_lossy().into_owned(), 64, 64, tx);

        // Let the loop run long enough for a capture + worker publish cycle.
        for _ in 0..64 {
            dispatch_server(&mut f);
        }

        assert_eq!(
            thumbnail_render_count() - renders_before,
            0,
            "a capture nobody is waiting for must be skipped"
        );
        assert!(
            !path.exists(),
            "no thumbnail may be published for a disconnected client"
        );
        assert_eq!(f.niri().thumbnail_queue_test_pending_len(), 0);
    });
}

/// A05.2: removing the output that hosts a pending request resolves the
/// request deterministically and leaves no queued/cached state.
#[test]
fn output_removed_with_pending_requests_resolves_without_leak() {
    with_diag(|| {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        init_renderer(&mut f);
        f.add_output(2, (1280, 720));
        let id = f.add_client();
        create_colored_window(&mut f, id, 64, 64, (255, 255, 0, 255));
        let window_id = mapped_window_id(f.niri());
        let dir = TestDirectory::new();
        let path = dir.0.join("window-output.png");

        let mut receivers = vec![request_thumbnail(&mut f, window_id, &path, 64, 64)];

        // Remove the window's host output while the request is still pending.
        // The workspace (and the window) is appended to the remaining output, so
        // the request must still resolve to a valid capture.
        let output = f.niri_output(1);
        f.niri().remove_output(&output);
        f.double_roundtrip(id);

        let replies = collect_replies(&mut f, &mut receivers);
        assert_eq!(replies.len(), 1, "the request must still be answered");
        let result = replies[0]
            .as_ref()
            .expect("window survives on the remaining output");
        assert!(result.width > 0 && result.height > 0);

        assert_eq!(f.niri().thumbnail_queue_test_pending_len(), 0);
    });
}

/// A05.2: when the renderer is unavailable (renderer reset / not yet
/// initialized), requests fail with the legacy error, the pipeline stays
/// usable, and no capture or queued state is left behind.
#[test]
fn renderer_unavailable_fails_requests_but_keeps_pipeline_usable() {
    with_diag(|| {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        // Note: no `init_renderer` — the headless backend has no GL renderer.
        let id = f.add_client();
        create_colored_window(&mut f, id, 64, 64, (255, 0, 0, 255));
        let window_id = mapped_window_id(f.niri());
        let dir = TestDirectory::new();
        let path = dir.0.join("window-no-renderer.png");

        let mut receivers = vec![request_thumbnail(&mut f, window_id, &path, 64, 64)];
        let replies = collect_replies(&mut f, &mut receivers);
        assert_eq!(replies.len(), 1);
        let err = replies[0]
            .as_ref()
            .expect_err("unavailable renderer must fail the request");
        assert_eq!(err, "primary renderer unavailable");
        assert_eq!(f.niri().thumbnail_queue_test_pending_len(), 0);

        // The pipeline recovers once the renderer is (re)initialized.
        init_renderer(&mut f);
        let mut receivers = vec![request_thumbnail(&mut f, window_id, &path, 64, 64)];
        let replies = collect_replies(&mut f, &mut receivers);
        assert!(
            replies[0].is_ok(),
            "pipeline must recover after renderer reset"
        );
        assert_eq!(f.niri().thumbnail_queue_test_pending_len(), 0);
    });
}

/// A05.2: removing the last output leaves the window without any output; the
/// pending request fails with the legacy error and no state is left behind.
#[test]
fn last_output_removed_with_pending_request_fails_with_legacy_error() {
    with_diag(|| {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        init_renderer(&mut f);
        let id = f.add_client();
        create_colored_window(&mut f, id, 64, 64, (255, 0, 0, 255));
        let window_id = mapped_window_id(f.niri());
        let dir = TestDirectory::new();
        let path = dir.0.join("window-last-output.png");

        let mut receivers = vec![request_thumbnail(&mut f, window_id, &path, 64, 64)];

        let output = f.niri_output(1);
        f.niri().remove_output(&output);
        f.double_roundtrip(id);

        let replies = collect_replies(&mut f, &mut receivers);
        assert_eq!(replies.len(), 1);
        let err = replies[0]
            .as_ref()
            .expect_err("a window without any output must fail the request");
        assert_eq!(
            err,
            &format!("window not found or not on an output: {window_id}"),
            "legacy error wording must be preserved"
        );
        assert_eq!(f.niri().thumbnail_queue_test_pending_len(), 0);
    });
}

/// A05.5: a commit on a subsurface (whose commit activity never touches the
/// toplevel) must invalidate the thumbnail cache.
#[test]
fn subsurface_commit_invalidates_cache_and_refreshes_pixels() {
    with_diag(|| {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        init_renderer(&mut f);
        let id = f.add_client();
        let surface = create_colored_window(&mut f, id, 64, 64, (255, 0, 0, 255));
        let window_id = mapped_window_id(f.niri());
        let dir = TestDirectory::new();
        let path = dir.0.join("window-sub-commit.png");

        // A 32x32 blue subsurface covering the window's top-left corner.
        let mut blue = vec![0_u8; 32 * 32 * 4];
        for chunk in blue.chunks_mut(4) {
            chunk.copy_from_slice(&[255, 0, 0, 255]); // B, G, R, A memory order
        }
        let sub = f.client(id).state.create_subsurface(&surface);
        f.client(id).state.attach_shm_buffer(&sub, 32, 32, &blue);
        sub.commit();
        f.double_roundtrip(id);

        let mut receivers = vec![request_thumbnail(&mut f, window_id, &path, 64, 64)];
        collect_replies(&mut f, &mut receivers);
        let renders_before = thumbnail_render_count();
        let first = decode_rgba(&path).expect("published PNG must decode");
        assert_eq!(
            &first[..4],
            &[0, 0, 255, 255],
            "the top-left pixel must show the blue subsurface"
        );

        // Change the subsurface content through the real commit path.
        let mut green = vec![0_u8; 32 * 32 * 4];
        for chunk in green.chunks_mut(4) {
            chunk.copy_from_slice(&[0, 255, 0, 255]); // B, G, R, A memory order
        }
        f.client(id).state.attach_shm_buffer(&sub, 32, 32, &green);
        sub.commit();
        f.double_roundtrip(id);

        let mut receivers = vec![request_thumbnail(&mut f, window_id, &path, 64, 64)];
        let replies = collect_replies(&mut f, &mut receivers);
        assert!(replies[0].is_ok(), "post-commit request must succeed");
        assert_eq!(
            thumbnail_render_count() - renders_before,
            1,
            "a subsurface commit must invalidate the cache"
        );
        let second = decode_rgba(&path).expect("published PNG must decode");
        assert_eq!(
            &second[..4],
            &[0, 255, 0, 255],
            "the fresh capture must show the subsurface's new content"
        );
    });
}

/// A05.5: destroying a subsurface must invalidate the cache even though no
/// commit follows the destruction.
#[test]
fn subsurface_destroy_invalidates_cache() {
    with_diag(|| {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        init_renderer(&mut f);
        let id = f.add_client();
        let surface = create_colored_window(&mut f, id, 64, 64, (255, 0, 0, 255));
        let window_id = mapped_window_id(f.niri());
        let dir = TestDirectory::new();
        let path = dir.0.join("window-sub-destroy.png");

        let mut blue = vec![0_u8; 32 * 32 * 4];
        for chunk in blue.chunks_mut(4) {
            chunk.copy_from_slice(&[255, 0, 0, 255]);
        }
        let sub = f.client(id).state.create_subsurface(&surface);
        f.client(id).state.attach_shm_buffer(&sub, 32, 32, &blue);
        sub.commit();
        f.double_roundtrip(id);

        let mut receivers = vec![request_thumbnail(&mut f, window_id, &path, 64, 64)];
        collect_replies(&mut f, &mut receivers);
        let renders_before = thumbnail_render_count();
        let first = decode_rgba(&path).expect("published PNG must decode");
        assert_eq!(&first[..4], &[0, 0, 255, 255]);

        // Destroy the subsurface through the real destroy path.
        sub.destroy();
        f.double_roundtrip(id);

        let mut receivers = vec![request_thumbnail(&mut f, window_id, &path, 64, 64)];
        let replies = collect_replies(&mut f, &mut receivers);
        assert!(replies[0].is_ok(), "post-destroy request must succeed");
        assert_eq!(
            thumbnail_render_count() - renders_before,
            1,
            "a subsurface destroy must invalidate the cache"
        );
        let second = decode_rgba(&path).expect("published PNG must decode");
        assert_eq!(
            &second[..4],
            &[255, 0, 0, 255],
            "the fresh capture must no longer show the subsurface"
        );
    });
}

/// A05.5: a commit on a popup of the window (whose commits never touch the
/// toplevel and whose commit counter would sit below the toplevel's) must
/// invalidate the thumbnail cache.
#[test]
fn popup_commit_invalidates_cache_and_refreshes_pixels() {
    with_diag(|| {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        init_renderer(&mut f);
        let id = f.add_client();
        let surface = create_colored_window(&mut f, id, 64, 64, (255, 0, 0, 255));
        let window_id = mapped_window_id(f.niri());
        let dir = TestDirectory::new();
        let path = dir.0.join("window-popup-commit.png");

        // A 32x32 blue xdg popup at the window's top-left corner.
        let parent_xdg = f.client(id).window(&surface).xdg_surface.clone();
        let popup = f.client(id).state.create_popup(&parent_xdg, 0, 0, 32, 32);
        // The first commit triggers the initial popup configure.
        popup.commit();
        f.double_roundtrip(id);
        // Ack the popup configure, attach its content and map it.
        {
            let state = &mut f.client(id).state;
            let popup_entry = state
                .popups
                .iter_mut()
                .find(|p| p.surface == popup)
                .unwrap();
            let serial = popup_entry.configure_serial.expect("popup configure");
            popup_entry.xdg_surface.ack_configure(serial);
        }
        let mut blue = vec![0_u8; 32 * 32 * 4];
        for chunk in blue.chunks_mut(4) {
            chunk.copy_from_slice(&[255, 0, 0, 255]); // B, G, R, A memory order
        }
        f.client(id).state.attach_shm_buffer(&popup, 32, 32, &blue);
        popup.commit();
        f.double_roundtrip(id);

        let mut receivers = vec![request_thumbnail(&mut f, window_id, &path, 64, 64)];
        collect_replies(&mut f, &mut receivers);
        let renders_before = thumbnail_render_count();
        let first = decode_rgba(&path).expect("published PNG must decode");
        assert_eq!(
            &first[..4],
            &[0, 0, 255, 255],
            "the top-left pixel must show the blue popup"
        );

        // Change the popup content through the real commit path.
        let mut green = vec![0_u8; 32 * 32 * 4];
        for chunk in green.chunks_mut(4) {
            chunk.copy_from_slice(&[0, 255, 0, 255]); // B, G, R, A memory order
        }
        f.client(id).state.attach_shm_buffer(&popup, 32, 32, &green);
        popup.commit();
        f.double_roundtrip(id);

        let mut receivers = vec![request_thumbnail(&mut f, window_id, &path, 64, 64)];
        let replies = collect_replies(&mut f, &mut receivers);
        assert!(replies[0].is_ok(), "post-commit request must succeed");
        assert_eq!(
            thumbnail_render_count() - renders_before,
            1,
            "a popup commit must invalidate the cache"
        );
        let second = decode_rgba(&path).expect("published PNG must decode");
        assert_eq!(
            &second[..4],
            &[0, 255, 0, 255],
            "the fresh capture must show the popup's new content"
        );
    });
}

/// A05.5: changing the output scale changes the capture resolution; the
/// cache version must reflect it and force a fresh capture.
#[test]
fn output_scale_change_invalidates_cache_and_refreshes_size() {
    with_diag(|| {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        init_renderer(&mut f);
        let id = f.add_client();
        create_colored_window(&mut f, id, 64, 64, (0, 255, 0, 255));
        let window_id = mapped_window_id(f.niri());
        let dir = TestDirectory::new();
        let path = dir.0.join("window-scale.png");

        // Max size 128 so the scale-2 capture (128x128 physical) is not
        // clamped back to 64.
        let mut receivers = vec![request_thumbnail(&mut f, window_id, &path, 128, 128)];
        let replies = collect_replies(&mut f, &mut receivers);
        let renders_before = thumbnail_render_count();
        let first = replies[0].as_ref().expect("scale-1 request must succeed");
        assert_eq!((first.width, first.height), (64, 64));

        // Change the output scale through the real output state path.
        let output = f.niri_output(1);
        output.change_current_state(
            None,
            None,
            Some(smithay::output::Scale::Fractional(2.0)),
            None,
        );
        f.niri().layout.update_output_size(&output);
        f.double_roundtrip(id);

        let mut receivers = vec![request_thumbnail(&mut f, window_id, &path, 128, 128)];
        let replies = collect_replies(&mut f, &mut receivers);
        assert!(replies[0].is_ok(), "post-scale request must succeed");
        assert_eq!(
            thumbnail_render_count() - renders_before,
            1,
            "an output scale change must invalidate the cache"
        );
        let second = replies[0].as_ref().expect("scale-2 request must succeed");
        assert_eq!(
            (second.width, second.height),
            (128, 128),
            "the capture must be re-rendered at the new scale"
        );
    });
}

/// A05.3: a real 4096x4096 capture (the protocol maximum) is measured: the
/// single-iteration latency stays bounded and the response reports the
/// actual capture size.
#[test]
fn real_max_size_capture_keeps_measured_iteration_latency_bounded() {
    with_diag(|| {
        let mut f = Fixture::new();
        f.add_output(1, (1920, 1080));
        init_renderer(&mut f);
        let id = f.add_client();
        create_colored_window(&mut f, id, 4096, 4096, (0, 128, 255, 255));
        let window_id = mapped_window_id(f.niri());
        let dir = TestDirectory::new();
        let path = dir.0.join("window-4096.png");

        let mut receivers = vec![request_thumbnail(&mut f, window_id, &path, 4096, 4096)];
        let start = std::time::Instant::now();
        dispatch_server(&mut f);
        let iteration = start.elapsed();

        let replies = collect_replies(&mut f, &mut receivers);
        assert_eq!(replies.len(), 1);
        let result = replies[0].as_ref().expect("4096x4096 capture must succeed");
        assert_eq!((result.width, result.height), (4096, 4096));

        eprintln!("T05 4096x4096 capture: single-iteration latency {iteration:?}");
        assert!(
            iteration < std::time::Duration::from_secs(2),
            "a 4096x4096 capture must not stall the loop beyond the measured bound: {iteration:?}"
        );
    });
}
