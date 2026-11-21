//! The exec wrappers in eitri.nvim, run by a real nvim against a recorded remote project and a stand-in `eitri`.
use std::os::unix::fs::PermissionsExt;
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
    Plugin,
    Preload,
}

/// Runs `body` (Lua) in a headless nvim whose cwd is a recorded remote project; `root` and `outside` are Lua globals.
fn check(startup: Startup, body: &str) {
    for nvim in versions() {
        let scratch = Scratch(std::env::temp_dir().join(format!("eitri-exec-{}", uuid::Uuid::new_v4())));
        let base = &scratch.0;
        let root = base.join("project");
        let outside = base.join("elsewhere");
        let hosts = base.join("state/eitri/hosts");
        agent::private_fs::create_private_dir_all(&hosts, base).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(root.join("file"), b"laptop\n").unwrap();
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
        eitri_core::remote_project::RemoteProjects::new(Some(base.join("state").as_os_str()), None)
            .save(&record)
            .unwrap();
        // A stand-in `eitri`: it records its arguments, answers the watch the plugin starts, and prints one
        // compiler-style line so `:make` has something to parse. As `host term` it stays until a hangup, which it
        // records.
        let program = base.join("eitri");
        std::fs::write(
            &program,
            "#!/bin/sh\n\
             if [ \"$1\" = watch ]; then cat >/dev/null; exit 0; fi\n\
             if [ \"$2\" = term ]; then trap 'echo hangup >> \"$FIXTURE/calls\"; exit 0' HUP; fi\n\
             printf '%s\\n' \"$*\" >> \"$FIXTURE/calls\"\n\
             if [ \"$2\" = term ]; then while :; do sleep 1; done; fi\n\
             echo 'file:1:1: boom'\n",
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        let plugin = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("nvim/eitri.nvim");
        let init = base.join("init.lua");
        let rtp = format!(
            "vim.opt.runtimepath:prepend({})",
            eitri_core::onboarding::lua_text::quote(plugin.to_str().unwrap())
        );
        std::fs::write(
            &init,
            if matches!(startup, Startup::Plugin) {
                rtp
            } else {
                String::new()
            },
        )
        .unwrap();
        let script = base.join("check.lua");
        std::fs::write(
            &script,
            format!(
                r#"
local ok, why = pcall(function()
local root, outside = vim.fn.getcwd(), vim.env.FIXTURE .. "/elsewhere"
local remote = require("eitri.remote")
{body}
end)
if not ok then io.stderr:write(tostring(why),"\n"); vim.cmd("cquit 1") else vim.cmd("qa!") end
"#
            ),
        )
        .unwrap();
        let mut cmd = Command::new(&nvim);
        if matches!(startup, Startup::Preload) {
            cmd.args([
                "--cmd",
                &eitri_core::remote_sync::nvim_loader(&root, program.to_str().unwrap()),
            ]);
        }
        cmd.args(["--headless", "-n", "-i", "NONE", "-u"])
            .arg(&init)
            .arg(root.join("file"))
            .args(["-c", "lua dofile(vim.env.FIXTURE .. '/check.lua')"])
            .current_dir(&root)
            .env("FIXTURE", base)
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .env(
                "PATH",
                std::env::join_paths(
                    std::iter::once(base.clone()).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
                )
                .unwrap(),
            )
            .env("HOME", base)
            .env_remove("NVIM")
            .env_remove("NVIM_LISTEN_ADDRESS")
            .env_remove("VIMINIT")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
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
        let deadline = Instant::now() + Duration::from_secs(15);
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
            "{nvim:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
#[ignore = "requires headless nvim"]
fn cmd_wraps_inside_a_remote_root_and_passes_through_outside() {
    check(
        Startup::Plugin,
        r#"local eitri = vim.env.FIXTURE .. "/eitri"
local inside = remote.cmd({ "prettier", "--stdin-filepath", "a b.ts" })
assert(vim.deep_equal(inside, { "eitri", "host", "exec", "--cwd", root, "--no-stdin", "--", "prettier", "--stdin-filepath", "a b.ts" }), vim.inspect(inside))
local sub = remote.cmd({ "ruff" }, { cwd = root .. "/src", stdin = true })
assert(vim.deep_equal(sub, { "eitri", "host", "exec", "--cwd", root .. "/src", "--no-flush", "--", "ruff" }), vim.inspect(sub))
local flushed = remote.cmd({ "ruff" }, { stdin = true, flush = true })
assert(vim.deep_equal(flushed, { "eitri", "host", "exec", "--cwd", root, "--", "ruff" }), vim.inspect(flushed))
local out = remote.cmd({ "ruff", "check" }, { cwd = outside })
assert(vim.deep_equal(out, { "ruff", "check" }), vim.inspect(out))
local noflush = remote.cmd({ "make" }, { flush = false })
assert(vim.deep_equal(noflush, { "eitri", "host", "exec", "--cwd", root, "--no-flush", "--no-stdin", "--", "make" }), vim.inspect(noflush))"#,
    )
}

#[test]
#[ignore = "requires headless nvim"]
fn makeprg_is_buffer_local_and_wrapped() {
    check(
        Startup::Plugin,
        r#"local before_global, before_efm = vim.go.makeprg, vim.bo.errorformat
vim.cmd("EitriRemote makeprg make -j4")
assert(vim.bo.makeprg:find("host", 1, true) and vim.bo.makeprg:find("--no-stdin", 1, true), vim.bo.makeprg)
assert(vim.endswith(vim.bo.makeprg, " -- make -j4"), vim.bo.makeprg)
assert(vim.go.makeprg == before_global and vim.bo.errorformat == before_efm)
vim.cmd("enew")
assert(vim.bo.makeprg == "" , "another buffer keeps the global makeprg: " .. vim.bo.makeprg)
vim.cmd("buffer 1")
vim.cmd("silent make")
local calls = table.concat(vim.fn.readfile(vim.env.FIXTURE .. "/calls"), "\n")
assert(calls:find("host exec --cwd " .. root .. " --no-stdin -- make -j4", 1, true), calls)
assert(#vim.fn.getqflist() > 0 and vim.fn.getqflist()[1].lnum == 1, vim.inspect(vim.fn.getqflist()))
assert(vim.tbl_contains(vim.fn.getcompletion("EitriRemote ", "cmdline"), "makeprg"))
local ok = pcall(vim.cmd, "EitriRemote nonsense")
assert(not ok)"#,
    )
}

#[test]
#[ignore = "requires headless nvim"]
fn the_module_loads_from_the_preload_snippet() {
    check(
        Startup::Preload,
        r#"assert(package.preload["eitri.remote.exec"], "the snippet preloads the exec module")
local argv = remote.cmd({ "stylua", "-" }, { cwd = root })
assert(vim.deep_equal(argv, { vim.env.FIXTURE .. "/eitri", "host", "exec", "--cwd", root, "--no-stdin", "--", "stylua", "-" }), vim.inspect(argv))
assert(package.loaded["eitri.remote.exec"])"#,
    )
}

#[test]
#[ignore = "requires headless nvim"]
fn term_opens_the_relay_in_a_terminal_buffer_of_a_new_tab() {
    check(
        Startup::Plugin,
        r#"local calls = vim.env.FIXTURE .. "/calls"
local tabs = vim.fn.tabpagenr("$")
vim.cmd("EitriRemote term")
assert(vim.fn.tabpagenr("$") == tabs + 1, "a new tab")
assert(vim.bo.buftype == "terminal", vim.bo.buftype)
-- The plugin's own sync watch is in the same file: wait for the relay's line.
local function lines() return vim.fn.filereadable(calls) == 1 and table.concat(vim.fn.readfile(calls), "\n") or "" end
assert(vim.wait(10000, function() return lines():find("host term", 1, true) ~= nil end), "the relay was started: " .. lines())
assert(lines():find("host term --cwd " .. root, 1, true), lines())
assert(vim.tbl_contains(vim.fn.getcompletion("EitriRemote ", "cmdline"), "term"))
-- Wiping the buffer hangs the relay up, which is what makes it close the shell on the server.
vim.cmd("bwipe!")
assert(vim.wait(10000, function() return lines():find("hangup", 1, true) ~= nil end), "the relay was hung up: " .. lines())
-- Outside a recorded project nothing opens.
vim.cmd("tabfirst")
vim.cmd("enew")
vim.cmd("cd " .. vim.fn.fnameescape(outside))
local before = vim.fn.tabpagenr("$")
vim.cmd("EitriRemote term")
assert(vim.fn.tabpagenr("$") == before, "no tab outside a remote project")"#,
    )
}
