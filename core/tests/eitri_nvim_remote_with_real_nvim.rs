//! The shipped plugin loaded by normal startup, with one private project and a local watch process.
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
    Options,
    Embedded,
}

fn check(body: &str) {
    check_startup(Startup::Plugin, body)
}

fn check_startup(startup: Startup, body: &str) {
    for nvim in versions() {
        let scratch = Scratch(std::env::temp_dir().join(format!("eitri-sync-{}", uuid::Uuid::new_v4())));
        let base = &scratch.0;
        let root = base.join("project");
        let hosts = base.join("state/eitri/hosts");
        agent::private_fs::create_private_dir_all(&hosts, base).unwrap();
        std::fs::create_dir(&root).unwrap();
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
        let store = eitri_core::remote_project::RemoteProjects::new(Some(base.join("state").as_os_str()), None);
        store.save(&record).unwrap();
        let program = base.join("eitri");
        script_fixture::write_executable_script(&program, br#"#!/usr/bin/env python3
import sys, os, json, hashlib
from pathlib import Path
base = Path(os.environ["FIXTURE"])
args = sys.argv[3:]
def flag(name): return (base / name).exists()
with (base / "executables").open("a") as log: print(sys.argv[0], file=log)
with (base / "calls").open("a") as log:
    print(json.dumps(args),file=log)
if args == ["watch", "--json"]:
    if flag("fail-watch"): sys.exit("watch fixture exited")
    st = {"host":"example-host", "phase":"watching", "git_pct":100, "conflicts":[{"path":"file","server_kind":"file","laptop_kind":"file"}],
          "problems":[{"path":"broken","side":"laptop","text":"absolute link"}], "copied_back":["restored"],"kept_here":["cache"],"paused":None}
    print(json.dumps({"type":"status","status":st,"shim":[["PATH",str(base/"shim")],["EITRI_GIT_SHIM_EXE",str(base/"eitri")],["EITRI_REAL_GIT","/usr/bin/git"]]}),flush=True)
    for line in sys.stdin:
        report = json.loads(line)
        if report.get("exit"): sys.exit("watch fixture exited")
        if report.get("conflicts"):
            st["conflicts"] = report["conflicts"]
            print(json.dumps({"type":"status","status":st}),flush=True)
        with (base / "reports").open("a") as log: log.write(line)
elif args[:3] == ["status","--json","--details"]:
    print(json.dumps({"choices":{p:{"merge":True,"keep_server":True,"keep_laptop":True} for p in ["file","other"]},"problems":[{"path":"pipe","side":"server","text":"other"}],"excludes":["/target/"]}))
elif args == ["show-server","file","--with-hashes"]:
    print("eitri-sync-hashes",json.dumps({"laptop":hashlib.sha256(Path("file").read_bytes()).hexdigest(),"server":hashlib.sha256(b"server\0\r\n").hexdigest()}),file=sys.stderr)
    sys.stdout.buffer.write(b"x" * (8 * 1024 * 1024 + 1) if flag("large-server") else b"server\0\r\n")
elif args == ["merge-write","file","--server-sha256",hashlib.sha256(b"server\0\r\n").hexdigest(),"--laptop-sha256",hashlib.sha256(Path("file").read_bytes()).hexdigest()]:
    if flag("refused"): sys.exit("resolve after the turn")
    data = sys.stdin.buffer.read()
    Path("file").write_bytes(data)
    print("eitri-sync-laptop-written",hashlib.sha256(data).hexdigest(),file=sys.stderr)
    if flag("partial"):
        print("laptop file written; server changed")
        sys.exit(1)
    print("Backup: /saved/server/file")
elif args in (["resolve","file","--keep","laptop"], ["resolve","file","--keep","server"], ["dismiss","--all"]):
    print("Backup: /saved/displaced/file")
else:
    sys.exit("unexpected command: " + repr(args))
"#,0o700).unwrap();
        let plugin = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("nvim/eitri.nvim");
        let selected = base.join("selected-eitri");
        std::fs::hard_link(&program, &selected).unwrap();
        let init = base.join("init.lua");
        let set_rtp = format!(
            "vim.opt.runtimepath:prepend({})",
            eitri_core::onboarding::lua_text::quote(plugin.to_str().unwrap())
        );
        let setup = match startup {
            Startup::Plugin => set_rtp,
            Startup::Options => format!(
                r#"{set_rtp}
local env = vim.env
vim.env = setmetatable({{}}, {{__index=function(_,k) assert(not k:match("^EITRI_"),k); return env[k] end,
 __newindex=function(_,k,v) env[k]=v end}})
require("eitri.remote").setup({{root=vim.fn.getcwd(),cmd=env.FIXTURE .. "/selected-eitri",embedded=true}})"#
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
local function wait(f) assert(vim.wait(4000,f,10),"timed out: " .. vim.fn.execute("messages")) end
local function lines() return table.concat(api.nvim_buf_get_lines(0,0,-1,false),"\n") end
local base = vim.env.FIXTURE
local function view() vim.cmd.EitriSync(); wait(function() return lines():find("Exclude rule:",1,true) end) end
local function enter(pattern)
 assert(vim.fn.search(pattern,"w") ~= 0,pattern)
 vim.fn.maparg("<CR>","n",false,true).callback()
end
local function diff()
 view(); enter("^  Merge$")
 wait(function() return vim.wo.diff and api.nvim_buf_get_name(0) == vim.fn.getcwd() .. "/file" end)
end
{body}
end)
if not ok then io.stderr:write(tostring(why),"\n"); vim.cmd("cquit 1") else vim.cmd("qa!") end
"#
            ),
        )
        .unwrap();
        // The script runs after plugin loading, with the real editor loop available to job callbacks.
        let mut cmd = Command::new(&nvim);
        if matches!(startup, Startup::Embedded) {
            cmd.args([
                "--cmd",
                &eitri_core::remote_sync::nvim_loader(&root, selected.to_str().unwrap()),
            ]);
        }
        cmd.args(["--headless", "-n", "-i", "NONE", "-u"])
            .arg(init)
            .arg(root.join("file"))
            .args(["-c", "lua dofile(vim.env.FIXTURE .. '/check.lua')"])
            .current_dir(&root)
            .env("FIXTURE", base)
            .env("EITRI_GIT_SHIM_EXE", base.join("wrong-executable"))
            .env("EITRI_PROJECT_ROOT", base.join("wrong-project"))
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
fn eitri_sync_exists_after_an_ordinary_start() {
    check(r#"assert(vim.fn.exists(":EitriSync") == 2)"#)
}
#[test]
#[ignore = "requires headless nvim"]
fn status_returns_the_host_and_conflict_count() {
    check(r#"wait(function() return require("eitri.remote").status() == "⇅ example-host · 1 conflicts" end)"#)
}
#[test]
#[ignore = "requires headless nvim"]
fn conflicted_buffer_gets_the_sign_and_notice() {
    check(
        r#"wait(function()
local ns = api.nvim_get_namespaces()["eitri-sync"]
if not ns then return false end
local marks = api.nvim_buf_get_extmarks(0,ns,0,-1,{details=true})
return #marks == 1 and vim.trim(marks[1][4].sign_text) == "⇅" and marks[1][4].virt_lines[1][1][1]:find("differs on example-host",1,true)
end)"#,
    )
}
#[test]
#[ignore = "requires headless nvim"]
fn eitri_sync_lists_conflicts_problems_copied_back_and_excludes() {
    check(
        r#"view()
for _, text in ipairs({"Conflict: file","not synced: absolute link","eitri host sync exclude <path>","Copied back: restored","/target/","Keep the server's","Merge","an edit made on one side beats a delete"}) do
assert(lines():find(text,1,true),lines()) end
local laptop = lines():match("[^\n]*not synced: absolute link[^\n]*")
assert(laptop:find("fix the entry on this laptop",1,true) and not laptop:find("terminal on",1,true), laptop)
local server = lines():match("[^\n]*not synced: other[^\n]*")
assert(server:find("fix the entry on the server",1,true) and server:find("terminal on example-host",1,true), server)"#,
    )
}
#[test]
#[ignore = "requires headless nvim"]
fn eitri_sync_lists_paths_that_do_not_sync() {
    check(
        r#"view(); assert(lines():find("pipe (server) · not synced: other",1,true),lines())
assert(lines():find("this laptop keeps its copy: cache",1,true),lines())"#,
    )
}
#[test]
#[ignore = "requires headless nvim"]
fn merge_opens_a_readonly_server_buffer_and_w_calls_merge_write() {
    check(
        r#"view()
local i = assert(vim.fn.search("^  Merge$","w"))
api.nvim_win_set_cursor(0,{i,0})
vim.fn.maparg("<CR>","n",false,true).callback()
wait(function() return vim.wo.diff and api.nvim_buf_get_name(0) == vim.fn.getcwd() .. "/file" end)
local other = vim.fn.winbufnr(vim.fn.winnr('#'))
assert(vim.bo[other].readonly and not vim.bo[other].modifiable)
api.nvim_buf_set_lines(0,0,-1,false,{"merged"})
vim.cmd.write()
assert(vim.fn.readfile("file")[1] == "merged")
assert(not vim.bo.modified)
assert(table.concat(vim.fn.readfile(base .. "/calls")," "):find("merge-write",1,true))"#,
    )
}
#[test]
#[ignore = "requires headless nvim"]
fn plain_nvim_attach_puts_the_shim_on_path() {
    check(
        r#"wait(function() return vim.split(vim.env.PATH,":",{plain=true})[1] == base .. "/shim" end)
assert(vim.env.EITRI_GIT_SHIM_EXE == base .. "/eitri")
assert(vim.env.EITRI_REAL_GIT == "/usr/bin/git")
vim.cmd.edit(vim.fn.fnameescape(base .. "/outside"))
assert(vim.split(vim.env.PATH,":",{plain=true})[1] == base .. "/shim", "local buffer removed the git shim")"#,
    )
}
#[test]
#[ignore = "requires headless nvim"]
fn module_reports_modified_buffers_to_watch() {
    check(
        r#"api.nvim_buf_set_lines(0,0,1,false,{"dirty"})
local path = api.nvim_buf_get_name(0)
local t = vim.uv.fs_stat(path).mtime
wait(function()
 if vim.fn.filereadable(base .. "/reports") == 0 then return false end
 for _,l in ipairs(vim.fn.readfile(base .. "/reports")) do
  local r=vim.json.decode(l)
  if r.unsaved[1] == path and r.files[1][2] == t.sec*1000+math.floor(t.nsec/1000000) then return true end
 end
end)
local n=0
for _,l in ipairs(vim.fn.readfile(base .. "/calls")) do if l:find("watch") then n=n+1 end end
assert(n==1)"#,
    )
}
#[test]
#[ignore = "requires headless nvim"]
fn module_reads_no_eitri_env_when_given_options() {
    check_startup(
        Startup::Options,
        r#"
wait(function() return require("eitri.remote").status() ~= "" end)
assert(vim.deep_equal(vim.fn.readfile(base .. "/executables"), {base .. "/selected-eitri"}))
assert(not vim.env.PATH:find(base .. "/shim",1,true))"#,
    )
}

#[test]
#[ignore = "requires headless nvim"]
fn embedded_cmd_starts_its_watch_without_the_plugin() {
    check_startup(
        Startup::Embedded,
        r#"
assert(not vim.g.loaded_eitri)
assert(vim.fn.exists(":EitriSync")==2)
view()
for _,exe in ipairs(vim.fn.readfile(base .. "/executables")) do assert(exe == base .. "/selected-eitri",exe) end
assert(lines():find("Conflict: file",1,true))"#,
    )
}

#[test]
#[ignore = "requires headless nvim"]
fn failed_watch_waits_for_an_explicit_retry() {
    check(
        r#"
local remote = require("eitri.remote")
wait(function() return remote.status() ~= "" end)
local function watches()
 local n = 0
 for _,l in ipairs(vim.fn.readfile(base .. "/calls")) do if l:find("watch") then n=n+1 end end
 return n
end
vim.fn.chansend(remote.current().job, '{"exit":true}\n')
wait(function() return remote.current().failed_at end)
for _=1,4 do vim.cmd("doautocmd BufEnter") end
assert(watches()==1 and not remote.current().job)
assert(vim.fn.execute("messages"):find("watch fixture exited",1,true))
vim.cmd.EitriSync()
wait(function() return watches()==2 and remote.status() ~= "" end)"#,
    )
}

#[test]
#[ignore = "requires headless nvim"]
fn unavailable_watch_executable_warns_once_across_buffer_entries() {
    check(
        r#"
local remote = require("eitri.remote")
local dir=base .. "/second"
vim.fn.mkdir(dir)
vim.cmd.cd(dir)
local attempts, warnings = 0, 0
local jobstart=vim.fn.jobstart
vim.fn.jobstart=function(...)
 attempts=attempts+1
 if attempts==1 then return -1 end
 return jobstart(...)
end
vim.notify=function(msg) if msg:find("could not start",1,true) then warnings=warnings+1 end end
remote.setup({root=dir,cmd=base .. "/does-not-exist"})
vim.cmd.edit(dir .. "/absent")
for _=1,3 do vim.cmd("doautocmd BufEnter") end
assert(attempts==1 and warnings==1)
vim.cmd.EitriSync()
assert(attempts==2 and lines():find("could not start",1,true))"#,
    )
}

#[test]
#[ignore = "requires headless nvim"]
fn enter_after_redraw_cannot_apply_another_conflicts_action() {
    check(
        r#"
view()
local selected=vim.fn.search("^  Keep the server's$","w")
local prompts={}
vim.fn.confirm=function(msg) table.insert(prompts,msg); return 1 end
local s=require("eitri.remote").current()
vim.fn.chansend(s.job,vim.json.encode({conflicts={{path="other",server_kind="file",laptop_kind="file"}}}) .. "\n")
wait(function() return lines():find("Conflict: other",1,true) and lines():find("  Merge",1,true) end)
vim.fn.maparg("<CR>","n",false,true).callback()
assert(#prompts==0)
for _,line in ipairs(vim.fn.readfile(base .. "/calls")) do assert(not line:find("resolve",1,true),line) end
api.nvim_win_set_cursor(0,{selected,0})
vim.bo.modifiable=true
api.nvim_set_current_line("a different row")
vim.fn.maparg("<CR>","n",false,true).callback()
assert(#prompts==0)"#,
    )
}

#[test]
#[ignore = "requires headless nvim"]
fn destructive_choices_name_the_path_and_side_and_show_the_backup() {
    check(
        r#"
view()
local prompts, notices={},{}
vim.fn.confirm=function(msg) table.insert(prompts,msg); return #prompts==3 and 1 or 2 end
vim.notify=function(msg) table.insert(notices,msg) end
enter("^  Keep the server's$")
enter("^Dismiss all copied back$")
enter("^  Keep the laptop's$")
assert(prompts[1]:find("file",1,true) and prompts[1]:find("remove the laptop copy",1,true))
assert(prompts[2]:find(vim.fn.getcwd(),1,true) and prompts[2]:find("Both sides keep",1,true))
assert(prompts[3]:find("file",1,true) and prompts[3]:find("remove the copy on example-host",1,true))
wait(function() return table.concat(notices):find("/saved/displaced/file",1,true) end)
local calls=table.concat(vim.fn.readfile(base .. "/calls"))
assert(not calls:find('"dismiss"') and not calls:find('"--keep", "server"'))"#,
    )
}

#[test]
#[ignore = "requires headless nvim"]
fn refused_merge_tracks_the_written_laptop_hash_and_buffer_state() {
    check(
        r#"
diff()
vim.fn.writefile({""},base .. "/partial")
api.nvim_buf_set_lines(0,0,-1,false,{"first save"})
local ok,msg=pcall(vim.cmd.write)
assert(not ok and msg:find("laptop file written;",1,true),msg)
assert(not vim.bo.modified and vim.fn.readfile("file")[1]=="first save")
vim.fn.delete(base .. "/partial")
vim.fn.writefile({""},base .. "/refused")
api.nvim_buf_set_lines(0,0,-1,false,{"second save"})
ok,msg=pcall(vim.cmd.write)
assert(not ok and msg:find("resolve after the turn",1,true),msg)
assert(vim.bo.modified and vim.fn.readfile("file")[1]=="first save")
vim.fn.delete(base .. "/refused")
local checked, command=false, vim.cmd
-- Record the real checktime command while letting Neovim process it.
vim.cmd=setmetatable({}, {__index=command,__call=function(_,cmd)
 if cmd=="checktime " .. api.nvim_get_current_buf() then checked=true end
 return command(cmd)
end})
command.write()
assert(checked and not vim.bo.modified and vim.fn.readfile("file")[1]=="second save")
assert(vim.fn.execute("messages"):find("/saved/server/file",1,true))"#,
    )
}

#[test]
#[ignore = "requires headless nvim"]
fn alternate_write_and_saveas_only_write_the_named_file() {
    check(
        r#"
diff()
api.nvim_buf_set_lines(0,0,-1,false,{"alternative"})
vim.cmd("write other")
assert(vim.fn.readfile("other")[1]=="alternative" and vim.fn.readfile("file")[1]=="laptop")
vim.cmd("saveas another")
assert(vim.fn.readfile("another")[1]=="alternative" and vim.fn.readfile("file")[1]=="laptop")
assert(not table.concat(vim.fn.readfile(base .. "/calls")):find("merge-write",1,true))"#,
    )
}

#[test]
#[ignore = "requires headless nvim"]
fn writing_the_original_buffer_after_wiping_the_server_diff_only_writes_the_laptop() {
    check(
        r#"diff()
vim.cmd("wincmd p")
vim.cmd.close()
assert(vim.fn.execute("messages"):find("writes now save only the laptop copy",1,true))
api.nvim_buf_set_lines(0,0,-1,false,{"after close"})
vim.cmd.write()
assert(vim.fn.readfile("file")[1]=="after close")
assert(not table.concat(vim.fn.readfile(base .. "/calls")):find("merge-write",1,true))"#,
    )
}

#[test]
#[ignore = "requires headless nvim"]
fn oversized_server_and_merged_buffer_are_refused() {
    check(
        r#"
vim.fn.writefile({""},base .. "/large-server")
view(); enter("^  Merge$")
wait(function() return vim.fn.execute("messages"):find("limited to 8 MiB",1,true) end)
assert(not vim.wo.diff)
vim.fn.delete(base .. "/large-server")
diff()
api.nvim_buf_set_lines(0,0,-1,false,{string.rep("x",8*1024*1024)})
local ok,msg=pcall(vim.cmd.write)
assert(not ok and msg:find("limited to 8 MiB",1,true),msg)
assert(vim.bo.modified and vim.fn.readfile("file")[1]=="laptop")
assert(not table.concat(vim.fn.readfile(base .. "/calls")):find("merge-write",1,true))"#,
    )
}

#[test]
#[ignore = "requires headless nvim"]
fn timeout_reports_whether_the_cli_confirmed_the_laptop_write() {
    check(
        r#"
diff()
local receipt=""
vim.system=function(args,opts)
 assert(args[4]=="merge-write")
 return {wait=function(_,timeout)
   assert(timeout==60000)
   return {code=124,stdout="",stderr=receipt}
 end}
end
for _,stage in ipairs({"before", "writing", "written"}) do
 local written=stage=="written"
 api.nvim_buf_set_lines(0,0,-1,false,{"edited"})
 receipt=written and ("eitri-sync-laptop-written " .. string.rep("a",64) .. "\n")
   or (stage=="writing" and "eitri-sync-laptop-writing\n" or "")
 local ok,msg=pcall(vim.cmd.write)
 assert(not ok and msg:find("outcome unknown",1,true),msg)
 assert(msg:find(written and "laptop file was written" or (stage=="writing" and "laptop write unconfirmed" or "laptop write did not start"),1,true),msg)
 assert(vim.bo.modified~=written)
end"#,
    )
}

mod script_fixture {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/executable.rs"));
}
