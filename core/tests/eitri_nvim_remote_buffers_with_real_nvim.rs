//! `eitri.nvim`'s `eitri-remote://` buffers in a real Neovim: read through a fake `eitri host read`, read-only, no
//! modeline, never read synchronously, and the jump from a language server's location (a fake `eitri host lsp` that
//! runs a small scripted server answering `definition` with a path the mirror does not hold).
//!
//! Runs once per Neovim: `EITRI_TEST_NVIM` (one binary) when set, else `nvim` on `PATH` and each of
//! `EITRI_TEST_NVIM_0_10`, `_0_11`, `_0_12` that is set (or cached under `CARGO_TARGET_DIR/nvim-releases/<version>`).
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn versions() -> Vec<PathBuf> {
    if let Some(path) = std::env::var_os("EITRI_TEST_NVIM") {
        return vec![path.into()];
    }
    let mut paths = vec![PathBuf::from("nvim")];
    for (key, version) in [("0_10", "0.10.4"), ("0_11", "0.11.2"), ("0_12", "0.12.6")] {
        if let Some(path) = std::env::var_os(format!("EITRI_TEST_NVIM_{key}")) {
            paths.push(path.into());
        } else if let Some(target) = std::env::var_os("CARGO_TARGET_DIR") {
            let cached = Path::new(&target).join("nvim-releases").join(version).join("bin/nvim");
            if cached.is_file() {
                paths.push(cached);
            }
        }
    }
    paths
}

#[derive(Clone, Copy)]
enum Startup {
    /// The plugin on the runtimepath, as a plain Neovim has it.
    Plugin,
    /// Eitri's window: the modules injected with `package.preload` and no plugin directory.
    Embedded,
}

/// The server's file the fake `eitri host read` serves; a modeline in it must not be read.
/// `textwidth` because no filetype plugin sets it.
const DEPENDENCY: &str = "pub fn dep() {}\n\n    pub fn second() {}\n// vim: set tw=77 :\n";
const DEPENDENCY_PATH: &str = "/opt/dep/src/lib.rs";

const FAKE_EITRI: &str = r#"#!/usr/bin/env python3
import os, sys, time, json
base = os.environ["FIXTURE"]
argv = sys.argv[1:]
with open(base + "/calls", "a") as log:
    log.write(json.dumps(argv) + "\n")
if argv[:2] == ["host", "read"] and len(argv) == 3:
    spec = argv[2]
    at = [i for i in range(len(spec) - 1) if spec[i:i + 2] == ":/"][0]
    host, path = spec[:at], spec[at + 1:]
    if os.path.exists(base + "/slow"):
        deadline = time.time() + 30
        while not os.path.exists(base + "/release") and time.time() < deadline:
            time.sleep(0.02)
    if os.path.exists(base + "/fail"):
        sys.stderr.write("\x1b]0;owned\x07refused: " + path + "\n")
        sys.exit(1)
    try:
        data = open(base + "/server" + path, "rb").read()
    except OSError as error:
        sys.stderr.write("eitri host read: " + str(error) + "\n")
        sys.exit(1)
    sys.stdout.buffer.write(data)
    sys.stdout.buffer.flush()
elif argv[:3] == ["host", "sync", "watch"]:
    # The plugin follows the project's sync as long as the buffer is in it; this fixture has nothing to report.
    for _ in sys.stdin:
        pass
elif argv[:2] == ["host", "lsp"] and "--" in argv:
    command = argv[argv.index("--") + 1:]
    os.execvp(command[0], command)
else:
    sys.stderr.write("unexpected: " + repr(argv) + "\n")
    sys.exit(2)
"#;

const FAKE_LSP: &str = r#"#!/usr/bin/env python3
import os, sys, json
base = os.environ["FIXTURE"]
stdin, stdout = sys.stdin.buffer, sys.stdout.buffer

def read():
    length = None
    while True:
        line = stdin.readline()
        if not line:
            return None
        line = line.strip()
        if not line:
            break
        name, _, value = line.partition(b":")
        if name.strip().lower() == b"content-length":
            length = int(value.strip())
    return json.loads(stdin.read(length))

def send(message):
    body = json.dumps(message).encode()
    stdout.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
    stdout.flush()

uri = open(base + "/location-uri").read().strip()
# Several servers can run side by side: each logs what it received under its own name, and only the one named in a
# `show` command asks the editor to show a document.
name = os.environ.get("LSP_NAME", "fake")
received = base + ("/lsp-received" if name == "fake" else "/lsp-received-" + name)
while True:
    message = read()
    if message is None:
        break
    with open(received, "a") as log:
        log.write(json.dumps(message) + "\n")
    method, ident = message.get("method"), message.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": ident, "result": {"capabilities": {"textDocumentSync": 1,
            "definitionProvider": not os.environ.get("NO_DEFINITION"),
            "executeCommandProvider": {"commands": ["show"]}}}})
    elif method == "workspace/executeCommand":
        send({"jsonrpc": "2.0", "id": ident, "result": None})
        arguments = message["params"].get("arguments") or []
        if arguments and arguments[0] == name:
            shown = globals().get("shown", 0) + 1
            globals()["shown"] = shown
            send({"jsonrpc": "2.0", "id": 9000 + shown, "method": "window/showDocument", "params": {
                "uri": arguments[1], "takeFocus": True,
                "selection": {"start": {"line": 2, "character": 11}, "end": {"line": 2, "character": 17}}}})
    elif method == "textDocument/definition":
        send({"jsonrpc": "2.0", "id": ident, "result": [{"uri": uri, "range": {
            "start": {"line": 2, "character": 11}, "end": {"line": 2, "character": 17}}}]})
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": ident, "result": None})
    elif method == "exit":
        break
    elif ident is not None:
        send({"jsonrpc": "2.0", "id": ident, "result": None})
"#;

fn check(body: &str) {
    check_startup(Startup::Plugin, body)
}

fn check_startup(startup: Startup, body: &str) {
    for nvim in versions() {
        let scratch = Scratch(std::env::temp_dir().join(format!("eitri-buffers-{}", uuid::Uuid::new_v4())));
        let base = &scratch.0;
        let root = base.join("project");
        let hosts = base.join("state/eitri/hosts");
        agent::private_fs::create_private_dir_all(&hosts, base).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), b"fn main() {\n    dep();\n}\n").unwrap();
        std::fs::create_dir_all(base.join("server/opt/dep/src")).unwrap();
        std::fs::write(base.join("server").join(&DEPENDENCY_PATH[1..]), DEPENDENCY).unwrap();
        std::fs::write(
            base.join("location-uri"),
            format!("eitri-remote://example-host{DEPENDENCY_PATH}"),
        )
        .unwrap();
        let record = eitri_core::remote_project::RemoteProject {
            version: 1,
            host: "example-host".into(),
            host_key_sha256: "SHA256:example".into(),
            root: root.clone(),
            helper_version: "v".into(),
            sync: None,
            created_at: 0,
            queued_stops: vec![],
        };
        let store = eitri_core::remote_project::RemoteProjects::new(Some(base.join("state").as_os_str()), None);
        store.save(&record).unwrap();
        let program = base.join("eitri");
        script_fixture::write_executable_script(&program, FAKE_EITRI, 0o700).unwrap();
        let lsp = base.join("fake-lsp");
        script_fixture::write_executable_script(&lsp, FAKE_LSP, 0o700).unwrap();
        let plugin = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("nvim/eitri.nvim");
        let init = base.join("init.lua");
        let setup = match startup {
            Startup::Plugin => format!(
                "vim.opt.runtimepath:prepend({})",
                eitri_core::onboarding::lua_text::quote(plugin.to_str().unwrap())
            ),
            Startup::Embedded => String::new(),
        };
        std::fs::write(&init, setup).unwrap();
        let script = base.join("check.lua");
        std::fs::write(
            &script,
            format!(
                r#"
local ok, why = pcall(function()
local api = vim.api
local uv = vim.uv or vim.loop
local base = vim.env.FIXTURE
local root = vim.fn.getcwd()
local uri = "eitri-remote://example-host{DEPENDENCY_PATH}"
local notes = {{}}
vim.notify = function(message, level) table.insert(notes, {{ message, level }}) end
local function wait(f, what) assert(vim.wait(10000, f, 10), "timed out: " .. (what or "") .. vim.fn.execute("messages")) end
local function lines(buf) return api.nvim_buf_get_lines(buf or 0, 0, -1, false) end
local function touch(name) io.open(base .. "/" .. name, "w"):close() end
local function calls()
  local out = {{}}
  local f = io.open(base .. "/calls")
  if f then for line in f:lines() do table.insert(out, line) end f:close() end
  return out
end
local function received(name)
  local out = {{}}
  local f = io.open(base .. "/lsp-received" .. (name and ("-" .. name) or ""))
  -- A line the fake server is still writing is cut short: it is read on the next look.
  if f then for line in f:lines() do local ok, message = pcall(vim.json.decode, line) if ok then table.insert(out, message) end end f:close() end
  return out
end
local function landed() return vim.bo.modifiable == false and lines()[1] == "pub fn dep() {{}}" end
{body}
end)
if not ok then io.stderr:write(tostring(why), "\n"); vim.cmd("cquit 1") else vim.cmd("qa!") end
"#
            ),
        )
        .unwrap();
        let version = Command::new(&nvim)
            .arg("--version")
            .output()
            .map(|out| {
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_owned()
            })
            .unwrap_or_default();
        eprintln!("running with {version} ({})", nvim.display());
        let mut cmd = Command::new(&nvim);
        if matches!(startup, Startup::Embedded) {
            cmd.args([
                "--cmd",
                &eitri_core::remote_sync::nvim_loader(&root, program.to_str().unwrap()),
            ]);
        }
        cmd.args(["--headless", "-i", "NONE", "-u"])
            .arg(&init)
            .args(["-c", "lua dofile(vim.env.FIXTURE .. '/check.lua')"])
            .current_dir(&root)
            .env("FIXTURE", base)
            .env("EITRI_PROGRAM", &program)
            // The plugin starts `eitri` by name for the project's sync: the fixture's, first on the path.
            .env(
                "PATH",
                std::env::join_paths(
                    std::iter::once(base.clone()).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
                )
                .unwrap(),
            )
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env_remove("NVIM")
            .env_remove("NVIM_LISTEN_ADDRESS")
            .env_remove("VIMINIT")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("HOME", base);
        for (key, dir) in [
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_RUNTIME_DIR", "run"),
            ("TMPDIR", "tmp"),
        ] {
            agent::private_fs::create_private_dir_all(&base.join(dir), base).unwrap();
            cmd.env(key, base.join(dir));
        }
        let mut child = cmd.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() > deadline {
                let _ = child.kill();
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{version} ({nvim:?}): {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// The program every case below runs `eitri host ...` through: the fixture's, named by the plugin's own option.
const SET_PROGRAM: &str = r#"require("eitri.remote.buffers").setup({ cmd = vim.env.EITRI_PROGRAM })"#;

#[test]
#[ignore = "requires headless nvim"]
fn the_buffer_is_read_only_and_has_no_modeline() {
    check(&format!(
        r#"{SET_PROGRAM}
-- The program is read asynchronously: with the server slow, the buffer says so and the editor keeps running.
touch("slow")
vim.cmd.EitriRemote("open {DEPENDENCY_PATH}")
local buf = api.nvim_get_current_buf()
assert(api.nvim_buf_get_name(buf) == uri, api.nvim_buf_get_name(buf))
assert(lines()[1] == "reading from example-host…", vim.inspect(lines()))
local ticks = 0
local timer = uv.new_timer()
timer:start(0, 10, vim.schedule_wrap(function() ticks = ticks + 1 end))
wait(function() return ticks >= 5 end, "the event loop to keep running while the read waits")
timer:stop(); timer:close()
assert(lines()[1] == "reading from example-host…")
touch("release")
wait(landed, "the text to land")
assert(table.concat(lines(), "\n") == vim.trim(table.concat({{"pub fn dep() {{}}", "", "    pub fn second() {{}}", "// vim: set tw=77 :"}}, "\n")))
-- The options: a plain file buffer that cannot be changed, written or swapped, and whose modeline was not read.
assert(vim.bo.buftype == "", vim.bo.buftype)
assert(vim.bo.modifiable == false and vim.bo.readonly == true and vim.bo.modified == false)
assert(vim.bo.swapfile == false and vim.bo.modeline == false)
-- Modelines are applied after the BufRead autocommands: run them again, as a person's autocommand could.
vim.cmd("doautocmd BufRead")
assert(vim.bo.textwidth ~= 77, "the modeline was read")
-- The placeholder and the text are not in the undo history.
assert(vim.bo.undolevels == -1 and vim.fn.undotree().seq_last == 0, vim.inspect(vim.fn.undotree()))
assert(vim.fn.swapname(buf) == "" or not uv.fs_stat(vim.fn.swapname(buf)), "a swap file was made")
-- Detection is from the path alone.
assert(vim.bo.filetype == "rust", vim.bo.filetype)
assert(not pcall(api.nvim_buf_set_lines, buf, 0, 1, false, {{"x"}}), "an edit went through")
-- A write is refused with its reason, and the buffer is as it was.
notes = {{}}
pcall(vim.cmd, "write!")
assert(#notes >= 1 and notes[#notes][1] == "this file is on example-host and read-only here", vim.inspect(notes))
assert(vim.bo.modified == false and lines()[1] == "pub fn dep() {{}}")
-- The read went through `eitri host read <host>:<path>` once.
local reads = 0
for _, call in ipairs(calls()) do
  if vim.deep_equal(vim.json.decode(call), {{"host", "read", "example-host:{DEPENDENCY_PATH}"}}) then reads = reads + 1 end
end
assert(reads == 1, vim.inspect(calls()))"#
    ));
}

#[test]
#[ignore = "requires headless nvim"]
fn a_line_asked_for_while_the_text_is_coming_is_kept() {
    check(&format!(
        r#"{SET_PROGRAM}
touch("slow")
vim.cmd.EitriRemote("open {DEPENDENCY_PATH}")
local buf = api.nvim_get_current_buf()
-- What `nvim_scratch.lua` calls after its `:edit`: a line the buffer has not got yet.
require("eitri.remote.buffers").want(buf, 3)
touch("release")
wait(landed, "the text to land")
assert(api.nvim_win_get_cursor(0)[1] == 3, vim.inspect(api.nvim_win_get_cursor(0)))"#
    ));
}

#[test]
#[ignore = "requires headless nvim"]
fn a_refused_read_says_why_with_hidden_characters_escaped() {
    check(&format!(
        r#"{SET_PROGRAM}
touch("fail")
vim.cmd.EitriRemote("open {DEPENDENCY_PATH}")
local buf = api.nvim_get_current_buf()
wait(function() return lines()[1] == "could not read example-host:{DEPENDENCY_PATH}" end, "the failure")
local shown = table.concat(lines(), "\n") .. table.concat(vim.tbl_map(function(n) return n[1] end, notes), "\n")
assert(not shown:find("[\1-\9\11-\31\127]"), "a control character is shown: " .. vim.inspect(shown))
assert(shown:find("\\u{{1b}}", 1, true) and shown:find("\\u{{7}}", 1, true), vim.inspect(shown))
assert(shown:find("refused: {DEPENDENCY_PATH}", 1, true), vim.inspect(shown))
assert(vim.bo.modifiable == false and vim.bo.filetype == "")
assert(notes[#notes][2] == vim.log.levels.ERROR)"#
    ));
}

#[test]
#[ignore = "requires headless nvim"]
fn open_needs_a_remote_project_and_an_absolute_path() {
    check(&format!(
        r#"{SET_PROGRAM}
vim.cmd.EitriRemote("open relative/path.rs")
assert(notes[#notes][1]:find("needs an absolute path on example-host", 1, true), vim.inspect(notes))
assert(api.nvim_buf_get_name(0):find("eitri-remote", 1, true) == nil)
vim.cmd("cd /")
vim.cmd.enew()
vim.cmd.EitriRemote("open {DEPENDENCY_PATH}")
assert(notes[#notes][1] == "this buffer is outside a recorded remote project", vim.inspect(notes))
local notes_before = #notes
local refused, why = pcall(vim.cmd.EitriRemote, "nothing")
assert(not refused and tostring(why):find("unknown subcommand nothing; expected one of: makeprg, open, term", 1, true), tostring(why))
assert(#notes == notes_before, "an unknown subcommand is said once, as the error: " .. vim.inspect(notes))
for _, call in ipairs(calls()) do assert(not call:find('"read"', 1, true), "a refused open still read: " .. call) end
-- The name a host and a path make, and what the plugin reads back from it.
local buffers = require("eitri.remote.buffers")
local name = buffers.uri("user@example-host:2222", "/opt/my dir/a#b.rs")
assert(name == "eitri-remote://user%40example-host%3A2222/opt/my%20dir/a%23b.rs", name)
local host, path = buffers.parse(name)
assert(host == "user@example-host:2222" and path == "/opt/my dir/a#b.rs", host .. path)
assert(buffers.parse("eitri-remote://host") == nil and buffers.parse("eitri-remote:///x") == nil)
assert(buffers.parse("eitri-remote://h/a%00b") == nil)"#
    ));
}

#[test]
#[ignore = "requires headless nvim"]
fn gd_into_a_dependency_opens_a_read_only_buffer_with_the_client_attached() {
    check(&format!(
        r#"{SET_PROGRAM}
vim.cmd.edit("src/main.rs")
local source = api.nvim_get_current_buf()
local client_id = vim.lsp.start({{
  name = "fake",
  cmd = {{ vim.env.EITRI_PROGRAM, "host", "lsp", "--root", root, "--", base .. "/fake-lsp" }},
  root_dir = root,
}})
assert(client_id, "the client did not start")
-- A second bridge of the same host for other files (a TOML server): it is not the one that produced the location.
local toml_id = vim.lsp.start({{
  name = "toml",
  cmd = {{ vim.env.EITRI_PROGRAM, "host", "lsp", "--root", root, "--", base .. "/fake-lsp" }},
  cmd_env = {{ LSP_NAME = "toml", NO_DEFINITION = "1" }},
  root_dir = root,
  filetypes = {{ "toml" }},
}})
assert(toml_id and toml_id ~= client_id, "the second client did not start")
wait(function()
  for _, message in ipairs(received()) do if message.method == "initialized" then return true end end
end, "the client to initialize")
wait(function()
  for _, message in ipairs(received("toml")) do if message.method == "initialized" then return true end end
end, "the second client to initialize")
api.nvim_win_set_cursor(0, {{ 2, 4 }})
vim.lsp.buf.definition()
wait(function() return api.nvim_buf_get_name(0) == uri end, "the jump")
local buf = api.nvim_get_current_buf()
assert(buf ~= source)
-- The text lands later; the jump asked for line 3, column 11, and gets it when the text is there.
wait(landed, "the text to land")
assert(vim.bo.modifiable == false and vim.bo.readonly == true and vim.bo.modeline == false)
assert(vim.bo.filetype == "rust", vim.bo.filetype)
local cursor = api.nvim_win_get_cursor(0)
assert(cursor[1] == 3 and cursor[2] == 11, vim.inspect(cursor))
-- The client that produced the location is attached to it, with the text and the language the server was told.
local attached = vim.tbl_map(function(client) return client.id end, vim.lsp.get_clients({{ bufnr = buf }}))
assert(vim.tbl_contains(attached, client_id), vim.inspect(attached))
assert(not vim.tbl_contains(attached, toml_id), "the TOML server was attached to a Rust file: " .. vim.inspect(attached))
local opened
wait(function()
  for _, message in ipairs(received()) do
    if message.method == "textDocument/didOpen" and message.params.textDocument.uri == uri then opened = message.params.textDocument return true end
  end
end, "the server to be told of the buffer")
assert(opened.languageId == "rust" and opened.text == table.concat(lines(), "\n") .. "\n", vim.inspect(opened))
-- A second jump into the same buffer goes straight to its position.
api.nvim_win_set_cursor(0, {{ 1, 0 }})
vim.cmd.edit("src/main.rs")
api.nvim_win_set_cursor(0, {{ 2, 4 }})
vim.lsp.buf.definition()
wait(function() return api.nvim_buf_get_name(0) == uri end, "the second jump")
assert(api.nvim_win_get_cursor(0)[1] == 3, vim.inspect(api.nvim_win_get_cursor(0)))
local reads = 0
for _, call in ipairs(calls()) do if call:find('"read"', 1, true) then reads = reads + 1 end end
assert(reads == 1, "the second jump read the file again: " .. vim.inspect(calls()))"#
    ));
}

/// Starts the bridge client `name` (a fake `eitri host lsp` of example-host's project) on the current buffer.
const START_CLIENT: &str = r#"
local function start_client(name, filetypes)
  local id = vim.lsp.start({
    name = name,
    cmd = { vim.env.EITRI_PROGRAM, "host", "lsp", "--root", root, "--", base .. "/fake-lsp" },
    cmd_env = { LSP_NAME = name },
    root_dir = root,
    filetypes = filetypes,
  }, { reuse_client = function() return false end })
  assert(id, name .. " did not start")
  wait(function()
    for _, message in ipairs(received(name)) do if message.method == "initialized" then return true end end
  end, name .. " to initialize")
  return id
end
local function attached_to(buf)
  return vim.tbl_map(function(client) return client.id end, vim.lsp.get_clients({ bufnr = buf }))
end
local function opened_by(name, wanted)
  for _, message in ipairs(received(name)) do
    if message.method == "textDocument/didOpen" and message.params.textDocument.uri == wanted then return true end
  end
  return false
end"#;

#[test]
#[ignore = "requires headless nvim"]
fn a_location_on_another_host_is_read_there_but_never_shown_to_this_server() {
    let other = format!("eitri-remote://other-host{DEPENDENCY_PATH}");
    check(&format!(
        r#"{SET_PROGRAM}
local f = io.open(base .. "/location-uri", "w") f:write("{other}") f:close()
vim.cmd.edit("src/main.rs")
local source = api.nvim_get_current_buf()
local client_id = vim.lsp.start({{
  name = "fake",
  cmd = {{ vim.env.EITRI_PROGRAM, "host", "lsp", "--root", root, "--", base .. "/fake-lsp" }},
  root_dir = root,
}})
wait(function()
  for _, message in ipairs(received()) do if message.method == "initialized" then return true end end
end, "the client to initialize")
api.nvim_win_set_cursor(0, {{ 2, 4 }})
vim.lsp.buf.definition()
wait(function() return api.nvim_buf_get_name(0) == "{other}" end, "the jump")
local buf = api.nvim_get_current_buf()
assert(buf ~= source)
wait(landed, "the text to land")
-- example-host's server is never attached to other-host's file, and never sees its text.
local attached = vim.tbl_map(function(client) return client.id end, vim.lsp.get_clients({{ bufnr = buf }}))
assert(not vim.tbl_contains(attached, client_id), "another host's client was attached: " .. vim.inspect(attached))
vim.wait(300, function() return false end, 10)
for _, message in ipairs(received()) do
  assert(not vim.inspect(message):find("other-host", 1, true), "the server saw another host's file: " .. vim.inspect(message))
end"#
    ));
}

#[test]
#[ignore = "requires headless nvim"]
fn show_document_attaches_only_the_client_that_asked_and_refuses_another_host() {
    let other = format!("eitri-remote://other-host{DEPENDENCY_PATH}");
    check(&format!(
        r#"{SET_PROGRAM}{START_CLIENT}
vim.cmd.edit("src/main.rs")
local source = api.nvim_get_current_buf()
local one, two = start_client("one"), start_client("two")
-- A server asks the editor to show another host's file: refused, nothing read, the buffer it was in stays.
vim.lsp.buf_request(0, "workspace/executeCommand", {{ command = "show", arguments = {{ "two", "{other}" }} }}, function() end)
wait(function()
  for _, note in ipairs(notes) do if note[1]:find("not its host", 1, true) then return true end end
end, "the refusal")
assert(api.nvim_get_current_buf() == source, api.nvim_buf_get_name(0))
for _, call in ipairs(calls()) do assert(not call:find("other-host", 1, true), "another host was read: " .. call) end
-- Its own host's file: shown at the selection once read, and only the client that asked is attached.
vim.lsp.buf_request(0, "workspace/executeCommand", {{ command = "show", arguments = {{ "two", uri }} }}, function() end)
wait(function() return api.nvim_buf_get_name(0) == uri end, "the jump")
local buf = api.nvim_get_current_buf()
wait(landed, "the text to land")
local cursor = api.nvim_win_get_cursor(0)
assert(cursor[1] == 3 and cursor[2] == 11, vim.inspect(cursor))
local attached = vim.tbl_map(function(client) return client.id end, vim.lsp.get_clients({{ bufnr = buf }}))
assert(vim.tbl_contains(attached, two) and not vim.tbl_contains(attached, one), vim.inspect({{ one, two, attached }}))
wait(function() return opened_by("two", uri) end, "the asking server to be told of the buffer")
assert(not opened_by("one", uri), "the other server was told of the buffer")"#
    ));
}

#[test]
#[ignore = "requires headless nvim"]
fn a_second_jump_into_a_loading_buffer_adds_its_clients() {
    check(&format!(
        r#"{SET_PROGRAM}{START_CLIENT}
vim.fn.writefile({{ "fn other() {{}}" }}, root .. "/src/other.rs")
vim.cmd.edit("src/main.rs")
local one = start_client("one")
vim.cmd.edit("src/other.rs")
local two = start_client("two")
touch("slow")
local location = {{ uri = uri, range = {{ start = {{ line = 2, character = 11 }}, ["end"] = {{ line = 2, character = 17 }} }} }}
-- Two jumps into the buffer while it is still loading, from files served by different bridges of the host.
vim.cmd.buffer(vim.fn.bufnr("main.rs"))
vim.lsp.util.show_document(location, "utf-16", {{ focus = true }})
wait(function() return api.nvim_buf_get_name(0) == uri end, "the first jump")
local remote = api.nvim_get_current_buf()
vim.cmd.buffer(vim.fn.bufnr("other.rs"))
vim.lsp.util.show_document(location, "utf-16", {{ focus = true }})
touch("release")
wait(landed, "the text to land")
local attached = vim.tbl_map(function(client) return client.id end, vim.lsp.get_clients({{ bufnr = remote }}))
table.sort(attached)
assert(vim.deep_equal(attached, {{ math.min(one, two), math.max(one, two) }}), vim.inspect({{ one, two, attached }}))"#
    ));
}

#[test]
#[ignore = "requires headless nvim"]
fn open_by_hand_attaches_the_bridge_of_the_buffer_it_leaves_and_works_inside_one() {
    check(&format!(
        r#"{SET_PROGRAM}{START_CLIENT}
vim.fn.writefile({{ "fn other() {{}}" }}, base .. "/server/opt/dep/src/other.rs")
vim.cmd.edit("src/main.rs")
local one = start_client("one")
-- A bridge of the same host for other files: it serves no Rust file.
local toml = start_client("toml", {{ "toml" }})
vim.cmd.EitriRemote("open {DEPENDENCY_PATH}")
wait(landed, "the text to land")
local buf = api.nvim_get_current_buf()
local attached = attached_to(buf)
assert(vim.tbl_contains(attached, one) and not vim.tbl_contains(attached, toml), vim.inspect({{ one, toml, attached }}))
wait(function() return opened_by("one", uri) end, "the server to be told of the buffer")
assert(not opened_by("toml", uri), "the TOML server was told of a Rust file")
-- Typed inside that buffer, `open` reads from its host, and the server of the buffer it leaves comes along.
local other = "eitri-remote://example-host/opt/dep/src/other.rs"
vim.cmd.EitriRemote("open /opt/dep/src/other.rs")
wait(function() return api.nvim_buf_get_name(0) == other end, "the open from inside an eitri-remote buffer" .. vim.inspect(notes))
wait(function() return vim.bo.modifiable == false and lines()[1] == "fn other() {{}}" end, "the second text to land")
assert(vim.tbl_contains(attached_to(0), one), vim.inspect(attached_to(0)))
wait(function() return opened_by("one", other) end, "the server to be told of the second buffer")"#
    ));
}

#[test]
#[ignore = "requires headless nvim"]
fn the_panels_open_from_a_buffer_without_servers_takes_a_running_bridge_for_the_language() {
    check(&format!(
        r#"{SET_PROGRAM}{START_CLIENT}
vim.cmd.edit("src/main.rs")
local rust = start_client("rust", {{ "rust" }})
-- A client that names no filetypes would serve any file: it is not taken from another buffer.
local generic = start_client("generic")
vim.cmd.enew()
assert(#attached_to(0) == 0)
-- What `nvim_scratch.lua` runs for the panel's open-path: the module is told, then the `:edit`.
local buffers = require("eitri.remote.buffers")
buffers.opening(uri)
api.nvim_cmd({{ cmd = "edit", args = {{ uri }}, magic = {{ file = false }} }}, {{}})
wait(landed, "the text to land")
local attached = attached_to(0)
assert(vim.tbl_contains(attached, rust) and not vim.tbl_contains(attached, generic), vim.inspect({{ rust, generic, attached }}))
wait(function() return opened_by("rust", uri) end, "the server to be told of the buffer")"#
    ));
}

#[test]
#[ignore = "requires headless nvim"]
fn an_open_from_a_buffer_without_servers_takes_the_bridge_whose_root_holds_the_file_and_none_when_ambiguous() {
    check(&format!(
        r#"{SET_PROGRAM}{START_CLIENT}
local function start_at(name, at)
  local id = vim.lsp.start({{
    name = name,
    cmd = {{ vim.env.EITRI_PROGRAM, "host", "lsp", "--root", at, "--", base .. "/fake-lsp" }},
    cmd_env = {{ LSP_NAME = name }},
    root_dir = at,
    filetypes = {{ "rust" }},
  }}, {{ reuse_client = function() return false end }})
  assert(id, name .. " did not start")
  wait(function()
    for _, message in ipairs(received(name)) do if message.method == "initialized" then return true end end
  end, name .. " to initialize")
  return id
end
vim.fn.mkdir(root .. "/other", "p")
local generated = root .. "/other/target/gen.rs"
vim.fn.mkdir(base .. "/server" .. root .. "/other/target", "p")
vim.fn.writefile({{ "pub fn generated() {{}}" }}, base .. "/server" .. generated)
vim.cmd.edit("src/main.rs")
local whole = start_at("whole", root)
local other = start_at("other", root .. "/other")
local buffers = require("eitri.remote.buffers")
local function open(path, first_line)
  vim.cmd.enew()
  assert(#attached_to(0) == 0)
  local name = "eitri-remote://example-host" .. path
  buffers.opening(name)
  api.nvim_cmd({{ cmd = "edit", args = {{ name }}, magic = {{ file = false }} }}, {{}})
  wait(function() return vim.bo.modifiable == false and lines()[1] == first_line end, "the text of " .. path)
end
-- A dependency outside both roots: two bridges could take it, so neither does.
open("{DEPENDENCY_PATH}", "pub fn dep() {{}}")
vim.wait(300)
assert(#attached_to(0) == 0, vim.inspect(attached_to(0)))
-- A file both roots hold (excluded from sync, so it is read from the server): the deeper root's bridge alone.
open(generated, "pub fn generated() {{}}")
wait(function() return #attached_to(0) > 0 end, "a client on the generated file")
vim.wait(300)
local attached = attached_to(0)
assert(vim.deep_equal(attached, {{ other }}), vim.inspect({{ whole = whole, other = other, attached = attached }}))"#
    ));
}

#[test]
#[ignore = "requires headless nvim"]
fn hidden_characters_are_escaped_as_the_rust_side_escapes_them() {
    // The code points the plugin must write out: the Rust side's (`agent_bridge::is_control_like`, which
    // `remote_sync::terminal_text` escapes), the zero-width spaces and joiners and the byte-order mark.
    let mut escaped = Vec::new();
    for value in (1u32..0x3000).chain([0xFEFE, 0xFEFF, 0xFFFD]) {
        let Some(c) = char::from_u32(value) else { continue };
        if eitri_core::agent_bridge::is_control_like(c) || matches!(value, 0x200B..=0x200D | 0xFEFF) {
            escaped.push(value.to_string());
        }
    }
    let escaped = escaped.join(", ");
    check(&format!(
        r#"local text = require("eitri.remote.text")
local escaped = {{}}
for _, value in ipairs({{ {escaped} }}) do escaped[value] = true end
local values = vim.fn.range(1, 0x2FFF)
vim.list_extend(values, {{ 0xFEFE, 0xFEFF, 0xFFFD }})
for _, value in ipairs(values) do
  local c = vim.fn.nr2char(value)
  local shown = text.escape("a" .. c .. "b")
  if escaped[value] then
    assert(shown == ("a\\u{{%x}}b"):format(value), ("U+%04X is shown as %s"):format(value, vim.inspect(shown)))
  else
    assert(shown == "a" .. c .. "b", ("U+%04X was escaped: %s"):format(value, vim.inspect(shown)))
  end
end
assert(text.escape("\0") == "\\u{{0}}", text.escape("\0"))
assert(require("eitri.remote.buffers").escape == text.escape, "the buffers module has an escaper of its own")"#
    ));
}

#[test]
#[ignore = "requires headless nvim"]
fn a_recorded_projects_own_host_wins_over_the_option() {
    check(&format!(
        r#"{SET_PROGRAM}
local remote = require("eitri.remote")
remote.setup({{ host = "elsewhere", cmd = vim.env.EITRI_PROGRAM }})
local project = remote.project_of(root .. "/src/main.rs")
assert(project and project.root == root and project.host == "example-host", vim.inspect(project))
assert(remote.project_of("/nowhere/else") == nil)"#
    ));
}

#[test]
#[ignore = "requires headless nvim"]
fn at_most_four_reads_run_at_once() {
    check(&format!(
        r#"{SET_PROGRAM}
touch("slow")
local buffers = require("eitri.remote.buffers")
local bufs = {{}}
for i = 1, 6 do
  local b = vim.fn.bufadd(buffers.uri("example-host", "/opt/dep/src/lib" .. i .. ".rs"))
  vim.fn.bufload(b)
  bufs[i] = b
end
local function reads()
  local n = 0
  for _, call in ipairs(calls()) do if call:find('"read"', 1, true) then n = n + 1 end end
  return n
end
wait(function() return reads() == 4 end, "four reads")
vim.wait(300, function() return reads() > 4 end, 10)
assert(reads() == 4, "more than four reads ran at once: " .. reads())
-- A buffer wiped while it waits gives its turn away.
api.nvim_buf_delete(bufs[5], {{ force = true }})
touch("release")
wait(function()
  for i = 1, 6 do
    if i ~= 5 and api.nvim_buf_get_lines(bufs[i], 0, 1, false)[1] == "reading from example-host…" then return false end
  end
  return true
end, "every read to end")
assert(reads() == 5, "reads: " .. reads())"#
    ));
}

#[test]
#[ignore = "requires headless nvim"]
fn the_module_loads_in_eitris_window_through_the_preload() {
    check_startup(
        Startup::Embedded,
        &format!(
            r#"
assert(package.loaded["eitri.remote.buffers"], "the buffers module was not preloaded")
assert(vim.fn.exists(":EitriRemote") == 2)
vim.cmd.EitriRemote("open {DEPENDENCY_PATH}")
wait(landed, "the text to land")
assert(api.nvim_buf_get_name(0) == uri and vim.bo.readonly == true)"#
        ),
    );
}

mod script_fixture {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/executable.rs"));
}
