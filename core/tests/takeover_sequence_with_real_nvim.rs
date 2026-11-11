//! The sequence moves a real stdio TUI through a private tmux terminal, without a display.
#[path = "support/tmux_tui.rs"]
mod tmux_tui;

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use eitri_core::companion::{InstallError, InstallRefusal};
use eitri_core::nvim_rpc::{LinkEvent, NvimLink};
use eitri_core::takeover::glue::GlueHandle;
use eitri_core::takeover::room::{room_binary, OriginLink};
use eitri_core::takeover::sequence::{
    after_attach, check, check_with_uid_for_tests, move_tui, prepare, Checks, Moved, Prepared, Refusal, StepError,
};
use rmpv::Value;
use tmux_tui::{wait_until, TmuxTui};

const BOUND: Duration = Duration::from_secs(2);
const ROOM_TEXT: &str = "This nvim is open in Eitri.";
const EDIT: &str = "takeover unsaved edit";

fn nvim() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .find_map(|dir| room_binary(dir.join("nvim").to_str()?))
        .expect("nvim 0.12 is required")
}

fn old_nvim() -> PathBuf {
    std::env::var_os("EITRI_TEST_NVIM_0_11")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let dir = PathBuf::from(std::env::var_os("CARGO_TARGET_DIR").unwrap()).join("nvim-releases/0.11.2");
            assert!(std::process::Command::new(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../tools/nvim-releases/fetch.sh")
            )
            .arg("0.11.2")
            .arg(&dir)
            .status()
            .unwrap()
            .success());
            dir.join("bin/nvim")
        })
}

struct Case {
    tui: TmuxTui,
    link: NvimLink,
}
impl Case {
    fn start() -> Self {
        Self::with_binary(&nvim())
    }
    fn with_binary(binary: &Path) -> Self {
        let tui = TmuxTui::start(binary);
        let link = tui.link();
        tui.lua(&link, "vim.o.more = false; return true", vec![]);
        tui.lua(
            &link,
            "vim.api.nvim_buf_set_lines(0,0,-1,false,{...}); return true",
            vec![Value::from(EDIT)],
        );
        wait_until(|| tui.capture().contains(EDIT));
        Self { tui, link }
    }
    fn checks(&self) -> Checks {
        Checks {
            addr: self.tui.server_addr().to_str().unwrap().into(),
            takeover_on: true,
            display_ok: true,
        }
    }
    fn lua(&self, code: &str) -> Value {
        self.tui.lua(&self.link, code, vec![])
    }
    fn install(&self, origin: &OriginLink) -> Result<GlueHandle, InstallError> {
        self.tui.lua(&self.link,
            "local opts = ...; _G.__eitri_companion = {mode='takeover', chan=opts.chan, teardown=function() _G.__eitri_companion=nil end}; return true",
            vec![Value::Map(vec![(Value::from("chan"), Value::from(origin.chan()))])]);
        Ok(GlueHandle {
            link_chan: origin.chan(),
        })
    }
    fn prepared(&self) -> Prepared {
        prepare(check(&self.checks()).unwrap(), &self.tui.dir, &|o| self.install(o)).unwrap()
    }
    fn home(&self) {
        wait_until(|| self.tui.capture().contains(EDIT));
        assert_eq!(self.lua("return vim.bo.modified"), Value::from(true));
        assert_eq!(self.lua("return vim.fn.getpid()"), Value::from(self.tui.server_pid()));
        assert!(agent::process_probe::pid_is_alive(self.tui.tui_pid()));
    }
    fn room_stopped(&self) {
        wait_until(|| {
            std::fs::read_dir(&self.tui.dir)
                .unwrap()
                .flatten()
                .all(|e| !e.file_name().to_string_lossy().starts_with("takeover-"))
        });
    }
}

fn error<T>(result: Result<T, StepError>, expected: StepError) {
    let actual = result.err().expect("the operation must refuse");
    assert_eq!(actual.exit_code(), expected.exit_code());
    assert_eq!(actual, expected);
}

fn release(p: Prepared) {
    let _ = p.room.let_go();
    drop(p);
}

#[test]
#[ignore = "needs nvim 0.12, nvim 0.11.2 and tmux"]
fn each_refusal_leaves_the_tui_home() {
    let c = Case::start();
    for refusal in [
        Refusal::NotASocket,
        Refusal::OtherUid,
        Refusal::Off,
        Refusal::NotOneTui,
        Refusal::AlreadyTaken,
        Refusal::NoDisplay,
        Refusal::Blocking,
    ] {
        let mut checks = c.checks();
        match refusal {
            Refusal::NotASocket => checks.addr = c.tui.dir.to_str().unwrap().into(),
            Refusal::OtherUid => (),
            Refusal::Off => checks.takeover_on = false,
            Refusal::NoDisplay => checks.display_ok = false,
            Refusal::AlreadyTaken => {
                c.lua("_G.__eitri_companion = { mode='takeover' }; return true");
            }
            Refusal::NotOneTui => {
                attach(&c.link);
            }
            Refusal::Blocking => {
                c.tui.send_keys("f");
                wait_blocked(&c.link);
            }
            _ => unreachable!(),
        }
        let before = c.tui.capture();
        let result = if refusal == Refusal::OtherUid {
            check_with_uid_for_tests(&checks, c.tui.server_addr().metadata().unwrap().uid().wrapping_add(1))
        } else {
            check(&checks)
        };
        error(result, StepError::Refused(refusal.clone()));
        assert_eq!(c.tui.capture(), before, "a refusal must leave the terminal unchanged");
        match refusal {
            Refusal::AlreadyTaken => {
                c.lua("_G.__eitri_companion=nil; return true");
            }
            Refusal::NotOneTui => {
                c.link.call("nvim_ui_detach", vec![]).wait(BOUND).unwrap().unwrap();
            }
            Refusal::Blocking => c.tui.send_keys("Escape"),
            _ => (),
        }
        c.home();
    }
    let c = Case::with_binary(&old_nvim());
    let before = c.tui.capture();
    error(check(&c.checks()), StepError::Refused(Refusal::TooOld("0.11.2".into())));
    assert_eq!(c.tui.capture(), before);
    c.home();
}

fn attach(link: &NvimLink) {
    link.call(
        "nvim_ui_attach",
        vec![
            Value::from(150),
            Value::from(32),
            Value::Map(vec![(Value::from("rgb"), Value::from(true))]),
        ],
    )
    .wait(BOUND)
    .unwrap()
    .unwrap();
}

fn wait_blocked(link: &NvimLink) {
    wait_until(|| {
        link.call("nvim_get_mode", vec![])
            .wait(Duration::from_millis(100))
            .and_then(Result::ok)
            .is_none_or(|v| {
                v.as_map().is_some_and(|m| {
                    m.iter().any(|(k, v)| {
                        (k.as_str() == Some("blocking") && v.as_bool() == Some(true))
                            || (k.as_str() == Some("mode")
                                && v.as_str().is_some_and(|s| s.starts_with('r') || s == "!"))
                    })
                })
            })
    });
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn an_error_answer_to_the_move_quits_the_room_and_exits_one() {
    let c = Case::start();
    let p = c.prepared();
    let socket = p.dir.socket().to_owned();
    c.lua("vim.cmd.connect = function() error('takeover move refused') end; return true");
    let e = move_tui(&p).err().unwrap();
    assert!(matches!(e, StepError::MoveError(_)));
    assert_eq!(e.exit_code(), 1);
    wait_until(|| !socket.exists());
    c.home();
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn an_unanswered_move_then_eitri_gone_then_the_queued_move_sends_the_tui_back() {
    let c = Case::start();
    let p = c.prepared();
    let fifo = c.tui.dir.join("move-release");
    let name = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: the path is NUL-terminated and remains live for the call.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    c.lua(&format!("local connect = vim.cmd.connect; vim.cmd.connect = function(...) vim.fn.readfile('{}'); return connect(...) end; return true", fifo.display()));
    error(move_tui(&p), StepError::MoveUnanswered);
    let socket = p.dir.socket().to_owned();
    drop(p);
    std::fs::write(&fifo, b"").unwrap();
    wait_until(|| !socket.exists());
    c.home();
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn a_tui_that_did_not_leave_is_let_go_and_exits_one() {
    let c = Case::start();
    let p = c.prepared();
    c.lua("vim.cmd.connect = function() end; return true");
    error(move_tui(&p), StepError::TuiStayed);
    c.home();
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn a_second_ui_after_attach_detaches_and_exits_one() {
    let c = Case::start();
    let p = c.prepared();
    assert!(matches!(move_tui(&p).unwrap(), Moved::Landed));
    let editor = c.tui.link();
    attach(&editor);
    let other = c.tui.link();
    attach(&other);
    error(after_attach(&p, editor.channel_id()), StepError::OtherUi);
    wait_until(|| !editor.is_alive());
    assert!(other.is_alive());
    drop(p);
    c.home();
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn a_room_killed_after_the_move_keeps_the_attach_and_reports_the_way_back_gone() {
    let c = Case::start();
    let p = c.prepared();
    assert!(matches!(move_tui(&p).unwrap(), Moved::Landed));
    let editor = c.tui.link();
    attach(&editor);
    let room = NvimLink::connect(p.dir.socket(), BOUND).unwrap().0;
    let pid = room
        .exec_lua("return vim.fn.getpid()", vec![])
        .wait(BOUND)
        .unwrap()
        .unwrap()
        .as_u64()
        .unwrap() as u32;
    let start = eitri_core::split::proc_start_time(pid);
    assert!(start.is_some());
    // The room is the child this test's prepare started, identified by its start time.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
    drop(room);
    wait_until(|| p.room.held().is_err());
    error(after_attach(&p, editor.channel_id()), StepError::WayBackGone);
    assert!(editor.is_alive());
    let uis = c.lua("return vim.tbl_map(function(u) return u.chan end, vim.api.nvim_list_uis())");
    assert_eq!(uis, Value::Array(vec![Value::from(editor.channel_id())]));
    editor.close();
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn an_origin_that_turns_busy_before_prepare_is_a_refusal_before_anything_moved() {
    let c = Case::start();
    let origin = check(&c.checks()).unwrap();
    c.link.input("f").wait(BOUND).unwrap().unwrap();
    wait_until(|| {
        c.link
            .call("nvim_get_mode", vec![])
            .wait(Duration::from_millis(500))
            .and_then(Result::ok)
            .and_then(|mode| mode.as_map().cloned())
            .is_some_and(|fields| {
                fields
                    .iter()
                    .any(|(k, v)| k.as_str() == Some("blocking") && v.as_bool() == Some(true))
            })
    });
    let result = prepare(origin, &c.tui.dir, &|o| c.install(o));
    error(result, StepError::Refused(Refusal::Blocking));
    c.tui.send_keys("Escape");
    c.home();
    c.room_stopped();
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn the_project_directory_is_the_global_one_not_a_windows_local_directory() {
    let c = Case::start();
    let global = c.lua("return vim.fn.getcwd(-1, -1)");
    let local = c.tui.dir.join("local-dir");
    std::fs::create_dir(&local).unwrap();
    c.tui.lua(
        &c.link,
        "vim.cmd.lcd(...); return true",
        vec![Value::from(local.to_str().unwrap())],
    );
    assert_ne!(c.lua("return vim.fn.getcwd()"), global);
    let p = c.prepared();
    assert_eq!(Value::from(p.cwd.to_str().unwrap()), global);
    release(p);
    c.home();
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn a_second_takeover_racing_the_first_is_refused_with_64_before_anything_moves() {
    let c = Case::start();
    let first = check(&c.checks()).unwrap();
    let second = check(&c.checks()).unwrap();
    let p = prepare(first, &c.tui.dir, &|o| c.install(o)).unwrap();
    let before = c.tui.capture();
    error(
        prepare(second, &c.tui.dir, &|_| {
            Err(InstallError::Refused(InstallRefusal::Takeover))
        }),
        StepError::Refused(Refusal::AlreadyTaken),
    );
    assert_eq!(c.tui.capture(), before);
    assert_eq!(
        std::fs::read_dir(&c.tui.dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("takeover-"))
            .count(),
        1
    );
    assert!(matches!(move_tui(&p).unwrap(), Moved::Landed));
    release(p);
    c.home();
    c.room_stopped();
}

#[derive(Clone, Copy, Debug)]
enum Busy {
    Find,
    HitEnter,
    Shell,
    Confirm,
}
impl Busy {
    fn keys(self) -> &'static str {
        match self {
            Self::Find => "f",
            Self::HitEnter => r#":echo join(repeat(['takeover prompt'], 80), "\n")"#,
            Self::Shell => ":!read",
            Self::Confirm => ":call confirm('takeover confirm', \"&Yes\\n&No\", 2)",
        }
    }
    fn enter(self, c: &Case) {
        // Input must come from the terminal UI: nvim uses the last input channel for connect.
        if matches!(self, Self::Shell) {
            let fifo = c.tui.dir.join("shell-release");
            let name = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
            // SAFETY: the path is NUL-terminated and remains live for the call.
            assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            c.tui.send_keys(&format!(":!read ignored < '{}'", fifo.display()));
        } else {
            c.tui.send_keys(self.keys());
        }
        if self.keys().starts_with(':') {
            c.tui.send_keys("Enter");
        }
        wait_blocked(&c.link);
    }
    fn answer(self, c: &Case) {
        if matches!(self, Self::Shell) {
            std::fs::write(c.tui.dir.join("shell-release"), b"release\n").unwrap();
        }
        c.tui
            .send_keys(if matches!(self, Self::Confirm) { "n" } else { "Escape" });
    }
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn each_step_with_the_origin_waiting_for_a_key_leaves_the_tui_home_or_in_the_room() {
    for busy in [Busy::Find, Busy::HitEnter, Busy::Shell, Busy::Confirm] {
        for point in 0..3 {
            let c = Case::start();
            if point == 0 {
                busy.enter(&c);
                let before = c.tui.capture();
                error(check(&c.checks()), StepError::Refused(Refusal::Blocking));
                assert_eq!(c.tui.capture(), before, "{busy:?}");
                busy.answer(&c);
            } else {
                let p = c.prepared();
                if point == 1 {
                    busy.enter(&c);
                    error(move_tui(&p), StepError::Refused(Refusal::Blocking));
                    busy.answer(&c);
                    drop(p);
                } else {
                    // Hold the room's moving answer until the origin enters its wait, after the re-check.
                    let (room, notices) = NvimLink::connect(p.dir.socket(), BOUND).unwrap();
                    room.exec_lua(
                        "local opts = ...; local moving = _G.__eitri_room.moving; _G.__eitri_room.moving = function() moving(); vim.rpcnotify(opts.chan, 'takeover_moving'); assert(vim.wait(1500, function() return _G.takeover_release == true end, 10)); return true end; return true",
                        vec![Value::Map(vec![(Value::from("chan"), Value::from(room.channel_id()))])])
                        .wait(BOUND).unwrap().unwrap();
                    let (result, before) = std::thread::scope(|scope| {
                        let moving = scope.spawn(|| move_tui(&p));
                        assert!(matches!(notices.recv_timeout(BOUND).unwrap(),
                            LinkEvent::Notification { method, .. } if method == "takeover_moving"));
                        busy.enter(&c);
                        let before = c.tui.capture();
                        room.exec_lua("_G.takeover_release = true; return true", vec![])
                            .wait(BOUND)
                            .unwrap()
                            .unwrap();
                        (moving.join().unwrap(), before)
                    });
                    // A confirm dialog services Lua requests, so the terminal can move while it waits.
                    if matches!(busy, Busy::Confirm) {
                        assert_eq!(result.unwrap(), Moved::Landed);
                    } else {
                        error(result, StepError::MoveUnanswered);
                    }
                    let screen = c.tui.capture();
                    assert!(screen == before || screen.contains(ROOM_TEXT), "{busy:?}: {screen}");
                    if matches!(busy, Busy::Confirm) {
                        let socket = p.dir.socket().to_owned();
                        release(p);
                        wait_until(|| !socket.exists());
                        wait_until(|| c.tui.capture().contains("takeover confirm"));
                    } else {
                        drop(p);
                    }
                    busy.answer(&c);
                }
            }
            c.home();
        }
    }
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn a_move_without_the_ui_notice_attaches_with_the_terminal_warning() {
    let c = Case::start();
    let p = c.prepared();
    let room = NvimLink::connect(p.dir.socket(), BOUND).unwrap().0;
    room.exec_lua(
        "vim.api.nvim_clear_autocmds({event='UIEnter', group='EitriTakeoverRoom'}); return true",
        vec![],
    )
    .wait(BOUND)
    .unwrap()
    .unwrap();
    assert_eq!(move_tui(&p).unwrap(), Moved::NotLanded);
    let editor = c.tui.link();
    attach(&editor);
    after_attach(&p, editor.channel_id()).unwrap();
    assert_eq!(
        room.exec_lua("return _G.__eitri_room.is_held", vec![])
            .wait(BOUND)
            .unwrap()
            .unwrap(),
        Value::from(true)
    );
    editor.close();
    drop(room);
    release(p);
    c.home();
    c.room_stopped();
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn a_late_install_answer_stops_the_room_without_moving_the_terminal() {
    let c = Case::start();
    let origin = check(&c.checks()).unwrap();
    let result = eitri_core::takeover::sequence::prepare_with_elapsed_for_tests(
        origin,
        &c.tui.dir,
        &|o| Ok(GlueHandle { link_chan: o.chan() }),
        Duration::from_millis(2050),
    );
    let error = result.err().expect("a late install must fail");
    assert!(matches!(error, StepError::Install(_)));
    assert_eq!(error.exit_code(), 1);
    c.home();
    c.room_stopped();
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn a_lost_move_answer_keeps_the_room_for_the_tui() {
    let c = Case::start();
    let p = c.prepared();
    c.tui.lua(&c.link,
        "local opts = ...; local connect = vim.cmd.connect; vim.cmd.connect = function(...) vim.fn.chanclose(opts.chan); return connect(...) end; return true",
        vec![Value::Map(vec![(Value::from("chan"), Value::from(p.origin.chan()))])]);
    error(move_tui(&p), StepError::MoveUnanswered);
    let socket = p.dir.socket().to_owned();
    drop(p);
    wait_until(|| !socket.exists());
    c.home();
}
