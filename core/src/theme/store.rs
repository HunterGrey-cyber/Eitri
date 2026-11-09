//! Private per-project theme payloads, read at start and written on one newest-wins worker.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Condvar, Mutex};

use serde::{Deserialize, Serialize};

use super::payload::{NvimThemePayload, PAYLOAD_VERSION};
use crate::state_paths;

pub const MAX_FILE_BYTES: u64 = 256 * 1024;

pub fn state_dir(xdg_state_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    state_paths::state_subdir(xdg_state_home, home, "theme")
}

pub fn path(dir: &Path, project_root: &Path) -> PathBuf {
    dir.join(state_paths::file_name(project_root))
}

#[derive(Debug, PartialEq)]
pub enum Loaded {
    Remembered(NvimThemePayload),
    Missing,
    Unusable(String),
}

#[derive(Serialize, Deserialize)]
struct ThemeFile {
    version: u32,
    project_root: String,
    payload: NvimThemePayload,
}

pub fn load(dir: &Path, project_root: &Path) -> Loaded {
    let path = path(dir, project_root);
    let unusable = |why| Loaded::Unusable(format!("{}: {why}", path.display()));
    let text = match agent::private_fs::read_private_to_string_capped(&path, MAX_FILE_BYTES) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Loaded::Missing,
        Err(e) => return unusable(e.to_string()),
    };
    let file: ThemeFile = match serde_json::from_str(&text) {
        Ok(file) => file,
        Err(e) => return unusable(format!("not a theme file ({e})")),
    };
    if file.version != 1 {
        return unusable(format!("version {} (this build reads 1)", file.version));
    }
    let root = project_root.to_string_lossy();
    if file.project_root != root {
        return unusable(format!("written for {}, not {root}", file.project_root));
    }
    if file.payload.v != PAYLOAD_VERSION {
        return unusable(format!(
            "payload version {} (this build reads {PAYLOAD_VERSION})",
            file.payload.v
        ));
    }
    Loaded::Remembered(file.payload)
}

pub fn encode(project_root: &Path, payload: &NvimThemePayload) -> Result<String, String> {
    let text = serde_json::to_string(&ThemeFile {
        version: 1,
        project_root: project_root.to_string_lossy().into_owned(),
        payload: payload.clone(),
    })
    .map_err(|e| e.to_string())?;
    if text.len() as u64 > MAX_FILE_BYTES {
        return Err(format!(
            "encoded theme is {} bytes, over the {MAX_FILE_BYTES}-byte cap",
            text.len()
        ));
    }
    Ok(text)
}

pub struct ThemeWriter {
    dir: Option<PathBuf>,
    project_root: PathBuf,
    last: Option<NvimThemePayload>,
    worker: Option<WriterHandle>,
    #[cfg(any(test, feature = "test-support"))]
    before_write: Option<Arc<dyn Fn() + Send + Sync>>,
}

struct WriterHandle {
    shared: Arc<WriterShared>,
    done: mpsc::Receiver<()>,
}

#[derive(Default)]
struct WriterShared {
    queue: Mutex<WriterQueue>,
    ready: Condvar,
}

#[derive(Default)]
struct WriterQueue {
    next: Option<NvimThemePayload>,
    finished: bool,
}

impl ThemeWriter {
    pub fn new(dir: Option<PathBuf>, project_root: PathBuf, last: Option<NvimThemePayload>) -> Self {
        Self {
            dir,
            project_root,
            last,
            worker: None,
            #[cfg(any(test, feature = "test-support"))]
            before_write: None,
        }
    }

    /// Hands over only a change; neither file work nor a wait for the writer happens here.
    pub fn remember(&mut self, payload: &NvimThemePayload) -> bool {
        let Some(dir) = &self.dir else { return false };
        if self.last.as_ref() == Some(payload) {
            return false;
        }
        self.last = Some(payload.clone());
        if self.worker.is_none() {
            let shared = Arc::new(WriterShared::default());
            let thread_shared = shared.clone();
            let dir = dir.clone();
            let root = self.project_root.clone();
            let (done_tx, done) = mpsc::channel();
            #[cfg(any(test, feature = "test-support"))]
            let before_write = self.before_write.clone();
            let spawned = std::thread::Builder::new().name("eitri-theme".into()).spawn(move || {
                #[cfg(any(test, feature = "test-support"))]
                let hook = || {
                    if let Some(hook) = &before_write {
                        hook();
                    }
                };
                #[cfg(not(any(test, feature = "test-support")))]
                let hook = || {};
                write_loop(&thread_shared, &dir, &root, hook);
                let _ = done_tx.send(());
            });
            if let Err(e) = spawned {
                eprintln!("[theme] could not remember the colours: {e}");
                self.dir = None;
                return true;
            }
            self.worker = Some(WriterHandle { shared, done });
        }
        let worker = self.worker.as_ref().unwrap();
        let mut queue = worker.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.next = Some(payload.clone());
        worker.shared.ready.notify_one();
        true
    }

    /// Closes the queue without waiting; the receiver reports after the last queued write and exit.
    pub fn finish(mut self) -> Option<mpsc::Receiver<()>> {
        self.close();
        self.worker.take().map(|worker| worker.done)
    }

    /// Set before the first handover so the worker can hold a write without holding the queue lock.
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_before_write(&mut self, hook: Arc<dyn Fn() + Send + Sync>) {
        self.before_write = Some(hook);
    }

    fn close(&self) {
        if let Some(worker) = &self.worker {
            let mut queue = worker.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            queue.finished = true;
            worker.shared.ready.notify_one();
        }
    }
}

impl Drop for ThemeWriter {
    fn drop(&mut self) {
        self.close();
    }
}

/// Writes each handed-over payload through this process's own `.tmp` beside the file, then renames it into place;
/// a failed write removes its `.tmp`. A process that exits between the write and the rename -- the window's close
/// does not wait for this thread -- leaves its `.tmp` behind, and nothing sweeps those: telling a dead window's from
/// a live one's would mean asking whether its process still runs, for a small file that `load` never reads (the
/// layout files make the same choice).
fn write_loop(shared: &WriterShared, dir: &Path, root: &Path, before_write: impl Fn()) {
    let path = path(dir, root);
    let temporary = state_paths::temporary(&path);
    let mut failed = false;
    loop {
        let payload = {
            let mut queue = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(payload) = queue.next.take() {
                    break payload;
                }
                if queue.finished {
                    return;
                }
                queue = shared.ready.wait(queue).unwrap_or_else(|e| e.into_inner());
            }
        };
        before_write();
        let written = (|| {
            state_paths::create_state_dir(dir).map_err(|e| e.to_string())?;
            let text = encode(root, &payload)?;
            agent::private_fs::write_private(&temporary, text.as_bytes()).map_err(|e| e.to_string())?;
            std::fs::rename(&temporary, &path).map_err(|e| e.to_string())
        })();
        match written {
            Ok(()) => failed = false,
            Err(e) => {
                let _ = std::fs::remove_file(&temporary);
                if !failed {
                    eprintln!("[theme] could not remember the colours: {e}");
                }
                failed = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::payload::{HlAttrs, NvimOptions};
    use std::os::unix::{ffi::OsStrExt, fs::PermissionsExt};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    };

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("eitri-theme-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir(&dir).unwrap();
            Self(dir)
        }
        fn dir(&self) -> PathBuf {
            self.0.join("eitri/theme")
        }
        fn root(&self) -> PathBuf {
            self.0.join("project")
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn payload() -> NvimThemePayload {
        NvimThemePayload {
            v: PAYLOAD_VERSION,
            groups: [
                (
                    "Normal".into(),
                    HlAttrs {
                        fg: Some(0x575279),
                        bg: Some(0xfaf4ed),
                        reverse: false,
                    },
                ),
                (
                    "IncSearch".into(),
                    HlAttrs {
                        reverse: true,
                        ..Default::default()
                    },
                ),
            ]
            .into(),
            options: NvimOptions {
                background: "light".into(),
                guifont: "Mono:h14".into(),
                colors_name: "rosé-pine-dawn".into(),
                cmdheight: Some(0),
                start_scale: None,
            },
        }
    }
    fn write(s: &Scratch, p: &NvimThemePayload) {
        let mut writer = ThemeWriter::new(Some(s.dir()), s.root(), None);
        assert!(writer.remember(p));
        writer.finish().unwrap().recv().unwrap();
    }
    fn fifo(path: &Path) {
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: the NUL-terminated path remains valid for the call.
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    }
    fn no_temporary_files(s: &Scratch) {
        assert!(std::fs::read_dir(s.dir()).unwrap().all(|entry| !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp")));
    }
    fn hold(writer: &mut ThemeWriter) -> (mpsc::Receiver<()>, mpsc::Sender<()>, Arc<AtomicUsize>) {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let count = Arc::new(AtomicUsize::new(0));
        let calls = count.clone();
        writer.set_before_write(Arc::new(move || {
            calls.fetch_add(1, Ordering::SeqCst);
            entered_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
        }));
        (entered_rx, release_tx, count)
    }

    #[test]
    fn a_remembered_payload_round_trips() {
        let s = Scratch::new();
        let p = payload();
        write(&s, &p);
        assert_eq!(load(&s.dir(), &s.root()), Loaded::Remembered(p));
    }
    #[test]
    fn a_payload_without_cmdheight_round_trips() {
        let s = Scratch::new();
        let mut p = payload();
        p.options.cmdheight = None;
        write(&s, &p);
        let file = path(&s.dir(), &s.root());
        let mut json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        json["payload"]["options"].as_object_mut().unwrap().remove("cmdheight");
        crate::test_scratch_dir::write_private(file, serde_json::to_string(&json).unwrap()).unwrap();
        assert_eq!(load(&s.dir(), &s.root()), Loaded::Remembered(p));
    }
    #[test]
    fn the_file_is_named_by_the_project_key_and_private() {
        let s = Scratch::new();
        write(&s, &payload());
        let file = path(&s.dir(), &s.root());
        assert_eq!(
            file,
            s.dir().join(format!("{}.json", state_paths::project_key(&s.root())))
        );
        assert_eq!(std::fs::metadata(file).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(s.dir()).unwrap().permissions().mode() & 0o777, 0o700);
    }
    #[test]
    fn a_missing_file_loads_as_missing() {
        let s = Scratch::new();
        assert_eq!(load(&s.dir(), &s.root()), Loaded::Missing);
    }
    #[test]
    fn load_refuses_each_unusable_file_and_says_why() {
        let s = Scratch::new();
        write(&s, &payload());
        let file = path(&s.dir(), &s.root());
        let original: serde_json::Value = serde_json::from_str(&encode(&s.root(), &payload()).unwrap()).unwrap();
        let mut version = original.clone();
        version["version"] = 2.into();
        let mut root = original.clone();
        root["project_root"] = "/another/project".into();
        let mut protocol = original;
        protocol["payload"]["v"] = 2.into();
        for (text, reason) in [
            ("not JSON".into(), "not a theme file"),
            (version.to_string(), "version 2"),
            (root.to_string(), "/another/project"),
            (protocol.to_string(), "payload version 2"),
            ("x".repeat(MAX_FILE_BYTES as usize + 1), "over the"),
        ] {
            crate::test_scratch_dir::write_private(&file, text).unwrap();
            let Loaded::Unusable(why) = load(&s.dir(), &s.root()) else {
                panic!("accepted unusable file")
            };
            assert!(why.starts_with(&file.display().to_string()), "{why}");
            assert!(why.contains(reason), "{why}");
        }
    }
    #[test]
    fn a_fifo_in_the_files_place_is_unusable_at_once() {
        let s = Scratch::new();
        state_paths::create_state_dir(&s.dir()).unwrap();
        let file = path(&s.dir(), &s.root());
        fifo(&file);
        let Loaded::Unusable(why) = load(&s.dir(), &s.root()) else {
            panic!("accepted FIFO")
        };
        assert!(why.starts_with(&file.display().to_string()));
        assert!(why.contains("not a regular file"));
    }
    #[test]
    fn an_unchanged_theme_at_start_writes_nothing() {
        let s = Scratch::new();
        let p = payload();
        write(&s, &p);
        let file = path(&s.dir(), &s.root());
        let bytes = std::fs::read(&file).unwrap();
        let mut writer = ThemeWriter::new(Some(s.dir()), s.root(), Some(p.clone()));
        assert!(!writer.remember(&p));
        assert!(writer.finish().is_none());
        assert_eq!(std::fs::read(file).unwrap(), bytes);
    }
    #[test]
    fn a_changed_payload_is_written_and_an_equal_one_is_not() {
        let s = Scratch::new();
        let a = payload();
        let mut b = a.clone();
        b.options.colors_name = "next".into();
        let mut writer = ThemeWriter::new(Some(s.dir()), s.root(), None);
        assert!(writer.remember(&a));
        assert!(!writer.remember(&a));
        assert!(writer.remember(&b));
        writer.finish().unwrap().recv().unwrap();
        assert_eq!(load(&s.dir(), &s.root()), Loaded::Remembered(b));
    }
    #[test]
    fn the_newest_payload_wins() {
        let s = Scratch::new();
        let a = payload();
        let mut b = a.clone();
        b.options.colors_name = "b".into();
        let mut c = a.clone();
        c.options.colors_name = "c".into();
        let mut writer = ThemeWriter::new(Some(s.dir()), s.root(), None);
        let (entered, release, count) = hold(&mut writer);
        assert!(writer.remember(&a));
        entered.recv().unwrap();
        assert!(writer.remember(&b));
        assert!(writer.remember(&c));
        let done = writer.finish().unwrap();
        release.send(()).unwrap();
        entered.recv().unwrap();
        release.send(()).unwrap();
        done.recv().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 2);
        assert_eq!(load(&s.dir(), &s.root()), Loaded::Remembered(c));
    }
    #[test]
    fn a_load_while_a_write_is_held_reads_the_previous_file_whole() {
        let s = Scratch::new();
        let a = payload();
        write(&s, &a);
        let mut b = a.clone();
        b.options.colors_name = "b".into();
        let mut writer = ThemeWriter::new(Some(s.dir()), s.root(), Some(a.clone()));
        let (entered, release, _) = hold(&mut writer);
        assert!(writer.remember(&b));
        entered.recv().unwrap();
        assert_eq!(load(&s.dir(), &s.root()), Loaded::Remembered(a));
        release.send(()).unwrap();
        writer.finish().unwrap().recv().unwrap();
        assert_eq!(load(&s.dir(), &s.root()), Loaded::Remembered(b));
        no_temporary_files(&s);
    }
    #[test]
    fn a_writer_without_a_state_dir_writes_nothing() {
        let s = Scratch::new();
        let mut writer = ThemeWriter::new(None, s.root(), None);
        assert!(!writer.remember(&payload()));
        assert!(writer.finish().is_none());
        assert!(!s.dir().exists());
    }
    #[test]
    fn a_failed_write_keeps_the_previous_file() {
        let s = Scratch::new();
        let a = payload();
        write(&s, &a);
        fifo(&state_paths::temporary(&path(&s.dir(), &s.root())));
        let mut b = a.clone();
        b.options.colors_name = "b".into();
        write(&s, &b);
        assert_eq!(load(&s.dir(), &s.root()), Loaded::Remembered(a));
        no_temporary_files(&s);
    }
    #[test]
    fn a_payload_over_the_cap_is_not_written() {
        let s = Scratch::new();
        let a = payload();
        write(&s, &a);
        let mut b = a.clone();
        b.options.guifont = "x".repeat(300 * 1024);
        let size = serde_json::to_string(
            &serde_json::json!({"version": 1, "project_root": s.root().to_string_lossy(), "payload": b}),
        )
        .unwrap()
        .len();
        assert!(encode(&s.root(), &b).unwrap_err().contains(&size.to_string()));
        write(&s, &b);
        assert_eq!(load(&s.dir(), &s.root()), Loaded::Remembered(a));
        no_temporary_files(&s);
    }
}
