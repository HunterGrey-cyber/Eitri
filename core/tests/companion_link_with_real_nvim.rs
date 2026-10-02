//! The companion's link driver against a real `nvim --headless --listen`: it attaches and installs,
//! notices when nvim quits and says so once, fails fast on a socket nobody listens on, and lets go
//! of an nvim that is waiting for a key without waiting for it.
//!
//! `#[ignore]`d: needs `nvim` on PATH. No tokens, no network, no display.
//!
//! Run: `cargo test -p eitri-core --test companion_link_with_real_nvim -- --ignored`

use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use eitri_core::companion::attach::{Detach, LinkState};
use eitri_core::companion::driver::{LinkDriver, Tick};
use eitri_core::companion::Sockets;
use eitri_core::nvim_rpc::{LinkEvent, NvimLink, Pending};
use rmpv::Value;

/// How long the glue may take to go once nvim gets its key: the liveness timer ticks every 500 ms.
const TEARDOWN_WITHIN: Duration = Duration::from_secs(1);

/// Under `std::env::temp_dir()`, not `CARGO_TARGET_TMPDIR`: the sockets bound inside are capped at
/// 103 bytes and a worktree's target directory already uses most of that. `case` stays at 8
/// characters or fewer for the same reason.
fn scratch_dir(case: &str) -> PathBuf {
    std::env::temp_dir().join(format!("cl-{}-{case}", std::process::id()))
}

/// A headless nvim on a scratch directory, killed by its own handle and removed on drop, with the
/// four sockets the glue writes to bound and never accepted from.
struct Nvim {
    child: Child,
    dir: PathBuf,
    sock: PathBuf,
    sockets: Sockets,
    _listeners: Vec<UnixListener>,
}

impl Nvim {
    fn start(case: &str) -> Nvim {
        let dir = scratch_dir(case);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("state")).unwrap();
        std::fs::write(dir.join("s.txt"), "a b c x\nline two\n").unwrap();
        let sock = agent::socket_path::in_dir(&dir, "n.sock").unwrap();
        let mut command = Command::new("nvim");
        command
            .args(["--headless", "--clean", "-n", "-i", "NONE", "--listen"])
            .arg(&sock)
            .arg(dir.join("s.txt"))
            .current_dir(&dir)
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("XDG_DATA_HOME", dir.join("data"))
            .env("XDG_CONFIG_HOME", dir.join("config"))
            // An inherited `$TMUX` would flip the install report and turn the navigator rule off.
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("NVIM")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (name, _) in std::env::vars_os() {
            if name.to_str().is_some_and(|n| n.starts_with("EITRI_")) {
                command.env_remove(name);
            }
        }
        let child = command.spawn().expect("nvim must be on PATH for this test");
        let mut listeners = Vec::new();
        let mut bind = |name: &str| {
            let path = agent::socket_path::in_dir(&dir, name).unwrap();
            listeners.push(UnixListener::bind(&path).expect("bind a feed socket"));
            Some(path)
        };
        let sockets = Sockets {
            editor_context: bind("ec.sock"),
            theme: bind("th.sock"),
            keys: bind("ky.sock"),
            pane_switch: bind("ps.sock"),
        };
        let nvim = Nvim {
            child,
            dir,
            sock,
            sockets,
            _listeners: listeners,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !nvim.sock.exists() {
            assert!(Instant::now() < deadline, "nvim never created its socket");
            std::thread::sleep(Duration::from_millis(20));
        }
        // An install before VimEnter would take the late-install branch, which these tests are not about.
        let (probe, _events) = nvim.connect();
        poll("nvim finishing its start", Duration::from_secs(5), || {
            (ask(&probe, "nvim_eval", vec![Value::from("v:vim_did_enter")]).as_i64() == Some(1)).then_some(())
        });
        probe.close();
        nvim
    }

    /// The socket file appears when nvim binds, a moment before it listens; until then a connect is refused.
    fn connect(&self) -> (NvimLink, Receiver<LinkEvent>) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match NvimLink::connect(&self.sock, Duration::from_secs(1)) {
                Ok(connected) => return connected,
                Err(e) => {
                    assert!(Instant::now() < deadline, "could not connect: {e}");
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }
}

impl Drop for Nvim {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn ask(link: &NvimLink, method: &str, params: Vec<Value>) -> Value {
    finish(&link.call(method, params), method, Duration::from_secs(5))
}

fn lua(link: &NvimLink, code: &str) -> Value {
    finish(&link.exec_lua(code, vec![]), code, Duration::from_secs(5))
}

fn finish(pending: &Pending, what: &str, within: Duration) -> Value {
    match pending.wait(within) {
        Some(Ok(value)) => value,
        Some(Err(e)) => panic!("{what}: {e}"),
        None => panic!("{what}: no answer within {within:?}"),
    }
}

fn poll<T>(what: &str, within: Duration, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + within;
    loop {
        if let Some(found) = check() {
            return found;
        }
        assert!(Instant::now() < deadline, "timed out after {within:?}: {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn field<'a>(map: &'a Value, name: &str) -> Option<&'a Value> {
    map.as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(name))
        .map(|(_, v)| v)
}

fn is_blocking(obs: &NvimLink) -> bool {
    field(&ask(obs, "nvim_get_mode", vec![]), "blocking").and_then(Value::as_bool) == Some(true)
}

/// Polls `driver` every 50 ms until `check` is satisfied, counting the ticks that asked for the
/// drafts to end. A timeout is a panic naming the last band, never a hang.
fn drive<T>(
    what: &str,
    driver: &mut LinkDriver,
    within: Duration,
    cancels: &mut u32,
    mut check: impl FnMut(&LinkDriver, &Tick) -> Option<T>,
) -> T {
    let deadline = Instant::now() + within;
    loop {
        let tick = driver.poll(Instant::now());
        if tick.cancel_drafts {
            *cancels += 1;
        }
        if let Some(found) = check(driver, &tick) {
            return found;
        }
        assert!(
            Instant::now() < deadline,
            "timed out after {within:?}: {what} (state {:?}, band {:?})",
            driver.state(),
            driver.band()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// What the glue leaves in nvim: its autocmds, timers, maps and globals.
const FOOTPRINT_LUA: &str = r#"
local n = 0
for _, g in ipairs({ "EitriEditorContext", "EitriThemeFeed", "eitri_keys", "eitri_nav" }) do
  local ok, x = pcall(vim.api.nvim_get_autocmds, { group = g })
  if ok then n = n + #x end
end
vim.uv.walk(function(h) if h:get_type() == "timer" and h:is_active() and not h:is_closing() then n = n + 1 end end)
for _, mode in ipairs({ "n", "x", "i" }) do
  for _, m in ipairs(vim.api.nvim_get_keymap(mode)) do
    if (m.desc or ""):find("^eitri") then n = n + 1 end
  end
end
for _, name in ipairs({ "__eitri_companion", "EitriScratch", "__eitri_keys_schedule", "eitri_reload_timer" }) do
  if rawget(_G, name) ~= nil then n = n + 1 end
end
return n
"#;

fn footprint(obs: &NvimLink) -> i64 {
    lua(obs, FOOTPRINT_LUA).as_i64().expect("a footprint count")
}

/// A connection that did `nvim_ui_attach`, so nvim treats `f` as a command that waits for its
/// character. Its redraws pile up unread in `_events`, which is unbounded.
struct Ui {
    link: NvimLink,
    _events: Receiver<LinkEvent>,
}

impl Ui {
    fn attach(nvim: &Nvim) -> Ui {
        let (link, events) = nvim.connect();
        let options = Value::Map(vec![(Value::from("rgb"), Value::from(true))]);
        ask(
            &link,
            "nvim_ui_attach",
            vec![Value::from(100), Value::from(30), options],
        );
        Ui { link, _events: events }
    }

    fn keys(&self, keys: &str) {
        ask(&self.link, "nvim_input", vec![Value::from(keys)]);
    }
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn attach_install_and_nvim_quit_reach_detached_and_cancel_drafts_once() {
    let nvim = Nvim::start("quit");
    let (obs, _obs_events) = nvim.connect();
    let mut driver = LinkDriver::new(Some(nvim.sock.clone()), nvim.sockets.clone());
    let mut cancels = 0;

    drive(
        "the panel attached",
        &mut driver,
        Duration::from_secs(3),
        &mut cancels,
        |driver, _| matches!(driver.state(), LinkState::Attached { .. }).then_some(()),
    );
    assert_eq!(driver.nvim_pid(), Some(nvim.child.id()));
    assert!(driver.has_part("scratch"), "the scratch part loaded");
    assert_eq!(driver.band().state, "attached");
    assert_eq!(cancels, 0, "nothing was cut by attaching");

    // The answer never comes: nvim is gone.
    drop(obs.call("nvim_command", vec![Value::from("qa!")]));
    drive(
        "the panel noticed nvim quit",
        &mut driver,
        Duration::from_secs(2),
        &mut cancels,
        |driver, _| {
            matches!(
                driver.state(),
                LinkState::Detached {
                    why: Detach::EditorWentAway
                }
            )
            .then_some(())
        },
    );
    assert_eq!(driver.band().text, "editor detached: run :EitriPanel to attach again");
    // A second of further polling must not ask again.
    let until = Instant::now() + Duration::from_secs(1);
    while Instant::now() < until {
        if driver.poll(Instant::now()).cancel_drafts {
            cancels += 1;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(cancels, 1, "the drafts end in exactly one tick");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_stale_address_reaches_failed_within_two_seconds() {
    let mut nvim = Nvim::start("stale");
    nvim.child.kill().expect("kill nvim by its own handle");
    nvim.child.wait().unwrap();
    assert!(nvim.sock.exists(), "kill -9 leaves the socket file behind");

    let mut driver = LinkDriver::new(Some(nvim.sock.clone()), nvim.sockets.clone());
    let mut cancels = 0;
    drive(
        "the attach failed",
        &mut driver,
        Duration::from_secs(2),
        &mut cancels,
        |driver, _| matches!(driver.state(), LinkState::Failed { .. }).then_some(()),
    );
    assert!(
        driver.band().text.starts_with("could not attach: "),
        "{:?}",
        driver.band()
    );
    assert_eq!(cancels, 0, "a failed attach had no drafts out");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn close_does_not_wait_for_a_blocked_nvim() {
    let nvim = Nvim::start("blocked");
    let (obs, _obs_events) = nvim.connect();
    let ui = Ui::attach(&nvim);
    let baseline = footprint(&obs);
    let mut driver = LinkDriver::new(Some(nvim.sock.clone()), nvim.sockets.clone());
    let mut cancels = 0;
    drive(
        "the panel attached",
        &mut driver,
        Duration::from_secs(3),
        &mut cancels,
        |driver, _| matches!(driver.state(), LinkState::Attached { .. }).then_some(()),
    );
    // Settled: the editor-context and keys snippets keep a timer for a moment after they load.
    std::thread::sleep(Duration::from_millis(500));
    assert_ne!(footprint(&obs), baseline, "the glue is installed");

    ui.keys("f");
    poll("nvim waiting for the character", Duration::from_secs(2), || {
        is_blocking(&obs).then_some(())
    });
    let started = Instant::now();
    driver.shutdown();
    let took = started.elapsed();
    assert!(took < Duration::from_millis(50), "shutdown took {took:?}");
    assert!(driver.is_shut());

    // While nvim waits, nothing is served: the glue cannot be seen going, and the request waits.
    let queued = obs.exec_lua(FOOTPRINT_LUA, vec![]);
    let until = Instant::now() + Duration::from_secs(1);
    while Instant::now() < until {
        assert!(is_blocking(&obs), "nvim stopped waiting by itself");
        assert!(
            queued.try_take().is_none(),
            "a request was answered during the char-wait"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    ui.keys("x");
    finish(&queued, "the queued footprint", Duration::from_secs(2));
    poll("the glue gone after the key", TEARDOWN_WITHIN, || {
        (footprint(&obs) == baseline).then_some(())
    });
}
