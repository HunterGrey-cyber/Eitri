//! `eitri.config.get(key)` / `eitri.config.set(key, value)`.
//!
//! v1 scope, deliberately minimal: in-memory only (nothing persists across a restart), and
//! values are strings or booleans (no tables; `nil` unsets a key; anything else is refused, see
//! `ConfigStore::refused`). A number is stored as its text, as Lua's own string
//! coercion gives it. A boolean stays a boolean, so a switch can be written
//! `eitri.config.set("key", false)` and `eitri.config.get("key")` gives `false` back: storing the
//! word instead would hand Lua the string `"false"`, which Lua counts as true. Rust reads a
//! boolean as the word `"true"` or `"false"`.

use mlua::{FromLua, IntoLua, Lua, Table, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// `pub`: `Kernel::new` constructs one directly
/// (`config::ConfigStore::default()`) to hand to `install`.
#[derive(Default)]
pub struct ConfigStore {
    values: HashMap<String, Stored>,
    /// Where `init.lua` set each key (`file:line`), for the keys it set from a file.
    origins: HashMap<String, String>,
    /// Shares startup validation with the other Lua APIs.
    init_refusals: Option<Rc<RefCell<super::kernel::InitRefusals>>>,
}

/// One stored value: the text of a string or number, or a boolean kept as one.
enum Stored {
    Text(String),
    Switch(bool),
}

impl Stored {
    fn as_str(&self) -> &str {
        match self {
            Stored::Text(text) => text,
            Stored::Switch(true) => "true",
            Stored::Switch(false) => "false",
        }
    }
}

impl ConfigStore {
    /// Reads a key from Rust. Added 2026-09-21 for `agent.account`: `init.lua` is where the host
    /// is configured, and the host has to be able to read what it was configured with. This is the
    /// same store `eitri.config.get` reads, not a parallel one -- a plugin and the shell see one
    /// value for one key, which is the only arrangement that cannot drift.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(Stored::as_str)
    }

    /// Where `init.lua` set `key`, as `file:line`, when it did so from a file.
    pub fn origin(&self, key: &str) -> Option<&str> {
        self.origins.get(key).map(String::as_str)
    }

    /// Takes `key` out, so what reads it finds it unset and uses its default. Nothing else changes.
    pub fn unset(&mut self, key: &str) {
        self.values.remove(key);
        self.origins.remove(key);
    }

    /// Every key starting with `prefix` and its value as [`ConfigStore::get`] reads it, in key order, so a
    /// reader that stops at the first bad key names the same one on every start.
    pub fn keys_under(&self, prefix: &str) -> Vec<(String, String)> {
        let mut keys: Vec<(String, String)> = self
            .values
            .iter()
            .filter(|(key, _)| key.starts_with(prefix))
            .map(|(key, value)| (key.clone(), value.as_str().to_owned()))
            .collect();
        keys.sort();
        keys
    }

    /// Every key with the value [`Self::get`] gives for it, sorted by key.
    pub fn entries(&self) -> Vec<(String, String)> {
        let mut entries: Vec<(String, String)> = self
            .values
            .iter()
            .map(|(key, value)| (key.clone(), value.as_str().to_string()))
            .collect();
        entries.sort();
        entries
    }

    /// Why `eitri.config.set` refused to store `key`, when `init.lua` tried and was refused (a table, a function, a
    /// string that is not UTF-8). The key then reads as unset, which for most settings is its default.
    pub fn refusal_for(&self, key: &str) -> Option<String> {
        self.init_refusals
            .as_ref()
            .and_then(|shared| shared.borrow().refusal_for("eitri.config.set", key))
    }

    /// The keys starting with `prefix` that `eitri.config.set` refused to store, with why.
    pub fn refusals_under(&self, prefix: &str) -> Vec<(String, String)> {
        self.init_refusals
            .as_ref()
            .map(|shared| shared.borrow().refusals_under("eitri.config.set", prefix))
            .unwrap_or_default()
    }

    /// Stores `value` as the text of `key`, as `eitri.config.set` would, for the host to put a stricter value where
    /// one was refused.
    pub fn set_text(&mut self, key: &str, value: &str) {
        self.origins.remove(key);
        self.values.insert(key.to_string(), Stored::Text(value.to_string()));
    }

    /// The first set refusal recorded at the Lua function boundary during init.
    /// Keeping it outside Lua makes a caught error visible to startup validation.
    pub fn refused(&self) -> Option<String> {
        self.init_refusals
            .as_ref()
            .and_then(|shared| shared.borrow().message("eitri.config.set"))
    }
}

/// The `init.lua` key: `eitri.config.set("window.restore", true | false)`. Whether the window that
/// holds a pair brings the second window back at start when it was open at the pair's last end, and
/// whether window 1 of a compositor group reopens the recorded windows, their outputs and its terminal.
pub const WINDOW_RESTORE_KEY: &str = "window.restore";

/// [`WINDOW_RESTORE_KEY`] as stored. Unset means they come back: they are part of the arrangement
/// the user left, as the workspaces are.
pub fn parse_window_restore(value: Option<&str>) -> Result<bool, String> {
    crate::config_switch::parse_switch(WINDOW_RESTORE_KEY, value, true)
}

/// What a desktop launch with no path does. Read only by `shell choose-project`.
pub const DESKTOP_START_KEY: &str = "window.desktop_start";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopStart {
    /// The project chooser.
    Choose,
    /// The newest project known to be present, with no window first; the chooser if there is none.
    Last,
    /// The launcher's own directory.
    Cwd,
}

/// [`DESKTOP_START_KEY`] as stored. Unset means the chooser.
pub fn parse_desktop_start(value: Option<&str>) -> Result<DesktopStart, String> {
    match value {
        None | Some("choose") => Ok(DesktopStart::Choose),
        Some("last") => Ok(DesktopStart::Last),
        Some("cwd") => Ok(DesktopStart::Cwd),
        Some(value) => Err(format!(
            "{DESKTOP_START_KEY:?} must be choose, last or cwd (got {value:?})"
        )),
    }
}

/// Whether the editor reopens the files from its last run.
pub const EDITOR_RESTORE_KEY: &str = "editor.restore";

pub fn parse_editor_restore(value: Option<&str>) -> Result<bool, String> {
    crate::config_switch::parse_switch(EDITOR_RESTORE_KEY, value, true)
}

/// Whether Eitri may take over a terminal nvim when explicitly asked.
pub const EDITOR_TAKEOVER_KEY: &str = "editor.takeover";

pub fn parse_editor_takeover(value: Option<&str>) -> Result<bool, String> {
    crate::config_switch::parse_switch(EDITOR_TAKEOVER_KEY, value, true)
}

/// The `init.lua` key: `eitri.config.set("terminal.claude_ide", true | false)`. Whether a `claude` started in this
/// window's bottom terminal can attach to its editor through the CLI's IDE lock file.
pub const TERMINAL_CLAUDE_IDE_KEY: &str = "terminal.claude_ide";

/// [`TERMINAL_CLAUDE_IDE_KEY`] as stored. Unset means off: it opens a port and writes under the CLI's own directory,
/// which only someone who asked for it should get.
pub fn parse_terminal_claude_ide(value: Option<&str>) -> Result<bool, String> {
    crate::config_switch::parse_switch(TERMINAL_CLAUDE_IDE_KEY, value, false)
}

/// The `init.lua` key: `eitri.config.set("remote.enabled", true | false)`. Whether this window starts remote control
/// (the `rv-` socket and `eitri remote serve`).
pub const REMOTE_ENABLED_KEY: &str = "remote.enabled";
/// Where `eitri remote serve` listens: `"loopback"`, `"socket"`, or one address of this machine.
pub const REMOTE_LISTEN_KEY: &str = "remote.listen";
/// The TCP port `serve` listens on; unset means the port chosen once for this user.
pub const REMOTE_PORT_KEY: &str = "remote.port";
/// Public names a proxy or a tunnel reaches `serve` by, separated by commas or spaces.
pub const REMOTE_ORIGINS_KEY: &str = "remote.origins";
/// How many days `serve` keeps rotated files of the remote audit log, from 1 to 3650; unset is 90.
pub const REMOTE_AUDIT_KEEP_DAYS_KEY: &str = "remote.audit.keep_days";
/// Whether `serve` wakes the phones that gave it a push endpoint when an agent needs the person or a belled tab
/// finishes; unset is on. Off, `serve` forks no notifier and posts nothing.
pub const REMOTE_NOTIFY_KEY: &str = "remote.notify";
/// The fewest and the most days [`REMOTE_AUDIT_KEEP_DAYS_KEY`] takes.
pub const REMOTE_AUDIT_KEEP_DAYS_MIN: u32 = 1;
pub const REMOTE_AUDIT_KEEP_DAYS_MAX: u32 = 3650;

/// The longest `remote.origins` entry taken, and how many: a DNS name is at most 253 bytes, and a scheme and port
/// add a few.
const REMOTE_ORIGIN_MAX_BYTES: usize = 300;
const REMOTE_ORIGINS_MAX: usize = 16;

/// Where `serve` listens, as `remote.listen` says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteListen {
    /// `127.0.0.1` and `::1`.
    Loopback,
    /// A Unix socket in the runtime directory.
    Socket,
    /// Exactly one address of this machine; the desk is asked about it before `serve` listens there.
    Address(std::net::IpAddr),
}

/// The `remote.*` keys as one reading of the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSettings {
    pub enabled: bool,
    pub listen: RemoteListen,
    /// `None` is the port chosen once for this user.
    pub port: Option<u16>,
    pub origins: Vec<String>,
    /// Days of rotated audit files `serve` keeps.
    pub audit_keep_days: u32,
    /// Whether `serve` posts wakes to phones' push endpoints.
    pub notify: bool,
}

/// [`REMOTE_ENABLED_KEY`] as stored. Unset means off: it opens a socket and starts a listener, which only someone
/// who asked for it should get.
pub fn parse_remote_enabled(value: Option<&str>) -> Result<bool, String> {
    crate::config_switch::parse_switch(REMOTE_ENABLED_KEY, value, false)
}

/// [`REMOTE_LISTEN_KEY`] as stored. Unset is loopback. A wildcard address is refused: it would take every network the
/// machine is on, and the address the desk confirms has to be one.
pub fn parse_remote_listen(value: Option<&str>) -> Result<RemoteListen, String> {
    let Some(value) = value else {
        return Ok(RemoteListen::Loopback);
    };
    match value.trim() {
        "loopback" => Ok(RemoteListen::Loopback),
        "socket" => Ok(RemoteListen::Socket),
        text => {
            let addr: std::net::IpAddr = text.parse().map_err(|_| {
                format!("{REMOTE_LISTEN_KEY:?} must be loopback, socket or one address of this machine (got {value:?})")
            })?;
            // An IPv4-mapped IPv6 spelling is the IPv4 address it maps: `::ffff:127.0.0.1` is loopback.
            let addr = addr.to_canonical();
            if addr.is_unspecified() {
                return Err(format!(
                    "{REMOTE_LISTEN_KEY:?} refuses the wildcard address {addr}: name the one address phones reach this machine by"
                ));
            }
            if addr.is_multicast() {
                return Err(format!("{REMOTE_LISTEN_KEY:?} refuses the multicast address {addr}"));
            }
            Ok(if addr.is_loopback() {
                RemoteListen::Loopback
            } else {
                RemoteListen::Address(addr)
            })
        }
    }
}

/// [`REMOTE_PORT_KEY`] as stored: a port from 1 to 65535, or unset for the stable one.
pub fn parse_remote_port(value: Option<&str>) -> Result<Option<u16>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let text = value.trim();
    match text.parse::<u16>() {
        Ok(port) if port != 0 && text.bytes().all(|b| b.is_ascii_digit()) => Ok(Some(port)),
        _ => Err(format!(
            "{REMOTE_PORT_KEY:?} must be a port from 1 to 65535 (got {value:?})"
        )),
    }
}

/// [`REMOTE_ORIGINS_KEY`] as stored: names separated by commas or spaces, none empty. A wildcard, a path, a query or
/// user information is refused by `serve`, which normalises each entry; this keeps out what could not be one.
pub fn parse_remote_origins(value: Option<&str>) -> Result<Vec<String>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let mut origins: Vec<String> = Vec::new();
    for entry in value.split([',', ' ']).filter(|entry| !entry.is_empty()) {
        if entry.len() > REMOTE_ORIGIN_MAX_BYTES || entry.contains('*') || !entry.bytes().all(|b| b.is_ascii_graphic())
        {
            return Err(format!(
                "{REMOTE_ORIGINS_KEY:?} takes public host names or origins, not {entry:?}"
            ));
        }
        if !origins.iter().any(|known| known == entry) {
            origins.push(entry.to_owned());
        }
    }
    if origins.len() > REMOTE_ORIGINS_MAX {
        return Err(format!(
            "{REMOTE_ORIGINS_KEY:?} names more than {REMOTE_ORIGINS_MAX} origins"
        ));
    }
    Ok(origins)
}

/// [`REMOTE_AUDIT_KEEP_DAYS_KEY`] as stored: whole days from 1 to 3650, or unset for 90.
pub fn parse_remote_audit_keep_days(value: Option<&str>) -> Result<u32, String> {
    let Some(value) = value else {
        return Ok(crate::remote_audit::AUDIT_KEEP_DAYS_DEFAULT);
    };
    let text = value.trim();
    match text.parse::<u32>() {
        Ok(days)
            if text.bytes().all(|b| b.is_ascii_digit())
                && (REMOTE_AUDIT_KEEP_DAYS_MIN..=REMOTE_AUDIT_KEEP_DAYS_MAX).contains(&days) =>
        {
            Ok(days)
        }
        _ => Err(format!(
            "{REMOTE_AUDIT_KEEP_DAYS_KEY:?} must be a number of days from {REMOTE_AUDIT_KEEP_DAYS_MIN} to \
             {REMOTE_AUDIT_KEEP_DAYS_MAX} (got {value:?})"
        )),
    }
}

/// [`REMOTE_NOTIFY_KEY`] as stored. Unset means on: nothing is posted until a phone gives `serve` an endpoint.
pub fn parse_remote_notify(value: Option<&str>) -> Result<bool, String> {
    crate::config_switch::parse_switch(REMOTE_NOTIFY_KEY, value, true)
}

impl RemoteSettings {
    /// The `remote.*` keys of `store`; the first bad one is the error.
    pub fn read(store: &ConfigStore) -> Result<RemoteSettings, String> {
        Ok(RemoteSettings {
            enabled: parse_remote_enabled(store.get(REMOTE_ENABLED_KEY))?,
            listen: parse_remote_listen(store.get(REMOTE_LISTEN_KEY))?,
            port: parse_remote_port(store.get(REMOTE_PORT_KEY))?,
            origins: parse_remote_origins(store.get(REMOTE_ORIGINS_KEY))?,
            audit_keep_days: parse_remote_audit_keep_days(store.get(REMOTE_AUDIT_KEEP_DAYS_KEY))?,
            notify: parse_remote_notify(store.get(REMOTE_NOTIFY_KEY))?,
        })
    }
}

/// The editor's presentation preference, read after init.lua has run.
pub const EDITOR_PRESENTATION_KEY: &str = "editor.presentation";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorPresentation {
    Auto,
    Subsurface,
    Gtk,
}

pub fn parse_editor_presentation(value: Option<&str>) -> Result<EditorPresentation, String> {
    match value {
        None | Some("auto") => Ok(EditorPresentation::Auto),
        Some("subsurface") => Ok(EditorPresentation::Subsurface),
        Some("gtk") => Ok(EditorPresentation::Gtk),
        Some(value) => Err(format!(
            "{EDITOR_PRESENTATION_KEY:?} must be auto, subsurface or gtk (got {value:?})"
        )),
    }
}

/// The terminal's optional family and height overrides, read after init.lua has run.
pub const TERMINAL_FONT_KEY: &str = "terminal.font";

#[derive(Debug, Clone, PartialEq)]
pub struct TerminalFontSetting {
    pub families: Vec<String>,
    pub size_px: Option<f32>,
}

/// An omitted part follows the editor; an invalid explicit height refuses startup rather than
/// silently substituting the editor's default.
pub fn parse_terminal_font(value: Option<&str>) -> Result<Option<TerminalFontSetting>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let invalid =
        || format!("{TERMINAL_FONT_KEY:?} must name font families or a height in [4, 100] points (got {value:?})");
    let families = crate::theme::guifont_families(value);
    let height = value
        .split(':')
        .skip(1)
        .filter_map(|part| part.strip_prefix('h'))
        .last();
    let size_px = match height {
        Some(height) => {
            let points = height.parse::<f32>().map_err(|_| invalid())?;
            if !points.is_finite() || !(4.0..=100.0).contains(&points) {
                return Err(invalid());
            }
            Some(if families.is_empty() {
                // A placeholder family lets a height-only override use the editor's pixel conversion.
                crate::theme::guifont_size_px(&format!("monospace:h{height}"))
            } else {
                crate::theme::guifont_size_px(value)
            })
        }
        None => None,
    };
    if families.is_empty() && size_px.is_none() {
        return Err(invalid());
    }
    Ok(Some(TerminalFontSetting { families, size_px }))
}

/// `pub`: `Kernel::new` calls this.
pub fn install(lua: &Lua, eitri: &Table, store: Rc<RefCell<ConfigStore>>) -> mlua::Result<()> {
    store.borrow_mut().init_refusals = Some(super::kernel::init_refusals(lua));
    let config_table = lua.create_table()?;

    let store_for_get = store.clone();
    let get_fn = super::kernel::registering_function(
        lua,
        "eitri.config.get",
        move |lua, key: String| -> mlua::Result<Value> {
            match store_for_get.borrow().values.get(&key) {
                Some(Stored::Text(text)) => text.clone().into_lua(lua),
                Some(Stored::Switch(on)) => Ok(Value::Boolean(*on)),
                None => Ok(Value::Nil),
            }
        },
    )?;

    let store_for_set = store.clone();
    let set_fn =
        super::kernel::registering_function(lua, "eitri.config.set", move |lua, (key, value): (Value, Value)| {
            let key = match key {
                Value::String(ref text) => text.to_str().map(|text| text.to_string()).ok(),
                _ => None,
            };
            let Some(key) = key else {
                return Err(mlua::Error::RuntimeError(
                    "eitri.config.set: the key must be a string".to_string(),
                ));
            };
            let stored = match value {
                // `nil` unsets the key, as assigning `nil` removes a Lua table's entry. It is also what
                // `os.getenv` gives for an unset variable.
                Value::Nil => {
                    store_for_set.borrow_mut().unset(&key);
                    return Ok(());
                }
                Value::Boolean(on) => Ok(Stored::Switch(on)),
                Value::String(_) | Value::Integer(_) | Value::Number(_) => String::from_lua(value, lua)
                    .map(Stored::Text)
                    .map_err(|_| "the string is not UTF-8".to_string()),
                other => Err(format!(
                    "a {} value cannot be stored (only a string, a number or a boolean)",
                    other.type_name()
                )),
            };
            match stored {
                Ok(stored) => {
                    let mut store = store_for_set.borrow_mut();
                    match super::kernel::caller_origin(lua) {
                        Some(origin) => store.origins.insert(key.clone(), origin),
                        None => store.origins.remove(&key),
                    };
                    store.values.insert(key, stored);
                    Ok(())
                }
                Err(why) => Err(mlua::Error::RuntimeError(format!(
                    "eitri.config.set({key:?}, ...): {why}"
                ))),
            }
        })?;

    config_table.set("get", get_fn)?;
    config_table.set("set", set_fn)?;
    eitri.set("config", config_table)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_with(init: &str) -> Rc<RefCell<ConfigStore>> {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(ConfigStore::default()));
        install(&lua, &eitri, store.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();
        lua.load(init).exec().unwrap();
        store
    }

    #[test]
    fn defaults_are_off_and_loopback() {
        assert_eq!(REMOTE_ENABLED_KEY, "remote.enabled");
        assert_eq!(REMOTE_LISTEN_KEY, "remote.listen");
        assert_eq!(REMOTE_PORT_KEY, "remote.port");
        assert_eq!(REMOTE_ORIGINS_KEY, "remote.origins");
        let read = RemoteSettings::read(&ConfigStore::default()).unwrap();
        assert_eq!(
            read,
            RemoteSettings {
                enabled: false,
                listen: RemoteListen::Loopback,
                port: None,
                origins: Vec::new(),
                audit_keep_days: 90,
                notify: true,
            }
        );
    }

    #[test]
    fn notify_key_defaults_true() {
        assert_eq!(REMOTE_NOTIFY_KEY, "remote.notify");
        assert_eq!(parse_remote_notify(None), Ok(true));
        assert!(RemoteSettings::read(&ConfigStore::default()).unwrap().notify);
        let store = store_with("eitri.config.set('remote.notify', false)");
        assert!(!RemoteSettings::read(&store.borrow()).unwrap().notify);
        let store = store_with("eitri.config.set('remote.notify', 'maybe')");
        let err = RemoteSettings::read(&store.borrow()).unwrap_err();
        assert!(err.contains("remote.notify"), "{err}");
    }

    #[test]
    fn keep_days_key_defaults_to_90() {
        assert_eq!(REMOTE_AUDIT_KEEP_DAYS_KEY, "remote.audit.keep_days");
        assert_eq!(parse_remote_audit_keep_days(None), Ok(90));
        assert_eq!(
            RemoteSettings::read(&ConfigStore::default()).unwrap().audit_keep_days,
            crate::remote_audit::AUDIT_KEEP_DAYS_DEFAULT
        );
        let store = store_with("eitri.config.set('remote.audit.keep_days', 30)");
        assert_eq!(RemoteSettings::read(&store.borrow()).unwrap().audit_keep_days, 30);
    }

    #[test]
    fn keep_days_must_be_1_to_3650() {
        for (value, days) in [("1", 1), ("3650", 3650), (" 7 ", 7)] {
            assert_eq!(parse_remote_audit_keep_days(Some(value)), Ok(days), "{value:?}");
        }
        for bad in ["0", "3651", "-1", "7d", "", "1e2", "90.5", "99999999999"] {
            let err = parse_remote_audit_keep_days(Some(bad)).unwrap_err();
            assert!(
                err.contains("remote.audit.keep_days") && err.contains("1 to 3650"),
                "{bad:?}: {err}"
            );
        }
        let store = store_with("eitri.config.set('remote.audit.keep_days', 0)");
        assert!(RemoteSettings::read(&store.borrow()).is_err());
    }

    #[test]
    fn the_keys_read_what_init_lua_set() {
        let store = store_with(
            "eitri.config.set('remote.enabled', true)\n\
             eitri.config.set('remote.listen', '203.0.113.7')\n\
             eitri.config.set('remote.port', 7400)\n\
             eitri.config.set('remote.origins', 'desk.tail1.ts.net, https://phone.example.org desk.tail1.ts.net')",
        );
        let read = RemoteSettings::read(&store.borrow()).unwrap();
        assert!(read.enabled);
        assert_eq!(read.listen, RemoteListen::Address("203.0.113.7".parse().unwrap()));
        assert_eq!(read.port, Some(7400));
        assert_eq!(read.origins, ["desk.tail1.ts.net", "https://phone.example.org"]);
        for (value, expected) in [
            ("'loopback'", RemoteListen::Loopback),
            ("'socket'", RemoteListen::Socket),
            ("'127.0.0.1'", RemoteListen::Loopback),
        ] {
            let store = store_with(&format!("eitri.config.set('remote.listen', {value})"));
            assert_eq!(parse_remote_listen(store.borrow().get(REMOTE_LISTEN_KEY)), Ok(expected));
        }
    }

    #[test]
    fn wildcard_listen_is_a_config_error() {
        for wildcard in ["0.0.0.0", "::", " 0.0.0.0 "] {
            let err = parse_remote_listen(Some(wildcard)).unwrap_err();
            assert!(err.contains("remote.listen") && err.contains("wildcard"), "{err}");
        }
        for other in ["", "everywhere", "desk.local", "224.0.0.1", "ff02::1"] {
            let err = parse_remote_listen(Some(other)).unwrap_err();
            assert!(err.contains("remote.listen"), "{err}");
        }
        // The error reaches a reader of the whole group, so the window can say it and use the default.
        let store = store_with("eitri.config.set('remote.listen', '0.0.0.0')");
        assert!(RemoteSettings::read(&store.borrow()).is_err());
    }

    #[test]
    fn a_bad_enabled_port_or_origin_names_its_key() {
        assert!(parse_remote_enabled(Some("yes"))
            .unwrap_err()
            .contains("remote.enabled"));
        for port in ["0", "65536", "-1", "80x", "", "1e3"] {
            assert!(
                parse_remote_port(Some(port)).unwrap_err().contains("remote.port"),
                "{port}"
            );
        }
        assert_eq!(parse_remote_port(Some("65535")), Ok(Some(65535)));
        for origin in ["*.ts.net", "a\u{7f}b", "bad\u{e9}", &"a".repeat(301)] {
            let err = parse_remote_origins(Some(origin)).unwrap_err();
            assert!(err.contains("remote.origins"), "{err}");
        }
        assert_eq!(parse_remote_origins(Some(" , ")), Ok(Vec::new()));
        let many = (0..17)
            .map(|n| format!("h{n}.example.org"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(parse_remote_origins(Some(&many)).unwrap_err().contains("more than"));
    }

    #[test]
    fn editor_takeover_parses_and_refuses_others() {
        assert_eq!(EDITOR_TAKEOVER_KEY, "editor.takeover");
        assert_eq!(parse_editor_takeover(None), Ok(true));
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(ConfigStore::default()));
        install(&lua, &eitri, store.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();
        for (value, expected) in [("true", true), ("'on'", true), ("false", false), ("'off'", false)] {
            lua.load(format!("eitri.config.set('editor.takeover', {value})"))
                .exec()
                .unwrap();
            assert_eq!(
                parse_editor_takeover(store.borrow().get(EDITOR_TAKEOVER_KEY)),
                Ok(expected)
            );
        }
        for bad in ["offer", "yes", "1", "0", "", "TRUE"] {
            let error = parse_editor_takeover(Some(bad)).unwrap_err();
            assert!(error.contains("editor.takeover"), "{error}");
        }
    }

    #[test]
    fn keys_under_a_prefix_come_in_key_order_as_get_reads_them() {
        let mut store = ConfigStore::default();
        for (key, value) in [
            ("agent.editor.tool.b", Stored::Switch(false)),
            ("agent.editor.tool.a", Stored::Text("on".to_owned())),
            ("agent.editor.tools", Stored::Switch(true)),
        ] {
            store.values.insert(key.to_owned(), value);
        }
        assert_eq!(
            store.keys_under("agent.editor.tool."),
            vec![
                ("agent.editor.tool.a".to_owned(), "on".to_owned()),
                ("agent.editor.tool.b".to_owned(), "false".to_owned()),
            ]
        );
        assert!(store.keys_under("nothing.").is_empty());
    }

    #[test]
    fn entries_are_sorted_and_read_as_get_reads() {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(ConfigStore::default()));
        install(&lua, &eitri, store.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();
        assert!(store.borrow().entries().is_empty());
        lua.load(
            "eitri.config.set('z.text', 'word')\n\
             eitri.config.set('a.number', 200)\n\
             eitri.config.set('m.switch', false)\n\
             eitri.config.set('gone', 'x')\n\
             eitri.config.set('gone', nil)\n",
        )
        .exec()
        .unwrap();
        let entries = store.borrow().entries();
        let entries: Vec<(&str, &str)> = entries.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(
            entries,
            [("a.number", "200"), ("m.switch", "false"), ("z.text", "word")]
        );
    }

    #[test]
    fn regression_terminal_font_requires_at_least_four_points() {
        for value in [":h0.001", "monospace:h3.99"] {
            let error = parse_terminal_font(Some(value)).unwrap_err();
            assert!(error.contains("4"), "{error}");
        }
        assert!(parse_terminal_font(Some(":h4")).is_ok());
    }

    #[test]
    fn terminal_font_parses_optional_families_and_height() {
        assert_eq!(TERMINAL_FONT_KEY, "terminal.font");
        assert_eq!(parse_terminal_font(None), Ok(None));
        for (value, families, size_px) in [
            (
                "Iosevka:h12",
                vec!["Iosevka"],
                Some(crate::theme::guifont_size_px("Iosevka:h12")),
            ),
            ("Iosevka", vec!["Iosevka"], None),
            (":h12", vec![], Some(crate::theme::guifont_size_px("Iosevka:h12"))),
            (
                "Fira\\ Code,Symbols_Nerd_Font:h10:h13.5:b",
                vec!["Fira Code", "Symbols Nerd Font"],
                Some(crate::theme::guifont_size_px("Mono:h13.5")),
            ),
            (":h100", vec![], Some(crate::theme::guifont_size_px("Mono:h100"))),
            (
                "Mono:hx:h12",
                vec!["Mono"],
                Some(crate::theme::guifont_size_px("Mono:h12")),
            ),
        ] {
            assert_eq!(
                parse_terminal_font(Some(value)),
                Ok(Some(TerminalFontSetting {
                    families: families.into_iter().map(str::to_owned).collect(),
                    size_px,
                })),
                "{value:?}"
            );
        }
    }

    #[test]
    fn terminal_font_refuses_empty_settings_and_invalid_heights_naming_the_key_and_value() {
        for value in [
            "",
            "  , ",
            ":b",
            ":h0",
            ":hx",
            ":h101",
            ":h-1",
            ":hNaN",
            ":hinf",
            ":h",
            "Mono:h12:hx",
            "Mono:h12:h",
            "Mono:h101",
        ] {
            let error = parse_terminal_font(Some(value)).unwrap_err();
            assert!(error.contains(TERMINAL_FONT_KEY), "{error}");
            assert!(error.contains(&format!("{value:?}")), "{error}");
        }
    }

    #[test]
    fn editor_presentation_parses_three_words_and_refuses_others() {
        assert_eq!(parse_editor_presentation(None), Ok(EditorPresentation::Auto));
        for (word, expected) in [
            ("auto", EditorPresentation::Auto),
            ("subsurface", EditorPresentation::Subsurface),
            ("gtk", EditorPresentation::Gtk),
        ] {
            assert_eq!(parse_editor_presentation(Some(word)), Ok(expected));
        }
        for bad in ["x", "", "Auto", " gtk", "subsurface\n", "true"] {
            let error = parse_editor_presentation(Some(bad)).unwrap_err();
            for word in ["editor.presentation", "auto", "subsurface", "gtk"] {
                assert!(error.contains(word), "{error}");
            }
        }
    }

    #[test]
    fn editor_restore_is_on_by_default_and_takes_both_spellings() {
        assert_eq!(EDITOR_RESTORE_KEY, "editor.restore");
        assert_eq!(parse_editor_restore(None), Ok(true));
        for (value, expected) in [("true", true), ("on", true), ("false", false), ("off", false)] {
            assert_eq!(parse_editor_restore(Some(value)), Ok(expected));
        }
    }

    #[test]
    fn parse_desktop_start_reads_three_values_and_refuses_others() {
        assert_eq!(DESKTOP_START_KEY, "window.desktop_start");
        assert_eq!(parse_desktop_start(None), Ok(DesktopStart::Choose));
        for (value, expected) in [
            ("choose", DesktopStart::Choose),
            ("last", DesktopStart::Last),
            ("cwd", DesktopStart::Cwd),
        ] {
            assert_eq!(parse_desktop_start(Some(value)), Ok(expected));
        }
        for bad in ["", "Choose", "true", "last "] {
            let err = parse_desktop_start(Some(bad)).unwrap_err();
            assert!(
                err.contains("\"window.desktop_start\""),
                "the refusal names its key: {err}"
            );
            assert!(
                err.contains("choose, last or cwd"),
                "the refusal names the values: {err}"
            );
        }
    }

    #[test]
    fn editor_restore_refuses_offer_naming_the_key() {
        let err = parse_editor_restore(Some("offer")).unwrap_err();
        assert!(err.contains("\"editor.restore\""), "{err}");
        assert!(err.contains("must be true or \"on\" / false or \"off\""), "{err}");
    }

    #[test]
    fn window_restore_takes_a_boolean_and_names_itself_otherwise() {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(ConfigStore::default()));
        install(&lua, &eitri, store.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();
        let read = || parse_window_restore(store.borrow().get(WINDOW_RESTORE_KEY));

        assert_eq!(read(), Ok(true), "unset, the second window comes back");
        for (set, want) in [("false", false), ("true", true), ("'off'", false), ("'on'", true)] {
            lua.load(format!("eitri.config.set('window.restore', {set})"))
                .exec()
                .unwrap();
            assert_eq!(read(), Ok(want), "{set}");
        }
        for bad in ["'later'", "1"] {
            lua.load(format!("eitri.config.set('window.restore', {bad})"))
                .exec()
                .unwrap();
            let err = read().unwrap_err();
            assert!(err.contains("\"window.restore\""), "the refusal names its key: {err}");
            assert!(err.contains("must be true or \"on\" / false or \"off\""), "{err}");
        }
    }

    #[test]
    fn terminal_claude_ide_is_off_by_default() {
        assert_eq!(TERMINAL_CLAUDE_IDE_KEY, "terminal.claude_ide");
        assert_eq!(parse_terminal_claude_ide(None), Ok(false));
    }

    #[test]
    fn terminal_claude_ide_takes_both_spellings() {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(ConfigStore::default()));
        install(&lua, &eitri, store.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();

        for (set, want) in [("true", true), ("false", false), ("'on'", true), ("'off'", false)] {
            lua.load(format!("eitri.config.set('terminal.claude_ide', {set})"))
                .exec()
                .unwrap();
            let read = parse_terminal_claude_ide(store.borrow().get(TERMINAL_CLAUDE_IDE_KEY));
            assert_eq!(read, Ok(want), "{set}");
        }
    }

    #[test]
    fn a_bad_terminal_claude_ide_names_the_key() {
        for bad in ["yes", "1", "0", "", "TRUE", "nil"] {
            let err = parse_terminal_claude_ide(Some(bad)).unwrap_err();
            assert!(
                err.contains("\"terminal.claude_ide\""),
                "the refusal names its key: {err}"
            );
            assert!(err.contains("must be true or \"on\" / false or \"off\""), "{err}");
        }
    }

    #[test]
    fn set_then_get_roundtrips_through_real_lua() {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(ConfigStore::default()));
        install(&lua, &eitri, store).unwrap();
        lua.globals().set("eitri", eitri).unwrap();

        lua.load(r#"eitri.config.set("greeting", "hello")"#).exec().unwrap();
        let value: String = lua.load(r#"return eitri.config.get("greeting")"#).eval().unwrap();
        assert_eq!(value, "hello");
    }

    #[test]
    fn what_init_lua_set_is_readable_from_rust_through_the_same_store() {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(ConfigStore::default()));
        install(&lua, &eitri, store.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();

        lua.load(r#"eitri.config.set("agent.account", "work")"#)
            .exec()
            .unwrap();
        assert_eq!(store.borrow().get("agent.account"), Some("work"));
        assert_eq!(store.borrow().get("agent.nothing"), None);
    }

    #[test]
    fn a_boolean_is_stored_as_its_word_and_a_number_as_its_text() {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(ConfigStore::default()));
        install(&lua, &eitri, store.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();

        lua.load(
            r#"
            eitri.config.set("a.on", true)
            eitri.config.set("a.off", false)
            eitri.config.set("a.size", 12)
            eitri.config.set("a.word", "false")
            "#,
        )
        .exec()
        .unwrap();
        assert_eq!(store.borrow().get("a.on"), Some("true"));
        assert_eq!(store.borrow().get("a.off"), Some("false"));
        assert_eq!(store.borrow().get("a.size"), Some("12"));
        assert_eq!(store.borrow().get("a.word"), Some("false"));
    }

    #[test]
    fn a_boolean_comes_back_to_lua_as_a_boolean() {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(ConfigStore::default()));
        install(&lua, &eitri, store).unwrap();
        lua.globals().set("eitri", eitri).unwrap();

        let (off, on, word): (bool, bool, String) = lua
            .load(
                r#"
                eitri.config.set("a.off", false)
                eitri.config.set("a.on", true)
                eitri.config.set("a.word", "false")
                return eitri.config.get("a.off") == false, eitri.config.get("a.on") == true,
                    eitri.config.get("a.word")
                "#,
            )
            .eval()
            .unwrap();
        assert!(off && on);
        assert_eq!(word, "false");
    }

    #[test]
    fn a_table_is_still_refused() {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(ConfigStore::default()));
        install(&lua, &eitri, store).unwrap();
        lua.globals().set("eitri", eitri).unwrap();
        assert!(lua.load(r#"eitri.config.set("a.t", {})"#).exec().is_err());
    }

    #[test]
    fn get_of_unset_key_returns_nil() {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(ConfigStore::default()));
        install(&lua, &eitri, store).unwrap();
        lua.globals().set("eitri", eitri).unwrap();

        let is_nil: bool = lua
            .load(r#"return eitri.config.get("never_set") == nil"#)
            .eval()
            .unwrap();
        assert!(is_nil);
    }

    /// A value the store cannot hold is refused by key, and remembered even when init.lua catches
    /// the error, so the shell can refuse to start instead of running on the key's default.
    #[test]
    fn a_value_the_store_cannot_hold_is_refused_and_remembered_through_a_pcall() {
        for (value, expected) in [
            (
                "{}",
                "eitri.config.set(\"agent.account\", ...): a table value cannot be stored",
            ),
            (
                "function() end",
                "eitri.config.set(\"agent.account\", ...): a function value cannot be stored",
            ),
            (
                "\"\\255\"",
                "eitri.config.set(\"agent.account\", ...): the string is not UTF-8",
            ),
        ] {
            let lua = Lua::new();
            let eitri = lua.create_table().unwrap();
            let store = Rc::new(RefCell::new(ConfigStore::default()));
            install(&lua, &eitri, store.clone()).unwrap();
            lua.globals().set("eitri", eitri).unwrap();

            lua.load(format!(
                r#"
                local ok = pcall(eitri.config.set, "agent.account", {value})
                assert(not ok)
                eitri.config.set("agent.font_size", 14)
                "#
            ))
            .exec()
            .expect("the chunk caught the refusal and went on");
            let store = store.borrow();
            let refused = store.refused().unwrap_or_else(|| panic!("{value} was not refused"));
            assert!(refused.starts_with(expected), "{value}: {refused}");
            assert_eq!(store.get("agent.account"), None, "{value} must not be stored");
            assert_eq!(store.get("agent.font_size"), Some("14"));
        }
    }

    /// `nil` unsets, the way `os.getenv` reports an unset variable: what was set before is gone and
    /// nothing is refused.
    #[test]
    fn nil_unsets_a_key_rather_than_being_refused() {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(ConfigStore::default()));
        install(&lua, &eitri, store.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();

        lua.load(
            r#"
            eitri.config.set("agent.account", "work")
            eitri.config.set("agent.account", nil)
            eitri.config.set("agent.never", os.getenv("EITRI_TEST_SURELY_UNSET_VARIABLE"))
            "#,
        )
        .exec()
        .unwrap();
        let store = store.borrow();
        assert_eq!(store.get("agent.account"), None);
        assert_eq!(store.get("agent.never"), None);
        assert_eq!(store.refused(), None);
    }

    #[test]
    fn a_key_that_is_not_a_string_is_refused() {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(ConfigStore::default()));
        install(&lua, &eitri, store.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();

        assert!(lua.load(r#"eitri.config.set({}, "work")"#).exec().is_err());
        assert_eq!(
            store.borrow().refused().as_deref(),
            Some("eitri.config.set: the key must be a string")
        );
    }

    #[test]
    fn strings_numbers_and_booleans_are_never_refused() {
        let lua = Lua::new();
        let eitri = lua.create_table().unwrap();
        let store = Rc::new(RefCell::new(ConfigStore::default()));
        install(&lua, &eitri, store.clone()).unwrap();
        lua.globals().set("eitri", eitri).unwrap();

        lua.load(r#"eitri.config.set("a", "x"); eitri.config.set("b", 1.5); eitri.config.set("c", true)"#)
            .exec()
            .unwrap();
        assert_eq!(store.borrow().refused(), None);
    }
}
