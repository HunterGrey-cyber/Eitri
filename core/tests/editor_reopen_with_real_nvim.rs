//! Reopening file windows in an isolated Neovim, driven over stdio RPC; no model or display.

use std::io::{BufReader, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::STANDARD, Engine};
use eitri_core::pane_switch::{PaneMessage, PaneSwitchChannel, PaneSwitchReader, ReopenLine};
use rmpv::Value as Msg;
use serde_json::{json, Value};

const TOKEN: &str = "0123456789abcdef";
const DEADLINE: Duration = Duration::from_secs(10);

struct Rig {
    root: PathBuf,
    channel: PaneSwitchChannel,
    reader: PaneSwitchReader,
    seen: Vec<PaneMessage>,
}

impl Rig {
    fn new(label: &str) -> Self {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("editor-reopen-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("project")).unwrap();
        std::fs::create_dir_all(root.join("runtime")).unwrap();
        std::fs::set_permissions(root.join("runtime"), std::fs::Permissions::from_mode(0o700)).unwrap();
        // Long files make cursor and viewport restoration observable independently.
        let text = (0..200)
            .map(|i| format!("{i:03} {}\n", "abcdefghij".repeat(2)))
            .collect::<String>();
        for i in 0..40 {
            std::fs::write(root.join(format!("project/{i}.txt")), &text).unwrap();
        }
        let mut channel = PaneSwitchChannel::bind_without_shim().expect("the channel binds");
        let reader = PaneSwitchReader::new(channel.take_listener().unwrap());
        Self {
            root,
            channel,
            reader,
            seen: Vec::new(),
        }
    }

    fn name(&self, name: &str) -> String {
        let project = self.root.join("project");
        STANDARD.encode(
            if name.is_empty() { project } else { project.join(name) }
                .as_os_str()
                .as_bytes(),
        )
    }

    fn leaf(&self, i: usize, cur: bool) -> Value {
        json!({"leaf": {"f": self.name(&format!("{i}.txt")), "lnum": 80 + i, "col": 4,
            "want": 4, "top": 70 + i, "left": 0, "w": 40, "h": 20, "cur": cur}})
    }

    fn record(&self, tree: Value) -> Value {
        json!({"v": 1, "root": self.name(""), "tab": 1,
            "tabs": [{"cols": 100, "lines": 48, "tree": tree}], "bufs": []})
    }

    fn input(&self, record: Value, skip: bool) {
        agent::private_fs::write_private(
            &self.channel.dir().join("reopen-in.json"),
            &serde_json::to_vec(&json!({"token": TOKEN, "root": self.name(""), "prefixes": [],
                "record": record, "skip": skip}))
            .unwrap(),
        )
        .unwrap();
    }

    fn simple(&self) {
        self.input(
            self.record(json!({"row": [self.leaf(0, true), self.leaf(1, false)]})),
            false,
        );
    }

    fn start(&self, config: &str, extra: &[&str]) -> Nvim {
        let config_path = self.root.join("init.lua");
        std::fs::write(&config_path, format!("vim.o.swapfile = false\n{config}\n")).unwrap();
        let stderr = std::fs::File::create(self.root.join("stderr")).unwrap();
        let mut command = Command::new("nvim");
        command.args(["--embed", "-i", "NONE", "--cmd", "set noswapfile"]);
        command.args(self.channel.nvim_args());
        if config.is_empty() {
            command.arg("--clean");
        } else {
            command.args(["--noplugin", "-u"]).arg(config_path);
        }
        let mut child = command
            .args(extra)
            .current_dir(self.root.join("project"))
            .envs(self.channel.child_env())
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
            .env("NVIM_LOG_FILE", self.root.join("nvim.log"))
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .expect("nvim on PATH");
        let input = child.stdin.take();
        let output = child.stdout.take().unwrap();
        let (tx, replies) = mpsc::channel();
        std::thread::spawn(move || {
            let mut read = BufReader::new(output);
            while let Ok(value) = rmpv::decode::read_value(&mut read) {
                if value[0].as_i64() == Some(1) && tx.send(value).is_err() {
                    break;
                }
            }
        });
        let mut nvim = Nvim {
            child,
            input,
            replies,
            next: 0,
        };
        nvim.call(
            "nvim_ui_attach",
            vec![100.into(), 50.into(), Msg::Map(vec![("rgb".into(), true.into())])],
        );
        nvim
    }

    fn messages(&mut self) -> &[PaneMessage] {
        self.seen.extend(self.reader.poll());
        &self.seen
    }

    fn ended(&mut self) -> (u32, u32, u32, u32, u64) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            for message in self.messages() {
                if let PaneMessage::Reopen(ReopenLine::Ended {
                    token,
                    opened,
                    gone,
                    failed,
                    squeezed,
                    micros,
                }) = message
                {
                    assert_eq!(token, TOKEN);
                    return (*opened, *gone, *failed, *squeezed, *micros);
                }
            }
            assert!(
                Instant::now() < deadline,
                "restore sends its ended line: {:?}",
                self.seen
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn handoff_path(&self) -> PathBuf {
        self.channel.dir().join(format!("reopen-out-{TOKEN}.json"))
    }

    fn handoff(&self) -> Value {
        assert_eq!(
            std::fs::metadata(self.handoff_path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let value: Value =
            serde_json::from_slice(&std::fs::read(self.handoff_path()).expect("handoff exists")).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 2);
        assert!(value["same"].is_boolean());
        assert_eq!(value["record"]["v"], 1);
        for tab in value["record"]["tabs"].as_array().into_iter().flatten() {
            for leaf in leaves(&tab["tree"]) {
                assert!(STANDARD.decode(leaf["f"].as_str().unwrap()).unwrap().starts_with(b"/"));
                assert!(leaf["lnum"].as_u64().unwrap() >= 1);
                assert!(leaf["top"].as_u64().unwrap() >= 1);
            }
        }
        value
    }

    fn positive_control(&mut self) {
        self.simple();
        let mut nvim = self.start("", &[]);
        nvim.done();
        assert_eq!(self.ended().0, 2);
        assert_eq!(nvim.lua("return #vim.api.nvim_list_wins()"), 2);
        nvim.quit();
        assert!(self.handoff()["same"].as_bool().unwrap());
        std::fs::remove_file(self.handoff_path()).unwrap();
        self.seen.clear();
        self.reader.poll();
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.channel.cleanup();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct Nvim {
    child: Child,
    input: Option<ChildStdin>,
    replies: Receiver<Msg>,
    next: u64,
}

impl Nvim {
    fn send(&mut self, method: &str, params: Vec<Msg>) -> u64 {
        self.next += 1;
        let value = Msg::Array(vec![0.into(), self.next.into(), method.into(), Msg::Array(params)]);
        let input = self.input.as_mut().unwrap();
        rmpv::encode::write_value(input, &value).unwrap();
        input.flush().unwrap();
        self.next
    }

    fn call(&mut self, method: &str, params: Vec<Msg>) -> Msg {
        let id = self.send(method, params);
        loop {
            let reply = self
                .replies
                .recv_timeout(DEADLINE)
                .expect("nvim answers within the deadline");
            if reply[1].as_u64() == Some(id) {
                assert!(reply[2].is_nil(), "RPC {method}: {:?}", reply[2]);
                return reply[3].clone();
            }
        }
    }

    fn lua(&mut self, code: &str) -> Value {
        let result = self.call(
            "nvim_exec_lua",
            vec![
                format!("local result=(function() {code} end)(); return vim.json.encode(result)").into(),
                Msg::Array(vec![]),
            ],
        );
        serde_json::from_str(result.as_str().expect("JSON answer")).unwrap()
    }

    fn entered(&mut self) {
        let deadline = Instant::now() + DEADLINE;
        while self.lua("return vim.v.vim_did_enter") != 1 {
            assert!(Instant::now() < deadline, "VimEnter runs");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn done(&mut self) {
        let deadline = Instant::now() + DEADLINE;
        while self.lua("local s=package.loaded['eitri.reopen']; return type(s)=='table' and s.done=='0123456789abcdef'")
            != true
        {
            assert!(Instant::now() < deadline, "the restore finishes");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn capture(&mut self) -> Value {
        self.lua("local p=vim.env.EITRI_REOPEN_LUA:match('^(.*)/'); local input=vim.json.decode(table.concat(vim.fn.readfile(p..'/reopen-in.json'), '\\n')); local r=assert(loadfile(p..'/editor_reopen_capture.lua'))(input); return r and vim.json.decode(r.json) or vim.NIL")
    }

    fn resize(&mut self, w: i64, h: i64) {
        self.call("nvim_ui_try_resize", vec![w.into(), h.into()]);
    }

    fn wait_exit(&mut self) {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if self.child.try_wait().unwrap().is_some() {
                return;
            }
            assert!(Instant::now() < deadline, "nvim exits within the deadline");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn quit(&mut self) {
        self.send("nvim_command", vec!["qa!".into()]);
        self.wait_exit();
    }
}

impl Drop for Nvim {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn leaves(tree: &Value) -> Vec<&Value> {
    if let Some(leaf) = tree.get("leaf") {
        return vec![leaf];
    }
    tree.get("row")
        .or_else(|| tree.get("col"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(leaves)
        .collect()
}

#[test]
#[ignore = "needs a real nvim on PATH and a private pane-switch socket; no model or display"]
fn a_three_tab_record_comes_back() {
    let mut rig = Rig::new("three-tabs");
    let mut record = rig.record(json!({"row": [rig.leaf(0, false), {"col": [rig.leaf(1, true), rig.leaf(2, false)]}]}));
    record["tabs"]
        .as_array_mut()
        .unwrap()
        .push(json!({"cols":100,"lines":48,"tree":{"row":[rig.leaf(3, false),rig.leaf(4, true)]}}));
    record["tabs"]
        .as_array_mut()
        .unwrap()
        .push(json!({"cols":100,"lines":48,"tree":rig.leaf(6, true)}));
    record["tab"] = json!(2);
    record["bufs"] = json!([rig.name("5.txt")]);
    rig.input(record.clone(), false);
    let mut nvim = rig.start("vim.o.scrolloff=0", &[]);
    nvim.done();
    let got = nvim.capture();
    assert_eq!(got["tab"], 2);
    assert_eq!(got["tabs"].as_array().unwrap().len(), 3);
    assert!(got["tabs"][0]["tree"]["row"][1].get("col").is_some());
    for (actual, expected) in got["tabs"]
        .as_array()
        .unwrap()
        .iter()
        .zip(record["tabs"].as_array().unwrap())
    {
        let actual = leaves(&actual["tree"]);
        let expected = leaves(&expected["tree"]);
        assert_eq!(actual.len(), expected.len());
        for (a, e) in actual.iter().zip(expected) {
            for field in ["f", "lnum", "col", "top", "cur"] {
                assert_eq!(a[field], e[field], "{field}");
            }
        }
    }
    assert_eq!(got["bufs"], record["bufs"]);
    assert_eq!(nvim.lua("for _,b in ipairs(vim.api.nvim_list_bufs()) do if vim.bo[b].buflisted and vim.api.nvim_buf_get_name(b)=='' then return false end end return true"), true);
    assert_eq!(rig.ended().0, 6);
}

#[test]
#[ignore = "needs a real nvim on PATH and a private pane-switch socket; no model or display"]
fn sizes_follow_shares_and_one_later_resize() {
    let mut rig = Rig::new("sizes");
    let mut a = rig.leaf(0, true);
    a["leaf"]["w"] = json!(30);
    let mut b = rig.leaf(1, false);
    b["leaf"]["w"] = json!(69);
    rig.input(rig.record(json!({"row":[a,b]})), false);
    let mut nvim = rig.start("", &[]);
    nvim.entered();
    nvim.resize(160, 70);
    nvim.done();
    for (columns, lines) in [(160, 70), (200, 90)] {
        nvim.resize(columns, lines);
        nvim.lua("vim.wait(200); return true");
        let record = nvim.capture();
        let actual = leaves(&record["tabs"][0]["tree"]);
        let first = actual[0]["w"].as_f64().unwrap();
        let total = first + actual[1]["w"].as_f64().unwrap();
        assert!(
            (first - total * 30.0 / 99.0).abs() <= 1.0,
            "widths follow the saved shares: {actual:?}"
        );
    }
    assert_eq!(rig.ended().3, 0);
}

fn grid(rig: &Rig, rows: usize, cols: usize) -> Value {
    json!({"col": (0..rows).map(|r| json!({"row": (0..cols).map(|c| rig.leaf(r*cols+c, r+c==0)).collect::<Vec<_>>()})).collect::<Vec<_>>()})
}

#[test]
#[ignore = "needs a real nvim on PATH and a private pane-switch socket; no model or display"]
fn a_32_leaf_record_survives_the_provisional_grid() {
    let mut rig = Rig::new("32-leaves");
    rig.input(rig.record(grid(&rig, 8, 4)), false);
    let mut nvim = rig.start("", &[]);
    nvim.entered();
    assert_eq!(nvim.lua("return #vim.api.nvim_list_wins()"), 1);
    nvim.resize(200, 90);
    nvim.done();
    assert_eq!(leaves(&nvim.capture()["tabs"][0]["tree"]).len(), 32);
    let ended = rig.ended();
    assert_eq!((ended.0, ended.3), (32, 0));
}

#[test]
#[ignore = "needs a real nvim on PATH and a private pane-switch socket; no model or display"]
fn phase_two_runs_after_its_second_without_a_resize() {
    let mut rig = Rig::new("fallback");
    rig.simple();
    let mut nvim = rig.start("", &[]);
    nvim.done();
    assert_eq!(nvim.lua("return #vim.api.nvim_list_wins()"), 2);
    assert_eq!(rig.ended().0, 2);
}

#[test]
#[ignore = "needs a real nvim on PATH and a private pane-switch socket; no model or display"]
fn a_key_between_phases_lists_the_rest() {
    let mut rig = Rig::new("key");
    rig.positive_control();
    rig.simple();
    let mut nvim = rig.start("", &[]);
    nvim.entered();
    nvim.call("nvim_input", vec!["iUSER\u{1b}".into()]);
    nvim.lua("return vim.wait(500, function() return vim.bo.modified end, 10)");
    nvim.done();
    assert_eq!(nvim.lua("return #vim.api.nvim_list_wins()"), 1);
    assert_eq!(
        nvim.lua("return vim.api.nvim_get_current_line():find('USER',1,true) ~= nil"),
        true
    );
    let record = nvim.capture();
    assert_eq!(record["bufs"], json!([rig.name("1.txt")]));
    assert_eq!(rig.ended().3, 1);
}

#[test]
#[ignore = "needs a real nvim on PATH and a private pane-switch socket; no model or display"]
fn capture_before_vimenter_answers_nil() {
    let rig = Rig::new("early");
    rig.simple();
    let mut nvim = rig.start("", &["--cmd", "lua vim.wait(1500)"]);
    assert_eq!(nvim.lua("return vim.v.vim_did_enter"), 0);
    assert_eq!(nvim.capture(), Value::Null);
    nvim.done();
    assert!(!nvim.capture().is_null());
}

#[test]
#[ignore = "needs a real nvim on PATH and a private pane-switch socket; no model or display"]
fn hostile_names_each_open_in_a_window() {
    let mut rig = Rig::new("hostile");
    let names = [
        "space name",
        "g\nso.vim",
        "pipe|source so.vim",
        "single'quote",
        "double\"quote",
        "$(touch sentinel)",
        "`touch sentinel`",
        "+command",
        "++option",
        "中文",
        "tab\tname",
        "%",
        "#",
        "*",
        "[",
        "\\",
        "~",
        "{",
        "trailing ",
    ];
    std::fs::write(rig.root.join("project/so.vim"), "call writefile(['ran'], 'sourced')").unwrap();
    std::fs::create_dir(rig.root.join("project/sentinel-dir")).unwrap();
    std::fs::write(rig.root.join("project/sentinel-dir/untouched"), "sentinel").unwrap();
    let mut children = Vec::new();
    for name in names {
        std::fs::write(rig.root.join("project").join(name), "file\n").unwrap();
        let mut leaf = rig.leaf(0, children.is_empty());
        leaf["leaf"]["f"] = json!(rig.name(name));
        children.push(leaf);
    }
    // Byte strings need not be UTF-8 to pass through JSON's base64 field.
    let non_utf8 = std::ffi::OsStr::from_bytes(b"invalid-\xff");
    let path = rig.root.join("project").join(non_utf8);
    std::fs::write(&path, "bytes\n").unwrap();
    let mut leaf = rig.leaf(0, false);
    leaf["leaf"]["f"] = json!(STANDARD.encode(path.as_os_str().as_bytes()));
    children.push(leaf);
    std::fs::write(rig.root.join("actual.txt"), "target\n").unwrap();
    symlink(rig.root.join("actual.txt"), rig.root.join("project/alias")).unwrap();
    let mut alias = rig.leaf(0, false);
    alias["leaf"]["f"] = json!(rig.name("alias"));
    children.push(alias);
    let expected: Vec<_> = children.iter().map(|n| n["leaf"]["f"].clone()).collect();
    // Vertical stacking fits the names without depending on Neovim's minimum column width.
    rig.input(rig.record(json!({"col":children})), false);
    let mut nvim = rig.start("", &[]);
    nvim.entered();
    nvim.resize(200, 90);
    nvim.done();
    let captured = nvim.capture();
    let got: Vec<_> = leaves(&captured["tabs"][0]["tree"])
        .iter()
        .map(|l| l["f"].clone())
        .collect();
    assert_eq!(got, expected);
    assert_eq!(rig.ended().0 as usize, expected.len());
    assert!(!rig.root.join("project/sourced").exists());
    assert!(!rig.root.join("project/sentinel").exists());
    assert_eq!(
        std::fs::read_to_string(rig.root.join("project/sentinel-dir/untouched")).unwrap(),
        "sentinel"
    );
}

#[test]
#[ignore = "needs a real nvim on PATH and a private pane-switch socket; no model or display"]
fn gone_and_fifo_files_are_counted_and_named() {
    let mut rig = Rig::new("gone");
    rig.input(
        rig.record(json!({"col":[rig.leaf(0, true),rig.leaf(1, false),rig.leaf(2, false)]})),
        false,
    );
    std::fs::remove_file(rig.root.join("project/0.txt")).unwrap();
    std::fs::remove_file(rig.root.join("project/1.txt")).unwrap();
    let fifo = std::ffi::CString::new(rig.root.join("project/1.txt").as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let mut nvim = rig.start("", &[]);
    nvim.done();
    let ended = rig.ended();
    assert_eq!((ended.0, ended.1, ended.2), (1, 2, 0));
    nvim.lua("vim.wait(100); return true");
    let names: Vec<_> = rig
        .messages()
        .iter()
        .filter_map(|m| match m {
            PaneMessage::Reopen(ReopenLine::Name { gone: true, name, .. }) => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(names, ["0.txt"]);
}

#[test]
#[ignore = "needs a real nvim on PATH and a private pane-switch socket; no model or display"]
fn an_unreadable_file_counts_failed_and_the_start_goes_on() {
    let mut rig = Rig::new("unreadable");
    rig.simple();
    std::fs::set_permissions(rig.root.join("project/0.txt"), std::fs::Permissions::from_mode(0)).unwrap();
    let mut nvim = rig.start("", &[]);
    nvim.done();
    let ended = rig.ended();
    assert_eq!((ended.0, ended.1, ended.2), (1, 0, 1));
    nvim.lua("vim.wait(100); return true");
    assert!(rig
        .messages()
        .iter()
        .any(|m| matches!(m, PaneMessage::Reopen(ReopenLine::Name {gone:false,name,..}) if name=="0.txt")));

    let mut rig = Rig::new("read-error");
    rig.simple();
    let mut nvim = rig.start(
        "vim.api.nvim_create_autocmd('BufReadPost',{pattern='*0.txt',callback=function() error('file refused') end})",
        &[],
    );
    nvim.done();
    let ended = rig.ended();
    assert_eq!((ended.0, ended.1, ended.2), (1, 0, 1));
    assert_eq!(leaves(&nvim.capture()["tabs"][0]["tree"])[0]["f"], rig.name("1.txt"));
}

#[test]
#[ignore = "needs a real nvim on PATH and a private pane-switch socket; no model or display"]
fn the_restored_cursor_wins_over_a_bufreadpost_jump() {
    let rig = Rig::new("cursor");
    rig.simple();
    let mut nvim = rig.start(
        "vim.api.nvim_create_autocmd('BufReadPost',{callback=function() vim.api.nvim_win_set_cursor(0,{3,0}) end})",
        &[],
    );
    nvim.done();
    for leaf in leaves(&nvim.capture()["tabs"][0]["tree"]) {
        assert!(leaf["lnum"].as_u64().unwrap() >= 80);
        assert_eq!(leaf["col"], 4);
    }
}

#[test]
#[ignore = "needs a real nvim on PATH and a private pane-switch socket; no model or display"]
fn too_many_leaves_are_squeezed_into_bufs() {
    let mut rig = Rig::new("squeezed");
    rig.input(rig.record(grid(&rig, 8, 4)), false);
    let mut nvim = rig.start("", &[]);
    nvim.entered();
    nvim.resize(24, 8);
    nvim.done();
    let ended = rig.ended();
    assert!(ended.3 > 0);
    assert_eq!(ended.0 + ended.3, 32);
    let record = nvim.capture();
    assert_eq!(
        leaves(&record["tabs"][0]["tree"]).len() + record["bufs"].as_array().unwrap().len(),
        32
    );
    assert_eq!(nvim.lua("local p=vim.env.EITRI_REOPEN_LUA:match('^(.*)/'); local a=vim.json.decode(table.concat(vim.fn.readfile(p..'/reopen-in.json'),'\\n')); return assert(loadfile(p..'/editor_reopen_capture.lua'))(a).same"), true);
}

#[test]
#[ignore = "needs a real nvim on PATH and a private pane-switch socket; no model or display"]
fn stand_aside_for_each_kind() {
    let cases = [
        ("vim.v.this_session='session.vim'", "this_session"),
        ("_G.MiniSessions={}", "mini.sessions"),
        ("package.loaded['persistence']={}", "persistence"),
        ("package.loaded['auto-session']={}", "auto-session"),
        ("package.loaded['resession']={}", "resession"),
        ("package.loaded['possession']={}", "possession"),
        ("package.loaded['nvim-possession']={}", "nvim-possession"),
        ("package.loaded['session_manager']={}", "session_manager"),
        ("package.loaded['persisted']={}", "persisted"),
        ("vim.g.startify_session_autoload=1", "startify"),
        ("vim.g.loaded_prosession=1", "prosession"),
        (
            "package.loaded['lazy.core.config']={plugins={['auto-session']={}}}",
            "auto-session",
        ),
    ];
    for (i, (config, word)) in cases.iter().enumerate() {
        let mut rig = Rig::new(&format!("aside-{i}"));
        rig.positive_control();
        rig.simple();
        let mut nvim = rig.start(config, &[]);
        nvim.entered();
        assert_eq!(nvim.lua("vim.wait(1200); return vim.api.nvim_buf_get_name(0)"), "");
        nvim.quit();
        assert!(!rig.handoff_path().exists());
        assert!(
            rig.messages()
                .iter()
                .any(|m| matches!(m,PaneMessage::Reopen(ReopenLine::Aside{word:w,..}) if w.as_str()==*word)),
            "{word}: {:?}",
            rig.seen
        );
        assert!(!rig
            .seen
            .iter()
            .any(|m| matches!(m, PaneMessage::Reopen(ReopenLine::Began { .. }))));
    }
}

#[test]
#[ignore = "needs a real nvim on PATH and a private pane-switch socket; no model or display"]
fn no_restore_with_args_a_config_file_or_no_input() {
    for kind in ["args", "screen", "off"] {
        let mut rig = Rig::new(kind);
        rig.positive_control();
        rig.simple();
        let mut nvim = match kind {
            "args" => rig.start("", &["2.txt"]),
            "screen" => rig.start(
                "vim.api.nvim_cmd({cmd='edit',args={vim.fn.getcwd()..'/2.txt'},magic={file=false}},{})",
                &[],
            ),
            _ => {
                std::fs::remove_file(rig.channel.dir().join("reopen-in.json")).unwrap();
                rig.start("", &[])
            }
        };
        nvim.entered();
        nvim.lua("vim.wait(1200); return true");
        nvim.quit();
        assert!(!rig.handoff_path().exists());
        assert!(
            rig.messages()
                .iter()
                .any(|m| matches!(m,PaneMessage::Reopen(ReopenLine::Aside{word,..}) if word.as_str()==kind)),
            "{}: {:?}",
            kind,
            rig.seen
        );
    }
}

#[test]
#[ignore = "needs a real nvim on PATH and a private pane-switch socket; no model or display"]
fn skip_true_restores_nothing_but_arms_the_handoff() {
    let mut rig = Rig::new("skip");
    rig.positive_control();
    rig.input(rig.record(rig.leaf(0, true)), true);
    let mut nvim = rig.start("", &[]);
    nvim.done();
    assert_eq!(nvim.lua("return vim.api.nvim_buf_get_name(0)"), "");
    nvim.lua("vim.api.nvim_cmd({cmd='edit',args={vim.fn.getcwd()..'/2.txt'},magic={file=false}},{}); return true");
    nvim.quit();
    assert_eq!(
        leaves(&rig.handoff()["record"]["tabs"][0]["tree"])[0]["f"],
        rig.name("2.txt")
    );
    assert_eq!(rig.ended().0, 0);
}

#[test]
#[ignore = "needs a real nvim on PATH and a private pane-switch socket; no model or display"]
fn autocmd_bang_in_a_config_restores_nothing_and_says_nothing() {
    let mut rig = Rig::new("autocmd");
    rig.positive_control();
    rig.simple();
    let mut nvim = rig.start("vim.cmd('autocmd!')", &[]);
    nvim.entered();
    assert_eq!(nvim.lua("vim.wait(1200); return vim.api.nvim_buf_get_name(0)"), "");
    nvim.quit();
    assert!(!rig.handoff_path().exists());
    assert!(!rig.messages().iter().any(|m| matches!(m, PaneMessage::Reopen(_))));
}

fn swap_owner(rig: &Rig, dirty: bool) -> Nvim {
    let input_path = rig.channel.dir().join("reopen-in.json");
    let input = std::fs::read(&input_path).unwrap();
    std::fs::remove_file(&input_path).unwrap();
    let mut owner = rig.start("vim.o.swapfile=true; vim.o.updatetime=50", &["0.txt"]);
    owner.entered();
    owner.lua(if dirty {
        "vim.api.nvim_buf_set_lines(0,0,1,false,{'UNSAVED'}); vim.cmd('preserve'); return true"
    } else {
        "vim.cmd('preserve'); return true"
    });
    std::fs::write(input_path, input).unwrap();
    owner
}

#[test]
#[ignore = "needs a real nvim on PATH and a private pane-switch socket; no model or display"]
fn a_dirty_dead_swap_opens_read_only_and_e_asks_again() {
    let mut rig = Rig::new("dirty-swap");
    rig.simple();
    let mut owner = swap_owner(&rig, true);
    owner.child.kill().unwrap();
    owner.wait_exit();
    rig.reader.poll();
    let mut nvim = rig.start("vim.o.swapfile=true", &[]);
    nvim.done();
    assert_eq!(nvim.lua("return vim.bo.readonly"), true);
    assert_ne!(nvim.lua("return vim.api.nvim_buf_get_lines(0,0,1,false)[1]"), "UNSAVED");
    assert_eq!(nvim.lua("local n=0; local id=vim.api.nvim_create_autocmd('SwapExists',{callback=function() n=n+1; vim.v.swapchoice='q' end}); pcall(vim.cmd,'edit'); vim.api.nvim_del_autocmd(id); return n"),1);
    nvim.lua("vim.wait(200); return true");
    assert!(rig
        .messages()
        .iter()
        .any(|m| matches!(m,PaneMessage::Recoverable{count,..} if *count>=1)));
}

#[test]
#[ignore = "needs a real nvim on PATH and a private pane-switch socket; no model or display"]
fn a_clean_dead_swap_opens_writable() {
    let rig = Rig::new("clean-swap");
    rig.simple();
    let mut owner = swap_owner(&rig, false);
    owner.child.kill().unwrap();
    owner.wait_exit();
    let mut nvim = rig.start("vim.o.swapfile=true", &[]);
    nvim.done();
    assert_eq!(nvim.lua("return vim.bo.readonly"), false);
}

#[test]
#[ignore = "needs a real nvim on PATH and a private pane-switch socket; no model or display"]
fn a_live_swap_opens_read_only_over_the_default() {
    let rig = Rig::new("live-swap");
    rig.simple();
    let _owner = swap_owner(&rig, true);
    let mut nvim = rig.start("vim.o.swapfile=true", &[]);
    nvim.done();
    assert_eq!(nvim.lua("return vim.bo.readonly"), true);
}

#[test]
#[ignore = "needs a real nvim on PATH and a private pane-switch socket; no model or display"]
fn the_handoff_is_written_on_qa_sigterm_sighup_and_stdin_close_and_parses() {
    for how in ["qa", "term", "hup", "stdin"] {
        let rig = Rig::new(&format!("handoff-{how}"));
        rig.simple();
        let mut nvim = rig.start("vim.o.scrolloff=0", &[]);
        nvim.done();
        let expected = nvim.capture();
        if how == "qa" {
            nvim.quit();
        } else if how == "stdin" {
            nvim.input.take();
            nvim.wait_exit();
        } else {
            assert_eq!(
                unsafe {
                    libc::kill(
                        nvim.child.id() as i32,
                        if how == "term" { libc::SIGTERM } else { libc::SIGHUP },
                    )
                },
                0
            );
            nvim.wait_exit();
        }
        let handoff = rig.handoff();
        assert_eq!(handoff["record"]["root"], expected["root"]);
        let actual_tabs = handoff["record"]["tabs"].as_array().unwrap();
        let expected_tabs = expected["tabs"].as_array().unwrap();
        assert_eq!(actual_tabs.len(), expected_tabs.len(), "{how}");
        for (actual, expected) in actual_tabs.iter().zip(expected_tabs) {
            let actual = leaves(&actual["tree"]);
            let expected = leaves(&expected["tree"]);
            assert_eq!(actual.len(), expected.len(), "{how}");
            for (actual, expected) in actual.iter().zip(expected) {
                for field in ["f", "lnum", "top"] {
                    assert_eq!(actual[field], expected[field], "{how}: {field}");
                }
            }
        }
        if how == "qa" {
            assert_eq!(handoff["record"], expected);
            assert_eq!(handoff["same"], true);
        }
    }
}

#[test]
#[ignore = "needs a real nvim on PATH and a private pane-switch socket; no model or display"]
fn no_handoff_on_sigkill_or_before_phase_two() {
    let mut rig = Rig::new("no-handoff");
    rig.positive_control();
    rig.simple();
    let mut nvim = rig.start("", &[]);
    nvim.done();
    nvim.child.kill().unwrap();
    nvim.wait_exit();
    assert!(!rig.handoff_path().exists());
    let mut nvim = rig.start("", &[]);
    nvim.entered();
    nvim.quit();
    assert!(!rig.handoff_path().exists());
}

#[test]
#[ignore = "needs a real nvim on PATH and a private pane-switch socket; no model or display"]
fn a_planted_tmp_symlink_or_fifo_is_left_alone_and_nvim_exits() {
    for kind in ["symlink", "fifo"] {
        let mut rig = Rig::new(&format!("planted-{kind}"));
        rig.positive_control();
        rig.simple();
        let target = rig.root.join("untouched");
        std::fs::write(&target, "safe").unwrap();
        let tmp = rig.channel.dir().join(format!("reopen-out-{TOKEN}-11223344.tmp"));
        if kind == "symlink" {
            symlink(&target, &tmp).unwrap();
        } else {
            let path = std::ffi::CString::new(tmp.as_os_str().as_bytes()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        }
        let mut nvim = rig.start("", &[]);
        nvim.done();
        // Force the random suffix to collide, including implementations using either byte or integer randomness.
        nvim.lua("vim.uv.random=function(n) return string.char(0x11,0x22,0x33,0x44):sub(1,n) end; math.random=function() return 0x11223344 end; return true");
        nvim.quit();
        assert_eq!(std::fs::read_to_string(target).unwrap(), "safe");
        let metadata = std::fs::symlink_metadata(tmp).unwrap();
        assert!(if kind == "symlink" {
            metadata.file_type().is_symlink()
        } else {
            std::os::unix::fs::FileTypeExt::is_fifo(&metadata.file_type())
        });
        assert!(
            !rig.handoff_path().exists(),
            "an exclusive-create collision skips the handoff"
        );
    }
}

#[test]
#[ignore = "needs a real nvim on PATH and a private pane-switch socket; no model or display"]
fn a_hang_in_bufread_sends_b_without_o() {
    let mut rig = Rig::new("hang");
    rig.simple();
    let mut nvim = rig.start(
        "vim.api.nvim_create_autocmd('BufReadPre',{pattern='*0.txt',callback=function() while true do end end})",
        &[],
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline
        && !rig
            .messages()
            .iter()
            .any(|m| matches!(m, PaneMessage::Reopen(ReopenLine::Began { .. })))
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    nvim.child.kill().unwrap();
    nvim.wait_exit();
    assert!(rig
        .messages()
        .iter()
        .any(|m| matches!(m, PaneMessage::Reopen(ReopenLine::Began { .. }))));
    assert!(!rig
        .seen
        .iter()
        .any(|m| matches!(m, PaneMessage::Reopen(ReopenLine::Ended { .. }))));
    assert!(!rig.handoff_path().exists());
}

#[test]
#[ignore = "needs a real nvim on PATH and a private pane-switch socket; no model or display"]
fn the_ended_line_carries_micros() {
    let mut rig = Rig::new("micros");
    rig.simple();
    let mut nvim = rig.start("", &[]);
    nvim.done();
    assert!(rig.ended().4 > 0, "elapsed microseconds are carried on the ended line");
}

#[test]
#[ignore = "needs a real nvim on PATH and a private pane-switch socket; no model or display"]
fn restore_cost_against_plain_opens() {
    // Each calibration starts with unloaded buffers in the process that just restored the same files.
    const CALIBRATION: &str = r#"
vim.api.nvim_create_autocmd('VimEnter', {once=true, callback=function()
  local function calibrate()
    local state = package.loaded['eitri.reopen']
    if type(state) ~= 'table' or state.done ~= '0123456789abcdef' then
      vim.defer_fn(calibrate, 10)
      return
    end
    local dir = assert(vim.env.EITRI_REOPEN_LUA:match('^(.*)/'))
    local input = vim.json.decode(table.concat(vim.fn.readfile(dir .. '/reopen-in.json'), '\n'))
    local tabs = {}
    local function collect(tree, names)
      if tree.leaf then
        local name = vim.base64.decode(tree.leaf.f)
        local stat = vim.uv.fs_stat(name)
        assert(name:sub(1, 1) == '/' and stat and stat.type == 'file')
        names[#names + 1] = name
      else
        for _, child in ipairs(tree.row or tree.col) do collect(child, names) end
      end
    end
    for _, tab in ipairs(input.record.tabs) do
      local names = {}
      collect(tab.tree, names)
      tabs[#tabs + 1] = names
    end
    vim.api.nvim_cmd({cmd='tabonly', bang=true}, {})
    vim.api.nvim_cmd({cmd='enew', bang=true}, {})
    local empty = vim.api.nvim_get_current_buf()
    for _, buf in ipairs(vim.api.nvim_list_bufs()) do
      if buf ~= empty then vim.api.nvim_buf_delete(buf, {force=true}) end
    end
    local started = vim.uv.hrtime()
    local count = 0
    for t, names in ipairs(tabs) do
      for f, name in ipairs(names) do
        vim.api.nvim_cmd({cmd=(t > 1 and f == 1) and 'tabnew' or 'edit',
          args={name}, magic={file=false}}, {})
        count = count + 1
      end
    end
    vim.g.reopen_calibration = {micros=math.max(1, math.floor((vim.uv.hrtime()-started)/1000)), count=count}
  end
  vim.schedule(calibrate)
end})
"#;

    let mut medians = Vec::new();
    for (files, tabs) in [(2, 1), (10, 3), (32, 8)] {
        let mut restores = Vec::new();
        let mut calibrations = Vec::new();
        for run in 0..5 {
            let mut rig = Rig::new(&format!("cost-{files}-{tabs}-{run}"));
            let mut record = rig.record(rig.leaf(0, true));
            let mut next_file = 0;
            record["tabs"] = json!((0..tabs)
                .map(|tab| {
                    let count = files / tabs + usize::from(tab < files % tabs);
                    let nodes = (0..count)
                        .map(|index| {
                            let leaf = rig.leaf(next_file, index == 0);
                            next_file += 1;
                            leaf
                        })
                        .collect::<Vec<_>>();
                    json!({"cols":100, "lines":48, "tree":{"row":nodes}})
                })
                .collect::<Vec<_>>());
            assert_eq!(next_file, files);
            rig.input(record, false);
            let mut nvim = rig.start(CALIBRATION, &[]);
            nvim.done();
            let (opened, gone, failed, squeezed, micros) = rig.ended();
            assert_eq!((opened as usize, gone, failed, squeezed), (files, 0, 0, 0));
            let deadline = Instant::now() + DEADLINE;
            let calibration = loop {
                let result = nvim.lua("return vim.g.reopen_calibration or vim.NIL");
                if !result.is_null() {
                    break result;
                }
                assert!(Instant::now() < deadline, "the VimEnter calibration finishes");
                std::thread::sleep(Duration::from_millis(10));
            };
            assert_eq!(calibration["count"], files);
            assert_eq!(nvim.lua("return #vim.api.nvim_list_tabpages()"), tabs);
            restores.push(micros);
            calibrations.push(calibration["micros"].as_u64().unwrap());
            nvim.quit();
        }
        restores.sort_unstable();
        calibrations.sort_unstable();
        let (restore, calibration) = (restores[2], calibrations[2]);
        println!("{files} files/{tabs} tabs: restore median {restore} us, plain opens median {calibration} us");
        medians.push((files, tabs, restore, calibration));
    }
    for (files, tabs, restore, calibration) in medians {
        assert!(
            restore <= 3 * calibration,
            "{files} files/{tabs} tabs: restore median {restore} us exceeds 3x plain opens median {calibration} us"
        );
    }
}
