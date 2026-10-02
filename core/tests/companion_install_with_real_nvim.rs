//! The companion injector against a real `nvim --headless --listen`: `INSTALL_LUA` loads the six
//! snippets, every one of them goes again when the panel's connection ends, and the install waits
//! (never fails) while nvim waits for a key.
//!
//! `#[ignore]`d: needs `nvim` on PATH. No tokens, no network, no display.
//!
//! Run: `cargo test -p eitri-core --test companion_install_with_real_nvim -- --ignored`

use std::io::Read;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use eitri_core::companion::{
    client_info_params, install_args, parse_install_report, InstallReport, Sockets, INSTALL_LUA, REPLACED_METHOD,
    TEARDOWN_LUA,
};
use eitri_core::nvim_rpc::{LinkEvent, NvimLink, Pending};
use rmpv::Value;

/// How long the glue may take to go after its panel's connection ended or nvim got its key: the
/// liveness timer ticks every 500 ms.
const TEARDOWN_WITHIN: Duration = Duration::from_secs(1);

/// Under `std::env::temp_dir()`, not `CARGO_TARGET_TMPDIR`: the sockets bound inside are capped at
/// 103 bytes (`agent::socket_path`) and a worktree's target directory already uses most of that.
/// `case` stays at 8 characters or fewer for the same reason.
fn scratch_dir(case: &str) -> PathBuf {
    std::env::temp_dir().join(format!("ci-{}-{case}", std::process::id()))
}

/// A headless nvim on a scratch directory, killed by its own handle and removed on drop.
struct Nvim {
    child: Child,
    dir: PathBuf,
    sock: PathBuf,
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
            // These tests run inside tmux: an inherited `$TMUX` would flip the report's `tmux` and
            // turn the navigator rule off.
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
        let nvim = Nvim { child, dir, sock };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !nvim.sock.exists() {
            assert!(Instant::now() < deadline, "nvim never created its socket");
            std::thread::sleep(Duration::from_millis(20));
        }
        // An install before VimEnter would skip the late-install branch these tests are about.
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

    fn sockets(&self) -> Sockets {
        let in_dir = |name: &str| Some(agent::socket_path::in_dir(&self.dir, name).unwrap());
        Sockets {
            editor_context: in_dir("ec.sock"),
            theme: in_dir("th.sock"),
            keys: in_dir("ky.sock"),
            pane_switch: in_dir("ps.sock"),
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

/// Call a method and wait for its answer; a hang or an nvim error is a failure naming the call.
fn ask(link: &NvimLink, method: &str, params: Vec<Value>) -> Value {
    finish(&link.call(method, params), method, Duration::from_secs(5))
}

fn lua(link: &NvimLink, code: &str, args: Vec<Value>) -> Value {
    finish(&link.exec_lua(code, args), code, Duration::from_secs(5))
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

fn strings(value: Option<&Value>) -> Vec<String> {
    // An empty Lua table arrives as an empty map or an empty array.
    value
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

/// A bound, non-blocking listener that hands back the lines nvim writes to it.
struct Lines {
    listener: UnixListener,
}

impl Lines {
    fn bind(path: &Path) -> Lines {
        let listener = UnixListener::bind(path).expect("bind the listener");
        listener.set_nonblocking(true).unwrap();
        Lines { listener }
    }

    /// The next line within `within`, or `None`.
    fn next(&self, within: Duration) -> Option<String> {
        let deadline = Instant::now() + within;
        loop {
            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                    let mut text = String::new();
                    let _ = stream.read_to_string(&mut text);
                    if let Some(line) = text.lines().next() {
                        return Some(line.to_string());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("accept failed: {e}"),
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

/// What the glue leaves in nvim.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Footprint {
    autocmds: i64,
    timers: i64,
    /// `<mode> <lhs>` of every map whose description starts with `eitri`.
    maps: Vec<String>,
    globals: Vec<String>,
    leader_function: bool,
}

const FOOTPRINT_LUA: &str = r#"
local groups, globals = ...
local a = 0
for _, g in ipairs(groups) do
  local ok, x = pcall(vim.api.nvim_get_autocmds, { group = g })
  if ok then a = a + #x end
end
local t = 0
vim.uv.walk(function(h) if h:get_type() == "timer" and h:is_active() and not h:is_closing() then t = t + 1 end end)
local maps = {}
for _, mode in ipairs({ "n", "x", "i" }) do
  for _, m in ipairs(vim.api.nvim_get_keymap(mode)) do
    if (m.desc or ""):find("^eitri") then maps[#maps + 1] = mode .. " " .. m.lhs end
  end
end
local present = {}
for _, name in ipairs(globals) do if rawget(_G, name) ~= nil then present[#present + 1] = name end end
return { autocmds = a, timers = t, maps = maps, globals = present,
         leader = vim.fn.exists("*EitriKeysLeaderChanged") == 1 }
"#;

fn footprint_args() -> Vec<Value> {
    let names = |list: &[&str]| Value::Array(list.iter().map(|g| Value::from(*g)).collect());
    vec![
        names(&["EitriEditorContext", "EitriThemeFeed", "eitri_keys", "eitri_nav"]),
        names(&[
            "__eitri_companion",
            "EitriScratch",
            "__eitri_keys_schedule",
            "eitri_reload_timer",
        ]),
    ]
}

fn read_footprint(reply: &Value) -> Footprint {
    Footprint {
        autocmds: field(reply, "autocmds").and_then(Value::as_i64).expect("autocmds"),
        timers: field(reply, "timers").and_then(Value::as_i64).expect("timers"),
        maps: strings(field(reply, "maps")),
        globals: strings(field(reply, "globals")),
        leader_function: field(reply, "leader").and_then(Value::as_bool).expect("leader"),
    }
}

fn footprint(link: &NvimLink) -> Footprint {
    read_footprint(&lua(link, FOOTPRINT_LUA, footprint_args()))
}

/// The footprint once it stops changing: the editor-context and keys snippets keep a timer for a
/// moment after they are loaded, and a footprint read in that moment is not what stays.
fn settled_footprint(link: &NvimLink) -> Footprint {
    let mut before = footprint(link);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        std::thread::sleep(Duration::from_millis(250));
        let now = footprint(link);
        if now == before {
            return now;
        }
        assert!(
            Instant::now() < deadline,
            "the footprint never settled: {before:?} then {now:?}"
        );
        before = now;
    }
}

/// Poll until the footprint is `want`: a debounce timer can stay active for 100 ms after a teardown.
fn footprint_becomes(link: &NvimLink, want: &Footprint, within: Duration, what: &str) {
    let deadline = Instant::now() + within;
    loop {
        let now = footprint(link);
        if now == *want {
            return;
        }
        assert!(Instant::now() < deadline, "{what}: wanted {want:?}, last saw {now:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
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

/// A panel's connection with its install already sent.
struct Panel {
    link: NvimLink,
    events: Receiver<LinkEvent>,
    install: Pending,
    chan: u64,
}

impl Panel {
    fn start(nvim: &Nvim, sockets: &Sockets) -> Panel {
        let (link, events) = nvim.connect();
        let chan = link.channel_id();
        // Not waited for: it is not api-fast, so it queues behind a char-wait like the install does.
        let _ = link.call("nvim_set_client_info", client_info_params());
        let install = link.exec_lua(INSTALL_LUA, install_args(chan, sockets));
        Panel {
            link,
            events,
            install,
            chan,
        }
    }

    fn report(&self, within: Duration) -> InstallReport {
        let value = finish(&self.install, "the install", within);
        parse_install_report(&value).expect("a readable install report")
    }

    /// Install again from this same connection.
    fn install_again(&self, sockets: &Sockets) -> InstallReport {
        let value = finish(
            &self.link.exec_lua(INSTALL_LUA, install_args(self.chan, sockets)),
            "the second install",
            Duration::from_secs(5),
        );
        parse_install_report(&value).expect("a readable install report")
    }
}

const ALL_PARTS: [&str; 6] = [
    "editor_context",
    "theme",
    "keys",
    "nav_fallback",
    "scratch",
    "buffer_reload",
];

fn is_blocking(obs: &NvimLink) -> bool {
    let mode = ask(obs, "nvim_get_mode", vec![]);
    field(&mode, "blocking").and_then(Value::as_bool) == Some(true)
}

fn nav_map_desc(obs: &NvimLink) -> String {
    lua(
        obs,
        "return vim.fn.maparg('<C-l>', 'n', false, true).desc or ''",
        vec![],
    )
    .as_str()
    .unwrap_or_default()
    .to_owned()
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn install_reports_every_part_and_each_feed_answers() {
    let nvim = Nvim::start("all");
    let sockets = nvim.sockets();
    let editor_context = Lines::bind(sockets.editor_context.as_ref().unwrap());
    let theme = Lines::bind(sockets.theme.as_ref().unwrap());
    let keys = Lines::bind(sockets.keys.as_ref().unwrap());
    let pane_switch = Lines::bind(sockets.pane_switch.as_ref().unwrap());
    let (obs, _obs_events) = nvim.connect();

    let panel = Panel::start(&nvim, &sockets);
    let report = panel.report(Duration::from_secs(5));
    assert_eq!(report.installed, ALL_PARTS);
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(report.nvim_pid, nvim.child.id());
    assert!(!report.in_tmux);

    let info = ask(&obs, "nvim_get_chan_info", vec![Value::from(panel.chan)]);
    let client = field(&info, "client").expect("a client in the channel info");
    assert_eq!(field(client, "name").and_then(Value::as_str), Some("eitri-panel"));
    assert_eq!(field(client, "type").and_then(Value::as_str), Some("remote"));

    let line = |lines: &Lines, what: &str| {
        lines
            .next(Duration::from_secs(2))
            .unwrap_or_else(|| panic!("no line on the {what} socket within 2 s"))
    };
    assert!(line(&editor_context, "editor context").contains('{'));
    assert!(line(&theme, "theme").contains("\"groups\""));
    assert!(line(&keys, "keys").contains("\"maps\""));

    poll("the navigator maps", Duration::from_secs(2), || {
        nav_map_desc(&obs).starts_with("eitri").then_some(())
    });
    ask(&obs, "nvim_input", vec![Value::from("<C-l>")]);
    assert_eq!(pane_switch.next(Duration::from_secs(2)).as_deref(), Some("R"));
    let edge = lua(&obs, "return _G.__eitri_companion.edge('left')", vec![]);
    assert_eq!(edge.as_bool(), Some(true));
    assert_eq!(pane_switch.next(Duration::from_secs(2)).as_deref(), Some("L"));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn closing_the_link_tears_everything_down_within_a_second() {
    let nvim = Nvim::start("close");
    let (obs, _obs_events) = nvim.connect();
    let baseline = settled_footprint(&obs);

    let panel = Panel::start(&nvim, &nvim.sockets());
    panel.report(Duration::from_secs(5));
    let installed = footprint(&obs);
    assert_ne!(installed, baseline);
    assert!(installed.autocmds > 0 && installed.timers > baseline.timers && !installed.maps.is_empty());
    assert!(installed.leader_function);
    lua(&obs, "_G.__t_edge = _G.__eitri_companion.edge", vec![]);

    panel.link.close();
    footprint_becomes(
        &obs,
        &baseline,
        TEARDOWN_WITHIN,
        "everything gone within a second of the close",
    );
    // A navigator plugin's stale hook must get `false`, not a write to a socket nobody reads.
    assert_eq!(lua(&obs, "return _G.__t_edge('left')", vec![]).as_bool(), Some(false));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_second_install_on_the_same_channel_is_idempotent() {
    let nvim = Nvim::start("twice");
    let sockets = nvim.sockets();
    let (obs, _obs_events) = nvim.connect();
    let baseline = settled_footprint(&obs);

    let panel = Panel::start(&nvim, &sockets);
    panel.report(Duration::from_secs(5));
    let once = settled_footprint(&obs);
    let again = panel.install_again(&sockets);
    assert_eq!(again.installed, ALL_PARTS);
    footprint_becomes(&obs, &once, TEARDOWN_WITHIN, "one install's footprint after the second");

    // The same channel is not "another panel": it is never told it was replaced.
    while let Ok(event) = panel.events.recv_timeout(Duration::from_millis(500)) {
        if let LinkEvent::Notification { method, .. } = event {
            assert_ne!(method, REPLACED_METHOD, "a panel was told it replaced itself");
        }
    }
    let torn = lua(&obs, TEARDOWN_LUA, vec![Value::from(panel.chan)]);
    assert_eq!(torn.as_bool(), Some(true));
    footprint_becomes(&obs, &baseline, TEARDOWN_WITHIN, "the baseline after the teardown");
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn last_install_wins_and_the_first_link_hears_it() {
    let nvim = Nvim::start("last");
    let sockets = nvim.sockets();
    let (obs, _obs_events) = nvim.connect();
    let baseline = settled_footprint(&obs);

    let first = Panel::start(&nvim, &sockets);
    first.report(Duration::from_secs(5));
    let one = settled_footprint(&obs);
    let second = Panel::start(&nvim, &sockets);
    second.report(Duration::from_secs(5));
    footprint_becomes(&obs, &one, TEARDOWN_WITHIN, "one install's footprint with two panels");

    let heard = poll(
        "the first panel hearing it was replaced",
        Duration::from_secs(2),
        || match first.events.recv_timeout(Duration::from_millis(100)) {
            Ok(LinkEvent::Notification { method, params }) if method == REPLACED_METHOD => Some(params),
            _ => None,
        },
    );
    assert_eq!(
        heard.iter().filter_map(Value::as_u64).collect::<Vec<_>>(),
        [second.chan]
    );

    // The first panel's own teardown is now someone else's glue to keep.
    assert_eq!(
        lua(&obs, TEARDOWN_LUA, vec![Value::from(first.chan)]).as_bool(),
        Some(false)
    );
    assert_eq!(settled_footprint(&obs), one);
    second.link.close();
    footprint_becomes(
        &obs,
        &baseline,
        TEARDOWN_WITHIN,
        "the baseline after the second panel closed",
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn explicit_teardown_of_another_channel_does_nothing() {
    let nvim = Nvim::start("other");
    let (obs, _obs_events) = nvim.connect();
    let panel = Panel::start(&nvim, &nvim.sockets());
    panel.report(Duration::from_secs(5));
    let installed = settled_footprint(&obs);
    let wrong = lua(&obs, TEARDOWN_LUA, vec![Value::from(panel.chan + 1000)]);
    assert_eq!(wrong.as_bool(), Some(false));
    assert_eq!(settled_footprint(&obs), installed);
    let right = lua(&obs, TEARDOWN_LUA, vec![Value::from(panel.chan)]);
    assert_eq!(right.as_bool(), Some(true));
}

/// One state nvim can be in when the install arrives, entered by typing `keys` into a UI.
struct State {
    name: &'static str,
    keys: &'static str,
    /// The keys that end the wait, for a state that holds the install back.
    release: &'static str,
    /// Whether nvim waits for a character, so that it answers the install only after `release`.
    blocking: bool,
}

const STATES: [State; 10] = [
    State {
        name: "f",
        keys: "f",
        release: "x",
        blocking: true,
    },
    State {
        name: "ctrl-w",
        keys: "<C-w>",
        release: "<Esc>",
        blocking: true,
    },
    State {
        name: "q",
        keys: "q",
        release: "<Esc>",
        blocking: true,
    },
    State {
        name: "hit-enter",
        keys: ":echo \"a\\nb\\nc\"<CR>",
        release: "<CR>",
        blocking: true,
    },
    State {
        // From a timer callback. nvim answers requests while a `getchar()` waits, from a callback, an
        // RPC request or a typed command alike, and does not report itself blocking.
        name: "getchar",
        keys: "",
        release: "x",
        blocking: false,
    },
    State {
        name: "operator",
        keys: "d",
        release: "<Esc>",
        blocking: false,
    },
    State {
        name: "insert",
        keys: "i",
        release: "<Esc>",
        blocking: false,
    },
    State {
        name: "input",
        keys: ":let x = input('p: ')<CR>",
        release: "<CR>",
        blocking: false,
    },
    State {
        name: "confirm",
        keys: ":let y = confirm('ok?', \"&y\\n&n\")<CR>",
        release: "<CR>",
        blocking: false,
    },
    State {
        name: "s///c",
        keys: ":%s/a/z/c<CR>",
        release: "n",
        blocking: false,
    },
];

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn install_in_each_blocking_state() {
    for (index, state) in STATES.iter().enumerate() {
        let nvim = Nvim::start(&format!("b{index}"));
        let (obs, _obs_events) = nvim.connect();
        let ui = Ui::attach(&nvim);
        if state.name == "getchar" {
            // No mode shows it: nothing to poll for but the callback running.
            lua(&ui.link, "vim.defer_fn(function() vim.fn.getchar() end, 10)", vec![]);
            std::thread::sleep(Duration::from_millis(500));
        } else {
            ui.keys(state.keys);
            poll(
                &format!("{}: nvim in the state", state.name),
                Duration::from_secs(2),
                || {
                    let mode = ask(&obs, "nvim_get_mode", vec![]);
                    let blocking = field(&mode, "blocking").and_then(Value::as_bool) == Some(true);
                    let in_mode = field(&mode, "mode").and_then(Value::as_str) != Some("n");
                    (if state.blocking { blocking } else { in_mode }).then_some(())
                },
            );
        }

        let panel = Panel::start(&nvim, &nvim.sockets());
        if state.blocking {
            assert!(
                panel.install.wait(Duration::from_secs(1)).is_none(),
                "{}: the install was answered while nvim waited for a key",
                state.name
            );
            assert!(is_blocking(&obs), "{}: nvim stopped blocking by itself", state.name);
            ui.keys(state.release);
        }
        let report = panel.report(Duration::from_secs(1));
        assert_eq!(report.installed, ALL_PARTS, "{}", state.name);
        assert!(report.failed.is_empty(), "{}: {:?}", state.name, report.failed);
    }
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn teardown_waits_for_the_answer_in_a_char_wait_and_then_runs() {
    let nvim = Nvim::start("wait");
    let (obs, _obs_events) = nvim.connect();
    let ui = Ui::attach(&nvim);
    let baseline = settled_footprint(&obs);
    let panel = Panel::start(&nvim, &nvim.sockets());
    panel.report(Duration::from_secs(5));
    assert_ne!(footprint(&obs), baseline);

    ui.keys("f");
    poll("nvim waiting for the character", Duration::from_secs(2), || {
        is_blocking(&obs).then_some(())
    });
    panel.link.close();
    // Nothing can read the footprint now: `exec_lua` is not answered while nvim waits for a key, and
    // the teardown runs from a scheduled callback that cannot run either.
    let queued = obs.exec_lua(FOOTPRINT_LUA, footprint_args());
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
    // Which state the queued request saw is not fixed: it may run before the timer does.
    finish(&queued, "the queued footprint", Duration::from_secs(2));
    footprint_becomes(
        &obs,
        &baseline,
        TEARDOWN_WITHIN,
        "the glue gone within a second of the key",
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_slot_change_while_attached_survives_teardown() {
    let nvim = Nvim::start("slot");
    let (obs, _obs_events) = nvim.connect();
    let baseline = settled_footprint(&obs);
    let panel = Panel::start(&nvim, &nvim.sockets());
    panel.report(Duration::from_secs(5));
    poll("the navigator maps", Duration::from_secs(2), || {
        nav_map_desc(&obs).starts_with("eitri").then_some(())
    });

    lua(
        &obs,
        "vim.keymap.set('n', '<C-l>', ':echo 1<CR>', { desc = 'mine' })",
        vec![],
    );
    panel.link.close();
    footprint_becomes(&obs, &baseline, TEARDOWN_WITHIN, "no eitri map left in n, x or i");
    assert_eq!(nav_map_desc(&obs), "mine");
    let visual = lua(
        &obs,
        "return vim.fn.maparg('<C-l>', 'x', false, true).desc or ''",
        vec![],
    );
    assert!(!visual.as_str().unwrap_or_default().starts_with("eitri"), "{visual:?}");
}

/// The same `getchar()`, called by an RPC request instead of a timer callback.
#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn install_is_served_while_a_getchar_called_over_rpc_waits() {
    let nvim = Nvim::start("rpcgc");
    let (obs, _obs_events) = nvim.connect();
    let ui = Ui::attach(&nvim);
    let _waiting = ui.link.exec_lua("return vim.fn.getchar()", vec![]);
    std::thread::sleep(Duration::from_millis(500));
    assert!(!is_blocking(&obs));
    let panel = Panel::start(&nvim, &nvim.sockets());
    let report = panel.report(Duration::from_secs(1));
    assert_eq!(report.installed, ALL_PARTS);
}
