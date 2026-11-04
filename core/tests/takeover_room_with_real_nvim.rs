//! A private tmux terminal exercises the waiting room with nvim 0.12, without a display or user config.
#[path = "support/tmux_tui.rs"]
mod tmux_tui;

use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use eitri_core::nvim_rpc::{LinkEvent, NvimLink};
use eitri_core::takeover::room::{room_binary, sweep_stale, LetGo, OriginLink, RoomDir, RoomEvent, RoomLink};
use rmpv::Value;
use tmux_tui::{wait_until, TmuxTui};

const ROOM_LUA: &str = include_str!("../src/takeover/waiting_room.lua");
const LINE_ONE: &str =
    "This nvim is open in Eitri. It comes back here when Eitri lets go of it. <Enter> takes it back now.";
const LINE_TWO: &str = "If the screen is blank when it comes back, nvim is waiting for a key: press Esc.";

fn nvim() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").expect("PATH is set"))
        .find_map(|dir| room_binary(dir.join("nvim").to_str()?))
        .expect("nvim 0.12 must be installed")
}

struct Case {
    tui: TmuxTui,
    origin: NvimLink,
    origin_events: Receiver<LinkEvent>,
    room: Option<RoomLink>,
    dir: Option<RoomDir>,
    pid: u32,
    start: Option<u64>,
    events: Receiver<RoomEvent>,
}

impl Case {
    fn start(moved: bool) -> Self {
        Self::start_at(moved, None)
    }
    /// `origin_name` makes the origin listen on one more address, and the room is told that one.
    fn start_at(moved: bool, origin_name: Option<&str>) -> Self {
        let tui = TmuxTui::start(&nvim());
        let (origin, origin_events) = NvimLink::connect(tui.server_addr(), Duration::from_secs(2)).unwrap();
        tui.lua(
            &origin,
            "vim.api.nvim_buf_set_lines(0, 0, -1, false, {'takeover unsaved edit'}); return true",
            vec![],
        );
        let dir = RoomDir::create(&tui.dir).unwrap();
        let (room, pid) = RoomLink::start(&nvim(), &dir, (0, 12)).unwrap();
        let origin_addr = match origin_name {
            Some(name) => {
                let path = tui.dir.join(name);
                tui.lua(
                    &origin,
                    "return vim.fn.serverstart(...)",
                    vec![Value::from(path.to_str().unwrap())],
                );
                path.to_str().unwrap().to_owned()
            }
            None => tui.server_addr().to_str().unwrap().to_owned(),
        };
        room.install(&origin_addr, room.chan()).unwrap();
        let events = room.events();
        let mut case = Self {
            tui,
            origin,
            origin_events,
            room: Some(room),
            dir: Some(dir),
            pid,
            start: eitri_core::split::proc_start_time(pid),
            events,
        };
        if moved {
            case.move_tui();
        }
        case
    }
    fn room(&self) -> &RoomLink {
        self.room.as_ref().unwrap()
    }
    fn move_tui(&mut self) {
        self.room().moving().unwrap();
        self.origin
            .exec_lua(
                "vim.cmd.connect(...)",
                vec![Value::from(self.dir.as_ref().unwrap().socket().to_str().unwrap())],
            )
            .wait(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        self.event(RoomEvent::Ui(true));
        wait_until(|| self.tui.capture().contains(LINE_ONE));
    }
    fn event(&self, expected: RoomEvent) {
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            let got = self
                .events
                .recv_timeout(end.saturating_duration_since(Instant::now()))
                .expect("the room notified the host");
            if got == expected {
                return;
            }
        }
    }
    fn wait_for_key(&self) {
        self.origin.input("f").wait(Duration::from_secs(2)).unwrap().unwrap();
        wait_until(|| {
            self.origin
                .call("nvim_get_mode", vec![])
                .wait(Duration::from_millis(500))
                .and_then(Result::ok)
                .and_then(|value| value.as_map().cloned())
                .is_some_and(|fields| {
                    fields
                        .iter()
                        .any(|(key, value)| key.as_str() == Some("blocking") && value.as_bool() == Some(true))
                })
        });
    }
    fn returned(&self) {
        wait_until(|| self.tui.capture().contains("takeover unsaved edit"));
        assert_eq!(
            self.tui.lua(&self.origin, "return vim.fn.getpid()", vec![]).as_u64(),
            Some(self.tui.server_pid() as u64)
        );
        assert_eq!(
            self.tui.lua(&self.origin, "return vim.bo.modified", vec![]),
            Value::from(true)
        );
    }
    fn exited(&self) {
        wait_until(|| std::fs::read_to_string(format!("/proc/{}/stat", self.pid)).is_err());
    }
}
impl Drop for Case {
    fn drop(&mut self) {
        self.room.take();
        // The room is our child, and a failed assertion must not leave it behind.
        if self.start.is_some() && eitri_core::split::proc_start_time(self.pid) == self.start {
            unsafe {
                libc::kill(self.pid as libc::pid_t, libc::SIGKILL);
            }
            wait_until(|| eitri_core::split::proc_start_time(self.pid) != self.start);
        }
        if let Some(dir) = self.dir.take() {
            let _ = dir.remove();
        }
    }
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn the_room_shows_its_two_lines_and_no_prompt() {
    let c = Case::start(true);
    let screen = c.tui.capture();
    assert!(screen.contains(LINE_ONE) && screen.contains(LINE_TWO), "{screen}");
    assert!(
        !screen.contains("Press ENTER") && !screen.contains("-- More --"),
        "{screen}"
    );
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn typed_keys_in_the_room_are_discarded_and_let_go_is_still_answered() {
    for keys in ["f", ":qa", ":detach"] {
        let c = Case::start(true);
        c.room().held().unwrap();
        c.tui.send_keys(keys);
        if keys.starts_with(':') {
            c.tui.send_keys("Enter");
        }
        assert_eq!(c.room().let_go().unwrap(), LetGo::Returned);
        c.returned();
    }
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn let_go_with_a_ui_returns_the_tui_to_the_origin() {
    let c = Case::start(true);
    let origin = OriginLink::connect(c.tui.server_addr().to_str().unwrap(), Duration::from_secs(2)).unwrap();
    assert!(origin.chan() > 0);
    let tui_pid = c.tui.tui_pid();
    assert_eq!(c.room().let_go().unwrap(), LetGo::Returned);
    c.returned();
    assert!(agent::process_probe::pid_is_alive(tui_pid));
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn let_go_without_a_ui_answers_no_ui() {
    let c = Case::start(false);
    assert_eq!(c.room().let_go().unwrap(), LetGo::NoUi);
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn the_rooms_own_connect_bang_sends_no_ui_false() {
    let c = Case::start(true);
    assert_eq!(c.room().let_go().unwrap(), LetGo::Returned);
    c.returned();
    c.exited();
    assert_eq!(
        c.events.recv_timeout(Duration::from_secs(2)),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
    );
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn eitri_link_closing_after_moving_waits_then_returns_a_late_tui() {
    let mut c = Case::start(false);
    c.room().moving().unwrap();
    c.room().pending().unwrap();
    c.room.take();
    std::thread::sleep(Duration::from_millis(600));
    assert!(agent::process_probe::pid_is_alive(c.pid));
    c.origin
        .exec_lua(
            "vim.cmd.connect(...)",
            vec![Value::from(c.dir.as_ref().unwrap().socket().to_str().unwrap())],
        )
        .wait(Duration::from_secs(2))
        .unwrap()
        .unwrap();
    c.returned();
    c.exited();
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn eitri_link_closing_with_neither_moving_nor_pending_quits_the_room() {
    let mut c = Case::start(false);
    c.room.take();
    c.exited();
    c.returned();
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn quit_ends_the_room() {
    let c = Case::start(true);
    c.room().quit().unwrap();
    c.exited();
    wait_until(|| c.tui.capture().contains("takeover-shell>"));
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn link_killed_while_the_origin_waits_for_a_key_returns_after_the_key() {
    let mut c = Case::start(true);
    c.wait_for_key();
    c.room.take();
    c.exited();
    c.tui.send_keys("Escape");
    c.returned();
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn enter_in_the_room_notifies_take_back_and_returns_after_two_seconds() {
    let c = Case::start(true);
    c.tui.lua(
        &c.origin,
        "_G.__eitri_companion = { take_back = function() _G.takeover_called = true end }; return true",
        vec![],
    );
    c.room().held().unwrap();
    c.tui.send_keys("Enter");
    c.event(RoomEvent::TakeBack);
    wait_until(|| c.tui.lua(&c.origin, "return _G.takeover_called == true", vec![]) == Value::from(true));
    assert!(
        c.tui.capture().contains(LINE_ONE),
        "the held room waits for the host before the fallback"
    );
    c.returned();
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn return_tui_sent_to_an_nvim_with_an_unsaved_buffer_refuses() {
    let c = Case::start(false);
    let installed = c
        .origin
        .exec_lua(
            ROOM_LUA,
            vec![
                Value::from(c.dir.as_ref().unwrap().socket().to_str().unwrap()),
                Value::from(c.origin.channel_id()),
            ],
        )
        .wait(Duration::from_secs(2))
        .unwrap()
        .unwrap();
    assert_eq!(installed, Value::from(true));
    assert_eq!(
        c.tui.lua(&c.origin, "return _G.__eitri_room.return_tui()", vec![]),
        Value::from(false)
    );
    assert_eq!(c.tui.lua(&c.origin, "for _, b in ipairs(vim.api.nvim_list_bufs()) do if vim.bo[b].modified then return vim.api.nvim_buf_get_lines(b,0,-1,false)[1] end end", vec![]), Value::from("takeover unsaved edit"));
    assert!(agent::process_probe::pid_is_alive(c.tui.server_pid()));
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn the_probe_finds_an_origin_that_ran_serverstop() {
    let c = Case::start(true);
    c.tui.lua(
        &c.origin,
        "vim.fn.serverstop(...); return true",
        vec![Value::from(c.tui.server_addr().to_str().unwrap())],
    );
    assert!(c.room().let_go().is_err());
    c.event(RoomEvent::OriginUnreachable);
    wait_until(|| {
        c.tui.capture().contains(&format!(
            "nvim is no longer reachable at {}",
            c.tui.server_addr().display()
        ))
    });
    assert!(agent::process_probe::pid_is_alive(c.pid));
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn the_origin_quitting_quits_the_room() {
    let c = Case::start(true);
    c.origin.exec_lua("vim.bo.modified = false; vim.cmd.qall()", vec![]);
    c.exited();
    wait_until(|| c.tui.capture().contains("takeover-shell>"));
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn losing_the_ui_notifies_ui_false() {
    let c = Case::start(true);
    // This private TUI was created by the helper, and killing it models a closed terminal.
    unsafe {
        libc::kill(c.tui.tui_pid() as libc::pid_t, libc::SIGTERM);
    }
    c.event(RoomEvent::Ui(false));
    assert!(agent::process_probe::pid_is_alive(c.pid));
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn let_go_into_an_origin_waiting_for_a_key_shows_the_blank_screen_then_the_session() {
    let c = Case::start(true);
    c.wait_for_key();
    assert_eq!(c.room().let_go().unwrap(), LetGo::Returned);
    c.exited();
    wait_until(|| {
        let screen = c.tui.capture();
        // The origin cannot redraw until the pending character is answered. The TUI may show
        // its intro while attaching, or leave the cleared screen blank.
        screen.trim().is_empty() || (screen.contains("NVIM") && screen.contains(":q<Enter>"))
    });
    assert!(!c.tui.capture().contains("takeover unsaved edit"));
    let mode = c
        .origin
        .call("nvim_get_mode", vec![])
        .wait(Duration::from_millis(500))
        .expect("the origin answers its mode while waiting for a key")
        .unwrap();
    assert!(
        mode.as_map()
            .unwrap()
            .iter()
            .any(|(key, value)| { key.as_str() == Some("blocking") && value.as_bool() == Some(true) }),
        "returning the TUI leaves the pending character for the person to answer"
    );
    c.tui.send_keys("Escape");
    c.returned();
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn an_origin_address_with_percent_and_hash_is_connected_literally() {
    let c = Case::start_at(true, Some("a#b%c.sock"));
    assert_eq!(c.room().let_go().unwrap(), LetGo::Returned);
    c.exited();
    c.returned();
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn let_go_into_an_origin_busy_in_system_returns_after_the_call() {
    let c = Case::start(true);
    let call = c.origin.exec_lua(
        "vim.rpcnotify(..., 'takeover_busy'); vim.fn.system({'sleep','3'}); return true",
        vec![Value::from(c.origin.channel_id())],
    );
    assert_eq!(
        c.origin_events.recv_timeout(Duration::from_secs(2)).unwrap(),
        LinkEvent::Notification {
            method: "takeover_busy".into(),
            params: vec![],
        }
    );
    assert_eq!(c.room().let_go().unwrap(), LetGo::Returned);
    c.exited();
    call.wait(Duration::from_secs(5)).unwrap().unwrap();
    c.returned();
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn a_confirm_dialog_open_at_let_go_is_still_there_after_the_return() {
    let c = Case::start(true);
    // A dialog opened without a UI answers its default instead of waiting for the person.
    c.origin
        .call(
            "nvim_ui_attach",
            vec![
                Value::from(150),
                Value::from(32),
                Value::Map(vec![(Value::from("rgb"), Value::from(true))]),
            ],
        )
        .wait(Duration::from_secs(2))
        .expect("the origin attaches the editor UI")
        .unwrap();
    let confirm = c
        .origin
        .exec_lua("return vim.fn.confirm('takeover confirm', '&Yes\\n&No', 2)", vec![]);
    wait_until(|| {
        c.tui
            .lua(&c.origin, "return vim.api.nvim_get_mode().mode", vec![])
            .as_str()
            == Some("r?")
    });
    c.origin
        .call("nvim_ui_detach", vec![])
        .wait(Duration::from_secs(2))
        .expect("the editor UI detaches while the dialog waits")
        .unwrap();
    assert!(confirm.try_take().is_none());
    assert_eq!(c.room().let_go().unwrap(), LetGo::Returned);
    c.exited();
    assert!(confirm.try_take().is_none());
    wait_until(|| c.tui.capture().contains("takeover confirm"));
    c.tui.send_keys("n");
    assert_eq!(confirm.wait(Duration::from_secs(3)).unwrap().unwrap(), Value::from(2));
    c.returned();
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn a_room_of_another_major_minor_is_refused() {
    let tui = TmuxTui::start(&nvim());
    let dir = RoomDir::create(&tui.dir).unwrap();
    assert!(RoomLink::start(&nvim(), &dir, (0, 13)).is_err());
    let old = std::env::var_os("EITRI_TEST_NVIM_0_11")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let target = std::env::var_os("CARGO_TARGET_DIR")
                .map(PathBuf::from)
                .expect("CARGO_TARGET_DIR is set");
            let path = target.join("nvim-releases/0.11.2");
            assert!(std::process::Command::new(
                Path::new(env!("CARGO_MANIFEST_DIR")).join("../tools/nvim-releases/fetch.sh")
            )
            .arg("0.11.2")
            .arg(&path)
            .status()
            .unwrap()
            .success());
            path.join("bin/nvim")
        });
    assert!(RoomLink::start(&old, &dir, (0, 11)).is_err());
    dir.remove().unwrap();
}

#[test]
#[ignore = "needs nvim 0.12 and tmux"]
fn sweep_takes_only_dead_rooms() {
    let c = Case::start(false);
    let live = c.dir.as_ref().unwrap().socket().parent().unwrap().to_path_buf();
    let dead = RoomDir::create(&c.tui.dir).unwrap();
    dead.write_pid(u32::MAX, 0).unwrap();
    let dead_path = dead.socket().parent().unwrap().to_path_buf();
    let reused = RoomDir::create(&c.tui.dir).unwrap();
    reused.write_pid(std::process::id(), u64::MAX).unwrap();
    let reused_path = reused.socket().parent().unwrap().to_path_buf();
    sweep_stale(&c.tui.dir);
    assert!(live.exists());
    assert!(!dead_path.exists() && !reused_path.exists());
}
