//! Classifies tool calls that reach the permission gate to decide whether to ask the user.
//!
//! Both backends install a `PreToolUse` gate with a `*` matcher. The legacy backend generates
//! its hook in `crate::process`; the sidecar supplies its hook through `permissionBroker.ts`.
//! Every call must reach the gate, but that does not mean every call needs a GUI card.
//! Forwarding every call to a card would prompt even for reads and searches.
//!
//! This policy lives in `agent` because it uses the CLI's tool vocabulary, including `Read`'s
//! `file_path` and `Bash`'s `command`. The core crate depends on `agent`, so both crates can use it.
//! The real-CLI conformance tests live alongside it in `agent/tests/permission_policy_conformance.rs`.
//!
//! The baseline is interactive `default` mode with no configured permission rules.
//! With `--print` and no `--permission-mode` flag, the CLI uses the same classification but denies
//! calls that would need a prompt. This lets tests identify calls that need no human response.
//! On CLI 2.1.272, `Read`, `ls`, `cat a.txt` and `git status --short` were allowed.
//! `Write`, `echo hi > c.txt`, `rm b.txt` and `git commit` were denied.
//! The CLI's `--permission-mode auto` is separate: it auto-approves and uses a background classifier.
//!
//! Anything this module cannot classify with confidence requires a card. A mistaken allow can
//! grant unwanted access; an unnecessary card costs a click.
//! There is no shell parser. A `Bash` command with metacharacters that redirect, chain, glob,
//! substitute or quote requires a card. Matching only the first word would wrongly allow
//! `echo hi > c.txt` just because `echo` is on the CLI's read-only list.
//! `WebFetch` always requires a card because the CLI's preapproved documentation domain list
//! is unavailable here. Unknown tool names also require a card.
//! Malformed tool input requires a card, including a non-object input, a missing path or a
//! `command` value that is not a string.
//!
//! Keep this policy separate from `crate::CONSERVATIVE_DISALLOWED_TOOLS`.
//! That array forbids tools in Bypass mode and is mirrored as `CONSERVATIVE_BYPASS_DENY`.
//! This module classifies calls that reach a live permission gate.

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

/// Tools allowed without a permission card, including checklist updates, subagent
/// launches and tool discovery.
///
/// `Task` and `Agent` rely on subagent tool calls being checked individually. That has
/// not been verified here: no observation confirms that inner calls reach this same
/// `PreToolUse` gate. Remove these entries if that assumption fails.
///
/// CLI 2.1.283 calls the subagent tool `Agent`; CLI 2.1.272 calls it `Task`. Both names
/// are kept so either version can launch a subagent without an unknown-tool card.
///
/// On CLI 2.1.272, tools are deferred: the model receives names and calls `ToolSearch`
/// to load a schema before using a tool. Each search reaches the gate, so treating it
/// as unknown would prompt before nearly every action. Under `claude --print` without
/// `--permission-mode`, `ToolSearch {"query": "select:Read"}` returned the schema.
/// It loads a description without opening a file, running a command or using the network.
pub const NEVER_ASK_TOOLS: &[&str] = &["TodoWrite", "Task", "Agent", "ToolSearch"];

/// Tools offered by CLI 2.1.272, read from its `system/init` output.
///
/// The policy tables use a different tool set: `Grep`, `Glob`, `MultiEdit` and `TodoWrite`
/// are absent from this build, while eighteen offered tools were unknown to the policy.
/// Unknown tools require a card. Several, including `SendMessage`, `PushNotification`,
/// `RemoteTrigger`, `CronCreate` and `EnterWorktree`, reach outside the machine or change
/// the tree. Only `ToolSearch` has been measured for allowance without a card.
/// Check a tool against the running CLI before adding it to an allow list; presence
/// in this table alone says nothing about its permission behavior.
///
/// The full CLI 2.1.283 tool set has not been captured here. A permission log confirms
/// that its subagent tool is named `Agent`, but does not establish the rest of its tools.
/// Read that version's `system/init` before adding a corresponding table.
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

/// The two fixed reasons a prefix rule (`crate::permission_rules`) may replace, and nothing else.
/// Both are returned by `classify_bash` only after every syntax, path, symlink, link-following, `cd`,
/// `git --output` and `find` action check has already passed.
pub const REASON_NOT_ON_READ_ONLY_LIST: &str = "this command is not on the CLI's read-only list";
pub const REASON_GIT_SUBCOMMAND_NOT_READ_ONLY: &str = "this git subcommand is not one of the read-only forms";
pub const REPLACEABLE_BY_A_RULE: &[&str] = &[REASON_NOT_ON_READ_ONLY_LIST, REASON_GIT_SUBCOMMAND_NOT_READ_ONLY];
/// The allow a rule produces, logged by `take_ui_delivery` like every other auto-answer.
pub const REASON_ALLOWED_BY_A_PROJECT_RULE: &str = "a prefix rule the user approved for this project";

/// Characters that stop command analysis and send the call to a permission card.
///
/// A command containing shell syntax cannot be judged from its first word alone. Redirects, pipes,
/// chaining, substitution, globs, braces, quoting, escapes, home expansion, history expansion,
/// comments, and embedded newlines all qualify.
///
/// With zsh's `extendedglob`, `^` negates a glob: `cat ^s` can select every file except `s`,
/// including a symlink outside the tree. The Bash tool may run zsh, so `^` also forces a card.
/// This includes `git show HEAD^`, just as `~` does for `HEAD~1`.
const BASH_CHARS_THAT_FORCE_A_CARD: &[char] = &[
    '|', '&', ';', '<', '>', '$', '`', '(', ')', '{', '}', '[', ']', '*', '?', '~', '!', '#', '\\', '"', '\'', '\n',
    '\r', '^',
];

/// The only characters the shell splits a command into words at, once every character above has
/// already sent the command to a card. Anything else a Unicode-aware split would treat as a blank
/// -- a no-break space, a vertical tab, a form feed -- is part of a word to bash and zsh, so this
/// policy must not split there either (see [`classify_bash`]).
const SHELL_BLANKS: [char; 2] = [' ', '\t'];

/// `find` actions that write or execute need a permission card even though `find` itself is
/// on the CLI's read-only list.
///
/// Match prefixes so `-exec` covers `-execdir`, `-ok` covers `-okdir`, and `-fprint` covers
/// `-fprint0` and `-fprintf`. Like `-fprint`, `-fprint0` truncates and writes its argument.
/// This also catches future actions with these prefixes, at the cost of asking for permission
/// for any harmless predicate that happens to start the same way.
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

/// The card for an option this policy does not know to read only what the command names. Not in
/// [`REPLACEABLE_BY_A_RULE`]: it is a fail-closed check, like every other card but the two there.
const REASON_OPTION_NOT_KNOWN_TO_ONLY_READ: &str =
    "an option this policy does not know to read only what the command names (it may read another file)";

/// Environment variables that tell git where its repository, work tree, objects or index are, so
/// that the project root no longer decides it. The Bash tool's shell inherits this process's
/// environment, so if any is set here git is no boundary (see [`git_repository_stays_inside`]).
const GIT_LOCATION_VARIABLES: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_INDEX_FILE",
];

/// What, beyond the call and the project root, decides whether a path in the call can reach out.
/// Read from the process environment by [`classify_permission_request`], passed in by tests so
/// nothing has to mutate the environment of the whole test process.
#[derive(Debug, Clone, Copy)]
struct Surroundings<'a> {
    home: Option<&'a Path>,
    /// Any of [`GIT_LOCATION_VARIABLES`] is set.
    git_location_from_environment: bool,
}

/// The one entry point. `tool_name` and `input` are the `PermissionRequested` event's own fields,
/// verbatim; `project_root` is the session's canonical project directory
/// (`neovibe_core::project_root`), which is the boundary `Read`/`Grep`/`Glob` are judged against.
///
/// Total: every input produces a verdict, and every path that is not a positive, confident match
/// produces [`PermissionVerdict::AskTheUser`].
pub fn classify_permission_request(tool_name: &str, input: &Value, project_root: &Path) -> Classification {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let surroundings = Surroundings {
        home: home.as_deref(),
        git_location_from_environment: GIT_LOCATION_VARIABLES.iter().any(|v| std::env::var_os(v).is_some()),
    };
    classify_in(tool_name, input, project_root, &surroundings)
}

/// [`classify_permission_request`], then the project's prefix rules -- in that order, always. A rule
/// turns an ask into an allow only when the ask's reason is in [`REPLACEABLE_BY_A_RULE`]; every
/// other verdict, allow or ask, is returned unchanged.
pub fn classify_with_rules(
    tool_name: &str,
    input: &Value,
    project_root: &Path,
    rules: &crate::permission_rules::PrefixRules,
) -> Classification {
    if rule_that_allows(tool_name, input, project_root, rules).is_some() {
        return allow(REASON_ALLOWED_BY_A_PROJECT_RULE);
    }
    classify_permission_request(tool_name, input, project_root)
}

/// The rule [`classify_with_rules`] would answer this call with, in Claude Code's own syntax
/// (`Bash(git log *)`), or `None` when the call is not one a rule answers -- the same condition,
/// so the two can never disagree. What a transcript row names when a saved rule, not the user,
/// allowed a call.
pub fn rule_that_allows(
    tool_name: &str,
    input: &Value,
    project_root: &Path,
    rules: &crate::permission_rules::PrefixRules,
) -> Option<String> {
    let rule = rules.matching_rule(tool_name, input)?;
    let classification = classify_permission_request(tool_name, input, project_root);
    (classification.needs_a_human() && REPLACEABLE_BY_A_RULE.contains(&classification.reason))
        .then(|| rule.to_rule_string())
}

/// [`classify_permission_request`] with the home directory and the git environment passed in rather
/// than read from the environment, so the "the root is not a boundary" branches are testable without
/// mutating `HOME` or `GIT_DIR` for the whole test process.
fn classify_in(tool_name: &str, input: &Value, project_root: &Path, surroundings: &Surroundings) -> Classification {
    let home = surroundings.home;
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
        // Check Glob's `pattern` as well as its optional `path`. A missing `path` does not confine
        // an absolute pattern or one containing `..` to the project root. Ask for permission when the
        // pattern cannot be confined, without relying on how the CLI interprets it.
        "Glob" => match input.get("pattern") {
            Some(Value::String(pattern))
                if pattern.starts_with('/') || pattern.starts_with('~') || pattern.contains("..") =>
            {
                ask("the glob pattern itself reaches outside the project")
            }
            Some(Value::String(_)) => classify_path(input, "path", PathField::Optional, &root),
            _ => ask("this Glob call has no string `pattern`"),
        },
        _ => classify_bash(input, &root, surroundings),
    }
}

/// Returns the canonical project root, or asks for permission if it cannot serve as a boundary.
///
/// An unresolved root cannot bound any path. The home directory and its ancestors, including `/`,
/// are also rejected: treating home as the project would allow reads of credentials inside it.
/// When no directory is supplied, the project root falls back to the process working directory,
/// which may be home. This check does not depend on how the application was launched.
///
/// If `home` is absent or cannot be resolved, the filesystem root is still rejected.
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

/// `Bash` is allowed only for a plain sequence of words whose first word is on the CLI's
/// read-only list and whose paths stay inside the project.
///
/// This is not a shell parser. Shell syntax requires a card, including redirection such as
/// `echo hi > c.txt`, pipelines, command substitution and `&&` chains. Harmless quoted commands
/// may require cards too.
///
/// Absolute arguments and arguments containing `..` require a card. So do arguments carrying
/// `=` or short-option clusters carrying `/`, since options can hide paths such as
/// `--output=/abs/file` and `-f/abs`. Existing entries under the root are resolved and checked
/// by [`arguments_stay_inside`], including paths through symlinks.
///
/// Options that follow links while walking require a card: `grep -R`, `find -L`, `ls -L`, `du
/// -L` and `diff -r`. The plain recursive forms `grep -r`, `find`, `ls -R` and `du` do not
/// follow links found during the walk.
///
/// `cd` requires a card because relative paths are checked against the project root, while the
/// Bash tool retains a working directory across calls. A user-approved `cd` can still move that
/// directory. A relative link under the new directory that is absent under the root will then
/// escape this path check.
///
/// Options can read paths or patterns from files that argv does not name directly.
/// [`options_read_only_what_they_name`] allows only known options; unknown options and
/// abbreviations require cards. A directory argument to `diff` requires a card because it
/// follows links in shared entries even without `-r`.
///
/// Git can discover a repository above the working directory or run programs configured in
/// `.git`, so [`git_repository_stays_inside`] checks its repository separately.
///
/// Words split only at [`SHELL_BLANKS`]. Other whitespace and control characters require cards
/// so the policy and shell cannot disagree about which path a word names.
fn classify_bash(input: &Value, root: &Path, surroundings: &Surroundings) -> Classification {
    let Some(Value::String(command)) = input.get("command") else {
        return ask("this Bash call has no string `command`");
    };
    if command.len() > MAX_BASH_COMMAND_LEN {
        return ask("the command is longer than the CLI's own re-prompt threshold");
    }
    if command.contains(BASH_CHARS_THAT_FORCE_A_CARD) {
        return ask("the command contains shell syntax this policy refuses to parse");
    }
    if command
        .chars()
        .any(|c| !SHELL_BLANKS.contains(&c) && (c.is_whitespace() || c.is_control()))
    {
        return ask("the command contains a blank or control character the shell does not split words at");
    }

    let mut words = command.split(SHELL_BLANKS).filter(|w| !w.is_empty());
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
    if program == "find"
        && arguments
            .iter()
            .any(|a| FIND_ACTIONS_THAT_ARE_NOT_READS.iter().any(|p| a.starts_with(p)))
    {
        return ask("this find call carries an action that writes or executes");
    }
    if !options_read_only_what_they_name(program, &arguments) {
        return ask(REASON_OPTION_NOT_KNOWN_TO_ONLY_READ);
    }
    // `diff DIR DIR` and `diff FILE DIR` open what the directory's entries name, following a link
    // out of the tree, and with no `-r` at all (measured 2026-09-28). A `--no-dereference` would not
    // follow, but proving an argument list safe is exactly the partial parse this module refuses.
    if program == "diff" && arguments.iter().any(|a| root.join(a).is_dir()) {
        return ask("diff of a directory opens what its entries link to, inside the project or not");
    }

    match program {
        "git" => classify_git(&arguments, root, surroundings),
        "cd" => ask("cd moves the Bash cwd, and every other judgement here assumes it is the root"),
        other if READ_ONLY_BASH_COMMANDS.contains(&other) => {
            allow("a read-only command from the CLI's own list, with no shell syntax")
        }
        _ => ask(REASON_NOT_ON_READ_ONLY_LIST),
    }
}

/// `git`, once the command has passed every check `classify_bash` makes of any command.
///
/// The repository comes first and for every subcommand, the writing ones included: when the
/// project root is not the boundary of the repository git will use, no reading of the arguments
/// can make the call stay inside, and a saved rule for `git commit` does not change that either.
fn classify_git(arguments: &[&str], root: &Path, surroundings: &Surroundings) -> Classification {
    if let Err(reason) = git_repository_stays_inside(root, surroundings) {
        return ask(reason);
    }
    if arguments
        .iter()
        .any(|a| GIT_ARGUMENT_PREFIXES_THAT_WRITE.iter().any(|p| a.starts_with(p)))
    {
        return ask("this git call carries an option that writes a file");
    }
    match arguments.split_first() {
        Some((sub, rest)) if READ_ONLY_GIT_SUBCOMMANDS.contains(sub) => {
            if git_options_read_only_what_they_name(sub, rest) {
                allow("a read-only git subcommand")
            } else {
                ask(REASON_OPTION_NOT_KNOWN_TO_ONLY_READ)
            }
        }
        _ => ask(REASON_GIT_SUBCOMMAND_NOT_READ_ONLY),
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
        // GNU diff -r follows links by default; `--no-dereference` exists but is not reproduced. So
        // does a plain `diff DIR DIR`, one level deep -- `classify_bash` cards that separately.
        "diff" => (Some('r'), &["--recursive"]),
        "find" => return arguments.iter().any(|a| FIND_OPTIONS_THAT_FOLLOW_LINKS.contains(a)),
        _ => return false,
    };
    arguments.iter().any(|a| {
        (is_short_option_cluster(a) && short.is_some_and(|c| a.contains(c)))
            || (a.len() > 2 && a.starts_with("--") && long.iter().any(|name| name.starts_with(a)))
    })
}

/// The options of one command on the CLI's read-only list that read nothing but what the command
/// names, and follow no link while walking. Read off `--help` on this host, 2026-09-28: GNU
/// coreutils 9.11, grep 3.12, diffutils 3.12, findutils 4.11.0, GNU which 2.25, bash 5.3.
///
/// **An allowlist, deliberately, and in full names only.** GNU `getopt_long` takes any unambiguous
/// abbreviation of a long option -- `wc --f paths` is `wc --files0-from paths` (measured) -- so a
/// list of dangerous names would have to be matched by every prefix of each, and would still be
/// wrong the day a release adds an option. Against this list an abbreviation of a *safe* option
/// cards too, which costs a click; an exact name wins over an abbreviation in `getopt_long`, so a
/// listed name never means a longer unlisted one.
///
/// What each list leaves out, and why:
/// - `wc`, `du`: `--files0-from` (reads the paths to report on from a file); `du` also `-X`/
///   `--exclude-from` (patterns from a file) and `-L`/`--dereference`;
/// - `grep`: `-f`/`--file` (patterns from a file), `--exclude-from`, `-R`/`--dereference-recursive`;
/// - `diff`: `-X`/`--exclude-from`, `-r`/`--recursive`, and `--from-file`/`--to-file` and the
///   `--*-format` family, which are not reads of another file but are rare enough to card;
/// - `ls`: `-L`/`--dereference`;
/// - `which`: `-i`/`--read-alias`/`--read-functions`, which read the shell's stdin;
/// - `cat`, `head`, `tail`, `stat`, `pwd`: nothing -- none of them has such an option; the lists
///   exist so an option this policy has never seen still cards.
///
/// A short option cluster may carry digits anywhere (`-5`, `-A3`, `-n10`), which are counts, never
/// options. Any other character must be one of the command's own letters, so a value glued to an
/// option (`grep -efoo`) cards unless every letter of it happens to be an option -- the right
/// direction to be wrong in. `echo` has no table: the shell builtin reads nothing, and every word is
/// text to it. `find` has its own ([`find_argument_only_reads`]); `git` its own
/// ([`git_options_read_only_what_they_name`]).
struct ReadOnlyOptions {
    short: &'static str,
    long: &'static [&'static str],
}

fn read_only_options(program: &str) -> Option<ReadOnlyOptions> {
    let options = match program {
        "ls" => ReadOnlyOptions {
            short: "aAbBcCdDfFgGhHiIklmnNopqQrRsStTuUvwxXZ1",
            long: &[
                "--all",
                "--almost-all",
                "--author",
                "--escape",
                "--block-size",
                "--ignore-backups",
                "--color",
                "--directory",
                "--dired",
                "--classify",
                "--file-type",
                "--format",
                "--full-time",
                "--group-directories-first",
                "--no-group",
                "--human-readable",
                "--si",
                "--dereference-command-line",
                "--dereference-command-line-symlink-to-dir",
                "--hide",
                "--hyperlink",
                "--indicator-style",
                "--inode",
                "--ignore",
                "--kibibytes",
                "--numeric-uid-gid",
                "--literal",
                "--hide-control-chars",
                "--show-control-chars",
                "--quote-name",
                "--quoting-style",
                "--reverse",
                "--recursive",
                "--size",
                "--sort",
                "--time",
                "--time-style",
                "--tabsize",
                "--width",
                "--context",
                "--zero",
                "--help",
                "--version",
            ],
        },
        "cat" => ReadOnlyOptions {
            short: "AbeEnstTuv",
            long: &[
                "--show-all",
                "--number-nonblank",
                "--show-ends",
                "--number",
                "--squeeze-blank",
                "--show-tabs",
                "--show-nonprinting",
                "--help",
                "--version",
            ],
        },
        "head" => ReadOnlyOptions {
            short: "cnqvz",
            long: &[
                "--bytes",
                "--lines",
                "--quiet",
                "--silent",
                "--verbose",
                "--zero-terminated",
                "--help",
                "--version",
            ],
        },
        "tail" => ReadOnlyOptions {
            short: "cfFnqsvz",
            long: &[
                "--bytes",
                "--debug",
                "--follow",
                "--lines",
                "--max-unchanged-stats",
                "--pid",
                "--quiet",
                "--silent",
                "--retry",
                "--sleep-interval",
                "--verbose",
                "--zero-terminated",
                "--help",
                "--version",
            ],
        },
        "grep" => ReadOnlyOptions {
            short: "EFGPeiywxzsvVmbnHhoqaIdDrLlcTZBACU",
            long: &[
                "--extended-regexp",
                "--fixed-strings",
                "--basic-regexp",
                "--perl-regexp",
                "--regexp",
                "--ignore-case",
                "--no-ignore-case",
                "--word-regexp",
                "--line-regexp",
                "--null-data",
                "--no-messages",
                "--invert-match",
                "--version",
                "--help",
                "--max-count",
                "--byte-offset",
                "--line-number",
                "--line-buffered",
                "--with-filename",
                "--no-filename",
                "--label",
                "--only-matching",
                "--quiet",
                "--silent",
                "--binary-files",
                "--text",
                "--directories",
                "--devices",
                "--recursive",
                "--include",
                "--exclude",
                "--exclude-dir",
                "--files-without-match",
                "--files-with-matches",
                "--count",
                "--initial-tab",
                "--null",
                "--before-context",
                "--after-context",
                "--context",
                "--group-separator",
                "--no-group-separator",
                "--color",
                "--colour",
                "--binary",
            ],
        },
        "wc" => ReadOnlyOptions {
            short: "cmlLw",
            long: &[
                "--bytes",
                "--chars",
                "--lines",
                "--debug",
                "--max-line-length",
                "--words",
                "--total",
                "--help",
                "--version",
            ],
        },
        "which" => ReadOnlyOptions {
            short: "a",
            long: &[
                "--all",
                "--skip-dot",
                "--skip-tilde",
                "--show-dot",
                "--show-tilde",
                "--tty-only",
                "--skip-alias",
                "--skip-functions",
                "--help",
                "--version",
            ],
        },
        "diff" => ReadOnlyOptions {
            short: "qscCuUenyWpFtTlNxSiEZbwBIaDdv",
            long: &[
                "--normal",
                "--brief",
                "--report-identical-files",
                "--context",
                "--unified",
                "--ed",
                "--rcs",
                "--side-by-side",
                "--width",
                "--left-column",
                "--suppress-common-lines",
                "--show-c-function",
                "--show-function-line",
                "--label",
                "--expand-tabs",
                "--initial-tab",
                "--tabsize",
                "--suppress-blank-empty",
                "--paginate",
                "--no-dereference",
                "--new-file",
                "--unidirectional-new-file",
                "--ignore-file-name-case",
                "--no-ignore-file-name-case",
                "--exclude",
                "--starting-file",
                "--ignore-case",
                "--ignore-tab-expansion",
                "--ignore-trailing-space",
                "--ignore-space-change",
                "--ignore-all-space",
                "--ignore-blank-lines",
                "--ignore-matching-lines",
                "--text",
                "--strip-trailing-cr",
                "--ifdef",
                "--minimal",
                "--horizon-lines",
                "--speed-large-files",
                "--color",
                "--palette",
                "--help",
                "--version",
            ],
        },
        "stat" => ReadOnlyOptions {
            short: "Lfct",
            long: &[
                "--dereference",
                "--file-system",
                "--cached",
                "--format",
                "--printf",
                "--terse",
                "--help",
                "--version",
            ],
        },
        "du" => ReadOnlyOptions {
            short: "0aAbBcDdHhklmPSstx",
            long: &[
                "--null",
                "--all",
                "--apparent-size",
                "--block-size",
                "--bytes",
                "--total",
                "--dereference-args",
                "--max-depth",
                "--human-readable",
                "--inodes",
                "--count-links",
                "--no-dereference",
                "--separate-dirs",
                "--si",
                "--summarize",
                "--threshold",
                "--time",
                "--time-style",
                "--exclude",
                "--one-file-system",
                "--help",
                "--version",
            ],
        },
        "pwd" => ReadOnlyOptions { short: "LP", long: &[] },
        _ => return None,
    };
    Some(options)
}

/// Whether every option in `arguments` is one `program` is known to read only what it names with.
/// `true` for a program with no table -- `echo`, `cd`, `git`, anything off the list -- whose own
/// branch decides it.
fn options_read_only_what_they_name(program: &str, arguments: &[&str]) -> bool {
    if program == "find" {
        return arguments.iter().all(|a| find_argument_only_reads(a));
    }
    let Some(known) = read_only_options(program) else {
        return true;
    };
    arguments.iter().all(|a| option_is_known(a, known.short, known.long))
}

/// One argument against a command's lists: an operand (anything not starting with `-`, and a bare
/// `-` or `--`) passes; a long option must be listed in full; a short cluster may hold only listed
/// letters and digits. Options after `--` are judged as options all the same -- a stricter reading
/// of an operand that looks like one, never a looser one.
fn option_is_known(argument: &str, short: &str, long: &[&str]) -> bool {
    if argument == "--" || !argument.starts_with('-') || argument == "-" {
        return true;
    }
    if argument.starts_with("--") {
        return long.contains(&argument);
    }
    argument[1..].chars().all(|c| c.is_ascii_digit() || short.contains(c))
}

/// `find`'s words that start with `-` and read only what they name. Its predicates are words, not
/// getopt options, and it takes no abbreviation (`find -files0 x` is "unknown predicate", measured),
/// so the match is exact. Left out: `-files0-from` (the starting points come from a file; CLI 2.1.283
/// ran it without a refusal on 2026-09-28, so this is stricter than the CLI), `-L`/`-follow`, `-D`,
/// and every action `FIND_ACTIONS_THAT_ARE_NOT_READS` names.
const FIND_WORDS_THAT_ONLY_READ: &[&str] = &[
    "-H",
    "-P",
    "-O0",
    "-O1",
    "-O2",
    "-O3",
    "-daystart",
    "-nowarn",
    "-warn",
    "-regextype",
    "-depth",
    "-maxdepth",
    "-mindepth",
    "-mount",
    "-xdev",
    "-noleaf",
    "-ignore_readdir_race",
    "-noignore_readdir_race",
    "-amin",
    "-anewer",
    "-atime",
    "-cmin",
    "-cnewer",
    "-context",
    "-ctime",
    "-empty",
    "-false",
    "-fstype",
    "-gid",
    "-group",
    "-ilname",
    "-iname",
    "-inum",
    "-ipath",
    "-iregex",
    "-iwholename",
    "-links",
    "-lname",
    "-mmin",
    "-mtime",
    "-name",
    "-newer",
    "-nouser",
    "-nogroup",
    "-path",
    "-perm",
    "-regex",
    "-readable",
    "-writable",
    "-executable",
    "-samefile",
    "-size",
    "-true",
    "-type",
    "-uid",
    "-used",
    "-user",
    "-wholename",
    "-xtype",
    "-print",
    "-print0",
    "-printf",
    "-ls",
    "-prune",
    "-quit",
    "-not",
    "-a",
    "-and",
    "-o",
    "-or",
    "--help",
    "--version",
];

/// One `find` argument: an operand (a starting point, or a predicate's value) passes, as does a
/// listed predicate, `-newerXY`, or a signed count (`-mtime -7`, `-size -10k`, `-perm -644`).
fn find_argument_only_reads(argument: &str) -> bool {
    if !argument.starts_with('-') || argument == "-" {
        return true;
    }
    if FIND_WORDS_THAT_ONLY_READ.contains(&argument) {
        return true;
    }
    let newer_xy = argument.len() == 8
        && argument.starts_with("-newer")
        && argument.as_bytes()[6].is_ascii()
        && "aBcm".contains(argument.as_bytes()[6] as char)
        && "aBcmt".contains(argument.as_bytes()[7] as char);
    let count = argument[1..]
        .strip_suffix(['b', 'c', 'w', 'k', 'M', 'G'])
        .unwrap_or(&argument[1..]);
    newer_xy || (!count.is_empty() && count.bytes().all(|b| b.is_ascii_digit()))
}

/// Long options of the read-only git subcommands that read nothing but the repository git uses and
/// the paths the call names. One list for all nine: a name means the same thing in each that has
/// it. From `git help <subcommand>` on git 2.55.0, 2026-09-28.
///
/// Left out, each because it reads another file or reaches past the repository: `--contents` and
/// `--ignore-revs-file` (blame), `--exclude-from` and `--exclude-per-directory` (ls-files),
/// `--no-index` (diff two paths outside git), `--stdin` (revisions from stdin), `--ext-diff` and
/// `--textconv` (run a configured program -- the repository's own config is checked separately, but
/// the user's is not), `--recurse-submodules`, `--resolve-git-dir`, `--git-path`, `--output*` (writes;
/// its own check above), and `--help` (which starts a pager or a browser). **git's parse-options
/// takes an unambiguous abbreviation** (`git blame --cont <file>` read `<file>`, measured), which is
/// why this is a list of full names and not of dangerous ones.
const GIT_READ_ONLY_LONG_OPTIONS: &[&str] = &[
    // log / show / diff: revision walking and output
    "--oneline",
    "--graph",
    "--all",
    "--decorate",
    "--no-decorate",
    "--clear-decorations",
    "--stat",
    "--shortstat",
    "--numstat",
    "--dirstat",
    "--summary",
    "--compact-summary",
    "--name-only",
    "--name-status",
    "--patch",
    "--no-patch",
    "--raw",
    "--patch-with-stat",
    "--patch-with-raw",
    "--abbrev-commit",
    "--no-abbrev-commit",
    "--abbrev",
    "--no-abbrev",
    "--full-index",
    "--binary",
    "--follow",
    "--reverse",
    "--no-merges",
    "--merges",
    "--first-parent",
    "--topo-order",
    "--date-order",
    "--author-date-order",
    "--left-right",
    "--cherry-pick",
    "--cherry-mark",
    "--cherry",
    "--boundary",
    "--simplify-by-decoration",
    "--source",
    "--parents",
    "--children",
    "--full-history",
    "--ancestry-path",
    "--dense",
    "--sparse",
    "--simplify-merges",
    "--show-pulls",
    "--branches",
    "--tags",
    "--remotes",
    "--no-walk",
    "--do-walk",
    "--relative-date",
    "--date",
    "--pretty",
    "--notes",
    "--no-notes",
    "--show-signature",
    "--no-show-signature",
    "--mailmap",
    "--no-mailmap",
    "--use-mailmap",
    "--log-size",
    "--walk-reflogs",
    "--grep",
    "--author",
    "--committer",
    "--since",
    "--until",
    "--after",
    "--before",
    "--max-count",
    "--skip",
    "--min-parents",
    "--max-parents",
    "--no-min-parents",
    "--no-max-parents",
    "--all-match",
    "--invert-grep",
    "--regexp-ignore-case",
    "--basic-regexp",
    "--extended-regexp",
    "--fixed-strings",
    "--perl-regexp",
    "--remove-empty",
    "--not",
    "--exclude",
    "--glob",
    "--reflog",
    "--single-worktree",
    "--ignore-missing",
    "--in-commit-order",
    "--show-linear-break",
    "--decorate-refs",
    "--decorate-refs-exclude",
    // diff output
    "--cached",
    "--staged",
    "--merge-base",
    "--word-diff",
    "--word-diff-regex",
    "--color-words",
    "--color",
    "--no-color",
    "--color-moved",
    "--no-color-moved",
    "--unified",
    "--function-context",
    "--ignore-space-change",
    "--ignore-all-space",
    "--ignore-space-at-eol",
    "--ignore-cr-at-eol",
    "--ignore-blank-lines",
    "--ignore-matching-lines",
    "--minimal",
    "--patience",
    "--histogram",
    "--anchored",
    "--diff-algorithm",
    "--indent-heuristic",
    "--no-indent-heuristic",
    "--find-renames",
    "--find-copies",
    "--find-copies-harder",
    "--no-renames",
    "--rename-empty",
    "--no-rename-empty",
    "--irreversible-delete",
    "--break-rewrites",
    "--diff-filter",
    "--pickaxe-all",
    "--pickaxe-regex",
    "--check",
    "--ws-error-highlight",
    "--full-diff",
    "--no-ext-diff",
    "--no-textconv",
    "--ignore-submodules",
    "--submodule",
    "--src-prefix",
    "--dst-prefix",
    "--no-prefix",
    "--default-prefix",
    "--line-prefix",
    "--inter-hunk-context",
    "--text",
    "--exit-code",
    "--quiet",
    "--relative",
    "--no-relative",
    "--ita-invisible-in-index",
    "--ita-visible-in-index",
    "--diff-merges",
    "--no-diff-merges",
    "--combined-all-paths",
    "--cc",
    "--remerge-diff",
    "--dd",
    "--expand-tabs",
    "--no-expand-tabs",
    // status
    "--short",
    "--branch",
    "--porcelain",
    "--long",
    "--verbose",
    "--untracked-files",
    "--ignored",
    "--show-stash",
    "--ahead-behind",
    "--no-ahead-behind",
    "--renames",
    "--column",
    "--no-column",
    "--null",
    // ls-files
    "--deleted",
    "--modified",
    "--others",
    "--stage",
    "--unmerged",
    "--killed",
    "--directory",
    "--no-empty-directory",
    "--exclude-standard",
    "--full-name",
    "--error-unmatch",
    "--with-tree",
    "--eol",
    "--deduplicate",
    "--debug",
    // blame
    "--root",
    "--show-stats",
    "--show-name",
    "--show-number",
    "--show-email",
    "--line-porcelain",
    "--incremental",
    "--score-debug",
    "--color-lines",
    "--color-by-age",
    "--ignore-rev",
    // shortlog
    "--numbered",
    "--email",
    "--group",
    // describe
    "--always",
    "--contains",
    "--dirty",
    "--broken",
    "--exact-match",
    "--match",
    "--candidates",
    // rev-parse
    "--show-toplevel",
    "--git-dir",
    "--absolute-git-dir",
    "--git-common-dir",
    "--abbrev-ref",
    "--verify",
    "--is-inside-work-tree",
    "--is-inside-git-dir",
    "--is-bare-repository",
    "--is-shallow-repository",
    "--show-prefix",
    "--show-cdup",
    "--show-superproject-working-tree",
    "--symbolic",
    "--symbolic-full-name",
    "--show-object-format",
    "--show-ref-format",
    "--revs-only",
    "--no-revs",
    "--flags",
    "--no-flags",
    "--default",
    "--sq",
    "--local-env-vars",
    "--path-format",
];

/// The short options of each read-only git subcommand, from the same source. Left out: blame's
/// `-S <revs-file>`, the diff family's `-O<orderfile>`, and ls-files' `-X <file>` -- each reads
/// another file. status's letters include those of `-u`'s own values (`-uno`, `-uall`,
/// `-unormal`), which git takes glued.
fn git_short_options(subcommand: &str) -> &'static str {
    match subcommand {
        "log" | "show" | "diff" => "pusSGzMCBDlRabwWXItrcmnLgiEFPq",
        "status" => "sbuzvnoalrm",
        "ls-files" => "cdmoiskutzvfx",
        "blame" => "bLltpesfncwMC",
        "shortlog" => "nsewc",
        "rev-parse" => "q",
        _ => "",
    }
}

fn git_options_read_only_what_they_name(subcommand: &str, arguments: &[&str]) -> bool {
    let short = git_short_options(subcommand);
    arguments
        .iter()
        .all(|a| option_is_known(a, short, GIT_READ_ONLY_LONG_OPTIONS))
}

const REASON_GIT_LOCATION_FROM_ENVIRONMENT: &str =
    "this environment tells git where its repository is (GIT_DIR or a sibling), so the project root does not";
const REASON_GIT_REPOSITORY_ABOVE_ROOT: &str =
    "the project is inside a larger git repository, which git reads beyond the project root";
const REASON_ROOT_IS_A_GIT_DIRECTORY: &str = "the project root is itself a git directory";
const REASON_DOT_GIT_NOT_A_REPOSITORY: &str =
    "the project's .git is not a repository git would stop at, so git may use one above it";
const REASON_GIT_CONFIG_RUNS_A_PROGRAM: &str =
    "the repository's own configuration can make git run a program or use another work tree";
const REASON_GIT_CONFIG_UNREADABLE: &str = "the repository's configuration could not be read the way git reads it";
const REASON_GIT_HOOK: &str = "the repository holds a hook git may run under a read-only command";
const REASON_GIT_ALTERNATES: &str = "the repository reads objects from another repository (objects/info/alternates)";
const REASON_GIT_TOO_MUCH_TO_CHECK: &str = "the repository has too many submodule git directories to check";

/// Checks that the repository git discovers from `root` stays within that boundary
/// and runs nothing of its own under a read. Discovery walks `.git` upward without
/// running git: even `git rev-parse` could invoke the repository's `core.fsmonitor`.
/// A repository above the root cards because `git show` and `git log -p` can expose
/// committed files outside the project. With the toplevel at the root, revision paths
/// and pathspecs are relative to it; `..` and absolute escape paths already card.
/// Configuration, hooks and object stores that reach outside also card through
/// [`repository_runs_nothing_of_its_own`]. Unreadable or invalid repository metadata
/// and submodule trees too large to examine card even if git would simply fail.
/// This does not discover submodule git directories outside `.git/modules/`, which
/// would require reading gitlinks from the index. It also cannot see `GIT_DIR` exported
/// only by the Bash tool's shell startup files.
fn git_repository_stays_inside(root: &Path, surroundings: &Surroundings) -> Result<(), &'static str> {
    if surroundings.git_location_from_environment {
        return Err(REASON_GIT_LOCATION_FROM_ENVIRONMENT);
    }
    let dot_git = root.join(".git");
    if dot_git.symlink_metadata().is_err() {
        if looks_like_a_git_directory(root) {
            return Err(REASON_ROOT_IS_A_GIT_DIRECTORY);
        }
        // Every ancestor up to `/`, ignoring the filesystem boundaries and ceiling directories that
        // stop git's own walk: a superset of what git would find, so never an answer git disagrees
        // with in the permissive direction. No repository anywhere is fine -- git then fails, or
        // (`git diff A B`) compares two paths, whose links it shows and never follows (measured).
        let above = root
            .ancestors()
            .skip(1)
            .any(|dir| dir.join(".git").symlink_metadata().is_ok() || looks_like_a_git_directory(dir));
        return if above {
            Err(REASON_GIT_REPOSITORY_ABOVE_ROOT)
        } else {
            Ok(())
        };
    }
    let git_dir = git_dir_named_by(&dot_git, root).ok_or(REASON_DOT_GIT_NOT_A_REPOSITORY)?;
    let common = git_directory_git_accepts(&git_dir).ok_or(REASON_DOT_GIT_NOT_A_REPOSITORY)?;
    repository_runs_nothing_of_its_own(&git_dir, &common, surroundings.home)
}

/// A bare repository, or a directory inside one: git's `is_git_directory` asks for a `HEAD` and
/// `objects/` and `refs/` (or a `commondir` naming where they are). Any `HEAD` with any of the
/// three counts here, which is looser than git and so cards more, never less.
fn looks_like_a_git_directory(dir: &Path) -> bool {
    dir.join("HEAD").symlink_metadata().is_ok()
        && ["objects", "refs", "commondir"]
            .iter()
            .any(|name| dir.join(name).symlink_metadata().is_ok())
}

/// The git directory a `.git` names: itself when it is a directory, or the `gitdir: <path>` a
/// gitfile holds (a linked worktree, a submodule), relative to the directory holding the `.git`.
/// `None` for anything git would reject, which makes git die rather than read anything.
fn git_dir_named_by(dot_git: &Path, root: &Path) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let meta = dot_git.metadata().ok()?;
    if meta.is_dir() {
        return Some(dot_git.to_path_buf());
    }
    if !meta.is_file() {
        return None;
    }
    let bytes = read_small_file(dot_git).ok()?;
    let named = trim_line_ends(bytes.strip_prefix(b"gitdir: ")?);
    if named.is_empty() {
        return None;
    }
    let path = Path::new(std::ffi::OsStr::from_bytes(named));
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    })
}

/// `git_dir`'s common directory, if git's `is_git_directory` would accept it -- and only then,
/// because git keeps walking up past a `.git` it does not accept (measured 2026-09-28: an empty
/// `.git`, a `HEAD` alone, `objects/` at mode 000 and a garbage `HEAD` each sent `git rev-parse
/// --show-toplevel` to the repository above). At least as strict as git: a `HEAD` naming `refs/`
/// (as `ref: refs/...` or a symlink) or a full object id, and `objects/` and `refs/` directories this
/// process may search, in the directory `commondir` names if there is one.
fn git_directory_git_accepts(git_dir: &Path) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    if !head_names_a_ref_or_an_object(&git_dir.join("HEAD")) {
        return None;
    }
    let common = match read_small_file(&git_dir.join("commondir")) {
        Ok(bytes) => {
            let named = trim_line_ends(&bytes);
            if named.is_empty() {
                return None;
            }
            let path = Path::new(std::ffi::OsStr::from_bytes(named));
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                git_dir.join(path)
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => git_dir.to_path_buf(),
        Err(_) => return None,
    };
    ["objects", "refs"]
        .iter()
        .all(|name| is_searchable_directory(&common.join(name)))
        .then_some(common)
}

fn head_names_a_ref_or_an_object(head: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(meta) = head.symlink_metadata() else {
        return false;
    };
    if meta.file_type().is_symlink() {
        return std::fs::read_link(head).is_ok_and(|target| target.as_os_str().as_bytes().starts_with(b"refs/"));
    }
    if !meta.is_file() {
        return false;
    }
    let Ok(bytes) = read_small_file(head) else {
        return false;
    };
    let line = trim_line_ends(&bytes);
    if let Some(rest) = line.strip_prefix(b"ref:") {
        let start = rest
            .iter()
            .position(|b| *b != b' ' && *b != b'\t')
            .unwrap_or(rest.len());
        return rest[start..].starts_with(b"refs/");
    }
    matches!(line.len(), 40 | 64) && line.iter().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
}

/// A directory git's `access(path, X_OK)` would accept.
fn is_searchable_directory(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    if !path.is_dir() {
        return false;
    }
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `c_path` is a valid NUL-terminated string that outlives the call, and `access` only
    // reads it.
    unsafe { libc::access(c_path.as_ptr(), libc::X_OK) == 0 }
}

/// Git's config files, gitfiles and `HEAD` are small; one past this is a card, not a long read on
/// the thread that pumps permission requests.
const MAX_GIT_FILE_BYTES: u64 = 1 << 20;

fn read_small_file(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_GIT_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_GIT_FILE_BYTES {
        return Err(std::io::Error::other("larger than a git file this policy reads"));
    }
    Ok(bytes)
}

fn trim_line_ends(bytes: &[u8]) -> &[u8] {
    let end = bytes
        .iter()
        .rposition(|b| *b != b'\n' && *b != b'\r')
        .map_or(0, |i| i + 1);
    &bytes[..end]
}

/// A read-only git command can execute programs or read outside the project because
/// of repository metadata, even when its arguments contain no unsafe paths.
/// `diff.external`, `core.fsmonitor` and `post-index-change` hooks can run programs;
/// `core.worktree` can redirect file reads. [`GitConfigVariable::can_run_a_program`]
/// checks configuration keys, following includes, and [`hooks_run_nothing_under_a_read`]
/// cards hooks not known to run only under writing commands.
/// Submodule git directories under `modules/` need the same config and hook checks,
/// since the superproject's `git status` and `git diff` use them.
/// `objects/info/alternates` can expose objects from another repository and cards too.
///
/// The model cannot write any of this without a card. What can is a directory or archive that
/// arrives with its own `.git` (`git clone` carries none of it), and there the user's own
/// git-aware editor plugins would run the same programs. This is stricter than the CLI, which
/// hardens its own internal git calls but not the Bash tool's.
fn repository_runs_nothing_of_its_own(git_dir: &Path, common: &Path, home: Option<&Path>) -> Result<(), &'static str> {
    // `common/config` is what git reads; `git_dir/config` and `config.worktree` are read whether or
    // not `extensions.worktreeConfig` asks for the latter -- a superset.
    let mut config_files = vec![common.join("config")];
    if git_dir != common {
        config_files.push(git_dir.join("config"));
    }
    config_files.push(git_dir.join("config.worktree"));
    for file in &config_files {
        config_runs_nothing(file, home, 0)?;
    }
    if common.join("objects/info/alternates").symlink_metadata().is_ok() {
        return Err(REASON_GIT_ALTERNATES);
    }
    hooks_run_nothing_under_a_read(&common.join("hooks"))?;
    let mut budget = MAX_SUBMODULE_GIT_DIRECTORIES;
    submodule_git_directories_run_nothing(&common.join("modules"), home, &mut budget)
}

/// Git's own limit (`MAX_INCLUDE_DEPTH`); past it git dies, so here it cards.
const MAX_CONFIG_INCLUDE_DEPTH: usize = 10;

/// A missing file is fine, as it is to git (a repository need not have a config, and a missing
/// `include.path` is silently skipped). Anything else git could not read or parse is a card.
fn config_runs_nothing(path: &Path, home: Option<&Path>, depth: usize) -> Result<(), &'static str> {
    if depth > MAX_CONFIG_INCLUDE_DEPTH {
        return Err(REASON_GIT_CONFIG_UNREADABLE);
    }
    let bytes = match read_small_file(path) {
        Ok(bytes) => bytes,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(())
        }
        Err(_) => return Err(REASON_GIT_CONFIG_UNREADABLE),
    };
    let variables = parse_git_config(&bytes).ok_or(REASON_GIT_CONFIG_UNREADABLE)?;
    for variable in &variables {
        if variable.can_run_a_program() {
            return Err(REASON_GIT_CONFIG_RUNS_A_PROGRAM);
        }
        if variable.is_an_include() {
            let target = variable
                .value
                .as_deref()
                .and_then(|value| include_target(value, path, home))
                .ok_or(REASON_GIT_CONFIG_UNREADABLE)?;
            config_runs_nothing(&target, home, depth + 1)?;
        }
    }
    Ok(())
}

/// Where an `include.path` value points, as git's `interpolate_path` resolves it: `~/` is the home
/// directory, a relative path is relative to the including file's directory. `~user/` and
/// `%(prefix)/` are not resolved here and card.
fn include_target(value: &[u8], including_file: &Path, home: Option<&Path>) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    if value.is_empty() || value.starts_with(b"%(") {
        return None;
    }
    if let Some(rest) = value.strip_prefix(b"~/") {
        return Some(home?.join(std::ffi::OsStr::from_bytes(rest)));
    }
    if value.starts_with(b"~") {
        return None;
    }
    let path = Path::new(std::ffi::OsStr::from_bytes(value));
    Some(if path.is_absolute() {
        path.to_path_buf()
    } else {
        including_file.parent()?.join(path)
    })
}

/// Hooks git runs only under commands that write or talk to another repository, never under one of
/// [`READ_ONLY_GIT_SUBCOMMANDS`], by `githooks(5)`. Measured 2026-09-28 on git 2.55.0 against all
/// nine: `post-checkout`, `reference-transaction` and `pre-auto-gc` ran under none of them, and
/// `post-index-change` -- not on this list -- ran under `git status`. `fsmonitor-watchman` is left
/// off: it runs whenever `core.fsmonitor` names it.
const HOOKS_NO_READ_ONLY_SUBCOMMAND_RUNS: &[&str] = &[
    "applypatch-msg",
    "pre-applypatch",
    "post-applypatch",
    "pre-commit",
    "pre-merge-commit",
    "prepare-commit-msg",
    "commit-msg",
    "post-commit",
    "pre-rebase",
    "post-checkout",
    "post-merge",
    "pre-push",
    "pre-receive",
    "update",
    "proc-receive",
    "post-receive",
    "post-update",
    "reference-transaction",
    "push-to-checkout",
    "pre-auto-gc",
    "post-rewrite",
    "sendemail-validate",
    "p4-changelist",
    "p4-prepare-changelist",
    "p4-post-changelist",
    "p4-pre-submit",
];

/// A hook git might run under a read is a card; one it runs only under writing commands is not, so
/// a repository with the usual `pre-commit` or `commit-msg` hook still reads without a card. Git
/// runs a hook only by its exact name and no hook's name holds a `.`, so `*.sample`,
/// `pre-commit.legacy` and a `README.md` are skipped, as is a directory (husky's `_`). An
/// unrecognised name cards: the next git may run it under a read.
fn hooks_run_nothing_under_a_read(hooks: &Path) -> Result<(), &'static str> {
    let entries = match std::fs::read_dir(hooks) {
        Ok(entries) => entries,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(())
        }
        Err(_) => return Err(REASON_GIT_HOOK),
    };
    for entry in entries {
        let entry = entry.map_err(|_| REASON_GIT_HOOK)?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(REASON_GIT_HOOK);
        };
        if name.contains('.') || HOOKS_NO_READ_ONLY_SUBCOMMAND_RUNS.contains(&name) || entry.path().is_dir() {
            continue;
        }
        return Err(REASON_GIT_HOOK);
    }
    Ok(())
}

/// How many directories under `modules/` are walked before the answer is a card.
const MAX_SUBMODULE_GIT_DIRECTORIES: usize = 1024;

/// Every submodule git directory under `dir` (`modules/<name>/`, where a name may hold `/`, and
/// each one's own nested `modules/`), held to the same config and hook rules as the repository.
fn submodule_git_directories_run_nothing(
    dir: &Path,
    home: Option<&Path>,
    budget: &mut usize,
) -> Result<(), &'static str> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(())
        }
        Err(_) => return Err(REASON_GIT_CONFIG_UNREADABLE),
    };
    for entry in entries {
        let path = entry.map_err(|_| REASON_GIT_CONFIG_UNREADABLE)?.path();
        if !path.is_dir() {
            continue;
        }
        *budget = budget.checked_sub(1).ok_or(REASON_GIT_TOO_MUCH_TO_CHECK)?;
        let is_a_git_dir =
            path.join("HEAD").symlink_metadata().is_ok() || path.join("config").symlink_metadata().is_ok();
        if is_a_git_dir {
            for file in ["config", "config.worktree"] {
                config_runs_nothing(&path.join(file), home, 0)?;
            }
            hooks_run_nothing_under_a_read(&path.join("hooks"))?;
            submodule_git_directories_run_nothing(&path.join("modules"), home, budget)?;
        } else {
            submodule_git_directories_run_nothing(&path, home, budget)?;
        }
    }
    Ok(())
}

/// One variable a git config file sets, named as git compares names: section and key lowercased.
/// The subsection is not kept -- no rule here depends on it.
struct GitConfigVariable {
    section: String,
    key: String,
    /// `None` for a bare key, which git reads as boolean true.
    value: Option<Vec<u8>>,
}

impl GitConfigVariable {
    /// A variable whose value git runs as a program, or which moves the work tree, under one of the
    /// read-only subcommands or anything they spawn. The ruling's list (`diff.external`,
    /// `diff.*.command`, `diff.*.textconv`, `core.fsmonitor`, `core.hooksPath`, `filter.*`,
    /// `core.sshCommand`, `core.editor`, `core.pager`), plus what reading `git-config(1)` for git
    /// 2.55.0 added: `core.worktree` (measured above), `hook.*` (config-defined hooks, measured
    /// above), `pager.*` (per-command pagers, as `core.pager`), `gpg.*` (`log.showSignature` runs
    /// `gpg.program`), `core.askPass`, `core.gitProxy`, `core.alternateRefsCommand`,
    /// `sequence.editor` and `interactive.diffFilter`. Matched whatever the subsection, so
    /// `[core "x"] fsmonitor` cards too -- stricter than git, never looser.
    fn can_run_a_program(&self) -> bool {
        matches!(
            (self.section.as_str(), self.key.as_str()),
            (
                "core",
                "fsmonitor"
                    | "hookspath"
                    | "sshcommand"
                    | "editor"
                    | "pager"
                    | "askpass"
                    | "gitproxy"
                    | "alternaterefscommand"
                    | "worktree",
            ) | ("diff", "external" | "command" | "textconv")
                | ("filter" | "pager" | "gpg" | "hook", _)
                | ("sequence", "editor")
                | ("interactive", "difffilter")
        )
    }

    /// `include.path` and `includeIf.<condition>.path`. Followed whatever the condition says.
    fn is_an_include(&self) -> bool {
        matches!(
            (self.section.as_str(), self.key.as_str()),
            ("include" | "includeif", "path")
        )
    }
}

/// Git's config grammar, after `config.c`'s `git_parse_source`, `get_base_var`,
/// `get_extended_base_var`, `get_value` and `parse_value`, closely enough that every variable git
/// reads is read here, under the section git gives it. The case that makes a line-by-line reading
/// wrong is a value continued with a trailing `\`: git reads a following `[user]` line as part of the
/// value, so a `fsmonitor = ...` under it is still `core.fsmonitor`. Anything git itself would
/// reject -- git then dies before running anything -- is `None`, which the caller turns into a card.
fn parse_git_config(bytes: &[u8]) -> Option<Vec<GitConfigVariable>> {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    let mut cursor = ConfigCursor { bytes, at: 0 };
    let mut section: Option<String> = None;
    let mut variables = Vec::new();
    let mut comment = false;
    loop {
        let (c, end) = cursor.next();
        if c == b'\n' {
            if end {
                return Some(variables);
            }
            comment = false;
            continue;
        }
        if comment || is_config_space(c) {
            continue;
        }
        if c == b'#' || c == b';' {
            comment = true;
            continue;
        }
        if c == b'[' {
            section = Some(cursor.section_header()?);
            continue;
        }
        if !c.is_ascii_alphabetic() {
            return None;
        }
        // A variable before any section header is not one git can name.
        let section = section.clone()?;
        let (key, value) = cursor.variable(c)?;
        variables.push(GitConfigVariable { section, key, value });
    }
}

/// Git's own `isspace` (`sane_ctype`): exactly these four, not `\v` or `\f`.
fn is_config_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r')
}

struct ConfigCursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl ConfigCursor<'_> {
    /// Git's `get_next_char`: `\r\n` reads as `\n`, and the end of the file reads as a `\n` that
    /// says it is the end, as often as it is asked.
    fn next(&mut self) -> (u8, bool) {
        match self.bytes.get(self.at) {
            None => (b'\n', true),
            Some(b'\r') if self.bytes.get(self.at + 1) == Some(&b'\n') => {
                self.at += 2;
                (b'\n', false)
            }
            Some(&c) => {
                self.at += 1;
                (c, false)
            }
        }
    }

    /// `[section]`, `[section.subsection]` or `[section "subsection"]`, after its `[`; the section's
    /// name, lowercased.
    fn section_header(&mut self) -> Option<String> {
        let mut name = String::new();
        loop {
            let (c, end) = self.next();
            if end {
                return None;
            }
            if c == b']' {
                break;
            }
            if is_config_space(c) {
                self.quoted_subsection(c)?;
                break;
            }
            if !(c.is_ascii_alphanumeric() || c == b'-' || c == b'.') {
                return None;
            }
            name.push(c.to_ascii_lowercase() as char);
        }
        let section = name.split('.').next().unwrap_or_default();
        (!section.is_empty()).then(|| section.to_string())
    }

    /// The ` "subsection"]` of a header, from the blank that ended the section's name. Only its shape
    /// matters here: a newline anywhere in it, or anything but `]` after the closing quote, is an
    /// error to git.
    fn quoted_subsection(&mut self, mut c: u8) -> Option<()> {
        loop {
            if c == b'\n' {
                return None;
            }
            c = self.next().0;
            if !is_config_space(c) {
                break;
            }
        }
        if c != b'"' {
            return None;
        }
        loop {
            let c = self.next().0;
            if c == b'\n' {
                return None;
            }
            if c == b'"' {
                break;
            }
            if c == b'\\' && self.next().0 == b'\n' {
                return None;
            }
        }
        (self.next().0 == b']').then_some(())
    }

    /// A variable's name, from its first letter, and its value if it has an `=`.
    fn variable(&mut self, first: u8) -> Option<(String, Option<Vec<u8>>)> {
        let mut key = String::from(first.to_ascii_lowercase() as char);
        let mut c;
        loop {
            let (next, end) = self.next();
            c = next;
            if end || !(c.is_ascii_alphanumeric() || c == b'-') {
                break;
            }
            key.push(c.to_ascii_lowercase() as char);
        }
        while c == b' ' || c == b'\t' {
            c = self.next().0;
        }
        if c == b'\n' {
            return Some((key, None));
        }
        if c != b'=' {
            return None;
        }
        Some((key, Some(self.value()?)))
    }

    /// Git's `parse_value`: quotes, the five escapes, a trailing `\` continuing onto the next line,
    /// `#`/`;` starting a comment outside quotes, surrounding blanks trimmed.
    fn value(&mut self) -> Option<Vec<u8>> {
        let mut value = Vec::new();
        let (mut quote, mut comment) = (false, false);
        let mut trim_len = 0usize;
        loop {
            let c = self.next().0;
            if c == b'\n' {
                if quote {
                    return None;
                }
                if trim_len > 0 {
                    value.truncate(trim_len);
                }
                return Some(value);
            }
            if comment {
                continue;
            }
            if is_config_space(c) && !quote {
                if trim_len == 0 {
                    trim_len = value.len();
                }
                if !value.is_empty() {
                    value.push(c);
                }
                continue;
            }
            if !quote && (c == b';' || c == b'#') {
                comment = true;
                continue;
            }
            trim_len = 0;
            if c == b'\\' {
                let escaped = match self.next().0 {
                    b'\n' => continue,
                    b't' => b'\t',
                    b'b' => 0x08,
                    b'n' => b'\n',
                    b'\\' => b'\\',
                    b'"' => b'"',
                    _ => return None,
                };
                value.push(escaped);
                continue;
            }
            if c == b'"' {
                quote = !quote;
                continue;
            }
            value.push(c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A real directory is needed so path resolution distinguishes an unresolvable path from
    /// one outside the root. A minimal `.git` from [`fabricate_git_dir`] also lets read-only
    /// git calls pass and stops repository discovery at the root, so repositories above
    /// `$TMPDIR` cannot change the result.
    struct Workspace {
        root: PathBuf,
    }

    impl Workspace {
        fn new() -> Self {
            let ws = Self::without_git();
            fabricate_git_dir(&ws.root.join(".git"));
            ws
        }
        /// A workspace with no `.git`, for the cases about a repository elsewhere, or none.
        fn without_git() -> Self {
            let root = std::env::temp_dir().join(format!("agent-permission-policy-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(root.join("src")).unwrap();
            std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
            Self { root }
        }
        fn path(&self) -> &Path {
            &self.root
        }
        fn write(&self, relative: &str, contents: impl AsRef<[u8]>) {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        #[cfg(unix)]
        fn link(&self, relative: &str, target: impl AsRef<Path>) {
            let path = self.root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(target, path).unwrap();
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// The least a directory needs for git's own `is_git_directory` to accept it: a `HEAD` naming a
    /// ref, and searchable `objects/` and `refs/`. Written by hand rather than by `git init`, so the
    /// tests neither need git installed nor pick up the host's templates -- one test
    /// (`a_repository_git_init_makes_is_one_the_policy_reads`) runs the real `git init` to check
    /// that this is the same shape.
    fn fabricate_git_dir(git_dir: &Path) {
        std::fs::create_dir_all(git_dir.join("objects")).unwrap();
        std::fs::create_dir_all(git_dir.join("refs/heads")).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    }

    /// A directory outside every workspace, holding `secret` -- what a leak would print.
    struct Outside {
        dir: PathBuf,
    }

    impl Outside {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("agent-policy-outside-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("secret"), "private text\n").unwrap();
            Self { dir }
        }
        fn secret(&self) -> PathBuf {
            self.dir.join("secret")
        }
    }

    impl Drop for Outside {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Every command whose verdict is not `expected`, so a red run lists every leaking row at once
    /// instead of stopping at the first.
    fn mismatches(commands: &[&str], root: &Path, expected: PermissionVerdict) -> Vec<String> {
        commands
            .iter()
            .filter_map(|command| {
                let got = classify_permission_request("Bash", &json!({ "command": command }), root);
                (got.verdict != expected).then(|| format!("`{command}` -> {:?} ({})", got.verdict, got.reason))
            })
            .collect()
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

    /// CLI 2.1.283 names the subagent tool `Agent`; older versions use `Task`.
    /// A call with `subagent_type` and `prompt` must get the same verdict under either name,
    /// so the rename does not cause an unknown-tool card for every subagent launch.
    #[test]
    fn a_subagent_launch_is_never_a_card_under_either_of_its_two_names() {
        let ws = Workspace::new();
        let input = json!({ "subagent_type": "general-purpose", "description": "d", "prompt": "p" });
        assert_eq!(
            verdict("Agent", input.clone(), ws.path()),
            PermissionVerdict::AllowWithoutAsking,
            "CLI 2.1.283 spells the subagent tool Agent"
        );
        assert_eq!(
            verdict("Task", input, ws.path()),
            PermissionVerdict::AllowWithoutAsking,
            "CLI 2.1.272 still spells it Task"
        );
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
        let classify_with_home = |tool: &str, input: &Value, root: &Path, home: Option<&Path>| {
            let surroundings = Surroundings {
                home,
                git_location_from_environment: false,
            };
            classify_in(tool, input, root, &surroundings)
        };
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

    // prefix rules

    use crate::permission_rules::{PrefixRule, PrefixRules};

    /// Both rules a person could plausibly have made from `command`: its first word, and its first
    /// two words. The strongest rule set that could apply to it.
    fn rules_for(command: &str) -> PrefixRules {
        let words: Vec<&str> = command.split_whitespace().collect();
        let mut rules = PrefixRules::default();
        for n in 1..=words.len().min(2) {
            if let Some(rule) = PrefixRule::parse(&format!("Bash({} *)", words[..n].join(" "))) {
                rules = rules.with(rule);
            }
        }
        rules
    }

    /// Matching prefix rules must not allow `Bash` inputs refused for syntax or paths.
    /// The corpus repeats every card-producing `Bash` input from this module's tests,
    /// using a workspace with the same links.
    #[test]
    fn no_rule_ever_turns_a_card_the_classifier_refused_on_syntax_or_paths_into_an_allow() {
        let ws = Workspace::new();
        let outside = std::env::temp_dir().join(format!("agent-policy-outside-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret"), "s").unwrap();
        std::os::unix::fs::symlink(&outside, ws.path().join("escape")).unwrap();
        std::os::unix::fs::symlink(ws.path().join("nowhere"), ws.path().join("dangling")).unwrap();
        let long = format!("ls {}", "a".repeat(MAX_BASH_COMMAND_LEN));
        let corpus: Vec<&str> = vec![
            "echo hi > c.txt",
            "echo hi >> c.txt",
            "cat < a.txt",
            "ls | tee out.txt",
            "ls && rm -rf .",
            "ls; rm -rf .",
            "echo $(rm -rf .)",
            "echo `rm -rf .`",
            "ls *.rs",
            "cat 'a b.txt'",
            "ls ~",
            "ls \\\n -l",
            "ls & ",
            "cat /etc/hostname",
            "ls ../..",
            "grep -r x ../other",
            "find . -delete",
            "find . -exec rm -rf . +",
            "find . -fprint out.txt",
            "git log --output=/home/someone/.bashrc",
            "git show --output=src/main.rs",
            "git log --output o3.txt",
            "git diff --output out.patch",
            "diff --from-file=/home/someone/.ssh/id_ed25519 README.md",
            "git blame --contents=/etc/hostname src/main.rs",
            "grep -f/etc/hostname src/main.rs",
            "diff --from-file=src/main.rs src/main.rs",
            "find . -maxdepth 0 -fprint0 src/main.rs",
            "find . -fprintf out.txt x",
            "find . -fls out.txt",
            "find . -execdir ls +",
            "find . -okdir ls +",
            "cat escape/secret",
            "head escape/secret",
            "ls escape",
            "cat dangling",
            "grep -fescape src/main.rs",
            "wc -l escape/secret",
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
            "cd",
            "cd src",
            "cd .",
            &long,
        ];
        for command in corpus {
            let input = json!({ "command": command });
            let plain = classify_permission_request("Bash", &input, ws.path());
            assert!(plain.needs_a_human(), "the corpus is card-producing: `{command}`");
            let with_rules = classify_with_rules("Bash", &input, ws.path(), &rules_for(command));
            assert_eq!(
                with_rules, plain,
                "a rule changed a fail-closed verdict for `{command}`"
            );
            assert_eq!(
                rule_that_allows("Bash", &input, ws.path(), &rules_for(command)),
                None,
                "no row may name a rule for `{command}`"
            );
        }
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// The positive control, without which the test above proves nothing: the commands the classifier
    /// refuses only because their first word is not on a read-only list DO become allows.
    #[test]
    fn a_matching_rule_allows_what_was_refused_only_for_its_first_words() {
        let ws = Workspace::new();
        for command in [
            "rm b.txt",
            "mv a b",
            "npm ci",
            "python script.py",
            "git commit -m probe",
            "git push",
            "uname -a",
        ] {
            let input = json!({ "command": command });
            assert!(
                classify_permission_request("Bash", &input, ws.path()).needs_a_human(),
                "`{command}`"
            );
            let decided = classify_with_rules("Bash", &input, ws.path(), &rules_for(command));
            assert_eq!(decided.verdict, PermissionVerdict::AllowWithoutAsking, "`{command}`");
            assert_eq!(decided.reason, REASON_ALLOWED_BY_A_PROJECT_RULE);
            // F18: the rule that did it is named, first match first (`rules_for` adds the one-word
            // rule before the two-word one).
            let first = command.split_whitespace().next().unwrap();
            assert_eq!(
                rule_that_allows("Bash", &input, ws.path(), &rules_for(command)),
                Some(format!("Bash({first} *)")),
                "`{command}`"
            );
        }
        // A rule for other words changes nothing.
        let other = PrefixRules::default().with(PrefixRule::parse("Bash(cargo test *)").unwrap());
        assert!(classify_with_rules("Bash", &json!({ "command": "rm b.txt" }), ws.path(), &other).needs_a_human());
        assert_eq!(
            rule_that_allows("Bash", &json!({ "command": "rm b.txt" }), ws.path(), &other),
            None
        );
        // A call the classifier already allows was not answered by a rule, whatever matches it.
        let ls = PrefixRules::default().with(PrefixRule::parse("Bash(ls *)").unwrap());
        assert_eq!(
            rule_that_allows("Bash", &json!({ "command": "ls" }), ws.path(), &ls),
            None
        );
        // No rule ever reaches another tool, even one the classifier cards.
        let all = PrefixRules::default().with(PrefixRule::parse("Bash(src *)").unwrap());
        assert!(classify_with_rules("Write", &json!({ "file_path": "src/main.rs" }), ws.path(), &all).needs_a_human());
    }

    /// Only the offer the card may show: a rule for a command it would allow, none for one it would
    /// not change.
    #[test]
    fn a_card_is_offered_a_rule_only_where_one_would_take_effect() {
        let ws = Workspace::new();
        let offer = |command: &str| crate::permission_rules::offer("Bash", &json!({ "command": command }), ws.path());
        assert_eq!(
            offer("git push origin main").map(|r| r.display()),
            Some("git push *".to_string())
        );
        assert_eq!(
            offer("cargo test --lib").map(|r| r.display()),
            Some("cargo test *".to_string())
        );
        assert_eq!(
            offer("git log --output o3.txt"),
            None,
            "a writing option: no rule changes that"
        );
        assert_eq!(offer("ls && rm -rf ."), None, "compound: no rule changes that");
        assert_eq!(offer("cat /etc/hostname"), None, "outside the root");
        assert_eq!(offer("ls"), None, "already allowed");
        assert_eq!(
            crate::permission_rules::offer("Write", &json!({ "file_path": "a" }), ws.path()),
            None
        );
    }

    // ---- reads the path checks never saw ---------------------------------------------------------
    //
    // Each of these was allowed without a card before the fix, and each was reproduced by running the
    // real command in a scratch fixture before being fixed.

    /// A list file inside the project names what the command then reads, so the path checks, which
    /// look only at argv, never see the path. `wc`/`du --files0-from` print an outside file's size,
    /// `find -files0-from` lists an outside directory (the CLI returned no refusal for it either --
    /// the oracle, 2026-09-28 -- so here the policy is stricter than the CLI, by choice), and
    /// GNU getopt takes every unambiguous abbreviation (`--files0`, `--f`) and a list with no NUL.
    /// Pattern and exclude files read another file's contents the same way.
    #[test]
    fn an_option_that_reads_paths_or_patterns_from_another_file_gets_a_card() {
        let ws = Workspace::new();
        let outside = Outside::new();
        ws.write("paths", format!("{}\0", outside.secret().display()));
        ws.write("plain", outside.secret().display().to_string());
        ws.write("dirs", format!("{}\0", outside.dir.display()));
        let leaks = mismatches(
            &[
                "wc -c --files0-from paths",
                "wc -c --files0 paths",
                "wc -c --f paths",
                "wc -c --files0-from plain",
                "du -b --files0-from paths",
                "du -b --files0-f paths",
                "find -files0-from dirs",
                "find -files0-from dirs -name secret",
                "grep --file plain src/main.rs",
                "grep -f plain src/main.rs",
                "grep -nf plain src/main.rs",
                "grep --exclude-from plain -r x src",
                "du -X plain .",
                "du --exclude-from plain .",
                "diff -X plain src/main.rs src/main.rs",
                "diff --exclude-from plain src/main.rs src/main.rs",
                "which --read-alias ls",
                "git blame --contents plain src/main.rs",
                "git blame --cont plain src/main.rs",
                "git blame -S plain src/main.rs",
                "git blame --ignore-revs-file plain src/main.rs",
                "git ls-files --exclude-from plain",
                "git ls-files -X plain",
                "git diff -O plain",
                "git diff --no-index src plain",
                "git log --stdin",
            ],
            ws.path(),
            PermissionVerdict::AskTheUser,
        );
        assert!(leaks.is_empty(), "allowed without a card: {leaks:#?}");

        // The ordinary spellings of the same commands are untouched.
        let cards = mismatches(
            &[
                "wc -c src/main.rs",
                "wc -l --lines src/main.rs",
                "du -sh src",
                "du -b src/main.rs",
                "grep -rn main src",
                "grep -e main -n src/main.rs",
                "grep --count main src/main.rs",
                "grep -A3 -i main src/main.rs",
                "find . -name main.rs -type f",
                "find src -mtime -7 -print",
                "find . -maxdepth 2 -newermt 2020-01-01",
                "head -n 5 src/main.rs",
                "head -5 src/main.rs",
                "tail -n 3 src/main.rs",
                "ls -la src",
                "ls -1 --group-directories-first",
                "cat -n src/main.rs",
                "stat -c %n src/main.rs",
                "diff -u src/main.rs src/main.rs",
                "which -a ls",
                "pwd -P",
                "echo --files0-from paths",
                "git log --oneline -5",
                "git log -n 3 --stat",
                "git blame src/main.rs",
                "git ls-files --others --exclude-standard",
                "git status --short --branch",
                "git status -uno",
                "git show --stat HEAD",
                "git diff --cached --name-only",
                "git rev-parse --show-toplevel",
                "git describe --tags --always",
            ],
            ws.path(),
            PermissionVerdict::AllowWithoutAsking,
        );
        assert!(cards.is_empty(), "carded although nothing else is read: {cards:#?}");
    }

    /// `diff dirA dirB` without `-r` still opens each entry the two directories share, and follows a
    /// link doing it: measured 2026-09-28, `a/f -> ../../outside-secret` against an empty `b/f`
    /// printed `< private text`. `diff FILE DIR` compares against `DIR/FILE`, so it follows too.
    #[cfg(unix)]
    #[test]
    fn diff_of_a_directory_gets_a_card_because_it_follows_a_link_inside_it() {
        let ws = Workspace::new();
        let outside = Outside::new();
        ws.link("a/f", outside.secret());
        ws.write("b/f", "");
        ws.link("a/main.rs", outside.secret());
        ws.write("a/g", "one\n");
        ws.write("b/g", "two\n");
        let leaks = mismatches(
            &[
                "diff a b",
                "diff -q a b",
                "diff --brief a b",
                "diff -u b a",
                "diff src/main.rs a",
                "diff a src/main.rs",
            ],
            ws.path(),
            PermissionVerdict::AskTheUser,
        );
        assert!(leaks.is_empty(), "allowed without a card: {leaks:#?}");
        // Two files are still two files.
        assert_eq!(bash("diff a/g b/g", ws.path()), PermissionVerdict::AllowWithoutAsking);
    }

    /// A project that is a subdirectory of a larger repository: git's discovery walks above the root,
    /// and `git show HEAD:outside-secret` printed a committed file outside the project (measured
    /// 2026-09-28). The root is then no boundary for git at all, the way `$HOME` is none for `Read`.
    #[test]
    fn a_project_inside_a_larger_repository_cards_every_git_call() {
        let outer = Workspace::without_git();
        fabricate_git_dir(&outer.path().join(".git"));
        outer.write("outside-secret", "private text\n");
        outer.write("project/src/main.rs", "fn main() {}");
        let project = outer.path().join("project");
        let leaks = mismatches(
            &[
                "git status",
                "git log -p",
                "git show HEAD:outside-secret",
                "git ls-files",
                "git diff",
                "git rev-parse --show-toplevel",
            ],
            &project,
            PermissionVerdict::AskTheUser,
        );
        assert!(leaks.is_empty(), "allowed without a card: {leaks:#?}");
        // Not git: the root is still the boundary for everything else.
        assert_eq!(bash("cat src/main.rs", &project), PermissionVerdict::AllowWithoutAsking);

        // A repository of its own at the project root stops git's walk there, whatever is above.
        fabricate_git_dir(&project.join(".git"));
        assert_eq!(bash("git log -p", &project), PermissionVerdict::AllowWithoutAsking);
    }

    /// Git accepts a `.git` directory only if it really is one; otherwise it keeps walking up
    /// (measured 2026-09-28: an empty `.git`, a `HEAD` alone, `objects/` mode 000 and a garbage
    /// `HEAD` each made `git rev-parse --show-toplevel` answer the repository ABOVE). So a `.git` at
    /// the root that is not a repository cannot count as the boundary. A bare repository above the
    /// root, or the root itself being a git directory, is the same case.
    #[test]
    fn a_dot_git_that_git_would_walk_past_is_no_boundary() {
        let outer = Workspace::without_git();
        fabricate_git_dir(&outer.path().join(".git"));
        let project = outer.path().join("project");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        assert_eq!(bash("git log", &project), PermissionVerdict::AskTheUser, "empty .git");
        std::fs::write(project.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        assert_eq!(bash("git log", &project), PermissionVerdict::AskTheUser, "HEAD alone");
        std::fs::create_dir_all(project.join(".git/objects")).unwrap();
        std::fs::create_dir_all(project.join(".git/refs")).unwrap();
        std::fs::write(project.join(".git/HEAD"), "garbage\n").unwrap();
        assert_eq!(bash("git log", &project), PermissionVerdict::AskTheUser, "garbage HEAD");
        std::fs::write(project.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        assert_eq!(
            bash("git log", &project),
            PermissionVerdict::AllowWithoutAsking,
            "the positive control: a real one"
        );
        // `objects/` that git may not search: git's `access(X_OK)` fails and it walks on. Root may
        // search anything, so there the premise does not hold.
        #[cfg(unix)]
        if unsafe { libc::geteuid() } != 0 {
            use std::os::unix::fs::PermissionsExt;
            let objects = project.join(".git/objects");
            std::fs::set_permissions(&objects, std::fs::Permissions::from_mode(0o000)).unwrap();
            let verdict = bash("git log", &project);
            std::fs::set_permissions(&objects, std::fs::Permissions::from_mode(0o755)).unwrap();
            assert_eq!(verdict, PermissionVerdict::AskTheUser, "objects/ at mode 000");
        }

        // A bare repository above the root: `HEAD`, `objects/`, `refs/` and no `.git`.
        let bare = Workspace::without_git();
        fabricate_git_dir(bare.path());
        std::fs::create_dir_all(bare.path().join("sub")).unwrap();
        assert_eq!(bash("git log", &bare.path().join("sub")), PermissionVerdict::AskTheUser);
        // The root itself a git directory.
        assert_eq!(bash("git log", bare.path()), PermissionVerdict::AskTheUser);
    }

    /// A repository's `.git/config` can make read-only commands run programs through
    /// `diff.external`, `core.fsmonitor` or `hook.<name>.event = post-index-change`.
    /// `core.worktree` can make `git diff` read outside the project. A directory or archive
    /// can arrive with this metadata even though writing it through the tools needs a card.
    #[test]
    fn a_repository_whose_own_config_can_run_a_program_cards_every_git_call() {
        for config in [
            "[diff]\n\texternal = sh helper.sh\n",
            "[diff \"x\"]\n\tcommand = sh helper.sh\n",
            "[diff \"x\"]\n\ttextconv = sh helper.sh\n",
            "[diff.x]\n\ttextconv = sh helper.sh\n", // the deprecated spelling of a subsection
            "[core]\n\tfsmonitor = ./fsm.sh\n",
            "[Core]\n\tFSMonitor = ./fsm.sh\n", // section and key names are case-insensitive
            "[core] fsmonitor = ./fsm.sh\n",    // a variable on the header's own line
            "[core]\n\thooksPath = hooks\n",
            "[core]\n\tsshCommand = sh x\n",
            "[core]\n\teditor = sh x\n",
            "[core]\n\tpager = sh x\n",
            "[core]\n\tworktree = /home\n",
            "[filter \"lfs\"]\n\tsmudge = sh x\n",
            "[filter \"lfs\"]\n\tprocess = sh x\n",
            "[pager]\n\tlog = sh x\n",
            "[gpg]\n\tprogram = sh x\n",
            "[gpg \"ssh\"]\n\tprogram = sh x\n",
            "[hook \"probe\"]\n\tcommand = sh x\n\tevent = post-index-change\n",
            "[sequence]\n\teditor = sh x\n",
            "[interactive]\n\tdiffFilter = sh x\n",
            // A continued value that LOOKS like a header: git reads `[user]` as part of `a`'s value,
            // so `fsmonitor` below it is still `core.fsmonitor`.
            "[core]\n\ta = x \\\n[user]\n\tfsmonitor = ./fsm.sh\n",
            // Something git cannot parse makes git die, so it runs nothing -- but it cards here too.
            "[core\n\tfsmonitor = ./fsm.sh\n",
        ] {
            let ws = Workspace::new();
            ws.write(".git/config", config);
            let leaks = mismatches(
                &["git status", "git diff", "git log"],
                ws.path(),
                PermissionVerdict::AskTheUser,
            );
            assert!(leaks.is_empty(), "config {config:?}: {leaks:#?}");
        }

        // The ordinary config `git init` and `git clone` write changes nothing.
        let ws = Workspace::new();
        ws.write(
            ".git/config",
            "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n\tlogallrefupdates = true\n\
             [remote \"origin\"]\n\turl = ssh://git@example.com/x.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n\
             [branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n\
             [user]\n\tname = \"A \\\"quoted\\\" name\" ; a comment\n\temail = a@example.com # another\n",
        );
        assert_eq!(bash("git status", ws.path()), PermissionVerdict::AllowWithoutAsking);
    }

    /// `include.path` and `includeIf.*.path` pull in another file git reads as part of the config,
    /// so what it sets counts. Every include is followed whatever its condition says -- a superset of
    /// what git reads. A missing include is ignored, as git ignores it; a loop past git's own depth
    /// limit cards.
    #[test]
    fn a_repository_config_include_is_read_as_git_reads_it() {
        let ws = Workspace::new();
        ws.write(".git/extra.inc", "[core]\n\tfsmonitor = ./fsm.sh\n");
        ws.write(".git/config", "[include]\n\tpath = extra.inc\n");
        assert_eq!(
            bash("git status", ws.path()),
            PermissionVerdict::AskTheUser,
            "include.path"
        );
        ws.write(".git/config", "[includeIf \"onbranch:nope\"]\n\tpath = extra.inc\n");
        assert_eq!(
            bash("git status", ws.path()),
            PermissionVerdict::AskTheUser,
            "includeIf"
        );
        ws.write(".git/config", "[include]\n\tpath = does-not-exist.inc\n");
        assert_eq!(
            bash("git status", ws.path()),
            PermissionVerdict::AllowWithoutAsking,
            "missing include"
        );
        ws.write(".git/config", "[include]\n\tpath = config\n");
        assert_eq!(
            bash("git status", ws.path()),
            PermissionVerdict::AskTheUser,
            "include loop"
        );
    }

    /// Code that runs without any config: a `post-index-change` hook ran under `git status` (measured
    /// 2026-09-28, git 2.55.0; none of the other hooks tried ran under any of the read-only
    /// subcommands). A submodule's own git directory under `.git/modules/` is read by the
    /// superproject's `git status` and `git diff`, whose `core.fsmonitor` ran twice under each. And
    /// alternates let `git show <id>` read another repository's objects.
    #[test]
    fn hooks_submodule_configs_and_alternates_that_reach_out_card_every_git_call() {
        let hook = Workspace::new();
        hook.write(".git/hooks/post-index-change", "#!/bin/sh\n");
        assert_eq!(
            bash("git status", hook.path()),
            PermissionVerdict::AskTheUser,
            "post-index-change"
        );
        let unknown = Workspace::new();
        unknown.write(".git/hooks/some-future-hook", "#!/bin/sh\n");
        assert_eq!(
            bash("git log", unknown.path()),
            PermissionVerdict::AskTheUser,
            "an unknown hook name"
        );
        // What `git init` and the usual hook managers install is not one of them.
        let usual = Workspace::new();
        for name in [
            "pre-commit",
            "pre-commit.legacy",
            "commit-msg",
            "pre-push",
            "post-checkout",
            "post-index-change.sample",
        ] {
            usual.write(&format!(".git/hooks/{name}"), "#!/bin/sh\n");
        }
        std::fs::create_dir_all(usual.path().join(".git/hooks/_")).unwrap();
        assert_eq!(bash("git status", usual.path()), PermissionVerdict::AllowWithoutAsking);

        let submodule = Workspace::new();
        submodule.write(".git/modules/libs/sub/config", "[core]\n\tfsmonitor = ./fsm.sh\n");
        assert_eq!(
            bash("git status", submodule.path()),
            PermissionVerdict::AskTheUser,
            "submodule config"
        );
        let submodule_hook = Workspace::new();
        submodule_hook.write(".git/modules/sub/config", "[core]\n\tbare = false\n");
        submodule_hook.write(".git/modules/sub/hooks/post-index-change", "#!/bin/sh\n");
        assert_eq!(
            bash("git diff", submodule_hook.path()),
            PermissionVerdict::AskTheUser,
            "submodule hook"
        );

        let alternates = Workspace::new();
        alternates.write(".git/objects/info/alternates", "/somewhere/else/.git/objects\n");
        assert_eq!(
            bash("git show HEAD", alternates.path()),
            PermissionVerdict::AskTheUser,
            "alternates"
        );
    }

    /// A linked worktree (`.git` a file naming a git directory elsewhere, which names its common
    /// directory) is the project's own repository: its toplevel is the root. What its config says is
    /// read from the common directory, where git reads it.
    #[test]
    fn a_linked_worktree_is_judged_by_its_common_directory() {
        let main = Workspace::new();
        let git_dir = main.path().join(".git/worktrees/wt");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/wt\n").unwrap();
        std::fs::write(git_dir.join("commondir"), "../..\n").unwrap();
        let wt = Workspace::without_git();
        std::fs::write(wt.path().join(".git"), format!("gitdir: {}\n", git_dir.display())).unwrap();
        assert_eq!(
            bash("git log --oneline", wt.path()),
            PermissionVerdict::AllowWithoutAsking
        );
        main.write(".git/config", "[diff]\n\texternal = sh helper.sh\n");
        assert_eq!(
            bash("git diff", wt.path()),
            PermissionVerdict::AskTheUser,
            "the common config"
        );
        std::fs::write(wt.path().join(".git"), "nonsense\n").unwrap();
        assert_eq!(
            bash("git log", wt.path()),
            PermissionVerdict::AskTheUser,
            "a gitfile git rejects"
        );
    }

    /// With the repository toplevel at the root, `<rev>:<path>` names a path relative
    /// to that root. Paths containing `..` or an absolute escape must still card.
    #[test]
    fn a_rev_path_argument_stays_inside_when_the_toplevel_is_the_root() {
        let ws = Workspace::new();
        assert_eq!(
            bash("git show HEAD:src/main.rs", ws.path()),
            PermissionVerdict::AllowWithoutAsking
        );
        // `HEAD:/etc/passwd` is not here: git looks `/etc/passwd` up in the tree, where no path starts
        // with `/`, and refuses it (measured 2026-09-28) -- so it reads nothing and needs no card.
        for command in [
            "git show HEAD:../secret",
            "git show HEAD:./../secret",
            "git log -- ../other",
        ] {
            assert_eq!(bash(command, ws.path()), PermissionVerdict::AskTheUser, "`{command}`");
        }
    }

    /// Words this policy splits differently from the shell. `split_whitespace` splits at a no-break
    /// space, a vertical tab and a form feed; bash and zsh do not, so `cat src<NBSP>x` read a file
    /// named `src<NBSP>x` -- here a link out -- while the checks saw `src` and `x` (measured
    /// 2026-09-28, both shells). And `^` is a glob in zsh with `extendedglob`: `cat ^s` printed every
    /// file but `s`, a link out included.
    #[cfg(unix)]
    #[test]
    fn a_blank_the_shell_does_not_split_on_and_a_zsh_glob_get_a_card() {
        let ws = Workspace::new();
        let outside = Outside::new();
        for separator in ['\u{a0}', '\u{0b}', '\u{0c}', '\u{2003}', '\u{85}'] {
            ws.link(&format!("src{separator}x"), outside.secret());
            let command = format!("cat src{separator}x");
            assert_eq!(bash(&command, ws.path()), PermissionVerdict::AskTheUser, "{command:?}");
        }
        ws.link("escape", outside.secret());
        assert_eq!(
            bash("cat ^s", ws.path()),
            PermissionVerdict::AskTheUser,
            "zsh extendedglob"
        );
        // A tab is a blank to the shell and to this policy alike.
        assert_eq!(
            bash("cat\tsrc/main.rs", ws.path()),
            PermissionVerdict::AllowWithoutAsking
        );
    }

    /// `GIT_DIR` (or a sibling) in the environment the Bash tool inherits tells git where the
    /// repository is, whatever the project root holds.
    #[test]
    fn a_git_location_from_the_environment_cards_every_git_call() {
        let ws = Workspace::new();
        let input = json!({ "command": "git log" });
        let with = |git_location_from_environment| Surroundings {
            home: None,
            git_location_from_environment,
        };
        assert!(classify_in("Bash", &input, ws.path(), &with(true)).needs_a_human());
        assert!(!classify_in("Bash", &input, ws.path(), &with(false)).needs_a_human());
        // Only git: nothing else reads those variables.
        let cat = json!({ "command": "cat src/main.rs" });
        assert!(!classify_in("Bash", &cat, ws.path(), &with(true)).needs_a_human());
    }

    /// `include.path = ~/x` is the home directory's `x`, as git resolves it.
    #[test]
    fn a_config_include_under_home_is_followed_there() {
        let ws = Workspace::new();
        let home = Workspace::without_git();
        home.write("hazard.inc", "[diff]\n\texternal = sh x\n");
        home.write("harmless.inc", "[user]\n\tname = a\n");
        let surroundings = Surroundings {
            home: Some(home.path()),
            git_location_from_environment: false,
        };
        let status = json!({ "command": "git status" });
        ws.write(".git/config", "[include]\n\tpath = ~/hazard.inc\n");
        assert!(classify_in("Bash", &status, ws.path(), &surroundings).needs_a_human());
        ws.write(".git/config", "[include]\n\tpath = ~/harmless.inc\n");
        assert!(!classify_in("Bash", &status, ws.path(), &surroundings).needs_a_human());
        ws.write(".git/config", "[include]\n\tpath = ~other/x.inc\n");
        assert!(classify_in("Bash", &status, ws.path(), &surroundings).needs_a_human());
    }

    /// The fabricated `.git` above stands in for a real one; this checks the real one reads the same:
    /// what `git init` writes (its `config`, its `*.sample` hooks) is a repository this policy reads
    /// without a card. Skipped where git is not installed.
    #[test]
    fn a_repository_git_init_makes_is_one_the_policy_reads() {
        let ws = Workspace::without_git();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .arg(ws.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_TEMPLATE_DIR")
            .status();
        let Ok(status) = init else {
            eprintln!("git is not installed; skipped");
            return;
        };
        assert!(status.success());
        assert!(
            ws.path().join(".git/hooks").is_dir(),
            "the premise: git init installed its sample hooks"
        );
        assert_eq!(
            bash("git status --short", ws.path()),
            PermissionVerdict::AllowWithoutAsking
        );
        assert_eq!(
            bash("git log --oneline", ws.path()),
            PermissionVerdict::AllowWithoutAsking
        );
    }

    /// The new cards are fail-closed checks, not "not on a list" -- so a saved rule matching the
    /// command changes none of them.
    #[cfg(unix)]
    #[test]
    fn no_rule_turns_any_of_the_p1_audit_cards_into_an_allow() {
        let ws = Workspace::new();
        let outside = Outside::new();
        ws.write("paths", format!("{}\0", outside.secret().display()));
        ws.link("a/f", outside.secret());
        ws.write("b/f", "");
        let check = |command: &str, root: &Path| {
            let input = json!({ "command": command });
            let plain = classify_permission_request("Bash", &input, root);
            assert!(plain.needs_a_human(), "the premise: `{command}` is a card");
            assert_eq!(
                classify_with_rules("Bash", &input, root, &rules_for(command)),
                plain,
                "a rule changed `{command}`"
            );
        };
        for command in [
            "wc -c --files0-from paths",
            "du --files0 paths",
            "diff a b",
            "grep -f paths src/main.rs",
        ] {
            check(command, ws.path());
        }
        ws.write(".git/config", "[core]\n\thooksPath = h\n");
        for command in ["git commit -m x", "git push origin main", "git status"] {
            check(command, ws.path());
        }
        let outer = Workspace::without_git();
        fabricate_git_dir(&outer.path().join(".git"));
        outer.write("project/x", "");
        for command in ["git push", "git commit -m x", "git log"] {
            check(command, &outer.path().join("project"));
        }
    }
}
