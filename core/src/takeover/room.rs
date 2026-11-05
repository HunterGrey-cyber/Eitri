use std::fmt;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

use rmpv::Value;

use super::sequence::StepError;

use crate::nvim_rpc::{LinkEvent, NvimLink};

/// A connection to the terminal's nvim, separate from the waiting room.
pub struct OriginLink {
    pub(super) link: NvimLink,
    events: Mutex<Option<mpsc::Receiver<LinkEvent>>>,
}

/// A connection to the waiting room, separate from the terminal's nvim.
pub struct RoomLink {
    link: NvimLink,
    events: Mutex<Option<mpsc::Receiver<RoomEvent>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    Connect(String),
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for LinkError {}

impl OriginLink {
    pub fn connect(addr: &str, bound: Duration) -> Result<OriginLink, LinkError> {
        let (link, events) = NvimLink::connect(Path::new(addr), bound).map_err(LinkError::Connect)?;
        Ok(Self {
            link,
            events: Mutex::new(Some(events)),
        })
    }

    pub fn chan(&self) -> u64 {
        self.link.channel_id()
    }

    /// The connection itself, for the window's own requests to the person's nvim once it holds it: the detach's
    /// teardown and what a close asks. The room is never reached through it.
    pub fn rpc(&self) -> &NvimLink {
        &self.link
    }

    /// Notifications stay with the origin connection rather than crossing into the room's namespace.
    pub fn events(&self) -> mpsc::Receiver<LinkEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap_or_else(|| mpsc::channel().1)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoomEvent {
    Ui(bool),
    TakeBack,
    OriginUnreachable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LetGo {
    Returned,
    NoUi,
}

pub(crate) const ROOM_LUA: &str = include_str!("waiting_room.lua");
pub(crate) const ROOM_OP_LUA: &str =
    "local room = _G.__eitri_room; if not room then error('no takeover room') end; return room[(...)]()";
const ANSWER_WITHIN: Duration = Duration::from_secs(2);
const START_WITHIN: Duration = Duration::from_secs(5);

/// A private directory belonging to one waiting room. It is removed after the room exits.
pub struct RoomDir {
    path: PathBuf,
    socket: PathBuf,
}

impl RoomDir {
    pub fn create(runtime: &Path) -> io::Result<Self> {
        agent::private_fs::require_safe_parent(runtime, "the takeover room")?;
        let name = format!("takeover-{}", &uuid::Uuid::new_v4().simple().to_string()[..16]);
        let path = runtime.join(name);
        let socket = agent::socket_path::in_dir(&path, "w.sock")?;
        agent::private_fs::create_private_dir_all(&path, &path)?;
        Ok(Self { path, socket })
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn write_pid(&self, pid: u32, start_time: u64) -> io::Result<()> {
        agent::private_fs::write_private(&self.path.join("room.pid"), format!("{pid} {start_time}\n").as_bytes())
    }

    pub fn remove(self) -> io::Result<()> {
        std::fs::remove_dir_all(self.path)
    }
}

/// Remove only private room directories whose recorded process is provably gone or replaced.
pub fn sweep_stale(runtime: &Path) {
    if agent::private_fs::require_safe_parent(runtime, "the takeover room").is_err() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(runtime) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let Some(suffix) = name.to_str().and_then(|name| name.strip_prefix("takeover-")) else {
            continue;
        };
        if suffix.len() != 16 || !suffix.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let Ok(meta) = path.symlink_metadata() else { continue };
        if !meta.is_dir() || meta.uid() != crate::panel_control::euid() || meta.mode() & 0o777 != 0o700 {
            continue;
        }
        let Ok(record) = agent::private_fs::read_private_nofollow_capped(&path.join("room.pid"), 128) else {
            continue;
        };
        let mut words = record.split_whitespace();
        let Some(pid) = words.next().and_then(|p| p.parse::<u32>().ok()) else {
            continue;
        };
        let Some(start) = words.next().and_then(|p| p.parse::<u64>().ok()) else {
            continue;
        };
        if words.next().is_some() {
            continue;
        }
        let gone = !agent::process_probe::pid_is_alive(pid);
        let replaced = crate::split::proc_start_time(pid).is_some_and(|now| start != 0 && now != start);
        if gone || replaced {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

/// The room uses the origin's executable, never a different nvim found through PATH.
pub fn room_binary(progpath: &str) -> Option<PathBuf> {
    let path = Path::new(progpath);
    if !path.is_absolute() {
        return None;
    }
    let meta = path.metadata().ok()?;
    let name = std::ffi::CString::new(progpath).ok()?;
    // SAFETY: name is NUL-terminated, and access only checks this process's permission to execute.
    let executable = unsafe { libc::access(name.as_ptr(), libc::X_OK) == 0 };
    (meta.is_file() && executable).then(|| path.to_path_buf())
}

fn field<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
    value
        .as_map()?
        .iter()
        .find(|(key, _)| key.as_str() == Some(name))
        .map(|(_, value)| value)
}

/// Synchronous calls on the room are bounded and must run on worker threads.
impl RoomLink {
    /// Starts and checks an owned child. This bounded operation belongs on a worker thread.
    pub fn start(binary: &Path, dir: &RoomDir, origin_version: (u64, u64)) -> Result<(Self, u32), StepError> {
        let deadline = Instant::now() + START_WITHIN;
        let mut command = Command::new(binary);
        command
            .args(["--clean", "--headless", "-n", "-i", "NONE", "--listen"])
            .arg(dir.socket())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: setsid is async-signal-safe; the child has not started any threads.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().map_err(|e| StepError::RoomStart(e.to_string()))?;
        let pid = child.id();
        let started = (|| {
            dir.write_pid(pid, crate::split::proc_start_time(pid).unwrap_or(0))
                .map_err(|e| e.to_string())?;
            let (link, events) = loop {
                if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                    return Err(format!("the takeover room exited: {status}"));
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err("the takeover room did not answer within 5 s".into());
                }
                match NvimLink::connect(dir.socket(), remaining) {
                    Ok(connection) => break connection,
                    Err(error) => {
                        if Instant::now() >= deadline {
                            return Err(error);
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            };
            let api = link
                .call("nvim_get_api_info", vec![])
                .wait(deadline.saturating_duration_since(Instant::now()))
                .ok_or_else(|| "the takeover room did not answer within 5 s".to_string())?
                .map_err(|e| e.to_string())?;
            let version = api
                .as_array()
                .and_then(|a| a.get(1))
                .and_then(|v| field(v, "version"))
                .ok_or_else(|| "the takeover room gave no version".to_string())?;
            let major = field(version, "major").and_then(Value::as_u64);
            let minor = field(version, "minor").and_then(Value::as_u64);
            if major.zip(minor) != Some(origin_version) || origin_version < (0, 12) {
                link.close();
                return Err("the takeover room needs the origin's major.minor version, at least 0.12".into());
            }
            let (tx, rx) = mpsc::channel();
            std::thread::Builder::new()
                .name("takeover-room-events".into())
                .spawn(move || {
                    for event in events {
                        let event = match event {
                            LinkEvent::Notification { method, params } => match method.as_str() {
                                "ui" => params.first().and_then(Value::as_bool).map(RoomEvent::Ui),
                                "take_back" => Some(RoomEvent::TakeBack),
                                "origin_unreachable" => Some(RoomEvent::OriginUnreachable),
                                _ => None,
                            },
                            LinkEvent::Closed => break,
                        };
                        if let Some(event) = event {
                            let _ = tx.send(event);
                        }
                    }
                })
                .map_err(|e| e.to_string())?;
            Ok(Self {
                link,
                events: Mutex::new(Some(rx)),
            })
        })();
        match started {
            Ok(room) => {
                // Reap the child without keeping its control link alive; a lost link is a room event.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                Ok((room, pid))
            }
            Err(error) => {
                // This is the child we just spawned, never the origin's process.
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(dir.socket());
                Err(StepError::RoomStart(error))
            }
        }
    }

    /// The channel id in the room's namespace, for its notifications back to this connection.
    pub fn chan(&self) -> u64 {
        self.link.channel_id()
    }

    pub fn install(&self, origin: &str, eitri_chan: u64) -> Result<(), StepError> {
        if eitri_chan != self.chan() {
            return Err(StepError::Install(
                "the takeover room needs its own control channel".into(),
            ));
        }
        let answer = self
            .link
            .exec_lua(ROOM_LUA, vec![Value::from(origin), Value::from(eitri_chan)])
            .wait(ANSWER_WITHIN)
            .ok_or_else(|| StepError::Install("the takeover room install did not answer".into()))?
            .map_err(|e| StepError::Install(e.to_string()))?;
        if answer != Value::from(true) {
            return Err(StepError::Install("the takeover room refused its install".into()));
        }
        Ok(())
    }

    fn operation(&self, name: &str) -> Result<Value, StepError> {
        self.link
            .exec_lua(ROOM_OP_LUA, vec![Value::from(name)])
            .wait(ANSWER_WITHIN)
            .ok_or_else(|| StepError::RoomStart(format!("the takeover room's {name} did not answer")))?
            .map_err(|e| StepError::RoomStart(e.to_string()))
    }

    pub fn moving(&self) -> Result<(), StepError> {
        self.operation("moving").map(|_| ())
    }

    pub fn pending(&self) -> Result<(), StepError> {
        self.operation("pending").map(|_| ())
    }

    pub fn held(&self) -> Result<(), StepError> {
        self.operation("held").map(|_| ())
    }

    pub fn let_go(&self) -> Result<LetGo, StepError> {
        match self.operation("let_go")?.as_str() {
            Some("returned") => Ok(LetGo::Returned),
            Some("no_ui") => Ok(LetGo::NoUi),
            _ => Err(StepError::RoomStart(
                "the takeover room refused to return its UI".into(),
            )),
        }
    }

    pub fn quit(&self) -> Result<(), StepError> {
        if self.operation("quit")? == Value::from(true) {
            Ok(())
        } else {
            Err(StepError::RoomStart("the takeover room refused to quit".into()))
        }
    }

    /// Notifications have one consumer. A later take returns an already closed receiver.
    pub fn events(&self) -> mpsc::Receiver<RoomEvent> {
        self.events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .unwrap_or_else(|| mpsc::channel().1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    const VIM: &str = r#"
        callbacks, scheduled, deferred, notices = {}, {}, {}, {}
        uis, channels, buffers = {}, {[9] = true, [2] = true}, {1}
        connected, quit_count, probe, fast = 0, 0, true, false
        vim = {
          v = { servername = '/room', vim_did_enter = 1 }, o = {},
          bo = {[1] = { buftype = '', modified = false }},
          api = {}, fn = {}, opt = {
            shortmess = { append = function() end },
            fillchars = { append = function() end, remove = function() end },
          },
          keymap = {}, uv = {}, cmd = {},
        }
        local api = vim.api
        api.nvim_create_namespace = function() return 1 end
        api.nvim_create_augroup = function() return 1 end
        api.nvim_del_augroup_by_id = function() callbacks = {} end
        api.nvim_get_chan_info = function(c)
          assert(not fast, 'channel checks need the main loop')
          return channels[c] and {id = c} or {}
        end
        api.nvim_list_bufs = function() return buffers end
        api.nvim_list_uis = function() return uis end
        api.nvim_get_current_buf = function() return 1 end
        api.nvim_buf_get_lines = function() return {''} end
        api.nvim_buf_get_name = function() return '' end
        api.nvim_buf_is_valid = function() return true end
        api.nvim_buf_set_lines = function() end
        api.nvim_create_autocmd = function(name, opts) callbacks[name] = opts.callback end
        vim.fn.sockconnect = function(_, address, opts)
          assert(address == '/origin' and opts.rpc)
          if not probe then return 0 end
          return 2
        end
        vim.fn.chanclose = function() end
        vim.uv.new_timer = function()
          return {
            start = function(_, first, repeat_ms, f)
              assert(first == 250 and repeat_ms == 250)
              tick_callback = f
            end,
            stop = function() end, close = function() end, is_closing = function() return false end,
          }
        end
        vim.cmd.redraw = function() end
        vim.cmd.qall = function() quit_count = quit_count + 1 end
        vim.cmd.connect = function(opts)
          assert(not fast and opts.bang and opts.args[1] == '/origin' and opts.magic.file == false)
          connected = connected + 1
          callbacks.UILeave()
        end
        vim.on_key = function(f) key_filter = f end
        vim.keymap.set = function(_, _, f) enter = f end
        vim.keymap.del = function() enter = nil end
        vim.rpcnotify = function(chan, method, ...)
          notices[#notices + 1] = { chan, method, ... }
        end
        vim.schedule = function(f) scheduled[#scheduled + 1] = f end
        vim.defer_fn = function(f, ms) assert(ms == 2000); deferred[#deferred + 1] = f end
        function flush()
          while #scheduled > 0 do table.remove(scheduled, 1)() end
        end
        function tick()
          fast = true; tick_callback(); fast = false; flush()
        end
    "#;

    fn room_lua() -> mlua::Lua {
        let lua = mlua::Lua::new();
        lua.load(VIM).exec().unwrap();
        assert!(lua.load(ROOM_LUA).call::<bool>(("/origin", 9)).unwrap());
        lua
    }

    #[test]
    fn takeover_return_requires_its_marker_address_and_only_unmodified_scratch_buffers() {
        room_lua()
            .load(
                r#"
            local room = _G.__eitri_room
            uis = {{chan = 1}}
            _G.__eitri_room = {}
            assert(not room.return_tui())
            _G.__eitri_room = room
            vim.v.servername = '/origin'
            assert(not room.return_tui())
            vim.v.servername = '/room'
            vim.bo[1].modified = true
            assert(not room.return_tui())
            vim.bo[1].modified = false
            vim.bo[2] = {buftype = '', modified = false}; buffers = {1, 2}
            assert(not room.return_tui())
            buffers = {1}
            assert(room.return_tui())
            flush()
            assert(connected == 1 and #notices == 0)
        "#,
            )
            .exec()
            .unwrap();
    }

    #[test]
    fn takeover_a_lost_host_waits_for_a_move_and_returns_the_late_ui_without_origin_requests() {
        room_lua()
            .load(
                r#"
            _G.__eitri_room.moving()
            _G.__eitri_room.pending()
            channels[9] = nil
            tick()
            assert(quit_count == 0 and connected == 0)
            uis = {{chan = 1}}
            tick()
            assert(connected == 1 and quit_count == 0)
            assert(#notices == 0)
        "#,
            )
            .exec()
            .unwrap();
        room_lua()
            .load("channels[9] = nil; tick(); assert(quit_count == 1)")
            .exec()
            .unwrap();
        room_lua()
            .load("channels[2] = nil; tick(); assert(quit_count == 1)")
            .exec()
            .unwrap();
    }

    /// A `let_go` that finds no UI (the terminal closed after the window last looked) leaves nothing to wait for: once
    /// Eitri's link closes, the room quits even after a move was sent. A UI reaching it before then is still returned.
    #[test]
    fn takeover_a_let_go_with_no_ui_quits_the_room_once_eitri_has_gone() {
        room_lua()
            .load(
                r#"
            local room = _G.__eitri_room
            room.moving()
            assert(room.let_go() == 'no_ui')
            tick()
            assert(quit_count == 0, 'the room quit while Eitri still holds its link')
            channels[9] = nil
            tick()
            assert(quit_count == 1 and connected == 0)
        "#,
            )
            .exec()
            .unwrap();
        room_lua()
            .load(
                r#"
            local room = _G.__eitri_room
            room.moving(); room.pending()
            assert(room.let_go() == 'no_ui')
            uis = {{chan = 1}}
            channels[9] = nil
            tick()
            assert(connected == 1 and quit_count == 0)
        "#,
            )
            .exec()
            .unwrap();
    }

    #[test]
    fn takeover_keys_keep_the_timer_runnable_and_enter_notifies_before_its_fallback() {
        room_lua()
            .load(
                r#"
            assert(key_filter('f', 'f') == '')
            assert(key_filter(':', ':') == '')
            assert(key_filter('\r', '\r') == nil)
            uis = {{chan = 1}}
            _G.__eitri_room.held()
            enter()
            assert(#notices == 2 and connected == 0)
            assert(notices[1][1] == 9 and notices[1][2] == 'take_back')
            assert(notices[2][1] == 2 and notices[2][2] == 'nvim_exec_lua')
            assert(notices[2][3] == 'return _G.__eitri_companion and _G.__eitri_companion.take_back()')
            assert(#notices[2][4] == 0)
            deferred[1](); flush()
            assert(connected == 1)
        "#,
            )
            .exec()
            .unwrap();
    }

    #[test]
    fn takeover_a_failed_probe_keeps_its_ui_and_notifies_once() {
        room_lua()
            .load(
                r#"
            uis = {{chan = 1}}; probe = false
            assert(_G.__eitri_room.let_go() == 'refused')
            assert(_G.__eitri_room.let_go() == 'refused')
            flush()
            assert(connected == 0 and #notices == 1)
            assert(notices[1][1] == 9 and notices[1][2] == 'origin_unreachable')
        "#,
            )
            .exec()
            .unwrap();
    }

    #[test]
    fn takeover_teardown_stops_already_scheduled_work() {
        room_lua()
            .load(
                r#"
            uis = {{chan = 1}}
            _G.__eitri_room.held(); enter()
            assert(_G.__eitri_room.return_tui())
            tick_callback()
            _G.__eitri_room.teardown()
            deferred[1](); flush()
            assert(connected == 0 and quit_count == 0 and _G.__eitri_room == nil)
        "#,
            )
            .exec()
            .unwrap();
    }

    #[test]
    fn takeover_room_paths_are_private_and_only_recorded_dead_rooms_are_swept() {
        let runtime =
            std::env::temp_dir().join(format!("takeover-{}", &uuid::Uuid::new_v4().simple().to_string()[..6]));
        agent::private_fs::create_private_dir_all(&runtime, &runtime).unwrap();
        let live = RoomDir::create(&runtime).unwrap();
        live.write_pid(
            std::process::id(),
            crate::split::proc_start_time(std::process::id()).unwrap_or(0),
        )
        .unwrap();
        let dead = RoomDir::create(&runtime).unwrap();
        dead.write_pid(u32::MAX, 0).unwrap();
        let unknown = RoomDir::create(&runtime).unwrap();
        let dead_path = dead.socket().parent().unwrap().to_path_buf();
        assert_eq!(live.socket(), agent::socket_path::in_dir(&live.path, "w.sock").unwrap());
        assert_eq!(
            live.socket().parent().unwrap().metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );
        sweep_stale(&runtime);
        assert!(live.socket().parent().unwrap().exists());
        assert!(unknown.socket().parent().unwrap().exists());
        assert!(!dead_path.exists());
        live.remove().unwrap();
        unknown.remove().unwrap();
        std::fs::remove_dir(runtime).unwrap();
    }

    #[test]
    fn takeover_room_binary_requires_an_absolute_regular_executable() {
        assert!(room_binary("nvim").is_none());
        assert!(room_binary("/").is_none());
        let file = std::env::temp_dir().join(format!("takeover-binary-{}", uuid::Uuid::new_v4().simple()));
        agent::private_fs::write_private(&file, b"a file").unwrap();
        assert!(room_binary(file.to_str().unwrap()).is_none());
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(room_binary(file.to_str().unwrap()).as_deref(), Some(Path::new(&file)));
        std::fs::remove_file(file).unwrap();
    }
}
