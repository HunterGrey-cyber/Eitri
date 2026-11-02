//! Capturing file windows with headless Neovim and isolated state; no model or display.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use eitri_core::pane_switch::PaneSwitchChannel;
use serde_json::Value;

struct Rig {
    root: PathBuf,
    channel: PaneSwitchChannel,
}

impl Rig {
    fn new(label: &str) -> Self {
        let root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("editor-reopen-capture-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("project/private")).unwrap();
        std::fs::create_dir_all(root.join("other")).unwrap();
        for name in ["a.txt", "b.txt", "c.txt", "listed.txt", "private/scratch.txt"] {
            std::fs::write(root.join("project").join(name), "abcdefghij\nsecond line\nthird line\n").unwrap();
        }
        Self {
            root,
            channel: PaneSwitchChannel::bind_without_shim().expect("the channel binds"),
        }
    }

    fn run(&self, body: &str) -> Value {
        let script = format!(
            r#"
local api = vim.api
local project = vim.env.EITRI_CAPTURE_PROJECT
local args = {{ token = '0123456789abcdef', root = vim.base64.encode(project),
  prefixes = {{ vim.base64.encode(project .. '/private/') }} }}
local capture = assert(loadfile(vim.env.EITRI_CAPTURE_SOURCE))
local function take() return capture(args) end
local function record() local result = assert(take(), 'capture answers after done'); return vim.json.decode(result.json) end
local function listed(name)
  local buf = vim.fn.bufadd(name)
  vim.bo[buf].buflisted = true
  return buf
end
local function open(name, cmd)
  local path = project .. '/' .. name
  assert(path:sub(1, 1) == '/' and vim.uv.fs_stat(path).type == 'file')
  api.nvim_cmd({{ cmd = cmd or 'edit', args = {{ path }}, magic = {{ file = false }} }}, {{}})
end
package.loaded['eitri.reopen'] = {{ done = args.token }}
local result = (function()
{body}
end)()
vim.fn.writefile({{ vim.json.encode(result) }}, vim.env.EITRI_CAPTURE_OUTPUT)
"#
        );
        let script_path = self.root.join("test.lua");
        std::fs::write(&script_path, script).unwrap();
        let output = Command::new("nvim")
            .args(["--headless", "--clean", "-i", "NONE", "--cmd", "set noswapfile hidden"])
            .args(self.channel.nvim_args())
            .args(["-c", "lua local ok,e=pcall(dofile,vim.env.EITRI_CAPTURE_SCRIPT); if not ok then io.stderr:write(tostring(e)); vim.cmd('cquit') end", "-c", "qa!"])
            .current_dir(self.root.join("project"))
            .envs(self.channel.child_env())
            .env("EITRI_CAPTURE_PROJECT", self.root.join("project"))
            .env("EITRI_CAPTURE_SCRIPT", script_path)
            .env("EITRI_CAPTURE_SOURCE", Path::new(env!("CARGO_MANIFEST_DIR")).join("src/editor_reopen_capture.lua"))
            .env("EITRI_CAPTURE_OUTPUT", self.root.join("out.json"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .stdin(Stdio::null())
            .output()
            .expect("nvim on PATH");
        assert!(
            output.status.success(),
            "nvim: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&std::fs::read(self.root.join("out.json")).unwrap()).unwrap()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.channel.cleanup();
        let _ = std::fs::remove_dir_all(&self.root);
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

fn list_len(value: &Value) -> usize {
    value
        .as_array()
        .map_or_else(|| value.as_object().unwrap().len(), Vec::len)
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn capture_is_nil_until_done_matches_the_token() {
    let rig = Rig::new("gate");
    let result = rig.run(
        r#"
local answers = {}
for _, state in ipairs({ false, true, 'loaded', {}, { done = 'fedcba9876543210' } }) do
  package.loaded['eitri.reopen'] = state
  answers[#answers + 1] = take() == nil
end
package.loaded['eitri.reopen'] = { done = args.token }
answers[#answers + 1] = take() ~= nil
return answers
"#,
    );
    assert_eq!(result, serde_json::json!([true, true, true, true, true, true]));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn capture_names_splits_tabs_cursors_and_listed_buffers() {
    let rig = Rig::new("layout");
    let result = rig.run(
        r#"
vim.o.columns = 150; vim.o.lines = 50; vim.o.showtabline = 2
open('a.txt'); open('b.txt', 'vsplit'); open('c.txt', 'split')
api.nvim_win_set_cursor(0, { 2, 3 })
local view = vim.fn.winsaveview()
local w, h = api.nvim_win_get_width(0), api.nvim_win_get_height(0)
open('b.txt', 'tabedit')
listed(project .. '/listed.txt')
local r = record()
return { record = r, names = { vim.base64.encode(project .. '/a.txt'), vim.base64.encode(project .. '/b.txt'),
 vim.base64.encode(project .. '/c.txt'), vim.base64.encode(project .. '/listed.txt') }, view = view, w = w, h = h,
 root = args.root, cols = vim.o.columns, lines = vim.o.lines - vim.o.cmdheight - 1 }
"#,
    );
    let r = &result["record"];
    assert_eq!(r["v"], 1);
    assert_eq!(r["root"], result["root"]);
    assert_eq!(r["tab"], 2);
    assert_eq!(r["tabs"].as_array().unwrap().len(), 2);
    assert_eq!(r["tabs"][0]["cols"], result["cols"]);
    assert_eq!(r["tabs"][0]["lines"], result["lines"]);
    let first = leaves(&r["tabs"][0]["tree"]);
    assert_eq!(first.len(), 3);
    assert!(r["tabs"][0]["tree"].get("row").is_some());
    assert!(r["tabs"][0]["tree"]["row"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n.get("col").is_some()));
    for name in result["names"].as_array().unwrap().iter().take(3) {
        assert!(first.iter().any(|leaf| &leaf["f"] == name));
    }
    let current = first.iter().find(|leaf| leaf["cur"] == true).unwrap();
    assert_eq!(first.iter().filter(|leaf| leaf["cur"] == true).count(), 1);
    for (field, view) in [
        ("lnum", "lnum"),
        ("col", "col"),
        ("want", "curswant"),
        ("top", "topline"),
        ("left", "leftcol"),
    ] {
        assert_eq!(current[field], result["view"][view]);
    }
    assert_eq!(current["w"], result["w"]);
    assert_eq!(current["h"], result["h"]);
    assert_eq!(leaves(&r["tabs"][1]["tree"])[0]["cur"], true);
    assert_eq!(r["bufs"], serde_json::json!([result["names"][3]]));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn capture_leaves_out_terminal_help_quickfix_unnamed_unwritten_floats_prefixed_and_eitri_tabs() {
    let rig = Rig::new("excluded");
    let result = rig.run(
        r#"
open('a.txt')
local function special(kind)
  vim.cmd('new'); local b = api.nvim_create_buf(true, false); api.nvim_win_set_buf(0, b)
  if kind == 'terminal' then vim.fn.termopen({ 'sh', '-c', 'exit 0' })
  elseif kind ~= '' then vim.bo[b].buftype = kind end
end
special('terminal'); special('help'); special('quickfix'); special('')
vim.cmd('new'); api.nvim_buf_set_name(0, project .. '/not-written.txt')
open('private/scratch.txt', 'split')
local float = listed(project .. '/b.txt')
api.nvim_open_win(float, false, { relative = 'editor', row = 1, col = 1, width = 10, height = 2 })
open('c.txt', 'tabedit'); vim.t.eitri_plugin_diff = false
local r = record()
return { record = r, a = vim.base64.encode(project .. '/a.txt'), b = vim.base64.encode(project .. '/b.txt'),
 c = vim.base64.encode(project .. '/c.txt') }
"#,
    );
    let r = &result["record"];
    assert_eq!(r["tabs"].as_array().unwrap().len(), 1);
    assert!(r.get("tab").is_none());
    let saved = leaves(&r["tabs"][0]["tree"]);
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0]["f"], result["a"]);
    let bufs = r["bufs"].as_array().unwrap();
    assert_eq!(
        bufs.len(),
        2,
        "eligible listed buffers remain even when their windows are excluded"
    );
    assert!(bufs.contains(&result["b"]) && bufs.contains(&result["c"]));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn capture_names_are_absolute_after_cd_away() {
    let rig = Rig::new("cwd");
    let result = rig.run(
        r#"
open('a.txt'); listed(project .. '/listed.txt')
vim.o.autochdir = true
api.nvim_set_current_dir(project .. '/../other')
return { record = record(), root = args.root, name = vim.base64.encode(project .. '/a.txt'),
 listed = vim.base64.encode(project .. '/listed.txt') }
"#,
    );
    assert_eq!(result["record"]["root"], result["root"]);
    assert_eq!(leaves(&result["record"]["tabs"][0]["tree"])[0]["f"], result["name"]);
    assert_eq!(result["record"]["bufs"], serde_json::json!([result["listed"]]));
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn capture_trims_to_the_budget() {
    let rig = Rig::new("budget");
    let result = rig.run(
        r#"
open('a.txt')
local original = vim.uv.fs_stat
local names = {}
for i = 1, 300 do
  local suffix = '/' .. tostring(i)
  local name = project .. '/' .. string.rep('x', 4000 - #project - 1 - #suffix) .. suffix
  names[name] = true
  listed(name)
end
-- Long component names cannot exist on the test filesystem; only their stat result is substituted.
vim.uv.fs_stat = function(name, ...) if names[name] then return { type = 'file' } end; return original(name, ...) end
local captured = assert(take())
vim.uv.fs_stat = original
return { bytes = #captured.json, record = vim.json.decode(captured.json) }
"#,
    );
    assert!(result["bytes"].as_u64().unwrap() <= 224 * 1024);
    assert_eq!(result["record"]["tabs"].as_array().unwrap().len(), 1);
    let bufs = result["record"]["bufs"].as_array().unwrap();
    assert!(!bufs.is_empty() && bufs.len() < 256);
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn capture_same_is_true_only_for_the_restored_json() {
    let rig = Rig::new("same");
    let result = rig.run(
        r#"
open('a.txt')
local first = assert(take())
package.loaded['eitri.reopen'].restored = first.json
local same = assert(take())
api.nvim_win_set_cursor(0, { 2, 1 })
local changed = assert(take())
return { first = first.same, same = same.same, changed = changed.same, stable = first.json == same.json }
"#,
    );
    assert_eq!(
        result,
        serde_json::json!({"first": false, "same": true, "changed": false, "stable": true})
    );
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn capture_with_no_file_windows_has_empty_lists() {
    let rig = Rig::new("empty");
    let result = rig.run("return record()");
    assert_eq!(list_len(&result["tabs"]), 0);
    assert_eq!(list_len(&result["bufs"]), 0);
    assert!(result.get("tab").is_none());
}

#[test]
#[ignore = "needs a real nvim on PATH; spends no tokens and needs no display"]
fn the_real_capture_json_parses_with_editor_record() {
    let rig = Rig::new("parse");
    let result = rig.run(
        r#"
local empty = take().json
vim.o.columns = 150; vim.o.lines = 50
open('a.txt'); open('b.txt', 'vsplit'); open('c.txt', 'split')
api.nvim_win_set_cursor(0, { 3, 4 })
open('b.txt', 'tabedit')
listed(project .. '/listed.txt')
return { empty = empty, full = take().json, same = take().same }
"#,
    );
    let project = rig.root.join("project");
    let parse = |key: &str| {
        eitri_core::editor_record::parse(result[key].as_str().expect("the capture's JSON"), &project)
            .unwrap_or_else(|e| panic!("the capture's {key} record is refused: {e}\n{}", result[key]))
    };
    let empty = parse("empty");
    assert!(empty.tabs.is_empty() && empty.bufs.is_empty());
    let full = parse("full");
    assert_eq!(full.tabs.len(), 2);
    assert_eq!(full.tab, Some(2));
    assert_eq!(
        full.bufs,
        vec![project.join("listed.txt").into_os_string().into_encoded_bytes()]
    );
    assert_eq!(
        full.file_count(),
        4,
        "a, b and c in windows (b twice) and the listed buffer"
    );
    assert_eq!(result["same"], false);
    // What the writer writes back reads as the same record.
    assert_eq!(
        eitri_core::editor_record::parse(&full.to_json(), &project).unwrap(),
        full
    );
}
