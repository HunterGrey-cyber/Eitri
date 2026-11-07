//! Remote control in a window: with `remote.enabled` on, a window that shows chats and shares its agents binds its
//! `rv-` socket and starts `eitri remote serve` when none runs; with it off nothing here runs and nothing is read.
//!
//! The decision is `eitri_core::remote_runtime::decide`, read by `eitri_remote::serve::spawn::plan`. Everything that
//! touches a file or a socket runs on a thread of its own: the decision, the bind, the desk's question about a named
//! address, the start of `serve`. The bound socket comes back to the GTK thread through the main loop and is handed to
//! the panel, which pumps it on its tick and closes it when the window goes. The keys are read once, when the window
//! starts: `eitri remote stop` stays stopped until a window starts again.
//!
//! A window that finds `serve` running pairs phones with what that `serve` listens on (its record), not with its own
//! keys, which may have changed since it started. And it looks once more [`IDLE_EDGE`] later: a `serve` that had just
//! seen no window, and so ends, may have held the lock when this window looked, and this window would otherwise be left
//! with no `serve` until another window starts. That second look starts nothing when `eitri remote stop` ran after the
//! first (`serve::stopped_since`): an explicit stop is not the idle exit it is there for.
//!
//! What keeps a window that asked for remote control from having it is said on the window's toast as well as on
//! stderr, since a window started from the desktop has no terminal: the agents layer off, a socket or directory that
//! could not be made, a `serve` already running with other keys, the desk's no to an address, and a `serve` that could
//! not start or ended with an error. Each line goes from the thread to the GTK thread through the main loop.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use eitri_core::lua::config::{ConfigStore, RemoteSettings};
use eitri_core::remote_pairing::{DEVICES_FILE, ENROLL_CODE_TTL};
use eitri_core::remote_runtime::{pairing_routes, start_serve, RemotePlan, ServeConfig};
use eitri_core::remote_rv::RV_IDLE;
use eitri_core::remote_rv::{parse_rv_outbound, rv_socket_path, RvClient, RvInbound, RvOutbound, RV_PROTOCOL};
use eitri_panel::agent_panel::{Pairing, RemoteSocket};
use eitri_remote::serve::spawn::{plan, running_config, ServeEnded, ServeSpawner};
use eitri_remote::serve::{stopped_since, Dirs};
use gtk4::glib;

use crate::agent_panel::AgentPanelHandle;

/// Where the desk's SSH host keys are read for a pairing through the SSH recipe.
const SSH_HOST_KEYS_DIR: &str = "/etc/ssh";

/// How often the connection that waits for the desk's answer about an address says it is still there: well inside the
/// `rv-` socket's idle deadline.
const ASK_KEEPALIVE: Duration = Duration::from_secs(20);
const _: () = assert!(ASK_KEEPALIVE.as_secs() * 2 < RV_IDLE.as_secs());

/// How long after finding `serve` running a window looks again: `serve` ends at once after the look that found no
/// window, well inside this.
const IDLE_EDGE: Duration = Duration::from_secs(10);

/// What a window that asked for remote control says when the agents layer, which remote control follows, is off.
const AGENTS_OFF: &str = "off: it follows the agents layer, and agents.enabled is false";

/// A line for the desk, sent from a thread of this module to the window's toast.
type Notes = futures_channel::mpsc::UnboundedSender<String>;

/// The desk's notices on `toast`, each after what is up there now; nothing once the window is gone.
pub(crate) fn toast_notice(toast: &Rc<crate::toast::Toast>) -> Rc<dyn Fn(String)> {
    let toast = Rc::downgrade(toast);
    Rc::new(move |text| {
        if let Some(toast) = toast.upgrade() {
            toast.show_notice_when_free(text);
        }
    })
}

/// Says `text` on stderr as a `[remote]` line and sends it to the desk.
fn say(notes: &Notes, text: String) {
    eprintln!("[remote] {text}");
    let _ = notes.unbounded_send(format!("Remote control: {text}"));
}

/// Switches remote control on for the window whose panel is `panel` and whose agents instance is `instance` (`None`
/// with the agents layer off), when `config` says so. What keeps it off, or keeps phones from reaching the desk, goes
/// to `notice` on the GTK thread.
pub(crate) fn start(
    panel: &AgentPanelHandle,
    instance: Option<&str>,
    config: &ConfigStore,
    notice: Rc<dyn Fn(String)>,
) {
    let settings = match RemoteSettings::read(config) {
        Ok(settings) => settings,
        Err(e) => {
            // A bad key is among the window's startup problems already.
            eprintln!("[remote] off: {e}");
            return;
        }
    };
    if !settings.enabled {
        return;
    }
    let (notes, mut noted) = futures_channel::mpsc::unbounded::<String>();
    glib::spawn_future_local(async move {
        while let Ok(text) = noted.recv().await {
            notice(text);
        }
    });
    let Some(instance) = instance else {
        say(&notes, AGENTS_OFF.to_owned());
        return;
    };
    let dirs = match Dirs::from_env() {
        Ok(dirs) => dirs,
        Err(e) => {
            say(&notes, format!("off: {e}"));
            return;
        }
    };
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            say(&notes, format!("off: this program's own path is unknown: {e}"));
            return;
        }
    };
    let wake = panel.remote_waker();
    let (socket_tx, socket_rx) = futures_channel::oneshot::channel::<RemoteSocket>();
    let instance = instance.to_owned();
    let thread_notes = notes.clone();
    let spawned = std::thread::Builder::new()
        .name("eitri-remote-start".into())
        .spawn(move || switch_on(&settings, &dirs, &instance, exe, wake, socket_tx, &thread_notes));
    if let Err(e) = spawned {
        say(&notes, format!("off: could not start: {e}"));
        return;
    }
    let panel = panel.clone();
    glib::spawn_future_local(async move {
        if let Ok(socket) = socket_rx.await {
            panel.set_remote(socket);
        }
    });
}

/// The thread's work: decide, bind, hand the socket over, ask the desk if the address needs it, start `serve`.
fn switch_on(
    settings: &RemoteSettings,
    dirs: &Dirs,
    instance: &str,
    exe: PathBuf,
    wake: agent::UiWake,
    socket_to: futures_channel::oneshot::Sender<RemoteSocket>,
    notes: &Notes,
) {
    // Taken before the first look, so a stop after it is seen by the second.
    let looked = SystemTime::now();
    let mut now = plan(settings, dirs);
    let RemotePlan::On {
        bind_rv: true,
        ask_listen,
        ..
    } = &now
    else {
        return;
    };
    let ask_listen = ask_listen.clone();
    if let Err(e) = eitri_core::panel_control::ensure_dir(&dirs.control) {
        say(notes, format!("off: {e}"));
        return;
    }
    if let Err(e) = eitri_core::state_paths::create_state_dir(&dirs.state) {
        say(notes, format!("off: could not make {}: {e}", dirs.state.display()));
        return;
    }
    let socket = match RemoteSocket::bind_waking(&dirs.control, instance, &dirs.state.join(DEVICES_FILE), wake) {
        Ok(socket) => socket,
        Err(e) => {
            say(notes, format!("off: could not bind this window's rv- socket: {e}"));
            return;
        }
    };
    let cfg = ServeConfig::from_settings(settings);
    // Only a running `serve` makes the decision start nothing and ask nothing.
    let found_running = matches!(
        now,
        RemotePlan::On {
            start_serve: None,
            ask_listen: None,
            ..
        }
    );
    let paired = if found_running {
        paired_with_running(dirs, &cfg, notes)
    } else {
        cfg.clone()
    };
    let port = match paired.port {
        Some(port) => Ok(port),
        None => eitri_remote::serve::stable_port(&dirs.state),
    };
    match port.and_then(|port| pairing_routes(&paired, port, &dirs.control)) {
        Ok(routes) => socket.set_pairing(Pairing {
            routes,
            ssh_dir: PathBuf::from(SSH_HOST_KEYS_DIR),
        }),
        Err(e) => say(
            notes,
            format!("this window cannot enroll phones: no port for serve: {e}"),
        ),
    }
    if socket_to.send(socket).is_err() {
        // The window went while this ran; the socket closed with the value.
        return;
    }
    println!("[remote] on: this window's tabs can be watched through eitri remote serve");
    let mut ask_listen = ask_listen;
    if found_running {
        std::thread::sleep(IDLE_EDGE);
        if stopped_since(dirs, looked) {
            println!("[remote] eitri remote stop ran: serve stays stopped until a window starts");
            return;
        }
        now = plan(settings, dirs);
        ask_listen = match &now {
            RemotePlan::On { ask_listen, .. } => ask_listen.clone(),
            RemotePlan::Off => None,
        };
    }
    if let Some(addr) = ask_listen {
        match ask_desk(dirs, instance, &addr) {
            Ok(true) => now = plan(settings, dirs),
            Ok(false) => {
                say(
                    notes,
                    format!("serve not started: the desk did not say yes to listening on {addr}"),
                );
                return;
            }
            Err(e) => {
                say(
                    notes,
                    format!("serve not started: the desk could not be asked about {addr}: {e}"),
                );
                return;
            }
        }
    }
    let spawner = ServeSpawner::new(exe).on_end(on_serve_end(dirs.clone(), cfg.clone(), notes.clone()));
    match start_serve(&now, &spawner) {
        Ok(true) => println!("[remote] started eitri remote serve {}", cfg.args().join(" ")),
        Ok(false) => {}
        Err(e) => say(notes, format!("could not start eitri remote serve: {e}")),
    }
}

/// What the reaper of a `serve` this window started tells the desk: a `serve` with other keys found running (named
/// key by key, from its record), or one that ended with an error.
fn on_serve_end(dirs: Dirs, own: ServeConfig, notes: Notes) -> eitri_remote::serve::spawn::OnServeEnd {
    let notes = std::sync::Mutex::new(notes);
    Arc::new(move |ended| {
        let differences = || running_config(&dirs).map(|running| running.differences(&own));
        if let Some(text) = ended_text(&ended, differences) {
            // The reaper said its own line on stderr already.
            let _ = notes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .unbounded_send(format!("Remote control: {text}"));
        }
    })
}

/// The desk's line for how a `serve` this window started ended, if it needs one; `differences` reads how the `serve`
/// running instead differs from this window's keys.
fn ended_text(ended: &ServeEnded, differences: impl FnOnce() -> Option<Vec<String>>) -> Option<String> {
    match ended {
        ServeEnded::RunningDifferent => Some(mismatch_text(&differences().unwrap_or_default())),
        ServeEnded::Failed(code) => Some(format!(
            "eitri remote serve ended with exit {code}, so phones cannot reach this desk (the window's log says why)"
        )),
        ServeEnded::Idle | ServeEnded::RunningSame | ServeEnded::Signalled | ServeEnded::Unknown => None,
    }
}

/// The line for a `serve` already running with other keys than this window's.
fn mismatch_text(differences: &[String]) -> String {
    let named = if differences.is_empty() {
        String::new()
    } else {
        format!(" ({})", differences.join("; "))
    };
    format!(
        "eitri remote serve already runs with other keys than this window's{named}; phones pair with what runs, and \
         this window's take effect after eitri remote stop and a new window"
    )
}

/// What phones are paired with when `serve` already runs: its configuration as recorded, said to the desk when it is
/// not this window's own; this window's own while the record is not whole yet.
fn paired_with_running(dirs: &Dirs, own: &ServeConfig, notes: &Notes) -> ServeConfig {
    let Some(running) = running_config(dirs) else {
        return own.clone();
    };
    let differences = running.differences(own);
    if !differences.is_empty() {
        say(notes, mismatch_text(&differences));
    }
    running
}

/// Asks this window's desk whether `serve` may listen on `addr`, through its own `rv-` socket, and waits for the
/// answer as long as the desk's question waits.
fn ask_desk(dirs: &Dirs, instance: &str, addr: &str) -> std::io::Result<bool> {
    ask_desk_at(
        &rv_socket_path(&dirs.control, instance)?,
        addr,
        ENROLL_CODE_TTL + Duration::from_secs(30),
    )
}

/// [`ask_desk`] through the `rv-` socket at `socket`, waiting at most `within`.
fn ask_desk_at(socket: &std::path::Path, addr: &str, within: Duration) -> std::io::Result<bool> {
    let mut client = RvClient::connect(socket)?;
    client.send(&RvInbound::ListenRequest {
        v: RV_PROTOCOL,
        addr: addr.to_owned(),
    })?;
    let until = Instant::now() + within;
    while Instant::now() < until {
        let wait = ASK_KEEPALIVE.min(until.saturating_duration_since(Instant::now()));
        let Some(line) = client.recv(wait)? else {
            client.send(&RvInbound::Ping { v: RV_PROTOCOL })?;
            continue;
        };
        match parse_rv_outbound(line.as_bytes()) {
            Ok(RvOutbound::ListenAnswer {
                addr: answered, yes, ..
            }) if answered == addr => return Ok(yes),
            Ok(RvOutbound::Refused { .. }) => return Ok(false),
            _ => {}
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::remote_rv::parse_rv_line;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            PathBuf::from(option_env!("CARGO_TARGET_DIR").unwrap_or(concat!(env!("CARGO_MANIFEST_DIR"), "/../target")))
                .join("tmp/remote-switch-tests")
                .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A stand-in for a window's `rv-` socket: reads the one request it is sent, then writes `answers` back.
    fn window_answering(socket: &std::path::Path, answers: Vec<String>) -> std::thread::JoinHandle<String> {
        let listener = UnixListener::bind(socket).unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            let mut writer = stream;
            for answer in answers {
                writer.write_all(format!("{answer}\n").as_bytes()).unwrap();
            }
            // Held open until the asker hangs up.
            let mut rest = String::new();
            let _ = reader.read_line(&mut rest);
            request
        })
    }

    fn line(out: RvOutbound) -> String {
        out.line()
    }

    #[test]
    fn the_desk_hears_of_a_serve_with_other_keys_or_one_that_failed() {
        let named = vec!["listen: running loopback, this window socket".to_owned()];
        let text = ended_text(&ServeEnded::RunningDifferent, || Some(named.clone())).unwrap();
        assert!(
            text.contains("(listen: running loopback, this window socket)"),
            "{text}"
        );
        assert!(text.contains("eitri remote stop"), "{text}");
        // Its record not read: still said, without the list.
        let text = ended_text(&ServeEnded::RunningDifferent, || None).unwrap();
        assert!(
            text.starts_with("eitri remote serve already runs with other keys than this window's;"),
            "{text}"
        );
        let text = ended_text(&ServeEnded::Failed(2), || unreachable!()).unwrap();
        assert!(text.contains("exit 2"), "{text}");
        for quiet in [
            ServeEnded::Idle,
            ServeEnded::RunningSame,
            ServeEnded::Signalled,
            ServeEnded::Unknown,
        ] {
            assert_eq!(ended_text(&quiet, || unreachable!()), None, "{quiet:?}");
        }
    }

    #[test]
    fn a_window_paired_with_a_running_serve_tells_the_desk_how_its_keys_differ() {
        let dir = scratch("differs");
        let dirs = Dirs {
            control: dir.join("control"),
            state: dir.join("state"),
        };
        eitri_core::panel_control::ensure_dir(&dirs.control).unwrap();
        let record = dirs.control.join(eitri_remote::serve::RECORD_FILE);
        let mut file = agent::private_fs::open_private(&record).unwrap();
        std::io::Write::write_all(
            &mut file,
            br#"{"v":1,"pid":1,"args":["--listen","loopback","--port","7500"],"sandbox":"full"}"#,
        )
        .unwrap();
        let own = |listen| ServeConfig {
            listen,
            port: None,
            origins: Vec::new(),
        };
        let (notes, mut noted) = futures_channel::mpsc::unbounded::<String>();
        let paired = paired_with_running(&dirs, &own(eitri_core::lua::config::RemoteListen::Loopback), &notes);
        assert_eq!(paired.port, Some(7500));
        assert!(noted.try_recv().is_err(), "the same keys say nothing");
        let paired = paired_with_running(&dirs, &own(eitri_core::lua::config::RemoteListen::Socket), &notes);
        assert_eq!(
            paired.listen,
            eitri_core::lua::config::RemoteListen::Loopback,
            "paired with what runs"
        );
        let text = noted.try_recv().unwrap();
        assert!(
            text.starts_with("Remote control: eitri remote serve already runs"),
            "{text}"
        );
        assert!(text.contains("listen: running loopback, this window socket"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_desk_is_asked_about_the_address_and_its_answer_read() {
        let dir = scratch("ask");
        let socket = dir.join("rv.sock");
        let window = window_answering(
            &socket,
            vec![
                line(RvOutbound::Pong { v: RV_PROTOCOL }),
                // An answer about another address is not this one's.
                line(RvOutbound::ListenAnswer {
                    v: RV_PROTOCOL,
                    addr: "198.51.100.9".into(),
                    yes: true,
                }),
                line(RvOutbound::ListenAnswer {
                    v: RV_PROTOCOL,
                    addr: "198.51.100.2".into(),
                    yes: true,
                }),
            ],
        );
        assert!(ask_desk_at(&socket, "198.51.100.2", Duration::from_secs(30)).unwrap());
        let request = window.join().unwrap();
        assert_eq!(
            parse_rv_line(request.trim_end().as_bytes()).unwrap(),
            RvInbound::ListenRequest {
                v: RV_PROTOCOL,
                addr: "198.51.100.2".into()
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_refusal_or_no_answer_in_time_is_a_no() {
        let dir = scratch("no");
        let socket = dir.join("rv.sock");
        let window = window_answering(
            &socket,
            vec![line(RvOutbound::Refused {
                v: RV_PROTOCOL,
                key: None,
                why: "the desk is already asked about an address".into(),
            })],
        );
        assert!(!ask_desk_at(&socket, "198.51.100.2", Duration::from_secs(30)).unwrap());
        window.join().unwrap();
        std::fs::remove_file(&socket).unwrap();
        let silent = window_answering(&socket, Vec::new());
        assert!(!ask_desk_at(&socket, "198.51.100.2", Duration::from_millis(200)).unwrap());
        silent.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
