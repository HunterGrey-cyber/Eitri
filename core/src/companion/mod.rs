//! The injector: what a companion panel loads into a user's own, already running nvim, and the
//! state machine that follows the attach.
//!
//! One `nvim_exec_lua` call ([`INSTALL_LUA`]) installs the six Eitri snippets and a liveness timer.
//! The same call removes whatever an earlier panel left, so a second panel replaces the first and a
//! second install from the same channel changes nothing. Nothing here touches the socket: the
//! caller sends [`install_args`] through an [`crate::nvim_rpc::NvimLink`] and reads the answer with
//! [`parse_install_report`]. A slow answer is a pending one, never a failed one, because nvim
//! queues requests while it waits for a character.

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use rmpv::Value;

pub mod attach;
pub mod driver;
pub mod env;
pub mod link;
pub mod stdio;

/// The installer: `nvim_exec_lua(INSTALL_LUA, install_args(...))`.
pub const INSTALL_LUA: &str = include_str!("install.lua");

/// Run the teardown of the install made by channel `...` (the one argument), if it is still the
/// current one. `true` when it ran, `false` when another channel's install holds the nvim or
/// nothing is installed.
pub const TEARDOWN_LUA: &str = "local chan = ...; local c = rawget(_G, '__eitri_companion'); if c and c.chan == chan then c.teardown() return true end return false";

/// Hand `EitriScratch` the hex-encoded request `...`; `false` when the scratch snippet is not installed.
pub const SCRATCH_CALL_LUA: &str = "local hex = ...; if type(rawget(_G, 'EitriScratch')) == 'table' then EitriScratch.call(hex) return true end return false";

/// The notification nvim sends the panel that held an nvim when another panel's install replaces it.
pub const REPLACED_METHOD: &str = "eitri_replaced";

/// The sockets the nvim-side glue writes to. A part whose socket is `None` is not installed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sockets {
    pub editor_context: Option<PathBuf>,
    pub theme: Option<PathBuf>,
    pub keys: Option<PathBuf>,
    /// Where the nav fallback, and `edge()` for a navigator plugin, write a direction letter.
    pub pane_switch: Option<PathBuf>,
}

/// A path as nvim receives it. A path that is not UTF-8 goes as raw bytes, which nvim's unpacker
/// turns into a Lua string too, so it still names the same file; a lossy conversion would name another.
fn path_value(path: &Path) -> Value {
    match path.to_str() {
        Some(text) => Value::from(text),
        None => Value::Binary(path.as_os_str().as_bytes().to_vec()),
    }
}

fn map(pairs: Vec<(&str, Value)>) -> Value {
    Value::Map(pairs.into_iter().map(|(k, v)| (Value::from(k), v)).collect())
}

/// The arguments of `nvim_set_client_info`: how the panel's channel is described in `nvim_get_chan_info`.
pub fn client_info_params() -> Vec<Value> {
    let number = |text: &str| Value::from(text.parse::<u64>().unwrap_or(0));
    let mut version = vec![
        ("major", number(env!("CARGO_PKG_VERSION_MAJOR"))),
        ("minor", number(env!("CARGO_PKG_VERSION_MINOR"))),
        ("patch", number(env!("CARGO_PKG_VERSION_PATCH"))),
    ];
    let pre = env!("CARGO_PKG_VERSION_PRE");
    if !pre.is_empty() {
        version.push(("prerelease", Value::from(pre)));
    }
    vec![
        Value::from("eitri-panel"),
        map(version),
        Value::from("remote"),
        map(vec![]),
        map(vec![("website", Value::from("https://eitri.cc"))]),
    ]
}

fn part(name: &str, src: &str, opts: Vec<(&str, Value)>) -> Value {
    map(vec![
        ("name", Value::from(name)),
        ("src", Value::from(src)),
        ("opts", map(opts)),
    ])
}

/// The three arguments of [`INSTALL_LUA`]: the panel's channel, the parts in install order and the
/// extras (`edge_socket`).
pub fn install_args(channel_id: u64, sockets: &Sockets) -> Vec<Value> {
    let mut parts = Vec::new();
    if let Some(socket) = &sockets.editor_context {
        parts.push(part(
            "editor_context",
            crate::editor_context::feed::NVIM_EDITOR_CONTEXT_LUA,
            vec![("socket", path_value(socket))],
        ));
    }
    if let Some(socket) = &sockets.theme {
        parts.push(part(
            "theme",
            crate::theme::feed::NVIM_THEME_LUA,
            vec![("socket", path_value(socket))],
        ));
    }
    if let Some(socket) = &sockets.keys {
        parts.push(part(
            "keys",
            crate::nvim_keys::feed::NVIM_KEYS_LUA,
            vec![("socket", path_value(socket))],
        ));
    }
    if let Some(socket) = &sockets.pane_switch {
        parts.push(part(
            "nav_fallback",
            crate::pane_switch::NAV_FALLBACK_LUA,
            vec![("socket", path_value(socket)), ("companion", Value::from(true))],
        ));
    }
    // An empty table, not nil: a snippet given a table never reads `vim.env`.
    parts.push(part("scratch", crate::scratch::NVIM_SCRATCH_LUA, vec![]));
    parts.push(part("buffer_reload", crate::buffer_reload::BUFFER_RELOAD_LUA, vec![]));
    let extra = match &sockets.pane_switch {
        Some(socket) => map(vec![("edge_socket", path_value(socket))]),
        None => map(vec![]),
    };
    vec![Value::from(channel_id), Value::Array(parts), extra]
}

/// What [`INSTALL_LUA`] answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    /// The parts that loaded, in install order.
    pub installed: Vec<String>,
    /// The parts that did not: `(name, why)`.
    pub failed: Vec<(String, String)>,
    pub nvim_pid: u32,
    /// Whether nvim runs inside tmux (its environment carries `TMUX`).
    pub in_tmux: bool,
}

/// The only report version this panel reads.
const REPORT_VERSION: u64 = 1;

fn field<'a>(report: &'a Value, name: &str) -> Option<&'a Value> {
    report
        .as_map()?
        .iter()
        .find(|(key, _)| key.as_str() == Some(name))
        .map(|(_, value)| value)
}

/// A Lua list that nvim may have encoded as an empty map when it holds nothing.
fn list<'a>(report: &'a Value, name: &str) -> Result<&'a [Value], String> {
    match field(report, name) {
        Some(Value::Array(items)) => Ok(items),
        Some(Value::Map(items)) if items.is_empty() => Ok(&[]),
        _ => Err(format!("the install report has no {name} list")),
    }
}

/// Read the table [`INSTALL_LUA`] returns. The error is a plain sentence for the band.
pub fn parse_install_report(report: &Value) -> Result<InstallReport, String> {
    if report.as_map().is_none() {
        return Err("the injector answered something that is not a report".to_owned());
    }
    match field(report, "version").and_then(Value::as_u64) {
        Some(REPORT_VERSION) => {}
        Some(other) => {
            return Err(format!(
                "the injector answered version {other}, this panel speaks {REPORT_VERSION}"
            ));
        }
        None => return Err("the install report has no version".to_owned()),
    }
    let text = |value: &Value, what: &str| {
        value
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("the install report has a {what} that is not text"))
    };
    let installed = list(report, "installed")?
        .iter()
        .map(|name| text(name, "part name"))
        .collect::<Result<Vec<_>, _>>()?;
    let failed = list(report, "failed")?
        .iter()
        .map(|entry| match entry.as_array().map(Vec::as_slice) {
            Some([name, why]) => Ok((text(name, "failure")?, text(why, "failure")?)),
            _ => Err("the install report has a failure that is not a name and a reason".to_owned()),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let nvim_pid = field(report, "pid")
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
        .ok_or_else(|| "the install report has no pid".to_owned())?;
    let in_tmux = field(report, "tmux")
        .and_then(Value::as_bool)
        .ok_or_else(|| "the install report does not say whether nvim runs in tmux".to_owned())?;
    Ok(InstallReport {
        installed,
        failed,
        nvim_pid,
        in_tmux,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get<'a>(value: &'a Value, key: &str) -> &'a Value {
        field(value, key).unwrap_or_else(|| panic!("no key {key} in {value:?}"))
    }

    fn all_sockets() -> Sockets {
        Sockets {
            editor_context: Some(PathBuf::from("/r/ec")),
            theme: Some(PathBuf::from("/r/th")),
            keys: Some(PathBuf::from("/r/ky")),
            pane_switch: Some(PathBuf::from("/r/ps")),
        }
    }

    fn names(args: &[Value]) -> Vec<String> {
        args[1]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| get(p, "name").as_str().unwrap().to_owned())
            .collect()
    }

    fn report(installed: Value, failed: Value) -> Value {
        map(vec![
            ("version", Value::from(1)),
            ("installed", installed),
            ("failed", failed),
            ("pid", Value::from(4242)),
            ("tmux", Value::from(false)),
        ])
    }

    #[test]
    fn install_args_lists_parts_in_order_and_leaves_out_a_missing_socket() {
        let args = install_args(7, &all_sockets());
        assert_eq!(args.len(), 3);
        assert_eq!(args[0].as_u64(), Some(7));
        assert_eq!(
            names(&args),
            [
                "editor_context",
                "theme",
                "keys",
                "nav_fallback",
                "scratch",
                "buffer_reload"
            ]
        );
        let parts = args[1].as_array().unwrap();
        let sources = [
            crate::editor_context::feed::NVIM_EDITOR_CONTEXT_LUA,
            crate::theme::feed::NVIM_THEME_LUA,
            crate::nvim_keys::feed::NVIM_KEYS_LUA,
            crate::pane_switch::NAV_FALLBACK_LUA,
            crate::scratch::NVIM_SCRATCH_LUA,
            crate::buffer_reload::BUFFER_RELOAD_LUA,
        ];
        for (part, source) in parts.iter().zip(sources) {
            assert_eq!(get(part, "src").as_str(), Some(source));
        }
        let opts = |i: usize| get(&parts[i], "opts");
        assert_eq!(get(opts(0), "socket").as_str(), Some("/r/ec"));
        assert_eq!(get(opts(1), "socket").as_str(), Some("/r/th"));
        assert_eq!(get(opts(2), "socket").as_str(), Some("/r/ky"));
        assert_eq!(get(opts(3), "socket").as_str(), Some("/r/ps"));
        assert_eq!(get(opts(3), "companion").as_bool(), Some(true));
        // A table, never nil: a snippet given a table does not read `vim.env`.
        assert_eq!(opts(4).as_map().map(Vec::len), Some(0));
        assert_eq!(opts(5).as_map().map(Vec::len), Some(0));
        assert_eq!(get(&args[2], "edge_socket").as_str(), Some("/r/ps"));

        let none = install_args(
            7,
            &Sockets {
                theme: Some(PathBuf::from("/r/th")),
                ..Sockets::default()
            },
        );
        assert_eq!(names(&none), ["theme", "scratch", "buffer_reload"]);
        assert_eq!(
            none[2].as_map().map(Vec::len),
            Some(0),
            "no pane-switch socket, no edge socket"
        );
        let no_nav = install_args(
            7,
            &Sockets {
                pane_switch: None,
                ..all_sockets()
            },
        );
        assert_eq!(
            names(&no_nav),
            ["editor_context", "theme", "keys", "scratch", "buffer_reload"]
        );
    }

    #[test]
    fn a_path_that_is_not_utf8_goes_as_the_same_bytes() {
        let raw = std::ffi::OsStr::from_bytes(b"/r/\xff\xfe");
        match path_value(Path::new(raw)) {
            Value::Binary(bytes) => assert_eq!(bytes, b"/r/\xff\xfe"),
            other => panic!("expected the raw bytes, got {other:?}"),
        }
        assert_eq!(path_value(Path::new("/r/ec")).as_str(), Some("/r/ec"));
    }

    #[test]
    fn parse_install_report_reads_installed_failed_pid_tmux() {
        let value = report(
            Value::Array(vec![Value::from("theme"), Value::from("keys")]),
            Value::Array(vec![Value::Array(vec![Value::from("scratch"), Value::from("boom")])]),
        );
        assert_eq!(
            parse_install_report(&value),
            Ok(InstallReport {
                installed: vec!["theme".into(), "keys".into()],
                failed: vec![("scratch".into(), "boom".into())],
                nvim_pid: 4242,
                in_tmux: false,
            })
        );
    }

    #[test]
    fn parse_install_report_takes_an_empty_list_as_a_map_or_an_array() {
        for empty in [Value::Map(vec![]), Value::Array(vec![])] {
            let parsed = parse_install_report(&report(empty.clone(), empty)).unwrap();
            assert!(parsed.installed.is_empty() && parsed.failed.is_empty());
        }
    }

    #[test]
    fn parse_install_report_refuses_another_version() {
        let mut value = report(Value::Array(vec![]), Value::Array(vec![]));
        if let Value::Map(pairs) = &mut value {
            pairs[0].1 = Value::from(2);
        }
        assert_eq!(
            parse_install_report(&value),
            Err("the injector answered version 2, this panel speaks 1".to_owned())
        );
        assert!(parse_install_report(&Value::from("no")).is_err());
    }

    #[test]
    fn parse_install_report_names_what_is_missing_in_a_plain_sentence() {
        let without = |gone: &str| {
            let mut value = report(Value::Array(vec![]), Value::Array(vec![]));
            if let Value::Map(pairs) = &mut value {
                pairs.retain(|(k, _)| k.as_str() != Some(gone));
            }
            parse_install_report(&value).unwrap_err()
        };
        assert_eq!(without("pid"), "the install report has no pid");
        assert!(without("installed").contains("installed"));
        assert!(without("failed").contains("failed"));
        assert!(without("tmux").contains("tmux"));
        assert!(without("version").contains("version"));
    }

    #[test]
    fn client_info_names_a_remote_eitri_panel() {
        let info = client_info_params();
        assert_eq!(info.len(), 5);
        assert_eq!(info[0].as_str(), Some("eitri-panel"));
        assert_eq!(info[2].as_str(), Some("remote"));
        let version = &info[1];
        assert_eq!(
            get(version, "major").as_u64(),
            env!("CARGO_PKG_VERSION_MAJOR").parse::<u64>().ok()
        );
        assert_eq!(
            get(version, "minor").as_u64(),
            env!("CARGO_PKG_VERSION_MINOR").parse::<u64>().ok()
        );
        assert_eq!(
            get(version, "patch").as_u64(),
            env!("CARGO_PKG_VERSION_PATCH").parse::<u64>().ok()
        );
        assert_eq!(
            field(version, "prerelease").is_some(),
            !env!("CARGO_PKG_VERSION_PRE").is_empty()
        );
        assert_eq!(get(&info[4], "website").as_str(), Some("https://eitri.cc"));
    }

    #[test]
    fn the_lua_parses() {
        let lua = mlua::Lua::new();
        for (name, source) in [
            ("install", INSTALL_LUA),
            ("teardown", TEARDOWN_LUA),
            ("scratch call", SCRATCH_CALL_LUA),
        ] {
            if let Err(e) = lua.load(source).set_name(name).into_function() {
                panic!("{name} does not parse: {e}");
            }
        }
    }

    #[test]
    fn install_lua_is_lua51_safe_and_names_the_replaced_method() {
        assert!(!INSTALL_LUA.contains("goto "), "goto is not in Lua 5.1 or LuaJIT");
        assert!(
            !INSTALL_LUA.contains("load("),
            "loadstring, not load(string): PUC Lua 5.1 has no string load"
        );
        assert!(
            INSTALL_LUA.contains(&format!("\"{REPLACED_METHOD}\"")),
            "install.lua and REPLACED_METHOD drifted"
        );
    }
}
