//! `eitri.nvim`'s wrap of `vim.lsp.start` in a real Neovim: a language server whose root is inside a recorded remote
//! project is started through `eitri host lsp` (a fake `eitri` that records its argv and runs the scripted server,
//! `core/tests/fixtures/lsp/scripted-server`), and anything else is left alone. `:checkhealth eitri` names what runs
//! where, and a plain Neovim (no window, no Eitri environment variable) gets the same wrap from the plugin.
//!
//! Runs once per Neovim: `EITRI_TEST_NVIM` (one binary) when set, else `nvim` on `PATH` and each of
//! `EITRI_TEST_NVIM_0_10`, `_0_11`, `_0_12` that is set (or cached under `CARGO_TARGET_DIR/nvim-releases/<version>`).
#![cfg(target_os = "linux")]

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
    /// Eitri's window: the modules injected with `package.preload` and no plugin directory, with the language-server
    /// options the window would pass (grace 120 s, `pinned` kept local).
    Embedded,
}

const DEPENDENCY_PATH: &str = "/opt/dep/src/lib.rs";

/// What the plugin starts by name: records its argv; `host lsp ... -- <cmd>` runs the command, as the bridge would,
/// `host read` serves a file of the fake server, and `host status` ends with the switch's line as the real one does
/// (`off` when `remote_dev_off` exists in the fixture directory, else `on`).
const FAKE_EITRI: &str = r#"#!/usr/bin/env python3
import os, sys, json
base = os.environ["FIXTURE"]
argv = sys.argv[1:]
with open(base + "/calls", "a") as log:
    log.write(json.dumps(argv) + "\n")
if argv[:2] == ["host", "read"] and len(argv) == 3:
    spec = argv[2]
    at = [i for i in range(len(spec) - 1) if spec[i:i + 2] == ":/"][0]
    try:
        data = open(base + "/server" + spec[at + 1:], "rb").read()
    except OSError as error:
        sys.stderr.write("eitri host read: " + str(error) + "\n")
        sys.exit(1)
    sys.stdout.buffer.write(data)
    sys.stdout.buffer.flush()
elif argv == ["--version"]:
    print("eitri 9.9.9-test")
elif argv == ["page", "--list"]:
    print("markdown")
elif argv == ["host", "status"]:
    print("example-host:/srv/project")
    print("remote development: " + ("off" if os.path.exists(base + "/remote_dev_off") else "on"))
elif argv[:3] == ["host", "sync", "watch"]:
    for _ in sys.stdin:
        pass
elif argv[:2] == ["host", "lsp"] and "--" in argv:
    command = argv[argv.index("--") + 1:]
    os.execvp(command[0], command)
else:
    sys.stderr.write("unexpected: " + repr(argv) + "\n")
    sys.exit(2)
"#;

/// The script of the scripted server: it answers `initialize` and then echoes.
const SCRIPT: &str = r#"{"recv": "initialize"}
{"send": {"jsonrpc": "2.0", "id": 1, "result": {"capabilities": {"textDocumentSync": 1}}}}
"#;

fn check(body: &str) -> Vec<String> {
    check_startup(Startup::Plugin, body)
}

/// Runs `body` in each Neovim; the stderr of each run.
fn check_startup(startup: Startup, body: &str) -> Vec<String> {
    let mut outputs = Vec::new();
    for nvim in versions() {
        let scratch = Scratch(std::env::temp_dir().join(format!("eitri-lsp-wrap-{}", uuid::Uuid::new_v4())));
        let base = &scratch.0;
        let root = base.join("project");
        let other_root = base.join("other-project");
        let hosts = base.join("state/eitri/hosts");
        agent::private_fs::create_private_dir_all(&hosts, base).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(&other_root).unwrap();
        std::fs::create_dir_all(base.join("elsewhere")).unwrap();
        std::fs::write(root.join("src/main.rs"), b"fn main() {\n    dep();\n}\n").unwrap();
        std::fs::write(root.join("Cargo.toml"), b"[package]\nname = \"p\"\n").unwrap();
        std::fs::write(base.join("elsewhere/notes.rs"), b"fn n() {}\n").unwrap();
        // A marker above the recorded root: a root found by it is not the project's.
        std::fs::write(base.join("above.marker"), b"").unwrap();
        std::fs::create_dir_all(base.join("server/opt/dep/src")).unwrap();
        std::fs::write(base.join("server").join(&DEPENDENCY_PATH[1..]), "pub fn dep() {}\n").unwrap();
        std::fs::write(base.join("script.jsonl"), SCRIPT).unwrap();
        let store = eitri_core::remote_project::RemoteProjects::new(Some(base.join("state").as_os_str()), None);
        for (host, dir) in [("example-host", &root), ("other-host", &other_root)] {
            store
                .save(&eitri_core::remote_project::RemoteProject {
                    version: 1,
                    host: host.into(),
                    host_key_sha256: "SHA256:example".into(),
                    root: dir.clone(),
                    helper_version: "v".into(),
                    sync: None,
                    created_at: 0,
                    queued_stops: vec![],
                })
                .unwrap();
        }
        let program = base.join("eitri");
        script_fixture::write_executable_script(&program, FAKE_EITRI, 0o700).unwrap();
        script_fixture::write_executable_script(&base.join("mutagen"), "#!/bin/sh\necho 0.18.1\n", 0o700).unwrap();
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
        let scripted = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lsp/scripted-server");
        let script = base.join("check.lua");
        std::fs::write(
            &script,
            format!(
                r#"
local ok, why = pcall(function()
local api = vim.api
local base = vim.env.FIXTURE
local root = vim.fn.getcwd()
local program = vim.env.EITRI_PROGRAM
local scripted = {scripted}
local notes = {{}}
vim.notify = function(message, level) table.insert(notes, {{ message, level }}) end
local function wait(f, what) assert(vim.wait(10000, f, 10), "timed out: " .. (what or "") .. vim.fn.execute("messages")) end
local function json_lines(path)
  local out = {{}}
  local f = io.open(path)
  if f then
    -- A line the fake is still writing is cut short: it is read on the next look.
    for line in f:lines() do local ok, value = pcall(vim.json.decode, line) if ok then table.insert(out, value) end end
    f:close()
  end
  return out
end
local function calls() return json_lines(base .. "/calls") end
local function bridge_calls()
  local out = {{}}
  for _, argv in ipairs(calls()) do if argv[1] == "host" and argv[2] == "lsp" then table.insert(out, argv) end end
  return out
end
-- What the scripted server of this name was sent, by method.
local function received(name)
  local out = {{}}
  for _, message in ipairs(json_lines(base .. "/received-" .. name)) do if message.method then table.insert(out, message.method) end end
  return out
end
local function got(name, method)
  return vim.tbl_contains(received(name), method)
end
-- A server config for the scripted server named `name`; `root_dir = false` leaves the root out.
local function server(name, extra)
  local config = vim.tbl_extend("force", {{ name = name, cmd = {{ scripted, base .. "/script.jsonl", base .. "/received-" .. name }}, root_dir = root }}, extra or {{}})
  if config.root_dir == false then config.root_dir = nil end
  return config
end
local lsp = require("eitri.remote.lsp")
{body}
end)
if not ok then io.stderr:write(tostring(why), "\n"); vim.cmd("cquit 1") else vim.cmd("qa!") end
"#,
                scripted = eitri_core::onboarding::lua_text::quote(scripted.to_str().unwrap()),
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
            let options = eitri_core::remote_sync::LspOptions {
                grace: 120,
                local_servers: vec!["pinned".into()],
            };
            cmd.args([
                "--cmd",
                &eitri_core::remote_sync::nvim_loader_with(&root, program.to_str().unwrap(), &options),
            ]);
        }
        cmd.args(["--headless", "-i", "NONE", "-u"])
            .arg(&init)
            .args(["-c", "lua dofile(vim.env.FIXTURE .. '/check.lua')"])
            .current_dir(&root)
            .env("FIXTURE", base)
            .env("EITRI_PROGRAM", &program)
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
        // No other variable of Eitri's reaches the editor either: a window's marks must not make a plain run pass.
        for (key, _) in std::env::vars_os() {
            if key.to_str().is_some_and(|key| key.starts_with("EITRI_")) {
                cmd.env_remove(key);
            }
        }
        cmd.env("EITRI_PROGRAM", &program);
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
        for line in String::from_utf8_lossy(&out.stderr)
            .lines()
            .filter(|l| l.starts_with("enable saw:"))
        {
            eprintln!("{version}: {line}");
        }
        assert!(
            out.status.success(),
            "{version} ({nvim:?}): {}",
            String::from_utf8_lossy(&out.stderr)
        );
        outputs.push(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    outputs
}

/// Sets the program the wrap runs `eitri host lsp` through, and the options a plain Neovim passes.
const SETUP: &str =
    r#"require("eitri.remote").setup({ cmd = program, lsp = { grace = 300, local_servers = { "pinned" } } })"#;

#[test]
#[ignore = "requires headless nvim"]
fn wrap_rewrites_cmd_for_a_root_inside_a_remote_project() {
    check(&format!(
        r#"{SETUP}
local config = {{ name = "ra", cmd = {{ "rust-analyzer", "--flag" }}, root_dir = root .. "/src", cmd_env = {{ A = "1" }},
  cmd_cwd = "/x", settings = {{ a = 1 }} }}
local wrapped = lsp.wrap(config)
assert(vim.deep_equal(wrapped.cmd, {{ program, "host", "lsp", "--root", root, "--grace", "300", "--", "rust-analyzer", "--flag" }}), vim.inspect(wrapped.cmd))
assert(wrapped.cmd_env == nil and wrapped.cmd_cwd == nil, "the laptop's environment and directory are dropped")
assert(wrapped.settings.a == 1 and wrapped.root_dir == root .. "/src" and wrapped.name == "ra")
assert(config.cmd[1] == "rust-analyzer" and config.cmd_env.A == "1" and #config.cmd == 2, "the config is not changed")
-- No root_dir: the buffer's path decides (nvim 0.11's `vim.lsp.enable` configs reach `vim.lsp.start` so).
vim.cmd.edit("src/main.rs")
local from_buffer = lsp.wrap({{ name = "ra", cmd = {{ "rust-analyzer" }} }})
assert(from_buffer.cmd[5] == root, vim.inspect(from_buffer.cmd))
-- The grace is clamped as the server does, and a bad one is the default.
require("eitri.remote").setup({{ cmd = program, lsp = {{ grace = 5 }} }})
assert(lsp.wrap(config).cmd[7] == "60")
require("eitri.remote").setup({{ cmd = program, lsp = {{ grace = 999999 }} }})
assert(lsp.wrap(config).cmd[7] == "86400")
require("eitri.remote").setup({{ cmd = program, lsp = {{ grace = "soon" }} }})
assert(lsp.wrap(config).cmd[7] == "1800")
require("eitri.remote").setup({{ cmd = program }})
assert(lsp.wrap(config).cmd[7] == "1800")
-- The one lookup: root_of is the sync's and the exec wrappers' recorded()
assert(require("eitri.remote").root_of(root .. "/src/main.rs") == root and require("eitri.remote").root_of("/nowhere") == nil)"#
    ));
}

#[test]
#[ignore = "requires headless nvim"]
fn wrap_leaves_other_roots_alone() {
    check(&format!(
        r#"{SETUP}
local config = {{ name = "ra", cmd = {{ "rust-analyzer" }}, root_dir = base .. "/elsewhere" }}
assert(rawequal(lsp.wrap(config), config), "a root outside every recorded project is left alone")
vim.cmd.edit(base .. "/elsewhere/notes.rs")
assert(vim.fn.getcwd() == root)
local no_root = {{ name = "ra", cmd = {{ "rust-analyzer" }} }}
assert(rawequal(lsp.wrap(no_root), no_root), "no root_dir and a buffer outside every project")
-- A string root_dir outside wins over a buffer inside.
vim.cmd.edit(root .. "/src/main.rs")
assert(rawequal(lsp.wrap(config), config))
local nameless = {{ name = "ra", cmd = {{ "rust-analyzer" }} }}
vim.cmd.enew()
assert(rawequal(lsp.wrap(nameless), nameless), "a buffer with no name has no path to decide by")
assert(rawequal(lsp.wrap(nil), nil) and lsp.wrap("x") == "x")
-- Told to keep everything local: a session switch and a buffer one.
local inside = {{ name = "ra", cmd = {{ "rust-analyzer" }}, root_dir = root }}
vim.g.eitri_remote_lsp = false
assert(rawequal(lsp.wrap(inside), inside))
vim.g.eitri_remote_lsp = nil
assert(not rawequal(lsp.wrap(inside), inside))
vim.b.eitri_remote_lsp = false
assert(rawequal(lsp.wrap(inside), inside))"#
    ));
}

#[test]
#[ignore = "requires headless nvim"]
fn a_pinned_server_stays_local() {
    check(&format!(
        r#"{SETUP}
local pinned = {{ name = "pinned", cmd = {{ "lua-language-server" }}, root_dir = root }}
assert(rawequal(lsp.wrap(pinned), pinned), "a server named in local_servers stays local")
assert(lsp.pinned.pinned == true)
local other = {{ name = "other", cmd = {{ "lua-language-server" }}, root_dir = root }}
assert(not rawequal(lsp.wrap(other), other))
local unnamed = {{ cmd = {{ "lua-language-server" }}, root_dir = root }}
assert(not rawequal(lsp.wrap(unnamed), unnamed), "a server with no name cannot be pinned")"#
    ));
}

#[test]
#[ignore = "requires headless nvim"]
fn a_function_cmd_stays_local_and_is_reported() {
    check(&format!(
        r#"{SETUP}
local by_function = {{ name = "dynamic", cmd = function() error("never called here") end, root_dir = root }}
assert(rawequal(lsp.wrap(by_function), by_function), "a function cmd builds its own transport: it stays local")
assert(lsp.function_cmds.dynamic == root, vim.inspect(lsp.function_cmds))
-- Only a server that would have run remotely is reported.
local elsewhere = {{ name = "dynamic-local", cmd = function() end, root_dir = base .. "/elsewhere" }}
lsp.wrap(elsewhere)
assert(lsp.function_cmds["dynamic-local"] == nil)"#
    ));
}

#[test]
#[ignore = "requires headless nvim"]
fn wrap_is_idempotent() {
    check(&format!(
        r#"{SETUP}
local once = lsp.wrap({{ name = "ra", cmd = {{ "rust-analyzer" }}, root_dir = root }})
local twice = lsp.wrap(once)
assert(rawequal(once, twice), "a wrapped config is returned as it is")
assert(#twice.cmd == 9 and twice.cmd[9] == "rust-analyzer", vim.inspect(twice.cmd))
-- A bridge command a person wrote is no one's to wrap again.
local own = {{ name = "own", cmd = {{ "eitri", "host", "lsp", "--root", root, "--", "x" }}, root_dir = root }}
assert(rawequal(lsp.wrap(own), own))
-- Starting twice through the installed wrapper still gives one bridge command.
vim.cmd.edit("src/main.rs")
local id = vim.lsp.start(server("twice"))
assert(id)
local client = vim.lsp.get_client_by_id(id)
assert(#client.config.cmd == 11 and client.config.cmd[2] == "host" and client.config.cmd[9] == scripted, vim.inspect(client.config.cmd))"#
    ));
}

#[test]
#[ignore = "requires headless nvim"]
fn vim_lsp_start_reaches_the_bridge() {
    check(&format!(
        r#"{SETUP}
vim.cmd.edit("src/main.rs")
local id = vim.lsp.start(server("direct"))
assert(id, "the client did not start")
wait(function() return got("direct", "initialize") end, "the server to be initialized through the bridge")
local bridges = bridge_calls()
assert(#bridges == 1, vim.inspect(calls()))
assert(vim.deep_equal(bridges[1], {{ "host", "lsp", "--root", root, "--grace", "300", "--", scripted, base .. "/script.jsonl", base .. "/received-direct" }}), vim.inspect(bridges[1]))
wait(function() return #vim.lsp.get_clients({{ bufnr = 0 }}) == 1 end, "the client to attach")
local clients = vim.lsp.get_clients({{ bufnr = 0 }})
assert(clients[1].id == id)
-- The server is told nvim's own rootUri, not the recorded root's server path.
assert(clients[1].config.root_dir == root)
-- A second start from another buffer of the same project reuses the client (the wrap changes nothing about that).
vim.cmd.enew()
vim.cmd.edit("Cargo.toml")
assert(vim.lsp.start(server("direct")) == id)
assert(#bridge_calls() == 1)
-- A server pinned local never reaches the bridge.
local pinned = vim.lsp.start(server("pinned"))
assert(pinned)
wait(function() return got("pinned", "initialize") end, "the pinned server to start")
assert(#bridge_calls() == 1, "the pinned server went through the bridge: " .. vim.inspect(calls()))"#
    ));
}

#[test]
#[ignore = "requires headless nvim"]
fn vim_lsp_enable_with_root_markers_reaches_the_bridge() {
    let outputs = check(&format!(
        r#"{SETUP}
if not vim.lsp.enable then
  return -- nvim 0.10 has no `vim.lsp.enable`
end
-- What `vim.lsp.start` receives from `vim.lsp.enable`, recorded just outside our wrap.
local seen = {{}}
local ours = vim.lsp.start
vim.lsp.start = function(config, opts, ...)
  table.insert(seen, {{ root_dir = config.root_dir, root_markers = config.root_markers, opts_root_markers = opts and opts._root_markers, bufnr = opts and opts.bufnr }})
  return ours(config, opts, ...)
end
local configured = server("scripted", {{ root_dir = false, root_markers = {{ "Cargo.toml" }}, filetypes = {{ "rust" }} }})
configured.name = nil
vim.lsp.config("scripted", configured)
vim.lsp.enable("scripted")
vim.cmd.edit("src/main.rs")
vim.bo.filetype = "rust"
wait(function() return got("scripted", "initialize") end, "the enabled server to be initialized through the bridge")
local bridges = bridge_calls()
assert(#bridges == 1, vim.inspect(calls()))
assert(bridges[1][3] == "--root" and bridges[1][4] == root, vim.inspect(bridges[1]))
assert(bridges[1][5] == "--grace" and bridges[1][6] == "300", vim.inspect(bridges[1]))
wait(function() return #vim.lsp.get_clients({{ bufnr = 0, name = "scripted" }}) == 1 end, "the client to attach: " .. vim.inspect(seen))
local clients = vim.lsp.get_clients({{ bufnr = 0, name = "scripted" }})
assert(clients[1].config.cmd[2] == "host" and clients[1].config.cmd[3] == "lsp")
-- What the 0.11+ enable hands `start` (printed for the record of the run when it is shown).
io.stderr:write("enable saw: " .. vim.json.encode(seen) .. "\n")"#
    ));
    // Neovim 0.10 has no `vim.lsp.enable` and returns early: a run with no newer Neovim checked nothing.
    assert!(
        outputs.iter().any(|stderr| stderr.contains("enable saw:")),
        "no Neovim 0.11 or later ran this test: set EITRI_TEST_NVIM_0_11 or EITRI_TEST_NVIM_0_12"
    );
}

#[test]
#[ignore = "requires headless nvim"]
fn the_module_loads_in_eitris_window_through_the_preload() {
    check_startup(
        Startup::Embedded,
        &format!(
            r#"
assert(package.preload["eitri.remote.lsp"], "the snippet preloads the language-server module")
assert(package.loaded["eitri.remote.lsp"], "setup loaded it")
vim.cmd.edit("src/main.rs")
-- The window's options reached the module: the grace, and the server kept local.
local id = vim.lsp.start(server("window"))
assert(id)
wait(function() return got("window", "initialize") end, "the server to be initialized through the bridge")
local bridges = bridge_calls()
assert(#bridges == 1 and bridges[1][5] == "--grace" and bridges[1][6] == "120", vim.inspect(bridges))
local pinned = vim.lsp.start(server("pinned"))
wait(function() return got("pinned", "initialize") end, "the pinned server to start")
assert(#bridge_calls() == 1, vim.inspect(calls()))
assert(require("eitri.remote").root_of(root .. "/src/main.rs") == root)"#
        ),
    );
}

#[test]
#[ignore = "requires headless nvim"]
fn an_eitri_remote_buffer_never_starts_a_local_server() {
    check(&format!(
        r#"{SETUP}
-- A language server configured on this machine attaches by filetype, as lspconfig's autostart and `vim.lsp.enable` do.
local attempts, started = 0, {{}}
api.nvim_create_autocmd("FileType", {{ pattern = "rust", callback = function(args)
  attempts = attempts + 1
  local id = vim.lsp.start(server("autostart"), {{ bufnr = args.buf }})
  if id then table.insert(started, id) end
end }})
vim.cmd.edit("{DEPENDENCY_PATH_EDIT}")
local buf = api.nvim_get_current_buf()
wait(function() return vim.bo[buf].filetype == "rust" and vim.bo[buf].modifiable == false and api.nvim_buf_get_lines(buf, 0, 1, false)[1] == "pub fn dep() {{}}" end, "the text to land")
vim.wait(300, function() return false end, 50)
assert(attempts >= 1 and #started == 0, "the autostart ran " .. attempts .. " times and started " .. vim.inspect(started))
assert(#vim.lsp.get_clients() == 0, "a server was started for a file of the server: " .. vim.inspect(vim.lsp.get_clients()))
assert(not vim.uv.fs_stat(base .. "/received-autostart"), "a local server ran")
assert(lsp.refused.autostart == true)
-- A server of this machine, started by hand for that buffer, is refused as well.
assert(vim.lsp.start(server("by-hand"), {{ bufnr = buf }}) == nil)
-- Another host's bridge is refused, and so is a bridge command that names no recorded project.
local other = {{ name = "other", root_dir = base .. "/other-project",
  cmd = {{ program, "host", "lsp", "--root", base .. "/other-project", "--", scripted, base .. "/script.jsonl", base .. "/received-other" }} }}
assert(vim.lsp.start(other, {{ bufnr = buf }}) == nil)
local nowhere = {{ name = "nowhere", cmd = {{ program, "host", "lsp", "--root", "/nowhere", "--", scripted, base .. "/script.jsonl", base .. "/received-nowhere" }} }}
assert(vim.lsp.start(nowhere, {{ bufnr = buf }}) == nil)
assert(#vim.lsp.get_clients() == 0 and #bridge_calls() == 0, vim.inspect(calls()))
-- The bridge of its own host is allowed.
local own = {{ name = "own", root_dir = root,
  cmd = {{ program, "host", "lsp", "--root", root, "--", scripted, base .. "/script.jsonl", base .. "/received-own" }} }}
local id = vim.lsp.start(own, {{ bufnr = buf }})
assert(id, "a bridge of the buffer's own host is allowed")
wait(function() return got("own", "initialize") end, "the own host's bridge to start")
-- A buffer of the same name that is not read yet is an eitri-remote buffer too.
local unread = vim.fn.bufadd("eitri-remote://example-host/opt/dep/src/other.rs")
assert(vim.lsp.start(server("unread"), {{ bufnr = unread }}) == nil)
-- A name with the scheme that does not parse is never a file of this machine either: no server at all, not even a
-- bridge (it has no host to match).
for _, name in ipairs({{ "eitri-remote://example-host", "eitri-remote:///opt/dep/src/lib.rs" }}) do
  local odd = vim.fn.bufadd(name)
  assert(vim.lsp.start(server("odd"), {{ bufnr = odd }}) == nil, name)
  assert(vim.lsp.start(own, {{ bufnr = odd }}) == nil, name)
end
assert(lsp.refused.odd == true and not vim.uv.fs_stat(base .. "/received-odd"))
-- A buffer of the project is unaffected: the same wrapper still sends its server to the bridge.
vim.cmd.edit(root .. "/src/main.rs")
assert(vim.lsp.start(server("plain")))
-- A local client attached by a path the wrapper does not see is detached again once it attaches.
local here = vim.lsp.start(server("pinned"))
assert(here)
wait(function() return got("pinned", "initialize") end, "the pinned server to start")
assert(vim.lsp.buf_attach_client(buf, here))
wait(function() return not vim.lsp.buf_is_attached(buf, here) end, "the local client to be detached")
assert(lsp.refused.pinned == true)"#,
        DEPENDENCY_PATH_EDIT = "eitri-remote://example-host/opt/dep/src/lib.rs"
    ));
}

#[test]
#[ignore = "requires headless nvim"]
fn checkhealth_names_remote_local_and_function_clients() {
    check(&format!(
        r#"{SETUP}
vim.cmd.edit("src/main.rs")
local function by_function(dispatchers)
  return vim.lsp.rpc.start({{ scripted, base .. "/script.jsonl", base .. "/received-dynamic" }}, dispatchers)
end
assert(vim.lsp.start(server("on-server")))
assert(vim.lsp.start(server("pinned")))
assert(vim.lsp.start(server("dynamic", {{ cmd = by_function }})))
wait(function() return #vim.lsp.get_clients({{ bufnr = 0 }}) == 3 end, "three clients to attach")
-- A name that came from outside is shown with its control characters escaped.
lsp.function_cmds["unattached\n\27[31mname"] = root
-- C1 controls and the invisible direction marks are escaped too.
lsp.function_cmds["bidi\226\128\174name\194\133"] = root
-- So are the line and paragraph separators and the byte-order mark.
lsp.function_cmds["line\226\128\168para\226\128\169mark\239\187\191"] = root
-- And the Arabic letter mark, as the Rust side escapes it.
lsp.function_cmds["arabic\216\156mark"] = root
local source = api.nvim_get_current_buf()
local function report(last)
  vim.cmd("checkhealth eitri")
  local buf = api.nvim_get_current_buf()
  local function text() return table.concat(api.nvim_buf_get_lines(buf, 0, -1, false), "\n") end
  local done = vim.wait(10000, function() return text():find(last or "mutagen", 1, true) ~= nil end, 10)
  assert(done, "timed out: the report to be written, a buffer of " .. api.nvim_buf_get_name(buf) .. ":\n" .. text())
  return text()
end
local function has(text, part) assert(text:find(part, 1, true), "missing " .. vim.inspect(part) .. " in:\n" .. text) end
local text = report()
has(text, "Remote project: example-host:" .. root)
has(text, "eitri: " .. program .. ", eitri 9.9.9-test")
has(text, "remote_dev.enabled is on")
has(text, "on-server: remote, runs on the server through the bridge (root " .. root .. ")")
has(text, "pinned: local, pinned")
has(text, "dynamic: local, its cmd is a function")
has(text, "mutagen: " .. base .. "/mutagen, version 0.18.1")
has(text, "unattached\\u{{a}}\\u{{1b}}[31mname: local, its cmd is a function")
assert(not text:find("\27", 1, true), "an escape character reached the report")
has(text, "bidi\\u{{202e}}name\\u{{85}}: local, its cmd is a function")
assert(not text:find("\226\128\174", 1, true) and not text:find("\194\133", 1, true), "an invisible mark reached the report")
has(text, "line\\u{{2028}}para\\u{{2029}}mark\\u{{feff}}: local, its cmd is a function")
has(text, "arabic\\u{{61c}}mark: local, its cmd is a function")
assert(not text:find("\216\156", 1, true), "the Arabic letter mark reached the report")
assert(not text:find("\226\128\168", 1, true) and not text:find("\239\187\191", 1, true), "a separator reached the report")
-- The program's own answer about the switch: off, as `eitri host open` refuses.
vim.cmd.tabclose()
assert(api.nvim_get_current_buf() == source)
io.open(base .. "/remote_dev_off", "w"):close()
has(report(), "remote_dev.enabled is off")
-- Outside every recorded project the section says so in one line.
vim.cmd.tabclose()
vim.cmd.edit(base .. "/elsewhere/notes.rs")
local outside = report("outside every recorded")
has(outside, "This buffer is outside every recorded remote project")
assert(not outside:find("Remote project:", 1, true) and not outside:find("on-server", 1, true), outside)"#
    ));
}

#[test]
#[ignore = "requires headless nvim"]
fn plain_nvim_without_a_window_starts_a_remote_server() {
    check(&format!(
        r#"-- No window: nothing of Eitri's is in the environment or preloaded, only the plugin on the runtimepath and a
-- project opened once (its record).
for key in pairs(vim.fn.environ()) do
  assert(not key:find("^EITRI_") or key == "EITRI_PROGRAM", "an Eitri variable is set: " .. key)
end
assert(package.preload["eitri.remote.lsp"] == nil and package.loaded["eitri.remote.lsp"], "the plugin loads it, a window does not")
vim.cmd.edit("src/main.rs")
-- The plugin alone wraps: the default grace, the program found by name on PATH.
assert(vim.lsp.start(server("by-default")))
wait(function() return got("by-default", "initialize") end, "the server to be initialized through the bridge")
assert(#bridge_calls() == 1 and bridge_calls()[1][6] == "1800", vim.inspect(calls()))
-- The three lines of the guide: the runtimepath (done by the startup file), the options, the project.
require("eitri.remote").setup({{ lsp = {{ grace = 300, local_servers = {{ "pinned" }} }} }})
assert(vim.lsp.start(server("plain")))
wait(function() return got("plain", "initialize") end, "the server to be initialized through the bridge")
local bridges = bridge_calls()
assert(#bridges == 2, vim.inspect(calls()))
assert(vim.deep_equal(bridges[2], {{ "host", "lsp", "--root", root, "--grace", "300", "--", scripted, base .. "/script.jsonl", base .. "/received-plain" }}), vim.inspect(bridges[2]))
assert(lsp.wrap(server("again")).cmd[1] == "eitri", "the program is the launcher's, found on PATH")
assert(vim.lsp.start(server("pinned")))
wait(function() return got("pinned", "initialize") end, "the pinned server to start")
assert(#bridge_calls() == 2, "the pinned server went through the bridge")"#
    ));
}

#[test]
#[ignore = "requires headless nvim"]
fn a_running_client_is_never_reused_across_the_bridge() {
    let outputs = check(&format!(
        r#"{SETUP}
local function side(id) return lsp.bridge_root(vim.lsp.get_client_by_id(id).config.cmd) end
local function at(name) return {{ scripted, base .. "/script.jsonl", base .. "/received-" .. name }} end
-- A server with no root, of the project: it runs through the bridge.
vim.cmd.edit("src/main.rs")
local project = api.nvim_get_current_buf()
local remote = vim.lsp.start(server("shared", {{ root_dir = false }}))
assert(remote and side(remote) == root)
wait(function() return got("shared", "initialize") end, "the bridge's server to start")
-- A file of this machine with the same root-less server gets a client of its own, here: nvim's own test compares
-- names and folders, and both have none.
vim.cmd.edit(base .. "/elsewhere/notes.rs")
local notes = api.nvim_get_current_buf()
local here = vim.lsp.start(server("shared", {{ root_dir = false, cmd = at("shared-here") }}))
assert(here and here ~= remote, "the bridge's client was reused for a file of this machine")
assert(side(here) == nil)
wait(function() return got("shared-here", "initialize") end, "the local server to start")
assert(not vim.lsp.buf_is_attached(notes, remote))
assert(#bridge_calls() == 1, vim.inspect(calls()))
-- The reverse: a root-less local server first, then a file of the project gets the bridge.
local first = vim.lsp.start(server("reverse", {{ root_dir = false }}))
assert(first and side(first) == nil)
wait(function() return got("reverse", "initialize") end, "the local server to start")
vim.cmd.edit(root .. "/src/main.rs")
local bridged = vim.lsp.start(server("reverse", {{ root_dir = false, cmd = at("reverse-remote") }}))
assert(bridged and bridged ~= first and side(bridged) == root, "a local client was reused for a file of the project")
assert(not vim.lsp.buf_is_attached(project, first))
-- nvim 0.10 never reuses a client with no root, so only 0.11 and later put the two halves above to the test.
if vim.fn.has("nvim-0.11") == 1 then io.stderr:write("root-less reuse checked\n") end
-- The switch that keeps servers local is not undone by a client of that name and root already running remotely.
local kept = vim.lsp.start(server("kept"))
assert(kept and side(kept) == root)
vim.g.eitri_remote_lsp = false
local now_local = vim.lsp.start(server("kept", {{ cmd = at("kept-here") }}))
assert(now_local and now_local ~= kept and side(now_local) == nil)
vim.g.eitri_remote_lsp = nil
-- On one side, the same name and root is reused as before; a caller's own test still has its say.
assert(vim.lsp.start(server("kept")) == kept)
local asked = 0
assert(vim.lsp.start(server("kept"), {{ reuse_client = function() asked = asked + 1 return false end }}) ~= kept)
assert(asked >= 1)"#
    ));
    assert!(
        outputs.iter().any(|stderr| stderr.contains("root-less reuse checked")),
        "no Neovim 0.11 or later ran this test: set EITRI_TEST_NVIM_0_11 or EITRI_TEST_NVIM_0_12"
    );
}

#[test]
#[ignore = "requires headless nvim"]
fn root_markers_decide_by_the_root_they_find() {
    let outputs = check(&format!(
        r#"{SETUP}
if vim.fn.has("nvim-0.11") ~= 1 then
  return -- nvim 0.10's `vim.lsp.start` has no root markers
end
vim.cmd.edit("src/main.rs")
local buf = api.nvim_get_current_buf()
-- A marker above the recorded root: the server would be told a root the server does not have, so it stays here.
local above = server("above", {{ root_dir = false }})
assert(rawequal(lsp.wrap(above, buf, {{ "above.marker" }}), above), "a root above the project went to the bridge")
-- The first marker found wins, as in nvim; the server is given that root.
local at_root = lsp.wrap(server("r", {{ root_dir = false }}), buf, {{ "Cargo.toml", "above.marker" }})
assert(lsp.bridge_root(at_root.cmd) == root and at_root.root_dir == root, vim.inspect(at_root))
-- No marker found: the buffer's path decides, and the server has no root.
local none = lsp.wrap(server("n", {{ root_dir = false }}), buf, {{ "no-such.marker" }})
assert(lsp.bridge_root(none.cmd) == root and none.root_dir == nil, vim.inspect(none))
-- Through the installed wrapper, as `vim.lsp.enable` calls it.
local id = vim.lsp.start(server("above", {{ root_dir = false }}), {{ bufnr = buf, _root_markers = {{ "above.marker" }} }})
assert(id)
local client = vim.lsp.get_client_by_id(id)
assert(lsp.bridge_root(client.config.cmd) == nil, vim.inspect(client.config.cmd))
assert(client.config.root_dir == vim.fs.dirname(root), vim.inspect(client.config.root_dir))
wait(function() return got("above", "initialize") end, "the local server to start")
assert(#bridge_calls() == 0, vim.inspect(calls()))
io.stderr:write("markers checked\n")"#
    ));
    assert!(
        outputs.iter().any(|stderr| stderr.contains("markers checked")),
        "no Neovim 0.11 or later ran this test: set EITRI_TEST_NVIM_0_11 or EITRI_TEST_NVIM_0_12"
    );
}

#[test]
#[ignore = "requires headless nvim"]
fn root_markers_are_searched_as_each_neovim_searches_them() {
    let outputs = check(&format!(
        r#"{SETUP}
if vim.fn.has("nvim-0.11") ~= 1 then
  return -- nvim 0.10's `vim.lsp.start` has no root markers
end
-- Markers at two levels: `Cargo.toml` at the recorded root, `inner.marker` below it, `above.marker` above it.
io.open(root .. "/src/inner.marker", "w"):close()
local src = root .. "/src"
vim.cmd.edit("src/main.rs")
local buf = api.nvim_get_current_buf()
-- nvim's own answer: the root its unwrapped `vim.lsp.start` gives a client for these markers.
local count = 0
local function nvims_root(markers)
  count = count + 1
  local id = lsp.originals.start(server("plain-" .. count, {{ root_dir = false }}), {{ bufnr = buf, _root_markers = markers }})
  assert(id, vim.inspect(markers))
  return vim.lsp.get_client_by_id(id).config.root_dir
end
-- The wrap finds the root nvim finds, and goes remote exactly when that root is in the project.
local function same(markers, expected, remote)
  local found = nvims_root(markers)
  assert(found == expected, vim.inspect(markers) .. ": nvim found " .. tostring(found) .. ", not " .. expected)
  local wrapped = lsp.wrap(server("w", {{ root_dir = false }}), buf, markers)
  if remote then
    assert(lsp.bridge_root(wrapped.cmd) == root and wrapped.root_dir == expected, vim.inspect(markers) .. " " .. vim.inspect(wrapped))
  else
    assert(lsp.bridge_root(wrapped.cmd) == nil, vim.inspect(markers) .. " went remote: " .. vim.inspect(wrapped.cmd))
  end
end
-- 0.11.0 and 0.11.1 take the nearest directory holding any marker; later releases each marker in turn.
local nearest = vim.fn.has("nvim-0.11.2") ~= 1
same({{ "Cargo.toml", "inner.marker" }}, nearest and src or root, true)
-- A marker above the project listed first: the nearest search stays inside it, the search in turn leaves it.
same({{ "above.marker", "inner.marker" }}, nearest and src or base, nearest)
-- A nested list (0.11.2 and later): its names have equal weight, the nearest wins.
if not nearest then
  same({{ {{ "above.marker", "inner.marker" }} }}, src, true)
  same({{ {{ "above.marker", "inner.marker" }}, "Cargo.toml" }}, src, true)
  same({{ "Cargo.toml", {{ "above.marker", "inner.marker" }} }}, root, true)
  same({{ {{ "above.marker", "no-such.marker" }}, "inner.marker" }}, base, false)
end
io.stderr:write("each search checked\n")"#
    ));
    assert!(
        outputs.iter().any(|stderr| stderr.contains("each search checked")),
        "no Neovim 0.11 or later ran this test: set EITRI_TEST_NVIM_0_11 or EITRI_TEST_NVIM_0_12"
    );
}

#[test]
#[ignore = "requires headless nvim"]
fn a_shared_setup_call_keeps_the_windows_options() {
    check_startup(
        Startup::Embedded,
        r#"
-- A shared nvim configuration's own call, as the guide gives it, run inside Eitri's window.
require("eitri.remote").setup({ cmd = "another-eitri", lsp = { grace = 999, local_servers = {} } })
vim.cmd.edit("src/main.rs")
local wrapped = lsp.wrap(server("window"))
assert(wrapped.cmd[1] == program and wrapped.cmd[7] == "120", vim.inspect(wrapped.cmd))
local pinned = server("pinned")
assert(rawequal(lsp.wrap(pinned), pinned), "the window's pin was dropped")
-- The window's root still bounds the lookup: another recorded project is not this window's.
assert(require("eitri.remote").root_of(base .. "/other-project") == nil)
assert(require("eitri.remote").root_of(root .. "/src/main.rs") == root)"#,
    );
}

mod script_fixture {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/executable.rs"));
}
