//! `eitri_core::split` against a real `nvim --headless --listen`, started through a stand-in
//! `neovide` script: the command the split builds, the wait for the socket, and what the wait says
//! when the editor dies or never listens. The real Neovide is never started.
//!
//! `#[ignore]`d: needs `nvim` on PATH. No tokens, no network, no display.
//!
//! Run: `cargo test -p eitri-core --test split_with_real_nvim -- --ignored`

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use eitri_core::nvim_rpc::NvimLink;
use eitri_core::split::{fresh_socket_path, neovide_command, neovide_program, wait_for_socket};

/// What `neovide --no-fork -- --listen <sock>` runs here: the two leading words Neovide itself
/// would consume are dropped and the rest goes to a headless nvim.
const LISTENING_STUB: &str = "#!/bin/sh\n\
    while [ \"$1\" = --no-fork ] || [ \"$1\" = -- ]; do shift; done\n\
    exec nvim --headless --clean -n -i NONE \"$@\"\n";

/// A stand-in that takes the same arguments and never starts an editor.
const SILENT_STUB: &str = "#!/bin/sh\nexec sleep 30\n";

fn euid() -> u32 {
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    unsafe { libc::geteuid() }
}

/// The stub scripts live under the target directory; the socket may not (a bound path is capped at
/// 103 bytes and a worktree's target directory already uses most of that), so it goes under the
/// system temp dir with a short name.
struct Case {
    stubs: PathBuf,
    sockets: PathBuf,
    children: Vec<Child>,
}

impl Case {
    fn new(name: &str) -> Case {
        let stubs = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("split-{}-{name}", std::process::id()));
        let sockets = std::env::temp_dir().join(format!("sp-{}-{name}", std::process::id()));
        for dir in [&stubs, &sockets] {
            let _ = std::fs::remove_dir_all(dir);
            std::fs::create_dir_all(dir).unwrap();
        }
        Case {
            stubs,
            sockets,
            children: Vec::new(),
        }
    }

    fn stub(&self, text: &str) -> PathBuf {
        let path = self.stubs.join("neovide");
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn socket(&self, nonce: u32) -> PathBuf {
        let mut once = Some(nonce);
        fresh_socket_path(&self.sockets, &self.stubs, 1, &mut || once.take().unwrap()).unwrap()
    }

    /// Starts the stub as the split does, with nvim's own state kept in the scratch directory.
    fn spawn(&mut self, program: &Path, sock: &Path) -> &mut Child {
        let mut command = neovide_command(program, sock, &self.stubs);
        command
            .env("XDG_STATE_HOME", self.stubs.join("state"))
            .env("XDG_DATA_HOME", self.stubs.join("data"))
            .env("XDG_CONFIG_HOME", self.stubs.join("config"))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        self.children.push(command.spawn().expect("the stub starts"));
        self.children.last_mut().unwrap()
    }
}

impl Drop for Case {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.stubs);
        let _ = std::fs::remove_dir_all(&self.sockets);
    }
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn the_split_command_starts_an_nvim_that_answers_on_the_socket() {
    let mut case = Case::new("ok");
    let stub = case.stub(LISTENING_STUB);
    // The program is found through `EITRI_NEOVIDE`, as a user would name a Neovide.
    let named = stub.clone().into_os_string();
    let program = neovide_program(&|name| (name == "EITRI_NEOVIDE").then(|| named.clone()), &|_| None).unwrap();
    assert_eq!(program, stub);

    let sock = case.socket(0xabcd_0001);
    let child = case.spawn(&program, &sock);
    wait_for_socket(&sock, child, euid(), Duration::from_secs(10)).expect("nvim listens");

    let (link, _events) = NvimLink::connect(&sock, Duration::from_secs(2)).expect("connect");
    let answer = link
        .call("nvim_eval", vec![rmpv::Value::from("1+1")])
        .wait(Duration::from_secs(5))
        .expect("an answer in time")
        .expect("nvim_eval succeeds");
    assert_eq!(answer.as_i64(), Some(2));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_killed_editor_is_reported_as_an_early_exit() {
    let mut case = Case::new("kill");
    let stub = case.stub(LISTENING_STUB);
    let first = case.socket(0xabcd_0002);
    let child = case.spawn(&stub, &first);
    wait_for_socket(&first, child, euid(), Duration::from_secs(10)).expect("nvim listens");

    // The child's own pid is the nvim, because the stub `exec`s it. Killing it by that pid is the
    // editor dying.
    let pid = child.id();
    // SAFETY: `pid` is our own live child, captured above.
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
    let _ = child.wait();

    let second = case.socket(0xabcd_0003);
    let started = Instant::now();
    let child = case.children.last_mut().unwrap();
    let err = wait_for_socket(&second, child, euid(), Duration::from_secs(10)).unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(5), "reported late: {err}");
    assert!(err.starts_with("eitri split: neovide exited ("), "{err}");
    assert!(err.contains("before nvim listened on"), "{err}");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn an_editor_that_never_listens_times_out_and_keeps_running() {
    let mut case = Case::new("hang");
    let stub = case.stub(SILENT_STUB);
    let sock = case.socket(0xabcd_0004);
    let child = case.spawn(&stub, &sock);
    let started = Instant::now();
    let err = wait_for_socket(&sock, child, euid(), Duration::from_secs(1)).unwrap_err();
    let took = started.elapsed();
    assert!(
        took >= Duration::from_secs(1) && took < Duration::from_secs(4),
        "{took:?}"
    );
    assert!(err.contains("did not listen") && err.contains("within 1 s"), "{err}");
    assert!(err.ends_with("Neovide is left running"), "{err}");
    assert!(child.try_wait().unwrap().is_none(), "the stub must still run");
}
