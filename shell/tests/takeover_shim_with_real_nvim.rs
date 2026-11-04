//! The built shim with a private tmux and an in-memory clipboard host, without a window or display.
use eitri_core::companion::{takeover_install_args, Sockets, INSTALL_LUA};
use eitri_core::host_clipboard::{self, Admission, Answer, ClipboardServer, Request, SOCKET_NAME};
use eitri_core::takeover::glue::{CommandsPart, TakeoverParts, TmuxSwap};
use std::io::Read;
use std::os::unix::fs::symlink;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Fixture {
    dir: PathBuf,
    tmux: Child,
    server: Option<ClipboardServer>,
}

fn tmux(socket: &Path, args: &[&str]) -> String {
    let out = Command::new("tmux")
        .arg("-S")
        .arg(socket)
        .args(args)
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .expect("NOT RUN: tmux must be on PATH");
    assert!(
        out.status.success(),
        "private tmux: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            server.stop_and_join();
        }
        let _ = self.tmux.kill();
        let _ = self.tmux.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
#[ignore = "needs real nvim and tmux on PATH and private Unix sockets; no display"]
fn the_real_shim_takes_nav_and_clipboard_while_attached_and_real_tmux_after() {
    assert!(
        Command::new("tmux").arg("-V").status().is_ok_and(|s| s.success()),
        "NOT RUN: tmux must be on PATH"
    );
    let dir = std::env::temp_dir().join(format!("takeover-{}", &uuid::Uuid::new_v4().simple().to_string()[..16]));
    agent::private_fs::create_private_dir_all(&dir, &dir).unwrap();
    let bin = dir.join("bin");
    agent::private_fs::create_private_dir_all(&bin, &dir).unwrap();
    symlink(env!("CARGO_BIN_EXE_eitri-tmux-shim"), bin.join("tmux")).unwrap();
    let real_socket = agent::socket_path::in_dir(&dir, "tm.sock").unwrap();
    let server = Command::new("tmux")
        .args(["-D", "-S"])
        .arg(&real_socket)
        .args(["-f", "/dev/null"])
        .env("HOME", &dir)
        .env("XDG_RUNTIME_DIR", &dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("SHELL", "/bin/sh")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut f = Fixture {
        dir: dir.clone(),
        tmux: server,
        server: None,
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while !real_socket.exists() {
        assert!(Instant::now() < deadline, "private tmux never bound its socket");
        std::thread::sleep(Duration::from_millis(10));
    }
    tmux(
        &real_socket,
        &[
            "new-session",
            "-d",
            "-x",
            "80",
            "-y",
            "24",
            "-s",
            "takeover",
            "/bin/sleep",
            "60",
        ],
    );
    let left = tmux(&real_socket, &["display-message", "-p", "#{pane_id}"]);
    tmux(&real_socket, &["split-window", "-h", "/bin/sleep", "60"]);
    let right = tmux(&real_socket, &["display-message", "-p", "#{pane_id}"]);
    assert_ne!(left, right);
    tmux(&real_socket, &["set-buffer", "before"]);
    let original_tmux = tmux(&real_socket, &["display-message", "-p", "#{socket_path},#{pid},0"]);
    let switch_socket = agent::socket_path::in_dir(&dir, "s.sock").unwrap();
    let nav = UnixListener::bind(&switch_socket).unwrap();
    nav.set_nonblocking(true).unwrap();
    let held = Arc::new(Mutex::new(Vec::new()));
    let clipboard = UnixListener::bind(agent::socket_path::in_dir(&dir, SOCKET_NAME).unwrap()).unwrap();
    let saved = Arc::clone(&held);
    f.server = Some(
        host_clipboard::serve(
            clipboard,
            Admission::for_window(&switch_socket),
            move |request, reply| match request {
                Request::Copy(text) => {
                    *saved.lock().unwrap() = text;
                    reply.send(Answer::Done);
                }
                Request::Paste => reply.send(Answer::Text(saved.lock().unwrap().clone())),
            },
        )
        .unwrap(),
    );
    let p = TakeoverParts {
        sockets: Sockets::default(),
        commands: CommandsPart {
            socket: switch_socket.clone(),
            words: eitri_core::nvim_verbs::completion_words(&[], &[]),
            group: None,
        },
        tmux_swap: Some(TmuxSwap {
            tmux: format!("{},1,0", switch_socket.display()),
            tmux_pane: "%0".into(),
            pane_switch_socket: switch_socket.clone(),
            shim_dir: bin,
        }),
    };
    // The headless script owns its helper channel; fill that id in immediately before install.
    let args = takeover_install_args(0, &p);
    let mut encoded = Vec::new();
    rmpv::encode::write_value(&mut encoded, &rmpv::Value::Array(args)).unwrap();
    agent::private_fs::write_private(&dir.join("args"), &encoded).unwrap();
    agent::private_fs::write_private(&dir.join("install.lua"), INSTALL_LUA.as_bytes()).unwrap();
    agent::private_fs::write_private(&dir.join("test.lua"), SCRIPT.as_bytes()).unwrap();
    let mut child = Command::new("nvim")
        .args(["--headless", "-u", "NONE", "-n", "-i", "NONE", "-l"])
        .arg(dir.join("test.lua"))
        .env("TAKEOVER_TEST_DIR", &dir)
        .env("NVIM_LOG_FILE", dir.join("nvim.log"))
        .env("TMUX", &original_tmux)
        .env("TMUX_PANE", &right)
        // The clipboard stand-in admits only this direct child and its descendants, so this fixture shows what the
        // shim does with a call it receives, not that a person's terminal nvim is admitted: that nvim is not a child
        // of the clipboard server, and its calls are refused until the server is told to admit it.
        .env("EITRI_PANE_SWITCH_SOCKET", &switch_socket)
        .env("HOME", &dir)
        .env("XDG_RUNTIME_DIR", &dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("XDG_DATA_HOME", dir.join("data"))
        .env("XDG_STATE_HOME", dir.join("state"))
        .env("XDG_CACHE_HOME", dir.join("cache"))
        .env_remove("NVIM")
        .env_remove("NVIM_LISTEN_ADDRESS")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("nvim did not finish the shim checks");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut error = String::new();
    child.stderr.take().unwrap().read_to_string(&mut error).unwrap();
    assert!(status.success(), "nvim shim checks: {error}");
    let (mut stream, _) = nav.accept().expect("the shim sent the pane direction");
    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut direction = String::new();
    stream.read_to_string(&mut direction).unwrap();
    assert_eq!(direction, "L\n");
    assert!(nav.accept().is_err(), "navigation after detach reached the shim");
    assert_eq!(*held.lock().unwrap(), b"attached\n");
    assert_eq!(tmux(&real_socket, &["save-buffer", "-"]), "returned");
    assert_eq!(tmux(&real_socket, &["display-message", "-p", "#{pane_id}"]), left);
}

const SCRIPT: &str = r#"
local dir = vim.env.TAKEOVER_TEST_DIR
local function read(name)
  local f = assert(io.open(dir .. '/' .. name, 'rb'))
  local s = f:read('*a'); f:close(); return s
end
local chan = vim.fn.jobstart({vim.v.progpath, '--clean', '--headless', '--embed', '-n', '-i', 'NONE'}, {rpc=true})
assert(chan > 0)
local args = vim.mpack.decode(read('args'))
args[1] = chan
local original = vim.env.TMUX
vim.g.clipboard = {name='takeover-tmux', cache_enabled=0,
 copy={['+']={'tmux','load-buffer','-'}, ['*']={'tmux','load-buffer','-'}},
 paste={['+']={'tmux','save-buffer','-'}, ['*']={'tmux','save-buffer','-'}}}
local report = assert(loadstring(read('install.lua')))(unpack(args))
assert(#report.failed == 0, vim.inspect(report.failed))
assert(vim.env.TMUX ~= original)
vim.fn.system('tmux select-pane -L')
assert(vim.v.shell_error == 0)
vim.api.nvim_buf_set_lines(0, 0, -1, false, {'attached'})
vim.cmd('normal! "+yy')
assert(vim.wait(5000, function()
  for _, c in ipairs(vim.api.nvim_list_chans()) do if c.stream == 'job' and c.id ~= chan then return false end end
  return true
end, 10))
assert(vim.fn.system({'tmux', 'save-buffer', '-'}) == 'attached\n')
__eitri_companion.teardown()
assert(vim.env.TMUX == original)
vim.fn.system('tmux select-pane -L')
assert(vim.v.shell_error == 0)
vim.api.nvim_buf_set_lines(0, 0, -1, false, {'returned'})
vim.cmd('normal! "+yy')
assert(vim.wait(5000, function()
  for _, c in ipairs(vim.api.nvim_list_chans()) do if c.stream == 'job' and c.id ~= chan then return false end end
  return true
end, 10))
assert(vim.fn.system({'tmux', 'save-buffer', '-'}) == 'returned\n')
vim.fn.jobstop(chan)
"#;
