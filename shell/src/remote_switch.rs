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
//! with no `serve` until another window starts.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eitri_core::lua::config::{ConfigStore, RemoteSettings};
use eitri_core::remote_pairing::{DEVICES_FILE, ENROLL_CODE_TTL};
use eitri_core::remote_runtime::{pairing_endpoint, start_serve, RemotePlan, ServeConfig};
use eitri_core::remote_rv::RV_IDLE;
use eitri_core::remote_rv::{parse_rv_outbound, rv_socket_path, RvClient, RvInbound, RvOutbound, RV_PROTOCOL};
use eitri_panel::agent_panel::{Pairing, RemoteSocket};
use eitri_remote::serve::spawn::{plan, running_config, ServeSpawner};
use eitri_remote::serve::Dirs;
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

/// Switches remote control on for the window whose panel is `panel` and whose agents instance is `instance`, when
/// `config` says so.
pub(crate) fn start(panel: &AgentPanelHandle, instance: &str, config: &ConfigStore) {
    let settings = match RemoteSettings::read(config) {
        Ok(settings) => settings,
        Err(e) => {
            eprintln!("[remote] off: {e}");
            return;
        }
    };
    if !settings.enabled {
        return;
    }
    let dirs = match Dirs::from_env() {
        Ok(dirs) => dirs,
        Err(e) => {
            eprintln!("[remote] off: {e}");
            return;
        }
    };
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            eprintln!("[remote] off: this program's own path is unknown: {e}");
            return;
        }
    };
    let wake = panel.remote_waker();
    let (socket_tx, socket_rx) = futures_channel::oneshot::channel::<RemoteSocket>();
    let instance = instance.to_owned();
    let spawned = std::thread::Builder::new()
        .name("eitri-remote-start".into())
        .spawn(move || switch_on(&settings, &dirs, &instance, exe, wake, socket_tx));
    if let Err(e) = spawned {
        eprintln!("[remote] off: could not start: {e}");
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
) {
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
        eprintln!("[remote] off: {e}");
        return;
    }
    if let Err(e) = eitri_core::state_paths::create_state_dir(&dirs.state) {
        eprintln!("[remote] off: could not make {}: {e}", dirs.state.display());
        return;
    }
    let socket = match RemoteSocket::bind_waking(&dirs.control, instance, &dirs.state.join(DEVICES_FILE), wake) {
        Ok(socket) => socket,
        Err(e) => {
            eprintln!("[remote] off: could not bind this window's rv- socket: {e}");
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
        paired_with_running(dirs, &cfg)
    } else {
        cfg.clone()
    };
    let port = match paired.port {
        Some(port) => Ok(port),
        None => eitri_remote::serve::stable_port(&dirs.state),
    };
    match port.and_then(|port| pairing_endpoint(&paired, port, &dirs.control)) {
        Ok(endpoint) => socket.set_pairing(Pairing {
            endpoint,
            ssh_dir: PathBuf::from(SSH_HOST_KEYS_DIR),
        }),
        Err(e) => eprintln!("[remote] this window cannot enroll phones: no port for serve: {e}"),
    }
    if socket_to.send(socket).is_err() {
        // The window went while this ran; the socket closed with the value.
        return;
    }
    println!("[remote] on: this window's tabs can be watched through eitri remote serve");
    let mut ask_listen = ask_listen;
    if found_running {
        std::thread::sleep(IDLE_EDGE);
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
                eprintln!("[remote] serve not started: the desk did not say yes to listening on {addr}");
                return;
            }
            Err(e) => {
                eprintln!("[remote] serve not started: the desk could not be asked about {addr}: {e}");
                return;
            }
        }
    }
    match start_serve(&now, &ServeSpawner::new(exe)) {
        Ok(true) => println!("[remote] started eitri remote serve {}", cfg.args().join(" ")),
        Ok(false) => {}
        Err(e) => eprintln!("[remote] could not start eitri remote serve: {e}"),
    }
}

/// What phones are paired with when `serve` already runs: its configuration as recorded, said when it is not this
/// window's own; this window's own while the record is not whole yet.
fn paired_with_running(dirs: &Dirs, own: &ServeConfig) -> ServeConfig {
    let Some(running) = running_config(dirs) else {
        return own.clone();
    };
    let differs = running.listen != own.listen || own.port.is_some_and(|port| running.port != Some(port));
    if differs {
        eprintln!(
            "[remote] eitri remote serve already runs with {}; this window's remote.* keys ({}) take effect after              eitri remote stop and a new window",
            running.args().join(" "),
            own.args().join(" ")
        );
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
