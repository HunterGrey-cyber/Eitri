use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use eitri_core::project_chooser::Fact;
use eitri_core::recent_projects::{read_with, Probe, RealProbe, Sources};
use eitri_core::{open_socket, state_paths};

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("rp-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
        fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn check(listen: bool) {
    let scratch = Scratch::new();
    let root = scratch.0.join("project");
    fs::create_dir(&root).unwrap();
    let sources = Sources::from_env(Some(scratch.0.as_os_str()), None, scratch.0.clone());
    let layout = sources.layout.as_ref().unwrap();
    state_paths::create_state_dir(layout).unwrap();
    agent::private_fs::write_private(
        &layout.join(format!("{}.pair.lock", state_paths::project_key(&root))),
        root.as_os_str().as_bytes(),
    )
    .unwrap();
    let socket = open_socket::socket_path(&sources.control, &root).unwrap();
    let listener = listen.then(|| UnixListener::bind(&socket).unwrap());
    let probe = RealProbe {
        control: sources.control.clone(),
    };
    assert_eq!(probe.window_holds(&root), listen);
    let flag = Box::leak(Box::new(AtomicBool::new(false)));
    let found = read_with(&sources, Box::new(probe), Duration::from_secs(5), flag);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].path, root);
    assert_eq!(found[0].fact, if listen { Fact::Open } else { Fact::Present });
    if let Some(listener) = listener {
        listener.set_nonblocking(true).unwrap();
        use std::io::Read;
        for _ in 0..2 {
            let (mut connection, _) = listener.accept().unwrap();
            let mut bytes = Vec::new();
            connection.read_to_end(&mut bytes).unwrap();
            assert!(bytes.is_empty());
        }
    }
}

#[test]
fn a_listening_socket_reads_open() {
    check(true);
}

#[test]
fn no_socket_reads_not_open() {
    check(false);
}
