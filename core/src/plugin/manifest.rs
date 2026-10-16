//! A local plugin's manifest, `eitri-plugin.toml`: who the plugin is, the page it serves, and the closed
//! set of host capabilities it asks for. Every key is checked and one this build does not know is refused:
//! a misspelt limit that silently did nothing is the failure a manifest must not have.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::Deserialize;

/// The bridge protocol this build speaks.
pub const PROTOCOL: u32 = 1;
/// The longest a page-supplied variable may be declared.
pub const MAX_VAR_LEN: usize = 256;
const MAX_ID_LEN: usize = 40;
const MAX_NAME_LEN: usize = 32;
const MAX_TITLE_CHARS: usize = 80;
/// A day: the longest timeout a task may declare. `0` means none.
const MAX_TIMEOUT_SECS: u64 = 86_400;

/// What a page may be given. A closed set: a name outside it refuses the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Capability {
    Tasks,
    EditorOpen,
    EditorAssist,
    LayoutFocus,
    State,
    Net,
}

impl Capability {
    pub const ALL: [Capability; 6] = [
        Capability::Tasks,
        Capability::EditorOpen,
        Capability::EditorAssist,
        Capability::LayoutFocus,
        Capability::State,
        Capability::Net,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Capability::Tasks => "tasks",
            Capability::EditorOpen => "editor.open",
            Capability::EditorAssist => "editor.assist",
            Capability::LayoutFocus => "layout.focus",
            Capability::State => "state",
            Capability::Net => "net",
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

    pub fn as_str(&self) -> String {
        match self.port {
            Some(port) => format!("https://{}:{port}", self.host),
            None => format!("https://{}", self.host),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKind {
    /// A directory, created 0700 at load when missing.
    Dir,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    pub default: String,
    pub kind: SettingKind,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub id: String,
    pub title: String,
    pub site: String,
    pub entry: String,
    /// In the order the manifest lists them, without repeats.
    pub capabilities: Vec<Capability>,
    pub settings: BTreeMap<String, Setting>,
    /// Templates over `${plugin}` and settings; made canonical paths at load.
    pub roots: BTreeMap<String, Template>,
    pub origins: Vec<Origin>,
    pub vars: BTreeMap<String, VarRule>,
    pub tasks: BTreeMap<String, TaskDecl>,
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
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSetting {
    default: String,
    kind: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNet {
    origins: Vec<String>,
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

pub fn parse_manifest(text: &str) -> Result<Manifest, String> {
    let raw: RawManifest = toml::from_str(text).map_err(|e| format!("eitri-plugin.toml: {e}"))?;
    let fail = |key: &str, why: String| Err(format!("eitri-plugin.toml: {key}: {why}"));

    if !CharClass::LowerDigitDash.admits(&raw.id) || raw.id.len() > MAX_ID_LEN {
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
        if !name_ok(name) || name == "plugin" {
            return fail(
                &format!("settings.{name}"),
                "a setting name is [a-z][a-z0-9_]*, at most 32, not plugin".into(),
            );
        }
        let kind = match setting.kind.as_str() {
            "dir" => SettingKind::Dir,
            other => {
                return fail(
                    &format!("settings.{name}.kind"),
                    format!("{other:?} is not a kind (dir)"),
                );
            }
        };
        if setting.default.trim().is_empty() {
            return fail(&format!("settings.{name}.default"), "must not be empty".into());
        }
        settings.insert(
            name.clone(),
            Setting {
                default: setting.default.clone(),
                kind,
            },
        );
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
        roots.insert(name.clone(), template);
    }

    let origins = match (&raw.net, capabilities.contains(&Capability::Net)) {
        (Some(_), false) => return fail("[net]", "declared without the net capability".into()),
        (Some(net), true) => net
            .origins
            .iter()
            .map(|o| {
                Origin::parse(o).ok_or_else(|| {
                    format!("eitri-plugin.toml: net.origins: {o:?} is not an https origin (no path, no trailing slash)")
                })
            })
            .collect::<Result<Vec<_>, _>>()?,
        (None, _) => Vec::new(),
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
            argv.push(
                Template::parse(arg, &setting_names, &var_names)
                    .map_err(|e| format!("eitri-plugin.toml: {key}.argv[{i}]: {e}"))?,
            );
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("testdata/demo-manifest.toml");

    #[test]
    fn the_example_manifest_parses() {
        let m = parse_manifest(EXAMPLE).expect("the example is valid");
        assert_eq!(m.id, "demo");
        assert_eq!(m.capabilities, Capability::ALL.to_vec());
        assert_eq!(m.settings["work"].default, "~/demo-work");
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
            ("\"state\", \"net\"]", "\"state\", \"net.raw\"]", "net.raw"),
            ("\"state\", \"net\"]", "\"state\"]", "[net]"),
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
            ("[settings]\nwork", "[settings]\nplugin", "plugin"),
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
}
