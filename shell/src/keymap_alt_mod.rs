//! The start's optional Alt-modifier probe. The window reads its answer without waiting, and reads no file itself: the
//! worker reads the window manager's config files first and publishes what they say, then asks `swaymsg` or `i3-msg`,
//! whose answer replaces it. A keymap built while the command is still out is decided by the files, so a slow command
//! no longer leaves the Alt keys where they were for that run.

use std::ffi::OsString;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use eitri_core::keymap::{
    alt_mod::{self, Answer, Applied},
    root_conflicts,
};

type Run = Arc<dyn Fn(&[&str]) -> Result<String, String> + Send + Sync>;
type Read = Arc<dyn Fn(&Path) -> Option<String> + Send + Sync>;

struct Probe {
    answer: Arc<OnceLock<Answer>>,
    /// What the config files alone say, published by the worker before it runs the command.
    files: Arc<OnceLock<Answer>>,
    #[cfg(test)]
    worker: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Probe {
    fn start_with(
        full: bool,
        mac: bool,
        env: impl Fn(&str) -> Option<OsString> + Send + 'static,
        run: Run,
        read: Read,
    ) -> Self {
        let (answer, files) = (Arc::new(OnceLock::new()), Arc::new(OnceLock::new()));
        let probe = Self {
            answer: answer.clone(),
            files: files.clone(),
            #[cfg(test)]
            worker: std::sync::Mutex::new(None),
        };
        if !full || mac {
            return probe;
        }
        let Some(desktop) = alt_mod::detect(&env) else {
            return probe;
        };
        let spawned = std::thread::Builder::new()
            .name("keymap-alt-mod".into())
            .spawn(move || {
                let _ = files.set(alt_mod::from_texts(desktop, None, &env, &*read));
                let reply = run(desktop.argv()).ok();
                let found = alt_mod::from_texts(desktop, reply.as_deref(), &env, &*read);
                let _ = answer.set(found);
            });
        #[cfg(test)]
        {
            *probe.worker.lock().unwrap() = spawned.ok();
        }
        #[cfg(not(test))]
        if let Err(err) = spawned {
            eprintln!("[keymap] could not start the Alt $mod probe ({})", err.kind());
        }
        probe
    }

    fn now(&self) -> Option<Answer> {
        self.answer.get().copied()
    }

    /// The answer the keymap is built from, as it stands: the command's when it came, else the config files' while
    /// the command is still out. Neither yet (the worker has not read the files) answers nothing, logged.
    fn for_keymap(&self) -> Option<Answer> {
        if let Some(answer) = self.now() {
            return Some(answer);
        }
        let files = self.files.get().copied();
        if files.is_some() {
            eprintln!(
                "[keymap] the Alt $mod probe's command had not answered; the window manager's config files decide"
            );
        }
        files
    }

    #[cfg(test)]
    fn finish(&self) {
        if let Some(worker) = self.worker.lock().unwrap().take() {
            worker.join().unwrap();
        }
    }
}

static PROBE: OnceLock<Probe> = OnceLock::new();

/// Started before GTK, from the environment alone; only a full Linux window under sway or i3 asks anything.
pub(crate) fn start_probe(full: bool) {
    PROBE.get_or_init(|| {
        Probe::start_with(
            full,
            cfg!(target_os = "macos"),
            |name| std::env::var_os(name),
            Arc::new(|argv| crate::root_conflicts::run_capped(argv, Duration::from_millis(500), 1 << 20)),
            Arc::new(crate::onboarding_detect::read_regular_file),
        )
    });
}

pub(crate) fn probe_now() -> Option<Answer> {
    PROBE.get().and_then(Probe::for_keymap)
}

/// Check the notice record on a worker, then wait for both the first-start screen and the current toast. Remember
/// only when the toast actually goes up, so closing the window while it waits cannot swallow the announcement.
pub(crate) fn announce(
    applied: Option<Applied>,
    toast: std::rc::Rc<crate::toast::Toast>,
    ready: impl Fn() -> bool + 'static,
) {
    let Some(applied) = applied else { return };
    eprintln!("[keymap] {}", applied.text().replace("Alt,", "Alt:"));
    let (send, recv) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("keymap-alt-notice".into())
        .spawn(move || {
            let file = alt_mod::state_file(
                std::env::var_os("XDG_STATE_HOME").as_deref(),
                std::env::var_os("HOME").as_deref(),
            );
            let fingerprint = applied.fingerprint();
            let due = file
                .as_deref()
                .is_none_or(|file| root_conflicts::needs_notice(file, &fingerprint));
            let _ = send.send((due, file, fingerprint));
        });
    if let Err(err) = spawned {
        eprintln!("[keymap] could not check the Alt $mod notice ({})", err.kind());
        return;
    }
    let toast = std::rc::Rc::downgrade(&toast);
    gtk4::glib::timeout_add_local(Duration::from_millis(100), move || {
        let Some(toast) = toast.upgrade() else {
            return gtk4::glib::ControlFlow::Break;
        };
        if !ready() {
            return gtk4::glib::ControlFlow::Continue;
        }
        match recv.try_recv() {
            Ok((due, file, fingerprint)) => {
                if due {
                    toast.show_notice_when_free_then(applied.text(), move || {
                        if let Some(file) = file {
                            let spawned =
                                std::thread::Builder::new()
                                    .name("keymap-alt-remember".into())
                                    .spawn(move || {
                                        if let Err(err) = root_conflicts::remember(&file, &fingerprint) {
                                            eprintln!("[keymap] could not record the Alt $mod notice ({})", err.kind());
                                        }
                                    });
                            if let Err(err) = spawned {
                                eprintln!("[keymap] could not start the Alt $mod notice record ({})", err.kind());
                            }
                        }
                    });
                }
                gtk4::glib::ControlFlow::Break
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => gtk4::glib::ControlFlow::Continue,
            Err(_) => gtk4::glib::ControlFlow::Break,
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::keymap::alt_mod::Desktop;
    use eitri_core::onboarding::WmMod;
    use std::sync::{mpsc, Mutex};
    use std::time::Duration;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + Send + 'static {
        let pairs: Vec<_> = pairs.iter().map(|(k, v)| (k.to_string(), OsString::from(v))).collect();
        move |name| pairs.iter().find(|(key, _)| key == name).map(|(_, v)| v.clone())
    }

    #[test]
    fn keymap_alt_mod_probe_starts_only_for_sway_or_i3_full_linux_windows() {
        let cases: &[(&[(&str, &str)], bool, bool, Option<Desktop>)] = &[
            (&[("SWAYSOCK", "/s")], true, false, Some(Desktop::Sway)),
            (&[("I3SOCK", "/i")], true, false, Some(Desktop::I3)),
            (
                &[("HYPRLAND_INSTANCE_SIGNATURE", "h"), ("I3SOCK", "/i")],
                true,
                false,
                None,
            ),
            (&[("NIRI_SOCKET", "/n"), ("I3SOCK", "/i")], true, false, None),
            (
                &[("XDG_CURRENT_DESKTOP", "GNOME"), ("WAYLAND_DISPLAY", "wayland-1")],
                true,
                false,
                None,
            ),
            (&[], true, false, None),
            (&[("SWAYSOCK", "")], true, false, None),
            (&[("SWAYSOCK", "/s")], false, false, None),
            (&[("I3SOCK", "/i")], false, false, None),
            (&[("SWAYSOCK", "/s")], true, true, None),
        ];
        for (pairs, full, mac, expected) in cases {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let called = calls.clone();
            let probe = Probe::start_with(
                *full,
                *mac,
                env(pairs),
                Arc::new(move |argv| {
                    called.lock().unwrap().push(argv.join(" "));
                    Ok("set $mod Mod1\n".into())
                }),
                Arc::new(|_| None),
            );
            probe.finish();
            assert_eq!(
                probe.now().map(|a| a.desktop),
                *expected,
                "{pairs:?}, full={full}, mac={mac}"
            );
            let commands = calls.lock().unwrap();
            match expected {
                Some(Desktop::Sway) => assert_eq!(*commands, ["swaymsg -t get_config"]),
                Some(Desktop::I3) => assert_eq!(*commands, ["i3-msg -t get_config"]),
                None => assert!(commands.is_empty()),
            }
            if expected.is_some() {
                assert_eq!(probe.now().unwrap().modifier, Some(WmMod::Alt));
            }
        }
    }

    #[test]
    fn keymap_alt_mod_probe_never_waits_and_uses_files_after_a_failed_command() {
        let (began, started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let released = Mutex::new(released);
        let probe = Probe::start_with(
            true,
            false,
            env(&[("SWAYSOCK", "/s"), ("HOME", "/home/u")]),
            Arc::new(move |_| {
                began.send(()).unwrap();
                released.lock().unwrap().recv().unwrap();
                Err("untrusted failure".into())
            }),
            Arc::new(|path| (path == Path::new("/home/u/.config/sway/config")).then(|| "set $mod Mod1\n".into())),
        );
        assert_eq!(probe.now(), None);
        started
            .recv_timeout(Duration::from_secs(5))
            .expect("the worker started independently");
        assert_eq!(probe.now(), None);
        release.send(()).unwrap();
        probe.finish();
        assert_eq!(probe.now().unwrap().modifier, Some(WmMod::Alt));
    }

    /// A keymap built while the worker still waits on its command is decided by the config files the worker read
    /// first, so a slow `swaymsg` does not leave the Alt keys where they are for that run; once the command answers,
    /// its answer is used. The caller's thread (the GTK thread at start) reads no file.
    #[test]
    fn keymap_alt_mod_probe_answers_a_keymap_built_early_from_the_files() {
        let (began, started) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let released = Mutex::new(released);
        let caller = std::thread::current().id();
        let read_here = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = read_here.clone();
        let probe = Probe::start_with(
            true,
            false,
            env(&[("SWAYSOCK", "/s"), ("HOME", "/home/u")]),
            Arc::new(move |_| {
                began.send(()).unwrap();
                released.lock().unwrap().recv().unwrap();
                Ok("set $mod Mod4\n".into())
            }),
            Arc::new(move |path| {
                if std::thread::current().id() == caller {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                (path == Path::new("/home/u/.config/sway/config")).then(|| "set $mod Mod1\n".into())
            }),
        );
        started
            .recv_timeout(Duration::from_secs(60))
            .expect("the worker started independently");
        assert_eq!(probe.now(), None, "the worker is still asking");
        let early = probe.for_keymap().expect("the files answer");
        assert_eq!(early.desktop, Desktop::Sway);
        assert_eq!(early.modifier, Some(WmMod::Alt));
        release.send(()).unwrap();
        probe.finish();
        assert_eq!(
            probe.for_keymap().unwrap().modifier,
            Some(WmMod::Super),
            "the command's answer once it came"
        );
        assert!(
            !read_here.load(std::sync::atomic::Ordering::SeqCst),
            "no file was read on the caller's thread"
        );

        let none = Probe::start_with(
            true,
            false,
            env(&[]),
            Arc::new(|_| Ok(String::new())),
            Arc::new(|_| None),
        );
        assert_eq!(none.for_keymap(), None, "nothing is asked off sway and i3");
    }
}
