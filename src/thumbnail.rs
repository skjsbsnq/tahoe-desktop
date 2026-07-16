use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::{process, thread};

use anyhow::{bail, Context as _};

use crate::utils::write_png_rgba8;

const THUMBNAIL_WORKER_QUEUE_CAPACITY: usize = 16;

pub(crate) type ThumbnailReply = Result<niri_ipc::WindowThumbnail, String>;

pub(crate) struct ThumbnailCapture {
    pub window_id: u64,
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
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
    reply: async_channel::Sender<ThumbnailReply>,
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
        reply: async_channel::Sender<ThumbnailReply>,
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
            reply,
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
                let _ = job
                    .reply
                    .try_send(Err(String::from("thumbnail publisher queue is full")));
            }
            Err(TrySendError::Disconnected(WorkerMessage::Publish(job))) => {
                restore_previous_publication_if_current(
                    &self.publications,
                    &job.capture.path,
                    job.publication,
                    job.previous_publication,
                );
                let _ = job
                    .reply
                    .try_send(Err(String::from("thumbnail publisher is unavailable")));
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
    if job.reply.is_closed() {
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
    if job
        .reply
        .send_blocking(result.map_err(|err| err.to_string()))
        .is_err()
    {
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

        if job.reply.is_closed() {
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
        let (first_reply, first_result) = async_channel::bounded(1);
        publisher.publish(
            ThumbnailCapture {
                window_id: 42,
                path: path.clone(),
                width: 1,
                height: 1,
                pixels: first_pixels.clone(),
            },
            first_reply,
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
            let (reply, result) = async_channel::bounded(1);
            publisher.publish(
                ThumbnailCapture {
                    window_id: 42,
                    path: path.clone(),
                    width: 1,
                    height: 1,
                    pixels: pixels.clone(),
                },
                reply,
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

        let (old_reply, _old_result) = async_channel::bounded(1);
        let old_job = PublishJob {
            capture: ThumbnailCapture {
                window_id: 7,
                path: path.clone(),
                width: 1,
                height: 1,
                pixels: vec![0xff, 0, 0, 0xff],
            },
            publication: old,
            previous_publication: None,
            reply: old_reply,
        };
        assert!(encode_and_publish(&old_job, &publications).is_err());
        assert!(!path.exists());

        let (current_reply, _current_result) = async_channel::bounded(1);
        let current_job = PublishJob {
            capture: ThumbnailCapture {
                window_id: 7,
                path: path.clone(),
                width: 1,
                height: 1,
                pixels: vec![0, 0xff, 0, 0xff],
            },
            publication: current,
            previous_publication: None,
            reply: current_reply,
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
        let (reply, _result) = async_channel::bounded(1);
        let job = PublishJob {
            capture: ThumbnailCapture {
                window_id: 8,
                path: path.clone(),
                width: 2,
                height: 2,
                pixels: vec![0xff],
            },
            publication: refresh,
            previous_publication: Some(previous),
            reply,
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
        let (reply, result) = async_channel::bounded(1);
        publisher.publish(
            ThumbnailCapture {
                window_id: 9,
                path: path.clone(),
                width: 1,
                height: 1,
                pixels: vec![0, 0, 0xff, 0xff],
            },
            reply,
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
}
