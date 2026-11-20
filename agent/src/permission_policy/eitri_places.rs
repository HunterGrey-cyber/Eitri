//! The places Eitri reads its own configuration and code from when it starts, and the calls that may write there.
//!
//! A file written into one of these places takes effect at Eitri's next start, with nobody asked: `init.lua` and
//! what it `require`s, a plugin that overrides a shipped one, Eitri's private Neovim and sidecar copies, the saved
//! prefix rules, the trust records and the remembered tab modes. So a call that writes there, or may, is a card in
//! every mode but Bypass, and is never left to the CLI's own auto classifier.
//!
//! The places ([`EitriPlaces::from_values`]):
//! - the configuration directory, by the rule Eitri reads it with: an absolute `EITRI_CONFIG_DIR`, else an absolute
//!   `$XDG_CONFIG_HOME/eitri`, and `~/.config/eitri` always (the Mac's, and the one a later start without the
//!   variable reads);
//! - `<data home>/eitri` (`$XDG_DATA_HOME`, absolute, and `~/.local/share` always): user plugins, which win over the
//!   shipped ones, the private Neovim, the sidecar, the Lua language server;
//! - `<state home>/eitri` (`$XDG_STATE_HOME`, absolute, and `~/.local/state` always): saved prefix rules, trust
//!   records, remembered modes, layouts, saved tabs;
//! - `<dir>/eitri` for each system data directory the plugin search reads (`XDG_DATA_DIRS`);
//! - `~/.local/lib/eitri`, where the installer puts the program, its launcher `~/.local/bin/eitri`, and the directory
//!   of the running binary (the shipped plugins and the sidecar sit beside it), unless that lies inside the project
//!   root: there it is a build tree the project's own build rewrites.
//! - any further directories the caller names (`EitriPlaceInputs::extra`): on a server, the remote helper's own data
//!   and state directories.
//!
//! Each place counts as named, as resolved, and by where every link inside it leads, so a file a dotfiles manager
//! linked into the configuration directory is a place wherever it lives (to the depth [`Kind::link_depth`] gives).
//! A place or a link whose spelling cannot be established cards every judged call.
//!
//! What is judged, by tool:
//! - `Write`, `Edit`, `MultiEdit`, `NotebookEdit`: the target, as written and as resolved (links followed wherever
//!   they lead, a missing tail placed under its nearest existing ancestor), against every spelling of every place,
//!   matched component by component with the protected names' own folding (case, the two confusable letters, the
//!   invisible characters). A raw path with `..` or a leading `~`, a target whose resolution cannot be established,
//!   and a hard-linked file (another of its names may lie in a place) are cards too.
//! - `Bash`, `Monitor`: the command as text, since there is no shell parser. It cards when it names a place by any
//!   spelling this policy can list -- absolute, below the home directory (so `~/`, `$HOME/` and `/home/<u>/` all
//!   match), below the project root, with repeated `/` and `/./` settled -- or names one of the variables that decide
//!   them, or when one of its first words that hold a `/` resolves into a place. A command that builds the path
//!   some other way is not seen: a tripwire for what models write, not a boundary.
//! - any tool this policy does not know: every string in its input, as for a command, and every string spelled as
//!   an absolute or `~` path judged as an edit target is.
//! - Reads (`Read`, `Grep`, `Glob`), the tools allowed by name and `WebFetch`/`WebSearch` write nothing locally and
//!   are not judged here.

use super::{
    ask, fold_confusables, resolve, spend, with_work_budget, Classification, Resolution, ALWAYS_ASK_TOOLS,
    MAX_RESOLUTION_STEPS, MAX_WORK_PER_CLASSIFICATION, NEVER_ASK_TOOLS, WORK_PER_ENTRY, WORK_PER_LOOKUP,
};
use serde_json::Value;
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

/// Why a call that writes, or may write, into one of Eitri's own places cards. Not in
/// [`super::REPLACEABLE_BY_A_RULE`]: a saved rule names the words of a command, not where Eitri reads its
/// configuration.
pub const REASON_EITRI_OWN_PLACES: &str = "this call writes, or may write, where Eitri reads its own configuration or \
     code at start (its config, data or state directory, or the running binary's directory)";

/// Why an edit target whose place could not be established cards here.
pub const REASON_EITRI_PLACE_UNKNOWN: &str =
    "where this call writes, or where one of Eitri's own configuration places is, could not be established";

/// Variables whose value decides where an Eitri place is; a command naming one may be building that path.
const PLACE_VARIABLE_WORDS: [&str; 4] = ["eitri_config_dir", "xdg_config_home", "xdg_data_home", "xdg_state_home"];

/// The edit tools and the field that names their target.
const EDIT_TARGET_FIELDS: [(&str, &str); 4] = [
    ("Write", "file_path"),
    ("Edit", "file_path"),
    ("MultiEdit", "file_path"),
    ("NotebookEdit", "notebook_path"),
];

/// Tools whose `command` is a shell command line.
const COMMAND_TOOLS: [&str; 2] = ["Bash", "Monitor"];

/// Tools that only read, judged against the project root elsewhere.
const READ_TOOLS: [&str; 3] = ["Read", "Grep", "Glob"];

/// The most JSON values of an unknown tool's input looked at; past it the call cards.
const MAX_INPUT_VALUES: usize = 10_000;

/// The environment the places are derived from, passed in so tests never read the real one.
#[derive(Debug, Clone, Copy, Default)]
pub struct EitriPlaceInputs<'a> {
    pub home: Option<&'a OsStr>,
    pub eitri_config_dir: Option<&'a OsStr>,
    pub xdg_config_home: Option<&'a OsStr>,
    pub xdg_data_home: Option<&'a OsStr>,
    pub xdg_state_home: Option<&'a OsStr>,
    /// The system data directories the plugin search reads after the user's own (`XDG_DATA_DIRS`).
    pub xdg_data_dirs: Option<&'a OsStr>,
    /// The canonical directory of the running binary.
    pub exe_dir: Option<&'a Path>,
    /// Further directories that are Eitri's own, beyond those the variables above decide: the remote helper's data
    /// and state directories, whose journal and pending requests decide what Eitri is told. Absolute ones only; links
    /// inside them are followed to a data directory's depth.
    pub extra: &'a [PathBuf],
}

/// What a place holds, which decides how deep the links inside it are followed back ([`Kind::link_depth`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Config,
    Data,
    State,
    Binary,
}

impl Kind {
    /// How many levels below the place its entries are read for links. The configuration directory is the user's
    /// own tree (a dotfiles manager links files into it one by one), so all of it that is plausibly deep; the data
    /// directory down to `plugins/<name>/<entry>`, the binary's directory down to `plugins/<name>`, the state
    /// directory its own entries. Eitri itself writes no link into any of them.
    fn link_depth(self) -> usize {
        match self {
            Kind::Config => 8,
            Kind::Data => 3,
            Kind::State => 1,
            Kind::Binary => 2,
        }
    }
}

/// Where Eitri reads its own configuration and code at start (module doc).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EitriPlaces {
    dirs: Vec<(PathBuf, Kind)>,
    exe_dir: Option<PathBuf>,
    home: Option<PathBuf>,
}

/// The running binary's directory, read once: a later read can find the binary deleted by an upgrade, when its
/// directory no longer resolves.
fn exe_dir_of_this_process() -> Option<PathBuf> {
    static EXE_DIR: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    EXE_DIR
        .get_or_init(|| {
            let exe = std::env::current_exe().ok()?;
            match std::fs::canonicalize(&exe) {
                Ok(exe) => exe.parent().map(Path::to_path_buf),
                // Linux names a deleted binary `<path> (deleted)`; its directory as named is still the place.
                Err(_) => {
                    let named = exe.to_string_lossy();
                    let named = named.strip_suffix(" (deleted)").unwrap_or(&named);
                    Path::new(named).parent().map(Path::to_path_buf)
                }
            }
        })
        .clone()
}

/// The environment variables that decide where Eitri's places are, in the order [`EitriPlaces::from_variables`] reads
/// them. A process that judges calls for an agent started elsewhere keeps these of that agent's environment.
pub const PLACE_VARIABLES: [&str; 6] = [
    "HOME",
    "EITRI_CONFIG_DIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_STATE_HOME",
    "XDG_DATA_DIRS",
];

impl EitriPlaces {
    /// This process's places. The only reader of the real environment here.
    pub fn from_process() -> EitriPlaces {
        EitriPlaces::from_variables(|name| std::env::var_os(name), exe_dir_of_this_process().as_deref(), &[])
    }

    /// The places for the value `var` gives each of [`PLACE_VARIABLES`], the running binary's directory `exe_dir`
    /// and the further directories `extra` ([`EitriPlaceInputs`]).
    pub fn from_variables(
        var: impl Fn(&str) -> Option<OsString>,
        exe_dir: Option<&Path>,
        extra: &[PathBuf],
    ) -> EitriPlaces {
        let [home, config, xdg_config, xdg_data, xdg_state, xdg_data_dirs] = PLACE_VARIABLES.map(var);
        EitriPlaces::from_values(&EitriPlaceInputs {
            home: home.as_deref(),
            eitri_config_dir: config.as_deref(),
            xdg_config_home: xdg_config.as_deref(),
            xdg_data_home: xdg_data.as_deref(),
            xdg_state_home: xdg_state.as_deref(),
            xdg_data_dirs: xdg_data_dirs.as_deref(),
            exe_dir,
            extra,
        })
    }

    /// The places for these values. Empty and relative values count as unset, as Eitri treats them when it reads
    /// them (a relative `EITRI_CONFIG_DIR` stops Eitri's start, so it names no place). `~/.local/lib/eitri`, where the
    /// installer puts the program, and `~/.local/bin/eitri`, its launcher, are places whatever binary runs, so they
    /// stay places while an upgrade swaps them. Each system data directory the plugin search reads
    /// (`XDG_DATA_DIRS`'s absolute entries, `/usr/local/share` and `/usr/share` when it is unset or empty) gives
    /// `<dir>/eitri`: a root-owned one costs nothing, and some are the user's (a Flatpak export directory).
    pub fn from_values(inputs: &EitriPlaceInputs<'_>) -> EitriPlaces {
        let absolute = |value: Option<&OsStr>| {
            value
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
        };
        let home = absolute(inputs.home);
        let mut dirs = Vec::new();
        dirs.extend(absolute(inputs.eitri_config_dir).map(|dir| (dir, Kind::Config)));
        for (base, kind) in [
            (inputs.xdg_config_home, Kind::Config),
            (inputs.xdg_data_home, Kind::Data),
            (inputs.xdg_state_home, Kind::State),
        ] {
            dirs.extend(absolute(base).map(|base| (base.join("eitri"), kind)));
        }
        if let Some(home) = &home {
            for (below, kind) in [
                (".config/eitri", Kind::Config),
                (".local/share/eitri", Kind::Data),
                (".local/state/eitri", Kind::State),
                (".local/lib/eitri", Kind::Binary),
                (".local/bin/eitri", Kind::Binary),
            ] {
                dirs.push((home.join(below), kind));
            }
        }
        let system: Vec<PathBuf> = match inputs.xdg_data_dirs {
            Some(value) if !value.is_empty() => std::env::split_paths(value).filter(|dir| dir.is_absolute()).collect(),
            _ => vec![PathBuf::from("/usr/local/share"), PathBuf::from("/usr/share")],
        };
        dirs.extend(system.into_iter().map(|dir| (dir.join("eitri"), Kind::Data)));
        dirs.extend(
            inputs
                .extra
                .iter()
                .filter(|dir| dir.is_absolute())
                .map(|dir| (dir.clone(), Kind::Data)),
        );
        let mut unique: Vec<(PathBuf, Kind)> = Vec::new();
        for dir in dirs {
            if !unique.iter().any(|(seen, _)| *seen == dir.0) {
                unique.push(dir);
            }
        }
        EitriPlaces {
            dirs: unique,
            exe_dir: inputs.exe_dir.map(Path::to_path_buf),
            home,
        }
    }

    /// The places as named, the running binary's directory last when it counts for `root`.
    pub fn dirs_for(&self, root: &Path) -> Vec<PathBuf> {
        self.places_for(root).into_iter().map(|(dir, _)| dir).collect()
    }

    fn places_for(&self, root: &Path) -> Vec<(PathBuf, Kind)> {
        let mut dirs = self.dirs.clone();
        if let Some(exe_dir) = &self.exe_dir {
            if !exe_dir.starts_with(root) && !dirs.iter().any(|(dir, _)| dir == exe_dir) {
                dirs.push((exe_dir.clone(), Kind::Binary));
            }
        }
        dirs
    }
}

/// A card when `tool_name` with `input`, in a session at `project_root`, writes or may write into one of `places`
/// (module doc); `None` when this check has nothing to say, which is no allow: the rest of the gate still judges
/// the call. Runs in a work budget of its own; running out cards.
pub fn eitri_places_card(
    tool_name: &str,
    input: &Value,
    project_root: &Path,
    places: &EitriPlaces,
) -> Option<Classification> {
    if READ_TOOLS.contains(&tool_name)
        || NEVER_ASK_TOOLS.contains(&tool_name)
        || (ALWAYS_ASK_TOOLS.contains(&tool_name) && tool_name != "MultiEdit")
    {
        return None;
    }
    let (card, ran_out) = with_work_budget(MAX_WORK_PER_CLASSIFICATION, || {
        judge(tool_name, input, project_root, places)
    });
    if ran_out {
        Some(ask(REASON_EITRI_PLACE_UNKNOWN))
    } else {
        card.map(|reason| ask(reason))
    }
}

/// The most path-like words of a command, or path-like strings of an unknown tool's input, resolved. Past it a
/// command is judged by its text alone, and an unknown tool cards.
const MAX_PATH_WORDS: usize = 64;

fn judge(tool_name: &str, input: &Value, project_root: &Path, places: &EitriPlaces) -> Option<&'static str> {
    let root = canonical(project_root);
    let Some(spellings) = spellings(&places.places_for(&root)) else {
        // A place, or a link in one, whose spelling could not be established: any call may reach it.
        return Some(REASON_EITRI_PLACE_UNKNOWN);
    };
    if let Some((_, field)) = EDIT_TARGET_FIELDS.iter().find(|(tool, _)| *tool == tool_name) {
        return match input.get(*field) {
            Some(Value::String(raw)) if !raw.is_empty() => judge_edit_target(raw, &root, &spellings),
            // Malformed: the CLI runs no edit without a target.
            _ => None,
        };
    }
    let home = places.home.as_deref();
    let needles = needles(&spellings, &root, home);
    if COMMAND_TOOLS.contains(&tool_name) {
        return match input.get("command") {
            Some(Value::String(command)) => {
                let named = names_a_place(command, &needles)
                    || path_words(command)
                        .take(MAX_PATH_WORDS)
                        .any(|word| word_reaches_a_place(&word, &root, home, &spellings));
                named.then_some(REASON_EITRI_OWN_PLACES)
            }
            _ => None,
        };
    }
    // A tool this policy does not know: what it does with its input is unknown, so every string counts, and one
    // that is an absolute or `~` path is judged as an edit target is. In a project that is a place it may write
    // there with no string at all.
    if needles.iter().any(String::is_empty) {
        return Some(REASON_EITRI_OWN_PLACES);
    }
    let mut seen = 0;
    let mut paths = 0;
    let mut relative = 0;
    let mut verdict = None;
    let found = any_string(input, &mut seen, &mut |text| {
        if names_a_place(text, &needles) {
            verdict = Some(REASON_EITRI_OWN_PLACES);
            return true;
        }
        let Some(path) = as_absolute_path(text, home) else {
            // A relative path is taken against the project root, the session's working directory; only a positive
            // answer counts, since where the tool takes it from is the tool's.
            if text.starts_with('~') || !text.contains('/') || text.contains(char::is_whitespace) {
                return false;
            }
            relative += 1;
            if relative <= MAX_PATH_WORDS && word_reaches_a_place(text, &root, home, &spellings) {
                verdict = Some(REASON_EITRI_OWN_PLACES);
                return true;
            }
            return false;
        };
        paths += 1;
        verdict = if paths > MAX_PATH_WORDS {
            Some(REASON_EITRI_PLACE_UNKNOWN)
        } else {
            judge_edit_target(&path, &root, &spellings)
        };
        verdict.is_some()
    });
    match found {
        Some(true) => verdict.or(Some(REASON_EITRI_OWN_PLACES)),
        Some(false) => None,
        None => Some(REASON_EITRI_PLACE_UNKNOWN),
    }
}

/// `path` resolved, or as given when it does not resolve: the root as the session names it is canonical already.
fn canonical(path: &Path) -> PathBuf {
    match resolve(Path::new("/"), path) {
        Resolution::At(resolved) => resolved,
        _ => path.to_path_buf(),
    }
}

/// Every spelling of every place, folded: as named, resolved as far as it exists, and where each link inside it
/// (to [`Kind::link_depth`]) leads. `None` when one of those could not be established.
fn spellings(places: &[(PathBuf, Kind)]) -> Option<Vec<Vec<String>>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    let add = |path: &Path, out: &mut Vec<Vec<String>>| {
        let folded = folded_components(path);
        if !folded.is_empty() && !out.contains(&folded) {
            out.push(folded);
        }
    };
    for (dir, kind) in places {
        add(dir, &mut out);
        // A place that is a link leading nowhere yet (a launcher whose build was cleaned) is where it would lead.
        let resolved = match resolve_with_missing_tail(Path::new("/"), dir) {
            Ok(resolved) => resolved,
            Err(()) => link_target(dir)?,
        };
        add(&resolved, &mut out);
        for target in links_inside(&resolved, kind.link_depth())? {
            add(&target, &mut out);
        }
    }
    Some(out)
}

/// Where every link in `dir`, down to `depth` levels, leads (a dangling one: where it would lead). A link to a
/// directory is entered too, once, at the link's own level, since a link inside it leads to a file Eitri reads as
/// well. Each directory opened costs a lookup of the work budget and each entry an entry. `None` when a directory on
/// the way or a link could not be read, or the budget ran out.
fn links_inside(dir: &Path, depth: usize) -> Option<Vec<PathBuf>> {
    let mut targets = Vec::new();
    // Each directory with the shallowest level it was entered at: one reached again higher up is walked again from
    // there, so how deep its links are followed does not hang on the order the entries are listed in.
    let mut entered: Vec<(PathBuf, usize)> = vec![(dir.to_path_buf(), 0)];
    let mut pending = vec![(dir.to_path_buf(), 0)];
    while let Some((at, level)) = pending.pop() {
        if !spend(WORK_PER_LOOKUP) {
            return None;
        }
        let entries = match std::fs::read_dir(&at) {
            Ok(entries) => entries,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                continue
            }
            Err(_) => return None,
        };
        for entry in entries {
            if !spend(WORK_PER_ENTRY) {
                return None;
            }
            let entry = entry.ok()?;
            let kind = entry.file_type().ok()?;
            let path = entry.path();
            if kind.is_symlink() {
                let target = link_target(&path)?;
                let shallower = entered.iter().all(|(seen, at)| *seen != target || *at > level + 1);
                if level + 1 < depth && shallower && spend(WORK_PER_LOOKUP) && target.is_dir() {
                    entered.retain(|(seen, _)| *seen != target);
                    entered.push((target.clone(), level + 1));
                    pending.push((target.clone(), level + 1));
                }
                targets.push(target);
            } else if kind.is_dir() && level + 1 < depth {
                pending.push((path, level + 1));
            }
        }
    }
    Some(targets)
}

/// Where the link `link` leads, or would lead once its target is created.
fn link_target(link: &Path) -> Option<PathBuf> {
    if let Resolution::At(resolved) = resolve(Path::new("/"), link) {
        return Some(resolved);
    }
    let target = std::fs::read_link(link).ok()?;
    let parent = link.parent()?;
    let base = resolve_with_missing_tail(Path::new("/"), parent).ok()?;
    resolve_with_missing_tail(&base, &target).ok()
}

fn folded_components(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(fold(&part.to_string_lossy())),
            _ => None,
        })
        .collect()
}

fn fold(text: &str) -> String {
    fold_confusables(text).to_lowercase()
}

/// Whether `path` is one of `spellings` or lies below one, component by component.
fn under_a_place(path: &Path, spellings: &[Vec<String>]) -> bool {
    let components = folded_components(path);
    spellings.iter().any(|place| components.starts_with(place))
}

fn judge_edit_target(raw: &str, root: &Path, spellings: &[Vec<String>]) -> Option<&'static str> {
    if raw.starts_with('~') || Path::new(raw).components().any(|c| matches!(c, Component::ParentDir)) {
        return Some(REASON_EITRI_PLACE_UNKNOWN);
    }
    let requested = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        root.join(raw)
    };
    if under_a_place(&requested, spellings) {
        return Some(REASON_EITRI_OWN_PLACES);
    }
    let resolved = match resolve_with_missing_tail(root, Path::new(raw)) {
        Ok(resolved) => resolved,
        Err(()) => return Some(REASON_EITRI_PLACE_UNKNOWN),
    };
    if under_a_place(&resolved, spellings) {
        return Some(REASON_EITRI_OWN_PLACES);
    }
    // Another name of the same data may lie in a place.
    let hard_linked = std::fs::metadata(&resolved)
        .map(|meta| meta.is_file() && std::os::unix::fs::MetadataExt::nlink(&meta) > 1)
        .unwrap_or(false);
    hard_linked.then_some(REASON_EITRI_PLACE_UNKNOWN)
}

/// `path` resolved from `base` with every link followed wherever it leads; a path that does not exist yet is its
/// nearest existing ancestor, resolved, with the rest joined on. `Err` where the resolution cannot be established,
/// a `..` in the part that does not exist included.
fn resolve_with_missing_tail(base: &Path, path: &Path) -> Result<PathBuf, ()> {
    match resolve(base, path) {
        Resolution::At(resolved) => return Ok(resolved),
        Resolution::Missing => {}
        _ => return Err(()),
    }
    let components: Vec<Component> = path.components().collect();
    if components.len() > MAX_RESOLUTION_STEPS {
        return Err(());
    }
    for prefix_len in (0..components.len()).rev() {
        let prefix: PathBuf = components[..prefix_len].iter().collect();
        match resolve(base, &prefix) {
            Resolution::At(mut resolved) => {
                for component in &components[prefix_len..] {
                    match component {
                        Component::Normal(part) => resolved.push(part),
                        Component::CurDir => {}
                        _ => return Err(()),
                    }
                }
                return Ok(resolved);
            }
            Resolution::Missing => continue,
            _ => return Err(()),
        }
    }
    Err(())
}

/// `text` as an absolute path when it is spelled as one: starting with `/`, or with `~` alone or `~/` (taken from
/// `home`).
fn as_absolute_path(text: &str, home: Option<&Path>) -> Option<String> {
    if text.starts_with('/') {
        return Some(text.to_string());
    }
    let rest = text.strip_prefix('~')?;
    if !(rest.is_empty() || rest.starts_with('/')) {
        return None;
    }
    Some(format!("{}{rest}", home?.display()))
}

/// The words of a command that may name a path: split where the shell may split or quote, kept when they hold a
/// `/` or start with `~`. A word the shell would expand further is judged as written, so this only ever adds cards.
fn path_words(command: &str) -> impl Iterator<Item = String> + '_ {
    command
        .split(|c: char| {
            c.is_whitespace() || matches!(c, ';' | '|' | '&' | '<' | '>' | '(' | ')' | '\'' | '"' | '`' | '=')
        })
        .filter(|word| word.contains('/') || word.starts_with('~'))
        .map(str::to_string)
}

/// Whether a command's `word`, taken against the project root (the command's working directory) with a leading `~`
/// from `home`, resolves into a place. Only a positive answer counts: a word that does not resolve says nothing.
fn word_reaches_a_place(word: &str, root: &Path, home: Option<&Path>, spellings: &[Vec<String>]) -> bool {
    let path = match as_absolute_path(word, home) {
        Some(path) => PathBuf::from(path),
        None if word.starts_with('~') => return false,
        None => root.join(word),
    };
    under_a_place(&path, spellings)
        || resolve_with_missing_tail(Path::new("/"), &path).is_ok_and(|resolved| under_a_place(&resolved, spellings))
}

/// The folded texts a command naming a place would contain: every spelling whole, and below the home directory
/// and the project root; and the variables that decide them. A place that is the root or holds it gives the empty
/// text, which every command contains: anything a command writes in the project may then be such a file.
fn needles(spellings: &[Vec<String>], root: &Path, home: Option<&Path>) -> Vec<String> {
    let root = folded_components(root);
    let homes: Vec<Vec<String>> = home
        .map(|home| {
            [
                Some(home.to_path_buf()),
                resolve_with_missing_tail(Path::new("/"), home).ok(),
            ]
            .into_iter()
            .flatten()
            .map(|spelling| folded_components(&spelling))
            .filter(|spelling| !spelling.is_empty())
            .collect()
        })
        .unwrap_or_default();
    let mut out: Vec<String> = PLACE_VARIABLE_WORDS.iter().map(|v| v.to_string()).collect();
    let mut add = |text: String| {
        if !out.contains(&text) {
            out.push(text);
        }
    };
    for place in spellings {
        add(format!("/{}", place.join("/")));
        for home in &homes {
            if place.len() > home.len() && place.starts_with(home) {
                add(place[home.len()..].join("/"));
            }
        }
        if root.starts_with(place) {
            // The project root is a place or lies inside one: whatever a command writes in the project may be
            // such a file, and every command contains the empty text.
            add(String::new());
        } else if place.starts_with(&root) {
            add(place[root.len()..].join("/"));
        }
    }
    out
}

/// `text` folded as the places are, with the spellings a path may take that name the same directory settled: runs of
/// `/` are one, and a `/./` is a `/`.
fn normalized(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in fold(text).chars() {
        if c == '/' && out.ends_with('/') {
            continue;
        }
        out.push(c);
        if c == '/' && out.ends_with("/./") {
            out.truncate(out.len() - 2);
        }
    }
    if out.ends_with("/.") {
        out.truncate(out.len() - 1);
    }
    out
}

fn names_a_place(text: &str, needles: &[String]) -> bool {
    let text = normalized(text);
    needles.iter().any(|needle| text.contains(needle.as_str()))
}

/// Whether any string in `value` satisfies `test`; `None` once more than [`MAX_INPUT_VALUES`] values were seen.
fn any_string(value: &Value, seen: &mut usize, test: &mut dyn FnMut(&str) -> bool) -> Option<bool> {
    *seen += 1;
    if *seen > MAX_INPUT_VALUES || !spend(1) {
        return None;
    }
    match value {
        Value::String(text) => Some(test(text)),
        Value::Array(items) => {
            for item in items {
                if any_string(item, seen, test)? {
                    return Some(true);
                }
            }
            Some(false)
        }
        Value::Object(fields) => {
            for (key, item) in fields {
                if test(key) || any_string(item, seen, test)? {
                    return Some(true);
                }
            }
            Some(false)
        }
        _ => Some(false),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        classify_in_manual_with_places, classify_with_places, classify_with_rules_and_places,
        rule_that_allows_with_places, PermissionVerdict,
    };
    use super::*;
    use crate::permission_rules::{PrefixRule, PrefixRules};
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    /// A home directory of its own holding a project, both real and canonical, so links and resolution are real.
    struct Scratch {
        dir: PathBuf,
    }

    impl Scratch {
        fn new() -> Scratch {
            let dir = std::env::temp_dir().join(format!("agent-eitri-places-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(dir.join("home/proj/src")).unwrap();
            let dir = dir.canonicalize().unwrap();
            for sub in ["", "home", "home/proj"] {
                std::fs::set_permissions(dir.join(sub), std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            std::fs::write(dir.join("home/proj/src/main.rs"), "fn main() {}\n").unwrap();
            Scratch { dir }
        }
        fn home(&self) -> PathBuf {
            self.dir.join("home")
        }
        fn root(&self) -> PathBuf {
            self.dir.join("home/proj")
        }
        fn places(&self) -> EitriPlaces {
            let home = self.home();
            EitriPlaces::from_values(&EitriPlaceInputs {
                home: Some(home.as_os_str()),
                ..EitriPlaceInputs::default()
            })
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn card(tool: &str, input: Value, root: &Path, places: &EitriPlaces) -> Option<&'static str> {
        eitri_places_card(tool, &input, root, places).map(|c| {
            assert_eq!(c.verdict, PermissionVerdict::AskTheUser);
            c.reason
        })
    }

    fn edit_input(tool: &str, path: &str) -> Value {
        match tool {
            "NotebookEdit" => json!({ "notebook_path": path, "new_source": "x" }),
            "Write" => json!({ "file_path": path, "content": "x" }),
            "MultiEdit" => json!({ "file_path": path, "edits": [{ "old_string": "a", "new_string": "b" }] }),
            _ => json!({ "file_path": path, "old_string": "a", "new_string": "b" }),
        }
    }

    const EDIT_TOOLS_HERE: [&str; 4] = ["Write", "Edit", "MultiEdit", "NotebookEdit"];

    #[test]
    fn the_places_follow_the_variables_eitri_reads_them_by() {
        let os = |s: &'static str| Some(OsStr::new(s));
        let dirs = |inputs: EitriPlaceInputs| EitriPlaces::from_values(&inputs).dirs_for(Path::new("/srv/project"));
        let system = || {
            vec![
                PathBuf::from("/usr/local/share/eitri"),
                PathBuf::from("/usr/share/eitri"),
            ]
        };
        let defaults = |home: &str| -> Vec<PathBuf> {
            let mut out: Vec<PathBuf> = [
                ".config/eitri",
                ".local/share/eitri",
                ".local/state/eitri",
                ".local/lib/eitri",
                ".local/bin/eitri",
            ]
            .iter()
            .map(|below| Path::new(home).join(below))
            .collect();
            out.extend(system());
            out
        };
        assert_eq!(
            dirs(EitriPlaceInputs {
                home: os("/home/u"),
                ..Default::default()
            }),
            defaults("/home/u")
        );
        let mut expected = vec![
            PathBuf::from("/c"),
            PathBuf::from("/xc/eitri"),
            PathBuf::from("/xd/eitri"),
            PathBuf::from("/xs/eitri"),
        ];
        expected.extend(defaults("/home/u").into_iter().take(5));
        expected.extend([PathBuf::from("/flat/share/eitri"), PathBuf::from("/usr/share/eitri")]);
        expected.push(PathBuf::from("/opt/eitri/0.4.0"));
        assert_eq!(
            dirs(EitriPlaceInputs {
                home: os("/home/u"),
                eitri_config_dir: os("/c"),
                xdg_config_home: os("/xc"),
                xdg_data_home: os("/xd"),
                xdg_state_home: os("/xs"),
                xdg_data_dirs: os("/flat/share:rel:/usr/share"),
                exe_dir: Some(Path::new("/opt/eitri/0.4.0")),
                extra: &[],
            }),
            expected,
            "the variables' places first, the home defaults always, the system data directories, the binary's \
             directory last"
        );
        assert_eq!(
            dirs(EitriPlaceInputs {
                home: os("/home/u"),
                eitri_config_dir: os("rel"),
                xdg_config_home: os(""),
                xdg_data_home: os("data"),
                xdg_data_dirs: os(""),
                ..Default::default()
            }),
            defaults("/home/u"),
            "empty and relative values name no place, as Eitri ignores or refuses them"
        );
        assert_eq!(
            dirs(EitriPlaceInputs {
                home: os("relative"),
                ..Default::default()
            }),
            system()
        );
        assert_eq!(
            dirs(EitriPlaceInputs {
                exe_dir: Some(Path::new("/srv/project/target/debug")),
                ..Default::default()
            }),
            system(),
            "a binary built inside the project is the project's build tree"
        );
    }

    /// A dotfiles manager links files into the configuration directory one by one: the file a link leads to is a
    /// place, wherever it lives, and so is where a dangling one would lead.
    #[test]
    fn a_link_inside_a_place_makes_its_target_a_place() {
        let s = Scratch::new();
        let (home, root, places) = (s.home(), s.root(), s.places());
        std::fs::create_dir_all(root.join("dotfiles/eitri")).unwrap();
        std::fs::write(root.join("dotfiles/eitri/init.lua"), "-- mine\n").unwrap();
        std::fs::create_dir_all(home.join(".config/eitri")).unwrap();
        std::os::unix::fs::symlink(
            root.join("dotfiles/eitri/init.lua"),
            home.join(".config/eitri/init.lua"),
        )
        .unwrap();
        std::os::unix::fs::symlink(root.join("dotfiles/eitri-lua"), home.join(".config/eitri/lua")).unwrap();
        for tool in EDIT_TOOLS_HERE {
            for target in ["dotfiles/eitri/init.lua", "dotfiles/eitri-lua/keys.lua"] {
                assert_eq!(
                    card(tool, edit_input(tool, target), &root, &places),
                    Some(REASON_EITRI_OWN_PLACES),
                    "{tool} {target}"
                );
            }
            assert_eq!(card(tool, edit_input(tool, "dotfiles/zshrc"), &root, &places), None);
        }
        for command in ["echo x >> dotfiles/eitri/init.lua", "cp a dotfiles/eitri-lua/b.lua"] {
            assert_eq!(
                card("Bash", json!({ "command": command }), &root, &places),
                Some(REASON_EITRI_OWN_PLACES),
                "{command}"
            );
        }
    }

    /// A link inside a directory a place links to, and a relative path an unknown tool is given through a link in
    /// the project.
    #[test]
    fn a_link_inside_a_linked_directory_and_a_relative_tool_path_are_places() {
        let s = Scratch::new();
        let (home, root, places) = (s.home(), s.root(), s.places());
        std::fs::create_dir_all(home.join(".config/eitri")).unwrap();
        std::fs::create_dir_all(root.join("config-lua")).unwrap();
        std::fs::write(root.join("shared.lua"), "return {}\n").unwrap();
        std::os::unix::fs::symlink(root.join("config-lua"), home.join(".config/eitri/lua")).unwrap();
        std::os::unix::fs::symlink(root.join("shared.lua"), root.join("config-lua/keys.lua")).unwrap();
        std::os::unix::fs::symlink(home.join(".config/eitri"), root.join("cfg")).unwrap();
        assert_eq!(
            card("Write", edit_input("Write", "shared.lua"), &root, &places),
            Some(REASON_EITRI_OWN_PLACES)
        );
        assert_eq!(
            card(
                "mcp__fs__write_file",
                json!({ "path": "cfg/init.lua", "content": "x" }),
                &root,
                &places
            ),
            Some(REASON_EITRI_OWN_PLACES)
        );
        assert_eq!(
            card(
                "mcp__fs__write_file",
                json!({ "path": "src/new.rs", "content": "x" }),
                &root,
                &places
            ),
            None
        );
    }

    /// Two links to one directory at different depths: the shallower one decides how deep its links are followed,
    /// whichever the walk meets first; and harmless strings before a relative path do not use up its lookups.
    #[test]
    fn a_directory_linked_twice_is_walked_from_its_shallower_link() {
        let s = Scratch::new();
        let (home, root, places) = (s.home(), s.root(), s.places());
        let config = home.join(".config/eitri");
        std::fs::create_dir_all(config.join("long/sub")).unwrap();
        std::fs::create_dir_all(config.join("short")).unwrap();
        std::fs::create_dir_all(root.join("config-lua/a/b/c/d/e")).unwrap();
        std::fs::write(root.join("shared.lua"), "return {}\n").unwrap();
        std::os::unix::fs::symlink(root.join("shared.lua"), root.join("config-lua/a/b/c/d/e/keys.lua")).unwrap();
        for alias in ["short/alias", "long/sub/alias"] {
            std::os::unix::fs::symlink(root.join("config-lua"), config.join(alias)).unwrap();
        }
        assert_eq!(
            card("Write", edit_input("Write", "shared.lua"), &root, &places),
            Some(REASON_EITRI_OWN_PLACES)
        );
        std::os::unix::fs::symlink(&config, root.join("cfg")).unwrap();
        let mut input = serde_json::Map::new();
        for i in 0..40 {
            input.insert(format!("a{i:02}"), json!("harmless"));
        }
        input.insert("path".into(), json!("cfg/init.lua"));
        assert_eq!(
            card("mcp__fs__write_file", Value::Object(input), &root, &places),
            Some(REASON_EITRI_OWN_PLACES)
        );
    }

    /// A place that is a link leading nowhere yet is where it would lead, and leaves every other call alone.
    #[test]
    fn a_dangling_place_is_where_it_would_lead() {
        let s = Scratch::new();
        let (home, root, places) = (s.home(), s.root(), s.places());
        std::fs::create_dir_all(home.join(".local/bin")).unwrap();
        std::os::unix::fs::symlink(root.join("target/release/shell"), home.join(".local/bin/eitri")).unwrap();
        assert_eq!(card("Write", edit_input("Write", "src/main.rs"), &root, &places), None);
        assert_eq!(card("Bash", json!({ "command": "cargo build" }), &root, &places), None);
        assert_eq!(
            card("Write", edit_input("Write", "target/release/shell"), &root, &places),
            Some(REASON_EITRI_OWN_PLACES)
        );
    }

    /// A place whose own spelling cannot be established (a link that loops) may be anywhere, so every judged call
    /// cards.
    #[test]
    fn a_place_that_cannot_be_resolved_cards_every_judged_call() {
        let s = Scratch::new();
        let (home, root, places) = (s.home(), s.root(), s.places());
        std::fs::create_dir_all(home.join(".config")).unwrap();
        std::os::unix::fs::symlink(home.join(".config/eitri"), home.join(".config/eitri")).unwrap();
        assert_eq!(
            card("Write", edit_input("Write", "src/main.rs"), &root, &places),
            Some(REASON_EITRI_PLACE_UNKNOWN)
        );
        assert_eq!(
            card("Bash", json!({ "command": "cargo build" }), &root, &places),
            Some(REASON_EITRI_PLACE_UNKNOWN)
        );
        assert_eq!(
            card("Read", json!({ "file_path": "src/main.rs" }), &root, &places),
            None
        );
    }

    #[test]
    fn a_system_data_directory_and_the_launcher_are_places() {
        let s = Scratch::new();
        let (home, root) = (s.home(), s.root());
        let flat = s.dir.join("flatpak/exports/share");
        let places = EitriPlaces::from_values(&EitriPlaceInputs {
            home: Some(home.as_os_str()),
            xdg_data_dirs: Some(flat.as_os_str()),
            ..EitriPlaceInputs::default()
        });
        for target in [
            flat.join("eitri/plugins/markdown/eitri-plugin.toml"),
            home.join(".local/bin/eitri"),
            home.join(".local/lib/eitri/plugins/markdown/web/dist/index.html"),
        ] {
            let target = target.display().to_string();
            assert_eq!(
                card("Write", edit_input("Write", &target), &root, &places),
                Some(REASON_EITRI_OWN_PLACES),
                "{target}"
            );
        }
        assert_eq!(
            card(
                "Bash",
                json!({ "command": "echo exec evil >> ~/.local/bin/eitri" }),
                &root,
                &places
            ),
            Some(REASON_EITRI_OWN_PLACES)
        );
        assert_eq!(
            card(
                "Write",
                edit_input("Write", &home.join(".local/bin/other").display().to_string()),
                &root,
                &places
            ),
            None
        );
    }

    /// The spellings a path takes that name the same directory: repeated `/`, `/./`, a link in the project, `..`.
    #[test]
    fn another_spelling_of_a_place_is_still_a_place() {
        let s = Scratch::new();
        let (home, root, places) = (s.home(), s.root(), s.places());
        std::fs::create_dir_all(home.join(".config/eitri")).unwrap();
        std::os::unix::fs::symlink(home.join(".config/eitri"), root.join("cfg")).unwrap();
        let h = home.display();
        for command in [
            "echo x > ~/.config//eitri/init.lua".to_string(),
            "echo x > ~/.config/./eitri/init.lua".to_string(),
            "cp evil.lua cfg/init.lua".to_string(),
            "cp evil.lua ../proj/cfg/init.lua".to_string(),
            "cp evil.lua src/../../.config/eitri/init.lua".to_string(),
            format!("tee {h}/.config/eitri/../eitri/init.lua"),
        ] {
            assert_eq!(
                card("Bash", json!({ "command": command }), &root, &places),
                Some(REASON_EITRI_OWN_PLACES),
                "{command}"
            );
        }
        for path in [
            format!("{h}/.config/./eitri/init.lua"),
            format!("{h}//.config/eitri/init.lua"),
            root.join("cfg/init.lua").display().to_string(),
            "~/.config/eitri/init.lua".to_string(),
        ] {
            assert!(
                card(
                    "mcp__fs__write_file",
                    json!({ "path": path, "content": "x" }),
                    &root,
                    &places
                )
                .is_some(),
                "{path}"
            );
        }
        assert_eq!(
            card(
                "mcp__fs__write_file",
                json!({ "path": format!("{h}/notes/x.md"), "content": "x" }),
                &root,
                &places
            ),
            None
        );
    }

    #[test]
    fn a_write_into_eitris_own_places_is_a_card_by_every_edit_tool() {
        let s = Scratch::new();
        let (home, root, places) = (s.home(), s.root(), s.places());
        let home = home.display();
        for tool in EDIT_TOOLS_HERE {
            for target in [
                format!("{home}/.config/eitri/init.lua"),
                format!("{home}/.config/eitri/lua/keys/more.lua"),
                format!("{home}/.config/eitri/neovide.toml"),
                format!("{home}/.local/share/eitri/plugins/md/eitri-plugin.toml"),
                format!("{home}/.local/share/eitri/nvim/0.11.4/bin/nvim"),
                format!("{home}/.local/state/eitri/permissions/rules.json"),
                format!("{home}/.local/state/eitri/trust/records.json"),
                // Spelled as the protected names are matched: case and the confusable letters fold.
                format!("{home}/.Config/EITRI/init.lua"),
                format!("{home}/.conf\u{0131}g/eitri/init.lua"),
                format!("{home}/.config/ei\u{200d}tri/init.lua"),
            ] {
                assert_eq!(
                    card(tool, edit_input(tool, &target), &root, &places),
                    Some(REASON_EITRI_OWN_PLACES),
                    "{tool} {target}"
                );
            }
            for target in [
                format!("{home}/.config/eitri-old/init.lua"),
                format!("{home}/.config/nvim/init.lua"),
                format!("{}/src/main.rs", root.display()),
                "src/new.rs".to_string(),
            ] {
                assert_eq!(
                    card(tool, edit_input(tool, &target), &root, &places),
                    None,
                    "{tool} {target}"
                );
            }
        }
    }

    #[test]
    fn a_link_into_a_place_is_a_card() {
        let s = Scratch::new();
        let (home, root, places) = (s.home(), s.root(), s.places());
        std::fs::create_dir_all(home.join(".config/eitri")).unwrap();
        std::fs::write(home.join(".config/eitri/init.lua"), "-- mine\n").unwrap();
        std::os::unix::fs::symlink(home.join(".config/eitri"), root.join("cfg")).unwrap();
        std::os::unix::fs::symlink(home.join(".config/eitri/init.lua"), root.join("init.lua")).unwrap();
        for tool in EDIT_TOOLS_HERE {
            for target in [
                root.join("cfg/init.lua"),
                root.join("cfg/lua/new.lua"),
                root.join("init.lua"),
            ] {
                let target = target.display().to_string();
                assert_eq!(
                    card(tool, edit_input(tool, &target), &root, &places),
                    Some(REASON_EITRI_OWN_PLACES),
                    "{tool} {target}"
                );
            }
        }
    }

    /// A dotfiles repository: the configuration directory is a link into the project, so the project's own path to
    /// it names no place until the place is resolved.
    #[test]
    fn a_place_that_is_a_link_into_the_project_is_a_card() {
        let s = Scratch::new();
        let (home, root, places) = (s.home(), s.root(), s.places());
        std::fs::create_dir_all(root.join("dotfiles/eitri")).unwrap();
        std::fs::create_dir_all(home.join(".config")).unwrap();
        std::os::unix::fs::symlink(root.join("dotfiles/eitri"), home.join(".config/eitri")).unwrap();
        for tool in EDIT_TOOLS_HERE {
            for target in [
                "dotfiles/eitri/init.lua".to_string(),
                format!("{}/dotfiles/eitri/x.lua", root.display()),
            ] {
                assert_eq!(
                    card(tool, edit_input(tool, &target), &root, &places),
                    Some(REASON_EITRI_OWN_PLACES),
                    "{tool} {target}"
                );
            }
        }
        assert_eq!(
            card(
                "Bash",
                json!({ "command": "cp new.lua dotfiles/eitri/init.lua" }),
                &root,
                &places
            ),
            Some(REASON_EITRI_OWN_PLACES)
        );
        assert_eq!(
            card("Bash", json!({ "command": "cp a dotfiles/zsh/b" }), &root, &places),
            None
        );
    }

    #[test]
    fn an_edit_target_this_check_cannot_place_is_a_card() {
        let s = Scratch::new();
        let (root, places) = (s.root(), s.places());
        std::fs::write(root.join("one"), "x").unwrap();
        std::fs::hard_link(root.join("one"), root.join("two")).unwrap();
        for tool in EDIT_TOOLS_HERE {
            for target in [
                "~/.config/eitri/init.lua".to_string(),
                "src/../../.config/eitri/init.lua".to_string(),
                root.join("two").display().to_string(),
            ] {
                assert_eq!(
                    card(tool, edit_input(tool, &target), &root, &places),
                    Some(REASON_EITRI_PLACE_UNKNOWN),
                    "{tool} {target}"
                );
            }
        }
    }

    #[test]
    fn a_command_naming_a_place_is_a_card() {
        let s = Scratch::new();
        let (home, root, places) = (s.home(), s.root(), s.places());
        let home = home.display();
        for tool in COMMAND_TOOLS {
            for command in [
                "echo 'x' >> ~/.config/eitri/init.lua".to_string(),
                "cp evil.lua $HOME/.local/share/eitri/plugins/md/main.lua".to_string(),
                "cp evil.lua ${HOME}/.local/share/eitri/plugins/md/main.lua".to_string(),
                format!("cat > {home}/.local/state/eitri/permissions/rules.json"),
                "printf x > \"$XDG_CONFIG_HOME/eitri/init.lua\"".to_string(),
                "EITRI_CONFIG_DIR=/tmp/x eitri".to_string(),
                "sed -i s/a/b/ ~/.CONFIG/Eitri/init.lua".to_string(),
                format!("ln -sf /tmp/evil {home}/.local/share/eitri/sidecar"),
            ] {
                assert_eq!(
                    card(tool, json!({ "command": command }), &root, &places),
                    Some(REASON_EITRI_OWN_PLACES),
                    "{tool} {command}"
                );
            }
            for command in ["cargo build", "echo eitri", "cat ~/.config/nvim/init.lua", "ls src"] {
                assert_eq!(
                    card(tool, json!({ "command": command }), &root, &places),
                    None,
                    "{tool} {command}"
                );
            }
        }
    }

    #[test]
    fn an_unknown_tool_naming_a_place_is_a_card_and_a_read_is_not() {
        let s = Scratch::new();
        let (home, root, places) = (s.home(), s.root(), s.places());
        let target = home.join(".config/eitri/init.lua").display().to_string();
        assert_eq!(
            card(
                "mcp__fs__write_file",
                json!({ "path": target, "content": "x" }),
                &root,
                &places
            ),
            Some(REASON_EITRI_OWN_PLACES)
        );
        assert_eq!(
            card(
                "mcp__fs__edit",
                json!({ "edits": [{ "files": ["a", "~/.config/eitri/lua/x.lua"] }] }),
                &root,
                &places
            ),
            Some(REASON_EITRI_OWN_PLACES)
        );
        assert_eq!(
            card("mcp__fs__write_file", json!({ "path": "src/x" }), &root, &places),
            None
        );
        let wide: Vec<Value> = (0..MAX_INPUT_VALUES + 1).map(|_| json!(0)).collect();
        assert_eq!(
            card("mcp__fs__write_file", json!({ "values": wide }), &root, &places),
            Some(REASON_EITRI_PLACE_UNKNOWN),
            "an input past the bound is not read through, and cards"
        );
        for (tool, input) in [
            ("Read", json!({ "file_path": target })),
            ("Grep", json!({ "path": target, "pattern": "x" })),
            ("Glob", json!({ "pattern": "~/.config/eitri/*" })),
            ("Agent", json!({ "prompt": format!("edit {target}") })),
            ("WebFetch", json!({ "url": "https://example.com/.config/eitri" })),
        ] {
            assert_eq!(card(tool, input, &root, &places), None, "{tool}");
        }
    }

    #[test]
    fn a_project_inside_a_place_cards_every_edit_and_command() {
        let s = Scratch::new();
        let home = s.home();
        let root = home.join(".local/share/eitri/plugins/mine");
        std::fs::create_dir_all(&root).unwrap();
        let places = s.places();
        assert_eq!(
            card(
                "Write",
                edit_input("Write", &root.join("init.lua").display().to_string()),
                &root,
                &places
            ),
            Some(REASON_EITRI_OWN_PLACES)
        );
        assert_eq!(
            card("Bash", json!({ "command": "touch a" }), &root, &places),
            Some(REASON_EITRI_OWN_PLACES)
        );
        assert_eq!(
            card("mcp__x__run", json!({}), &root, &places),
            Some(REASON_EITRI_OWN_PLACES)
        );
    }

    /// The gate's own classifier: a place inside the project is no fast-path write, and a command into it is not one
    /// a saved rule answers, in Auto and Manual alike.
    #[test]
    fn the_policy_cards_a_place_inside_the_project() {
        let s = Scratch::new();
        let (home, root) = (s.home(), s.root());
        let conf = root.join("conf");
        std::fs::create_dir_all(conf.join("eitri")).unwrap();
        let places = EitriPlaces::from_values(&EitriPlaceInputs {
            home: Some(home.as_os_str()),
            xdg_config_home: Some(conf.as_os_str()),
            ..EitriPlaceInputs::default()
        });
        let none = EitriPlaces::default();
        let write = json!({ "file_path": conf.join("eitri/init.lua").display().to_string(), "content": "x" });
        let copy = json!({ "command": "cp evil.lua conf/eitri/init.lua" });
        let rules = PrefixRules::default().with(PrefixRule::parse("Bash(cp *)").unwrap());

        // Without the places the same calls are allowed, so the cards below are this check's.
        assert!(!classify_with_places("Write", &write, &root, &none).needs_a_human());
        assert!(!classify_with_rules_and_places("Bash", &copy, &root, &rules, &none).needs_a_human());
        assert!(!classify_in_manual_with_places("Bash", &copy, &root, &rules, &none).needs_a_human());

        let auto_write = classify_with_places("Write", &write, &root, &places);
        assert_eq!(auto_write.reason, REASON_EITRI_OWN_PLACES);
        assert!(auto_write.needs_a_human());
        for classification in [
            classify_with_rules_and_places("Bash", &copy, &root, &rules, &places),
            classify_in_manual_with_places("Bash", &copy, &root, &rules, &places),
        ] {
            assert!(classification.needs_a_human());
            assert_eq!(classification.reason, REASON_EITRI_OWN_PLACES);
        }
        assert_eq!(
            rule_that_allows_with_places("Bash", &copy, &root, &rules, &places),
            None
        );
        assert!(!super::super::REPLACEABLE_BY_A_RULE.contains(&REASON_EITRI_OWN_PLACES));

        // A place that cannot be established keeps its own card where the policy's would be one a rule answers.
        std::os::unix::fs::symlink(conf.join("loop"), conf.join("loop")).unwrap();
        let unknown = EitriPlaces::from_values(&EitriPlaceInputs {
            home: Some(home.as_os_str()),
            eitri_config_dir: Some(conf.as_os_str()),
            ..EitriPlaceInputs::default()
        });
        let classification = classify_with_rules_and_places("Bash", &copy, &root, &rules, &unknown);
        assert!(classification.needs_a_human());
        assert_eq!(classification.reason, REASON_EITRI_PLACE_UNKNOWN);
        assert!(!super::super::REPLACEABLE_BY_A_RULE.contains(&REASON_EITRI_PLACE_UNKNOWN));
    }
}
