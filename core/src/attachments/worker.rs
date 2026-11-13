use super::*;
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc::{self, SyncSender};

// These types deliberately have no Debug or serialization implementation: results contain image bytes.
pub struct NormalizeJob {
    pub raw: RawReady,
    pub max_out_bytes: u64,
}

pub struct NormalizeDone {
    pub id: AttachmentId,
    pub result: Result<Normalized, ImageRefusal>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct WorkerBusy;

/// A serial decoder with four waiting jobs. The callback runs here; the shell supplies a callback
/// that posts the result onto its main context without touching toolkit objects on this thread.
pub struct Worker {
    sender: SyncSender<NormalizeJob>,
}

impl Worker {
    /// Each job is decoded by a child process started from `decoder`.
    pub fn new(callback: Box<dyn Fn(NormalizeDone) + Send>, decoder: DecoderCommand) -> io::Result<Self> {
        Self::with_normalizer(callback, move |raw, max_out_bytes| {
            normalize_in_child(raw, max_out_bytes, &decoder)
        })
    }

    fn with_normalizer(
        callback: Box<dyn Fn(NormalizeDone) + Send>,
        normalize: impl Fn(&[u8], u64) -> Result<Normalized, ImageRefusal> + Send + 'static,
    ) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<NormalizeJob>(4);
        std::thread::Builder::new()
            .name("image-normalize".into())
            .spawn(move || {
                while let Ok(job) = receiver.recv() {
                    // A decoder may panic on untrusted bytes; keep the queue available for later jobs.
                    let result = catch_unwind(AssertUnwindSafe(|| {
                        job.raw.read().and_then(|raw| normalize(&raw, job.max_out_bytes))
                    }))
                    .unwrap_or(Err(ImageRefusal::Undecodable));
                    callback(NormalizeDone { id: job.raw.id, result });
                }
            })?;
        Ok(Self { sender })
    }

    /// Only enqueues. Full and disconnected workers are refused without waiting.
    pub fn submit(&self, job: NormalizeJob) -> Result<(), WorkerBusy> {
        self.sender.try_send(job).map_err(|_| WorkerBusy)
    }
}

// Dropping the last sender lets the thread drain and exit. It never joins on the window thread.

#[cfg(test)]
mod tests {
    use super::super::isolate::tests::test_decoder;
    use super::super::normalize::normalize;
    use super::*;
    use crate::test_scratch_dir::ScratchDir;
    use sha2::{Digest, Sha256};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    fn job(s: &mut Staging) -> NormalizeJob {
        let raw = super::super::normalize::tests::png(1, 1, 255);
        job_bytes(s, &raw)
    }

    fn job_bytes(s: &mut Staging, raw: &[u8]) -> NormalizeJob {
        let id = s.begin(1, 1, raw.len() as u64).unwrap();
        s.append(id, 0, raw).unwrap();
        let ready = s.finish(id, &format!("{:x}", Sha256::digest(raw))).unwrap();
        NormalizeJob {
            raw: ready,
            max_out_bytes: RAW_MAX_BYTES,
        }
    }

    #[test]
    fn a_panicking_decoder_is_refused_and_the_next_job_completes() {
        let scratch = ScratchDir::new("attach", "panic");
        let mut staging = Staging::open(&scratch).unwrap();
        let (sent, received) = mpsc::channel();
        let worker = Worker::with_normalizer(Box::new(move |done| sent.send(done).unwrap()), |raw, max| {
            if raw == b"panic" {
                panic!("injected decoder panic");
            }
            normalize(raw, max)
        })
        .unwrap();
        assert_refusal_then_success(&worker, &mut staging, &received, b"panic", ImageRefusal::Undecodable);
    }

    fn assert_refusal_then_success(
        worker: &Worker,
        staging: &mut Staging,
        received: &mpsc::Receiver<NormalizeDone>,
        raw: &[u8],
        expected: ImageRefusal,
    ) {
        let bad = job_bytes(staging, raw);
        let bad_id = bad.raw.id;
        worker.submit(bad).unwrap();
        let done = received.recv_timeout(Duration::from_secs(60)).unwrap();
        assert_eq!(done.id, bad_id);
        assert_eq!(done.result.err(), Some(expected));
        let good = job(staging);
        let good_id = good.raw.id;
        worker.submit(good).unwrap();
        let done = received.recv_timeout(Duration::from_secs(60)).unwrap();
        assert_eq!(done.id, good_id);
        assert!(done.result.is_ok());
    }

    #[test]
    fn an_animated_webp_with_mismatched_alpha_and_vp8_is_refused_and_the_worker_survives() {
        let raw = super::super::normalize::regressions::panicking_webp();
        let scratch = ScratchDir::new("attach", "webp");
        let mut staging = Staging::open(&scratch).unwrap();
        let (sent, received) = mpsc::channel();
        let worker = Worker::new(Box::new(move |done| sent.send(done).unwrap()), test_decoder()).unwrap();
        assert_refusal_then_success(
            &worker,
            &mut staging,
            &received,
            &raw,
            ImageRefusal::TooManyPixels { width: 2, height: 2 },
        );
    }

    // Releasing on unwind keeps even an assertion failure from stranding the worker thread.
    struct Release(Arc<(Mutex<bool>, Condvar)>);
    impl Drop for Release {
        fn drop(&mut self) {
            let (mutex, condvar) = &*self.0;
            *mutex.lock().unwrap() = true;
            condvar.notify_all();
        }
    }

    #[test]
    fn the_worker_refuses_a_fifth_queued_job() {
        let scratch = ScratchDir::new("attach", "worker");
        let mut staging = Staging::open(&scratch).unwrap();
        let (sent, received) = mpsc::channel();
        let held = Arc::new((Mutex::new(false), Condvar::new()));
        let release = Release(held.clone());
        let worker = Worker::new(
            Box::new(move |done| {
                sent.send((done.id, done.result.is_ok(), std::thread::current().id()))
                    .unwrap();
                let (mutex, condvar) = &*held;
                let (released, timeout) = condvar
                    .wait_timeout_while(mutex.lock().unwrap(), Duration::from_secs(60), |r| !*r)
                    .unwrap();
                assert!(*released && !timeout.timed_out(), "worker release hang guard");
            }),
            test_decoder(),
        )
        .unwrap();
        let first = job(&mut staging);
        let first_id = first.raw.id;
        worker.submit(first).unwrap();
        let (id, ok, thread) = received.recv_timeout(Duration::from_secs(60)).unwrap();
        assert_eq!(id, first_id);
        assert!(ok);
        assert_ne!(thread, std::thread::current().id());
        let mut expected = Vec::new();
        for _ in 0..4 {
            let queued = job(&mut staging);
            expected.push(queued.raw.id);
            worker.submit(queued).unwrap();
        }
        assert_eq!(worker.submit(job(&mut staging)), Err(WorkerBusy));
        drop(worker); // Closing the queue must neither join nor wait on the held callback.
        drop(release);
        for id in expected {
            let (actual, ok, _) = received.recv_timeout(Duration::from_secs(60)).unwrap();
            assert_eq!(actual, id);
            assert!(ok);
        }
    }

    #[test]
    fn a_removed_raw_file_returns_a_failure_without_panicking() {
        let scratch = ScratchDir::new("attach", "removed");
        let mut staging = Staging::open(&scratch).unwrap();
        let job = job(&mut staging);
        let id = job.raw.id;
        staging.remove(id);
        let (sent, received) = mpsc::channel();
        let worker = Worker::new(
            Box::new(move |done| {
                sent.send(done).unwrap();
            }),
            test_decoder(),
        )
        .unwrap();
        worker.submit(job).unwrap();
        let done = received.recv_timeout(Duration::from_secs(60)).unwrap();
        assert_eq!(done.id, id);
        assert!(matches!(done.result, Err(ImageRefusal::Undecodable)));
    }
}
