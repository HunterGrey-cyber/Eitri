use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use eitri_core::nvim_rpc::NvimLink;
use rmpv::Value;

pub struct TmuxTui {
    pub dir: PathBuf,
    socket: PathBuf,
    addr: PathBuf,
    server: u32,
    tui: u32,
    starts: [Option<u64>; 2],
}

pub fn wait_until(mut condition: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(6);
    loop {
        if condition() {
            return;
        }
        assert!(Instant::now() < end, "the terminal did not reach the expected state");
        std::thread::sleep(Duration::from_millis(25));
    }
}

impl TmuxTui {
    pub fn start(nvim: &Path) -> TmuxTui {
        let available = Command::new("tmux").arg("-V").output();
        assert!(available.is_ok_and(|o| o.status.success()), "NOT RUN: tmux is required");
        let dir = std::env::temp_dir().join(format!("takeover-{}", &uuid::Uuid::new_v4().simple().to_string()[..6]));
        agent::private_fs::create_private_dir_all(&dir, &dir).unwrap();
        let socket = agent::socket_path::in_dir(&dir, "tm.sock").unwrap();
        let addr = agent::socket_path::in_dir(&dir, "n.sock").unwrap();
        let mut tui = TmuxTui {
            dir,
            socket,
            addr,
            server: 0,
            tui: 0,
            starts: [None, None],
        };
        tui.tmux(&[
            "new-session",
            "-d",
            "-s",
            "takeover",
            "-x",
            "150",
            "-y",
            "32",
            "/bin/sh",
            "-i",
        ]);
        tui.tmux(&["set-option", "-g", "remain-on-exit", "on"]);
        let quote = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
        let line = format!(
            "PS1='takeover-shell> '; {} --clean -n -i NONE --listen {}",
            quote(nvim.to_str().unwrap()),
            quote(tui.addr.to_str().unwrap())
        );
        tui.tmux(&["send-keys", "-t", "takeover:0.0", "-l", &line]);
        tui.send_keys("Enter");
        wait_until(|| tui.addr.exists());
        let link = tui.link();
        tui.server = tui.lua(&link, "return vim.fn.getpid()", vec![]).as_u64().unwrap() as u32;
        wait_until(|| tui.lua(&link, "return #vim.api.nvim_list_uis()", vec![]).as_u64() == Some(1));
        let info = tui.lua(
            &link,
            "local u = vim.api.nvim_list_uis()[1]; return vim.api.nvim_get_chan_info(u.chan)",
            vec![],
        );
        tui.tui = info
            .as_map()
            .and_then(|m| m.iter().find(|(k, _)| k.as_str() == Some("client")))
            .and_then(|(_, c)| c.as_map())
            .and_then(|m| m.iter().find(|(k, _)| k.as_str() == Some("attributes")))
            .and_then(|(_, a)| a.as_map())
            .and_then(|m| m.iter().find(|(k, _)| k.as_str() == Some("pid")))
            .and_then(|(_, p)| p.as_u64().or_else(|| p.as_str().and_then(|s| s.parse().ok())))
            .map(|p| p as u32)
            .expect("the terminal UI reports its pid");
        tui.starts = [tui.server, tui.tui].map(eitri_core::split::proc_start_time);
        tui
    }

    fn tmux(&self, args: &[&str]) -> Output {
        let output = Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .args(["-f", "/dev/null"])
            .args(args)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("NVIM")
            .env("XDG_CONFIG_HOME", self.dir.join("config"))
            .env("XDG_DATA_HOME", self.dir.join("data"))
            .env("XDG_STATE_HOME", self.dir.join("state"))
            .env("TERM", "xterm-256color")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "tmux {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    pub fn server_addr(&self) -> &Path {
        &self.addr
    }
    pub fn capture(&self) -> String {
        String::from_utf8(self.tmux(&["capture-pane", "-p", "-t", "takeover:0.0"]).stdout).unwrap()
    }
    pub fn send_keys(&self, keys: &str) {
        self.tmux(&["send-keys", "-t", "takeover:0.0", keys]);
    }
    pub fn tui_pid(&self) -> u32 {
        self.tui
    }
    pub fn server_pid(&self) -> u32 {
        self.server
    }
    pub fn link(&self) -> NvimLink {
        NvimLink::connect(&self.addr, Duration::from_secs(2)).unwrap().0
    }
    pub fn lua(&self, link: &NvimLink, code: &str, args: Vec<Value>) -> Value {
        link.exec_lua(code, args)
            .wait(Duration::from_secs(5))
            .expect("nvim answered")
            .unwrap()
    }
}

impl Drop for TmuxTui {
    fn drop(&mut self) {
        // Both processes were started by this private terminal. The server can outlive its UI.
        for (pid, start) in [self.server, self.tui].into_iter().zip(self.starts) {
            if pid > 0 && start.is_some() && eitri_core::split::proc_start_time(pid) == start {
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
            }
        }
        let _ = Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .arg("kill-server")
            .output();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
