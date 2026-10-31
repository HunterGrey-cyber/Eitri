//! The root-key conflict check: whether an input method or the desktop takes a root key before the window sees it.
//! `eitri_core::keymap::root_conflicts` judges; this module gathers the evidence and shows the result.
//!
//! Nothing runs unless `init.lua` bound a root key. The compositor's answer (`swaymsg -t get_config`, `hyprctl
//! binds -j`) and the session-bus check run on one worker thread, with a fixed argument list and no shell, at most
//! 1 MiB of output and the child killed after 3 s; the result comes back over a channel polled from the GTK thread
//! for at most 5 s. GNOME's shortcut settings are read on the GTK thread, and only in a GNOME session. A failure of
//! any step is one log line: a changed desktop must not stop the window from starting.

use std::ffi::OsString;
use std::io::Read;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use eitri_core::compositor::{self, Wm};
use eitri_core::keymap::root_conflicts::{self as judge, gnome, Source};
use eitri_core::keymap::{Binding, Chord};
use gtk4::gio::{self, prelude::*};
use gtk4::glib;

use crate::toast::Toast;

/// The longest a compositor command may run before it is killed.
const COMMAND_TIME: Duration = Duration::from_secs(3);
/// The most output read from a compositor command.
const OUTPUT_LIMIT: usize = 1 << 20;
/// How long a bus call may take.
const BUS_CALL_MS: i32 = 1000;
/// How long the GTK thread waits for the worker before giving up.
const WORKER_DEADLINE: Duration = Duration::from_secs(5);
const POLL_EVERY: Duration = Duration::from_millis(100);

/// What the worker found.
struct Gathered {
    /// The input-method names on the session bus that have an owner.
    bus_owners: Vec<&'static str>,
    desktop: Vec<(Source, Vec<(Chord, String)>)>,
    /// Why a step found nothing, one line each.
    notes: Vec<String>,
}

/// Starts the check for `root`; does nothing, and starts nothing, when the table is empty.
pub(crate) fn start(root: &[Binding], toast: Rc<Toast>) {
    if root.is_empty() {
        return;
    }
    let wm = compositor::detect(&|name| std::env::var_os(name));
    let root = root.to_vec();
    let (tx, rx) = mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("root-conflicts".to_string())
        .spawn(move || {
            let _ = tx.send(gather(wm));
        });
    if let Err(err) = spawned {
        eprintln!("[keys] root key check: could not start ({})", err.kind());
        return;
    }
    let started = Instant::now();
    glib::timeout_add_local(POLL_EVERY, move || match rx.try_recv() {
        Ok(gathered) => {
            finish(&root, wm, gathered, &toast);
            glib::ControlFlow::Break
        }
        Err(mpsc::TryRecvError::Empty) if started.elapsed() < WORKER_DEADLINE => glib::ControlFlow::Continue,
        Err(_) => {
            eprintln!("[keys] root key check: the desktop did not answer in time; skipped");
            glib::ControlFlow::Break
        }
    });
}

fn gather(wm: Wm) -> Gathered {
    let mut notes = Vec::new();
    let mut desktop = Vec::new();
    let found = match wm {
        Wm::Sway => Some((
            Source::Sway,
            run_capped(&["swaymsg", "-r", "-t", "get_config"], COMMAND_TIME, OUTPUT_LIMIT)
                .and_then(|text| judge::sway_chords(&text)),
        )),
        Wm::Hyprland => Some((
            Source::Hyprland,
            run_capped(&["hyprctl", "binds", "-j"], COMMAND_TIME, OUTPUT_LIMIT)
                .and_then(|text| judge::hyprland_chords(&text)),
        )),
        _ => None,
    };
    match found {
        Some((source, Ok(chords))) => desktop.push((source, chords)),
        Some((source, Err(why))) => notes.push(format!("{}: {why}", source.name())),
        None => {}
    }
    Gathered {
        bus_owners: bus_owners(&mut notes),
        desktop,
        notes,
    }
}

/// The input-method bus names that have an owner. No session bus (no address in the environment and no socket in
/// the runtime directory) skips the check; the bus is never started for it.
pub(crate) fn bus_owners(notes: &mut Vec<String>) -> Vec<&'static str> {
    let Some(address) = session_bus_address() else {
        return Vec::new();
    };
    let connection = match gio::DBusConnection::for_address_sync(
        &address,
        gio::DBusConnectionFlags::AUTHENTICATION_CLIENT | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
        None,
        gio::Cancellable::NONE,
    ) {
        Ok(connection) => connection,
        Err(_) => {
            notes.push("session bus: could not connect".to_string());
            return Vec::new();
        }
    };
    let mut owners = Vec::new();
    for name in judge::BUS_NAMES {
        let reply = connection.call_sync(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "NameHasOwner",
            Some(&(*name,).to_variant()),
            glib::VariantTy::new("(b)").ok(),
            gio::DBusCallFlags::NONE,
            BUS_CALL_MS,
            gio::Cancellable::NONE,
        );
        match reply {
            Ok(reply) => {
                if reply.get::<(bool,)>().is_some_and(|(owned,)| owned) {
                    owners.push(*name);
                }
            }
            Err(_) => {
                notes.push("session bus: no answer".to_string());
                break;
            }
        }
    }
    let _ = connection.close_sync(gio::Cancellable::NONE);
    owners
}

fn session_bus_address() -> Option<String> {
    session_bus_address_in(
        std::env::var_os("DBUS_SESSION_BUS_ADDRESS"),
        std::env::var_os("XDG_RUNTIME_DIR"),
        |path| path.exists(),
    )
}

fn session_bus_address_in(
    address: Option<OsString>,
    runtime_dir: Option<OsString>,
    exists: impl Fn(&std::path::Path) -> bool,
) -> Option<String> {
    if let Some(address) = address.filter(|a| !a.is_empty()) {
        return address.into_string().ok();
    }
    let socket = std::path::Path::new(&runtime_dir.filter(|d| !d.is_empty())?).join("bus");
    if !socket.is_absolute() || !exists(&socket) {
        return None;
    }
    Some(format!("unix:path={}", socket.to_str()?))
}

/// Runs `argv` with no shell and nothing on stdin, reads its standard output whole, and kills it by its pid when it
/// has not finished in `timeout` or has written more than `limit` bytes. An exit status other than 0 is an error.
pub(crate) fn run_capped(argv: &[&str], timeout: Duration, limit: usize) -> Result<String, String> {
    let (program, args) = argv.split_first().ok_or_else(|| "no command".to_string())?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("{program} could not start ({})", err.kind()))?;
    let deadline = Instant::now() + timeout;
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("{program} gave no output"));
    };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let read = stdout.take(limit as u64 + 1).read_to_end(&mut bytes);
        let _ = tx.send(read.map(|_| bytes));
    });
    let outcome = match rx.recv_timeout(timeout) {
        Ok(Ok(bytes)) if bytes.len() > limit => Err(format!("{program} wrote more than {limit} bytes")),
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(err)) => Err(format!("{program} output could not be read ({})", err.kind())),
        Err(_) => Err(format!("{program} did not answer in time")),
    };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if outcome.is_ok() && Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => break None,
        }
    };
    let Some(status) = status else {
        // Still running (finished reading early, too much output, or too slow), or not waitable: kill by pid.
        let _ = child.kill();
        let _ = child.wait();
        return outcome.and_then(|_| Err(format!("{program} did not exit in time")));
    };
    let bytes = outcome?;
    if !status.success() {
        return Err(format!("{program} failed"));
    }
    String::from_utf8(bytes).map_err(|_| format!("{program} output is not text"))
}

/// GNOME's live shortcuts: every `as` key of the shortcut schemas that are installed.
fn gnome_chords() -> Vec<(Chord, String)> {
    let Some(source) = gio::SettingsSchemaSource::default() else {
        return Vec::new();
    };
    let mut accels = Vec::new();
    for id in gnome::SCHEMAS {
        // A schema that is not installed, and a relocatable one (`Settings::new` aborts on it), are skipped.
        let Some(schema) = source.lookup(id, true) else {
            continue;
        };
        if schema.path().is_none() {
            continue;
        }
        let settings = gio::Settings::new(id);
        for key in schema.list_keys() {
            if schema.key(&key).value_type().as_str() != "as" {
                continue;
            }
            for accel in settings.strv(&key).iter() {
                accels.push((key.to_string(), accel.to_string()));
            }
        }
    }
    judge::gnome_chords(&accels)
}

fn finish(root: &[Binding], wm: Wm, gathered: Gathered, toast: &Rc<Toast>) {
    for note in &gathered.notes {
        eprintln!("[keys] root key check: {}", note.escape_debug());
    }
    let ims = judge::im_evidence(&|name| std::env::var_os(name), &gathered.bus_owners);
    let mut desktop = gathered.desktop;
    if judge::desktop_sources(wm).contains(&Source::Gnome) {
        desktop.push((Source::Gnome, gnome_chords()));
    }
    let found = judge::conflicts(root, &ims, &desktop);
    for conflict in &found {
        eprintln!("{}", judge::log_line(conflict));
    }
    let state = judge::state_file(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    );
    // A source that failed leaves a smaller set (or none): announcing or recording it would show the next good launch's
    // set as news again.
    if !gathered.notes.is_empty() {
        return;
    }
    let (text, error) = judge::announce(state.as_deref(), &found);
    if let Some(text) = text {
        toast.show_notice_when_free(text);
    }
    if let (Some(file), Some(err)) = (state.as_deref(), error) {
        eprintln!(
            "[keys] root key check: could not record the notice in {}: {err}",
            file.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(argv: &[&str]) -> Result<String, String> {
        run_capped(argv, Duration::from_secs(20), 1 << 16)
    }

    #[test]
    fn a_command_runs_without_a_shell_and_its_output_is_read_whole() {
        assert_eq!(out(&["printf", "%s", "a b;c"]).unwrap(), "a b;c");
        assert!(out(&["false"]).is_err(), "a failing command is an error");
        assert!(out(&["eitri-no-such-command"]).unwrap_err().contains("could not start"));
        assert!(out(&[]).is_err());
    }

    #[test]
    fn a_command_that_never_answers_is_killed_and_reaped() {
        let err = run_capped(&["sleep", "100000"], Duration::from_millis(300), 1 << 16).unwrap_err();
        assert!(err.contains("did not answer in time"), "{err}");
    }

    #[test]
    fn more_output_than_the_limit_is_an_error_and_stops_the_command() {
        let err = run_capped(&["yes"], Duration::from_secs(20), 4096).unwrap_err();
        assert!(err.contains("wrote more than 4096 bytes"), "{err}");
    }

    #[test]
    fn output_that_is_not_text_is_an_error() {
        assert!(out(&["printf", "\\377\\376"]).unwrap_err().contains("not text"));
    }

    #[test]
    fn the_session_bus_is_the_environments_address_or_the_runtime_sockets_and_never_started() {
        let some = |s: &str| Some(OsString::from(s));
        assert_eq!(
            session_bus_address_in(some("unix:path=/run/x/bus"), some("/run/user/1"), |_| false).as_deref(),
            Some("unix:path=/run/x/bus")
        );
        assert_eq!(
            session_bus_address_in(None, some("/run/user/1"), |p| p
                == std::path::Path::new("/run/user/1/bus"))
            .as_deref(),
            Some("unix:path=/run/user/1/bus")
        );
        assert_eq!(session_bus_address_in(some(""), some("/run/user/1"), |_| false), None);
        assert_eq!(session_bus_address_in(None, None, |_| true), None);
        assert_eq!(
            session_bus_address_in(None, some("run/user"), |_| true),
            None,
            "a relative directory is refused"
        );
    }
}
