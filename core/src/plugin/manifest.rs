//! A local plugin's manifest, `eitri-plugin.toml`: who the plugin is, the page it serves, and the closed
//! set of host capabilities it asks for. Every key is checked and one this build does not know is refused:
//! a misspelt limit that silently did nothing is the failure a manifest must not have.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::Deserialize;

use crate::layout::rules::{parse_at, At};
use crate::layout::spec::{self, LuaTree, SpecNode, SplitKind};
use crate::layout::ModuleId;

/// The bridge protocol this build speaks.
pub const PROTOCOL: u32 = 1;
/// The longest a page-supplied variable may be declared.
pub const MAX_VAR_LEN: usize = 256;
const MAX_ID_LEN: usize = 40;
const MAX_NAME_LEN: usize = 32;
const MAX_TITLE_CHARS: usize = 80;
/// A day: the longest timeout a task may declare. `0` means none.
const MAX_TIMEOUT_SECS: u64 = 86_400;

/// The most filetypes a `[follow]` table may list.
pub const MAX_FILETYPES: usize = 16;
/// The most nvim commands a plugin may define.
pub const MAX_COMMANDS: usize = 8;
/// The longest nvim command name a plugin may define.
pub const MAX_COMMAND_NAME: usize = 40;
/// The most values a `choice` setting may list.
pub const MAX_CHOICES: usize = 8;

/// What a page may be given. A closed set: a name outside it refuses the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Capability {
    Tasks,
    EditorOpen,
    EditorAssist,
    LayoutFocus,
    State,
    Net,
    EditorFollow,
    EditorCommands,
    EditorCwd,
    EditorBuffers,
    TerminalCwd,
}

impl Capability {
    pub const ALL: [Capability; 11] = [
        Capability::Tasks,
        Capability::EditorOpen,
        Capability::EditorAssist,
        Capability::LayoutFocus,
        Capability::State,
        Capability::Net,
        Capability::EditorFollow,
        Capability::EditorCommands,
        Capability::EditorCwd,
        Capability::EditorBuffers,
        Capability::TerminalCwd,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Capability::Tasks => "tasks",
            Capability::EditorOpen => "editor.open",
            Capability::EditorAssist => "editor.assist",
            Capability::LayoutFocus => "layout.focus",
            Capability::State => "state",
            Capability::Net => "net",
            Capability::EditorFollow => "editor.follow",
            Capability::EditorCommands => "editor.commands",
            Capability::EditorCwd => "editor.cwd",
            Capability::EditorBuffers => "editor.buffers",
            Capability::TerminalCwd => "terminal.cwd",
        }
    }

    pub fn parse(text: &str) -> Option<Capability> {
        Capability::ALL.into_iter().find(|c| c.as_str() == text)
    }
}

/// The characters a page-supplied variable may hold. A closed set of three: a pattern language would
/// need a regular-expression engine this tree does not ship.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharClass {
    LowerDigitDash,
    LowerDigit,
    Digit,
}

impl CharClass {
    pub fn parse(text: &str) -> Option<CharClass> {
        match text {
            "lower-digit-dash" => Some(CharClass::LowerDigitDash),
            "lower-digit" => Some(CharClass::LowerDigit),
            "digit" => Some(CharClass::Digit),
            _ => None,
        }
    }

    /// Whether `value` is in this class. Never empty, and never starting or ending with `-`: a value
    /// becomes one argument of a program the host runs, and one starting with a dash would be read by
    /// that program as an option.
    pub fn admits(self, value: &str) -> bool {
        let fits = |b: u8| match self {
            CharClass::LowerDigitDash => b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-',
            CharClass::LowerDigit => b.is_ascii_lowercase() || b.is_ascii_digit(),
            CharClass::Digit => b.is_ascii_digit(),
        };
        !value.is_empty() && !value.starts_with('-') && !value.ends_with('-') && value.bytes().all(fits)
    }
}

/// One part of an argument or path template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    Text(String),
    /// `${plugin}`: the plugin's directory, canonical.
    Plugin,
    /// `${<setting>}`: a value the user's `init.lua` gave, or the manifest's default.
    Setting(String),
    /// `${<var>}`: a value the page supplies, checked against its declared class and length.
    Var(String),
}

/// A string with `${name}` placeholders, split once at load. There is no shell and no word splitting:
/// a template is always exactly one argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template(Vec<Piece>);

impl Template {
    pub fn parse(text: &str, settings: &BTreeSet<String>, vars: &BTreeSet<String>) -> Result<Template, String> {
        let mut pieces = Vec::new();
        let mut literal = String::new();
        let mut rest = text;
        while let Some(at) = rest.find("${") {
            literal.push_str(&rest[..at]);
            let after = &rest[at + 2..];
            let end = after
                .find('}')
                .ok_or_else(|| format!("{text:?}: a `${{` has no closing `}}`"))?;
            let name = &after[..end];
            let piece = if name == "plugin" {
                Piece::Plugin
            } else if settings.contains(name) {
                Piece::Setting(name.to_owned())
            } else if vars.contains(name) {
                Piece::Var(name.to_owned())
            } else {
                return Err(format!("{text:?}: unknown placeholder ${{{name}}}"));
            };
            if !literal.is_empty() {
                pieces.push(Piece::Text(std::mem::take(&mut literal)));
            }
            pieces.push(piece);
            rest = &after[end + 1..];
        }
        literal.push_str(rest);
        if !literal.is_empty() {
            pieces.push(Piece::Text(literal));
        }
        Ok(Template(pieces))
    }

    pub fn pieces(&self) -> &[Piece] {
        &self.0
    }

    pub fn uses_vars(&self) -> bool {
        self.0.iter().any(|p| matches!(p, Piece::Var(_)))
    }

    pub fn vars(&self) -> Vec<&str> {
        self.0
            .iter()
            .filter_map(|p| match p {
                Piece::Var(name) => Some(name.as_str()),
                _ => None,
            })
            .collect()
    }
}

/// An `https` origin a page may connect to: a lower-case host name and an optional port, nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    host: String,
    port: Option<u16>,
}

impl Origin {
    pub fn parse(text: &str) -> Option<Origin> {
        let rest = text.strip_prefix("https://")?;
        let (host, port) = match rest.split_once(':') {
            Some((host, port)) => {
                // `u16::from_str` takes a leading `+`; a port is digits only.
                if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                (host, Some(port.parse::<u16>().ok().filter(|p| *p != 0)?))
            }
            None => (rest, None),
        };
        let host_ok = !host.is_empty()
            && host.len() <= 253
            && !host.starts_with(['.', '-'])
            && !host.ends_with(['.', '-'])
            && host
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-');
        host_ok.then(|| Origin {
            host: host.to_owned(),
            port,
        })
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    /// The port a connection to this origin uses: the written one, or `https`'s 443.
    pub fn port(&self) -> u16 {
        self.port.unwrap_or(443)
    }

    pub fn as_str(&self) -> String {
        match self.port {
            Some(port) => format!("https://{}:{port}", self.host),
            None => format!("https://{}", self.host),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingKind {
    /// A directory, created 0700 at load when missing.
    Dir,
    /// `true` or `false`.
    Bool,
    /// One of a closed list of words, in the order the manifest lists them.
    Choice(Vec<String>),
}

impl SettingKind {
    pub fn name(&self) -> &'static str {
        match self {
            SettingKind::Dir => "dir",
            SettingKind::Bool => "bool",
            SettingKind::Choice(_) => "choice",
        }
    }
}

/// A setting's value: a `dir` or `choice` setting holds text, a `bool` setting a boolean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingValue {
    Text(String),
    Bool(bool),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    pub default: SettingValue,
    pub kind: SettingKind,
}

/// `[follow]`: the filetypes whose buffers the page may follow, and the bool setting that shows the module
/// by itself when such a buffer becomes current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FollowDecl {
    pub filetypes: Vec<String>,
    pub auto_show: Option<String>,
}

/// What an nvim command the plugin defines does to the plugin's own module.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandDoes {
    Show,
    Toggle,
    Hide,
}

impl CommandDoes {
    pub fn as_str(self) -> &'static str {
        match self {
            CommandDoes::Show => "show",
            CommandDoes::Toggle => "toggle",
            CommandDoes::Hide => "hide",
        }
    }
}

/// `[commands.<Name>]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandDecl {
    pub name: String,
    pub does: CommandDoes,
}

/// Whether a first launch puts the module on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleStart {
    Shown,
    Hidden,
}

/// `[module]`: how the module starts, and the plugin's own open rule (`at`, `size`, `focus`).
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleTable {
    pub start: ModuleStart,
    pub at: Option<At>,
    pub size: Option<f32>,
    pub focus: bool,
}

/// `[net] images`: whether the page may load `https` images from any host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetImages {
    Https,
    Off,
    /// `"${<setting>}"`: a `choice` setting whose values are `https` and `off`.
    Setting(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VarRule {
    pub chars: CharClass,
    pub max_len: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskDecl {
    /// `argv[0]` is `${plugin}/...` and uses no variable; the rest may use any.
    pub argv: Vec<Template>,
    /// Uses `${plugin}` and settings only, so it is checked at load.
    pub cwd: Template,
    /// `None` when the manifest says `0`: the run lasts until cancelled or the plugin stops.
    pub timeout: Option<Duration>,
}

/// Which module receives the keys when the plugin's own window opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WindowFocus {
    #[default]
    Page,
    Editor,
    Terminal,
}

/// A plugin's declaration of its own project window.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowDecl {
    pub root: Template,
    pub title: String,
    pub focus: WindowFocus,
    pub layout: LuaTree,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    pub id: String,
    pub title: String,
    pub site: String,
    pub entry: String,
    /// In the order the manifest lists them, without repeats.
    pub capabilities: Vec<Capability>,
    pub settings: BTreeMap<String, Setting>,
    /// Templates over `${plugin}` and `dir` settings; made canonical paths at load.
    pub roots: BTreeMap<String, Template>,
    pub origins: Vec<Origin>,
    pub vars: BTreeMap<String, VarRule>,
    pub tasks: BTreeMap<String, TaskDecl>,
    pub follow: Option<FollowDecl>,
    /// In name order.
    pub commands: Vec<CommandDecl>,
    pub module: Option<ModuleTable>,
    pub window: Option<WindowDecl>,
    pub images: Option<NetImages>,
    /// `[page] handoff`: the page answers `host.save` with its state, which the page that replaces it in another
    /// window receives in its `host.hello` reply. False unless the manifest says so.
    pub handoff: bool,
}

impl Manifest {
    pub fn has(&self, capability: Capability) -> bool {
        self.capabilities.contains(&capability)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    id: String,
    title: String,
    protocol: u32,
    site: String,
    entry: String,
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    settings: BTreeMap<String, RawSetting>,
    #[serde(default)]
    roots: BTreeMap<String, String>,
    net: Option<RawNet>,
    #[serde(default)]
    vars: BTreeMap<String, RawVar>,
    #[serde(default)]
    tasks: BTreeMap<String, RawTask>,
    follow: Option<RawFollow>,
    #[serde(default)]
    commands: BTreeMap<String, RawCommand>,
    module: Option<RawModule>,
    page: Option<RawPage>,
    window: Option<RawWindow>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawWindow {
    root: String,
    title: String,
    focus: Option<String>,
    layout: toml::Value,
}

/// Reads TOML's node spelling; the shared builder checks the resulting tree.
fn window_node(value: &toml::Value, at: &str) -> Result<SpecNode, String> {
    // A bare string is shorthand for `{ module = "<name>" }` with no share; the Lua grammar has no such spelling.
    if let toml::Value::String(name) = value {
        return Ok(SpecNode::Module {
            name: name.clone(),
            share: None,
        });
    }
    let table = value
        .as_table()
        .ok_or_else(|| format!("{at}: expected a module name or a table"))?;
    let heads: Vec<&str> = ["module", "row", "column", "tabbed", "stacked"]
        .into_iter()
        .filter(|head| table.contains_key(*head))
        .collect();
    if heads.len() != 1 {
        return Err(format!(
            "{at}: exactly one of module, row, column, tabbed or stacked is required"
        ));
    }
    let head = heads[0];
    for key in table.keys() {
        if key != head && key != "share" && !(key == "active" && matches!(head, "tabbed" | "stacked")) {
            return Err(format!("{at}: unknown field {key}"));
        }
    }
    let share = table
        .get("share")
        .map(|value| {
            value
                .as_float()
                .or_else(|| value.as_integer().map(|n| n as f64))
                .ok_or_else(|| format!("{at}: a share is a number, got {}", value.type_str()))
        })
        .transpose()?;
    if head == "module" {
        let name = table[head]
            .as_str()
            .ok_or_else(|| format!("{at}: module must be a string"))?;
        return Ok(SpecNode::Module {
            name: name.into(),
            share,
        });
    }
    let values = table[head]
        .as_array()
        .ok_or_else(|| format!("{at}: {head} must be an array of nodes"))?;
    let children = values
        .iter()
        .enumerate()
        .map(|(i, value)| window_node(value, &format!("item {} of the {head}", i + 1)))
        .collect::<Result<Vec<_>, _>>()?;
    // An integral float names the same child as the integer, as it does in Lua; the range is checked by the
    // builder, which words the refusal for both grammars.
    let active = match table.get("active") {
        None => None,
        Some(toml::Value::Integer(n)) => Some(*n),
        Some(toml::Value::Float(n)) if n.fract() == 0.0 && *n >= 1.0 && *n <= children.len() as f64 => Some(*n as i64),
        Some(toml::Value::Float(n)) => {
            return Err(format!("{at}: active = {n}: the {head} has {} items", children.len()));
        }
        Some(_) => return Err(format!("{at}: active must be an integer")),
    };
    Ok(SpecNode::Split {
        kind: SplitKind::parse(head).unwrap(),
        children,
        share,
        active,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSetting {
    default: toml::Value,
    kind: String,
    values: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNet {
    origins: Option<Vec<String>>,
    images: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawVar {
    chars: Option<String>,
    max_len: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTask {
    argv: Vec<String>,
    cwd: String,
    timeout_secs: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFollow {
    filetypes: Vec<String>,
    auto_show: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommand {
    does: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPage {
    handoff: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawModule {
    start: Option<String>,
    at: Option<String>,
    size: Option<f64>,
    focus: Option<bool>,
}

/// `[a-z0-9_]`, starting with a letter, at most 32: a setting, variable, root or task name.
fn name_ok(name: &str) -> bool {
    name.len() <= MAX_NAME_LEN
        && name.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// A path relative to the plugin directory with no way out of it.
fn relative_path_ok(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && path.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
}

/// A plugin id: `[a-z0-9-]`, at most 40, not starting or ending with `-`.
pub fn id_ok(id: &str) -> bool {
    CharClass::LowerDigitDash.admits(id) && id.len() <= MAX_ID_LEN
}

/// A `choice` value or a filetype: lower-case letters, digits and `_` (and `-` for a choice), 1-32 bytes.
fn word_ok(word: &str, dash: bool) -> bool {
    !word.is_empty()
        && word.len() <= MAX_NAME_LEN
        && word
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || (dash && b == b'-'))
}

/// nvim's rule for a user command, `[A-Z][A-Za-z0-9]*`, at most 40.
fn command_name_ok(name: &str) -> bool {
    name.len() <= MAX_COMMAND_NAME
        && name.bytes().next().is_some_and(|b| b.is_ascii_uppercase())
        && name.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// A path or an argument may name only `dir` settings: a boolean or a choice word has no meaning there.
fn dir_settings_only(template: &Template, settings: &BTreeMap<String, Setting>, key: &str) -> Result<(), String> {
    for piece in template.pieces() {
        if let Piece::Setting(name) = piece {
            if let Some(setting) = settings.get(name) {
                if setting.kind != SettingKind::Dir {
                    return Err(format!(
                        "eitri-plugin.toml: {key}: ${{{name}}} is a {} setting; only a dir setting may be part of a \
                         path or an argument",
                        setting.kind.name()
                    ));
                }
            }
        }
    }
    Ok(())
}

pub fn parse_manifest(text: &str) -> Result<Manifest, String> {
    let raw: RawManifest = toml::from_str(text).map_err(|e| format!("eitri-plugin.toml: {e}"))?;
    let fail = |key: &str, why: String| Err(format!("eitri-plugin.toml: {key}: {why}"));

    if !id_ok(&raw.id) {
        return fail(
            "id",
            format!(
                "{:?} must be [a-z0-9-], at most {MAX_ID_LEN}, not starting or ending with -",
                raw.id
            ),
        );
    }
    if raw.title.trim().is_empty()
        || raw.title.chars().count() > MAX_TITLE_CHARS
        || raw.title.chars().any(char::is_control)
    {
        return fail("title", format!("must be 1-{MAX_TITLE_CHARS} printable characters"));
    }
    if raw.protocol != PROTOCOL {
        return fail(
            "protocol",
            format!("{} is not the protocol this Eitri speaks ({PROTOCOL})", raw.protocol),
        );
    }
    if !relative_path_ok(&raw.site) {
        return fail(
            "site",
            format!("{:?} must be a relative path inside the plugin directory", raw.site),
        );
    }
    if !relative_path_ok(&raw.entry) {
        return fail(
            "entry",
            format!("{:?} must be a relative path inside the site", raw.entry),
        );
    }

    let mut capabilities = Vec::new();
    for name in &raw.capabilities {
        let capability = Capability::parse(name).ok_or_else(|| {
            let known: Vec<&str> = Capability::ALL.iter().map(|c| c.as_str()).collect();
            format!(
                "eitri-plugin.toml: capabilities: unknown capability {name:?} (known: {})",
                known.join(", ")
            )
        })?;
        if !capabilities.contains(&capability) {
            capabilities.push(capability);
        }
    }

    let mut settings = BTreeMap::new();
    for (name, setting) in &raw.settings {
        // `key` and `position` are options `eitri.plugin.load` reads itself, so a setting of either name
        // could never be given a value.
        if !name_ok(name) || matches!(name.as_str(), "plugin" | "key" | "position") {
            return fail(
                &format!("settings.{name}"),
                "a setting name is [a-z][a-z0-9_]*, at most 32, not plugin, key or position".into(),
            );
        }
        let kind = match setting.kind.as_str() {
            "dir" => SettingKind::Dir,
            "bool" => SettingKind::Bool,
            "choice" => {
                let key = format!("settings.{name}.values");
                let Some(values) = &setting.values else {
                    return fail(&key, "a choice setting lists its values".into());
                };
                if values.is_empty() || values.len() > MAX_CHOICES {
                    return fail(&key, format!("must list 1-{MAX_CHOICES} values"));
                }
                for (i, value) in values.iter().enumerate() {
                    if !word_ok(value, true) {
                        return fail(&key, format!("{value:?} must be [a-z0-9_-], 1-32"));
                    }
                    if values[..i].contains(value) {
                        return fail(&key, format!("{value:?} is listed twice"));
                    }
                }
                SettingKind::Choice(values.clone())
            }
            other => {
                return fail(
                    &format!("settings.{name}.kind"),
                    format!("{other:?} is not a kind (dir, bool, choice)"),
                );
            }
        };
        if setting.values.is_some() && !matches!(kind, SettingKind::Choice(_)) {
            return fail(
                &format!("settings.{name}.values"),
                "only a choice setting has values".into(),
            );
        }
        let key = format!("settings.{name}.default");
        let default = match (&kind, &setting.default) {
            (SettingKind::Dir, toml::Value::String(text)) if text.trim().is_empty() => {
                return fail(&key, "must not be empty".into());
            }
            (SettingKind::Dir, toml::Value::String(text)) => SettingValue::Text(text.clone()),
            (SettingKind::Dir, _) => return fail(&key, "a dir setting's default is a path".into()),
            (SettingKind::Bool, toml::Value::Boolean(b)) => SettingValue::Bool(*b),
            (SettingKind::Bool, _) => return fail(&key, "a bool setting's default is true or false".into()),
            (SettingKind::Choice(values), toml::Value::String(text)) if values.contains(text) => {
                SettingValue::Text(text.clone())
            }
            (SettingKind::Choice(values), _) => {
                let listed: Vec<String> = values.iter().map(|v| format!("{v:?}")).collect();
                return fail(&key, format!("must be one of {}", listed.join(", ")));
            }
        };
        settings.insert(name.clone(), Setting { default, kind });
    }
    let setting_names: BTreeSet<String> = settings.keys().cloned().collect();

    let mut vars = BTreeMap::new();
    for (name, var) in &raw.vars {
        let key = format!("vars.{name}");
        if !name_ok(name) || name == "plugin" || setting_names.contains(name) {
            return fail(
                &key,
                "a variable name is [a-z][a-z0-9_]*, at most 32, not plugin and not a setting's".into(),
            );
        }
        let Some(chars) = &var.chars else {
            return fail(
                &key,
                "no character class (chars = \"lower-digit-dash\" | \"lower-digit\" | \"digit\")".into(),
            );
        };
        let chars = CharClass::parse(chars).ok_or_else(|| {
            format!("eitri-plugin.toml: {key}.chars: {chars:?} is not lower-digit-dash, lower-digit or digit")
        })?;
        let max_len = match var.max_len {
            Some(n) if n >= 1 && n <= MAX_VAR_LEN as u64 => n as usize,
            Some(n) => {
                return fail(
                    &format!("{key}.max_len"),
                    format!("{n} is not between 1 and {MAX_VAR_LEN}"),
                );
            }
            None => {
                return fail(
                    &format!("{key}.max_len"),
                    "missing: every variable states its length".into(),
                );
            }
        };
        vars.insert(name.clone(), VarRule { chars, max_len });
    }
    let var_names: BTreeSet<String> = vars.keys().cloned().collect();
    let none = BTreeSet::new();

    let mut roots = BTreeMap::new();
    for (name, text) in &raw.roots {
        if !name_ok(name) {
            return fail(
                &format!("roots.{name}"),
                "a root name is [a-z][a-z0-9_]*, at most 32".into(),
            );
        }
        let template = Template::parse(text, &setting_names, &none)
            .map_err(|e| format!("eitri-plugin.toml: roots.{name}: {e}"))?;
        dir_settings_only(&template, &settings, &format!("roots.{name}"))?;
        roots.insert(name.clone(), template);
    }

    for capability in [Capability::EditorCwd, Capability::TerminalCwd] {
        if capabilities.contains(&capability) && raw.window.is_none() {
            return fail(
                "capabilities",
                format!("{} needs a [window] table", capability.as_str()),
            );
        }
    }
    let window = raw
        .window
        .as_ref()
        .map(|window| {
            let title = &window.title;
            if title.is_empty() || title.chars().count() > 40 || title.chars().any(super::keys::hidden) {
                return Err(concat!(
                    "eitri-plugin.toml: window.title: must be 1-40 characters ",
                    "without controls or hidden characters"
                )
                .into());
            }
            let root = Template::parse(&window.root, &setting_names, &none)
                .map_err(|e| format!("eitri-plugin.toml: window.root: {e}"))?;
            dir_settings_only(&root, &settings, "window.root")?;
            let focus = match window.focus.as_deref().unwrap_or("page") {
                "page" => WindowFocus::Page,
                "editor" => WindowFocus::Editor,
                "terminal" => WindowFocus::Terminal,
                other => {
                    return Err(format!(
                        "eitri-plugin.toml: window.focus: {other:?} must be page, editor or terminal"
                    ))
                }
            };
            let node = window_node(&window.layout, "the tree")
                .map_err(|e| format!("eitri-plugin.toml: window.layout: {e}"))?;
            let layout = spec::build(&node, &|name| match name {
                "page" => Ok(ModuleId::lua(&raw.id)),
                "editor" => Ok(ModuleId::editor()),
                "terminal" => Ok(ModuleId::terminal()),
                "chat" => Ok(ModuleId::agent()),
                _ => Err(format!(
                    "unknown module {name:?}; expected page, editor, terminal or chat"
                )),
            })
            .map_err(|e| format!("eitri-plugin.toml: window.layout: {e}"))?;
            Ok(WindowDecl {
                root,
                title: title.clone(),
                focus,
                layout,
            })
        })
        .transpose()?;

    let (origins, images) = match (&raw.net, capabilities.contains(&Capability::Net)) {
        (Some(_), false) => return fail("[net]", "declared without the net capability".into()),
        (Some(net), true) => {
            if net.origins.is_none() && net.images.is_none() {
                return fail("[net]", "names neither origins nor images".into());
            }
            let origins = net
                .origins
                .iter()
                .flatten()
                .map(|o| {
                    Origin::parse(o).ok_or_else(|| {
                        format!(
                            "eitri-plugin.toml: net.origins: {o:?} is not an https origin (no path, no trailing slash)"
                        )
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            let images = match net.images.as_deref() {
                None => None,
                Some("https") => Some(NetImages::Https),
                Some("off") => Some(NetImages::Off),
                Some(text) => {
                    let named = text
                        .strip_prefix("${")
                        .and_then(|rest| rest.strip_suffix('}'))
                        .and_then(|name| settings.get(name).map(|s| (name, s)));
                    match named {
                        Some((
                            name,
                            Setting {
                                kind: SettingKind::Choice(values),
                                ..
                            },
                        )) if values.iter().all(|v| v == "https" || v == "off") => {
                            Some(NetImages::Setting(name.to_owned()))
                        }
                        _ => {
                            return fail(
                                "net.images",
                                format!(
                                    "{text:?} must be \"https\", \"off\" or \"${{<setting>}}\" naming a choice setting \
                                     whose values are https and off"
                                ),
                            );
                        }
                    }
                }
            };
            (origins, images)
        }
        (None, _) => (Vec::new(), None),
    };

    if !raw.tasks.is_empty() && !capabilities.contains(&Capability::Tasks) {
        return fail("[tasks]", "declared without the tasks capability".into());
    }
    let mut tasks = BTreeMap::new();
    for (name, task) in &raw.tasks {
        let key = format!("tasks.{name}");
        if !CharClass::LowerDigitDash.admits(name) || name.len() > MAX_ID_LEN {
            return fail(&key, "a task name is [a-z0-9-], at most 40".into());
        }
        let mut argv = Vec::new();
        for (i, arg) in task.argv.iter().enumerate() {
            let template = Template::parse(arg, &setting_names, &var_names)
                .map_err(|e| format!("eitri-plugin.toml: {key}.argv[{i}]: {e}"))?;
            dir_settings_only(&template, &settings, &format!("{key}.argv[{i}]"))?;
            argv.push(template);
        }
        let first_ok = matches!(argv.first().map(Template::pieces), Some([Piece::Plugin, Piece::Text(t), ..]) if t.starts_with('/'))
            && !argv[0].uses_vars();
        if !first_ok {
            return fail(
                &format!("{key}.argv[0]"),
                "must be ${plugin}/<path> with no variable: only the plugin's own executables run".into(),
            );
        }
        let cwd = Template::parse(&task.cwd, &setting_names, &none)
            .map_err(|e| format!("eitri-plugin.toml: {key}.cwd: {e}"))?;
        dir_settings_only(&cwd, &settings, &format!("{key}.cwd"))?;
        if task.timeout_secs > MAX_TIMEOUT_SECS {
            return fail(
                &format!("{key}.timeout_secs"),
                format!("{} is over a day ({MAX_TIMEOUT_SECS})", task.timeout_secs),
            );
        }
        let timeout = (task.timeout_secs > 0).then(|| Duration::from_secs(task.timeout_secs));
        tasks.insert(name.clone(), TaskDecl { argv, cwd, timeout });
    }

    let needs_roots = capabilities
        .iter()
        .any(|c| matches!(c, Capability::EditorOpen | Capability::EditorAssist))
        || !tasks.is_empty();
    if needs_roots && roots.is_empty() {
        return fail(
            "[roots]",
            "editor.open, editor.assist and tasks need at least one root".into(),
        );
    }

    let follow = match (&raw.follow, capabilities.contains(&Capability::EditorFollow)) {
        (Some(_), false) => return fail("[follow]", "declared without the editor.follow capability".into()),
        (None, true) => return fail("[follow]", "editor.follow needs a [follow] table".into()),
        (None, false) => None,
        (Some(f), true) => {
            if f.filetypes.is_empty() || f.filetypes.len() > MAX_FILETYPES {
                return fail("follow.filetypes", format!("must list 1-{MAX_FILETYPES} filetypes"));
            }
            let mut filetypes: Vec<String> = Vec::new();
            for filetype in &f.filetypes {
                if !word_ok(filetype, false) {
                    return fail("follow.filetypes", format!("{filetype:?} must be [a-z0-9_], 1-32"));
                }
                if !filetypes.contains(filetype) {
                    filetypes.push(filetype.clone());
                }
            }
            if let Some(name) = &f.auto_show {
                if !matches!(
                    settings.get(name),
                    Some(Setting {
                        kind: SettingKind::Bool,
                        ..
                    })
                ) {
                    return fail("follow.auto_show", format!("{name:?} must name a bool setting"));
                }
            }
            Some(FollowDecl {
                filetypes,
                auto_show: f.auto_show.clone(),
            })
        }
    };

    let has_commands = capabilities.contains(&Capability::EditorCommands);
    if !raw.commands.is_empty() && !has_commands {
        return fail("[commands]", "declared without the editor.commands capability".into());
    }
    if has_commands && raw.commands.is_empty() {
        return fail(
            "[commands]",
            "editor.commands needs at least one [commands.<Name>]".into(),
        );
    }
    if raw.commands.len() > MAX_COMMANDS {
        return fail("[commands]", format!("at most {MAX_COMMANDS} commands"));
    }
    let mut commands = Vec::new();
    for (name, command) in &raw.commands {
        if !command_name_ok(name) {
            return fail(
                &format!("commands.{name}"),
                format!("a command name is [A-Z][A-Za-z0-9]*, at most {MAX_COMMAND_NAME}"),
            );
        }
        let does = match command.does.as_str() {
            "show" => CommandDoes::Show,
            "toggle" => CommandDoes::Toggle,
            "hide" => CommandDoes::Hide,
            other => {
                return fail(
                    &format!("commands.{name}.does"),
                    format!("{other:?} is not show, toggle or hide"),
                );
            }
        };
        commands.push(CommandDecl {
            name: name.clone(),
            does,
        });
    }

    let module = match &raw.module {
        None => None,
        Some(m) => {
            let start = match m.start.as_deref() {
                None | Some("shown") => ModuleStart::Shown,
                Some("hidden") => ModuleStart::Hidden,
                Some(other) => return fail("module.start", format!("{other:?} is not shown or hidden")),
            };
            let at = match m.at.as_deref() {
                None => None,
                Some(name) => match parse_at(name) {
                    Some(at) => Some(at),
                    None => {
                        return fail(
                            "module.at",
                            format!(
                                "{name:?} is not a place (side, bottom, main, below-editor, right, below, left, \
                                 above, tab)"
                            ),
                        );
                    }
                },
            };
            let size = match m.size {
                None => None,
                Some(share) if share > 0.0 && share < 1.0 && (share as f32) < 1.0 => Some(share as f32),
                Some(share) => {
                    return fail(
                        "module.size",
                        format!("{share} must be a share strictly between 0 and 1"),
                    );
                }
            };
            Some(ModuleTable {
                start,
                at,
                size,
                focus: m.focus.unwrap_or(true),
            })
        }
    };

    Ok(Manifest {
        id: raw.id,
        title: raw.title,
        site: raw.site,
        entry: raw.entry,
        capabilities,
        settings,
        roots,
        origins,
        vars,
        tasks,
        follow,
        commands,
        module,
        window,
        images,
        handoff: raw.page.and_then(|p| p.handoff).unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("testdata/demo-manifest.toml");

    const WINDOW_BASE: &str = r#"id = "demo"
title = "Demo"
protocol = 1
site = "site"
entry = "index.html"
[roots]
work = "${plugin}"
"#;

    fn window_manifest(fields: &str) -> String {
        format!("{WINDOW_BASE}\n[window]\nroot = \"${{plugin}}\"\n{fields}")
    }

    #[test]
    fn window_parses_root_title_focus_and_layout() {
        let m = parse_manifest(&window_manifest(
            r#"title = "Study"
focus = "editor"
layout = { row = [{ module = "page", share = 0.55 }, { column = [{ module = "editor" }, { module = "terminal" }] }, { module = "chat", share = 0.1 }] }"#,
        )).unwrap();
        let window = m.window.unwrap();
        assert_eq!(window.root, m.roots["work"]);
        assert_eq!(window.title, "Study");
        assert_eq!(window.focus, WindowFocus::Editor);
        assert_eq!(
            window.layout.tiled.leaves(),
            vec![
                crate::layout::ModuleId::lua("demo"),
                crate::layout::ModuleId::editor(),
                crate::layout::ModuleId::terminal(),
                crate::layout::ModuleId::agent(),
            ]
        );
        assert!(window.layout.floats.is_empty());
    }

    #[test]
    fn window_focus_defaults_to_page() {
        let m = parse_manifest(&window_manifest("title = 'Demo'\nlayout = { module = 'page' }")).unwrap();
        assert_eq!(m.window.unwrap().focus, WindowFocus::Page);
    }

    #[test]
    fn window_title_refuses_41_chars_a_control_and_a_hidden_character() {
        for title in [
            "x".repeat(41),
            "".into(),
            "line\nbreak".into(),
            "hidden\u{200b}".into(),
            "line\u{2028}".into(),
            "paragraph\u{2029}".into(),
            "direction\u{2066}".into(),
            "filler\u{3164}".into(),
            "delete\u{7f}".into(),
        ] {
            let fields = format!(
                "title = {}\nlayout = {{ module = 'page' }}",
                toml::Value::String(title).to_string()
            );
            let err = parse_manifest(&window_manifest(&fields)).unwrap_err();
            assert!(err.contains("window.title"), "{err}");
        }
        let title = "字".repeat(40);
        assert!(parse_manifest(&window_manifest(&format!(
            "title = '{title}'\nlayout = {{ module = 'page' }}"
        )))
        .is_ok());
    }

    #[test]
    fn window_focus_accepts_terminal_and_refuses_unknown_names() {
        let fields = "title = 'Demo'\nfocus = 'terminal'\nlayout = { module = 'terminal' }";
        assert_eq!(
            parse_manifest(&window_manifest(fields)).unwrap().window.unwrap().focus,
            WindowFocus::Terminal
        );
        let err = parse_manifest(&window_manifest(
            &fields.replace("focus = 'terminal'", "focus = 'chat'"),
        ))
        .unwrap_err();
        assert!(err.contains("window.focus") && err.contains("chat"), "{err}");
    }

    #[test]
    fn window_capabilities_parse_with_a_window_and_round_trip() {
        let text = window_manifest("title = 'Demo'\nlayout = { module = 'page' }").replacen(
            "[roots]",
            "capabilities = ['editor.cwd', 'editor.buffers', 'terminal.cwd']\n[roots]",
            1,
        );
        let manifest = parse_manifest(&text).unwrap();
        assert_eq!(
            manifest.capabilities,
            vec![
                Capability::EditorCwd,
                Capability::EditorBuffers,
                Capability::TerminalCwd
            ]
        );
        for capability in Capability::ALL {
            assert_eq!(Capability::parse(capability.as_str()), Some(capability));
        }
    }

    #[test]
    fn window_layout_refuses_an_unknown_module_name() {
        let err = parse_manifest(&window_manifest("title = 'Demo'\nlayout = { module = 'other' }")).unwrap_err();
        assert!(err.contains("other") && err.contains("window.layout"), "{err}");
    }

    #[test]
    fn window_layout_accepts_a_bare_string_as_a_module_node() {
        let bare = parse_manifest(&window_manifest(
            r#"title = "Study"
layout = { row = [ { module = "page", share = 0.42 }, { column = [ "editor", { module = "terminal", share = 0.3 } ] } ] }"#,
        ))
        .unwrap();
        let tables = parse_manifest(&window_manifest(
            r#"title = "Study"
layout = { row = [ { module = "page", share = 0.42 }, { column = [ { module = "editor" }, { module = "terminal", share = 0.3 } ] } ] }"#,
        ))
        .unwrap();
        assert_eq!(bare.window.unwrap().layout, tables.window.unwrap().layout);
    }

    #[test]
    fn window_layout_accepts_a_bare_string_at_the_root() {
        let bare = parse_manifest(&window_manifest("title = 'Demo'\nlayout = \"editor\"")).unwrap();
        let table = parse_manifest(&window_manifest("title = 'Demo'\nlayout = { module = 'editor' }")).unwrap();
        assert_eq!(bare.window.unwrap().layout, table.window.unwrap().layout);
    }

    #[test]
    fn window_layout_refuses_a_bare_unknown_name_like_the_table_form() {
        let refusal =
            |layout: &str| parse_manifest(&window_manifest(&format!("title = 'Demo'\nlayout = {layout}"))).unwrap_err();
        assert_eq!(refusal("'other'"), refusal("{ module = 'other' }"));
        assert_eq!(
            refusal("{ row = [ 'editor', 'other' ] }"),
            refusal("{ row = [ { module = 'editor' }, { module = 'other' } ] }")
        );
    }

    #[test]
    fn window_layout_refuses_two_node_heads() {
        let err = parse_manifest(&window_manifest(
            "title = 'Demo'\nlayout = { module = 'page', row = [] }",
        ))
        .unwrap_err();
        assert!(err.contains("exactly one"), "{err}");
        let err = parse_manifest(&window_manifest(
            "title = 'Demo'\nlayout = { module = 'page', module = 'editor' }",
        ))
        .unwrap_err();
        assert!(err.contains("duplicate key"), "{err}");
    }

    #[test]
    fn editor_cwd_without_window_is_refused_naming_it() {
        let text = WINDOW_BASE.replace("[roots]", "capabilities = ['editor.cwd']\n[roots]");
        let err = parse_manifest(&text).unwrap_err();
        assert!(err.contains("editor.cwd") && err.contains("[window]"), "{err}");
    }

    #[test]
    fn terminal_cwd_without_window_is_refused_naming_it() {
        let text = WINDOW_BASE.replace("[roots]", "capabilities = ['terminal.cwd']\n[roots]");
        let err = parse_manifest(&text).unwrap_err();
        assert!(err.contains("terminal.cwd") && err.contains("[window]"), "{err}");
    }

    #[test]
    fn editor_buffers_needs_no_window() {
        let text = WINDOW_BASE.replace("[roots]", "capabilities = ['editor.buffers']\n[roots]");
        assert!(parse_manifest(&text).unwrap().has(Capability::EditorBuffers));
    }

    #[test]
    fn the_example_manifest_parses() {
        let m = parse_manifest(EXAMPLE).expect("the example is valid");
        assert_eq!(m.id, "demo");
        assert_eq!(m.capabilities, Capability::ALL[..8].to_vec());
        assert_eq!(m.settings["work"].default, SettingValue::Text("~/demo-work".into()));
        assert_eq!(m.settings["work"].kind, SettingKind::Dir);
        assert_eq!(
            m.settings["auto"],
            Setting {
                default: SettingValue::Bool(false),
                kind: SettingKind::Bool
            }
        );
        assert_eq!(
            m.settings["images"],
            Setting {
                default: SettingValue::Text("https".into()),
                kind: SettingKind::Choice(vec!["https".into(), "off".into()])
            }
        );
        assert_eq!(m.images, Some(NetImages::Setting("images".into())));
        assert_eq!(
            m.follow,
            Some(FollowDecl {
                filetypes: vec!["markdown".into(), "rst".into()],
                auto_show: Some("auto".into())
            })
        );
        assert_eq!(
            m.commands,
            [
                CommandDecl {
                    name: "DemoShow".into(),
                    does: CommandDoes::Show
                },
                CommandDecl {
                    name: "DemoToggle".into(),
                    does: CommandDoes::Toggle
                },
            ]
        );
        assert_eq!(
            m.module,
            Some(ModuleTable {
                start: ModuleStart::Hidden,
                at: parse_at("right"),
                size: Some(0.4),
                focus: false
            })
        );
        assert_eq!(Capability::ALL.len(), 11);
        assert_eq!(Capability::parse("editor.follow"), Some(Capability::EditorFollow));
        assert_eq!(Capability::parse("editor.commands"), Some(Capability::EditorCommands));
        assert_eq!(
            m.vars["course"],
            VarRule {
                chars: CharClass::LowerDigitDash,
                max_len: 40
            }
        );
        assert_eq!(m.vars["exercise"].max_len, 64);
        assert_eq!(m.tasks.len(), 3);
        assert_eq!(m.tasks["watch"].timeout, None);
        assert_eq!(m.tasks["prepare"].timeout, Some(std::time::Duration::from_secs(60)));
        assert_eq!(m.tasks["prepare"].argv[0].pieces()[0], Piece::Plugin);
        assert!(m.tasks["prepare"].argv[2].uses_vars());
        assert_eq!(
            m.origins.iter().map(Origin::as_str).collect::<Vec<_>>(),
            ["https://learn.example.org"]
        );
        assert!(m.has(Capability::Net));
    }

    /// Each edit of the example must be refused, with a message naming what was wrong.
    #[test]
    fn every_broken_rule_is_refused_by_name() {
        let cases: &[(&str, &str, &str)] = &[
            ("title = \"Demo\"", "titel = \"Demo\"", "titel"),
            ("protocol = 1", "protocol = 2", "protocol"),
            ("id = \"demo\"", "id = \"Demo_1\"", "id"),
            ("site = \"build/site\"", "site = \"../site\"", "site"),
            ("entry = \"index.html\"", "entry = \"/index.html\"", "entry"),
            (
                "\"net\", \"editor.follow\"",
                "\"net.raw\", \"editor.follow\"",
                "net.raw",
            ),
            ("\"state\", \"net\", ", "\"state\", ", "[net]"),
            (
                "chars = \"lower-digit-dash\"\nmax_len = 40",
                "max_len = 40",
                "character class",
            ),
            (
                "chars = \"lower-digit-dash\"\nmax_len = 40",
                "chars = \"lower-digit-dash\"",
                "max_len",
            ),
            ("max_len = 40", "max_len = 0", "max_len"),
            ("max_len = 40", "max_len = 257", "max_len"),
            (
                "chars = \"lower-digit-dash\"\nmax_len = 40",
                "chars = \"[a-z]+\"\nmax_len = 40",
                "[a-z]+",
            ),
            ("kind = \"dir\"", "kind = \"file\"", "kind"),
            (
                "\"${plugin}/bin/learn\", \"prepare\"",
                "\"/usr/bin/learn\", \"prepare\"",
                "argv[0]",
            ),
            (
                "\"${plugin}/bin/learn\", \"prepare\"",
                "\"${plugin}/${course}\", \"prepare\"",
                "argv[0]",
            ),
            ("\"prepare\", \"${course}\"", "\"prepare\", \"${nope}\"", "${nope}"),
            ("\"prepare\", \"${course}\"", "\"prepare\", \"${course\"", "closing"),
            (
                "timeout_secs = 60\n\n[tasks.watch]",
                "timeout_secs = 90000\n\n[tasks.watch]",
                "timeout_secs",
            ),
            (
                "cwd = \"${work}\"\ntimeout_secs = 60\n\n[tasks.watch]",
                "cwd = \"${work}/${course}\"\ntimeout_secs = 60\n\n[tasks.watch]",
                "cwd",
            ),
            (
                "origins = [\"https://learn.example.org\"]",
                "origins = [\"https://learn.example.org/sync\"]",
                "origin",
            ),
            (
                "origins = [\"https://learn.example.org\"]",
                "origins = [\"http://learn.example.org\"]",
                "origin",
            ),
            ("[vars.exercise]", "[vars.work]", "work"),
            // Added beside `work`, so the error can only be about the name ("plugin" alone is in every error's
            // `eitri-plugin.toml`). `eitri.plugin.load` reads `key` and `position` itself, so a setting of
            // either name could never be set.
            (
                "[settings]\nwork",
                "[settings]\nplugin = { default = false, kind = \"bool\" }\nwork",
                "settings.plugin",
            ),
            (
                "[settings]\nwork",
                "[settings]\nkey = { default = false, kind = \"bool\" }\nwork",
                "settings.key",
            ),
            (
                "[settings]\nwork",
                "[settings]\nposition = { default = false, kind = \"bool\" }\nwork",
                "settings.position",
            ),
        ];
        for (from, to, named) in cases {
            assert!(EXAMPLE.contains(from), "the example has no {from:?}");
            let text = EXAMPLE.replacen(from, to, 1);
            let err = parse_manifest(&text).expect_err(&format!("{to:?} was accepted"));
            assert!(err.contains(named), "{to:?}: {err:?} does not name {named:?}");
        }
    }

    #[test]
    fn tasks_need_the_tasks_capability_and_editor_verbs_need_a_root() {
        let no_tasks = EXAMPLE.replacen("[\"tasks\", ", "[", 1);
        assert!(parse_manifest(&no_tasks).unwrap_err().contains("tasks"));
        let no_roots = EXAMPLE
            .replacen("[roots]\nwork = \"${work}\"\n", "", 1)
            .replace("cwd = \"${work}\"", "cwd = \"${plugin}\"");
        assert!(parse_manifest(&no_roots).unwrap_err().contains("roots"));
    }

    #[test]
    fn a_class_never_admits_an_empty_value_or_a_leading_or_trailing_dash() {
        let c = CharClass::LowerDigitDash;
        assert!(c.admits("rust") && c.admits("ch04-02-fix-total-len") && c.admits("go1"));
        for bad in ["", "-rf", "--work", "a-", "Rust", "a_b", "a b", "ä", "a/b"] {
            assert!(!c.admits(bad), "{bad:?}");
        }
        assert!(CharClass::LowerDigit.admits("abc12") && !CharClass::LowerDigit.admits("a-b"));
        assert!(CharClass::Digit.admits("042") && !CharClass::Digit.admits("4a") && !CharClass::Digit.admits(""));
    }

    /// Run by hand against a plugin's real manifest:
    /// `EITRI_TEST_PLUGIN_MANIFEST=<path> cargo test -p eitri-core --lib -- --ignored an_external_manifest_parses`.
    #[test]
    #[ignore = "reads the manifest named by EITRI_TEST_PLUGIN_MANIFEST"]
    fn an_external_manifest_parses() {
        let path = std::env::var_os("EITRI_TEST_PLUGIN_MANIFEST").expect("set EITRI_TEST_PLUGIN_MANIFEST");
        let text = std::fs::read_to_string(&path).unwrap();
        let m = parse_manifest(&text).unwrap_or_else(|e| panic!("{e}"));
        println!("{}: {} task(s), capabilities {:?}", m.id, m.tasks.len(), m.capabilities);
    }

    #[test]
    fn a_template_splits_into_text_and_placeholders() {
        let settings: BTreeSet<String> = ["work".to_string()].into();
        let vars: BTreeSet<String> = ["course".to_string()].into();
        let t = Template::parse("${plugin}/x-${course}.${work}$", &settings, &vars).unwrap();
        assert_eq!(
            t.pieces(),
            [
                Piece::Plugin,
                Piece::Text("/x-".into()),
                Piece::Var("course".into()),
                Piece::Text(".".into()),
                Piece::Setting("work".into()),
                Piece::Text("$".into()),
            ]
        );
        assert_eq!(t.vars(), ["course"]);
    }
    /// Each malformed example must be refused with a message naming what is wrong.
    #[test]
    fn every_new_rule_is_refused_by_name() {
        let cases: &[(&str, &str, &str)] = &[
            // [follow] and editor.follow need each other.
            (
                "\"net\", \"editor.follow\", \"editor.commands\"]",
                "\"net\", \"editor.commands\"]",
                "[follow]",
            ),
            (
                "[follow]\nfiletypes = [\"markdown\", \"rst\"]\nauto_show = \"auto\"\n",
                "",
                "[follow]",
            ),
            (
                "filetypes = [\"markdown\", \"rst\"]",
                "filetypes = []",
                "follow.filetypes",
            ),
            (
                "filetypes = [\"markdown\", \"rst\"]",
                "filetypes = [\"Markdown\"]",
                "\"Markdown\"",
            ),
            (
                "filetypes = [\"markdown\", \"rst\"]",
                "filetypes = [\"mark-down\"]",
                "\"mark-down\"",
            ),
            ("auto_show = \"auto\"", "auto_show = \"images\"", "follow.auto_show"),
            ("auto_show = \"auto\"", "auto_show = \"nope\"", "follow.auto_show"),
            // [commands] and editor.commands need each other.
            (
                "\"net\", \"editor.follow\", \"editor.commands\"]",
                "\"net\", \"editor.follow\"]",
                "[commands]",
            ),
            (
                "[commands.DemoShow]\ndoes = \"show\"\n\n[commands.DemoToggle]\ndoes = \"toggle\"",
                "",
                "[commands]",
            ),
            ("[commands.DemoShow]", "[commands.demoShow]", "commands.demoShow"),
            ("[commands.DemoShow]", "[commands.Demo_Show]", "commands.Demo_Show"),
            ("does = \"toggle\"", "does = \"flip\"", "commands.DemoToggle.does"),
            ("does = \"toggle\"", "does = \"toggle\"\nwhen = \"x\"", "when"),
            // Setting kinds.
            (
                "auto = { default = false, kind = \"bool\" }",
                "auto = { default = \"no\", kind = \"bool\" }",
                "settings.auto.default",
            ),
            (
                "auto = { default = false, kind = \"bool\" }",
                "auto = { default = false, kind = \"flag\" }",
                "settings.auto.kind",
            ),
            ("values = [\"https\", \"off\"]", "values = []", "settings.images.values"),
            (
                "values = [\"https\", \"off\"]",
                "values = [\"https\", \"https\"]",
                "twice",
            ),
            (
                "values = [\"https\", \"off\"]",
                "values = [\"https\", \"Off\"]",
                "\"Off\"",
            ),
            (
                "images = { default = \"https\"",
                "images = { default = \"ftp\"",
                "settings.images.default",
            ),
            (
                "images = { default = \"https\", kind = \"choice\", values = [\"https\", \"off\"] }",
                "images = { default = \"https\", kind = \"choice\" }",
                "settings.images.values",
            ),
            (
                "work = { default = \"~/demo-work\", kind = \"dir\" }",
                "work = { default = \"~/demo-work\", kind = \"dir\", values = [\"a\"] }",
                "settings.work.values",
            ),
            (
                "work = { default = \"~/demo-work\", kind = \"dir\" }",
                "work = { default = true, kind = \"dir\" }",
                "settings.work.default",
            ),
            // A path or an argument names only dir settings.
            (
                "[roots]\nwork = \"${work}\"\n",
                "[roots]\nwork = \"${work}\"\nflag = \"${auto}\"\n",
                "${auto}",
            ),
            (
                "cwd = \"${work}\"\ntimeout_secs = 0",
                "cwd = \"${images}\"\ntimeout_secs = 0",
                "${images}",
            ),
            // [net] images.
            ("images = \"${images}\"", "images = \"${auto}\"", "net.images"),
            ("images = \"${images}\"", "images = \"http\"", "net.images"),
            (
                "origins = [\"https://learn.example.org\"]\nimages = \"${images}\"\n",
                "",
                "[net]",
            ),
            // [module].
            ("start = \"hidden\"", "start = \"later\"", "module.start"),
            ("at = \"right\"", "at = \"up\"", "module.at"),
            ("size = 0.4", "size = 1.0", "module.size"),
            ("size = 0.4", "size = 0.0", "module.size"),
            ("focus = false", "focus = false\nfloat = true", "float"),
            ("focus = false", "focus = false\nscratchpad = true", "scratchpad"),
        ];
        for (from, to, named) in cases {
            assert!(EXAMPLE.contains(from), "the example has no {from:?}");
            let text = EXAMPLE.replacen(from, to, 1);
            let err = parse_manifest(&text).expect_err(&format!("{to:?} was accepted"));
            assert!(err.contains(named), "{to:?}: {err:?} does not name {named:?}");
        }
    }

    #[test]
    fn a_command_name_is_at_most_40_and_a_plugin_defines_at_most_8() {
        let long = format!("[commands.D{}]", "a".repeat(MAX_COMMAND_NAME));
        let err = parse_manifest(&EXAMPLE.replacen("[commands.DemoShow]", &long, 1)).unwrap_err();
        assert!(err.contains("commands.Daaa"), "{err}");
        let fits = format!("[commands.D{}]", "a".repeat(MAX_COMMAND_NAME - 1));
        assert!(parse_manifest(&EXAMPLE.replacen("[commands.DemoShow]", &fits, 1)).is_ok());

        let mut many = EXAMPLE.to_string();
        for i in 0..MAX_COMMANDS - 1 {
            many.push_str(&format!("\n[commands.Extra{i}]\ndoes = \"hide\"\n"));
        }
        let err = parse_manifest(&many).unwrap_err();
        assert!(err.contains("[commands]") && err.contains("at most 8"), "{err}");
    }

    #[test]
    fn a_follow_table_folds_repeated_filetypes_and_holds_at_most_16() {
        let twice = EXAMPLE.replacen(
            "filetypes = [\"markdown\", \"rst\"]",
            "filetypes = [\"markdown\", \"markdown\"]",
            1,
        );
        assert_eq!(parse_manifest(&twice).unwrap().follow.unwrap().filetypes, ["markdown"]);
        let list: Vec<String> = (0..=MAX_FILETYPES).map(|i| format!("\"t{i}\"")).collect();
        let many = EXAMPLE.replacen(
            "filetypes = [\"markdown\", \"rst\"]",
            &format!("filetypes = [{}]", list.join(", ")),
            1,
        );
        assert!(parse_manifest(&many).unwrap_err().contains("follow.filetypes"));
    }

    #[test]
    fn module_and_net_tables_are_optional_and_default_as_today() {
        let no_module = &EXAMPLE[..EXAMPLE.find("\n[module]\n").unwrap() + 1];
        let no_module = no_module.replacen(", \"editor.follow\", \"editor.commands\"]", "]", 1);
        let m = parse_manifest(&no_module).unwrap();
        assert_eq!((m.module, m.follow, m.commands.len()), (None, None, 0));
        let shown = EXAMPLE.replacen("start = \"hidden\"\nat = \"right\"\nsize = 0.4\nfocus = false", "", 1);
        assert_eq!(
            parse_manifest(&shown).unwrap().module,
            Some(ModuleTable {
                start: ModuleStart::Shown,
                at: None,
                size: None,
                focus: true
            })
        );
        let origins_only = EXAMPLE.replacen("images = \"${images}\"\n", "", 1);
        assert_eq!(parse_manifest(&origins_only).unwrap().images, None);
        let images_only = EXAMPLE.replacen("origins = [\"https://learn.example.org\"]\n", "", 1);
        let m = parse_manifest(&images_only).unwrap();
        assert!(m.origins.is_empty());
        assert_eq!(m.images, Some(NetImages::Setting("images".into())));
        for (value, want) in [("\"https\"", NetImages::Https), ("\"off\"", NetImages::Off)] {
            let text = EXAMPLE.replacen("images = \"${images}\"", &format!("images = {value}"), 1);
            assert_eq!(parse_manifest(&text).unwrap().images, Some(want));
        }
    }

    #[test]
    fn a_page_table_with_handoff_parses_and_an_unknown_page_key_is_refused() {
        assert!(
            !parse_manifest(EXAMPLE).unwrap().handoff,
            "a page hands nothing over unless its manifest says so"
        );
        let with = |table: &str| format!("{EXAMPLE}\n[page]\n{table}");
        assert!(parse_manifest(&with("handoff = true\n")).unwrap().handoff);
        assert!(!parse_manifest(&with("handoff = false\n")).unwrap().handoff);
        assert!(
            !parse_manifest(&with("")).unwrap().handoff,
            "an empty table keeps the default"
        );
        let err = parse_manifest(&with("handoff = true\nrestore = true\n")).unwrap_err();
        assert!(err.contains("restore"), "{err}");
        let err = parse_manifest(&with("handoff = \"yes\"\n")).unwrap_err();
        assert!(err.contains("handoff"), "{err}");
    }

    /// The preview plugin's manifest as it ships, comments included.
    #[test]
    fn a_follow_plugin_manifest_parses() {
        let text = r#"id = "markdown"
title = "Markdown preview"
protocol = 1
site = "web/dist"                   # the build directory, the same path installed and in the source tree
entry = "index.html"
capabilities = ["editor.follow", "editor.commands", "net"]

[module]
start = "hidden"                    # "shown" (the default) | "hidden": not placed at a first launch
at = "right"                        # the declared open rule: an `at` value of eitri.layout.rule
focus = false

[follow]
filetypes = ["markdown"]            # 1-16 names, each [a-z0-9_]{1,32}
auto_show = "auto"                  # names a bool setting; optional

[commands.EitriMarkdown]
does = "show"                       # show | toggle | hide
[commands.EitriMarkdownToggle]
does = "toggle"
[commands.EitriMarkdownHide]
does = "hide"

[settings.auto]
kind = "bool"
default = false

[settings.remote_images]
kind = "choice"
values = ["https", "off"]
default = "https"

[settings.sync]
kind = "choice"
values = ["both", "editor", "off"]
default = "both"

[net]
images = "${remote_images}"         # resolves to "https" or "off"
"#;
        let m = parse_manifest(text).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            m.capabilities,
            [Capability::EditorFollow, Capability::EditorCommands, Capability::Net]
        );
        assert!(m.roots.is_empty() && m.origins.is_empty());
        assert_eq!(m.images, Some(NetImages::Setting("remote_images".into())));
        let names: Vec<(&str, CommandDoes)> = m.commands.iter().map(|c| (c.name.as_str(), c.does)).collect();
        assert_eq!(
            names,
            [
                ("EitriMarkdown", CommandDoes::Show),
                ("EitriMarkdownHide", CommandDoes::Hide),
                ("EitriMarkdownToggle", CommandDoes::Toggle),
            ]
        );
        assert_eq!(
            m.settings["sync"].kind,
            SettingKind::Choice(vec!["both".into(), "editor".into(), "off".into()])
        );
        let module = m.module.unwrap();
        assert_eq!(
            (module.start, module.at, module.size, module.focus),
            (ModuleStart::Hidden, parse_at("right"), None, false)
        );
    }

    /// The Markdown preview's own manifest is an ordinary one: every table it uses is a general one.
    #[test]
    fn the_markdown_plugin_manifest_parses() {
        let text = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../plugins/markdown/eitri-plugin.toml"
        ));
        let m = parse_manifest(text).expect("the plugin's manifest parses");
        assert_eq!(
            (m.id.as_str(), m.site.as_str(), m.entry.as_str()),
            ("markdown", "web/dist", "index.html")
        );
        assert_eq!(
            m.capabilities,
            vec![Capability::EditorFollow, Capability::EditorCommands, Capability::Net]
        );
        let module = m.module.as_ref().expect("[module]");
        assert_eq!(module.start, ModuleStart::Hidden);
        assert_eq!(module.at, crate::layout::rules::parse_at("right"));
        assert_eq!(module.size, None);
        assert!(!module.focus);
        assert_eq!(
            m.follow,
            Some(FollowDecl {
                filetypes: vec!["markdown".into()],
                auto_show: Some("auto".into())
            })
        );
        let commands: Vec<(&str, CommandDoes)> = m.commands.iter().map(|c| (c.name.as_str(), c.does)).collect();
        assert_eq!(
            commands,
            vec![
                ("EitriMarkdown", CommandDoes::Show),
                ("EitriMarkdownHide", CommandDoes::Hide),
                ("EitriMarkdownToggle", CommandDoes::Toggle),
            ]
        );
        assert_eq!(m.images, Some(NetImages::Setting("remote_images".into())));
        assert_eq!(
            m.settings["auto"],
            Setting {
                default: SettingValue::Bool(false),
                kind: SettingKind::Bool
            }
        );
        assert_eq!(
            m.settings["remote_images"],
            Setting {
                default: SettingValue::Text("https".into()),
                kind: SettingKind::Choice(vec!["https".into(), "off".into()]),
            }
        );
        assert_eq!(
            m.settings["sync"],
            Setting {
                default: SettingValue::Text("both".into()),
                kind: SettingKind::Choice(vec!["both".into(), "editor".into(), "off".into()]),
            }
        );
        assert!(m.origins.is_empty() && m.roots.is_empty() && m.tasks.is_empty());
    }
}
