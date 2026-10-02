//! What one tmux command means for Eitri: a step of the import, a source-file to follow, nothing
//! (an option or anything else that is not a key), or a skip with its reason.
//!
//! Only commands with a known Eitri action are imported, and only with the flags listed for each:
//! a flag outside that list skips the binding rather than being ignored, because a flag can turn a
//! command into its opposite (`kill-window -a` closes every window but this one).

use super::lex::{self, Arg, Command, Vars};
use super::Step;
use crate::keymap::{Action, KeySpec, SwapTarget, TabAction};
use crate::layout::{Axis, Direction};

/// What a top-level statement's command does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Outcome {
    Step(Step),
    /// `source-file`, with the paths as written (already expanded by the lexer) and `-q`.
    Source {
        quiet: bool,
        paths: Vec<String>,
    },
    /// Not about keys: an option, an environment variable, a session.
    Ignore,
    Skip(String),
}

/// tmux 3.7's commands and their aliases (`cmd-*.c`), for resolving a name the way tmux does: an
/// exact alias, else an exact name, else a name it is the unique start of.
const COMMANDS: &[(&str, Option<&str>)] = &[
    ("attach-session", Some("attach")),
    ("bind-key", Some("bind")),
    ("break-pane", Some("breakp")),
    ("capture-pane", Some("capturep")),
    ("choose-buffer", None),
    ("choose-client", None),
    ("choose-tree", None),
    ("clear-history", Some("clearhist")),
    ("clear-prompt-history", Some("clearphist")),
    ("clock-mode", None),
    ("command-prompt", None),
    ("confirm-before", Some("confirm")),
    ("copy-mode", None),
    ("customize-mode", None),
    ("delete-buffer", Some("deleteb")),
    ("detach-client", Some("detach")),
    ("display-menu", Some("menu")),
    ("display-message", Some("display")),
    ("display-panes", Some("displayp")),
    ("display-popup", Some("popup")),
    ("find-window", Some("findw")),
    ("has-session", Some("has")),
    ("if-shell", Some("if")),
    ("join-pane", Some("joinp")),
    ("kill-pane", Some("killp")),
    ("kill-server", None),
    ("kill-session", None),
    ("kill-window", Some("killw")),
    ("last-pane", Some("lastp")),
    ("last-window", Some("last")),
    ("link-window", Some("linkw")),
    ("list-buffers", Some("lsb")),
    ("list-clients", Some("lsc")),
    ("list-commands", Some("lscm")),
    ("list-keys", Some("lsk")),
    ("list-panes", Some("lsp")),
    ("list-sessions", Some("ls")),
    ("list-windows", Some("lsw")),
    ("load-buffer", Some("loadb")),
    ("lock-client", Some("lockc")),
    ("lock-server", Some("lock")),
    ("lock-session", Some("locks")),
    ("move-pane", Some("movep")),
    ("move-window", Some("movew")),
    ("new-pane", Some("newp")),
    ("new-session", Some("new")),
    ("new-window", Some("neww")),
    ("next-layout", Some("nextl")),
    ("next-window", Some("next")),
    ("paste-buffer", Some("pasteb")),
    ("pipe-pane", Some("pipep")),
    ("previous-layout", Some("prevl")),
    ("previous-window", Some("prev")),
    ("refresh-client", Some("refresh")),
    ("rename-session", Some("rename")),
    ("rename-window", Some("renamew")),
    ("resize-pane", Some("resizep")),
    ("resize-window", Some("resizew")),
    ("respawn-pane", Some("respawnp")),
    ("respawn-window", Some("respawnw")),
    ("rotate-window", Some("rotatew")),
    ("run-shell", Some("run")),
    ("save-buffer", Some("saveb")),
    ("select-layout", Some("selectl")),
    ("select-pane", Some("selectp")),
    ("select-window", Some("selectw")),
    ("send-keys", Some("send")),
    ("send-prefix", None),
    ("server-access", None),
    ("set-buffer", Some("setb")),
    ("set-environment", Some("setenv")),
    ("set-hook", None),
    ("set-option", Some("set")),
    ("set-window-option", Some("setw")),
    ("show-buffer", Some("showb")),
    ("show-environment", Some("showenv")),
    ("show-hooks", None),
    ("show-messages", Some("showmsgs")),
    ("show-options", Some("show")),
    ("show-prompt-history", Some("showphist")),
    ("show-window-options", Some("showw")),
    ("source-file", Some("source")),
    ("split-window", Some("splitw")),
    ("start-server", Some("start")),
    ("suspend-client", Some("suspendc")),
    ("swap-pane", Some("swapp")),
    ("swap-window", Some("swapw")),
    ("switch-client", Some("switchc")),
    ("unbind-key", Some("unbind")),
    ("unlink-window", Some("unlinkw")),
    ("wait-for", Some("wait")),
];

/// The flags each command this module reads takes, in tmux's own getopt spelling (a letter
/// followed by `:` takes a value) -- copied from each command's `args` template in tmux 3.7.
fn template(name: &str) -> Option<&'static str> {
    Some(match name {
        "bind-key" => "nrN:T:",
        "unbind-key" => "anqT:",
        "set-option" => "aFgopqst:uUw",
        "source-file" => "t:Fnqv",
        "split-window" => "bc:de:EfF:hIkl:m:p:PR:s:S:t:vZ",
        "select-pane" => "DdegLlMmP:RT:t:UZ",
        "last-pane" => "det:Z",
        "resize-pane" => "DLMRTt:Ux:y:Z",
        "swap-pane" => "dDs:t:UZ",
        "select-layout" => "Enopt:",
        "kill-pane" | "kill-window" | "next-window" | "previous-window" => "at:",
        "last-window" => "t:",
        "new-window" => "abc:de:F:kn:PSt:",
        "command-prompt" => "1CbeFiklI:Np:t:T:",
        "choose-tree" => "F:f:GK:NO:rst:wyZ",
        "copy-mode" => "deHMqSs:t:u",
        "send-keys" => "c:FHKlMN:Rt:X",
        "send-prefix" => "2t:",
        "confirm-before" => "bc:p:t:y",
        "select-window" => "lnpTt:",
        _ => return None,
    })
}

/// `name` as tmux resolves it, or why tmux would not.
fn resolve(name: &str) -> Result<&'static str, String> {
    if let Some((full, _)) = COMMANDS.iter().find(|(_, alias)| *alias == Some(name)) {
        return Ok(full);
    }
    if let Some((full, _)) = COMMANDS.iter().find(|(full, _)| *full == name) {
        return Ok(full);
    }
    // tmux 3.7 has neither, but older tmux did and old configs still say them; they are its
    // chooser, which Eitri's tab chooser is.
    if matches!(name, "choose-window" | "choose-session") {
        return Ok(if name == "choose-window" {
            "choose-window"
        } else {
            "choose-session"
        });
    }
    let starts: Vec<&str> = COMMANDS
        .iter()
        .map(|(full, _)| *full)
        .filter(|full| full.starts_with(name))
        .collect();
    match starts.as_slice() {
        [one] if !name.is_empty() => Ok(one),
        [] | [_] => Err(refuses(format!("unknown command {name}"))),
        _ => Err(refuses(format!("ambiguous command {name}"))),
    }
}

fn refuses(why: impl std::fmt::Display) -> String {
    format!("tmux refuses it: {why}")
}

fn no_equivalent(what: &str) -> String {
    format!("no Eitri equivalent: {what}")
}

const FORMAT: &str = "a format (#{...}), which only tmux can expand";
const ROOT: &str = "a root key: without the prefix, keys belong to nvim and the panel in Eitri";

/// A command's flags and the arguments after them, read as tmux's getopt does: flags until the
/// first word that does not start with `-`, or a lone `-`, or `--`; a flag that takes a value takes
/// the rest of its word or the next word.
#[derive(Debug)]
struct Parsed {
    flags: Vec<(char, Option<String>)>,
    rest: Vec<Arg>,
}

impl Parsed {
    fn has(&self, flag: char) -> bool {
        self.flags.iter().any(|(f, _)| *f == flag)
    }

    fn value(&self, flag: char) -> Option<&str> {
        self.flags
            .iter()
            .rev()
            .find(|(f, _)| *f == flag)
            .and_then(|(_, v)| v.as_deref())
    }

    /// Every flag given must be one of `allowed`; the first that is not is the skip.
    fn only(&self, allowed: &str) -> Result<(), String> {
        match self.flags.iter().find(|(f, _)| !allowed.contains(*f)) {
            Some((f, _)) => Err(format!("flag -{f} not imported")),
            None => Ok(()),
        }
    }

    /// No arguments after the flags beyond the first `n`.
    fn at_most(&self, n: usize) -> Result<(), String> {
        match self.rest.get(n) {
            Some(Arg::Word(w)) => Err(format!("argument {w} not imported")),
            Some(Arg::Block(_)) => Err("argument { ... } not imported".to_string()),
            None => Ok(()),
        }
    }
}

fn getopt(template: &str, args: &[Arg]) -> Result<Parsed, String> {
    let mut flags = Vec::new();
    let mut i = 0;
    while let Some(Arg::Word(word)) = args.get(i) {
        let Some(cluster) = word.strip_prefix('-') else { break };
        if cluster.is_empty() {
            break;
        }
        i += 1;
        if cluster == "-" {
            break;
        }
        for (at, flag) in cluster.char_indices() {
            if !flag.is_ascii_alphanumeric() {
                return Err(refuses(format!("invalid flag -{flag}")));
            }
            let Some(found) = template.find(flag) else {
                return Err(refuses(format!("unknown flag -{flag}")));
            };
            if template[found + 1..].starts_with(':') {
                let rest = &cluster[at + flag.len_utf8()..];
                let value = if !rest.is_empty() {
                    rest.to_string()
                } else {
                    match args.get(i) {
                        Some(Arg::Word(w)) => {
                            i += 1;
                            w.clone()
                        }
                        Some(Arg::Block(_)) => return Err(refuses(format!("-{flag} argument must be a string"))),
                        None => return Err(refuses(format!("-{flag} expects an argument"))),
                    }
                };
                flags.push((flag, Some(value)));
                break;
            }
            flags.push((flag, None));
        }
    }
    Ok(Parsed {
        flags,
        rest: args[i..].to_vec(),
    })
}

fn command_name(command: &Command) -> Result<&str, String> {
    match command.first() {
        Some(Arg::Word(name)) if name.contains("#{") => Err(FORMAT.to_string()),
        Some(Arg::Word(name)) => Ok(name),
        _ => Err(refuses("a { ... } block where a command name was expected")),
    }
}

/// A top-level statement's command.
pub(super) fn top_level(command: &Command, vars: &Vars) -> Outcome {
    let name = match command_name(command).and_then(resolve) {
        Ok(name) => name,
        Err(why) => {
            // A command tmux does not know is tmux's error, not a key Eitri missed; only report it
            // when it looks like it was about keys.
            return match command_name(command) {
                Ok(raw) if raw.starts_with("bind") || raw.starts_with("unbind") => Outcome::Skip(why),
                Ok(raw) if raw.contains("#{") => Outcome::Skip(why),
                _ => Outcome::Ignore,
            };
        }
    };
    let args = &command[1..];
    match name {
        "bind-key" => match bind(args, vars) {
            Ok(step) => Outcome::Step(step),
            Err(why) => Outcome::Skip(why),
        },
        "unbind-key" => match unbind(args) {
            Ok(step) => Outcome::Step(step),
            Err(why) => Outcome::Skip(why),
        },
        "set-option" => set_option(args),
        "source-file" => source_file(args),
        "run-shell" | "if-shell" => Outcome::Skip(format!("{name} runs a shell command, which Eitri never does")),
        _ => Outcome::Ignore,
    }
}

fn set_option(args: &[Arg]) -> Outcome {
    let Ok(parsed) = getopt(template("set-option").expect("listed"), args) else {
        return Outcome::Ignore;
    };
    let option = parsed.rest.first().and_then(Arg::word);
    match option {
        Some("prefix2") => Outcome::Skip("prefix2 is not imported".to_string()),
        Some("prefix") => match prefix(&parsed) {
            Ok(key) => Outcome::Step(Step::Prefix(key)),
            Err(why) => Outcome::Skip(why),
        },
        _ => Outcome::Ignore,
    }
}

fn prefix(parsed: &Parsed) -> Result<KeySpec, String> {
    parsed.only("gq")?;
    if !parsed.has('g') {
        // Measured: tmux reading `set prefix C-a` at startup leaves the prefix as it was.
        return Err("set prefix without -g: tmux has no session to set it on while it reads its config".to_string());
    }
    parsed.at_most(2)?;
    let key = parsed
        .rest
        .get(1)
        .and_then(Arg::word)
        .ok_or_else(|| refuses("set prefix needs a key"))?;
    if key.contains("#{") {
        return Err(FORMAT.to_string());
    }
    KeySpec::parse(key).map_err(|e| e.to_string())
}

fn source_file(args: &[Arg]) -> Outcome {
    let parsed = match getopt(template("source-file").expect("listed"), args) {
        Ok(parsed) => parsed,
        Err(why) => return Outcome::Skip(why),
    };
    if parsed.has('n') {
        return Outcome::Skip("source-file -n only parses the file; tmux runs nothing from it".to_string());
    }
    if parsed.has('F') {
        return Outcome::Skip(format!("source-file -F names the file with a format; {FORMAT}"));
    }
    if let Err(why) = parsed.only("qv") {
        return Outcome::Skip(why);
    }
    let mut paths = Vec::new();
    for arg in &parsed.rest {
        match arg {
            Arg::Word(w) => paths.push(w.clone()),
            Arg::Block(_) => return Outcome::Skip(refuses("a { ... } block where a path was expected")),
        }
    }
    if paths.is_empty() {
        return Outcome::Skip(refuses("source-file needs a path"));
    }
    Outcome::Source {
        quiet: parsed.has('q'),
        paths,
    }
}

fn table_of(parsed: &Parsed) -> Result<(), String> {
    let table = parsed.value('T').map(str::to_string).unwrap_or_else(|| {
        if parsed.has('n') {
            "root".into()
        } else {
            "prefix".into()
        }
    });
    match table.as_str() {
        "prefix" => Ok(()),
        "root" => Err(ROOT.to_string()),
        other => Err(format!("the {other} table is not imported")),
    }
}

fn key_of(arg: Option<&Arg>) -> Result<KeySpec, String> {
    match arg {
        Some(Arg::Word(word)) if word.contains("#{") => Err(FORMAT.to_string()),
        Some(Arg::Word(word)) => KeySpec::parse(word).map_err(|e| e.to_string()),
        Some(Arg::Block(_)) => Err(refuses("a { ... } block where a key was expected")),
        None => Err(refuses("missing key")),
    }
}

fn unbind(args: &[Arg]) -> Result<Step, String> {
    let parsed = getopt(template("unbind-key").expect("listed"), args)?;
    table_of(&parsed)?;
    if parsed.has('a') {
        parsed.at_most(0).map_err(|_| refuses("key given with -a"))?;
        return Ok(Step::UnbindAll);
    }
    let key = key_of(parsed.rest.first())?;
    parsed.at_most(1)?;
    Ok(Step::Unbind(key))
}

fn bind(args: &[Arg], vars: &Vars) -> Result<Step, String> {
    let parsed = getopt(template("bind-key").expect("listed"), args)?;
    table_of(&parsed)?;
    let key = key_of(parsed.rest.first())?;
    let commands = commands_of(&parsed.rest[1..], vars)?;
    let action = match commands.as_slice() {
        [] => return Err("binds the key to no command".to_string()),
        [one] => action(one, vars)?,
        _ => return Err("more than one command, which one Eitri key cannot run".to_string()),
    };
    Ok(Step::Bind {
        key,
        action,
        repeatable: parsed.has('r'),
    })
}

/// The commands a key binding (or `confirm-before`, or `command-prompt`) runs, from its arguments
/// as tmux reads them: a `{ ... }` block; one word, read again as a command string; or the words
/// themselves, split into commands where a word ends in `;` (which is what `\;` gives).
fn commands_of(args: &[Arg], vars: &Vars) -> Result<Vec<Command>, String> {
    match args {
        [] => Ok(Vec::new()),
        [Arg::Block(commands)] => Ok(commands.clone()),
        [Arg::Word(text)] => {
            let first = text.split_whitespace().next().unwrap_or("");
            if first.contains("#{") {
                return Err(FORMAT.to_string());
            }
            lex::commands_in(text, vars).map_err(refuses)
        }
        _ => {
            let mut commands = Vec::new();
            let mut current: Command = Vec::new();
            for arg in args {
                match arg {
                    Arg::Word(word) if word.ends_with(';') && !word.ends_with("\\;") => {
                        let word = &word[..word.len() - 1];
                        if !word.is_empty() {
                            current.push(Arg::Word(word.to_string()));
                        }
                        if !current.is_empty() {
                            commands.push(std::mem::take(&mut current));
                        }
                    }
                    other => current.push(other.clone()),
                }
            }
            if !current.is_empty() {
                commands.push(current);
            }
            Ok(commands)
        }
    }
}

fn one_command(args: &[Arg], vars: &Vars, what: &str) -> Result<Command, String> {
    let commands = commands_of(args, vars)?;
    match commands.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(refuses(format!("{what} needs a command"))),
        _ => Err("more than one command, which one Eitri key cannot run".to_string()),
    }
}

/// Whether every pane can be handed `key` by `send-keys`. The editor takes any key and the
/// terminal a Ctrl letter or one plain character (`shell`'s `terminal::keys::literal`, which a test
/// there holds this to), but the panel's composer acts only on `C-a`, select all, and drops the
/// rest (`App.tsx`'s `literal_key` handling, which a test here reads). So `C-a` is the one key a
/// `send-keys` binding can deliver wherever the keys are; any other is skipped, not imported to do
/// nothing in the panel.
pub fn every_pane_takes(key: &KeySpec) -> bool {
    *key == KeySpec::char('a', true, false)
}

fn direction_flag(parsed: &Parsed, order: &str) -> Option<Direction> {
    order.chars().find(|f| parsed.has(*f)).map(|f| match f {
        'L' => Direction::Left,
        'R' => Direction::Right,
        'U' => Direction::Up,
        _ => Direction::Down,
    })
}

/// The Eitri action one bound command maps to, or why it does not.
fn action(command: &Command, vars: &Vars) -> Result<Action, String> {
    let name = resolve(command_name(command)?)?;
    let args = &command[1..];
    let parsed = match template(name) {
        Some(t) => getopt(t, args)?,
        None if matches!(name, "choose-window" | "choose-session") => getopt("", args)?,
        None => return Err(no_equivalent(name)),
    };
    match name {
        "send-prefix" => {
            if parsed.has('2') {
                return Err("prefix2 is not imported".to_string());
            }
            parsed.only("")?;
            parsed.at_most(0)?;
            Ok(Action::SendPrefix)
        }
        "send-keys" => {
            parsed.only("")?;
            let key = key_of(parsed.rest.first()).map_err(|e| {
                if e.ends_with("missing key") {
                    refuses("send-keys needs a key")
                } else {
                    e
                }
            })?;
            parsed.at_most(1)?;
            if !every_pane_takes(&key) {
                return Err(format!("Eitri cannot send {key} to every pane"));
            }
            Ok(Action::SendKeys(key))
        }
        "split-window" => {
            parsed.only("hvcplbdf")?;
            parsed.at_most(0)?;
            // tmux: -h splits left/right whatever else is given.
            Ok(Action::Split(if parsed.has('h') { Axis::Row } else { Axis::Column }))
        }
        "select-pane" => {
            parsed.only("LDURlt")?;
            parsed.at_most(0)?;
            if let Some(target) = parsed.value('t') {
                if target != ":.+" {
                    return Err(format!("-t {target} not imported"));
                }
                if parsed.flags.len() == 1 {
                    return Ok(Action::SelectNext);
                }
            }
            if parsed.has('l') {
                return Ok(Action::SelectLast);
            }
            // tmux checks the directions in this order.
            match direction_flag(&parsed, "LRUD") {
                Some(dir) if !parsed.has('t') => Ok(Action::Select(dir)),
                Some(_) => Err("flag -t not imported".to_string()),
                None => Err(no_equivalent("select-pane with no direction")),
            }
        }
        "last-pane" => {
            parsed.only("")?;
            Ok(Action::SelectLast)
        }
        "resize-pane" => {
            parsed.only("LDURZ")?;
            if parsed.has('Z') {
                // tmux zooms and ignores everything else; anything else given is not imported.
                parsed.only("Z")?;
                parsed.at_most(0)?;
                return Ok(Action::Zoom);
            }
            parsed.at_most(1)?;
            let Some(dir) = direction_flag(&parsed, "LRUD") else {
                return Err(no_equivalent("resize-pane with no direction"));
            };
            let cells = match parsed.rest.first() {
                None => 1,
                Some(Arg::Word(n)) => n
                    .parse::<u16>()
                    .ok()
                    .filter(|n| (1..=500).contains(n))
                    .ok_or_else(|| format!("a count of 1 to 500 cells, not {n}"))?,
                Some(Arg::Block(_)) => return Err("argument { ... } not imported".to_string()),
            };
            Ok(Action::Resize { dir, cells })
        }
        "swap-pane" => {
            parsed.only("UDt")?;
            parsed.at_most(0)?;
            if let Some(target) = parsed.value('t') {
                let dir = match target {
                    "{left-of}" => Direction::Left,
                    "{right-of}" => Direction::Right,
                    "{up-of}" => Direction::Up,
                    "{down-of}" => Direction::Down,
                    other => return Err(format!("-t {other} not imported")),
                };
                if parsed.flags.len() != 1 {
                    return Err("more than one target, which Eitri cannot follow".to_string());
                }
                return Ok(Action::Swap(SwapTarget::Toward(dir)));
            }
            // tmux checks -D before -U.
            if parsed.has('D') {
                Ok(Action::Swap(SwapTarget::Next))
            } else if parsed.has('U') {
                Ok(Action::Swap(SwapTarget::Prev))
            } else {
                Err(no_equivalent("swap-pane with no direction"))
            }
        }
        "select-layout" => {
            parsed.only("")?;
            parsed.at_most(1)?;
            match parsed.rest.first().and_then(Arg::word) {
                Some("even-horizontal") => Ok(Action::Even(Axis::Row)),
                Some("even-vertical") => Ok(Action::Even(Axis::Column)),
                Some(other) => Err(no_equivalent(&format!("select-layout {other}"))),
                None => Err(no_equivalent("select-layout")),
            }
        }
        "kill-pane" => {
            parsed.only("")?;
            parsed.at_most(0)?;
            Ok(Action::ModuleKill)
        }
        "kill-window" => {
            parsed.only("")?;
            parsed.at_most(0)?;
            // Eitri always asks y/n before closing a tab.
            Ok(Action::Tab(TabAction::Close))
        }
        "new-window" => {
            parsed.only("cna")?;
            parsed.at_most(0)?;
            Ok(Action::Tab(TabAction::New))
        }
        "next-window" | "previous-window" | "last-window" => {
            parsed.only("")?;
            parsed.at_most(0)?;
            Ok(Action::Tab(match name {
                "next-window" => TabAction::Next,
                "previous-window" => TabAction::Prev,
                _ => TabAction::Last,
            }))
        }
        "command-prompt" => {
            if parsed.rest.is_empty() {
                parsed.only("")?;
                return Ok(Action::PanelCommandLine);
            }
            let inner = one_command(&parsed.rest, vars, "command-prompt")
                .map_err(|_| no_equivalent("command-prompt with that template"))?;
            match command_name(&inner).and_then(resolve) {
                Ok("rename-window") => {
                    parsed.only("Ip")?;
                    Ok(Action::Tab(TabAction::Rename))
                }
                _ => Err(no_equivalent("command-prompt with that template")),
            }
        }
        "choose-tree" | "choose-window" | "choose-session" => {
            parsed.only("")?;
            parsed.at_most(0)?;
            Ok(Action::Tab(TabAction::Choose))
        }
        "copy-mode" => {
            parsed.only("u")?;
            parsed.at_most(0)?;
            Ok(Action::CopyMode { up: parsed.has('u') })
        }
        "confirm-before" => {
            parsed.only("py")?;
            let inner = one_command(&parsed.rest, vars, "confirm-before")?;
            let inner_name = resolve(command_name(&inner)?)?;
            match inner_name {
                // Eitri asks y/n itself before either.
                "kill-pane" | "kill-window" => action(&inner, vars),
                other => Err(no_equivalent(&format!("confirm-before {other}"))),
            }
        }
        "select-window" => Err("select-window: Eitri's 1-9 already select tabs, counted from 1".to_string()),
        other => Err(no_equivalent(other)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_resolve_as_tmux_resolves_them() {
        assert_eq!(resolve("bind"), Ok("bind-key"));
        assert_eq!(resolve("splitw"), Ok("split-window"));
        assert_eq!(resolve("split"), Ok("split-window"), "the unique start of a name");
        assert_eq!(
            resolve("next"),
            Ok("next-window"),
            "an alias beats being the start of next-layout"
        );
        assert!(resolve("kill").unwrap_err().contains("ambiguous"));
        assert!(resolve("nope").unwrap_err().contains("unknown command nope"));
    }

    #[test]
    fn flags_read_as_getopt_reads_them() {
        let w = |s: &str| Arg::Word(s.to_string());
        let parsed = getopt("nrN:T:", &[w("-rT"), w("prefix"), w("x"), w("-y")]).unwrap();
        assert_eq!(parsed.flags, vec![('r', None), ('T', Some("prefix".into()))]);
        assert_eq!(parsed.rest, vec![w("x"), w("-y")]);
        let parsed = getopt("nrN:T:", &[w("-Tcopy-mode"), w("--"), w("-")]).unwrap();
        assert_eq!(parsed.value('T'), Some("copy-mode"));
        assert_eq!(parsed.rest, vec![w("-")]);
        assert!(getopt("nr", &[w("-x")]).unwrap_err().contains("unknown flag -x"));
    }

    #[test]
    fn wildcard_matching_takes_linear_time_on_a_pathological_pattern() {
        let pattern = format!("{}z", "*".repeat(32));
        let name = "a".repeat(64);
        let started = std::time::Instant::now();
        assert!(!super::super::fnmatch(&pattern, &name));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        assert!(super::super::fnmatch("a*b*c*", "aXbYc"));
        assert!(super::super::fnmatch("**x", "abx"));
        assert!(!super::super::fnmatch("*[0-9]", "abc"));
        assert!(super::super::fnmatch("*[0-9]", "ab7"));
    }

    #[test]
    fn what_every_pane_can_be_sent() {
        let k = |s: &str| KeySpec::parse(s).unwrap();
        assert!(every_pane_takes(&k("C-a")));
        for no in [
            "C-l", "q", "Q", "%", "é", "Enter", "Up", "F5", "M-x", "C-M-a", "C-=", "Space", "S-Up",
        ] {
            assert!(!every_pane_takes(&k(no)), "{no}");
        }
    }
}
