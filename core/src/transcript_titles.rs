//! The Claude CLI's own session titles (`agent::transcript::newest_ai_title`), read on a worker thread
//! instead of the window's.
//!
//! The resume picker, the chooser and `hello` show each Claude record's newest `ai-title`. Reading it is a
//! `stat`, an `open` and a 64 KiB tail read per row: about a millisecond for a picker cold, several on a
//! network file system. [`TitleCache::apply`] only
//! answers from memory -- the title read last time, or, for a transcript not read yet, the row's own
//! title from Eitri's record (its first prompt, also used when there is no `ai-title`) -- and
//! asks the worker to look again. The worker reads a transcript again only when its size or time
//! changed, and when a title it found differs from what memory held it raises the window's wake and
//! says so ([`TitleCache::take_changed`]); the window then sends `hello` again with the new title. So a
//! row shows something true at once and the CLI's title a moment later, and the window never waits.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::time::SystemTime;

use agent::ResumableSession;

/// What the worker last found for one transcript.
#[derive(Clone)]
struct Seen {
    /// Size and modification time when it was read: an unchanged file is not read again.
    stamp: Option<(u64, SystemTime)>,
    title: Option<String>,
}

#[derive(Default)]
struct Shared {
    seen: HashMap<PathBuf, Seen>,
    /// A title differs from the one memory held before the worker's last read.
    changed: bool,
    /// Paths handed to the worker and not read yet, so a picker opened twice asks once.
    queued: HashSet<PathBuf>,
    /// Reads the worker has not finished (a test waits for none).
    pending: usize,
}

pub struct TitleCache {
    shared: Arc<Mutex<Shared>>,
    /// Started at the first read asked for.
    jobs: Mutex<Option<mpsc::Sender<PathBuf>>>,
    wake: Option<agent::UiWake>,
}

impl TitleCache {
    /// `wake` is raised when a title changed; `None` for a window that polls anyway.
    pub fn new(wake: Option<agent::UiWake>) -> TitleCache {
        TitleCache {
            shared: Arc::default(),
            jobs: Mutex::new(None),
            wake,
        }
    }

    /// Lays each Claude row's newest known `ai-title` over its own title, from memory, and asks the worker
    /// to look at every row's transcript again. A row whose transcript has not been read, or holds no
    /// title, keeps its own. Touches no disk.
    pub fn apply(&self, canonical_cwd: &str, sessions: &mut [ResumableSession]) {
        let mut ask = Vec::new();
        {
            let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            for session in sessions.iter_mut() {
                let Ok(path) = agent::transcript::transcript_path(canonical_cwd, &session.provider_session_id) else {
                    continue;
                };
                if let Some(title) = shared.seen.get(&path).and_then(|seen| seen.title.clone()) {
                    session.title = Some(title);
                }
                if shared.queued.insert(path.clone()) {
                    shared.pending += 1;
                    ask.push(path);
                }
            }
        }
        for path in ask {
            self.ask(path);
        }
    }

    /// The newest title read for `provider_session_id`'s transcript, from memory; `None` when none is known.
    pub fn known_title(&self, canonical_cwd: &str, provider_session_id: &str) -> Option<String> {
        let path = agent::transcript::transcript_path(canonical_cwd, provider_session_id).ok()?;
        let shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        shared.seen.get(&path).and_then(|seen| seen.title.clone())
    }

    /// Whether a title changed since the last call: the window's greeting is out of date.
    pub fn take_changed(&self) -> bool {
        std::mem::take(&mut self.shared.lock().unwrap_or_else(|e| e.into_inner()).changed)
    }

    /// Waits until the worker has read everything asked of it, for a test.
    #[cfg(any(test, feature = "test-support"))]
    pub fn wait_read(&self, within: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + within;
        while self.shared.lock().unwrap_or_else(|e| e.into_inner()).pending > 0 {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        true
    }

    fn ask(&self, path: PathBuf) {
        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        if jobs.is_none() {
            let (tx, rx) = mpsc::channel::<PathBuf>();
            let shared = self.shared.clone();
            let wake = self.wake.clone();
            let spawned = std::thread::Builder::new()
                .name("eitri-titles".into())
                .spawn(move || read_loop(&rx, &shared, wake.as_ref()));
            match spawned {
                Ok(_) => *jobs = Some(tx),
                Err(e) => {
                    eprintln!("[titles] the title reader could not start ({e}); rows show Eitri's own titles");
                    let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
                    shared.queued.clear();
                    shared.pending = 0;
                    return;
                }
            }
        }
        if let Some(jobs) = jobs.as_ref() {
            if jobs.send(path.clone()).is_err() {
                let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
                shared.queued.remove(&path);
                shared.pending = shared.pending.saturating_sub(1);
            }
        }
    }
}

/// The worker: one transcript at a time, read only when it changed; ends when the cache is dropped.
fn read_loop(jobs: &mpsc::Receiver<PathBuf>, shared: &Mutex<Shared>, wake: Option<&agent::UiWake>) {
    while let Ok(path) = jobs.recv() {
        let stamp = std::fs::metadata(&path)
            .ok()
            .and_then(|meta| meta.modified().ok().map(|modified| (meta.len(), modified)));
        let known = shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .seen
            .get(&path)
            .cloned();
        let unchanged = known
            .as_ref()
            .is_some_and(|seen| stamp.is_some() && seen.stamp == stamp);
        let title = if unchanged {
            known.as_ref().and_then(|seen| seen.title.clone())
        } else {
            agent::transcript::newest_ai_title(&path)
        };
        let raise = {
            let mut shared = shared.lock().unwrap_or_else(|e| e.into_inner());
            let before = shared.seen.get(&path).and_then(|seen| seen.title.clone());
            let differs = before != title;
            shared.seen.insert(path.clone(), Seen { stamp, title });
            shared.queued.remove(&path);
            shared.pending = shared.pending.saturating_sub(1);
            shared.changed |= differs;
            differs
        };
        if raise {
            if let Some(wake) = wake {
                wake();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn session(id: &str, title: Option<&str>) -> ResumableSession {
        ResumableSession {
            provider: "claude".into(),
            provider_session_id: id.into(),
            created_at: "1".into(),
            updated_at: "2".into(),
            title: title.map(str::to_string),
            name: None,
        }
    }

    fn title_line(title: &str) -> String {
        format!("{{\"type\":\"ai-title\",\"aiTitle\":\"{title}\"}}\n")
    }

    /// The window is answered from memory at once -- the record's own title first -- and the CLI's title
    /// arrives with one wake; an unchanged transcript wakes nothing again, a new title does.
    #[test]
    fn titles_are_read_off_the_window_thread_and_arrive_with_a_wake() {
        // The test binary's one fake config root; a project of this test's own under it.
        crate::agent_backend::test_claude_projects_root();
        let cwd_owned = format!("/home/u/titles-{}", uuid::Uuid::new_v4());
        let cwd = cwd_owned.as_str();
        let path = agent::transcript::transcript_path(cwd, "s1").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, title_line("from the CLI")).unwrap();

        let wakes = Arc::new(AtomicUsize::new(0));
        let cache = {
            let wakes = wakes.clone();
            TitleCache::new(Some(Arc::new(move || {
                wakes.fetch_add(1, Ordering::SeqCst);
            })))
        };
        let mut rows = vec![session("s1", Some("first prompt")), session("s2", None)];
        cache.apply(cwd, &mut rows);
        assert_eq!(
            rows[0].title.as_deref(),
            Some("first prompt"),
            "the record's own title meanwhile"
        );
        assert!(cache.wait_read(std::time::Duration::from_secs(5)));
        assert_eq!(wakes.load(Ordering::SeqCst), 1, "one title found, one wake");
        assert!(cache.take_changed());
        assert!(!cache.take_changed(), "taken");

        let mut rows = vec![session("s1", Some("first prompt")), session("s2", None)];
        cache.apply(cwd, &mut rows);
        assert_eq!(rows[0].title.as_deref(), Some("from the CLI"));
        assert_eq!(rows[1].title, None, "no transcript, no title");
        assert!(cache.wait_read(std::time::Duration::from_secs(5)));
        assert_eq!(wakes.load(Ordering::SeqCst), 1, "nothing changed, nothing woken");
        assert!(!cache.take_changed());

        // The CLI retitles the session: the next look finds it.
        std::fs::write(
            &path,
            format!("{}{}", title_line("from the CLI"), title_line("renamed")),
        )
        .unwrap();
        cache.apply(cwd, &mut rows);
        assert!(cache.wait_read(std::time::Duration::from_secs(5)));
        assert_eq!(wakes.load(Ordering::SeqCst), 2);
        assert!(cache.take_changed());
        let mut rows = vec![session("s1", None)];
        cache.apply(cwd, &mut rows);
        assert_eq!(rows[0].title.as_deref(), Some("renamed"));

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// `apply` does no disk work: it answers a hundred rows from memory (nothing is known yet, so each keeps its
    /// own title) and leaves the reading to the worker, which finishes afterwards.
    #[test]
    fn apply_answers_from_memory() {
        let cache = TitleCache::new(None);
        let mut rows: Vec<_> = (0..100).map(|i| session(&format!("s{i}"), Some("own"))).collect();
        cache.apply("/nonexistent/for/titles", &mut rows);
        assert!(rows.iter().all(|r| r.title.as_deref() == Some("own")));
        assert!(cache.wait_read(std::time::Duration::from_secs(5)));
    }

    /// What the window's thread spends titling sixteen rows whose transcripts are 1 MB each: reading them
    /// itself or answering from the cache. For a measurement run by hand
    /// (`--release --ignored --nocapture`).
    #[test]
    #[ignore]
    fn measure_titling_sixteen_rows() {
        crate::agent_backend::test_claude_projects_root();
        let cwd_owned = format!("/home/u/titles-measure-{}", uuid::Uuid::new_v4());
        let cwd = cwd_owned.as_str();
        let rows: Vec<_> = (0..16).map(|i| session(&format!("m{i}"), Some("own"))).collect();
        let filler = "{\"type\":\"assistant\",\"message\":\"".to_string() + &"x".repeat(1000) + "\"}\n";
        for row in &rows {
            let path = agent::transcript::transcript_path(cwd, &row.provider_session_id).unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut text = title_line("a title");
            while text.len() < 1_000_000 {
                text.push_str(&filler);
            }
            std::fs::write(&path, text).unwrap();
        }
        let mut direct = Vec::new();
        let mut cached = Vec::new();
        let cache = TitleCache::new(None);
        for _ in 0..20 {
            let started = std::time::Instant::now();
            for row in &rows {
                let path = agent::transcript::transcript_path(cwd, &row.provider_session_id).unwrap();
                std::hint::black_box(agent::transcript::newest_ai_title(&path));
            }
            direct.push(started.elapsed());
            let mut copy = rows.clone();
            let started = std::time::Instant::now();
            cache.apply(cwd, &mut copy);
            cached.push(started.elapsed());
            assert!(cache.wait_read(std::time::Duration::from_secs(5)));
        }
        direct.sort();
        cached.sort();
        eprintln!(
            "sixteen titles on the window's thread: read directly median {:?} max {:?}; from the cache median {:?} max {:?}",
            direct[10], direct[19], cached[10], cached[19]
        );
        let first = agent::transcript::transcript_path(cwd, "m0").unwrap();
        let _ = std::fs::remove_dir_all(first.parent().unwrap());
    }
}
