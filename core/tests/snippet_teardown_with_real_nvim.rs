//! The theme, keys and scratch snippets, injected into a running nvim the way a companion panel does
//! and then torn down again, driven by a real `nvim --embed --headless` over msgpack-RPC.
//!
//! `#[ignore]`d: needs `nvim` on PATH. No tokens, no network, no display.
//!
//! Run: `cargo test -p eitri-core --test snippet_teardown_with_real_nvim -- --ignored`

use std::io::Read;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rmpv::Value;

#[path = "support/embed_client.rs"]
mod embed_client;
use embed_client::{opts_map, Embed};

const THEME_LUA: &str = include_str!("../src/theme/nvim_theme.lua");
const KEYS_LUA: &str = include_str!("../src/nvim_keys/nvim_keys.lua");
const SCRATCH_LUA: &str = include_str!("../src/nvim_scratch.lua");

/// A scratch directory short enough for a socket path (the cap is 103 bytes), removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("td-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A bound, non-blocking listener that hands back the lines nvim writes to it.
struct Lines {
    listener: UnixListener,
    path: PathBuf,
}

impl Lines {
    fn bind(dir: &Path, name: &str) -> Lines {
        let path = dir.join(name);
        let listener = UnixListener::bind(&path).expect("bind the listener");
        listener.set_nonblocking(true).unwrap();
        Lines { listener, path }
    }

    fn socket(&self) -> Value {
        Value::from(self.path.display().to_string())
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

fn started(scratch: &Scratch) -> Embed {
    let mut nvim = Embed::start(scratch.path(), &[], &[]);
    nvim.wait_until("VimEnter", |n| n.eval("v:vim_did_enter").as_i64() == Some(1));
    nvim
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn injected_theme_sends_at_once_after_vimenter_and_tears_down() {
    let scratch = Scratch::new("theme");
    let lines = Lines::bind(scratch.path(), "t.sock");
    let mut nvim = started(&scratch);
    let baseline = nvim.footprint(&["EitriThemeFeed"]);

    nvim.inject(THEME_LUA, opts_map(&[("socket", lines.socket())]));
    // The resnapshot tick is 3 s; a line inside 1 s is the immediate one.
    let first = lines
        .next(Duration::from_secs(1))
        .expect("a snapshot within 1 s of the injection");
    assert!(first.contains("\"groups\""), "{first}");
    let running = nvim.footprint(&["EitriThemeFeed"]);
    assert!(running.autocmds > 0, "{running:?}");
    assert_eq!(running.active_timers, baseline.active_timers + 1, "{running:?}");

    // Control: a live feed does send on a colorscheme change.
    nvim.command("colorscheme desert");
    assert!(
        lines.next(Duration::from_secs(1)).is_some(),
        "the live feed ignored :colorscheme"
    );

    // One :colorscheme fires both ColorScheme and OptionSet; let every line it caused arrive.
    while lines.next(Duration::from_millis(400)).is_some() {}

    nvim.teardown();
    let after = nvim.footprint(&["EitriThemeFeed"]);
    assert_eq!(after.autocmds, 0, "{after:?}");
    assert_eq!(after.active_timers, baseline.active_timers, "{after:?}");
    nvim.command("colorscheme blue");
    assert_eq!(lines.next(Duration::from_secs(1)), None, "a torn-down feed sent");
    nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn injected_keys_report_at_once_and_teardown_removes_the_watcher() {
    let scratch = Scratch::new("keys");
    let lines = Lines::bind(scratch.path(), "k.sock");
    let mut nvim = started(&scratch);

    nvim.inject(KEYS_LUA, opts_map(&[("socket", lines.socket())]));
    let first = lines
        .next(Duration::from_secs(1))
        .expect("a report within 1 s of the injection");
    assert!(first.contains("\"maps\""), "{first}");
    assert_eq!(nvim.eval("exists('*EitriKeysLeaderChanged')").as_i64(), Some(1));
    assert!(nvim.footprint(&["eitri_keys"]).autocmds > 0);

    // Control: a live watcher reports a leader change.
    nvim.command("let g:mapleader = ';'");
    assert!(
        lines.next(Duration::from_secs(1)).is_some(),
        "the live watcher ignored mapleader"
    );

    nvim.teardown();
    assert_eq!(nvim.eval("exists('*EitriKeysLeaderChanged')").as_i64(), Some(0));
    assert_eq!(
        nvim.lua("return _G.__eitri_keys_schedule == nil", vec![]),
        Value::from(true)
    );
    nvim.command("let g:mapleader = ','");
    assert_eq!(lines.next(Duration::from_secs(1)), None, "a torn-down watcher sent");
    assert_eq!(nvim.footprint(&["eitri_keys"]).autocmds, 0);
    nvim.quit();
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn a_teardown_during_the_keys_debounce_sends_nothing() {
    let scratch = Scratch::new("deb");
    let lines = Lines::bind(scratch.path(), "k.sock");
    let empty = scratch.path().join("empty.vim");
    std::fs::write(&empty, "").unwrap();
    let mut nvim = started(&scratch);

    nvim.inject(KEYS_LUA, opts_map(&[("socket", lines.socket())]));
    lines.next(Duration::from_secs(1)).expect("the first report");

    // Control: a changed option plus a sourced file schedule a report that does arrive.
    let change = format!("vim.o.timeoutlen = %d vim.cmd('source {}')", empty.display());
    nvim.lua(&change.replace("%d", "1234"), vec![]);
    lines.next(Duration::from_secs(1)).expect("the control report");

    // The same change, then the teardown in the same request, well inside the 100 ms debounce.
    nvim.lua(&format!("{} _G.__test_td()", change.replace("%d", "1235")), vec![]);
    assert_eq!(
        lines.next(Duration::from_millis(500)),
        None,
        "a report went out after the teardown"
    );
    nvim.quit();
}

fn hex(text: &str) -> String {
    text.bytes().map(|b| format!("{b:02x}")).collect()
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn injected_scratch_answers_and_teardown_removes_its_global_only_if_ours() {
    let scratch = Scratch::new("scr");
    let file = scratch.path().join("note.md");
    std::fs::write(&file, "hello\n").unwrap();
    let file = file.canonicalize().unwrap();
    let mut nvim = started(&scratch);

    nvim.inject(SCRATCH_LUA, opts_map(&[]));
    let request = serde_json::json!({ "op": "open", "path": file.display().to_string() }).to_string();
    nvim.lua(&format!("EitriScratch.call('{}')", hex(&request)), vec![]);
    assert_eq!(
        nvim.eval("expand('%:p')").as_str(),
        Some(file.display().to_string().as_str())
    );

    nvim.teardown();
    assert_eq!(nvim.lua("return EitriScratch == nil", vec![]), Value::from(true));

    // Someone else's global survives a second call of the old teardown.
    nvim.lua("EitriScratch = {}", vec![]);
    nvim.teardown();
    assert_eq!(nvim.lua("return type(EitriScratch)", vec![]), Value::from("table"));
    nvim.quit();
}
