//! Which tool calls actually need a human, and which ones do not.
//!
//! # Why this exists
//!
//! Both backends install a `PreToolUse` gate whose matcher is `*` -- the legacy one generates the
//! hook itself (`crate::process`), the sidecar gets one from Verdandi's `permissionBroker.ts`. That
//! matcher is CORRECT and is not what this module changes: "the host sees every call" is the right
//! layering, because the policy of what needs a human belongs to the product that has the human.
//!
//! What was wrong is that neovibe equated "the host was asked" with "the user must answer", and
//! forwarded every single call to a GUI card. On 2026-09-19 the owner reported the consequence in
//! four words -- "auto mode, 一直弹窗" -- a card for every `Read`, every `Grep`, every time. His
//! stated rule for the fix was to match the real Claude CLI: "auto mode 我的想法不应该是和 claude cli
//! 一样吗，你看下 claude cli 是什么规则，尽量做到行为一样".
//!
//! # Why it lives in `agent` and not in `neovibe-core`
//!
//! Both crates can reach it either way (`neovibe-core` depends on `agent`, never the reverse), so
//! the tie is broken by what the code is ABOUT. This is a statement about Claude's own tool
//! vocabulary -- `Read`'s `file_path`, `Bash`'s `command`, the CLI's own default-mode rules -- and
//! every other Claude-specific fact in this workspace lives in `agent`. It also puts the policy in
//! the same crate as the real-CLI conformance tests that check it against the binary
//! (`agent/tests/permission_policy_conformance.rs`), which is where the oracle is.
//!
//! # The rule being reproduced
//!
//! The real CLI's *interactive* `default` mode, with no configured permission rules. That mode is
//! also observable headlessly, which is what makes this testable rather than a table copied out of
//! documentation: `claude --print` with **no** `--permission-mode` flag applies the same
//! classification and, where it would have prompted, **denies** instead. So "what the CLI allows
//! under `default` with no hook" is exactly "what needs no human". Measured on CLI 2.1.272,
//! 2026-09-19: `Read` allowed, `Write` denied and the file really not created; `ls`, `cat a.txt`
//! and `git status --short` allowed; `echo hi > c.txt`, `rm b.txt` and `git commit` denied.
//!
//! Note that `--permission-mode auto` -- which is the flag neovibe passes, and which this change
//! does not touch -- is a different thing again: it auto-approves and has a separate classifier
//! model review actions in the background. The rules reproduced here are `default`'s.
//!
//! # The one rule that governs every uncertainty
//!
//! **Anything this module cannot classify with confidence gets a card.** Not a best guess, not a
//! first-word match, not a partial parse. Getting a classification wrong in the permissive
//! direction is a security defect; getting it wrong in the other direction costs a click. Concrete
//! consequences, all deliberate:
//!
//! - There is no shell parser here and there will not be one. Any `Bash` command carrying a
//!   metacharacter that could redirect, chain, glob, substitute or quote goes to a card, even
//!   though many such commands are harmless. The measured case this exists for is `echo hi >
//!   c.txt`: `echo` IS on the CLI's read-only list and the call was still refused, because the
//!   redirect is checked separately. A classifier that matched the first word and stopped would be
//!   wrong, in the permissive direction, on a command models emit constantly.
//! - `WebFetch` always gets a card in this version. The CLI exempts "preapproved documentation
//!   domains" and that list is not published in any documentation this project could fetch, so
//!   there is nothing to reproduce and guessing at it is the same permissive error.
//! - An unknown tool name gets a card. New tools appear in CLI releases; the failure mode for a
//!   tool this table has never heard of must be a click, not a silent grant.
//! - Input JSON that is not the shape the tool declares -- not an object, a missing path, a
//!   `command` that is not a string -- gets a card. A classifier that cannot read the call cannot
//!   judge it.
//!
//! # What this module is NOT
//!
//! It is not `crate::CONSERVATIVE_DISALLOWED_TOOLS`, and the two must not be merged. That array is
//! a floor for `Bypass` -- tools the model may not use *at all* when nothing is gating it -- and it
//! is hand-copied into Verdandi as `CONSERVATIVE_BYPASS_DENY`, deliberately in the same order so
//! the two can be grepped across repositories. This module classifies calls that DID reach a live
//! gate. Different question, different answer, unchanged array.

use serde_json::Value;
use std::path::{Path, PathBuf};

/// What to do with one tool call that reached the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionVerdict {
    /// Answer it here, immediately, and never draw a card. The user still sees the tool call
    /// itself in the transcript -- this suppresses the *question*, not the record.
    AllowWithoutAsking,
    /// Put it in front of the user. Every uncertainty lands here.
    AskTheUser,
}

/// A verdict and the reason for it. The reason is `&'static str` on purpose: it is a fixed label
/// for a branch of this function, not a message built from the call's own (attacker-influenced)
/// contents, and it is what a log line or a failing test prints instead of a bare boolean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Classification {
    pub verdict: PermissionVerdict,
    pub reason: &'static str,
}

impl Classification {
    pub fn needs_a_human(&self) -> bool {
        self.verdict == PermissionVerdict::AskTheUser
    }
}

fn allow(reason: &'static str) -> Classification {
    Classification {
        verdict: PermissionVerdict::AllowWithoutAsking,
        reason,
    }
}

fn ask(reason: &'static str) -> Classification {
    Classification {
        verdict: PermissionVerdict::AskTheUser,
        reason,
    }
}

/// Tools the CLI asks about every time in `default` mode, decidable from the name alone.
///
/// `WebFetch` is here for a different reason from the rest -- see the module doc's rule (d): the
/// CLI does exempt some documentation domains, and this version refuses to guess which.
pub const ALWAYS_ASK_TOOLS: &[&str] = &["Write", "Edit", "MultiEdit", "NotebookEdit", "WebSearch", "WebFetch"];

/// Tools that change no file, run no command and reach no network -- the CLI does not ask about
/// these and neither does this.
///
/// **`Task` is here on documentation alone and is the weakest row in this table.** The CLI does not
/// prompt for a subagent launch because the subagent's own tool calls are checked individually;
/// that is stated in the docs and is *not* something this project has observed -- nothing here has
/// confirmed that a subagent's inner calls reach this same `PreToolUse` gate. If that assumption is
/// ever disproved, this entry is a hole and must be the first thing removed.
///
/// **`ToolSearch` is here on a measurement, and it is the row that made auto mode usable**
/// (2026-09-19, later). On CLI 2.1.272 the tools are *deferred*: the model is given their names
/// only, and has to call `ToolSearch` to load a schema before it can call anything at all. Every
/// one of those searches reaches this gate -- `agent/tests/backend_conformance.rs` records the same
/// thing from the product side -- so while `ToolSearch` was "not in the policy's table" the user got
/// a card before nearly every action the agent took, which is what the owner reported as auto mode
/// still being a wall of popups. Measured against the oracle the same way as everything else here:
/// `claude --print` with no `--permission-mode`, which denies where it would have prompted, **ran**
/// `ToolSearch {"query": "select:Read"}` and returned the schema. It loads a description; it opens
/// no file, runs no command and reaches no network.
pub const NEVER_ASK_TOOLS: &[&str] = &["TodoWrite", "Task", "ToolSearch"];

/// The tools CLI 2.1.272 actually offers, read off its own `system/init` on 2026-09-19 (later).
///
/// It is here to keep one fact visible: **the two tables above were written against a tool set that
/// is not this one.** `Grep`, `Glob`, `MultiEdit` and `TodoWrite` are named by the policy and do not
/// exist on this build; eighteen tools that DO exist were unknown to it, and an unknown tool is a
/// card. Most of those are rare enough that a card is the right answer and no worse than an
/// annoyance -- and several (`SendMessage`, `PushNotification`, `RemoteTrigger`, `CronCreate`,
/// `EnterWorktree`) reach outside this machine or change the tree, so they must stay cards. Only
/// `ToolSearch` was frequent enough to matter, and only it has been measured. **Do not promote a
/// name off this list without its own oracle run**; the list is a record of what exists, not a
/// judgement about any of it.
pub const TOOLS_OFFERED_BY_CLI_2_1_272: &[&str] = &[
    "Task",
    "Bash",
    "CronCreate",
    "CronDelete",
    "CronList",
    "DesignSync",
    "Edit",
    "EnterWorktree",
    "ExitWorktree",
    "ListAgents",
    "LSP",
    "Monitor",
    "NotebookEdit",
    "PushNotification",
    "Read",
    "RemoteTrigger",
    "ReportFindings",
    "ScheduleWakeup",
    "SendMessage",
    "Skill",
    "TaskOutput",
    "TaskStop",
    "ToolSearch",
    "WebFetch",
    "WebSearch",
    "Write",
];

/// The read-only `Bash` commands the CLI's own documentation names, verbatim and in its order.
///
/// Being on this list is necessary and **nowhere near sufficient**: see
/// `classify_bash` below and the module doc. `echo` is on this list and `echo hi >
/// c.txt` was measured DENIED.
pub const READ_ONLY_BASH_COMMANDS: &[&str] = &[
    "ls", "cat", "echo", "pwd", "head", "tail", "grep", "find", "wc", "which", "diff", "stat", "du", "cd",
];

/// `git` subcommands that only read. Deliberately narrower than "read-only git forms" in the docs:
/// `branch`, `tag`, `config` and `stash` all have writing forms distinguished only by their
/// arguments, and distinguishing them is exactly the kind of partial parse this module refuses to
/// do. `git commit` was measured denied; nothing here would have allowed it anyway.
pub const READ_ONLY_GIT_SUBCOMMANDS: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "blame",
    "rev-parse",
    "ls-files",
    "shortlog",
    "describe",
];

/// The docs name 10,000 characters as a length past which the CLI re-prompts even for an otherwise
/// read-only command. Reproduced rather than reasoned about.
pub const MAX_BASH_COMMAND_LEN: usize = 10_000;

/// Characters that end the analysis and send the call to a card.
///
/// This is not a claim that each one is dangerous. It is a claim that a command containing one
/// cannot be judged by looking at its first word, which is the only thing this module knows how to
/// do. Redirects (`< > |`), chaining (`; &`), substitution (`$ ` ( )`), globs (`* ? [ ]`), braces,
/// quoting (`" '`), escapes (`\`), home expansion (`~`), history (`!`), comments (`#`) and embedded
/// newlines all qualify.
const BASH_CHARS_THAT_FORCE_A_CARD: &[char] = &[
    '|', '&', ';', '<', '>', '$', '`', '(', ')', '{', '}', '[', ']', '*', '?', '~', '!', '#', '\\', '"', '\'', '\n',
    '\r',
];

/// `find` actions that write or execute. `find` is on the CLI's read-only list, and `find . -delete`
/// is on nobody's.
///
/// Matched by PREFIX, not exactly, since 2026-09-19 (later): an exact match missed `-fprint0`,
/// which truncates and writes its argument just as `-fprint` does and was simply not in the list
/// (review finding, reproduced in scratch). `-exec` covers `-execdir`, `-ok` covers `-okdir`, and
/// `-fprint` covers `-fprint0` and `-fprintf`; a future writing action spelled with one of these
/// prefixes gets a card without anyone remembering to add it. The cost is a card for any predicate
/// that merely starts the same way, which is the right direction.
const FIND_ACTIONS_THAT_ARE_NOT_READS: &[&str] = &["-exec", "-ok", "-delete", "-fprint", "-fls"];

/// `find` options that make it follow symbolic links while it walks, so a link inside the tree
/// that points out of it is descended into. Plain `find` does not follow them (`-P` is the
/// default); these do. Exact match, because `find`'s own predicates are exact.
const FIND_OPTIONS_THAT_FOLLOW_LINKS: &[&str] = &["-L", "-follow"];

/// `git` arguments that write a file. `--output=<file>` and the separate-argument form `--output
/// <file>` are both accepted by `log`, `show` and `diff` -- every one of them on
/// [`READ_ONLY_GIT_SUBCOMMANDS`] -- and both really write: measured in a scratch repository
/// 2026-09-19, `git log --output=../o1.txt` wrote 110 bytes outside the repo and `git log --output
/// o3.txt` wrote inside it. The second form carries no `=` and no path punctuation at all, so
/// nothing else in this module would have seen it. Matched by prefix, which also cards the
/// harmless `--output-indicator-*` options; git's diff options were measured NOT to accept an
/// abbreviation (`--outp=` was rejected), so the prefix is not there to catch one.
const GIT_ARGUMENT_PREFIXES_THAT_WRITE: &[&str] = &["--output"];

/// The one entry point. `tool_name` and `input` are the `PermissionRequested` event's own fields,
/// verbatim; `project_root` is the session's canonical project directory
/// (`neovibe_core::project_root`), which is the boundary `Read`/`Grep`/`Glob` are judged against.
///
/// Total: every input produces a verdict, and every path that is not a positive, confident match
/// produces [`PermissionVerdict::AskTheUser`].
pub fn classify_permission_request(tool_name: &str, input: &Value, project_root: &Path) -> Classification {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    classify_with_home(tool_name, input, project_root, home.as_deref())
}

/// [`classify_permission_request`] with the home directory passed in rather than read from the
/// environment, so the "the root is not a boundary" branch is testable without mutating `HOME` for
/// the whole test process.
fn classify_with_home(tool_name: &str, input: &Value, project_root: &Path, home: Option<&Path>) -> Classification {
    // Before anything else: a call whose input is not even an object is not a call this module can
    // read. Both backends deliver `input` as whatever JSON arrived, so this is reachable.
    if !input.is_object() {
        return ask("the tool input is not a JSON object");
    }

    if NEVER_ASK_TOOLS.contains(&tool_name) {
        return allow("this tool changes no file, runs no command and reaches no network");
    }
    if ALWAYS_ASK_TOOLS.contains(&tool_name) {
        return ask("the CLI asks about this tool every time in its default mode");
    }

    if !matches!(tool_name, "Read" | "Grep" | "Glob" | "Bash") {
        return ask("this tool is not in the policy's table");
    }
    // Every remaining tool is judged against the project root, so the root has to be a boundary.
    let root = match boundary(project_root, home) {
        Ok(root) => root,
        Err(classification) => return classification,
    };

    match tool_name {
        // `file_path` is required by the tool's own schema, so its absence is a malformed call.
        "Read" => classify_path(input, "file_path", PathField::Required, &root),
        // `path` is optional for both, and its absence means "the working directory" -- which is
        // inside the root by definition.
        "Grep" => classify_path(input, "path", PathField::Optional, &root),
        // Glob's `pattern` is checked as well as its `path` since 2026-09-19 (later). Before, a
        // `pattern` of `/home/user/.ssh/*` or `../../**/*` with no `path` was allowed as "no path
        // given, so this reads the project root" -- review finding. Whether the CLI's Glob really
        // honours an absolute or `..` pattern was NOT checked; a pattern this module cannot confine
        // to the root gets a card either way.
        "Glob" => match input.get("pattern") {
            Some(Value::String(pattern))
                if pattern.starts_with('/') || pattern.starts_with('~') || pattern.contains("..") =>
            {
                ask("the glob pattern itself reaches outside the project")
            }
            Some(Value::String(_)) => classify_path(input, "path", PathField::Optional, &root),
            _ => ask("this Glob call has no string `pattern`"),
        },
        _ => classify_bash(input, &root),
    }
}

/// The canonical project root, or a card if it cannot serve as a boundary.
///
/// Two ways it cannot, and the second is the one that matters (review finding, 2026-09-19 later):
/// - it does not resolve on disk -- if the boundary is unknown, nothing can be judged against it;
/// - it IS the home directory, or an ancestor of it (`/` included). `project_root` falls back to
///   the process cwd when no directory is given (`neovibe_core::project_root`), and the packaged
///   `.desktop` entry passes `%f`, which is empty when neovibe is started from the app menu -- so a
///   menu launch can make `$HOME` the "project", and then every `Read` of `~/.ssh/id_ed25519` and
///   every `cat .credentials/...` would count as "inside the project". Whether a menu launch really
///   starts with cwd `$HOME` was NOT observed; the check does not depend on it.
///
/// `home` unset or unresolvable still leaves the `/` check, since `/` is an ancestor of every home.
fn boundary(project_root: &Path, home: Option<&Path>) -> Result<PathBuf, Classification> {
    let Ok(root) = project_root.canonicalize() else {
        return Err(ask("the project root itself could not be resolved"));
    };
    if root.parent().is_none() {
        return Err(ask("the project root is the filesystem root, which is no boundary"));
    }
    if let Some(home) = home.and_then(|h| h.canonicalize().ok()) {
        if home.starts_with(&root) {
            return Err(ask(
                "the project root is the home directory or above it, which is no boundary",
            ));
        }
    }
    Ok(root)
}

enum PathField {
    Required,
    Optional,
}

/// `Read`/`Grep`/`Glob` are allowed only for a path that really resolves to somewhere inside the
/// project root.
///
/// Every failure is a card, and the failures are not hypothetical:
/// - the field is present but not a string, or is empty -- malformed;
/// - the path does not exist, so `canonicalize` fails -- unresolvable;
/// - the path exists but canonicalizes outside the root -- this is what catches both `..`
///   traversal and a symlink pointing out of the tree, because `canonicalize` resolves both before
///   the containment test rather than after;
/// - the root itself will not canonicalize, or is `$HOME` or above it -- see [`boundary`], which
///   the caller has already run; `root` here is its canonical result.
///
/// Resolving BEFORE comparing is the whole design. A textual check would be defeated by
/// `src/../../etc/passwd` and by any symlink; `canonicalize` is a real syscall that follows both.
fn classify_path(input: &Value, field: &str, requirement: PathField, root: &Path) -> Classification {
    let raw = match input.get(field) {
        None | Some(Value::Null) => match requirement {
            // No path means the working directory, which is the root.
            PathField::Optional => return allow("no path given, so this reads the project root"),
            PathField::Required => return ask("this tool's required path field is missing"),
        },
        Some(Value::String(s)) => s,
        Some(_) => return ask("this tool's path field is not a string"),
    };
    if raw.is_empty() {
        return ask("this tool's path field is empty");
    }

    let candidate: PathBuf = {
        let p = Path::new(raw);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            root.join(p)
        }
    };
    let Ok(resolved) = candidate.canonicalize() else {
        return ask("the path could not be resolved on disk");
    };
    if resolved.starts_with(root) {
        allow("the path resolves inside the project root")
    } else {
        ask("the path resolves outside the project root")
    }
}

/// `Bash` is allowed only for a command this module can read in full -- which means a plain
/// sequence of whitespace-separated words, whose first word is on the CLI's own read-only list, and
/// which touches nothing outside the project by path.
///
/// **This is deliberately not a shell parser.** The measured case that dictates the shape is `echo
/// hi > c.txt`: allowlisted first word, denied by the real CLI because the redirect is checked
/// separately. Rather than reproduce that check -- and the ones for pipelines, `tee`, command
/// substitution and `&&` chains, each an opportunity to be wrong in the permissive direction -- any
/// command carrying a character that could mean any of those things goes to a card. The cost is
/// cards for harmless quoted commands. That is the correct direction to be wrong in.
///
/// The path rule is this module's own addition, not the CLI's: an argument that is absolute or
/// contains `..` reaches outside the project, and unlike `Read` there is no single field to resolve
/// and contain. So those go to a card too, which is why `cat /etc/shadow` cannot be auto-allowed.
///
/// **That rule was purely textual until 2026-09-19 (later), and an adversarial review found three
/// ways past it**, each reproduced in scratch before being fixed:
/// - a path glued to an option -- `git log --output=/abs/file` (writes the file), `diff
///   --from-file=/abs/secret` (prints it) -- neither starts with `/`. Now any argument carrying `=`
///   gets a card, and so does a short-option cluster carrying `/` (`-f/abs`);
/// - a relative path through a symlink inside the tree that points out of it -- `cat
///   escape/secret`. Now every argument that names an existing entry under the root is
///   `canonicalize`d and contained exactly as `Read`'s path is (see [`arguments_stay_inside`]);
/// - a command that FOLLOWS such links while it walks -- `grep -R`, `find -L`, `ls -L`, `du -L`,
///   and `diff -r`, which follows them by default (measured: it printed a linked-out file's
///   contents). Those options get a card; the plain recursive forms (`grep -r`, `find`, `ls -R`,
///   `du`) do not follow links found mid-walk and are unchanged.
///
/// `cd` gets a card although it is on the CLI's list: every relative argument here is judged
/// against the project root, which is only right while the Bash tool's cwd IS the root. The CLI
/// keeps a Bash cwd across calls (its binary carries a "Shell cwd was reset to" message, so it
/// resets it in at least some cases -- which ones was NOT checked), and a bare `cd` would move it to
/// `$HOME`. A standalone `cd` does nothing else, so the card costs nothing real. **Residual, not
/// closed:** a `cd` the user approved inside a carded command still moves that cwd, and after that a
/// relative argument naming a link-out that exists under the new cwd but not under the root is not
/// seen by the symlink check.
fn classify_bash(input: &Value, root: &Path) -> Classification {
    let Some(Value::String(command)) = input.get("command") else {
        return ask("this Bash call has no string `command`");
    };
    if command.len() > MAX_BASH_COMMAND_LEN {
        return ask("the command is longer than the CLI's own re-prompt threshold");
    }
    if command.contains(BASH_CHARS_THAT_FORCE_A_CARD) {
        return ask("the command contains shell syntax this policy refuses to parse");
    }

    let mut words = command.split_whitespace();
    let Some(program) = words.next() else {
        return ask("the command is empty");
    };
    let arguments: Vec<&str> = words.collect();

    if arguments.iter().any(|a| a.starts_with('/') || a.contains("..")) {
        return ask("an argument reaches outside the project by path");
    }
    if arguments.iter().any(|a| a.contains('=')) {
        return ask("an argument glues a value to an option, which this policy refuses to parse");
    }
    if arguments.iter().any(|a| is_short_option_cluster(a) && a.contains('/')) {
        return ask("an argument glues a path to a short option, which this policy refuses to parse");
    }
    if !arguments_stay_inside(&arguments, root) {
        return ask("an argument resolves outside the project root");
    }
    if follows_links_while_walking(program, &arguments) {
        return ask("this command follows symbolic links while it walks the tree");
    }

    match program {
        "git" => match arguments.first() {
            Some(_)
                if arguments
                    .iter()
                    .any(|a| GIT_ARGUMENT_PREFIXES_THAT_WRITE.iter().any(|p| a.starts_with(p))) =>
            {
                ask("this git call carries an option that writes a file")
            }
            Some(sub) if READ_ONLY_GIT_SUBCOMMANDS.contains(sub) => allow("a read-only git subcommand"),
            _ => ask("this git subcommand is not one of the read-only forms"),
        },
        "find"
            if arguments
                .iter()
                .any(|a| FIND_ACTIONS_THAT_ARE_NOT_READS.iter().any(|p| a.starts_with(p))) =>
        {
            ask("this find call carries an action that writes or executes")
        }
        "cd" => ask("cd moves the Bash cwd, and every other judgement here assumes it is the root"),
        other if READ_ONLY_BASH_COMMANDS.contains(&other) => {
            allow("a read-only command from the CLI's own list, with no shell syntax")
        }
        _ => ask("this command is not on the CLI's read-only list"),
    }
}

/// `-abc`, not `--long` and not a bare `-`.
fn is_short_option_cluster(argument: &str) -> bool {
    argument.len() > 1 && argument.starts_with('-') && !argument.starts_with("--")
}

/// Whether every argument that names something on disk under the root resolves inside it.
///
/// An argument that names nothing under the root -- a grep pattern, an `echo` word, a git revision
/// -- is not a path this command can read through, and passes. One that names an existing entry
/// (including a dangling symlink, hence `symlink_metadata` rather than `exists`) must
/// `canonicalize` inside the root; a link out of the tree, or one that will not resolve, fails.
///
/// A short-option cluster is checked at every suffix as well, because an option's value can be
/// glued to it (`grep -fescape`, where `escape` is a link out); `-la` costs two extra `lstat`s of
/// names that do not exist.
fn arguments_stay_inside(arguments: &[&str], root: &Path) -> bool {
    arguments.iter().all(|argument| {
        let candidates: Vec<&str> = if is_short_option_cluster(argument) {
            (1..argument.len()).filter_map(|i| argument.get(i..)).collect()
        } else {
            vec![argument]
        };
        candidates.into_iter().all(|candidate| {
            let joined = root.join(candidate);
            if joined.symlink_metadata().is_err() {
                return true;
            }
            matches!(joined.canonicalize(), Ok(resolved) if resolved.starts_with(root))
        })
    })
}

/// Whether `program`'s arguments ask it to follow symbolic links met while walking a tree.
///
/// Long options are matched as any prefix of the dangerous name longer than `--`, because GNU
/// `getopt_long` accepts an unambiguous abbreviation (`--deref`), and an ambiguous one gets a card
/// too. Short options are matched as a letter anywhere in a cluster, which also cards a glued value
/// that happens to contain the letter (`grep -eRx`) -- the right direction to be wrong in.
fn follows_links_while_walking(program: &str, arguments: &[&str]) -> bool {
    let (short, long): (Option<char>, &[&str]) = match program {
        "grep" => (Some('R'), &["--dereference-recursive"]),
        "ls" | "du" => (Some('L'), &["--dereference"]),
        // GNU diff -r follows links by default; `--no-dereference` exists but is not reproduced.
        "diff" => (Some('r'), &["--recursive"]),
        "find" => return arguments.iter().any(|a| FIND_OPTIONS_THAT_FOLLOW_LINKS.contains(a)),
        _ => return false,
    };
    arguments.iter().any(|a| {
        (is_short_option_cluster(a) && short.is_some_and(|c| a.contains(c)))
            || (a.len() > 2 && a.starts_with("--") && long.iter().any(|name| name.starts_with(a)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A real directory on disk, because every path branch here canonicalizes for real -- a
    /// fabricated root would make the "unresolvable path" and "outside the root" branches
    /// indistinguishable.
    struct Workspace {
        root: PathBuf,
    }

    impl Workspace {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("agent-permission-policy-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(root.join("src")).unwrap();
            std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
            Self { root }
        }
        fn path(&self) -> &Path {
            &self.root
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn verdict(tool: &str, input: Value, root: &Path) -> PermissionVerdict {
        classify_permission_request(tool, &input, root).verdict
    }

    fn bash(command: &str, root: &Path) -> PermissionVerdict {
        verdict("Bash", json!({ "command": command }), root)
    }

    // ---- name-decidable ------------------------------------------------------------------------

    #[test]
    fn the_tools_the_cli_always_asks_about_always_get_a_card() {
        let ws = Workspace::new();
        for tool in ALWAYS_ASK_TOOLS {
            assert_eq!(
                verdict(tool, json!({ "file_path": "src/main.rs" }), ws.path()),
                PermissionVerdict::AskTheUser,
                "{tool} must always reach the user"
            );
        }
    }

    /// Pinned separately from the loop above because it is a deliberate deviation from the CLI
    /// (rule (d)): the CLI exempts some documentation domains and this version does not reproduce
    /// the list. If someone later implements that exemption, this test is the thing that has to be
    /// consciously changed.
    #[test]
    fn webfetch_gets_a_card_even_for_a_documentation_domain() {
        let ws = Workspace::new();
        assert_eq!(
            verdict("WebFetch", json!({ "url": "https://docs.rs/serde" }), ws.path()),
            PermissionVerdict::AskTheUser
        );
    }

    #[test]
    fn checklist_and_subagent_tools_never_get_a_card() {
        let ws = Workspace::new();
        for tool in NEVER_ASK_TOOLS {
            assert_eq!(
                verdict(tool, json!({ "todos": [] }), ws.path()),
                PermissionVerdict::AllowWithoutAsking,
                "{tool} must not interrupt the user"
            );
        }
    }

    /// `ToolSearch` loads a tool's schema and does nothing else, and on this build the model must
    /// call it before it can use any tool at all -- so a card here is a card before nearly every
    /// action. Measured against the CLI (see `NEVER_ASK_TOOLS`), not reasoned about.
    #[test]
    fn a_tool_search_never_interrupts_the_user() {
        let ws = Workspace::new();
        assert_eq!(
            verdict(
                "ToolSearch",
                json!({ "query": "select:Read", "max_results": 1 }),
                ws.path()
            ),
            PermissionVerdict::AllowWithoutAsking
        );
    }

    /// The other side of that change, and the reason it is one name rather than a category: every
    /// remaining tool this CLI offers still reaches the user. Written as the real list off
    /// `system/init` rather than an invented name, because the names that matter are the ones a real
    /// session can actually produce -- five of these reach outside this machine or change the tree.
    #[test]
    fn every_other_tool_this_cli_offers_still_reaches_the_user() {
        let ws = Workspace::new();
        let decided_elsewhere = ["Task", "Bash", "Read", "ToolSearch"];
        let mut carded = 0;
        for tool in TOOLS_OFFERED_BY_CLI_2_1_272 {
            if decided_elsewhere.contains(tool) {
                continue;
            }
            assert_eq!(
                verdict(tool, json!({ "file_path": "src/main.rs" }), ws.path()),
                PermissionVerdict::AskTheUser,
                "{tool} must still reach the user"
            );
            carded += 1;
        }
        assert_eq!(
            carded, 22,
            "the CLI's tool list changed; re-read its system/init before editing this"
        );
    }

    #[test]
    fn an_unknown_tool_name_gets_a_card() {
        let ws = Workspace::new();
        assert_eq!(
            verdict("SomeToolInventedNextRelease", json!({}), ws.path()),
            PermissionVerdict::AskTheUser
        );
    }

    // ---- malformed input -----------------------------------------------------------------------

    #[test]
    fn input_that_is_not_an_object_gets_a_card_whatever_the_tool_is() {
        let ws = Workspace::new();
        // Including for a tool that would otherwise be auto-allowed by name: a call this module
        // cannot read is a call it must not judge.
        for tool in ["Read", "TodoWrite", "Bash", "Task"] {
            assert_eq!(
                verdict(tool, json!("not an object"), ws.path()),
                PermissionVerdict::AskTheUser,
                "{tool} with a non-object input"
            );
            assert_eq!(verdict(tool, json!(null), ws.path()), PermissionVerdict::AskTheUser);
            assert_eq!(
                verdict(tool, json!([1, 2, 3]), ws.path()),
                PermissionVerdict::AskTheUser
            );
        }
    }

    #[test]
    fn a_read_with_the_wrong_shape_gets_a_card() {
        let ws = Workspace::new();
        assert_eq!(verdict("Read", json!({}), ws.path()), PermissionVerdict::AskTheUser);
        assert_eq!(
            verdict("Read", json!({ "file_path": 7 }), ws.path()),
            PermissionVerdict::AskTheUser
        );
        assert_eq!(
            verdict("Read", json!({ "file_path": "" }), ws.path()),
            PermissionVerdict::AskTheUser
        );
    }

    #[test]
    fn a_bash_call_with_no_string_command_gets_a_card() {
        let ws = Workspace::new();
        assert_eq!(verdict("Bash", json!({}), ws.path()), PermissionVerdict::AskTheUser);
        assert_eq!(
            verdict("Bash", json!({ "command": ["ls"] }), ws.path()),
            PermissionVerdict::AskTheUser
        );
    }

    // ---- paths ---------------------------------------------------------------------------------

    #[test]
    fn a_read_inside_the_project_root_needs_no_human() {
        let ws = Workspace::new();
        for path in ["src/main.rs", "./src/main.rs"] {
            assert_eq!(
                verdict("Read", json!({ "file_path": path }), ws.path()),
                PermissionVerdict::AllowWithoutAsking,
                "{path}"
            );
        }
        let absolute = ws.path().join("src/main.rs");
        assert_eq!(
            verdict("Read", json!({ "file_path": absolute }), ws.path()),
            PermissionVerdict::AllowWithoutAsking
        );
    }

    #[test]
    fn a_read_outside_the_project_root_gets_a_card() {
        let ws = Workspace::new();
        assert_eq!(
            verdict("Read", json!({ "file_path": "/etc/hostname" }), ws.path()),
            PermissionVerdict::AskTheUser
        );
    }

    /// `..` must not escape, and the reason it does not is that `canonicalize` resolves it before
    /// the containment test. A textual `starts_with` on the unresolved string would pass this call.
    #[test]
    fn dot_dot_traversal_does_not_escape_the_root() {
        let ws = Workspace::new();
        assert_eq!(
            verdict(
                "Read",
                json!({ "file_path": "src/../../../../etc/hostname" }),
                ws.path()
            ),
            PermissionVerdict::AskTheUser
        );
        // And a `..` that stays inside is still fine -- this is a containment test, not a ban on
        // the characters.
        assert_eq!(
            verdict("Read", json!({ "file_path": "src/../src/main.rs" }), ws.path()),
            PermissionVerdict::AllowWithoutAsking
        );
    }

    #[test]
    fn a_path_that_does_not_resolve_gets_a_card() {
        let ws = Workspace::new();
        assert_eq!(
            verdict("Read", json!({ "file_path": "src/does-not-exist.rs" }), ws.path()),
            PermissionVerdict::AskTheUser
        );
    }

    #[test]
    fn a_project_root_that_does_not_resolve_gets_a_card() {
        let gone = std::env::temp_dir().join(format!("agent-policy-gone-{}", uuid::Uuid::new_v4()));
        assert_eq!(
            verdict("Read", json!({ "file_path": "anything" }), &gone),
            PermissionVerdict::AskTheUser
        );
    }

    /// A symlink out of the tree is the same defect as `..` wearing a different hat, and the same
    /// `canonicalize` catches it. Unix-only because that is how the link is made.
    #[cfg(unix)]
    #[test]
    fn a_symlink_pointing_out_of_the_root_gets_a_card() {
        let ws = Workspace::new();
        let link = ws.path().join("escape");
        std::os::unix::fs::symlink("/etc/hostname", &link).unwrap();
        assert_eq!(
            verdict("Read", json!({ "file_path": "escape" }), ws.path()),
            PermissionVerdict::AskTheUser
        );
    }

    #[test]
    fn a_grep_or_glob_with_no_path_reads_the_root_and_needs_no_human() {
        let ws = Workspace::new();
        for tool in ["Grep", "Glob"] {
            assert_eq!(
                verdict(tool, json!({ "pattern": "fn main" }), ws.path()),
                PermissionVerdict::AllowWithoutAsking,
                "{tool}"
            );
            assert_eq!(
                verdict(tool, json!({ "pattern": "fn main", "path": "src" }), ws.path()),
                PermissionVerdict::AllowWithoutAsking,
                "{tool} inside the root"
            );
            assert_eq!(
                verdict(tool, json!({ "pattern": "root:", "path": "/etc" }), ws.path()),
                PermissionVerdict::AskTheUser,
                "{tool} outside the root"
            );
        }
    }

    // ---- Bash ----------------------------------------------------------------------------------

    /// Every command on the CLI's own documented read-only list, bare, needs no human.
    #[test]
    fn each_documented_read_only_command_needs_no_human() {
        let ws = Workspace::new();
        // Except `cd`, which gets a card for a reason of this module's own -- see
        // `bare_cd_gets_a_card_because_it_moves_the_cwd_every_other_check_assumes`.
        for command in READ_ONLY_BASH_COMMANDS.iter().filter(|c| **c != "cd") {
            assert_eq!(
                bash(command, ws.path()),
                PermissionVerdict::AllowWithoutAsking,
                "bare `{command}`"
            );
        }
    }

    /// The three commands measured ALLOWED by the real CLI on 2.1.272, 2026-09-19. This is the
    /// half of that measurement that can be asserted without billing a turn; the other half is
    /// `agent/tests/permission_policy_conformance.rs`.
    #[test]
    fn the_commands_the_real_cli_allowed_need_no_human_here_either() {
        let ws = Workspace::new();
        for command in ["ls", "cat a.txt", "git status --short"] {
            assert_eq!(
                bash(command, ws.path()),
                PermissionVerdict::AllowWithoutAsking,
                "`{command}` was measured allowed by the real CLI"
            );
        }
    }

    /// The three commands measured DENIED by the real CLI in the same run. `echo hi > c.txt` is the
    /// one this whole design is built around: allowlisted first word, denied because of the
    /// redirect.
    #[test]
    fn the_commands_the_real_cli_denied_get_a_card_here() {
        let ws = Workspace::new();
        for command in ["echo hi > c.txt", "rm b.txt", "git commit --allow-empty -m probe"] {
            assert_eq!(
                bash(command, ws.path()),
                PermissionVerdict::AskTheUser,
                "`{command}` was measured denied by the real CLI"
            );
        }
    }

    #[test]
    fn every_shape_of_shell_syntax_gets_a_card() {
        let ws = Workspace::new();
        for command in [
            "echo hi > c.txt",  // redirect
            "echo hi >> c.txt", // appending redirect
            "cat < a.txt",      // input redirect
            "ls | tee out.txt", // pipeline
            "ls && rm -rf .",   // chain
            "ls; rm -rf .",     // sequence
            "echo $(rm -rf .)", // command substitution
            "echo `rm -rf .`",  // backtick substitution
            "ls *.rs",          // unquoted glob
            "cat 'a b.txt'",    // quoting
            "ls ~",             // home expansion
            "ls \\\n -l",       // escape and newline
            "ls & ",            // background
        ] {
            assert_eq!(bash(command, ws.path()), PermissionVerdict::AskTheUser, "`{command}`");
        }
    }

    #[test]
    fn a_command_over_the_length_threshold_gets_a_card() {
        let ws = Workspace::new();
        let long = format!("echo {}", "a".repeat(MAX_BASH_COMMAND_LEN));
        assert!(long.len() > MAX_BASH_COMMAND_LEN);
        assert_eq!(bash(&long, ws.path()), PermissionVerdict::AskTheUser);
        // And the same command just under it is still fine, so the test is about the threshold
        // rather than about the letter `a`.
        let short = format!("echo {}", "a".repeat(MAX_BASH_COMMAND_LEN - 100));
        assert_eq!(bash(&short, ws.path()), PermissionVerdict::AllowWithoutAsking);
    }

    #[test]
    fn a_command_that_is_not_on_the_read_only_list_gets_a_card() {
        let ws = Workspace::new();
        for command in [
            "rm b.txt",
            "mv a b",
            "curl http://example.com",
            "python script.py",
            "npm ci",
        ] {
            assert_eq!(bash(command, ws.path()), PermissionVerdict::AskTheUser, "`{command}`");
        }
    }

    #[test]
    fn a_bash_argument_reaching_outside_the_project_gets_a_card() {
        let ws = Workspace::new();
        for command in ["cat /etc/hostname", "ls ../..", "grep -r x ../other"] {
            assert_eq!(bash(command, ws.path()), PermissionVerdict::AskTheUser, "`{command}`");
        }
    }

    #[test]
    fn only_read_only_git_subcommands_need_no_human() {
        let ws = Workspace::new();
        for sub in READ_ONLY_GIT_SUBCOMMANDS {
            assert_eq!(
                bash(&format!("git {sub}"), ws.path()),
                PermissionVerdict::AllowWithoutAsking,
                "git {sub}"
            );
        }
        // Writing forms, and the ambiguous ones this module deliberately refuses to split.
        for sub in [
            "commit", "push", "checkout", "branch", "tag", "config", "stash", "reset",
        ] {
            assert_eq!(
                bash(&format!("git {sub}"), ws.path()),
                PermissionVerdict::AskTheUser,
                "git {sub}"
            );
        }
        assert_eq!(bash("git", ws.path()), PermissionVerdict::AskTheUser, "bare git");
    }

    /// `find` is on the CLI's read-only list and has actions that are not reads. The metacharacter
    /// rule already stops `find . -exec rm {} \;` (braces, backslash, semicolon); this catches the
    /// forms that carry no punctuation at all.
    #[test]
    fn a_find_that_writes_or_executes_gets_a_card() {
        let ws = Workspace::new();
        assert_eq!(
            bash("find . -name main.rs", ws.path()),
            PermissionVerdict::AllowWithoutAsking
        );
        for command in ["find . -delete", "find . -exec rm -rf . +", "find . -fprint out.txt"] {
            assert_eq!(bash(command, ws.path()), PermissionVerdict::AskTheUser, "`{command}`");
        }
    }

    // ---- the review of 2026-09-19 (later): every finding that the policy failed open ------------

    #[test]
    fn a_git_option_that_writes_a_file_gets_a_card_in_both_spellings() {
        let ws = Workspace::new();
        for command in [
            "git log --output=/home/someone/.bashrc", // glued, absolute
            "git show --output=src/main.rs",          // glued, inside the project
            "git log --output o3.txt",                // separate argument: no `=`, no `/`, still writes
            "git diff --output out.patch",
        ] {
            assert_eq!(bash(command, ws.path()), PermissionVerdict::AskTheUser, "`{command}`");
        }
        // And the ordinary read-only forms are untouched.
        for command in ["git log -5", "git log --oneline", "git show HEAD", "git diff --stat"] {
            assert_eq!(
                bash(command, ws.path()),
                PermissionVerdict::AllowWithoutAsking,
                "`{command}`"
            );
        }
    }

    #[test]
    fn a_path_glued_to_an_option_gets_a_card() {
        let ws = Workspace::new();
        for command in [
            "diff --from-file=/home/someone/.ssh/id_ed25519 README.md",
            "git blame --contents=/etc/hostname src/main.rs",
            "grep -f/etc/hostname src/main.rs",
            "diff --from-file=src/main.rs src/main.rs", // an `=` gets a card whatever follows it
        ] {
            assert_eq!(bash(command, ws.path()), PermissionVerdict::AskTheUser, "`{command}`");
        }
    }

    #[test]
    fn every_find_action_that_writes_gets_a_card_including_fprint0() {
        let ws = Workspace::new();
        for command in [
            "find . -maxdepth 0 -fprint0 src/main.rs",
            "find . -fprintf out.txt x",
            "find . -fls out.txt",
            "find . -execdir ls +",
            "find . -okdir ls +",
        ] {
            assert_eq!(bash(command, ws.path()), PermissionVerdict::AskTheUser, "`{command}`");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_bash_path_through_a_symlink_out_of_the_root_gets_a_card() {
        let ws = Workspace::new();
        let outside = std::env::temp_dir().join(format!("agent-policy-outside-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret"), "SECRET").unwrap();
        std::os::unix::fs::symlink(&outside, ws.path().join("escape")).unwrap();
        std::os::unix::fs::symlink("/nonexistent-target", ws.path().join("dangling")).unwrap();
        for command in [
            "cat escape/secret",
            "head escape/secret",
            "ls escape",
            "cat dangling",
            "grep -fescape src/main.rs", // a link glued to a short option
            "wc -l escape/secret",
        ] {
            assert_eq!(bash(command, ws.path()), PermissionVerdict::AskTheUser, "`{command}`");
        }
        // A path through a link that stays inside is still fine.
        std::os::unix::fs::symlink(ws.path().join("src"), ws.path().join("inner")).unwrap();
        assert_eq!(
            bash("cat inner/main.rs", ws.path()),
            PermissionVerdict::AllowWithoutAsking
        );
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn a_walk_that_follows_symlinks_gets_a_card() {
        let ws = Workspace::new();
        for command in [
            "grep -R token .",
            "grep -rR token .",
            "grep --dereference-recursive token .",
            "grep --deref token .",
            "find -L . -name secret",
            "find . -follow -name secret",
            "ls -RL",
            "du -L .",
            "du --dereference .",
            "diff -r src src",
            "diff -ur src src",
            "diff --recursive src src",
        ] {
            assert_eq!(bash(command, ws.path()), PermissionVerdict::AskTheUser, "`{command}`");
        }
        // The forms that do not follow links found mid-walk are unchanged.
        for command in [
            "grep -r token .",
            "grep -rn token src",
            "find . -name main.rs",
            "ls -R",
            "du -a .",
        ] {
            assert_eq!(
                bash(command, ws.path()),
                PermissionVerdict::AllowWithoutAsking,
                "`{command}`"
            );
        }
    }

    #[test]
    fn bare_cd_gets_a_card_because_it_moves_the_cwd_every_other_check_assumes() {
        let ws = Workspace::new();
        for command in ["cd", "cd src", "cd ."] {
            assert_eq!(bash(command, ws.path()), PermissionVerdict::AskTheUser, "`{command}`");
        }
    }

    #[test]
    fn a_glob_pattern_reaching_outside_gets_a_card_even_with_no_path() {
        let ws = Workspace::new();
        for pattern in ["/home/someone/.ssh/*", "../../**/*", "src/../../*", "~/.ssh/*"] {
            assert_eq!(
                verdict("Glob", json!({ "pattern": pattern }), ws.path()),
                PermissionVerdict::AskTheUser,
                "{pattern}"
            );
        }
        assert_eq!(
            verdict("Glob", json!({}), ws.path()),
            PermissionVerdict::AskTheUser,
            "no pattern"
        );
        assert_eq!(
            verdict("Glob", json!({ "pattern": "**/*.rs" }), ws.path()),
            PermissionVerdict::AllowWithoutAsking
        );
    }

    /// A menu launch can make the process cwd -- and so the project root -- the home directory.
    /// Then nothing under it may count as "inside the project".
    #[test]
    fn a_project_root_at_or_above_home_is_no_boundary() {
        let ws = Workspace::new();
        let classify = |tool: &str, input: Value, root: &Path, home: &Path| {
            classify_with_home(tool, &input, root, Some(home)).verdict
        };
        // The root IS home.
        assert_eq!(
            classify("Read", json!({ "file_path": "src/main.rs" }), ws.path(), ws.path()),
            PermissionVerdict::AskTheUser
        );
        assert_eq!(
            classify("Bash", json!({ "command": "ls" }), ws.path(), ws.path()),
            PermissionVerdict::AskTheUser
        );
        // The root is an ancestor of home.
        let home = ws.path().join("src");
        assert_eq!(
            classify("Grep", json!({ "pattern": "x" }), ws.path(), &home),
            PermissionVerdict::AskTheUser
        );
        // The filesystem root, whatever home is.
        assert_eq!(
            classify_with_home("Read", &json!({ "file_path": "etc/hostname" }), Path::new("/"), None).verdict,
            PermissionVerdict::AskTheUser
        );
        // A project BELOW home is still a boundary.
        assert_eq!(
            classify("Read", json!({ "file_path": "main.rs" }), &home, ws.path()),
            PermissionVerdict::AllowWithoutAsking
        );
        // And name-decidable tools do not depend on the root at all.
        assert_eq!(
            classify("TodoWrite", json!({ "todos": [] }), ws.path(), ws.path()),
            PermissionVerdict::AllowWithoutAsking
        );
    }
}
