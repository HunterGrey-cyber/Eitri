//! A person's `NEOVIDE_*` variables are not obeyed by the embedded editor.
//!
//! The editor builds its settings through the pinned fork's own command-line parser, and that parser reads
//! `NEOVIDE_SERVER` (and about thirty more) from the process environment whatever arguments it is given.
//! With the variable naming a live nvim, an editor would attach to that nvim instead of starting its own, and
//! on close act on it. The window removes them at the start of `main` (`drop_inherited_editor_env`); this
//! drives that function against a real, listening nvim and reads the settings the editor would be built from.
//!
//! No display and no window: `CmdLineSettings::default()` is `parse_from(empty)`, the same parser the editor
//! runs with its extra nvim arguments. One test in this binary, because it changes the process environment.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use neovide::cmd_line::CmdLineSettings;

/// A headless nvim listening on a socket in a short private directory, ended and removed on drop.
struct ListeningNvim {
    child: Child,
    dir: PathBuf,
    socket: PathBuf,
}

impl ListeningNvim {
    fn start() -> Self {
        let dir = std::env::temp_dir().join(format!("nvenv-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let socket = dir.join("s");
        let child = Command::new("nvim")
            .args(["--clean", "--headless", "-n", "--listen"])
            .arg(&socket)
            .current_dir(&dir)
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("XDG_DATA_HOME", dir.join("data"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("nvim (>= 0.11) on PATH");
        let nvim = ListeningNvim { child, dir, socket };
        let deadline = Instant::now() + Duration::from_secs(10);
        while UnixStream::connect(&nvim.socket).is_err() {
            assert!(
                Instant::now() < deadline,
                "nvim never listened on {}",
                nvim.socket.display()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        nvim
    }
}

impl Drop for ListeningNvim {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn neovide_variables_in_the_environment_do_not_reach_the_editors_settings() {
    let mut nvim = ListeningNvim::start();
    let socket = nvim.socket.to_string_lossy().into_owned();

    // SAFETY: the only test in this binary, so no other test thread reads the environment; the nvim child
    // was spawned with its own variables.
    unsafe {
        std::env::set_var("NEOVIDE_SERVER", &socket);
        std::env::set_var("NEOVIDE_FORK", "1");
        std::env::set_var("NEOVIDE_NO_MULTIGRID", "1");
        std::env::set_var("NEOVIDE_CHDIR", "/definitely/not/a/project");
        // Eitri's own switch for the keyboard path: it must survive.
        std::env::set_var("NEOVIDE_EDITOR_LOG_KEYS", "1");
    }

    // The hazard, so the check below cannot pass on a fork that stopped reading the environment: the parser
    // does take these.
    let before = CmdLineSettings::default();
    assert_eq!(
        before.server.as_deref(),
        Some(socket.as_str()),
        "the fork no longer reads NEOVIDE_SERVER"
    );
    assert!(before.fork && before.no_multi_grid);
    assert_eq!(before.chdir.as_deref(), Some("/definitely/not/a/project"));

    eitri_core::companion::env::drop_inherited_editor_env();

    let after = CmdLineSettings::default();
    assert_eq!(after.server, None, "the editor would attach to the person's nvim");
    assert!(!after.fork, "NEOVIDE_FORK reached the editor");
    assert!(!after.no_multi_grid, "NEOVIDE_NO_MULTIGRID reached the editor");
    assert_eq!(after.chdir, None, "NEOVIDE_CHDIR reached the editor");
    for name in [
        "NEOVIDE_SERVER",
        "NEOVIDE_FORK",
        "NEOVIDE_NO_MULTIGRID",
        "NEOVIDE_CHDIR",
    ] {
        assert!(std::env::var_os(name).is_none(), "{name} is still set");
    }
    assert!(
        std::env::var_os("NEOVIDE_EDITOR_LOG_KEYS").is_some(),
        "Eitri's own debug switch was removed"
    );

    // The person's nvim is still theirs and still listening.
    assert!(
        nvim.child.try_wait().expect("try_wait").is_none(),
        "the person's nvim ended"
    );
    UnixStream::connect(&nvim.socket).expect("the person's nvim still listens");
}
