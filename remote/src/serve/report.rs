//! Who is connected, told to every Eitri window.
//!
//! When a phone's connection is admitted and when it closes, `serve` sends every window's `rv-` socket a
//! `devices_connected` line naming each phone connected now: its device, the `Host` its request named, the address it
//! came from and when. A window keeps the device's last sighting from it. The lines go out on a thread of their own,
//! so no phone's session waits for a window, and a report that finds a newer one queued behind it is skipped.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use eitri_core::remote_rv::{rv_socket_path, Connected, RvClient, RvInbound, RV_PROTOCOL};

use super::route::is_instance;

/// Sends the connected phones to every window; cheap to clone.
#[derive(Clone)]
pub struct Reporter {
    tx: Sender<Vec<Connected>>,
}

impl Reporter {
    /// Starts the reporting thread for the windows whose sockets are in `control_dir`.
    pub fn start(control_dir: &Path) -> std::io::Result<Reporter> {
        let (tx, rx) = mpsc::channel();
        let control = control_dir.to_owned();
        std::thread::Builder::new()
            .name("eitri-remote-report".into())
            .spawn(move || run(&control, rx))?;
        Ok(Reporter { tx })
    }

    /// Reports `now`, the phones connected now.
    pub fn report(&self, now: Vec<Connected>) {
        let _ = self.tx.send(now);
    }
}

fn run(control: &Path, rx: Receiver<Vec<Connected>>) {
    while let Ok(mut now) = rx.recv() {
        // Only the newest report matters.
        while let Ok(newer) = rx.try_recv() {
            now = newer;
        }
        let line = RvInbound::DevicesConnected {
            v: RV_PROTOCOL,
            devices: now,
        };
        // A window that cannot be reached is going away; it is told at the next report, if it stays.
        for socket in window_sockets(control) {
            if let Ok(mut client) = RvClient::connect(&socket) {
                if let Err(e) = client.send(&line) {
                    eprintln!("[remote] could not report the connected phones: {e}");
                }
            }
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

/// The instance ids of every window with an `rv-` socket in `control`, in order.
pub fn window_instances(control: &Path) -> Vec<String> {
    window_sockets(control)
        .iter()
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?;
            Some(name.strip_prefix("rv-")?.strip_suffix(".sock")?.to_owned())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
