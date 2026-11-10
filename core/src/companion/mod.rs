//! The injector: what a companion panel loads into a user's own, already running nvim, and the
//! state machine that follows the attach.
//!
//! One `nvim_exec_lua` call ([`INSTALL_LUA`]) installs an agent's six snippets or a page's theme,
//! with a liveness timer for each slot. An install replaces only its slot's earlier holder; a second
//! install from the same channel tears down and reinstalls without telling that channel it
//! was replaced. Nothing here touches the socket: the
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

/// Activity-driven buffer reloads for an agent slot.
pub const RELOAD_PART_LUA: &str = include_str!("reload_part.lua");

/// Run the teardown of the install made by channel `...` (the one argument), if it is still the
/// current one. `true` when it ran, `false` when another channel's install holds the nvim or
/// nothing is installed.
pub const TEARDOWN_LUA: &str = "local chan = ...; local c = rawget(_G, '__eitri_companion'); if c and c.chan == chan then c.teardown() return true end return false";

/// Tear down only the page held by the supplied `(id, channel)`.
pub const PAGE_TEARDOWN_LUA: &str = "local id, chan = ...; local pages = rawget(_G, '__eitri_pages'); local c = pages and pages[id]; if c and c.chan == chan then c.teardown() return true end return false";

/// Call a named operation only while the supplied slot belongs to this channel.
pub const SLOT_CALL_LUA: &str = r#"
local slot, chan, name, args = ...
local state
if slot == "agent" then
  state = rawget(_G, "__eitri_companion")
elseif type(slot) == "string" and slot:sub(1, 5) == "page:" then
  local pages = rawget(_G, "__eitri_pages")
  state = pages and pages[slot:sub(6)]
end
if not state or state.torn or state.chan ~= chan then return false end
local call = state.calls and state.calls[name]
if type(call) ~= "function" then return false end
return call(unpack(args or {}))
"#;

/// The glue a connection installs into its nvim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallKind {
    Agent,
    Page { id: String },
}

/// A page id is also safe as a suffix in nvim's group names.
pub fn page_id_ok(id: &str) -> bool {
    (1..=40).contains(&id.len())
        && !id.starts_with('-')
        && !id.ends_with('-')
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Hand `EitriScratch` the hex-encoded request `...`; `false` when the scratch snippet is not installed.
pub const SCRATCH_CALL_LUA: &str = "local hex = ...; if type(rawget(_G, 'EitriScratch')) == 'table' then EitriScratch.call(hex) return true end return false";

/// The notification nvim sends the panel that held an nvim when another panel's install replaces it.
pub const REPLACED_METHOD: &str = "eitri_replaced";

/// The sockets the nvim-side glue writes to, and how its buffer reload runs. A part whose socket is `None` is not
/// installed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sockets {
    pub editor_context: Option<PathBuf>,
    pub theme: Option<PathBuf>,
    pub keys: Option<PathBuf>,
    /// Where the nav fallback, and `edge()` for a navigator plugin, write a direction letter.
    pub pane_switch: Option<PathBuf>,
    /// The agent's buffer reload keeps its timer on for the whole attach: for a host that never sends the reload
    /// part's `busy` and `now` calls. Without it the timer runs only while the panel says its agent is busy.
    pub reload_always: bool,
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
pub fn client_info_params(kind: &InstallKind) -> Vec<Value> {
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
        Value::from(match kind {
            InstallKind::Agent => "eitri-panel",
            InstallKind::Page { .. } => "eitri-page",
        }),
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
/// extras (`edge_socket` for an agent, `slot` for a page).
pub fn install_args(channel_id: u64, sockets: &Sockets, kind: &InstallKind) -> Vec<Value> {
    let mut parts = Vec::new();
    if let InstallKind::Page { id } = kind {
        if let Some(socket) = &sockets.theme {
            parts.push(part(
                "theme",
                crate::theme::feed::NVIM_THEME_LUA,
                vec![
                    ("socket", path_value(socket)),
                    ("group", Value::from(format!("EitriThemeFeed_page_{id}"))),
                ],
            ));
        }
        return vec![
            Value::from(channel_id),
            Value::Array(parts),
            map(vec![(
                "slot",
                map(vec![("kind", Value::from("page")), ("id", Value::from(id.as_str()))]),
            )]),
        ];
    }
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
    let reload = if sockets.reload_always {
        vec![("always", Value::from(true))]
    } else {
        vec![]
    };
    parts.push(part("buffer_reload", RELOAD_PART_LUA, reload));
    let extra = match &sockets.pane_switch {
        Some(socket) => map(vec![("edge_socket", path_value(socket))]),
        None => map(vec![]),
    };
    vec![Value::from(channel_id), Value::Array(parts), extra]
}

/// A feed's current attachment stamp. Active installs use even numbers; detach advances to the next odd number.
pub type FeedGeneration = std::sync::Arc<std::sync::atomic::AtomicU64>;

/// Removes an attachment stamp only when it belongs to the reader's current editor.
pub fn feed_line<'a>(line: &'a str, current: Option<&FeedGeneration>) -> Option<&'a str> {
    let stamped = line.strip_prefix("G ").and_then(|s| s.split_once(' '));
    match (stamped, current) {
        (Some((stamp, body)), current) => {
            let generation = stamp.parse::<u64>().ok()?;
            if current.is_some_and(|current| {
                current.load(std::sync::atomic::Ordering::Relaxed) != generation || generation == 0
            }) {
                None
            } else {
                Some(body)
            }
        }
        (None, None) => Some(line),
        (None, Some(_)) => None,
    }
}

pub(crate) fn stamp_install(args: &mut [Value], generation: u64) {
    if let Value::Map(extra) = &mut args[2] {
        extra.push(("generation".into(), generation.into()));
    }
}

/// The takeover uses the companion parts with a reload timer that stays on for the whole attach.
/// All addresses and environment values travel as arguments to the snippets.
pub fn takeover_install_args(channel_id: u64, parts: &crate::takeover::glue::TakeoverParts) -> Vec<Value> {
    use crate::takeover::glue::{CURSOR_PART_LUA, ENV_PART_LUA, FORK_PART_LUA, UI_PART_LUA};

    let mut sockets = parts.sockets.clone();
    sockets.reload_always = true;
    let mut args = install_args(channel_id, &sockets, &InstallKind::Agent);
    let mut items = args[1].as_array().expect("the companion parts are an array").clone();
    let strings = |words: &[String]| Value::Array(words.iter().map(|word| Value::from(word.as_str())).collect());
    let group = match &parts.commands.group {
        None => Value::Nil,
        Some((lhs, keys)) => map(vec![
            ("lhs", Value::from(lhs.as_str())),
            (
                "keys",
                Value::Array(
                    keys.iter()
                        .map(|key| {
                            map(vec![
                                ("suffix", Value::from(key.suffix.as_str())),
                                ("words", strings(&key.words)),
                                ("desc", Value::from(key.desc.as_str())),
                            ])
                        })
                        .collect(),
                ),
            ),
        ]),
    };
    items.push(part(
        "takeover_commands",
        crate::nvim_verbs::install::EITRI_COMMANDS_LUA,
        vec![
            ("socket", path_value(&parts.commands.socket)),
            ("words", strings(&parts.commands.words.first)),
            ("names", strings(&parts.commands.words.known)),
            ("modules", strings(&parts.commands.words.modules)),
            ("group", group),
            ("teardown", Value::from(true)),
        ],
    ));
    items.push(part("takeover_cursor", CURSOR_PART_LUA, vec![]));
    if let Some(swap) = &parts.tmux_swap {
        items.push(part(
            "takeover_env",
            ENV_PART_LUA,
            vec![
                ("tmux", Value::from(swap.tmux.as_str())),
                ("tmux_pane", Value::from(swap.tmux_pane.as_str())),
                ("socket", path_value(&swap.pane_switch_socket)),
                ("shim_dir", path_value(&swap.shim_dir)),
            ],
        ));
    }
    items.push(part(
        "takeover_ui",
        UI_PART_LUA,
        vec![("chan", Value::from(channel_id))],
    ));
    items.push(part("takeover_fork", FORK_PART_LUA, vec![]));
    args[1] = Value::Array(items);
    let mut extra = args[2].as_map().expect("the companion extras are a map").clone();
    extra.push((Value::from("mode"), Value::from("takeover")));
    args[2] = Value::Map(extra);
    args
}

/// What [`INSTALL_LUA`] answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    /// Absent in reports from the older injector.
    pub slot: Option<String>,
    /// The parts that loaded, in install order.
    pub installed: Vec<String>,
    /// The parts that did not: `(name, why)`.
    pub failed: Vec<(String, String)>,
    pub nvim_pid: u32,
    /// Whether nvim runs inside tmux (its environment carries `TMUX`).
    pub in_tmux: bool,
}

/// The current report version; the original report remains readable.
const REPORT_VERSION: u64 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallRefusal {
    EitriWindow,
    Takeover,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallError {
    Refused(InstallRefusal),
    Malformed(String),
}

impl From<String> for InstallError {
    fn from(why: String) -> Self {
        Self::Malformed(why)
    }
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(why) => f.write_str(why),
            Self::Refused(InstallRefusal::EitriWindow) => {
                f.write_str("this nvim is open in an Eitri window: use its panel and page keys")
            }
            Self::Refused(InstallRefusal::Takeover) => {
                f.write_str("this nvim is open in an Eitri window: its pages open there")
            }
        }
    }
}

impl std::error::Error for InstallError {}

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
pub fn parse_install_report(report: &Value) -> Result<InstallReport, InstallError> {
    if report.as_map().is_none() {
        return Err("the injector answered something that is not a report".to_owned().into());
    }
    if let Some(refused) = field(report, "refused") {
        return Err(match refused.as_str() {
            Some("eitri_window") => InstallError::Refused(InstallRefusal::EitriWindow),
            Some("takeover") => InstallError::Refused(InstallRefusal::Takeover),
            _ => InstallError::Malformed("the install report has an unknown refusal".to_owned()),
        });
    }
    let version = field(report, "version").and_then(Value::as_u64);
    match version {
        Some(1 | REPORT_VERSION) => {}
        Some(other) => {
            return Err(format!("the injector answered version {other}, this panel speaks {REPORT_VERSION}").into());
        }
        None => return Err("the install report has no version".to_owned().into()),
    }
    let text = |value: &Value, what: &str| {
        value
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("the install report has a {what} that is not text"))
    };
    let slot = match (version, field(report, "slot")) {
        (Some(1), None) => None,
        (_, Some(value)) if matches!(value.as_str(), Some("agent" | "page")) => value.as_str().map(str::to_owned),
        _ => {
            return Err(InstallError::Malformed(
                "the install report has no valid slot".to_owned(),
            ))
        }
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
        slot,
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
            reload_always: false,
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
    fn takeover_reuses_the_companion_parts_and_the_windows_own_command_options() {
        use crate::nvim_verbs::{install, leader::LeaderKey, CommandWords};
        use crate::takeover::glue::{CommandsPart, TakeoverParts};

        let keys = vec![LeaderKey {
            suffix: "x".into(),
            words: vec!["focus".into(), "lua:preview".into()],
            desc: "preview".into(),
        }];
        let parts = TakeoverParts {
            sockets: all_sockets(),
            commands: CommandsPart {
                socket: PathBuf::from("/r/takeover'\nsocket"),
                words: CommandWords {
                    first: vec!["focus".into()],
                    known: vec!["focus".into(), "show".into()],
                    modules: vec!["lua:preview".into()],
                },
                group: Some(("<Space>e".into(), keys.clone())),
            },
            tmux_swap: None,
        };
        let companion_before = install_args(7, &parts.sockets, &InstallKind::Agent);
        let args = takeover_install_args(7, &parts);
        let mut always = parts.sockets.clone();
        always.reload_always = true;
        let companion_always = install_args(7, &always, &InstallKind::Agent);
        let items = args[1].as_array().unwrap();
        assert_eq!(&items[..6], companion_always[1].as_array().unwrap().as_slice());
        let command = items
            .iter()
            .find(|p| get(p, "name").as_str() == Some("takeover_commands"))
            .unwrap();
        let expected = install::install_args(&parts.commands.socket, &parts.commands.words, Some(("<Space>e", &keys)));
        fn ordered(value: &Value) -> Value {
            match value {
                Value::Map(items) => {
                    let mut items: Vec<_> = items.iter().map(|(key, value)| (key.clone(), ordered(value))).collect();
                    items.sort_by_key(|(key, _)| key.as_str().unwrap_or_default().to_owned());
                    Value::Map(items)
                }
                Value::Array(items) => Value::Array(items.iter().map(ordered).collect()),
                other => other.clone(),
            }
        }
        // The takeover alone asks the command part for its teardown; the window's own install never does.
        let mut with_teardown = ordered(&install::to_msgpack(&expected));
        if let Value::Map(items) = &mut with_teardown {
            items.push((Value::from("teardown"), Value::from(true)));
            items.sort_by_key(|(key, _)| key.as_str().unwrap_or_default().to_owned());
        }
        assert_eq!(ordered(get(command, "opts")), with_teardown);
        assert_eq!(get(&args[2], "mode").as_str(), Some("takeover"));
        assert_eq!(get(items.last().unwrap(), "name").as_str(), Some("takeover_fork"));
        assert_eq!(install_args(7, &parts.sockets, &InstallKind::Agent), companion_before);
    }

    /// A host that never reports its agent's activity asks for the reload timer at all times; the others leave the
    /// part's options empty, so its timer follows the panel's busy calls.
    #[test]
    fn reload_always_reaches_the_reload_part_and_only_then() {
        let reload_opts = |sockets: &Sockets| {
            let args = install_args(7, sockets, &InstallKind::Agent);
            let parts = args[1].as_array().unwrap().clone();
            let part = parts
                .iter()
                .find(|p| get(p, "name").as_str() == Some("buffer_reload"))
                .expect("the reload part is installed")
                .clone();
            assert_eq!(get(&part, "src").as_str(), Some(RELOAD_PART_LUA));
            get(&part, "opts").clone()
        };
        let always = reload_opts(&Sockets {
            reload_always: true,
            ..all_sockets()
        });
        assert_eq!(get(&always, "always").as_bool(), Some(true));
        assert_eq!(always.as_map().map(Vec::len), Some(1));
        assert_eq!(reload_opts(&all_sockets()).as_map().map(Vec::len), Some(0));
        assert_eq!(reload_opts(&Sockets::default()).as_map().map(Vec::len), Some(0));
    }

    #[test]
    fn install_args_lists_parts_in_order_and_leaves_out_a_missing_socket() {
        let args = install_args(7, &all_sockets(), &InstallKind::Agent);
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
            RELOAD_PART_LUA,
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
            &InstallKind::Agent,
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
            &InstallKind::Agent,
        );
        assert_eq!(
            names(&no_nav),
            ["editor_context", "theme", "keys", "scratch", "buffer_reload"]
        );
    }

    #[test]
    fn page_args_install_only_the_theme_in_its_own_group() {
        let kind = InstallKind::Page {
            id: "sample-page".into(),
        };
        let args = install_args(8, &all_sockets(), &kind);
        assert_eq!(names(&args), ["theme"]);
        let theme = &args[1].as_array().unwrap()[0];
        assert_eq!(get(get(theme, "opts"), "socket").as_str(), Some("/r/th"));
        assert_eq!(
            get(get(theme, "opts"), "group").as_str(),
            Some("EitriThemeFeed_page_sample-page")
        );
        let slot = get(&args[2], "slot");
        assert_eq!(get(slot, "kind").as_str(), Some("page"));
        assert_eq!(get(slot, "id").as_str(), Some("sample-page"));
        assert_eq!(args[2].as_map().unwrap().len(), 1);
        assert!(names(&install_args(8, &Sockets::default(), &kind)).is_empty());
    }

    #[test]
    fn page_ids_have_bounded_ascii_names_and_no_edge_hyphens() {
        for id in ["a", "0", "a--b", &"a".repeat(40)] {
            assert!(page_id_ok(id), "{id}");
        }
        for id in ["", "-", "-a", "a-", "A", "a_b", "a/b", "a\n", "é", &"a".repeat(41)] {
            assert!(!page_id_ok(id), "{id}");
        }
    }

    #[test]
    fn a_refusal_reads_as_the_sentence_for_its_own_editor() {
        assert_eq!(
            InstallError::Refused(InstallRefusal::EitriWindow).to_string(),
            "this nvim is open in an Eitri window: use its panel and page keys"
        );
        assert_eq!(
            InstallError::Refused(InstallRefusal::Takeover).to_string(),
            "this nvim is open in an Eitri window: its pages open there"
        );
    }

    #[test]
    fn reports_read_both_slots_and_distinguish_refusals_from_malformed_answers() {
        for slot in ["agent", "page"] {
            let mut value = report(Value::Array(vec![]), Value::Array(vec![]));
            let Value::Map(pairs) = &mut value else { unreachable!() };
            pairs[0].1 = Value::from(2);
            pairs.push((Value::from("slot"), Value::from(slot)));
            assert_eq!(parse_install_report(&value).unwrap().slot.as_deref(), Some(slot));
        }
        for (name, refusal) in [
            ("eitri_window", InstallRefusal::EitriWindow),
            ("takeover", InstallRefusal::Takeover),
        ] {
            assert_eq!(
                parse_install_report(&map(vec![("refused", Value::from(name))])),
                Err(InstallError::Refused(refusal))
            );
        }
        for value in [Value::from(false), map(vec![("refused", Value::from("other"))])] {
            assert!(matches!(parse_install_report(&value), Err(InstallError::Malformed(_))));
        }
        let mut value = report(Value::Array(vec![]), Value::Array(vec![]));
        if let Value::Map(pairs) = &mut value {
            pairs[0].1 = Value::from(2);
        }
        assert!(matches!(parse_install_report(&value), Err(InstallError::Malformed(_))));
        if let Value::Map(pairs) = &mut value {
            pairs.push((Value::from("slot"), Value::from("other")));
        }
        assert!(matches!(parse_install_report(&value), Err(InstallError::Malformed(_))));
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
                slot: None,
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
            pairs[0].1 = Value::from(3);
        }
        assert_eq!(
            parse_install_report(&value),
            Err(InstallError::Malformed(
                "the injector answered version 3, this panel speaks 2".to_owned()
            ))
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
            parse_install_report(&value).unwrap_err().to_string()
        };
        assert_eq!(without("pid"), "the install report has no pid");
        assert!(without("installed").contains("installed"));
        assert!(without("failed").contains("failed"));
        assert!(without("tmux").contains("tmux"));
        assert!(without("version").contains("version"));
    }

    #[test]
    fn client_info_names_a_remote_eitri_panel() {
        let info = client_info_params(&InstallKind::Agent);
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
    fn old_context_and_direction_generations_are_dropped_even_after_reattach() {
        use std::sync::atomic::Ordering::Relaxed;
        let current: FeedGeneration = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1));
        for payload in [r#"{"v":1,"file":"old.rs"}"#, "R"] {
            let old = format!("G 1 {payload}");
            assert_eq!(feed_line(&old, Some(&current)), Some(payload));
            current.store(0, Relaxed);
            assert_eq!(feed_line(&old, Some(&current)), None);
            current.store(2, Relaxed);
            assert_eq!(feed_line(&old, Some(&current)), None);
            assert_eq!(feed_line(payload, Some(&current)), None);
            assert_eq!(feed_line(&format!("G 2 {payload}"), Some(&current)), Some(payload));
            current.store(1, Relaxed);
        }
    }

    #[test]
    fn the_install_stamps_every_part_and_the_edge_hook_with_its_generation() {
        let mut args = install_args(3, &all_sockets(), &InstallKind::Agent);
        stamp_install(&mut args, 42);
        let lua = mlua::Lua::new();
        lua.load(
            r#"
            captured = {}; loadstring = load
            vim = { env = {}, fn = { getpid = function() return 1 end },
                uv = { new_timer = function() return { start = function() end } end },
                schedule_wrap = function(f) return f end }
        "#,
        )
        .exec()
        .unwrap();
        let parts = lua.create_table().unwrap();
        for (i, part) in args[1].as_array().unwrap().iter().enumerate() {
            let table = lua.create_table().unwrap();
            table.set("name", get(part, "name").as_str().unwrap()).unwrap();
            table
                .set("src", "local opts = ...; captured[#captured + 1] = opts.generation")
                .unwrap();
            parts.set(i + 1, table).unwrap();
        }
        let extra = lua.create_table_from([("generation", 42)]).unwrap();
        let _: mlua::Value = lua.load(INSTALL_LUA).call((3, parts, extra)).unwrap();
        let captured: mlua::Table = lua.globals().get("captured").unwrap();
        assert_eq!(captured.raw_len(), args[1].as_array().unwrap().len());
        for generation in captured.sequence_values::<u64>() {
            assert_eq!(generation.unwrap(), 42);
        }
        assert_eq!(get(&args[2], "generation").as_u64(), Some(42));
    }

    #[test]
    fn an_agent_install_refuses_an_embedded_editor_before_running_parts() {
        let lua = mlua::Lua::new();
        lua.load("__eitri_window = {}; __eitri_companion = { teardown = function() touched = true end }")
            .exec()
            .unwrap();
        let parts = lua
            .create_sequence_from([lua
                .create_table_from([
                    ("name", "editor_context"),
                    (
                        "src",
                        "touched = true; vim.api.nvim_create_augroup('eitri_editor_watch', { clear = true })",
                    ),
                ])
                .unwrap()])
            .unwrap();
        let reply: mlua::Table = lua
            .load(INSTALL_LUA)
            .call((7, parts, lua.create_table().unwrap()))
            .unwrap();
        assert_eq!(reply.get::<String>("refused").unwrap(), "eitri_window");
        assert!(lua.globals().get::<Option<bool>>("touched").unwrap().is_none());
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
