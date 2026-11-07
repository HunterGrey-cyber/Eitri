//! The start's optional Alt-modifier probe. The window reads its answer without waiting.

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
        let answer = Arc::new(OnceLock::new());
        let probe = Self {
            answer: answer.clone(),
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
    PROBE.get().and_then(Probe::now)
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
}
