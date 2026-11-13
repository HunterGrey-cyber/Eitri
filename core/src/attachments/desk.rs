//! The window's attachment desk: one thread that owns the window's staging directory and its decoder worker, and
//! does every file and process step of an import for the tab set (`crate::tab_set::AttachmentDesk`), which only
//! sends it requests and reads its events on the window's tick. Nothing here runs on the window's thread.
//!
//! An image comes back as its prepared bytes in memory (read through [`Staging::read`] right after they are
//! stored), so a send reads no file on the window's thread. The page's token for an image is the
//! [`AttachmentId`]'s text; the desk keeps the ids it issued and knows nothing else by name.

use super::*;
use crate::tab_set::{AttachmentDesk, DeskEnd, DeskEvent, DeskRequest, PreparedImage, IMAGE_MAX_BYTES};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

enum Message {
    Request(DeskRequest),
    Done(NormalizeDone),
    /// The window let go of its end: the desk drops its worker and its staging directory and ends.
    Closed,
}

/// Starts the desk of one window: `base` is the runtime directory its staging lives under (the user's
/// `agent::state_dirs::runtime_dir` when `None`), `decoder` the child each image is decoded in, and `wake` what
/// tells the window an event is waiting. The stale staging of processes that are gone is swept first, on the
/// desk's own thread. A desk whose staging cannot be made refuses every import with the reason.
pub fn start_desk(base: Option<PathBuf>, decoder: DecoderCommand, wake: Option<agent::UiWake>) -> AttachmentDesk {
    let (desk, end) = AttachmentDesk::channels();
    let started = std::thread::Builder::new()
        .name("attachment-desk".into())
        .spawn(move || serve(base, decoder, wake, end));
    if let Err(error) = started {
        // The desk's end went with the closure: every request is refused as stopped.
        eprintln!("eitri: the image attachment thread could not start: {error}");
    }
    desk
}

/// What the composer is told for a refusal of the decoder child: a child that cannot run at all says why.
fn refusal_words(refusal: &ImageRefusal) -> String {
    refusal.sentence()
}

fn serve(base: Option<PathBuf>, decoder: DecoderCommand, wake: Option<agent::UiWake>, end: DeskEnd) {
    let DeskEnd { requests, events } = end;
    let (to_desk, inbox) = mpsc::channel::<Message>();
    let forward = to_desk.clone();
    // Requests arrive on their own channel; one inbox serves them and the worker's results in order.
    let forwarded = std::thread::Builder::new()
        .name("attachment-desk-in".into())
        .spawn(move || {
            for request in requests {
                if forward.send(Message::Request(request)).is_err() {
                    return;
                }
            }
            let _ = forward.send(Message::Closed);
        });
    if forwarded.is_err() {
        return;
    }
    let base = base.or_else(agent::state_dirs::runtime_dir);
    let staging = match &base {
        None => Err("this window cannot take images: there is no runtime directory".to_string()),
        Some(base) => {
            sweep_stale_staging(base);
            Staging::open(base).map_err(|error| format!("this window cannot take images: {error}"))
        }
    };
    let done = to_desk;
    let worker = staging.as_ref().ok().map(|_| {
        Worker::new(
            Box::new(move |result| {
                let _ = done.send(Message::Done(result));
            }),
            decoder,
        )
        .map_err(|error| format!("this window cannot take images: {error}"))
    });
    let (mut staging, worker) = match (staging, worker) {
        (Ok(staging), Some(Ok(worker))) => (Ok(staging), Some(worker)),
        (Err(why), _) | (_, Some(Err(why))) => (Err(why), None),
        (Ok(_), None) => unreachable!("a worker is made with every staging"),
    };
    let mut desk = Desk {
        ids: HashMap::new(),
        events,
    };
    for message in inbox {
        match message {
            Message::Closed => break,
            Message::Request(request) => desk.request(&mut staging, worker.as_ref(), request),
            Message::Done(done) => {
                if let Ok(staging) = &mut staging {
                    desk.prepared(staging, done);
                }
            }
        }
        if let Some(wake) = &wake {
            wake();
        }
    }
    // The worker drains what it holds and exits; dropping the staging deletes the window's directory.
    drop(worker);
    drop(staging);
}

struct Desk {
    ids: HashMap<String, AttachmentId>,
    events: mpsc::Sender<DeskEvent>,
}

impl Desk {
    fn tell(&self, event: DeskEvent) {
        let _ = self.events.send(event);
    }

    fn forget(&mut self, staging: &mut Staging, token: &str) {
        if let Some(id) = self.ids.remove(token) {
            staging.remove(id);
        }
    }

    fn request(&mut self, staging: &mut Result<Staging, String>, worker: Option<&Worker>, request: DeskRequest) {
        let staging = match staging {
            Ok(staging) => staging,
            Err(why) => {
                let why = why.clone();
                match request {
                    DeskRequest::Begin { reply, .. } => self.tell(DeskEvent::Begun {
                        reply,
                        result: Err(why),
                    }),
                    DeskRequest::Chunk { reply, .. } | DeskRequest::End { reply, .. } => {
                        self.tell(DeskEvent::Answered {
                            reply,
                            result: Err(why),
                        })
                    }
                    DeskRequest::ImportFile { reply, .. } => self.tell(DeskEvent::Imported {
                        reply,
                        result: Err(why),
                    }),
                    DeskRequest::Remove { .. } => {}
                    #[cfg(test)]
                    DeskRequest::ReservedBytes(reply) => {
                        let _ = reply.send(0);
                    }
                }
                return;
            }
        };
        match request {
            #[cfg(test)]
            DeskRequest::ReservedBytes(reply) => {
                let _ = reply.send(staging.reserved_bytes());
            }
            DeskRequest::Begin {
                reply,
                tab,
                generation,
                len,
            } => {
                let result = staging.begin(tab, generation, len).map(|id| {
                    let token = id.to_string();
                    self.ids.insert(token.clone(), id);
                    token
                });
                self.tell(DeskEvent::Begun {
                    reply,
                    result: result.map_err(|refusal| refusal.sentence()),
                });
            }
            DeskRequest::Chunk {
                reply,
                token,
                offset,
                data,
            } => {
                let result = match self.ids.get(&token) {
                    None => Err(StagingRefusal::UnknownId.sentence()),
                    Some(id) => staging.append(*id, offset, &data).map_err(|refusal| refusal.sentence()),
                };
                if result.is_err() {
                    self.forget(staging, &token);
                }
                self.tell(DeskEvent::Answered { reply, result });
            }
            DeskRequest::End { reply, token, sha256 } => {
                let result = match self.ids.get(&token) {
                    None => Err(StagingRefusal::UnknownId.sentence()),
                    Some(id) => staging
                        .finish(*id, &sha256)
                        .map_err(|refusal| refusal.sentence())
                        .and_then(|raw| {
                            let job = NormalizeJob {
                                raw,
                                max_out_bytes: IMAGE_MAX_BYTES,
                            };
                            worker
                                .ok_or_else(|| "this window cannot take images".to_string())?
                                .submit(job)
                                .map_err(|WorkerBusy| {
                                    "too many images are being prepared: paste it again in a moment".to_string()
                                })
                        }),
                };
                if result.is_err() {
                    self.forget(staging, &token);
                }
                self.tell(DeskEvent::Answered { reply, result });
            }
            DeskRequest::Remove { tokens } => {
                for token in tokens {
                    self.forget(staging, &token);
                }
            }
            DeskRequest::ImportFile {
                reply,
                tab,
                generation,
                path,
            } => {
                let result = self.import_file(staging, worker, tab, generation, &path);
                self.tell(DeskEvent::Imported { reply, result });
            }
        }
    }

    /// A file the person dropped: read here, staged as an import of its own (its digest the one computed here) and
    /// handed to the worker, as a page's import is once its end arrives. Its token and length, or why not, with
    /// nothing of it kept.
    fn import_file(
        &mut self,
        staging: &mut Staging,
        worker: Option<&Worker>,
        tab: u64,
        generation: u64,
        path: &Path,
    ) -> Result<(String, u64), String> {
        let bytes = read_dropped_file(path)?;
        let len = bytes.len() as u64;
        let id = staging
            .begin(tab, generation, len)
            .map_err(|refusal| refusal.sentence())?;
        let token = id.to_string();
        self.ids.insert(token.clone(), id);
        let sha256: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
        let staged = bytes
            .chunks(CHUNK_MAX_BYTES)
            .enumerate()
            .try_for_each(|(index, chunk)| staging.append(id, (index * CHUNK_MAX_BYTES) as u64, chunk))
            .and_then(|()| staging.finish(id, &sha256))
            .map_err(|refusal| refusal.sentence())
            .and_then(|raw| {
                let job = NormalizeJob {
                    raw,
                    max_out_bytes: IMAGE_MAX_BYTES,
                };
                worker
                    .ok_or_else(|| "this window cannot take images".to_string())?
                    .submit(job)
                    .map_err(|WorkerBusy| "too many images are being prepared: drop it again in a moment".to_string())
            });
        if let Err(why) = staged {
            self.forget(staging, &token);
            return Err(why);
        }
        Ok((token, len))
    }

    /// A decoded image: stored, read back, and reported; or the refusal reported and its files removed. One
    /// removed while it was decoded is forgotten.
    fn prepared(&mut self, staging: &mut Staging, done: NormalizeDone) {
        let token = done.id.to_string();
        if !self.ids.contains_key(&token) {
            return;
        }
        let result = done
            .result
            .map_err(|refusal| refusal_words(&refusal))
            .and_then(|normalized| {
                let staged = staging
                    .store_normalized(done.id, &normalized)
                    .map_err(|refusal| refusal.sentence())?;
                let data = staging
                    .read(done.id)
                    .map_err(|_| StagingRefusal::Io(std::io::Error::other("read")).sentence())?;
                // From here the bytes live in memory only; the quota still counts them until the image goes.
                staging.release_file(done.id);
                Ok(PreparedImage {
                    meta: agent::AttachmentMeta {
                        media_type: staged.image_type.media_type().to_string(),
                        bytes: staged.bytes,
                        width: staged.width,
                        height: staged.height,
                        sha256: staged.sha256.iter().map(|b| format!("{b:02x}")).collect(),
                    },
                    data,
                    thumbnail_png: staged.thumbnail_png,
                })
            });
        if result.is_err() {
            self.forget(staging, &token);
        }
        self.tell(DeskEvent::Prepared { token, result });
    }
}

/// How long the desk waits for one dropped file to be read before it gives up on it.
pub const DROP_READ_SECONDS: u64 = 20;

/// Reads a file the person dropped on a chat page, for an import: a regular file (followed through a link, as the
/// file manager showed it) of at most [`RAW_MAX_BYTES`], checked before opening and again on the open handle
/// ([`read_dropped_handle`]). The read runs on a thread of its own, waited on for [`DROP_READ_SECONDS`]: a file on a
/// mount that hangs (a network or FUSE drive) leaves that thread behind rather than the window's desk, which every
/// later import waits on. The reason is in the composer's words. Unlike the host's own state, a dropped file need not
/// be private or the user's own: the person chose it, and only an image that decodes is ever sent.
pub fn read_dropped_file(path: &Path) -> Result<Vec<u8>, String> {
    let path = path.to_path_buf();
    within(std::time::Duration::from_secs(DROP_READ_SECONDS), move || {
        read_dropped_now(&path)
    })
    .unwrap_or_else(|| {
        Err(format!(
            "this file took longer than {DROP_READ_SECONDS} s to read: copy it to this computer and drop it again"
        ))
    })
}

/// `work`'s result if it is done within `deadline`, on a thread of its own; `None` if not (the thread is left to end
/// by itself) or if no thread could start.
fn within<T: Send + 'static>(deadline: std::time::Duration, work: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (done, result) = mpsc::channel();
    std::thread::Builder::new()
        .name("attachment-drop-read".into())
        .spawn(move || {
            let _ = done.send(work());
        })
        .ok()?;
    result.recv_timeout(deadline).ok()
}

fn not_regular() -> String {
    "only an image file can be attached, not a folder or a device".to_string()
}

fn unreadable() -> String {
    "this file could not be read".to_string()
}

fn read_dropped_now(path: &Path) -> Result<Vec<u8>, String> {
    let meta = std::fs::metadata(path).map_err(|_| unreadable())?;
    if !meta.is_file() {
        return Err(not_regular());
    }
    if meta.len() > RAW_MAX_BYTES {
        return Err(ImageRefusal::TooLargeRaw { bytes: meta.len() }.sentence());
    }
    // Non-blocking, so a FIFO swapped in after the check above cannot stall the open.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| unreadable())?;
    read_dropped_handle(file)
}

/// The open handle of a dropped file, read: it must be a regular file now (whatever the path named when it was
/// checked), within [`RAW_MAX_BYTES`] as it is read, and not empty.
fn read_dropped_handle(file: std::fs::File) -> Result<Vec<u8>, String> {
    let meta = file.metadata().map_err(|_| unreadable())?;
    if !meta.is_file() {
        return Err(not_regular());
    }
    let mut bytes = Vec::with_capacity(meta.len().min(RAW_MAX_BYTES) as usize);
    file.take(RAW_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| unreadable())?;
    if bytes.len() as u64 > RAW_MAX_BYTES {
        return Err(ImageRefusal::TooLargeRaw {
            bytes: bytes.len() as u64,
        }
        .sentence());
    }
    if bytes.is_empty() {
        return Err(ImageRefusal::Empty.sentence());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dropped_regular_file_is_read_whole() {
        let dir = crate::test_scratch_dir::ScratchDir::new("attach", "drop-read");
        let path = dir.join("shot.png");
        std::fs::write(&path, b"\x89PNG").unwrap();
        // A file others may write is the person's choice to drop: read as any other.
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o666)).unwrap();
        assert_eq!(read_dropped_file(&path).unwrap(), b"\x89PNG");
        let link = dir.join("link.png");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert_eq!(read_dropped_file(&link).unwrap(), b"\x89PNG", "a link is followed");
    }

    /// A read that does not end in time is given up on, and the desk goes on.
    #[test]
    fn a_read_that_hangs_is_given_up_on() {
        let (release, wait) = mpsc::channel::<()>();
        let started = std::time::Instant::now();
        let read = within(std::time::Duration::from_millis(50), move || {
            let _ = wait.recv();
            1
        });
        assert_eq!(read, None);
        assert!(started.elapsed() >= std::time::Duration::from_millis(50));
        drop(release);
        assert_eq!(within(std::time::Duration::from_secs(10), || 2), Some(2));
    }

    #[test]
    fn a_dropped_folder_fifo_empty_or_oversized_file_is_refused() {
        let dir = crate::test_scratch_dir::ScratchDir::new("attach", "drop-refuse");
        assert!(read_dropped_file(&dir).unwrap_err().contains("not a folder"));
        let fifo = dir.join("fifo");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert!(
            read_dropped_file(&fifo).unwrap_err().contains("not a folder"),
            "never opened, never waited on"
        );
        // A FIFO swapped in after the path was checked: the open handle is checked again, and nothing waits on it.
        let opened = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&fifo)
            .unwrap();
        assert!(read_dropped_handle(opened).unwrap_err().contains("not a folder"));
        let empty = dir.join("empty.png");
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(read_dropped_file(&empty).unwrap_err(), ImageRefusal::Empty.sentence());
        let big = dir.join("big.png");
        std::fs::File::create(&big).unwrap().set_len(RAW_MAX_BYTES + 1).unwrap();
        assert_eq!(
            read_dropped_file(&big).unwrap_err(),
            ImageRefusal::TooLargeRaw {
                bytes: RAW_MAX_BYTES + 1
            }
            .sentence()
        );
        assert_eq!(
            read_dropped_file(&dir.join("gone.png")).unwrap_err(),
            "this file could not be read"
        );
    }
}

#[cfg(test)]
mod refusal_tests {
    use super::*;

    #[test]
    fn one_setup_failure_does_not_relabel_a_later_bad_image() {
        let missing = DecoderCommand::new("/nonexistent/decoder", ["x"]);
        let raw = super::super::test_png(1, 1, 255);
        let failed = normalize_in_child(&raw, RAW_MAX_BYTES, &missing).err().unwrap();
        assert!(refusal_words(&failed).contains("decoder could not run"));
        let later = normalize_in_child(
            b"\xff\xd8\xff\xe0truncated",
            RAW_MAX_BYTES,
            &super::super::test_decoder(),
        )
        .err()
        .unwrap();
        assert_eq!(later, ImageRefusal::Undecodable);
        assert_eq!(refusal_words(&later), "this image could not be decoded");
        assert!(normalize_in_child(&raw, RAW_MAX_BYTES, &super::super::test_decoder()).is_ok());
    }
}
