//! Who is connected, told to every Eitri window.
//!
//! At startup, when a phone's connection is admitted and when it closes, `serve` sends every window's `rv-` socket a
//! `devices_connected` line naming each phone connected now: its device, the `Host` its request named, the address it
//! came from and when. A window keeps the device's last sighting from it. The lines go out on a thread of their own,
//! so no phone's session waits for a window, and a report that finds a newer one queued behind it is skipped.
//!
//! The same thread looks for windows every [`LOOK_EVERY`] between reports and tells each `rv-` socket that has not yet
//! taken the newest report (a window opened since, one whose socket was bound again, one that could not be reached),
//! so a window that opens while a phone is connected shows it without waiting for another phone to come or go.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use eitri_core::remote_rv::{rv_socket_path, Connected, RvClient, RvInbound, RV_PROTOCOL};

use super::route::is_instance;

/// How often the windows are looked for between reports.
pub const LOOK_EVERY: Duration = Duration::from_secs(1);

/// Sends the connected phones to every window; cheap to clone.
#[derive(Clone)]
pub struct Reporter {
    tx: Sender<ReportStep>,
}

enum ReportStep {
    Now(Vec<Connected>),
    #[cfg(feature = "remote-test-hooks")]
    Look(Sender<()>),
}

impl Reporter {
    /// Starts the reporting thread for the windows whose sockets are in `control_dir`.
    pub fn start(control_dir: &Path) -> std::io::Result<Reporter> {
        Self::start_with_interval(control_dir, LOOK_EVERY)
    }

    pub(crate) fn start_with_interval(control_dir: &Path, interval: Duration) -> std::io::Result<Reporter> {
        let (tx, rx) = mpsc::channel();
        // A new serve has no phones, even if a window still names phones from the previous process.
        tx.send(ReportStep::Now(Vec::new()))
            .expect("the report receiver is alive");
        let control = control_dir.to_owned();
        let epoch = boot_clock_ns();
        std::thread::Builder::new()
            .name("eitri-remote-report".into())
            .spawn(move || run(&control, epoch, rx, interval))?;
        Ok(Reporter { tx })
    }

    /// Reports `now`, the phones connected now.
    pub fn report(&self, now: Vec<Connected>) {
        let _ = self.tx.send(ReportStep::Now(now));
    }

    #[cfg(feature = "remote-test-hooks")]
    pub(crate) fn look_for_tests(&self) {
        let (tx, rx) = mpsc::channel();
        self.tx.send(ReportStep::Look(tx)).expect("the reporter ended");
        rx.recv_timeout(Duration::from_secs(60))
            .expect("the reporter did not finish its look");
    }
}

/// Nanoseconds on `CLOCK_BOOTTIME`: one clock for every process of the machine, which never goes back (a wall clock
/// set back would make a later `serve` look older than an earlier one) and restarts only with the machine, which no
/// window outlives.
fn boot_clock_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `clock_gettime` writes one `timespec` into the live value it is given.
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut ts) } != 0 {
        return 0;
    }
    (ts.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(ts.tv_nsec as u64)
}

/// A socket as one bind made it: its path, inode and change time, so a socket bound again at the same path is another.
type Bound = (PathBuf, u64, i64, i64);

fn bound(path: PathBuf) -> Option<Bound> {
    let meta = std::fs::symlink_metadata(&path).ok()?;
    Some((path, meta.ino(), meta.ctime(), meta.ctime_nsec()))
}

fn run(control: &Path, epoch: u64, rx: Receiver<ReportStep>, interval: Duration) {
    // The newest report, the number the next one gets, and the sockets that have taken the newest.
    let mut newest: Option<RvInbound> = None;
    let mut seq = 0u64;
    let mut told: BTreeSet<Bound> = BTreeSet::new();
    loop {
        #[cfg(feature = "remote-test-hooks")]
        let mut looks = Vec::new();
        match rx.recv_timeout(interval) {
            Ok(step) => {
                let mut now = None;
                let mut take = |step| match step {
                    ReportStep::Now(report) => now = Some(report),
                    #[cfg(feature = "remote-test-hooks")]
                    ReportStep::Look(done) => looks.push(done),
                };
                take(step);
                // Only the newest report matters.
                while let Ok(newer) = rx.try_recv() {
                    take(newer);
                }
                if let Some(now) = now {
                    newest = Some(RvInbound::DevicesConnected {
                        v: RV_PROTOCOL,
                        devices: now,
                        epoch,
                        seq,
                    });
                    seq += 1;
                    told.clear();
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        // Startup queues an empty report; subsequent reports replace it through the same path.
        let Some(line) = &newest else { continue };
        let present: Vec<Bound> = window_sockets(control).into_iter().filter_map(bound).collect();
        told.retain(|socket| present.contains(socket));
        for socket in present {
            if told.contains(&socket) {
                continue;
            }
            // A window that cannot be reached is going away, or not listening yet: it is tried at the next look.
            if let Ok(mut client) = RvClient::connect(&socket.0) {
                if let Err(e) = client.send(line) {
                    eprintln!("[remote] could not report the connected phones: {e}");
                }
                told.insert(socket);
            }
        }
        #[cfg(feature = "remote-test-hooks")]
        for done in looks {
            let _ = done.send(());
        }
    }
}

/// The `rv-` socket of every window in `control`.
pub fn window_sockets(control: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(control) else {
        return Vec::new();
    };
    let mut found = BTreeMap::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(instance) = name.strip_prefix("rv-").and_then(|rest| rest.strip_suffix(".sock")) else {
            continue;
        };
        if !is_instance(instance) {
            continue;
        }
        if let Ok(path) = rv_socket_path(control, instance) {
            found.insert(instance.to_owned(), path);
        }
    }
    found.into_values().collect()
}

/// The instance ids of the windows with remote control on now, in order: an `rv-` socket whose window still runs (its
/// rows file names a live process). A socket a crashed window left behind is no window. Nothing is connected to, so no
/// window is woken to find out; when the rows cannot be read, a socket that takes a connection counts instead.
pub fn live_windows(control: &Path) -> Vec<String> {
    let running: BTreeSet<String> = match eitri_core::agents::scan_windows(control, eitri_core::agents::SCHEMA_V) {
        Ok(windows) => windows.into_iter().map(|(_, window)| window.instance).collect(),
        Err(e) => {
            eprintln!("[remote] the windows were not read ({e}); asking their sockets");
            return window_sockets(control)
                .iter()
                .filter(|socket| std::os::unix::net::UnixStream::connect(socket).is_ok())
                .filter_map(|socket| instance_of(socket))
                .collect();
        }
    };
    window_instances(control)
        .into_iter()
        .filter(|instance| running.contains(instance))
        .collect()
}

fn instance_of(socket: &Path) -> Option<String> {
    let name = socket.file_name()?.to_str()?;
    Some(name.strip_prefix("rv-")?.strip_suffix(".sock")?.to_owned())
}

/// The instance ids of every window with an `rv-` socket in `control`, in order.
pub fn window_instances(control: &Path) -> Vec<String> {
    window_sockets(control)
        .iter()
        .filter_map(|path| instance_of(path))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_serve_reports_nobody_to_existing_and_later_windows() {
        use std::io::{BufRead, BufReader};
        use std::os::unix::net::UnixListener;
        let dir = agent::state_dirs::redirect_state_to_a_test_root().join("report-start");
        let _ = std::fs::remove_dir_all(&dir);
        eitri_core::panel_control::ensure_dir(&dir).unwrap();
        let first = UnixListener::bind(rv_socket_path(&dir, "a3f09c1d2e4b5f60").unwrap()).unwrap();
        let reporter = Reporter::start(&dir).unwrap();
        let empty = |listener: &UnixListener| {
            listener.set_nonblocking(true).unwrap();
            let until = std::time::Instant::now() + Duration::from_secs(30);
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(std::time::Instant::now() < until, "no startup report arrived");
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            stream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line).unwrap();
            assert_eq!(
                eitri_core::remote_rv::parse_rv_line(line.trim_end().as_bytes()).unwrap(),
                RvInbound::DevicesConnected {
                    v: RV_PROTOCOL,
                    devices: Vec::new(),
                    epoch: match eitri_core::remote_rv::parse_rv_line(line.trim_end().as_bytes()).unwrap() {
                        RvInbound::DevicesConnected { epoch, .. } => epoch,
                        other => panic!("{other:?}"),
                    },
                    seq: 0,
                }
            );
        };
        empty(&first);
        let later = UnixListener::bind(rv_socket_path(&dir, "b4e1c09a7d3f5e21").unwrap()).unwrap();
        empty(&later);
        drop(reporter);
        drop((first, later));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn window_sockets_are_the_rv_sockets_of_instances() {
        let dir =
            PathBuf::from(option_env!("CARGO_TARGET_DIR").unwrap_or(concat!(env!("CARGO_MANIFEST_DIR"), "/../target")))
                .join("tmp/remote-tests")
                .join(format!("report-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "rv-a3f09c1d2e4b5f60.sock",
            "rv-b4e1c09a7d3f5e21.sock",
            "ag-a3f09c1d2e4b5f60.sock",
            "rv-NOTANINSTANCE00.sock",
            "rv-a3f09c1d2e4b5f6.sock",
            "remote.sock",
        ] {
            std::fs::write(dir.join(name), b"").unwrap();
        }
        assert_eq!(
            window_instances(&dir),
            vec!["a3f09c1d2e4b5f60".to_owned(), "b4e1c09a7d3f5e21".to_owned()]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
