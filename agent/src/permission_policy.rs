//! Classifies tool calls that reach the permission gate.
//!
//! Both backends install a `PreToolUse` gate with matcher `*`. The gate sees every call;
//! this module decides which calls need a user prompt. Asking the host about a call must
//! not automatically produce a GUI card for harmless reads and searches.
//!
//! The policy lives in `agent` because it depends on the CLI's tool names, input fields
//! and permission behavior. Its CLI conformance tests live in the same crate.
//!
//! The baseline is the CLI's interactive `default` mode without configured permission
//! rules. Under `claude --print` without `--permission-mode`, calls that would prompt
//! are denied instead. On CLI 2.1.272, `Read`, `ls`, `cat a.txt` and `git status --short`
//! were allowed; `Write`, `echo hi > c.txt`, `rm b.txt` and `git commit` were denied.
//! The CLI's `auto` mode is separate and uses a classifier for some actions.
//!
//! For `Write`, `Edit` and `NotebookEdit`, this module also reproduces the deterministic
//! `acceptEdits` fast path from CLI 2.1.283. Both the requested and resolved paths must
//! stay inside the project root and pass [`DANGEROUS_DIRECTORIES`],
//! [`DANGEROUS_RELATIVE_PATH`] and [`PROTECTED_EDIT_FILE_BASENAMES`]. Hard-linked files,
//! malformed input, unresolvable targets and paths that cannot be examined get a card.
//! A qualifying target gets [`PermissionVerdict::AllowWithoutAsking`].
//!
//! The application resolves paths in a different process from the CLI that opens them.
//! Links such as `/proc/self/cwd` or `/dev/fd` can mean different things in each process.
//! The shared resolver therefore refuses lookups under `/proc`, `/sys` and `/dev`
//! ([`KERNEL_TREES`]). [`resolve_in_root`] also refuses links outside the root or links
//! that leave it at any step, even if they return inside. These restrictions apply to
//! read tools and `Bash` arguments as well as edit targets.
//!
//! `boundary` rejects a root that no longer resolves to itself. Otherwise, moving the
//! project and replacing it with a link could silently change the permission boundary.
//! `classify_search_path` rejects `Grep` and `Glob` paths whose spelling the CLI would
//! change by trimming, expanding `~` or settling `..` lexically.
//!
//! A path can still change between this check and the CLI opening it. A process able
//! to change a directory on the path can retarget a link or replace a directory with
//! one. Closing that window requires the opening process to enforce containment,
//! for example with `openat2` and `RESOLVE_BENEATH`.
//!
//! The entire `.claude` directory requires a card here. The CLI's exceptions for its
//! skills, agents, commands, scheduled tasks and worktrees are not reproduced.
//!
//! Saved [`crate::permission_rules::PrefixRules`] are allow-only and match only `Bash`.
//! They never match `Write`, `Edit` or `NotebookEdit`, so they cannot override the edit
//! fast path or widen it past protected paths. This module does not read the CLI's own
//! `permissions.deny` or `permissions.ask` settings.
//!
//! CLI 2.1.283's bundle indicates that its own deny rules override a hook `allow` and
//! its edit ask rules can still reach `canUseTool`. On the sidecar, a provider prompt
//! with `matched_ask_rule` gets a card in every mode through `ProviderPrompt::needs_a_human`.
//! The legacy backend refuses under `--print` when no prompt tool is available.
//! This chain has not been verified against the running CLI; the corresponding
//! conformance test remains ignored.
//!
//! Bypass mode is handled before `classify_with_rules` and never reaches this module.
//! The CLI is still launched with `--permission-mode default`, and the `PreToolUse`
//! gate still sees every call.
//!
//! Anything this module cannot classify confidently gets a card. A permissive mistake
//! can grant unsafe access; a restrictive mistake costs a click. `Bash` commands with
//! metacharacters that could redirect, chain, glob, substitute or quote require a card.
//! Matching only the first word would incorrectly allow a command such as `echo hi > c.txt`.
//! `WebFetch` requires a card because the CLI's documentation-domain exemptions are
//! not known here. Unknown tools and malformed input also require a card.
//!
//! `crate::CONSERVATIVE_DISALLOWED_TOOLS` serves a separate purpose: it forbids tools
//! entirely in Bypass mode. This module classifies calls that reach an active gate.

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

/// Tools the CLI asks about every time, decidable from the name alone -- what is left once `Write`,
/// `Edit` and `NotebookEdit` moved to their own conditional check (the acceptEdits fast path, see
/// the module doc and [`EDIT_TOOLS`]).
///
/// `WebFetch` is here for a different reason from the rest -- see the module doc's rule (d): the
/// CLI does exempt some documentation domains, and this version refuses to guess which.
///
/// `MultiEdit` deliberately stays here rather than joining the fast path: it is not on either CLI
/// tool list this module has actually read (`TOOLS_OFFERED_BY_CLI_2_1_272`, or the 2.1.283 `Agent`
/// rename noted beside it), so there is no measurement of how the real CLI treats it under
/// acceptEdits and guessing would be the same permissive error rule (d) already refuses for
/// `WebFetch`.
pub const ALWAYS_ASK_TOOLS: &[&str] = &["MultiEdit", "WebSearch", "WebFetch"];

/// `Write`, `Edit` and `NotebookEdit`: the tools the acceptEdits fast path applies to (module doc,
/// "The acceptEdits fast path"). Not `MultiEdit` -- see [`ALWAYS_ASK_TOOLS`]'s own doc for why.
const EDIT_TOOLS: &[&str] = &["Write", "Edit", "NotebookEdit"];

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
/// A git global option before the subcommand (`-C`, `-c`, `--git-dir`, `--work-tree`,
/// `--namespace`, ...). Excluded from [`REPLACEABLE_BY_A_RULE`]: relocating git can select a
/// repository the policy did not check, so a saved `Bash(git -C *)` rule must not answer it.
pub const REASON_GIT_GLOBAL_OPTION: &str =
    "a git global option before the subcommand can point git at a repository this policy did not check";
/// The work budget ran out (see [`MAX_WORK_PER_CLASSIFICATION`]). Running out always cards.
pub const REASON_TOO_MUCH_WORK: &str =
    "checking this call would need more of the disk read than this policy reads (too many files, links or includes)";

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
    /// The work this classification may do on the filesystem ([`MAX_WORK_PER_CLASSIFICATION`]
    /// outside tests).
    work_budget: usize,
}

/// The one entry point. `tool_name` and `input` are the `PermissionRequested` event's own fields,
/// verbatim; `project_root` is the session's canonical project directory
/// (`eitri_core::project_root`), which is the boundary `Read`/`Grep`/`Glob` are judged against.
///
/// Total: every input produces a verdict, and every path that is not a positive, confident match
/// produces [`PermissionVerdict::AskTheUser`].
pub fn classify_permission_request(tool_name: &str, input: &Value, project_root: &Path) -> Classification {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let surroundings = Surroundings {
        home: home.as_deref(),
        git_location_from_environment: GIT_LOCATION_VARIABLES.iter().any(|v| std::env::var_os(v).is_some()),
        work_budget: MAX_WORK_PER_CLASSIFICATION,
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

/// [`classify_permission_request`] with the home directory and git environment passed in, so
/// boundary checks can be tested without changing `HOME` or `GIT_DIR` for the whole test
/// process.
///
/// The whole classification shares one work budget. Exhausting it requires a card regardless of
/// the result returned by the check that ran out.
fn classify_in(tool_name: &str, input: &Value, project_root: &Path, surroundings: &Surroundings) -> Classification {
    let (classification, ran_out) = with_work_budget(surroundings.work_budget, || {
        classify_within_budget(tool_name, input, project_root, surroundings)
    });
    if ran_out {
        ask(REASON_TOO_MUCH_WORK)
    } else {
        classification
    }
}

fn classify_within_budget(
    tool_name: &str,
    input: &Value,
    project_root: &Path,
    surroundings: &Surroundings,
) -> Classification {
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

    if !matches!(tool_name, "Read" | "Grep" | "Glob" | "Bash") && !EDIT_TOOLS.contains(&tool_name) {
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
        // `path` is optional for both tools; omission means the working directory, inside the root.
        // Only a supplied `path` that the CLI searches as written reaches the containment check
        // in `classify_search_path`.
        "Grep" => classify_search_path(input, &root),
        // Check Glob's `pattern` as well as its optional `path`. A missing `path` does not confine
        // an absolute pattern or one containing `..` to the project root. Ask for permission when the
        // pattern cannot be confined, without relying on how the CLI interprets it.
        "Glob" => match input.get("pattern") {
            Some(Value::String(pattern))
                if pattern.starts_with('/') || pattern.starts_with('~') || pattern.contains("..") =>
            {
                ask("the glob pattern itself reaches outside the project")
            }
            Some(Value::String(_)) => classify_search_path(input, &root),
            _ => ask("this Glob call has no string `pattern`"),
        },
        // The acceptEdits fast path (module doc). `file_path` is `Write`/`Edit`'s own schema field;
        // `NotebookEdit` names its target `notebook_path` instead.
        "Write" | "Edit" => classify_edit_target(input, "file_path", &root),
        "NotebookEdit" => classify_edit_target(input, "notebook_path", &root),
        _ => classify_bash(input, &root, surroundings),
    }
}

/// Returns the canonical project root, or a card if it cannot serve as a boundary.
///
/// An unresolvable root cannot bound any target. The home directory and its ancestors,
/// including `/`, are also rejected: otherwise credentials under the home directory
/// would count as project files. With no explicit directory, `project_root` falls back
/// to the process cwd. The packaged `.desktop` entry passes `%f`, which is empty for an
/// app-menu launch. Such a launch could make `$HOME` the project, though its actual cwd
/// has not been observed. If `home` is unset or unresolvable, `/` is still rejected.
///
/// Roots under `/proc`, `/sys` or `/dev` are rejected because their links can resolve
/// differently in the application and CLI processes; see [`KERNEL_TREES`].
///
/// The root must still resolve to its original canonical path. If it is renamed and
/// replaced by a link, accepting the link's destination would change the boundary
/// while the CLI could still be in the original directory. Path-based calls require
/// a card until the root resolves to itself again.
///
/// This compares paths, not directory identity. Replacing the root with a real directory
/// under the same name is not detected. The CLI also uses that path string for its cwd,
/// so both processes address the replacement. `Path` compares components, treating a
/// trailing `/` or an embedded `.` as the same path.
fn boundary(project_root: &Path, home: Option<&Path>) -> Result<PathBuf, Classification> {
    let Ok(root) = project_root.canonicalize() else {
        return Err(ask("the project root itself could not be resolved"));
    };
    if root.parent().is_none() {
        return Err(ask("the project root is the filesystem root, which is no boundary"));
    }
    if is_in_a_kernel_tree(&root) {
        return Err(ask(
            "the project root is in /proc, /sys or /dev, whose links lead somewhere different in each process",
        ));
    }
    if root != project_root {
        return Err(ask(REASON_ROOT_NO_LONGER_ITSELF));
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

/// Why a project root that no longer resolves to itself cards. Not in
/// [`REPLACEABLE_BY_A_RULE`].
const REASON_ROOT_NO_LONGER_ITSELF: &str =
    "the project root no longer resolves to itself: it was moved, or a link now stands in its place";

enum PathField {
    Required,
    Optional,
}

/// Allows `Read`, `Grep` and `Glob` only when the target resolves inside the project root.
///
/// Malformed, empty, missing-on-disk or unresolvable targets require a card, as do paths
/// that resolve outside the root. The caller has already checked the root with [`boundary`].
///
/// Resolution must precede containment checks because textual comparisons can miss
/// `..` traversal and symlinks. [`resolve_in_root`] bounds the work on links controlled
/// by the project. It refuses lookups under `/proc`, `/sys` or `/dev`, links outside the
/// root and links that leave it on the way, even if they return inside. The CLI opens
/// paths in its own process, whose cwd, root and descriptors can differ from this one's.
///
/// Physical resolution is valid only for the spelling the CLI opens. `Read` hook input
/// already uses the CLI's `expandPath` spelling. `Grep` and `Glob` instead pass through
/// [`classify_search_path`] before reaching this check.
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

    // Resolution is bounded because the project controls the link chain and can make it costly.
    // `resolve_in_root` refuses paths through `/proc`, `/sys` or `/dev` and links outside
    // or leaving the root, even if the final target would be inside.
    let resolved = match resolve_in_root(root, Path::new(raw)) {
        Resolution::At(resolved) => resolved,
        Resolution::Missing | Resolution::Unexaminable => return ask("the path could not be resolved on disk"),
        Resolution::Refused => return ask("the path's links could not be resolved within this policy's limits"),
        Resolution::ThroughAKernelTree => return ask(REASON_THROUGH_A_KERNEL_TREE),
        Resolution::LinkLeavesTheRoot => return ask(REASON_LINK_LEAVES_THE_ROOT),
    };
    if resolved.starts_with(root) {
        allow("the path resolves inside the project root")
    } else {
        ask("the path resolves outside the project root")
    }
}

/// Why a `Grep`/`Glob` `path` the CLI would search under another spelling cards. Not
/// in [`REPLACEABLE_BY_A_RULE`].
const REASON_SEARCH_PATH_RESPELLED: &str = "the CLI searches this path under another spelling \
     (it trims it, expands a leading `~`, and resolves `..` by name), so this policy cannot judge it";

/// Checks `Grep` and `Glob` paths with [`classify_path`] only when the CLI searches them as written.
///
/// CLI 2.1.283 does not apply `backfillObservableInput` to these tools, so their hook
/// input retains the model's spelling. The tool's `expandPath` trims whitespace,
/// expands `~` or `~/` from the home directory and settles `..` lexically. Physical
/// resolution here could therefore check a different target from the CLI's search.
/// For example, `d/../..` with `d -> sub/deeper` can resolve physically to the project
/// root while lexical normalization points to its parent. A project directory named
/// `~` or a name with leading whitespace can cause similar mismatches.
///
/// Paths with a `..` component, leading `~` or whitespace at either end require a card.
/// Whitespace includes `char::is_whitespace` and U+FEFF, covering JavaScript's `trim`.
/// Remaining paths are searched as `cwd/path` or an absolute path, with only `.` and
/// repeated `/` settled, matching [`resolve_in_root`]. Neither backend enables these
/// tools on CLI 2.1.283, which requires `--tools` or `--allowedTools` to offer them.
/// `Read` and edit tools already supply the normalized spelling in their hook input.
fn classify_search_path(input: &Value, root: &Path) -> Classification {
    if let Some(Value::String(raw)) = input.get("path") {
        if search_path_is_respelled_by_the_cli(raw) {
            return ask(REASON_SEARCH_PATH_RESPELLED);
        }
    }
    classify_path(input, "path", PathField::Optional, root)
}

/// See [`classify_search_path`].
fn search_path_is_respelled_by_the_cli(raw: &str) -> bool {
    let trimmed = |c: char| c.is_whitespace() || c == '\u{feff}';
    raw.starts_with(trimmed)
        || raw.ends_with(trimmed)
        || raw.starts_with('~')
        || Path::new(raw)
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
}

// the acceptEdits fast path

/// Claude Code 2.1.283's own protected-directory names, matched case-insensitively at ANY depth in
/// the resolved path -- not only as the target's immediate parent, and not only relative to the
/// project root. Read out of the CLI's own minified bundle, not memory:
///
/// ```text
/// $ grep -o 'NGn as DANGEROUS_DIRECTORIES' /scratch/auto-parity/cli-2.1.283.strings
/// export{NGn as DANGEROUS_DIRECTORIES, ...}
/// $ grep -o 'var NGn=\[[^]]*\]' /scratch/auto-parity/cli-2.1.283.strings
/// var NGn=[".git",".vscode",".idea",".claude",".husky",".cargo",".devcontainer",".yarn",".mvn"]
/// ```
///
/// `NGn` is consumed by the function the CLI's own `checkPathSafetyForAutoEdit` (minified `t5`,
/// the acceptEdits safety check itself -- confirmed by reading its body, which calls
/// `for (let R of w) if (Ff(R,h,g)) return {safe:false, message: "...which is a sensitive file."}`)
/// calls on every resolved candidate path: it scans EVERY path segment against this list, matched
/// case-insensitively at any position, so `foo/.git/bar` and `.git/hooks/x` are both protected,
/// not only a literal top-level `.git/`. Reproduced here exactly, with one deliberate omission --
/// see the module doc's "acceptEdits fast path" section for why `.claude/skills`, `.claude/agents`,
/// `.claude/commands`, `.claude/scheduled_tasks.json` and `.claude/worktrees` are NOT carved back
/// out here the way the CLI's own `Ff` carves them out for its personalization features.
const DANGEROUS_DIRECTORIES: &[&str] = &[
    ".git",
    ".vscode",
    ".idea",
    ".claude",
    ".husky",
    ".cargo",
    ".devcontainer",
    ".yarn",
    ".mvn",
];

/// One more relative sequence the CLI's own `Ff` protects at any position, found declared beside
/// `NGn` in the very same bundle chunk: `var Sf=[".config/git"]`. Two contiguous segments, not a
/// single directory name, so it is checked separately from [`DANGEROUS_DIRECTORIES`]. Extra
/// fidelity beyond the written list in the module doc, included because it only ever cards
/// MORE often, never less.
const DANGEROUS_RELATIVE_PATH: [&str; 2] = [".config", "git"];

/// Claude Code 2.1.283's protected FILE basenames -- matched against the target's own last path
/// segment only, case-insensitively, regardless of which directory holds it. Found as a whole,
/// small bundle chunk exporting exactly `{kDt, F$}` (`F$` the lowercased `Set` the real check
/// reads):
///
/// ```text
/// $ grep -o 'var kDt=\[[^]]*\]' /scratch/auto-parity/cli-2.1.283.strings | head -1
/// var kDt=[".gitconfig",".gitmodules",".bashrc",".bash_profile",".zshrc",".zprofile",".profile",
///  ".zshenv",".zlogin",".zlogout",".bash_login",".bash_aliases",".bash_logout",".envrc",
///  ".ripgreprc",".mcp.json",".claude.json",".npmrc",".yarnrc",".yarnrc.yml",".pnp.cjs",
///  ".pnp.loader.mjs",".pnpmfile.cjs","bunfig.toml",".bunfig.toml",".bazelrc",".bazelversion",
///  ".bazeliskrc",".pre-commit-config.yaml","lefthook.yml",".lefthook.yml","lefthook.yaml",
///  ".lefthook.yaml","gradle-wrapper.properties","maven-wrapper.properties",".devcontainer.json",
///  "pyrightconfig.json"]
/// ```
///
/// `kDt` is the exact identifier the CLI's own `Ff` reads at its basename check (`if(h){let
/// P=Ki(h);if(kDt.some((R)=>to(R)===P))return!0}`, `h` being the resolved path's last segment) --
/// the same `Ff` that backs `checkPathSafetyForAutoEdit`, cited above `DANGEROUS_DIRECTORIES`.
const PROTECTED_EDIT_FILE_BASENAMES: &[&str] = &[
    ".gitconfig",
    ".gitmodules",
    ".bashrc",
    ".bash_profile",
    ".zshrc",
    ".zprofile",
    ".profile",
    ".zshenv",
    ".zlogin",
    ".zlogout",
    ".bash_login",
    ".bash_aliases",
    ".bash_logout",
    ".envrc",
    ".ripgreprc",
    ".mcp.json",
    ".claude.json",
    ".npmrc",
    ".yarnrc",
    ".yarnrc.yml",
    ".pnp.cjs",
    ".pnp.loader.mjs",
    ".pnpmfile.cjs",
    "bunfig.toml",
    ".bunfig.toml",
    ".bazelrc",
    ".bazelversion",
    ".bazeliskrc",
    ".pre-commit-config.yaml",
    "lefthook.yml",
    ".lefthook.yml",
    "lefthook.yaml",
    ".lefthook.yaml",
    "gradle-wrapper.properties",
    "maven-wrapper.properties",
    ".devcontainer.json",
    "pyrightconfig.json",
];

/// Classifies `Write`, `Edit` and `NotebookEdit` using the path input named by `field`.
/// The `acceptEdits` fast path allows a target only when both its requested and resolved
/// paths are inside `root`, neither spelling matches [`DANGEROUS_DIRECTORIES`],
/// [`DANGEROUS_RELATIVE_PATH`] or [`PROTECTED_EDIT_FILE_BASENAMES`], and the target
/// is not a hard-linked file.
///
/// New targets resolve through their nearest existing ancestor with
/// [`resolve_missing_edit_target`]. This allows new files in unprotected project
/// directories while catching ancestor symlinks that leave the root.
///
/// Raw paths containing `..` or starting with `~` require a card. Applying a missing
/// tail lexically can otherwise cancel a nonexistent component and skip resolving an
/// existing symlink or checking a hard link, as in `nx/../escape/x` with `nx` absent.
/// For these three tools, the CLI normalizes hook input through
/// `backfillObservableInput`: trimming, expanding `~` and applying `path.resolve`.
/// Rejecting these raw spellings therefore does not reject the CLI's normalized input.
///
/// Protected names are checked on the requested, root-joined spelling before any
/// filesystem work, as well as on the resolved spelling. Otherwise, a protected path
/// such as `/project/.npmrc` linked to `/project/config/npm-user.conf` could pass
/// because the resolved basename alone is unprotected.
///
/// Containment also applies to both spellings. A path outside the root can resolve
/// inside through a link controlled outside the project; retargeting that link could
/// send the later write elsewhere. CLI 2.1.283's own fast path requires both spellings
/// to be inside a working directory. A hook `allow` bypasses the ordinary working-directory
/// ask, so this policy must enforce that boundary itself.
///
/// Both spellings being inside is insufficient for process-dependent links. The
/// application resolves the target, but the CLI child writes it with the project root
/// as cwd. A link through `/proc/self/cwd/..` can point to different directories in
/// those processes. [`resolve_in_root`] refuses lookups under `/proc`, `/sys` or `/dev`
/// and links outside or leaving the root, even if they return inside. Read paths use
/// the same restriction, including for links that sit outside the root.
fn classify_edit_target(input: &Value, field: &str, root: &Path) -> Classification {
    let raw = match input.get(field) {
        Some(Value::String(s)) if !s.is_empty() => s,
        Some(Value::String(_)) => return ask("this tool's path field is empty"),
        None | Some(Value::Null) => return ask("this tool's required path field is missing"),
        Some(_) => return ask("this tool's path field is not a string"),
    };

    if edit_target_raw_path_is_suspect(raw) {
        return ask(
            "the path contains `..` or starts with `~` -- the real CLI's own hook never hands this \
             policy a path shaped like that for this tool, so this policy does not try to resolve \
             one itself",
        );
    }

    let requested = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        root.join(raw)
    };
    // Both spellings must be inside, as in the CLI's own `zy`: contained as written
    // here -- lexically, which is exact since `..` was carded above -- and as resolved below.
    if !requested.starts_with(root) {
        return ask("the path as written is outside the project root, wherever it leads today");
    }
    if path_is_protected(&requested) {
        return ask("the requested path names one of Claude Code's protected paths or files");
    }

    let target = match resolve_edit_target(root, Path::new(raw)) {
        Ok(target) => target,
        Err(reason) => return ask(reason),
    };

    if !target.logical.starts_with(root) {
        return ask("the path resolves outside the project root");
    }
    if target.is_hard_link {
        return ask("the target is a hard-linked file (more than one path names the same data)");
    }
    if path_is_protected(&target.logical) {
        return ask("the target is one of Claude Code's protected paths or files");
    }
    allow("Claude Code's acceptEdits mode writes here without asking (in-project, not a protected path)")
}

/// Whether `raw` -- an edit tool's own path field, exactly as given, before any resolution -- is a
/// shape this policy refuses to reason about itself rather than a shape the real CLI would ever
/// hand its hook for `Write`/`Edit`/`NotebookEdit` (see [`classify_edit_target`]'s own doc for why
/// that makes this free): any `..` path component, or a leading `~`. A `~` embedded elsewhere in the
/// path (not the leading character) is left alone -- the CLI's own tilde expansion is a whole-string
/// prefix match, not a general substitution, so a literal `~` mid-path is an ordinary filename
/// character there too.
fn edit_target_raw_path_is_suspect(raw: &str) -> bool {
    raw.starts_with('~')
        || Path::new(raw)
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
}

/// One [`classify_edit_target`] target, resolved.
struct EditTarget {
    /// The canonical path, if it exists; otherwise the nearest existing ancestor's canonical path
    /// with the missing tail joined on literally (there is nothing on disk there yet to resolve).
    logical: PathBuf,
    /// Whether the target, as it exists today, has more than one hard link. Always `false` for a
    /// target that does not exist yet -- a nonexistent path has no links to count.
    is_hard_link: bool,
}

/// Why an edit target did not resolve: past this policy's limits, or behind a lookup that failed.
const REASON_EDIT_TARGET_PAST_LIMITS: &str = "the path's links could not be resolved within this policy's limits";
const REASON_EDIT_TARGET_UNEXAMINABLE: &str =
    "a directory or name on the way could not be examined, so where the path leads is unknown";

fn resolve_edit_target(root: &Path, raw: &Path) -> Result<EditTarget, &'static str> {
    match resolve_in_root(root, raw) {
        Resolution::At(resolved) => {
            // `resolve`'s own contract: `At` has every symlink already followed, so `resolved`
            // itself is never a symlink and a plain `metadata` read (not `symlink_metadata`) is
            // exactly the file this policy is judging.
            let is_hard_link = std::fs::metadata(&resolved)
                .map(|meta| meta.is_file() && std::os::unix::fs::MetadataExt::nlink(&meta) > 1)
                .unwrap_or(false);
            Ok(EditTarget {
                logical: resolved,
                is_hard_link,
            })
        }
        Resolution::Refused => Err(REASON_EDIT_TARGET_PAST_LIMITS),
        // An unexaminable path is not a missing target: a link behind it has not been checked.
        Resolution::Unexaminable => Err(REASON_EDIT_TARGET_UNEXAMINABLE),
        // Kernel-tree links may lead to a different place in the CLI process.
        Resolution::ThroughAKernelTree => Err(REASON_THROUGH_A_KERNEL_TREE),
        Resolution::LinkLeavesTheRoot => Err(REASON_LINK_LEAVES_THE_ROOT),
        Resolution::Missing => resolve_missing_edit_target(root, raw),
    }
}

/// Resolves the nearest existing ancestor of `raw` and applies the missing tail lexically.
/// `.` and `..` are collapsed against that ancestor so a suffix that walks out of it is
/// not mistaken for staying inside. Prefixes are tried from longest to shortest, ending
/// at the empty relative path, which resolves to `root`.
/// [`classify_edit_target`] rejects `..` before calling this function. The `ParentDir` arm
/// still handles direct calls, including tests, but lexical tail handling cannot check
/// links exposed by cancelling a missing component.
/// Each attempt rebuilds and resolves a prefix, so a missing first component can cause
/// O(components²) work on the GTK thread. [`MAX_RESOLUTION_STEPS`] caps the component count
/// before the first `PathBuf` is built, bounding work outside [`resolve`]'s own budget.
fn resolve_missing_edit_target(root: &Path, raw: &Path) -> Result<EditTarget, &'static str> {
    let components: Vec<std::path::Component> = raw.components().collect();
    if components.len() > MAX_RESOLUTION_STEPS {
        return Err(REASON_EDIT_TARGET_PAST_LIMITS);
    }
    for prefix_len in (0..components.len()).rev() {
        let prefix: PathBuf = components[..prefix_len].iter().collect();
        // `resolve_in_root` rather than `resolve` is defensive, like the arms below
        //: the full path came
        // back `Missing` from `resolve_in_root`, so every step a prefix repeats has already passed
        // its kernel-tree and link checks, and with `resolve` here every test still passes.
        match resolve_in_root(root, &prefix) {
            Resolution::At(resolved) => {
                let mut logical = resolved;
                for component in &components[prefix_len..] {
                    match component {
                        std::path::Component::Normal(part) => logical.push(part),
                        std::path::Component::ParentDir => {
                            logical.pop();
                        }
                        // `CurDir`, and a `RootDir`/`Prefix` that can only ever appear at index 0,
                        // already consumed by `prefix` -- neither changes `logical`.
                        _ => {}
                    }
                }
                return Ok(EditTarget {
                    logical,
                    is_hard_link: false,
                });
            }
            Resolution::Refused => return Err(REASON_EDIT_TARGET_PAST_LIMITS),
            // Defensive: the full path came back `Missing`, so every lookup on the way to what was
            // missing succeeded, and a prefix repeats those lookups -- only a directory changing
            // between the two can land here (a mutation to `continue` survives the tests for that
            // reason). [`resolve_edit_target`] cards the reachable case before this is called.
            Resolution::Unexaminable => return Err(REASON_EDIT_TARGET_UNEXAMINABLE),
            // Defensive for the same reason: the full path met no such step before
            // what was missing, and a prefix takes the same steps.
            Resolution::ThroughAKernelTree => return Err(REASON_THROUGH_A_KERNEL_TREE),
            Resolution::LinkLeavesTheRoot => return Err(REASON_LINK_LEAVES_THE_ROOT),
            Resolution::Missing => continue,
        }
    }
    // Unreachable in practice -- `prefix_len == 0` above always resolves to `root` -- but a card
    // rather than a panic if it somehow is not.
    Err(REASON_EDIT_TARGET_PAST_LIMITS)
}

/// Whether `path`, requested or resolved, names a protected directory, relative sequence,
/// or file in [`DANGEROUS_DIRECTORIES`], [`DANGEROUS_RELATIVE_PATH`] or
/// [`PROTECTED_EDIT_FILE_BASENAMES`]. The edit classifier checks both spellings.
/// Matching is case-insensitive and uses [`fold_confusables`], so dotless i, long s,
/// and invisible or bidi-control characters cannot hide an otherwise protected name.
/// DOS 8.3 names, NTFS alternate-data-stream suffixes and trailing dots or spaces are
/// not normalized here because this product's filesystem calls target Linux and macOS.
fn path_is_protected(path: &Path) -> bool {
    let segments: Vec<String> = path
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => Some(fold_confusables(&part.to_string_lossy()).to_lowercase()),
            _ => None,
        })
        .collect();

    if segments
        .iter()
        .any(|segment| DANGEROUS_DIRECTORIES.contains(&segment.as_str()))
    {
        return true;
    }
    if segments
        .windows(2)
        .any(|pair| pair[0] == DANGEROUS_RELATIVE_PATH[0] && pair[1] == DANGEROUS_RELATIVE_PATH[1])
    {
        return true;
    }
    match segments.last() {
        Some(last) => PROTECTED_EDIT_FILE_BASENAMES.contains(&last.as_str()),
        None => false,
    }
}

/// A fold of the confusable characters [`path_is_protected`] matches before lowercasing -- read
/// directly off the CLI's own bundle ([codex], "important", verified against
/// `/scratch/auto-parity/cli-2.1.283.strings` rather than taken on the finding's word alone).
/// Deliberately spelled out by codepoint number below rather than pasted as a literal regex, since
/// the literal characters are exactly what this function exists not to let slide through unnoticed
/// in source text.
///
/// The CLI's own minified source (`grep -o 'v\$t=/\[[^]]*\]/' cli-2.1.283.strings`) declares
/// `v$t` as a character class covering four codepoint ranges -- U+200C through U+200F (ZWNJ, ZWJ,
/// LRM, RLM), U+202A through U+202E (the bidi embedding/override controls), U+206A through U+206F
/// (the deprecated symmetric-swapping format characters) and U+FEFF (BOM / zero-width no-break
/// space) -- and its fold function (exported as `Ki`/`kDo`, found beside `v$t` in the same chunk)
/// lowercases, maps Turkish dotless i (U+0131) and long s (U+017F) to their ASCII look-alikes with
/// `.replace(...)`, then strips every character `v$t` matches with `.replace(g,"")` before (mode
/// `Ki`) optionally trimming an NTFS `:stream` suffix and trailing dots/spaces. The four ranges and
/// the two letter mappings are all reproduced here; the NTFS/trailing-dot half is not -- a Windows
/// filesystem behaviour this Linux/macOS-only product cannot be exploited through
/// ([`path_is_protected`]'s own doc says so already). Every character this folds can only make a
/// match MORE likely, never less, so the narrower scope is still the safe direction for this
/// module's own always-card-on-uncertainty rule.
fn fold_confusables(segment: &str) -> String {
    segment
        .chars()
        .filter(|c| {
            !matches!(*c,
                '\u{200C}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{206A}'..='\u{206F}' | '\u{FEFF}'
            )
        })
        .map(|c| match c {
            '\u{0131}' => 'i',
            '\u{017F}' => 's',
            other => other,
        })
        .collect()
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
        return ask("an argument resolves outside the project root, or not within this policy's limits");
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
    if program == "diff"
        && arguments
            .iter()
            .any(|a| resolve_existing(root, Path::new(a)).is_some_and(|resolved| resolved.is_dir()))
    {
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
    // Skip only exact `--no-pager` and `-P` options before locating the subcommand. They only
    // disable the pager; abbreviated or combined forms are rejected by git. Leave every other
    // global option in place so it requires a card, including options that relocate or
    // reconfigure git.
    let skipped = arguments
        .iter()
        .take_while(|a| matches!(**a, "--no-pager" | "-P"))
        .count();
    match arguments[skipped..].split_first() {
        Some((sub, rest)) if READ_ONLY_GIT_SUBCOMMANDS.contains(sub) => {
            if git_options_read_only_what_they_name(sub, rest) {
                allow("a read-only git subcommand")
            } else {
                ask(REASON_OPTION_NOT_KNOWN_TO_ONLY_READ)
            }
        }
        // An option remaining after `--no-pager`/`-P` is global. Options such as `-C`,
        // `--git-dir`, `--work-tree` and `--namespace` can select a repository the check above
        // did not inspect. A saved prefix rule must not replace this reason.
        Some((option, _)) if option.starts_with('-') => ask(REASON_GIT_GLOBAL_OPTION),
        _ => ask(REASON_GIT_SUBCOMMAND_NOT_READ_ONLY),
    }
}

/// `-abc`, not `--long` and not a bare `-`.
fn is_short_option_cluster(argument: &str) -> bool {
    argument.len() > 1 && argument.starts_with('-') && !argument.starts_with("--")
}

/// Whether every argument naming an entry under the root resolves inside it. An argument naming
/// no entry, such as a grep pattern or git revision, passes. Existing entries, including
/// dangling symlinks, must resolve inside the root; links out and unresolvable links fail.
///
/// Each suffix of a short-option cluster is checked because an option value may be attached, as
/// in `grep -fescape`. The literal argument is checked too: after `--`, `-n` can name a file
/// rather than an option. Checking only the suffix `n` would miss a symlink named `-n`.
///
/// Each distinct candidate is resolved once by [`resolve`], so repeated arguments cannot
/// repeatedly traverse the same expensive link chain.
fn arguments_stay_inside(arguments: &[&str], root: &Path) -> bool {
    let mut candidates: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for argument in arguments {
        candidates.insert(argument);
        if is_short_option_cluster(argument) {
            candidates.extend((1..argument.len()).filter_map(|i| argument.get(i..)));
        }
    }
    candidates
        .into_iter()
        .all(|candidate| match resolve_in_root(root, Path::new(candidate)) {
            // Unexaminable too: the command runs as this same user and is refused the same lookup.
            Resolution::Missing | Resolution::Unexaminable => true,
            Resolution::At(resolved) => resolved.starts_with(root),
            // The command runs in another process, with the project root as its cwd.
            Resolution::Refused | Resolution::ThroughAKernelTree | Resolution::LinkLeavesTheRoot => false,
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
const REASON_GIT_CONFIG_UNKNOWN_KEY: &str =
    "the repository sets a configuration key this policy cannot confirm is inert for a read-only command";
const REASON_GIT_URL_RUNS_A_PROGRAM: &str =
    "a repository remote URL uses a transport that runs a program (ext::, fd:: or another helper)";
const REASON_GIT_CONFIG_UNREADABLE: &str = "the repository's configuration could not be read the way git reads it";
const REASON_GIT_HOOK: &str = "the repository holds a hook git may run under a read-only command";
const REASON_GIT_HOOKSPATH: &str = "the repository's core.hooksPath points somewhere this policy cannot clear";
const REASON_GIT_ALTERNATES: &str = "the repository reads objects from another repository (objects/info/alternates)";
const REASON_GIT_DIR_ESCAPES: &str = "a symbolic link in the repository's git directory resolves outside both it and \
     the project (or not at all), so git may read outside";
const REASON_GIT_DIR_UNREADABLE: &str = "the repository's git directory could not be read in full";
const REASON_GIT_DIR_TOO_BIG: &str = "the repository's git directory holds too many entries to check";
const REASON_GIT_TOO_MUCH_TO_CHECK: &str = "the repository has too many submodule git directories to check";
const REASON_GIT_DIR_TOO_MANY_LINKS: &str =
    "the repository's git directory holds more symbolic links than this policy resolves";
const REASON_GIT_DIR_NOT_THIS_PROJECTS: &str =
    "the project's .git names a repository outside the project that does not \
     name this project back as its linked worktree or submodule";

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
    let git_dir = git_dir_named_by(root).ok_or(REASON_DOT_GIT_NOT_A_REPOSITORY)?;
    let common = git_directory_git_accepts(&git_dir).ok_or(REASON_DOT_GIT_NOT_A_REPOSITORY)?;
    repository_is_this_projects(&dot_git, &git_dir, &common, root)?;
    repository_runs_nothing_of_its_own(&git_dir, &common, root, surroundings.home)
}

/// Checks that the git directory and common directory belong to this project. A `.git` gitfile,
/// symlink or `commondir` can select an unrelated repository and expose its committed files
/// even if its config runs nothing.
///
/// Both directories may resolve inside the root. If either is outside, the root's `.git` must
/// be a plain gitfile and the outside repository must contain a back-link to this root. A
/// symlinked `.git` is refused.
///
/// For a linked worktree, the git directory is `<common>/worktrees/<name>` and its `gitdir`
/// file must name this root's `.git`, absolutely or relative to that directory. For a
/// submodule, the git directory is its own common directory under `<a git
/// directory>/modules/<path>`, and its config's last `core.worktree` must resolve to this root.
///
/// Other outside layouts and failed reads require cards. This includes `--separate-git-dir`,
/// manually moved worktrees awaiting `git worktree repair`, and submodules whose back-link
/// exists only in `config.worktree` or an include. `git_dir` and `common` are canonical.
fn repository_is_this_projects(dot_git: &Path, git_dir: &Path, common: &Path, root: &Path) -> Result<(), &'static str> {
    if git_dir.starts_with(root) && common.starts_with(root) {
        return Ok(());
    }
    // `root` is canonical, so a `.git` that is a regular file (not a symlink) is canonical too.
    let is_a_gitfile = dot_git.symlink_metadata().is_ok_and(|meta| meta.file_type().is_file());
    if is_a_gitfile
        && (is_a_linked_worktree_of(git_dir, common, dot_git) || is_a_submodule_checked_out_at(git_dir, common, root))
    {
        Ok(())
    } else {
        Err(REASON_GIT_DIR_NOT_THIS_PROJECTS)
    }
}

/// `git_dir` is `<common>/worktrees/<name>` and its `gitdir` back-link names `dot_git`. Both paths
/// canonical.
fn is_a_linked_worktree_of(git_dir: &Path, common: &Path, dot_git: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    if git_dir.parent() != Some(common.join("worktrees").as_path()) {
        return false;
    }
    let Ok(bytes) = read_small_file(&git_dir.join("gitdir")) else {
        return false;
    };
    let named = trim_line_ends(&bytes);
    if named.is_empty() {
        return false;
    }
    resolve_existing(git_dir, Path::new(std::ffi::OsStr::from_bytes(named))).is_some_and(|named| named == dot_git)
}

/// `git_dir` is a submodule's own repository, under a `modules/` directory of a git directory, whose
/// config's last `core.worktree` (no subsection) resolves to `root`. Git resolves a relative one by
/// entering the git directory and then the value, so it is joined to the canonical git directory
/// and canonicalized, as git's own `chdir`s would resolve it. Both paths canonical.
fn is_a_submodule_checked_out_at(git_dir: &Path, common: &Path, root: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    if git_dir != common {
        return false;
    }
    let under_a_superproject = git_dir.ancestors().skip(1).any(|dir| {
        dir.file_name().is_some_and(|name| name == "modules")
            && dir
                .parent()
                .is_some_and(|superproject| git_directory_git_accepts(superproject).is_some())
    });
    if !under_a_superproject {
        return false;
    }
    let Some(variables) = read_small_file(&git_dir.join("config"))
        .ok()
        .and_then(|bytes| parse_git_config(&bytes))
    else {
        return false;
    };
    let Some(value) = variables
        .iter()
        .rev()
        .find(|variable| variable.is_core_worktree() && !variable.has_subsection)
        .and_then(|variable| variable.value.as_deref())
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    resolve_existing(git_dir, Path::new(std::ffi::OsStr::from_bytes(value))).is_some_and(|named| named == root)
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

/// Returns the canonical git directory named by `<root>/.git`: the directory itself or the path
/// in a `gitdir: <path>` file, relative to the root. Both use [`resolve`] to bound link
/// traversal before later lookups. Returns `None` for rejected forms or paths that cannot
/// resolve within bounds.
fn git_dir_named_by(root: &Path) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let dot_git = resolve_existing(root, Path::new(".git"))?;
    let meta = dot_git.metadata().ok()?;
    if meta.is_dir() {
        return Some(dot_git);
    }
    if !meta.is_file() {
        return None;
    }
    let bytes = read_small_file(&dot_git).ok()?;
    let named = trim_line_ends(bytes.strip_prefix(b"gitdir: ")?);
    if named.is_empty() {
        return None;
    }
    resolve_existing(root, Path::new(std::ffi::OsStr::from_bytes(named)))
}

/// `git_dir`'s common directory, if git's `is_git_directory` would accept it -- and only then,
/// because git keeps walking up past a `.git` it does not accept (measured 2026-09-28: an empty
/// `.git`, a `HEAD` alone, `objects/` at mode 000 and a garbage `HEAD` each sent `git rev-parse
/// --show-toplevel` to the repository above). At least as strict as git: a `HEAD` naming `refs/`
/// (as `ref: refs/...` or a symlink) or a full object id, and `objects/` and `refs/` directories this
/// process may search, in the directory `commondir` names if there is one.
fn git_directory_git_accepts(git_dir: &Path) -> Option<PathBuf> {
    if !head_names_a_ref_or_an_object(&git_dir.join("HEAD")) {
        return None;
    }
    let common = commondir_of(git_dir).ok()?;
    ["objects", "refs"]
        .iter()
        .all(|name| is_searchable_directory(&common.join(name)))
        .then_some(common)
}

/// Returns the common directory named by `gitdir/commondir`, relative to the git directory, or
/// the git directory itself when that file is absent. Empty or unreadable `commondir` files are
/// errors. Both the top repository and each submodule need this check because a different
/// common directory may contain config that runs a program.
fn commondir_of(git_dir: &Path) -> Result<PathBuf, &'static str> {
    use std::os::unix::ffi::OsStrExt;
    match read_small_file(&git_dir.join("commondir")) {
        Ok(bytes) => {
            let named = trim_line_ends(&bytes);
            if named.is_empty() {
                return Err(REASON_GIT_CONFIG_UNREADABLE);
            }
            resolve_existing(git_dir, Path::new(std::ffi::OsStr::from_bytes(named))).ok_or(REASON_GIT_CONFIG_UNREADABLE)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(git_dir.to_path_buf()),
        Err(_) => Err(REASON_GIT_CONFIG_UNREADABLE),
    }
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

/// Charges the work budget even for a missing file. Budget exhaustion returns an error other
/// than `NotFound`, so callers cannot mistake it for absence.
fn read_small_file(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    if !spend(WORK_PER_FILE) {
        return Err(std::io::Error::other("out of work"));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_GIT_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_GIT_FILE_BYTES {
        return Err(std::io::Error::other("larger than a git file this policy reads"));
    }
    if !spend(bytes.len() / 1024 * WORK_PER_KIB) {
        return Err(std::io::Error::other("out of work"));
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

// Classification runs on the GTK main loop, so unbounded path resolution can freeze the window.
// The project controls paths and links in its tree and `.git`.
//
// [`resolve`] reads each link before following it and limits target length, components, `..`,
// hops and steps. Each lookup uses an already resolved prefix, so the kernel does not follow
// links on its behalf.
//
// Every lookup, directory entry and file read also spends from one classification budget.
// Exhausting it requires a card regardless of which check ran out.

/// The work one classification may do, in units of about one directory entry listed (~0.16 us on
/// this host, release build). Calibrated 2026-09-28 so that a classification spending all of it
/// takes about 40 ms in a release build; of the repositories measured, the
/// largest repository spends about 4,000.
const MAX_WORK_PER_CLASSIFICATION: usize = 200_000;
/// One component looked up (`lstat`) or one link read (`readlink`) by [`resolve`].
const WORK_PER_LOOKUP: usize = 16;
/// One directory entry listed.
const WORK_PER_ENTRY: usize = 1;
/// One file opened and read by [`read_small_file`], plus this much per KiB read.
const WORK_PER_FILE: usize = 20;
const WORK_PER_KIB: usize = 48;

thread_local! {
    static WORK_LEFT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static RAN_OUT_OF_WORK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Runs `check` with `budget` units of work, and says whether it ran out. Outside such a scope the
/// budget is zero, so a check that is not run through here fails closed.
fn with_work_budget<T>(budget: usize, check: impl FnOnce() -> T) -> (T, bool) {
    let saved_left = WORK_LEFT.replace(budget);
    let saved_ran_out = RAN_OUT_OF_WORK.replace(false);
    let result = check();
    let ran_out = RAN_OUT_OF_WORK.replace(saved_ran_out);
    WORK_LEFT.set(saved_left);
    (result, ran_out)
}

/// Takes `units` from the budget; `false`, for good, once it is spent.
fn spend(units: usize) -> bool {
    match WORK_LEFT.get().checked_sub(units) {
        Some(left) if !RAN_OUT_OF_WORK.get() => {
            WORK_LEFT.set(left);
            true
        }
        _ => {
            WORK_LEFT.set(0);
            RAN_OUT_OF_WORK.set(true);
            false
        }
    }
}

/// The most symbolic links a git directory may hold before it cards without any of them being
/// followed. Six measured repositories hold none; a hook symlinked into the
/// project is one per hook, and git knows 28 hook names.
const MAX_GIT_DIRECTORY_SYMLINKS: usize = 64;
/// Limits on ONE link's target, read with `readlink` before it is followed, on every hop. A hook
/// symlinked into the project (`../../scripts/pre-commit`, or from a nested submodule's git
/// directory seven `..` deep) sits far inside them; the review's padded hops (~4000 bytes, ~800
/// `..`) do not. Components are counted as written, `.` included -- each costs a lookup.
const MAX_LINK_TARGET_BYTES: usize = 1024;
const MAX_LINK_TARGET_COMPONENTS: usize = 64;
const MAX_LINK_TARGET_PARENTS: usize = 16;
/// Links followed in one resolution. An ordinary path meets zero or one.
const MAX_LINK_HOPS: usize = 8;
/// Components looked up in one resolution, the path's own and every target's.
const MAX_RESOLUTION_STEPS: usize = 256;
/// The deepest resolved path a lookup is made under: each `lstat` walks its prefix again in the
/// kernel, so this bounds what one lookup costs.
const MAX_RESOLVED_DEPTH: usize = 128;

/// The kernel's own trees. What a path through one of them names depends
/// on WHICH process looks it up -- `/proc/self`, `/proc/thread-self`, `/proc/<pid>/cwd`,
/// `/proc/<pid>/root`, `/proc/<pid>/fd/<n>`, `/dev/fd` and `/dev/stdin` all lead somewhere
/// different in Eitri, which judges a path, and in the CLI child, which writes or reads it -- so no
/// resolution here looks anything up under one of them at all ([`Resolution::ThroughAKernelTree`]).
/// No per-process link is known in `/sys` or `/dev/shm`; they are refused with the rest rather than
/// sorted entry by entry, which would be the partial reading this module refuses.
const KERNEL_TREES: [&str; 3] = ["/proc", "/sys", "/dev"];

fn is_in_a_kernel_tree(path: &Path) -> bool {
    KERNEL_TREES.iter().any(|tree| path.starts_with(tree))
}

/// Why a path that passes through [`KERNEL_TREES`] cards. Not in
/// [`REPLACEABLE_BY_A_RULE`].
const REASON_THROUGH_A_KERNEL_TREE: &str =
    "a path passes through /proc, /sys or /dev, whose links lead somewhere different in each process";
/// Why a path through a link that leaves the root cards, wherever it ends.
const REASON_LINK_LEAVES_THE_ROOT: &str =
    "a link on the way sits outside the project root or leads out of it, even if the path comes back in";

/// What [`resolve`] found.
#[derive(Debug, PartialEq, Eq)]
enum Resolution {
    /// The path, canonical: every link followed, no `.` or `..` left.
    At(PathBuf),
    /// Nothing is there, as named: a component of the path itself does not exist (or is not a
    /// directory). A dangling link AT the end of the path is not this, but [`Resolution::Refused`].
    Missing,
    /// A component could not be examined, for example because directory search was denied
    /// or a name was too long. The acceptEdits fallback cards instead of treating this as
    /// missing: an unexamined directory could hide a link outside the root.
    /// Other callers treat this like `Missing`: `Read` cards either way, while Bash,
    /// git includes and submodule lookups run as the same user and fail the same lookup.
    Unexaminable,
    /// It would not resolve within the limits above, a link's target would not read, the budget
    /// ran out, or the path ends in a link that leads nowhere.
    Refused,
    /// A component to look up lay under [`KERNEL_TREES`], so where the path
    /// leads depends on the process resolving it. Returned by every resolution, whatever it is for.
    ThroughAKernelTree,
    /// Only from [`resolve_in_root`]: a link met on the way sits outside the root, or
    /// once a link was followed a step left the root -- whether or not the path then comes back in.
    LinkLeavesTheRoot,
}

/// Whether a link's target is small enough to follow. See [`MAX_LINK_TARGET_BYTES`].
fn link_target_is_modest(target: &[u8]) -> bool {
    let parts = target.split(|b| *b == b'/').filter(|part| !part.is_empty());
    target.len() <= MAX_LINK_TARGET_BYTES
        && parts.clone().count() <= MAX_LINK_TARGET_COMPONENTS
        && parts.filter(|part| *part == b"..").count() <= MAX_LINK_TARGET_PARENTS
}

/// The components of a path as the kernel reads them, `.` dropped, in reverse (a stack), each
/// marked with whether it is one of the path's own (`true`) or a link target's.
fn components_to_resolve(path: &[u8], own: bool) -> Vec<(Vec<u8>, bool)> {
    path.split(|b| *b == b'/')
        .filter(|part| !part.is_empty() && *part != b".")
        .rev()
        .map(|part| (part.to_vec(), own))
        .collect()
}

/// Resolves `path` physically from canonical `base` when relative, within the limits
/// above and the work budget. Each component is looked up with `lstat` under the
/// resolved prefix; links are read with `readlink` and checked by [`link_target_is_modest`]
/// before following them, so the kernel does not follow unchecked links here.
/// [`Resolution::Missing`] means a component is absent or is not a directory.
/// A dangling final link yields [`Resolution::Refused`]; other lookup failures yield
/// [`Resolution::Unexaminable`].
/// Nothing under [`KERNEL_TREES`] is looked up: those paths yield
/// [`Resolution::ThroughAKernelTree`] because their links can resolve differently in
/// this process and the CLI child or git. Paths judged against the project root use
/// [`resolve_in_root`], which also confines links.
fn resolve(base: &Path, path: &Path) -> Resolution {
    resolve_confined(base, path, None)
}

/// Resolves a project path with `root` as both base and boundary for `Read`, `Grep`,
/// `Glob`, edits and Bash arguments. A link encountered outside the root is refused.
/// After following any link, each step must remain inside the root or on its ancestors,
/// even if an escaping path would later return ([`Resolution::LinkLeavesTheRoot`]).
/// Ancestors are allowed so absolute targets can walk down from `/`; any link there
/// is still outside the root and cards. Before following a link, physical `..` steps
/// may leave the root, and the caller checks containment of the final path.
/// The policy and CLI child can have different working directories, so a link through
/// `/proc/self/cwd` can resolve inside here and outside for the child. [`KERNEL_TREES`]
/// rejects such paths; confining links also avoids relying on paths outside the project
/// that could change between classification and use.
fn resolve_in_root(root: &Path, path: &Path) -> Resolution {
    resolve_confined(root, path, Some(root))
}

/// [`resolve`], and with `root` given, [`resolve_in_root`].
fn resolve_confined(base: &Path, path: &Path, root: Option<&Path>) -> Resolution {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let mut resolved = if path.is_absolute() {
        PathBuf::from("/")
    } else {
        base.to_path_buf()
    };
    // Defensive check: callers already confine `base`. `resolve_in_root` starts at a root
    // checked by `boundary`; an absolute path starts at `/`. Other callers start at resolved
    // git directories or real subdirectories reached without entering `/proc`, `/sys` or
    // `/dev`.
    if is_in_a_kernel_tree(&resolved) {
        return Resolution::ThroughAKernelTree;
    }
    let mut pending = components_to_resolve(path.as_os_str().as_bytes(), true);
    let mut own_left = pending.len();
    let (mut steps, mut hops, mut following_the_last) = (0, 0, false);
    while let Some((part, own)) = pending.pop() {
        steps += 1;
        if steps > MAX_RESOLUTION_STEPS || !spend(WORK_PER_LOOKUP) {
            return Resolution::Refused;
        }
        if own {
            own_left -= 1;
        }
        if part == b".." {
            // The parent of a step inside the root, or on its ancestors, is one too.
            resolved.pop();
            continue;
        }
        let candidate = resolved.join(std::ffi::OsStr::from_bytes(&part));
        if candidate.components().count() > MAX_RESOLVED_DEPTH {
            return Resolution::Refused;
        }
        // Reject kernel-tree paths before any filesystem lookup because their links can depend on the process.
        if is_in_a_kernel_tree(&candidate) {
            return Resolution::ThroughAKernelTree;
        }
        if let Some(root) = root {
            if hops > 0 && !candidate.starts_with(root) && !root.starts_with(&candidate) {
                return Resolution::LinkLeavesTheRoot;
            }
        }
        let meta = match candidate.symlink_metadata() {
            Ok(meta) => meta,
            // Only "not found" and "not a directory" say nothing is there.
            // Any other failure -- a directory on the way that may not be searched, a name too long
            // to look up -- says nothing about what IS there.
            Err(e) if !following_the_last => {
                return match e.kind() {
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => Resolution::Missing,
                    _ => Resolution::Unexaminable,
                }
            }
            Err(_) => return Resolution::Refused,
        };
        if !meta.file_type().is_symlink() {
            resolved = candidate;
            continue;
        }
        if root.is_some_and(|root| !candidate.starts_with(root)) {
            return Resolution::LinkLeavesTheRoot;
        }
        hops += 1;
        if hops > MAX_LINK_HOPS || !spend(WORK_PER_LOOKUP) {
            return Resolution::Refused;
        }
        if own && own_left == 0 {
            following_the_last = true;
        }
        let Ok(target) = std::fs::read_link(&candidate) else {
            return Resolution::Refused;
        };
        let target = target.into_os_string().into_vec();
        if !link_target_is_modest(&target) {
            return Resolution::Refused;
        }
        if target.first() == Some(&b'/') {
            resolved = PathBuf::from("/");
        }
        pending.extend(components_to_resolve(&target, false));
    }
    Resolution::At(resolved)
}

/// [`resolve`] of a path that must exist: the canonical path, or `None`.
fn resolve_existing(base: &Path, path: &Path) -> Option<PathBuf> {
    match resolve(base, path) {
        Resolution::At(resolved) => Some(resolved),
        Resolution::Missing
        | Resolution::Unexaminable
        | Resolution::Refused
        | Resolution::ThroughAKernelTree
        | Resolution::LinkLeavesTheRoot => None,
    }
}

/// A read-only git command can execute programs or read outside the project because
/// of repository metadata, even when its arguments contain no unsafe paths.
/// `diff.external`, `core.fsmonitor` and `post-index-change` hooks can run programs;
/// `core.worktree` can redirect file reads. A promisor remote using `ext::` can run a
/// helper during lazy fetch, and `blame.ignoreRevsFile` names a file to read.
/// Configuration therefore uses an allowlist: keys not known to be inert for reads
/// card through [`config_runs_nothing`], and includes are followed.
/// [`hooks_run_nothing_under_a_read`] cards hooks not known to require writing commands.
/// Submodule directories under `modules/` need the same checks; their `commondir` can
/// also redirect common configuration. `objects/info/alternates` can expose foreign
/// objects, and symlinked `objects`, `refs` or `packed-refs` can expose foreign data
/// without an alternates file. Symlinked pack files and loose-object directories can
/// also escape, so [`git_directory_reads_stay_inside`] confines every link in the tree.
///
/// The model cannot write any of this without a card. What can is a directory or archive that
/// arrives with its own `.git` (`git clone` carries none of it), and there the user's own
/// git-aware editor plugins would run the same programs. This is stricter than the CLI, which
/// hardens its own internal git calls but not the Bash tool's.
fn repository_runs_nothing_of_its_own(
    git_dir: &Path,
    common: &Path,
    root: &Path,
    home: Option<&Path>,
) -> Result<(), &'static str> {
    // First contain every link in the git directories using bounded resolution, so later reads
    // cannot follow a link whose resolution cost has not been bounded.
    let mut entries = MAX_GIT_DIRECTORY_ENTRIES;
    git_directory_reads_stay_inside(common, root, &mut entries)?;
    if !git_dir.starts_with(common) {
        git_directory_reads_stay_inside(git_dir, root, &mut entries)?;
    }
    // `common/config` is what git reads; `git_dir/config` and `config.worktree` are read whether or
    // not `extensions.worktreeConfig` asks for the latter -- a superset.
    let mut config_files = vec![common.join("config")];
    if git_dir != common {
        config_files.push(git_dir.join("config"));
    }
    config_files.push(git_dir.join("config.worktree"));
    // The top repository's working tree is the project root, which is where a relative
    // `core.hooksPath` and `core.worktree` resolve and the boundary they must stay inside.
    let scope = ConfigScope {
        root,
        git_dir,
        common,
        home,
        is_top_level: true,
    };
    for file in &config_files {
        config_runs_nothing(file, &scope, 0)?;
    }
    if common.join("objects/info/alternates").symlink_metadata().is_ok() {
        return Err(REASON_GIT_ALTERNATES);
    }
    hooks_run_nothing_under_a_read(&common.join("hooks"))?;
    let mut budget = MAX_SUBMODULE_GIT_DIRECTORIES;
    let mut scan = SubmoduleScan {
        root,
        home,
        walked: common,
        budget: &mut budget,
        entries: &mut entries,
    };
    submodule_git_directories_run_nothing(&common.join("modules"), &mut scan)
}

/// How many entries of a repository's git directories are walked before the answer is a card. The
/// six measured repositories held 420 to 1767 each, none of them a symbolic link; git's
/// own `gc --auto` packs a repository once it passes 6700 loose objects.
const MAX_GIT_DIRECTORY_ENTRIES: usize = 200_000;

/// Every symlink in `dir` must resolve inside it or to a file inside the project. Project
/// directories are refused because their entries were not walked. The walk follows no links;
/// `dir` must already be canonical.
///
/// Checking only top-level entries misses nested links such as `objects/pack/pack-*.{idx,pack}`
/// and `objects/<xx>`, which can expose another repository's objects. The same risk applies to
/// `index`, `shallow`, loose refs and reflogs. Any `objects/info/alternates` file or unreadable
/// directory requires a card.
///
/// Bound the work before following links: the walk stops at [`MAX_RESOLVED_DEPTH`], refuses
/// more than [`MAX_GIT_DIRECTORY_SYMLINKS`] links, and resolves each accepted link with
/// [`resolve`]. Padded link chains must not make this scan block the UI.
fn git_directory_reads_stay_inside(dir: &Path, root: &Path, entries: &mut usize) -> Result<(), &'static str> {
    let mut links = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        if next.components().count() > MAX_RESOLVED_DEPTH {
            return Err(REASON_GIT_DIR_TOO_BIG);
        }
        if !spend(WORK_PER_LOOKUP) {
            return Err(REASON_TOO_MUCH_WORK);
        }
        for entry in std::fs::read_dir(&next).map_err(|_| REASON_GIT_DIR_UNREADABLE)? {
            let entry = entry.map_err(|_| REASON_GIT_DIR_UNREADABLE)?;
            *entries = entries.checked_sub(1).ok_or(REASON_GIT_DIR_TOO_BIG)?;
            if !spend(WORK_PER_ENTRY) {
                return Err(REASON_TOO_MUCH_WORK);
            }
            let file_type = entry.file_type().map_err(|_| REASON_GIT_DIR_UNREADABLE)?;
            if file_type.is_dir() {
                pending.push(entry.path());
            } else if file_type.is_symlink() {
                if links.len() == MAX_GIT_DIRECTORY_SYMLINKS {
                    return Err(REASON_GIT_DIR_TOO_MANY_LINKS);
                }
                links.push(entry.path());
            }
        }
    }
    for link in &links {
        // The walk entered only real directories from a canonical start, so `parent` is canonical.
        let (Some(parent), Some(name)) = (link.parent(), link.file_name()) else {
            return Err(REASON_GIT_DIR_ESCAPES);
        };
        match resolve(parent, Path::new(name)) {
            Resolution::At(resolved)
                if resolved.starts_with(dir) || (resolved.starts_with(root) && !resolved.is_dir()) => {}
            _ => return Err(REASON_GIT_DIR_ESCAPES),
        }
    }
    Ok(())
}

/// What a config file is read against: the working-tree root a relative `core.hooksPath` and
/// `core.worktree` resolve to and must stay inside, the git directory and common directory those
/// paths resolve relative to, and whether this is the top repository (a submodule's config is not
/// something git writes `core.hooksPath` into, so any there cards).
struct ConfigScope<'a> {
    root: &'a Path,
    git_dir: &'a Path,
    common: &'a Path,
    home: Option<&'a Path>,
    is_top_level: bool,
}

/// Git's own limit (`MAX_INCLUDE_DEPTH`); past it git dies, so here it cards.
const MAX_CONFIG_INCLUDE_DEPTH: usize = 10;

/// A missing file is fine, as it is to git (a repository need not have a config, and a missing
/// `include.path` is silently skipped). Anything else git could not read or parse is a card. Every
/// variable is judged against the allowlist ([`GitConfigVariable`]); `include`s are followed, a
/// remote URL's transport is checked, and `core.hooksPath`/`core.worktree` are resolved and
/// contained rather than carded outright.
fn config_runs_nothing(path: &Path, scope: &ConfigScope, depth: usize) -> Result<(), &'static str> {
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
        if variable.is_an_include() {
            let target = variable
                .value
                .as_deref()
                .and_then(|value| include_target(value, path, scope.home))
                .ok_or(REASON_GIT_CONFIG_UNREADABLE)?;
            // Resolve includes within bounds and skip missing ones as git does. Every include
            // read spends from the work budget so branching include chains cannot cause
            // unbounded reads.
            match resolve(Path::new("/"), &target) {
                // As git, running as this same user, is refused the same lookup.
                Resolution::Missing | Resolution::Unexaminable => {}
                Resolution::At(target) => config_runs_nothing(&target, scope, depth + 1)?,
                // A missing include here can exist in git's process: `/proc/self/cwd/x.inc`
                // uses this process's cwd here and the project root in git.
                Resolution::ThroughAKernelTree => return Err(REASON_THROUGH_A_KERNEL_TREE),
                Resolution::Refused | Resolution::LinkLeavesTheRoot => return Err(REASON_GIT_CONFIG_UNREADABLE),
            }
            continue;
        }
        if variable.is_core_hookspath() {
            hookspath_reads_only_known_hooks(variable.value.as_deref(), scope)?;
            continue;
        }
        if variable.is_core_worktree() {
            worktree_stays_inside(variable.value.as_deref(), scope)?;
            continue;
        }
        if variable.is_a_url() {
            if variable.value.as_deref().is_some_and(url_runs_a_program) {
                return Err(REASON_GIT_URL_RUNS_A_PROGRAM);
            }
            continue;
        }
        if !variable.is_inert_for_reads() {
            return Err(REASON_GIT_CONFIG_UNKNOWN_KEY);
        }
    }
    Ok(())
}

/// Whether a remote URL names a transport that runs a program. Git's `<transport>::<address>`
/// syntax runs an external `git-remote-<transport>` helper; `ext::` can run an arbitrary
/// command. A path or scheme URL has a `/` or `:` before any `::` and is not helper syntax.
fn url_runs_a_program(value: &[u8]) -> bool {
    let Some(pos) = value.windows(2).position(|w| w == b"::") else {
        return false;
    };
    let transport = &value[..pos];
    !transport.is_empty()
        && transport
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'+' | b'.'))
}

/// Treat repository `core.hooksPath` as the hook directory. Resolve `~/` against home, absolute
/// paths as written and relative paths against the working-tree root, where git runs hooks. A
/// directory inside the project or git directory uses the same hook allowlist as `.git/hooks`.
/// Outside, missing, unreadable, `~user/` or submodule-config paths require cards.
fn hookspath_reads_only_known_hooks(value: Option<&[u8]>, scope: &ConfigScope) -> Result<(), &'static str> {
    use std::os::unix::ffi::OsStrExt;
    let value = value.ok_or(REASON_GIT_HOOKSPATH)?;
    if !scope.is_top_level {
        return Err(REASON_GIT_HOOKSPATH);
    }
    let path = if let Some(rest) = value.strip_prefix(b"~/") {
        scope
            .home
            .ok_or(REASON_GIT_HOOKSPATH)?
            .join(Path::new(std::ffi::OsStr::from_bytes(rest)))
    } else if value.starts_with(b"~") || value.is_empty() {
        return Err(REASON_GIT_HOOKSPATH);
    } else {
        let p = Path::new(std::ffi::OsStr::from_bytes(value));
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            scope.root.join(p)
        }
    };
    let canonical = resolve_existing(Path::new("/"), &path).ok_or(REASON_GIT_HOOKSPATH)?;
    let inside = canonical.starts_with(scope.root)
        || canonical.starts_with(scope.git_dir)
        || canonical.starts_with(scope.common);
    if !inside {
        return Err(REASON_GIT_HOOKSPATH);
    }
    hooks_run_nothing_under_a_read(&canonical)
}

/// `core.worktree` relocates the working tree relative to the git directory. A submodule
/// normally points back inside the superproject. Allow it only when it resolves inside the
/// project root; outside or unresolvable paths require cards because `git diff` can otherwise
/// read outside the project.
fn worktree_stays_inside(value: Option<&[u8]>, scope: &ConfigScope) -> Result<(), &'static str> {
    use std::os::unix::ffi::OsStrExt;
    let value = value.ok_or(REASON_GIT_CONFIG_RUNS_A_PROGRAM)?;
    if value.is_empty() {
        return Err(REASON_GIT_CONFIG_RUNS_A_PROGRAM);
    }
    match resolve_existing(scope.git_dir, Path::new(std::ffi::OsStr::from_bytes(value))) {
        Some(resolved) if resolved.starts_with(scope.root) => Ok(()),
        _ => Err(REASON_GIT_CONFIG_RUNS_A_PROGRAM),
    }
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
        if !spend(WORK_PER_ENTRY) {
            return Err(REASON_TOO_MUCH_WORK);
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err(REASON_GIT_HOOK);
        };
        // Inspect the entry's own type without following links, since project-controlled link
        // chains can make listing expensive. Git never runs a directory. A directory symlink
        // with a name outside the hook allowlist still requires a card.
        let is_dir = entry.file_type().is_ok_and(|file_type| file_type.is_dir());
        if name.contains('.') || HOOKS_NO_READ_ONLY_SUBCOMMAND_RUNS.contains(&name) || is_dir {
            continue;
        }
        return Err(REASON_GIT_HOOK);
    }
    Ok(())
}

/// How many directories under `modules/` are walked before the answer is a card.
const MAX_SUBMODULE_GIT_DIRECTORIES: usize = 1024;

/// What the walk of submodule git directories carries along: the project root, home, the top
/// repository's common directory as [`git_directory_reads_stay_inside`] walked it (a submodule's
/// own common directory under it has been walked already), and the two budgets.
struct SubmoduleScan<'a> {
    root: &'a Path,
    home: Option<&'a Path>,
    walked: &'a Path,
    budget: &'a mut usize,
    entries: &'a mut usize,
}

/// Checks every submodule git directory under `dir`, including names containing `/` and nested
/// `modules/`, against the repository's config, hook and object-store rules. Resolve each
/// `commondir` and check the common directory git actually reads: its `core.fsmonitor` can run
/// under the superproject's `git status`. Unresolvable paths require cards. `scan.root` bounds
/// each submodule's `core.worktree`, which normally points back inside the superproject.
fn submodule_git_directories_run_nothing(dir: &Path, scan: &mut SubmoduleScan) -> Result<(), &'static str> {
    // `dir` resolved first: `<common>/modules` may itself be a link, and an entry's `..` must be
    // taken physically, as git takes it.
    let dir = match resolve(Path::new("/"), dir) {
        Resolution::At(dir) => dir,
        Resolution::Missing | Resolution::Unexaminable => return Ok(()),
        Resolution::ThroughAKernelTree => return Err(REASON_THROUGH_A_KERNEL_TREE),
        Resolution::Refused | Resolution::LinkLeavesTheRoot => return Err(REASON_GIT_CONFIG_UNREADABLE),
    };
    let dir = dir.as_path();
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
        let entry = entry.map_err(|_| REASON_GIT_CONFIG_UNREADABLE)?;
        if !spend(WORK_PER_ENTRY) {
            return Err(REASON_TOO_MUCH_WORK);
        }
        // Follow links as git does, using bounded resolution. The containing directory's walk
        // has already checked where they may lead. `dir` and every directory entered from it
        // are canonical.
        let path = match entry.file_type() {
            Ok(file_type) if file_type.is_dir() => entry.path(),
            Ok(file_type) if file_type.is_symlink() => match resolve_existing(dir, Path::new(&entry.file_name())) {
                Some(resolved) if resolved.is_dir() => resolved,
                _ => continue,
            },
            _ => continue,
        };
        *scan.budget = scan.budget.checked_sub(1).ok_or(REASON_GIT_TOO_MUCH_TO_CHECK)?;
        let is_a_git_dir =
            path.join("HEAD").symlink_metadata().is_ok() || path.join("config").symlink_metadata().is_ok();
        if is_a_git_dir {
            // git reads a submodule's config and hooks from its COMMON directory, which its own
            // `commondir` may relocate. Resolve it and check there; an unresolvable one cards.
            let common = commondir_of(&path)?;
            // Contained first, as for the top repository, when the walk above did not cover it.
            if !common.starts_with(scan.walked) {
                git_directory_reads_stay_inside(&common, scan.root, scan.entries)?;
            }
            let scope = ConfigScope {
                root: scan.root,
                git_dir: &path,
                common: &common,
                home: scan.home,
                is_top_level: false,
            };
            for file in ["config", "config.worktree"] {
                config_runs_nothing(&common.join(file), &scope, 0)?;
                if common != path {
                    config_runs_nothing(&path.join(file), &scope, 0)?;
                }
            }
            if common.join("objects/info/alternates").symlink_metadata().is_ok() {
                return Err(REASON_GIT_ALTERNATES);
            }
            hooks_run_nothing_under_a_read(&common.join("hooks"))?;
            submodule_git_directories_run_nothing(&common.join("modules"), scan)?;
        } else {
            submodule_git_directories_run_nothing(&path, scan)?;
        }
    }
    Ok(())
}

/// A git config variable with section and key lowercased for comparison. Keep whether a
/// subsection exists, but not its name: the submodule back-link must be plain `core.worktree`,
/// which git does not read from `[core "x"]`.
struct GitConfigVariable {
    section: String,
    has_subsection: bool,
    key: String,
    /// `None` for a bare key, which git reads as boolean true.
    value: Option<Vec<u8>>,
}

impl GitConfigVariable {
    /// Allows only config keys known to be inert for the permitted read-only subcommands.
    /// Unknown keys require cards because a denylist can miss program execution through
    /// lazy-fetch transports or file reads through keys such as `blame.ignoreRevsFile`. The
    /// caller separately handles includes, URL keys, `core.hooksPath` and `core.worktree`.
    ///
    /// Section and key are compared lowercased. Format flags and filesystem booleans are inert,
    /// as are remote fetch refspecs, branch remote names and merge refs, submodule active
    /// settings and commit identity strings. Git does not read the `glab-resolved` or
    /// `vscode-merge-base` metadata keys.
    ///
    /// Pull, rerere, branch description/rebase and submodule branch settings are unused by the
    /// permitted reads. Submodule update allows only `checkout`, `rebase`, `merge` and `none`;
    /// `!command` and unknown values require cards. `extensions.relativeworktrees` only
    /// describes how worktree links are stored.
    ///
    /// `filter.lfs.*` and partial-clone keys remain outside the allowlist.
    /// `extensions.partialclone` names a promisor remote that lazy fetch can reach.
    fn is_inert_for_reads(&self) -> bool {
        match (self.section.as_str(), self.key.as_str()) {
            ("submodule", "update") => matches!(
                self.value.as_deref(),
                Some(b"checkout" | b"rebase" | b"merge" | b"none")
            ),
            (
                "core",
                "repositoryformatversion"
                | "filemode"
                | "bare"
                | "logallrefupdates"
                | "ignorecase"
                | "precomposeunicode"
                | "symlinks",
            )
            | ("remote", "fetch" | "glab-resolved")
            | ("branch", "remote" | "merge" | "vscode-merge-base" | "description" | "rebase")
            | ("submodule", "active" | "branch")
            | ("user", "name" | "email")
            | ("pull", "rebase" | "ff")
            | ("rerere", "enabled" | "autoupdate")
            | (
                "extensions",
                "objectformat" | "refstorage" | "worktreeconfig" | "preciousobjects" | "relativeworktrees",
            ) => true,
            _ => false,
        }
    }

    /// `remote.<n>.url` and `submodule.<n>.url`, whose value the caller checks for a program transport.
    fn is_a_url(&self) -> bool {
        matches!(
            (self.section.as_str(), self.key.as_str()),
            ("remote" | "submodule", "url")
        )
    }

    /// `core.hooksPath`, resolved and contained by the caller rather than carded outright.
    fn is_core_hookspath(&self) -> bool {
        (self.section.as_str(), self.key.as_str()) == ("core", "hookspath")
    }

    /// `core.worktree`, resolved and contained by the caller.
    fn is_core_worktree(&self) -> bool {
        (self.section.as_str(), self.key.as_str()) == ("core", "worktree")
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
    let mut section: Option<(String, bool)> = None;
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
        let (section, has_subsection) = section.clone()?;
        let (key, value) = cursor.variable(c)?;
        variables.push(GitConfigVariable {
            section,
            has_subsection,
            key,
            value,
        });
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
    /// name, lowercased, and whether a subsection followed it.
    fn section_header(&mut self) -> Option<(String, bool)> {
        let mut name = String::new();
        let mut quoted = false;
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
                quoted = true;
                break;
            }
            if !(c.is_ascii_alphanumeric() || c == b'-' || c == b'.') {
                return None;
            }
            name.push(c.to_ascii_lowercase() as char);
        }
        let section = name.split('.').next().unwrap_or_default();
        (!section.is_empty()).then(|| (section.to_string(), quoted || name.contains('.')))
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
            // Canonical, as the session's own root always is: a root that does not
            // resolve to itself is no boundary, and macOS's `$TMPDIR` is under `/var -> private/var`.
            let root = root.canonicalize().unwrap();
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

    /// Every CLI tool outside `decided_elsewhere` still reaches the user. Use the actual
    /// `system/init` tool list so the test covers names a session can produce.
    ///
    /// `Write`, `Edit` and `NotebookEdit` have separate acceptEdits fast path tests: an
    /// ordinary in-project target is allowed without asking by their own classifier.
    #[test]
    fn every_other_tool_this_cli_offers_still_reaches_the_user() {
        let ws = Workspace::new();
        let decided_elsewhere = ["Task", "Bash", "Read", "ToolSearch", "Write", "Edit", "NotebookEdit"];
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
            carded, 19,
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
        // Use the canonical root, matching the CLI's cwd. A path through a system link
        // outside the root, such as macOS's `$TMPDIR` under `/var -> private/var`, cards
        // because links outside the root are not followed.
        let absolute = ws.path().canonicalize().unwrap().join("src/main.rs");
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

    /// The session root is already canonical and must keep resolving to itself.
    /// Renaming it and replacing its path with a link must not silently make another
    /// directory the project while the CLI child remains in the renamed directory.
    /// A hook `allow` overrides the CLI's working-directory prompt, so every call whose
    /// paths are checked must card when the root no longer resolves to itself.
    #[cfg(unix)]
    #[test]
    fn a_project_root_replaced_by_a_link_is_no_boundary() {
        struct Shared(PathBuf);
        impl Drop for Shared {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let shared = Shared(std::env::temp_dir().join(format!("agent-policy-shared-{}", uuid::Uuid::new_v4())));
        std::fs::create_dir_all(shared.0.join("project/src")).unwrap();
        std::fs::create_dir_all(shared.0.join("private")).unwrap();
        let root = shared.0.join("project").canonicalize().unwrap();
        let private = shared.0.join("private").canonicalize().unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(private.join("secret"), "private text\n").unwrap();
        let calls = [
            ("Write", json!({ "file_path": private.join("new.txt") })),
            ("Read", json!({ "file_path": private.join("secret") })),
            ("Grep", json!({ "pattern": "x", "path": &private })),
            ("Write", json!({ "file_path": "new.txt" })),
            ("Read", json!({ "file_path": "secret" })),
            ("Bash", json!({ "command": "cat secret" })),
        ];

        // Before: the project is the project -- the three calls naming the other directory card, and
        // an ordinary in-project write is allowed.
        for (tool, input) in &calls[..3] {
            assert_eq!(
                verdict(tool, input.clone(), &root),
                PermissionVerdict::AskTheUser,
                "{tool} {input}"
            );
        }
        assert_eq!(
            verdict("Write", json!({ "file_path": "new.txt" }), &root),
            PermissionVerdict::AllowWithoutAsking
        );

        // The swap: the project renamed away, a link to the other directory in its place.
        std::fs::rename(&root, shared.0.join("project.old")).unwrap();
        std::os::unix::fs::symlink(&private, &root).unwrap();
        let leaks: Vec<String> = calls
            .iter()
            .filter_map(|(tool, input)| {
                let got = classify_permission_request(tool, input, &root);
                (got.verdict != PermissionVerdict::AskTheUser).then(|| format!("{tool} {input}: {}", got.reason))
            })
            .collect();
        assert!(
            leaks.is_empty(),
            "allowed after the root was replaced:\n{}",
            leaks.join("\n")
        );
        assert_eq!(
            classify_permission_request("Write", &calls[0].1, &root).reason,
            REASON_ROOT_NO_LONGER_ITSELF
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

    // The acceptEdits fast path tests cover new and existing files, nested directories,
    // NotebookEdit, protected names at any depth, symlink and `..` escapes, hard links, saved
    // rules and malformed input. The `$HOME` boundary case is covered by
    // `a_project_root_at_or_above_home_is_no_boundary`, which supplies explicit `Surroundings`.

    #[test]
    fn an_edit_or_write_inside_the_project_root_needs_no_human() {
        let ws = Workspace::new();
        // A brand-new file directly in the root.
        assert_eq!(
            verdict("Write", json!({ "file_path": "brand-new.txt" }), ws.path()),
            PermissionVerdict::AllowWithoutAsking,
            "a new file"
        );
        // An existing file.
        assert_eq!(
            verdict("Edit", json!({ "file_path": "src/main.rs" }), ws.path()),
            PermissionVerdict::AllowWithoutAsking,
            "an existing file"
        );
        // A brand-new file under directories that do not exist yet either -- exercises
        // `resolve_missing_edit_target` peeling more than one level.
        assert_eq!(
            verdict("Write", json!({ "file_path": "a/b/c/new.rs" }), ws.path()),
            PermissionVerdict::AllowWithoutAsking,
            "a nested new file under nonexistent directories"
        );
        // NotebookEdit's own path field.
        assert_eq!(
            verdict("NotebookEdit", json!({ "notebook_path": "analysis.ipynb" }), ws.path()),
            PermissionVerdict::AllowWithoutAsking,
            "NotebookEdit"
        );
        // A relative path spelled with a leading `./`.
        assert_eq!(
            verdict("Write", json!({ "file_path": "./also-new.txt" }), ws.path()),
            PermissionVerdict::AllowWithoutAsking,
            "./ prefix"
        );
    }

    /// Every one of [`DANGEROUS_DIRECTORIES`], at the root and nested under an ordinary directory,
    /// for every edit tool -- none of these need to exist on disk first: a nonexistent protected
    /// directory still resolves through `resolve_missing_edit_target` down to `root` and the
    /// literal tail still carries the protected name, which is exactly the case this exists to
    /// catch (a model creating `.git/hooks/pre-commit` where no `.git` exists yet is still denied).
    #[test]
    fn an_edit_or_write_in_a_protected_directory_still_cards_at_any_depth() {
        let ws = Workspace::new();
        for dir in DANGEROUS_DIRECTORIES {
            for path in [format!("{dir}/x"), format!("sub/{dir}/x")] {
                for tool in ["Write", "Edit"] {
                    assert_eq!(
                        verdict(tool, json!({ "file_path": &path }), ws.path()),
                        PermissionVerdict::AskTheUser,
                        "{tool} into {path}"
                    );
                }
            }
        }
        assert_eq!(
            verdict(
                "NotebookEdit",
                json!({ "notebook_path": ".claude/agents/x.ipynb" }),
                ws.path()
            ),
            PermissionVerdict::AskTheUser,
            "NotebookEdit into a protected directory"
        );
        // Case-insensitive, like the CLI's own match.
        assert_eq!(
            verdict("Write", json!({ "file_path": ".GIT/config" }), ws.path()),
            PermissionVerdict::AskTheUser,
            "uppercase spelling"
        );
    }

    /// [`DANGEROUS_RELATIVE_PATH`] (`.config/git`), the one two-segment sequence beside the
    /// single-directory names -- extra fidelity beyond the module doc's written list, cited in the
    /// const's own doc.
    #[test]
    fn an_edit_or_write_under_config_slash_git_still_cards() {
        let ws = Workspace::new();
        assert_eq!(
            verdict("Write", json!({ "file_path": ".config/git/attributes" }), ws.path()),
            PermissionVerdict::AskTheUser
        );
        assert_eq!(
            verdict("Write", json!({ "file_path": "nested/.config/git/x" }), ws.path()),
            PermissionVerdict::AskTheUser
        );
    }

    /// Every one of [`PROTECTED_EDIT_FILE_BASENAMES`], at the root and nested -- matched by
    /// basename alone, regardless of which directory holds it.
    #[test]
    fn an_edit_or_write_to_a_protected_file_still_cards_regardless_of_directory() {
        let ws = Workspace::new();
        for name in PROTECTED_EDIT_FILE_BASENAMES {
            for path in [name.to_string(), format!("sub/{name}")] {
                assert_eq!(
                    verdict("Write", json!({ "file_path": &path }), ws.path()),
                    PermissionVerdict::AskTheUser,
                    "{path}"
                );
            }
        }
    }

    /// A symlink that IS the target, existing, resolving outside the root -- the plain escape case
    /// `Read`'s own path check already catches, reproduced here for the edit tools.
    #[test]
    fn an_edit_or_write_to_a_symlink_that_leaves_the_root_cards() {
        let ws = Workspace::new();
        ws.link("escape.txt", "/etc/hostname");
        assert_eq!(
            verdict("Write", json!({ "file_path": "escape.txt" }), ws.path()),
            PermissionVerdict::AskTheUser
        );
    }

    /// A symlinked DIRECTORY inside the root that points out of it, used as the ancestor of a
    /// brand-new file that does not itself exist -- the case that only the acceptEdits fast path's
    /// own "nearest existing ancestor" logic can even reach, since a plain `resolve` of the full
    /// path already returns `At` (fully following the link) for an existing target, but here the
    /// leaf itself is missing and it is the ancestor's own resolution that must catch the escape.
    #[test]
    fn a_write_through_a_symlinked_directory_that_leaves_the_root_still_cards() {
        let ws = Workspace::new();
        let outside = Outside::new();
        ws.link("escape", &outside.dir);
        assert_eq!(
            verdict("Write", json!({ "file_path": "escape/new.txt" }), ws.path()),
            PermissionVerdict::AskTheUser
        );
    }

    /// `..` walked lexically against the resolved ancestor (not textually), reaching outside the
    /// root for a target that does not itself exist.
    #[test]
    fn an_edit_or_write_reaching_outside_via_dotdot_still_cards() {
        let ws = Workspace::new();
        assert_eq!(
            verdict("Write", json!({ "file_path": "../outside.txt" }), ws.path()),
            PermissionVerdict::AskTheUser
        );
        assert_eq!(
            verdict("Write", json!({ "file_path": "sub/../../outside.txt" }), ws.path()),
            PermissionVerdict::AskTheUser
        );
    }

    /// A hard link: more than one path names the same data, so a write through either name also
    /// changes what the other name reads -- checked on both names, since neither is more "the
    /// original" than the other once the link exists.
    #[test]
    fn an_edit_or_write_to_a_hard_linked_file_cards() {
        let ws = Workspace::new();
        let original = ws.path().join("original.txt");
        std::fs::write(&original, "v1").unwrap();
        std::fs::hard_link(&original, ws.path().join("linked.txt")).unwrap();
        assert_eq!(
            verdict("Write", json!({ "file_path": "linked.txt" }), ws.path()),
            PermissionVerdict::AskTheUser,
            "the new name"
        );
        assert_eq!(
            verdict("Edit", json!({ "file_path": "original.txt" }), ws.path()),
            PermissionVerdict::AskTheUser,
            "the original name -- both name the same data"
        );
        // A plain file with one link is unaffected.
        assert_eq!(
            verdict("Write", json!({ "file_path": "src/main.rs" }), ws.path()),
            PermissionVerdict::AllowWithoutAsking
        );
    }

    /// A raw edit path containing `..` after a missing component must card.
    /// Applying the missing tail lexically can cancel `nx` and reach the real `escape`
    /// symlink without resolving it. Here `nx` is absent and `escape` points outside
    /// the root, so accepting the lexical result could allow a write outside it.
    #[test]
    fn a_dotdot_past_a_missing_component_cannot_reach_a_real_symlink_lexically() {
        let ws = Workspace::new();
        let outside = Outside::new();
        ws.link("escape", &outside.dir);
        for path in [
            "nx/../escape/pwned.txt",
            "./nx/../escape/pwned.txt",
            "sub/nx/../../escape/pwned.txt",
        ] {
            assert_eq!(
                verdict("Write", json!({ "file_path": path }), ws.path()),
                PermissionVerdict::AskTheUser,
                "{path}: must not reach the real outside symlink through a lexical `..`"
            );
        }
        // The absolute-path form the review also found allowed.
        let absolute = ws.path().join("nx/../escape/pwned.txt");
        assert_eq!(
            verdict("Write", json!({ "file_path": absolute.to_str().unwrap() }), ws.path()),
            PermissionVerdict::AskTheUser,
            "the same shape, spelled as an absolute path"
        );
    }

    /// The same mechanism, but reaching an existing HARD-LINKED file rather than an outside symlink:
    /// `resolve_missing_edit_target`'s fallback always set `is_hard_link: false`, since that branch
    /// believes it is naming something that does not exist yet ([codex], "important").
    #[test]
    fn a_dotdot_past_a_missing_component_cannot_reach_a_hard_link_lexically() {
        let ws = Workspace::new();
        let original = ws.path().join("original.txt");
        std::fs::write(&original, "v1").unwrap();
        std::fs::hard_link(&original, ws.path().join("linked.txt")).unwrap();
        assert_eq!(
            verdict("Write", json!({ "file_path": "nx/../linked.txt" }), ws.path()),
            PermissionVerdict::AskTheUser,
            "must not bypass the hard-link check via a lexical `..`"
        );
    }

    /// `~` is never expanded by this policy -- the real CLI's own hook always hands it an
    /// already-expanded path for these three tools (see `edit_target_raw_path_is_suspect`'s own
    /// doc), so a raw `~` reaching here at all is a card, not a `$HOME`-relative resolution.
    #[test]
    fn a_leading_tilde_in_an_edit_target_gets_a_card_rather_than_expanding() {
        let ws = Workspace::new();
        for (tool, field, path) in [
            ("Write", "file_path", "~/evil.desktop"),
            ("Write", "file_path", "~"),
            ("NotebookEdit", "notebook_path", "~/x.ipynb"),
        ] {
            assert_eq!(
                verdict(tool, json!({ field: path }), ws.path()),
                PermissionVerdict::AskTheUser,
                "{tool} {path}"
            );
        }
        // A `~` that is not the leading character is an ordinary filename character, not a card by
        // this rule (it may still card for other reasons; this only pins that THIS rule does not
        // fire on it).
        assert_eq!(
            verdict("Write", json!({ "file_path": "file~backup.txt" }), ws.path()),
            PermissionVerdict::AllowWithoutAsking,
            "a mid-name ~ is not tilde expansion"
        );
    }

    /// [codex], "blocking": the CLI checks a protected name at both the requested spelling and the
    /// resolved one. An ordinary, unremarkable symlink whose target has an unprotected name used to
    /// slip past this policy entirely, since only [`EditTarget::logical`] -- the resolved path --
    /// was ever checked.
    #[test]
    fn a_protected_basename_still_cards_when_it_is_a_symlink_to_an_unprotected_name() {
        let ws = Workspace::new();
        ws.write("config/npm-user.conf", "registry=https://example.com\n");
        ws.link(".npmrc", ws.path().join("config/npm-user.conf"));
        assert_eq!(
            verdict("Write", json!({ "file_path": ".npmrc" }), ws.path()),
            PermissionVerdict::AskTheUser,
            "the requested spelling is a protected basename, even though the resolved one is not"
        );
    }

    /// The same gap, for a protected DIRECTORY rather than a protected file basename.
    #[test]
    fn a_protected_directory_still_cards_when_it_is_a_symlink_to_an_unprotected_name() {
        let ws = Workspace::new();
        std::fs::create_dir_all(ws.path().join("editor-config")).unwrap();
        ws.link(".vscode", ws.path().join("editor-config"));
        assert_eq!(
            verdict("Write", json!({ "file_path": ".vscode/settings.json" }), ws.path()),
            PermissionVerdict::AskTheUser,
            "the requested path names a protected directory, even though it is a symlink"
        );
    }

    /// [codex], "important": the CLI's own `Ki` folds a handful of confusable characters before
    /// matching a protected name, so a lookalike cannot rename its way past the check. Narrower than
    /// `Ki` (see `fold_confusables`'s own doc), but covers the two characters and the invisible-run
    /// the review actually reproduced.
    #[test]
    fn a_confusable_spelling_of_a_protected_name_still_cards() {
        let ws = Workspace::new();
        for path in [
            ".g\u{0131}t/config",  // dotless i (Turkish), U+0131
            ".v\u{017F}code/x",    // long s, U+017F
            ".git\u{200F}/config", // a right-to-left mark inside the name
            "sub/.\u{0131}dea/x",  // nested, same fold
        ] {
            assert_eq!(
                verdict("Write", json!({ "file_path": path }), ws.path()),
                PermissionVerdict::AskTheUser,
                "{path:?}"
            );
        }
    }

    /// Both edit-target resolvers must require a card for `Resolution::Refused` instead of
    /// falling through to allow. A dangling symlink is `Refused`, not `Missing`: a final link
    /// whose target does not exist is unresolvable, rather than an absent edit target.
    #[test]
    fn a_dangling_symlink_as_the_edit_target_itself_cards() {
        let ws = Workspace::new();
        ws.link("dangling", "/nonexistent-target-for-permission-policy-tests");
        assert_eq!(
            verdict("Write", json!({ "file_path": "dangling" }), ws.path()),
            PermissionVerdict::AskTheUser
        );
    }

    /// The same, with the dangling link as an ANCESTOR of the (also missing) target -- the case that
    /// specifically exercises `resolve_missing_edit_target`'s own `Resolution::Refused => return
    /// None` arm, since the full-path `resolve` call also refuses (a dangling link is never
    /// `Missing`), and the fallback must not paper over that by treating "refused" as "keep trying a
    /// shorter prefix".
    #[test]
    fn a_dangling_symlink_as_an_ancestor_of_a_missing_target_cards() {
        let ws = Workspace::new();
        ws.link("dangling", "/nonexistent-target-for-permission-policy-tests");
        assert_eq!(
            verdict("Write", json!({ "file_path": "dangling/new.txt" }), ws.path()),
            PermissionVerdict::AskTheUser
        );
    }

    /// An edit spelled outside the root cards even when it resolves inside.
    /// An outside `x` linked to the root makes `<outside>/x/new.txt` resolve inside,
    /// but retargeting `x` before the write could redirect it elsewhere. Both requested
    /// and resolved spellings must therefore be inside the root.
    #[test]
    fn an_edit_spelled_outside_the_root_cards_even_when_it_resolves_inside() {
        let ws = Workspace::new();
        let outside = Outside::new();
        let alias = outside.dir.join("x");
        std::os::unix::fs::symlink(ws.path(), &alias).unwrap();
        for (tool, relative) in [
            ("Write", ".ssh/authorized_keys"),
            ("Write", ".config/autostart/evil.desktop"),
            ("Write", "new.txt"),
            ("Edit", "src/main.rs"),
        ] {
            let spelled_outside = alias.join(relative);
            assert_eq!(
                verdict(
                    tool,
                    json!({ "file_path": spelled_outside.to_str().unwrap() }),
                    ws.path()
                ),
                PermissionVerdict::AskTheUser,
                "{tool} {}",
                spelled_outside.display()
            );
        }
        // The control: the same targets spelled through the root itself are still fast-pathed, so
        // the cards above come from the spelling and nothing else.
        let root = ws.path().canonicalize().unwrap();
        for (tool, relative) in [("Write", "new.txt"), ("Edit", "src/main.rs")] {
            assert_eq!(
                verdict(
                    tool,
                    json!({ "file_path": root.join(relative).to_str().unwrap() }),
                    ws.path()
                ),
                PermissionVerdict::AllowWithoutAsking,
                "{tool} {relative} spelled under the root"
            );
        }
    }

    /// Only the resolved path is protected here; the requested spelling hides it behind
    /// a link. This tests the resolved-path check independently of the requested-path
    /// check, so the requested-path check cannot mask a missing resolved-path check.
    #[test]
    fn a_protected_path_reached_only_through_a_link_still_cards() {
        let ws = Workspace::new();
        std::fs::create_dir_all(ws.path().join(".git/hooks")).unwrap();
        ws.link("hooks", ".git/hooks");
        ws.link("gitlink", "src/../.git");
        for (tool, path) in [
            ("Write", "hooks/pre-commit"), // a new file under a link into `.git`
            ("Write", "gitlink/config"),   // the same through a link whose target climbs with `..`
            ("Edit", "gitlink/HEAD"),      // an existing file through the same link
        ] {
            assert_eq!(
                verdict(tool, json!({ "file_path": path }), ws.path()),
                PermissionVerdict::AskTheUser,
                "{tool} {path}"
            );
        }
        // The control: the same shape of link to an unprotected directory is fast-pathed, so the
        // cards above come from where the links lead.
        std::fs::create_dir_all(ws.path().join("docs")).unwrap();
        ws.link("docs-link", "docs");
        assert_eq!(
            verdict("Write", json!({ "file_path": "docs-link/new.md" }), ws.path()),
            PermissionVerdict::AllowWithoutAsking
        );
    }

    /// [`MAX_RESOLUTION_STEPS`] bounds `resolve_missing_edit_target` before any search.
    /// Without the cap, a missing first component causes an O(n^2) prefix search on the
    /// GTK thread before the tail is joined to the root. An excessive component count
    /// must card, while shorter missing paths can still use the new-file fast path.
    #[test]
    fn an_edit_target_of_more_components_than_one_resolution_looks_up_cards() {
        let ws = Workspace::new();
        let too_deep = format!("nx/{}f.txt", "d/".repeat(MAX_RESOLUTION_STEPS));
        let classification = classify_permission_request("Write", &json!({ "file_path": too_deep }), ws.path());
        assert_eq!(classification.verdict, PermissionVerdict::AskTheUser);
        assert_eq!(
            classification.reason,
            "the path's links could not be resolved within this policy's limits"
        );
        // Under the cap, the same shape is an ordinary new nested file.
        let deep = format!("nx/{}f.txt", "d/".repeat(8));
        assert_eq!(
            verdict("Write", json!({ "file_path": deep }), ws.path()),
            PermissionVerdict::AllowWithoutAsking
        );
    }

    /// Restores a directory's mode when dropped, so a failing assertion still leaves a workspace
    /// `remove_dir_all` can delete.
    struct RestoreMode(PathBuf);

    impl Drop for RestoreMode {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
    }

    /// Only "not found" and "not a directory" count as [`Resolution::Missing`].
    /// Other lookup failures yield [`Resolution::Unexaminable`] and card for edits,
    /// so the missing-target fallback cannot allow a write through an unexamined link.
    /// Here `locked` lacks search permission and contains `inner` pointing outside.
    /// Bash arguments treat the failure as missing because the command runs as the same
    /// user and cannot perform the lookup either.
    #[test]
    fn a_component_that_cannot_be_examined_cards_rather_than_counting_as_missing() {
        use std::os::unix::fs::PermissionsExt;
        let ws = Workspace::new();
        let outside = Outside::new();
        let locked = ws.path().join("locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::os::unix::fs::symlink(&outside.dir, locked.join("inner")).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o600)).unwrap();
        let _restore = RestoreMode(locked.clone());
        if locked.join("inner").symlink_metadata().is_ok() {
            // A process that may search any directory (root, or CAP_DAC_OVERRIDE) sees the link, so
            // this case cannot be built here; the link-out case is covered by the other tests.
            eprintln!("skipped: this process can search a mode-0600 directory");
            return;
        }
        for (tool, path) in [
            ("Write", "locked/inner/new.txt"), // the review's probe: the link out behind the lock
            ("Write", "locked/new.txt"),       // nothing behind the lock can be examined at all
            ("Edit", "locked/inner/secret"),
        ] {
            assert_eq!(
                verdict(tool, json!({ "file_path": path }), ws.path()),
                PermissionVerdict::AskTheUser,
                "{tool} {path}"
            );
        }
        // A name longer than a directory entry may be fails the lookup the same way.
        assert_eq!(
            verdict(
                "Write",
                json!({ "file_path": format!("{}/new.txt", "n".repeat(300)) }),
                ws.path()
            ),
            PermissionVerdict::AskTheUser,
            "a component the filesystem refuses to look up"
        );
        // Not found and not a directory stay missing: a new file, and one under a regular file
        // (which the write itself will fail on), are still ordinary in-project writes.
        assert_eq!(
            verdict("Write", json!({ "file_path": "fresh/new.txt" }), ws.path()),
            PermissionVerdict::AllowWithoutAsking
        );
        assert_eq!(
            verdict("Write", json!({ "file_path": "src/main.rs/new.txt" }), ws.path()),
            PermissionVerdict::AllowWithoutAsking
        );
    }

    /// Every path-judged tool, one input each, naming `link` (a link at the root) as the way in:
    /// a new file and an existing one written, a notebook edited, a file read, a directory
    /// searched and listed, and a file printed by `cat`. `link` must lead to a directory holding
    /// `page.md` and `nb.ipynb`.
    fn every_path_tool_through(link: &str, root: &Path) -> Vec<(&'static str, Value)> {
        vec![
            (
                "Write",
                json!({ "file_path": format!("{link}/.config/autostart/evil.desktop") }),
            ),
            ("Write", json!({ "file_path": root.join(link).join("new.txt") })),
            ("Edit", json!({ "file_path": format!("{link}/page.md") })),
            ("NotebookEdit", json!({ "notebook_path": format!("{link}/nb.ipynb") })),
            ("Read", json!({ "file_path": format!("{link}/page.md") })),
            ("Grep", json!({ "pattern": "page", "path": link })),
            ("Glob", json!({ "pattern": "*.md", "path": link })),
            ("Bash", json!({ "command": format!("cat {link}/page.md") })),
        ]
    }

    /// Every call [`every_path_tool_through`] makes of `link` whose verdict is not `expected`, so a
    /// red run lists each tool that leaked rather than stopping at the first.
    fn verdicts_other_than(expected: PermissionVerdict, link: &str, root: &Path) -> Vec<String> {
        every_path_tool_through(link, root)
            .into_iter()
            .filter_map(|(tool, input)| {
                let got = classify_permission_request(tool, &input, root);
                (got.verdict != expected).then(|| format!("{tool} {input} through {link}: {}", got.reason))
            })
            .collect()
    }

    /// Links through `/proc` or `/dev` can resolve differently here and in the CLI child.
    /// `/proc/self/fd/<n>` uses each process's own descriptor; `/dev/fd` links to it.
    /// `/proc/self/root` and `/proc/thread-self/root` use the process's own root.
    /// Even if these links lead inside the project here, they must card for every path tool.
    /// The `/proc/self/cwd` case changes this process's cwd and therefore runs separately
    /// in `agent/tests/permission_policy_process_cwd.rs`.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_link_through_proc_or_dev_cards_for_every_path_tool() {
        use std::os::fd::AsRawFd;
        let ws = Workspace::new();
        let root = ws.path().canonicalize().unwrap();
        ws.write("page.md", "page\n");
        ws.write("nb.ipynb", "{}\n");
        // A directory this process holds open, for as long as the test runs.
        let src = std::fs::File::open(root.join("src")).unwrap();
        let fd = src.as_raw_fd();
        let mut leaks = Vec::new();
        for (name, target) in [
            ("via-proc-self-fd", format!("/proc/self/fd/{fd}/..")),
            ("via-dev-fd", format!("/dev/fd/{fd}/..")),
            ("via-proc-self-root", format!("/proc/self/root{}", root.display())),
            (
                "via-proc-thread-self-root",
                format!("/proc/thread-self/root{}", root.display()),
            ),
        ] {
            ws.link(name, &target);
            assert_eq!(
                std::fs::canonicalize(root.join(name)).unwrap(),
                root,
                "{name} -> {target} must lead back to the root in this process, or this test tests nothing"
            );
            leaks.extend(verdicts_other_than(PermissionVerdict::AskTheUser, name, &root));
        }
        assert!(leaks.is_empty(), "allowed:\n{}", leaks.join("\n"));
        // Carded for this reason, not merely because each link also leaves the root.
        for name in [
            "via-proc-self-fd",
            "via-dev-fd",
            "via-proc-self-root",
            "via-proc-thread-self-root",
        ] {
            let read = classify_permission_request("Read", &json!({ "file_path": format!("{name}/page.md") }), &root);
            assert_eq!(read.reason, REASON_THROUGH_A_KERNEL_TREE, "{name}");
        }
    }

    /// A link that leaves the root and returns must card even if its final target is inside.
    /// Outside path components can differ by process or change before use. Only links
    /// whose resolution stays within the root are allowed; the controls below cover those.
    #[cfg(unix)]
    #[test]
    fn a_link_out_of_the_root_and_back_in_cards() {
        let ws = Workspace::new();
        let root = ws.path().canonicalize().unwrap();
        let root_name = root.file_name().unwrap().to_str().unwrap().to_string();
        let outside = Outside::new();
        let outside_dir = outside.dir.canonicalize().unwrap();
        let outside_name = outside_dir.file_name().unwrap().to_str().unwrap().to_string();
        ws.write("docs/page.md", "page\n");
        ws.write("docs/nb.ipynb", "{}\n");
        std::os::unix::fs::symlink(&root, outside_dir.join("back")).unwrap();
        let mut leaks = Vec::new();
        for (name, target) in [
            // The review's shape: out to an outside directory, `..`, and back in by name.
            ("out-and-back", format!("{}/../{root_name}/docs", outside_dir.display())),
            // The same, relative, through a sibling of the root.
            ("via-sibling", format!("../{outside_name}/../{root_name}/docs")),
            // Through a link that sits outside the project and leads back into it.
            ("via-outside-link", format!("{}/back/docs", outside_dir.display())),
        ] {
            ws.link(name, &target);
            assert_eq!(
                std::fs::canonicalize(root.join(name)).unwrap(),
                root.join("docs"),
                "{name} -> {target} must end inside the root, or this test tests nothing"
            );
            leaks.extend(verdicts_other_than(PermissionVerdict::AskTheUser, name, &root));
        }
        assert!(leaks.is_empty(), "allowed:\n{}", leaks.join("\n"));
        for name in ["out-and-back", "via-sibling", "via-outside-link"] {
            let write = classify_permission_request("Write", &json!({ "file_path": format!("{name}/new.md") }), &root);
            assert_eq!(write.reason, REASON_LINK_LEAVES_THE_ROOT, "{name}");
        }
        // A read spelled through the outside link itself (no link in the project at all): the link
        // is not followed where it sits outside the root, though it leads back in.
        let spelled_outside = outside_dir.join("back/docs/page.md");
        let read = classify_permission_request("Read", &json!({ "file_path": spelled_outside }), &root);
        assert_eq!(
            read.verdict,
            PermissionVerdict::AskTheUser,
            "Read through a link outside the project"
        );
        assert_eq!(read.reason, REASON_LINK_LEAVES_THE_ROOT);

        // The controls: links that stay inside at every step are still followed, whatever their
        // spelling -- a relative chain, an absolute target (which walks down from `/` along the
        // root's own ancestors, the same directories the root itself was resolved through), and a
        // `..` up to the root's own parent and straight back in by name.
        ws.link("chain", "chain-next");
        ws.link("chain-next", "docs");
        ws.link("absolute-in-root", root.join("docs"));
        ws.link("up-and-in", format!("../{root_name}/docs"));
        // (`.config/autostart/...` is no protected path, so that `Write` runs here too.)
        let carded: Vec<String> = ["chain", "absolute-in-root", "up-and-in"]
            .into_iter()
            .flat_map(|name| verdicts_other_than(PermissionVerdict::AllowWithoutAsking, name, &root))
            .collect();
        assert!(carded.is_empty(), "carded:\n{}", carded.join("\n"));
    }

    /// CLI 2.1.283 passes raw `Grep` and `Glob` paths to the hook, then searches using
    /// `expandPath`: whitespace is trimmed, home paths are expanded and `..` is resolved
    /// lexically against the cwd. Physical resolution here can therefore disagree:
    /// `d/../..` can stay inside physically but escape lexically, `~` can name a local
    /// directory here but the home directory in the CLI, and trimming can expose a link out.
    /// These spellings must card whenever the tools are offered. This launch does not
    /// offer them unless `--tools` or `--allowedTools` names them.
    #[cfg(unix)]
    #[test]
    fn a_search_path_the_cli_spells_differently_cards() {
        let ws = Workspace::new();
        let root = ws.path().to_path_buf();
        let outside = Outside::new();
        ws.write("sub/deeper/page.md", "page\n");
        ws.link("d", "sub/deeper");
        ws.write("~/.ssh/known_hosts", "host\n");
        ws.link("sub2", &outside.dir);
        ws.link("sub3", &outside.dir);
        let respelled = [" sub2", "\tsub2", "\u{3000}sub2", "\u{feff}sub2", "sub3 "];
        for name in respelled {
            ws.write(&format!("{name}/page.md"), "page\n");
        }
        let mut paths = vec!["d/../..", "~/.ssh", "~"];
        paths.extend(respelled);
        let mut leaks = Vec::new();
        for path in paths {
            // Each names somewhere real inside the root when resolved here, or this tests nothing.
            let (here, _) = with_work_budget(MAX_WORK_PER_CLASSIFICATION, || resolve_in_root(&root, Path::new(path)));
            assert!(
                matches!(&here, Resolution::At(at) if at.starts_with(&root)),
                "{path:?} must resolve inside the root here, not {here:?}"
            );
            for (tool, input) in [
                ("Grep", json!({ "pattern": "page", "path": path })),
                ("Glob", json!({ "pattern": "*.md", "path": path })),
            ] {
                let got = classify_permission_request(tool, &input, &root);
                if got.verdict != PermissionVerdict::AskTheUser {
                    leaks.push(format!("{tool} {input}: {}", got.reason));
                }
            }
        }
        assert!(leaks.is_empty(), "allowed:\n{}", leaks.join("\n"));

        // The controls: spellings the CLI keeps as they are -- a name with a space or a `~` inside
        // it, a `.`, an absolute in-root path, and a link that stays inside -- are still allowed.
        ws.write("my dir/page.md", "page\n");
        ws.write("a~b/page.md", "page\n");
        let absolute = root.join("sub").display().to_string();
        let mut carded = Vec::new();
        for path in ["src", "./src", "my dir", "a~b", "d", "sub/deeper", absolute.as_str()] {
            for (tool, input) in [
                ("Grep", json!({ "pattern": "page", "path": path })),
                ("Glob", json!({ "pattern": "*.md", "path": path })),
            ] {
                let got = classify_permission_request(tool, &input, &root);
                if got.verdict != PermissionVerdict::AllowWithoutAsking {
                    carded.push(format!("{tool} {input}: {}", got.reason));
                }
            }
        }
        assert!(carded.is_empty(), "carded:\n{}", carded.join("\n"));
    }

    #[test]
    fn a_write_or_edit_with_the_wrong_shape_gets_a_card() {
        let ws = Workspace::new();
        for tool in ["Write", "Edit"] {
            assert_eq!(
                verdict(tool, json!({}), ws.path()),
                PermissionVerdict::AskTheUser,
                "{tool} missing file_path"
            );
            assert_eq!(
                verdict(tool, json!({ "file_path": 7 }), ws.path()),
                PermissionVerdict::AskTheUser,
                "{tool} non-string file_path"
            );
            assert_eq!(
                verdict(tool, json!({ "file_path": "" }), ws.path()),
                PermissionVerdict::AskTheUser,
                "{tool} empty file_path"
            );
            assert_eq!(
                verdict(tool, json!({ "file_path": null }), ws.path()),
                PermissionVerdict::AskTheUser,
                "{tool} null file_path"
            );
        }
    }

    #[test]
    fn a_notebookedit_with_the_wrong_shape_gets_a_card() {
        let ws = Workspace::new();
        assert_eq!(
            verdict("NotebookEdit", json!({}), ws.path()),
            PermissionVerdict::AskTheUser,
            "missing notebook_path"
        );
        assert_eq!(
            verdict("NotebookEdit", json!({ "notebook_path": 7 }), ws.path()),
            PermissionVerdict::AskTheUser,
            "non-string notebook_path"
        );
        assert_eq!(
            verdict("NotebookEdit", json!({ "notebook_path": "" }), ws.path()),
            PermissionVerdict::AskTheUser,
            "empty notebook_path"
        );
        // The wrong field name entirely: `file_path` is `Write`/`Edit`'s field, not this tool's.
        assert_eq!(
            verdict("NotebookEdit", json!({ "file_path": "x.ipynb" }), ws.path()),
            PermissionVerdict::AskTheUser,
            "wrong field name"
        );
    }

    /// Module doc, "Saved rules ... cannot interact with any of this in either direction":
    /// `PrefixRules::matching_rule` returns `None` for any tool other than `Bash`, so a populated
    /// rule set changes nothing about `Write`/`Edit`/`NotebookEdit` -- neither widening an allow
    /// past a protected path, nor narrowing one the fast path would otherwise grant.
    #[test]
    fn no_rule_can_touch_the_edit_tools_at_all() {
        let ws = Workspace::new();
        // Content is irrelevant -- any populated `PrefixRules` proves the point, since none of them
        // can ever match a non-`Bash` call.
        let rules = rules_for("git log --oneline");
        assert!(!rules.is_empty());

        for (tool, field, path) in [
            ("Write", "file_path", "new.txt"),
            ("Edit", "file_path", ".git/config"),
            ("NotebookEdit", "notebook_path", "n.ipynb"),
        ] {
            assert!(
                rule_that_allows(tool, &json!({ field: path }), ws.path(), &rules).is_none(),
                "{tool} must never be offered a rule"
            );
        }
        // And `classify_with_rules` agrees exactly with the rule-free classification either way.
        assert_eq!(
            classify_with_rules("Write", &json!({ "file_path": "new.txt" }), ws.path(), &rules).verdict,
            PermissionVerdict::AllowWithoutAsking,
            "the fast path still allows -- a rule cannot narrow it"
        );
        assert_eq!(
            classify_with_rules("Edit", &json!({ "file_path": ".git/config" }), ws.path(), &rules).verdict,
            PermissionVerdict::AskTheUser,
            "the protected path still cards -- a rule cannot widen past it"
        );
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
        // A link staying inside is allowed. Use a canonical target: passing through a system
        // link outside the root, such as macOS's `/var`, leaves the root on the way and cards.
        std::os::unix::fs::symlink(ws.path().canonicalize().unwrap().join("src"), ws.path().join("inner")).unwrap();
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
                work_budget: MAX_WORK_PER_CLASSIFICATION,
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
        // The acceptEdits fast path uses the same boundary check: a target that otherwise
        // passes still requires a card when the root is `$HOME` or an ancestor of it.
        assert_eq!(
            classify("Write", json!({ "file_path": "new.txt" }), ws.path(), ws.path()),
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

    /// A root under `/proc`, `/sys` or `/dev` cannot serve as a project boundary.
    /// Links there can depend on the process, so resolving a path here cannot establish
    /// where the CLI child will open it.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_project_root_in_proc_sys_or_dev_is_no_boundary() {
        let mut leaks = Vec::new();
        for (root, file) in [
            ("/dev", "null"),
            ("/dev", "stdin"),
            ("/proc", "self/status"),
            ("/sys", "kernel"),
        ] {
            for (tool, input) in [
                ("Read", json!({ "file_path": file })),
                ("Grep", json!({ "pattern": "x", "path": file })),
                ("Write", json!({ "file_path": "new.txt" })),
                ("Bash", json!({ "command": "ls" })),
            ] {
                let got = classify_permission_request(tool, &input, Path::new(root));
                if got.verdict != PermissionVerdict::AskTheUser {
                    leaks.push(format!("{tool} {input} under {root}: {}", got.reason));
                }
            }
        }
        assert!(leaks.is_empty(), "allowed:\n{}", leaks.join("\n"));
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
        // A Bash prefix rule cannot answer another tool's card. Use `WebFetch`, which always
        // requires a card, to test this. An in-project `Write` is already allowed by
        // `classify_edit_target` and would not test rule isolation. The edit fast path has
        // separate coverage in `no_rule_can_touch_the_edit_tools_at_all`.
        let all = PrefixRules::default().with(PrefixRule::parse("Bash(src *)").unwrap());
        assert!(
            classify_with_rules("WebFetch", &json!({ "url": "https://example.com" }), ws.path(), &all).needs_a_human()
        );
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

    /// A linked worktree (a `.git` file naming a git directory elsewhere, which names its common
    /// directory) is the project's own repository: its toplevel is the root. What its config says is
    /// read from the common directory, where git reads it.
    ///
    /// The git directory also holds the `gitdir` back-link naming this worktree's `.git`, as every
    /// `git worktree add` writes it. Without one, a repository outside the root is not the
    /// project's, and it cards (see
    /// `a_dot_git_naming_a_repository_outside_the_root_cards_unless_that_repository_names_the_root`).
    #[test]
    fn a_linked_worktree_is_judged_by_its_common_directory() {
        let main = Workspace::new();
        let git_dir = main.path().join(".git/worktrees/wt");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/wt\n").unwrap();
        std::fs::write(git_dir.join("commondir"), "../..\n").unwrap();
        let wt = Workspace::without_git();
        std::fs::write(wt.path().join(".git"), format!("gitdir: {}\n", git_dir.display())).unwrap();
        std::fs::write(
            git_dir.join("gitdir"),
            format!("{}\n", wt.path().join(".git").display()),
        )
        .unwrap();
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
            work_budget: MAX_WORK_PER_CLASSIFICATION,
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
            work_budget: MAX_WORK_PER_CLASSIFICATION,
        };
        let status = json!({ "command": "git status" });
        ws.write(".git/config", "[include]\n\tpath = ~/hazard.inc\n");
        assert!(classify_in("Bash", &status, ws.path(), &surroundings).needs_a_human());
        ws.write(".git/config", "[include]\n\tpath = ~/harmless.inc\n");
        assert!(!classify_in("Bash", &status, ws.path(), &surroundings).needs_a_human());
        ws.write(".git/config", "[include]\n\tpath = ~other/x.inc\n");
        assert!(classify_in("Bash", &status, ws.path(), &surroundings).needs_a_human());
    }

    /// A git include through `/proc/self/cwd` can be missing here but present for git.
    /// Git starts in the project root, where the included file can set `core.fsmonitor`.
    /// Such an include must card rather than be skipped as missing.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_git_include_through_proc_self_cwd_cards() {
        let ws = Workspace::new();
        let name = format!("policy-include-{}.inc", uuid::Uuid::new_v4());
        ws.write(&name, "[core]\n\tfsmonitor = ./fsm.sh\n");
        assert!(
            !std::env::current_dir().unwrap().join(&name).exists(),
            "this process's cwd must not hold the include, or this test tests nothing"
        );
        ws.write(".git/config", format!("[include]\n\tpath = /proc/self/cwd/{name}\n"));
        let got = classify_permission_request("Bash", &json!({ "command": "git status" }), ws.path());
        assert_eq!(got.verdict, PermissionVerdict::AskTheUser, "{}", got.reason);
        // The control: a missing include named without `/proc` is still skipped, as git skips it.
        ws.write(
            ".git/config",
            format!(
                "[include]\n\tpath = {}\n",
                ws.path().join("absent").join(&name).display()
            ),
        );
        assert_eq!(bash("git status", ws.path()), PermissionVerdict::AllowWithoutAsking);
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


    /// A promisor remote can make a read-only git command execute a helper during lazy fetch.
    /// With `promisor`, `partialclonefilter`, an `ext::` URL and `protocol.ext.allow=always`,
    /// `git show <missing-oid>` can run the configured program. The promisor/filter keys are
    /// outside the inert allowlist, and the URL check also rejects the helper transport.
    #[test]
    fn a_promisor_remote_with_a_program_transport_url_cards_every_git_call() {
        let ws = Workspace::new();
        ws.write(
            ".git/config",
            "[remote \"audit\"]\n\turl = ext::sh -c id\n\tpromisor = true\n\tpartialclonefilter = blob:none\n\
             [protocol \"ext\"]\n\tallow = always\n",
        );
        let leaks = mismatches(
            &[
                "git show 1111111111111111111111111111111111111111",
                "git status",
                "git log",
            ],
            ws.path(),
            PermissionVerdict::AskTheUser,
        );
        assert!(leaks.is_empty(), "promisor+ext:: allowed without a card: {leaks:#?}");

        // The program transport alone, in an otherwise ordinary remote URL, cards too.
        let plain = Workspace::new();
        plain.write(".git/config", "[remote \"origin\"]\n\turl = ext::sh -c id\n");
        assert_eq!(
            bash("git log", plain.path()),
            PermissionVerdict::AskTheUser,
            "ext:: url"
        );
        let fd = Workspace::new();
        fd.write(".git/config", "[remote \"origin\"]\n\turl = fd::7\n");
        assert_eq!(bash("git log", fd.path()), PermissionVerdict::AskTheUser, "fd:: url");
        let helper = Workspace::new();
        helper.write(".git/config", "[remote \"origin\"]\n\turl = evil::whatever\n");
        assert_eq!(
            bash("git log", helper.path()),
            PermissionVerdict::AskTheUser,
            "an arbitrary <transport>:: helper"
        );
    }

    /// `blame.ignoreRevsFile` can make `git blame` read an outside file and include its first
    /// line in an error. Config keys outside the inert allowlist require cards even when they
    /// run no program.
    #[cfg(unix)]
    #[test]
    fn a_config_key_that_reads_an_external_file_or_is_simply_unknown_cards_every_git_call() {
        for config in [
            "[blame]\n\tignoreRevsFile = /etc/os-release\n",
            "[blame]\n\tignoreRevsFile = ../secret\n",
            // Any key the allowlist does not name is a card, program or not: default-deny.
            "[core]\n\tuntrackedCache = true\n",
            "[log]\n\tshowSignature = true\n",
            "[remote \"origin\"]\n\tpromisor = true\n",
            "[extensions]\n\tpartialClone = origin\n",
        ] {
            let ws = Workspace::new();
            ws.write(".git/config", config);
            let leaks = mismatches(
                &["git blame src/main.rs", "git status", "git log"],
                ws.path(),
                PermissionVerdict::AskTheUser,
            );
            assert!(leaks.is_empty(), "config {config:?}: {leaks:#?}");
        }
    }

    /// An option-shaped argument can be a filename: `cat -- -n` follows a symlink named `-n`
    /// even when no entry named `n` exists. Check the literal spelling as a path so removing
    /// the leading dash cannot bypass containment.
    #[cfg(unix)]
    #[test]
    fn a_complete_option_shaped_filename_is_checked_as_a_path() {
        let ws = Workspace::new();
        let outside = Outside::new();
        ws.link("-n", outside.secret());
        for command in ["cat -- -n", "head -- -n", "wc -c -- -n", "grep x -- -n", "cat -n"] {
            assert_eq!(bash(command, ws.path()), PermissionVerdict::AskTheUser, "`{command}`");
        }
        // A real regular file named `-n` inside the root is fine: the literal check contains it.
        let inside = Workspace::new();
        inside.write("-n", "hello\n");
        assert_eq!(bash("cat -- -n", inside.path()), PermissionVerdict::AllowWithoutAsking);
        // And an ordinary `-n` option with no such file is untouched.
        assert_eq!(
            bash("grep -n main src/main.rs", inside.path()),
            PermissionVerdict::AllowWithoutAsking
        );
    }

    /// A symlink at `.git/objects` or another git read path can expose another repository's
    /// objects without `objects/info/alternates`. Discovery follows links when testing
    /// searchability, so containment must be checked separately.
    #[cfg(unix)]
    #[test]
    fn a_git_read_path_that_escapes_the_git_directory_cards() {
        for escaping in ["objects", "refs", "packed-refs"] {
            let ws = Workspace::new();
            let outside = Outside::new();
            let inside = ws.path().join(".git").join(escaping);
            let _ = std::fs::remove_dir_all(&inside);
            let _ = std::fs::remove_file(&inside);
            std::os::unix::fs::symlink(&outside.dir, &inside).unwrap();
            assert_eq!(
                bash("git show HEAD", ws.path()),
                PermissionVerdict::AskTheUser,
                "`.git/{escaping}` symlinked out"
            );
        }
    }

    /// A submodule's `commondir` can select a different common directory whose config runs
    /// `core.fsmonitor` during the superproject's `git status`. Check that common config as
    /// well as the submodule's own git directory.
    #[cfg(unix)]
    #[test]
    fn a_submodule_commondir_pointing_at_a_foreign_common_config_cards() {
        let ws = Workspace::new();
        let foreign = Outside::new();
        std::fs::write(foreign.dir.join("config"), "[core]\n\tfsmonitor = ./x.sh\n").unwrap();
        ws.write(".git/modules/sub/HEAD", "ref: refs/heads/main\n");
        ws.write(".git/modules/sub/commondir", format!("{}\n", foreign.dir.display()));
        assert_eq!(
            bash("git status", ws.path()),
            PermissionVerdict::AskTheUser,
            "the submodule's commondir points at a foreign common config"
        );
        // An unresolvable commondir cards too.
        let bad = Workspace::new();
        bad.write(".git/modules/sub/HEAD", "ref: refs/heads/main\n");
        bad.write(".git/modules/sub/commondir", "\n");
        assert_eq!(
            bash("git status", bad.path()),
            PermissionVerdict::AskTheUser,
            "empty commondir"
        );
    }

    /// The other side of the allowlist: the exact config key sets found in local repositories
    /// -- and what `git init`/`git clone` write -- read without a card. Built from
    /// `git config --local --list --name-only` on this host, 2026-09-28.
    #[test]
    fn ordinary_repository_configs_stay_card_free() {
        // A plain repository: what git init + a normal clone leave.
        let plain = "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n\
                     \tlogallrefupdates = true\n\tignorecase = true\n\tprecomposeunicode = true\n\
                     [remote \"origin\"]\n\turl = git@git.example.org:owner/x.git\n\
                     \tfetch = +refs/heads/*:refs/remotes/origin/*\n\
                     [branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n";
        // A repository the glab CLI has touched: it writes remote.<n>.glab-resolved, which git never reads.
        let glab = "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n\
                        \tlogallrefupdates = true\n\
                        [remote \"origin\"]\n\turl = https://git.example.org/owner/tool.git\n\
                        \tfetch = +refs/heads/*:refs/remotes/origin/*\n\tglab-resolved = base\n\
                        [branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n";
        for config in [plain, glab] {
            let ws = Workspace::new();
            ws.write(".git/config", config);
            let leaks = mismatches(
                &["git status", "git log", "git diff", "git show HEAD"],
                ws.path(),
                PermissionVerdict::AllowWithoutAsking,
            );
            assert!(leaks.is_empty(), "config {config:?} carded: {leaks:#?}");
        }

        // core.hooksPath at the repository's own (absolute) .git/hooks, and VS Code's
        // branch.<n>.vscode-merge-base, which git never reads.
        let ws = Workspace::new();
        std::fs::create_dir_all(ws.path().join(".git/hooks")).unwrap();
        ws.write(".git/hooks/pre-commit.sample", "#!/bin/sh\n");
        let hooks_abs = ws.path().join(".git/hooks");
        ws.write(
            ".git/config",
            format!(
                "[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n\
                 \tlogallrefupdates = true\n\thooksPath = {}\n\
                 [remote \"origin\"]\n\turl = git@git.example.org:owner/infra.git\n\
                 \tfetch = +refs/heads/*:refs/remotes/origin/*\n\
                 [branch \"main\"]\n\tremote = origin\n\tmerge = refs/heads/main\n\
                 \tvscode-merge-base = origin/main\n",
                hooks_abs.display()
            ),
        );
        let leaks = mismatches(
            &["git status", "git log", "git diff", "git show HEAD"],
            ws.path(),
            PermissionVerdict::AllowWithoutAsking,
        );
        assert!(leaks.is_empty(), "hooksPath-at-its-own-hooks style carded: {leaks:#?}");

        // A relative core.hooksPath at the default hooks directory reads without a card too.
        let rel = Workspace::new();
        std::fs::create_dir_all(rel.path().join(".git/hooks")).unwrap();
        rel.write(".git/config", "[core]\n\thooksPath = .git/hooks\n");
        assert_eq!(bash("git status", rel.path()), PermissionVerdict::AllowWithoutAsking);
    }


    /// A linked worktree's git directory as `git worktree add` lays it out (git 2.55.0, measured):
    /// `<common>/worktrees/<name>` holding `HEAD`, `commondir` = `../..` and, when given, the
    /// `gitdir` back-link naming the worktree's own `.git` file.
    fn fabricate_linked_worktree_git_dir(common: &Path, name: &str, back_link: Option<&str>) -> PathBuf {
        let git_dir = common.join("worktrees").join(name);
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/wt\n").unwrap();
        std::fs::write(git_dir.join("commondir"), "../..\n").unwrap();
        if let Some(back_link) = back_link {
            std::fs::write(git_dir.join("gitdir"), format!("{back_link}\n")).unwrap();
        }
        git_dir
    }

    /// A submodule's git directory as `git submodule add` lays it out (git 2.55.0, measured):
    /// `<super git dir>/modules/<path>`, a repository of its own whose config sets `core.worktree`
    /// back to the submodule's work tree -- here `worktree`, when given.
    fn fabricate_submodule_git_dir(super_git_dir: &Path, path: &str, worktree: Option<&str>) -> PathBuf {
        let git_dir = super_git_dir.join("modules").join(path);
        fabricate_git_dir(&git_dir);
        if let Some(worktree) = worktree {
            std::fs::write(git_dir.join("config"), format!("[core]\n\tworktree = {worktree}\n")).unwrap();
        }
        git_dir
    }

    /// A `.git` gitfile or symlink can select any repository on disk and expose its committed
    /// files. An outside repository belongs to this root only when it contains a back-link
    /// naming the root: a linked worktree's `gitdir` file or a submodule's `core.worktree`.
    #[cfg(unix)]
    #[test]
    fn a_dot_git_naming_a_repository_outside_the_root_cards_unless_that_repository_names_the_root() {
        let reads = ["git show HEAD:secret", "git log -p", "git status", "git diff"];
        let expect_card = |label: &str, project: &Workspace| {
            let leaks = mismatches(&reads, project.path(), PermissionVerdict::AskTheUser);
            assert!(leaks.is_empty(), "{label}: allowed without a card: {leaks:#?}");
        };
        let victim = Workspace::new();
        victim.write("secret", "private text\n");
        let victim_git = victim.path().join(".git");
        let gitfile = |dir: &Path| format!("gitdir: {}\n", dir.display());

        // 1. A plain gitfile naming the other repository.
        let project = Workspace::without_git();
        project.write(".git", gitfile(&victim_git));
        expect_card("a plain gitfile", &project);

        // 2. `.git` a symlink to it.
        let project = Workspace::without_git();
        project.link(".git", &victim_git);
        expect_card("a symlinked .git", &project);

        // 3. A gitfile borrowing a real linked worktree of it, whose back-link names that worktree.
        let real_wt = Workspace::without_git();
        let real_wt_dot_git = real_wt.path().join(".git");
        let borrowed =
            fabricate_linked_worktree_git_dir(&victim_git, "real", Some(&real_wt_dot_git.display().to_string()));
        real_wt.write(".git", gitfile(&borrowed));
        let project = Workspace::without_git();
        project.write(".git", gitfile(&borrowed));
        expect_card("a borrowed worktree git directory", &project);

        // 4. `.git` a symlink to that worktree's own gitfile: canonically it IS the file the back-link
        //    names, so only refusing a symlinked `.git` cards it.
        let project = Workspace::without_git();
        project.link(".git", &real_wt_dot_git);
        expect_card("a symlink to another worktree's gitfile", &project);

        // 5. A worktree git directory with no back-link at all.
        let unlinked = fabricate_linked_worktree_git_dir(&victim_git, "unlinked", None);
        let project = Workspace::without_git();
        project.write(".git", gitfile(&unlinked));
        expect_card("no gitdir back-link", &project);

        // 6. A back-link naming this root, in a directory that is not `<common>/worktrees/<name>`.
        let project = Workspace::without_git();
        let not_a_worktree = victim_git.join("elsewhere/wt");
        std::fs::create_dir_all(&not_a_worktree).unwrap();
        std::fs::write(not_a_worktree.join("HEAD"), "ref: refs/heads/wt\n").unwrap();
        std::fs::write(not_a_worktree.join("commondir"), "../..\n").unwrap();
        std::fs::write(
            not_a_worktree.join("gitdir"),
            format!("{}\n", project.path().join(".git").display()),
        )
        .unwrap();
        project.write(".git", gitfile(&not_a_worktree));
        expect_card("a back-link outside worktrees/", &project);

        // 7. A git directory inside the root whose `commondir` names the other repository.
        let project = Workspace::new();
        project.write(".git/commondir", format!("{}\n", victim_git.display()));
        expect_card("a commondir outside the root", &project);

        // 8. Another project's submodule: its `core.worktree` names that project's work tree.
        std::fs::create_dir_all(victim.path().join("libs/sub")).unwrap();
        let theirs = fabricate_submodule_git_dir(&victim_git, "libs/sub", Some("../../../../libs/sub"));
        let project = Workspace::without_git();
        project.write(".git", gitfile(&theirs));
        expect_card("another project's submodule", &project);

        // 9. A submodule git directory with no `core.worktree`.
        let no_worktree = fabricate_submodule_git_dir(&victim_git, "no-worktree", None);
        let project = Workspace::without_git();
        project.write(".git", gitfile(&no_worktree));
        expect_card("a submodule git directory with no core.worktree", &project);

        // 10. `core.worktree` naming this root, in a directory that is not under a `modules/`.
        let project = Workspace::without_git();
        let loose = victim_git.join("loose/sub");
        fabricate_git_dir(&loose);
        std::fs::write(
            loose.join("config"),
            format!(
                "[core]\n\tworktree = {}\n",
                project.path().canonicalize().unwrap().display()
            ),
        )
        .unwrap();
        project.write(".git", gitfile(&loose));
        expect_card("core.worktree outside modules/", &project);

        // 11. `core.worktree` in a subsection is not `core.worktree` to git.
        let project = Workspace::without_git();
        let subsectioned = fabricate_submodule_git_dir(&victim_git, "subsectioned", None);
        std::fs::write(
            subsectioned.join("config"),
            format!(
                "[core \"x\"]\n\tworktree = {}\n",
                project.path().canonicalize().unwrap().display()
            ),
        )
        .unwrap();
        project.write(".git", gitfile(&subsectioned));
        expect_card("core.worktree in a subsection", &project);

        // 12. A symlinked `.git` cards even where the back-link would hold: `.git` a link to a
        //     gitfile elsewhere, naming a submodule git directory whose `core.worktree` IS this root.
        //     (A worktree's back-link is compared with the literal `<root>/.git`, which a symlink
        //     never canonicalizes to, so case 4 cards without this check; this one does not.)
        let project = Workspace::without_git();
        let root = project.path().canonicalize().unwrap().display().to_string();
        let named_back = fabricate_submodule_git_dir(&victim_git, "named-back", Some(&root));
        let elsewhere = Workspace::without_git();
        elsewhere.write("gitfile", gitfile(&named_back));
        project.link(".git", elsewhere.path().join("gitfile"));
        expect_card("a symlinked .git whose target's back-link holds", &project);
        // The positive control: the same repository through a plain gitfile.
        let _ = std::fs::remove_file(project.path().join(".git"));
        project.write(".git", gitfile(&named_back));
        assert_eq!(
            bash("git log -p", project.path()),
            PermissionVerdict::AllowWithoutAsking,
            "the same back-link through a plain gitfile"
        );
    }

    /// A linked worktree reads without a card when its `gitdir` back-link names this root's
    /// `.git`, absolutely or relative to its git directory. A submodule reads without a card
    /// when its `core.worktree` resolves to this root.
    #[cfg(unix)]
    #[test]
    fn a_linked_worktree_or_submodule_that_names_the_root_back_reads_without_a_card() {
        let reads = ["git show HEAD:src/main.rs", "git log -p", "git status", "git diff"];
        let expect_allowed = |label: &str, root: &Path| {
            let cards = mismatches(&reads, root, PermissionVerdict::AllowWithoutAsking);
            assert!(cards.is_empty(), "{label}: carded: {cards:#?}");
        };
        // A worktree at `<repo>/.worktrees/<lane>` uses the git directory
        // `<repo>/.git/worktrees/<lane>`.
        let repo = Workspace::new();
        let lane = repo.path().join(".worktrees/lane");
        std::fs::create_dir_all(lane.join("src")).unwrap();
        std::fs::write(lane.join("src/main.rs"), "fn main() {}").unwrap();
        let back_link = lane.join(".git").display().to_string();
        let git_dir = fabricate_linked_worktree_git_dir(&repo.path().join(".git"), "lane", Some(&back_link));
        std::fs::write(lane.join(".git"), format!("gitdir: {}\n", git_dir.display())).unwrap();
        expect_allowed("a worktree inside its repository", &lane);

        // A relative back-link, from the worktree's git directory (`<repo>/.git/worktrees/sibling`,
        // four levels below the directory holding both workspaces) to its `.git`.
        let sibling = Workspace::without_git();
        let relative = format!(
            "../../../../{}/.git",
            sibling.path().file_name().unwrap().to_str().unwrap()
        );
        let git_dir = fabricate_linked_worktree_git_dir(&repo.path().join(".git"), "sibling", Some(&relative));
        sibling.write(".git", format!("gitdir: {}\n", git_dir.display()));
        assert_eq!(
            git_dir.join(&relative).canonicalize().unwrap(),
            sibling.path().join(".git").canonicalize().unwrap(),
            "the premise: the relative back-link names the sibling's .git"
        );
        expect_allowed("a relative back-link", sibling.path());

        // The project root as a submodule's work tree: `<super>/libs/sub`, the git directory at
        // `<super>/.git/modules/libs/sub` with `core.worktree = ../../../../libs/sub`, and the
        // relative gitfile git writes.
        let superproject = Workspace::new();
        let sub = superproject.path().join("libs/sub");
        std::fs::create_dir_all(sub.join("src")).unwrap();
        std::fs::write(sub.join("src/main.rs"), "fn main() {}").unwrap();
        fabricate_submodule_git_dir(
            &superproject.path().join(".git"),
            "libs/sub",
            Some("../../../../libs/sub"),
        );
        std::fs::write(sub.join(".git"), "gitdir: ../../.git/modules/libs/sub\n").unwrap();
        expect_allowed("a submodule's work tree", &sub);
    }

    /// What git itself writes, run for real on this host's git: `git worktree add` (a
    /// `.worktrees/<lane>` inside the repository, a sibling, and `--relative-paths`) and `git
    /// submodule add` (the project root being the submodule's work tree, and a nested one) read
    /// without a card. `git init --separate-git-dir` writes no back-link at all (measured: no
    /// `core.worktree`, nothing naming the work tree), so it cards; a worktree moved by hand keeps a
    /// stale back-link and cards until `git worktree repair`. Skipped where git is not installed.
    #[test]
    fn the_worktree_and_submodule_layouts_git_writes_are_judged_as_git_leaves_them() {
        if std::process::Command::new("git").arg("--version").output().is_err() {
            eprintln!("git is not installed; skipped");
            return;
        }
        let scratch = Workspace::without_git();
        let top = scratch.path().canonicalize().unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let status = std::process::Command::new("git")
                .current_dir(dir)
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "protocol.file.allow=always",
                ])
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env_remove("GIT_TEMPLATE_DIR")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?} in {}", dir.display());
        };
        let reads = ["git log --oneline", "git status --short", "git show HEAD"];
        let judged = |root: &Path, expected: PermissionVerdict| mismatches(&reads, root, expected);

        git(&top, &["init", "-q", "main"]);
        let main = top.join("main");
        git(&main, &["commit", "-q", "--allow-empty", "-m", "init"]);
        git(&main, &["worktree", "add", "-q", ".worktrees/lane", "-b", "lane"]);
        git(&main, &["worktree", "add", "-q", "../sibling", "-b", "sibling"]);
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                "--relative-paths",
                "../relative",
                "-b",
                "relative",
            ],
        );
        for root in [
            main.join(".worktrees/lane"),
            top.join("sibling"),
            top.join("relative"),
            main.clone(),
        ] {
            let cards = judged(&root, PermissionVerdict::AllowWithoutAsking);
            assert!(cards.is_empty(), "{}: {cards:#?}", root.display());
        }

        // Moved by hand: the back-link still names the old place, so it cards -- until repaired.
        std::fs::rename(top.join("sibling"), top.join("moved")).unwrap();
        let leaks = judged(&top.join("moved"), PermissionVerdict::AskTheUser);
        assert!(leaks.is_empty(), "a moved worktree: {leaks:#?}");
        git(&top.join("moved"), &["worktree", "repair"]);
        let cards = judged(&top.join("moved"), PermissionVerdict::AllowWithoutAsking);
        assert!(cards.is_empty(), "a repaired worktree: {cards:#?}");

        // Submodules: `libs/sub` of `super`, which itself holds `deep/nest`.
        for name in ["subsrc", "nested", "super"] {
            git(&top, &["init", "-q", name]);
            git(&top.join(name), &["commit", "-q", "--allow-empty", "-m", "init"]);
        }
        git(
            &top.join("subsrc"),
            &["submodule", "add", "-q", "../nested", "deep/nest"],
        );
        git(&top.join("subsrc"), &["commit", "-q", "-m", "nest"]);
        let superproject = top.join("super");
        git(&superproject, &["submodule", "add", "-q", "../subsrc", "libs/sub"]);
        git(&superproject, &["submodule", "update", "-q", "--init", "--recursive"]);
        for root in [
            superproject.clone(),
            superproject.join("libs/sub"),
            superproject.join("libs/sub/deep/nest"),
        ] {
            let cards = judged(&root, PermissionVerdict::AllowWithoutAsking);
            assert!(cards.is_empty(), "{}: {cards:#?}", root.display());
        }

        // `--separate-git-dir`: a gitfile naming a repository that names nothing back.
        let separate = top.join("separate.git");
        git(
            &top,
            &[
                "init",
                "-q",
                &format!("--separate-git-dir={}", separate.display()),
                "sepwt",
            ],
        );
        let leaks = judged(&top.join("sepwt"), PermissionVerdict::AskTheUser);
        assert!(leaks.is_empty(), "a separate git dir: {leaks:#?}");
    }

    /// Nested symlinks can expose another repository's objects even when top-level git entries
    /// stay inside. Links at `.git/objects/pack/pack-*.{idx,pack}` or `.git/objects/<xx>` can
    /// redirect `git show HEAD:<file>` to those objects. Every symlink in the git directory
    /// must resolve inside it or inside the project.
    #[cfg(unix)]
    #[test]
    fn a_symlink_anywhere_in_the_git_directory_that_leaves_it_and_the_project_cards() {
        for entry in [
            "objects/pack/pack-1.pack",
            "objects/ab",
            "index",
            "shallow",
            "refs/heads/main",
            "logs/HEAD",
            "worktrees/wt/index",
            "modules/sub/objects/pack/pack-1.pack",
        ] {
            let ws = Workspace::new();
            let outside = Outside::new();
            if entry.starts_with("modules/sub/") {
                fabricate_git_dir(&ws.path().join(".git/modules/sub"));
            }
            ws.link(&format!(".git/{entry}"), outside.secret());
            let leaks = mismatches(
                &["git show HEAD:secret", "git status", "git log"],
                ws.path(),
                PermissionVerdict::AskTheUser,
            );
            assert!(leaks.is_empty(), "`.git/{entry}` linked out: {leaks:#?}");
        }

        // A link to a DIRECTORY inside the project is not enough: nobody walked what is in it, and
        // here that holds a link out.
        let ws = Workspace::new();
        let outside = Outside::new();
        ws.link("vendor/ab/cdef", outside.secret());
        ws.link(".git/objects/ab", ws.path().join("vendor/ab"));
        assert_eq!(
            bash("git show HEAD", ws.path()),
            PermissionVerdict::AskTheUser,
            "a link to a project directory"
        );

        // A link that stays inside the git directory, or to a file inside the project, reads as before.
        let ws = Workspace::new();
        ws.write(".git/refs/heads/main", "1111111111111111111111111111111111111111\n");
        ws.link(".git/refs/heads/alias", ws.path().join(".git/refs/heads/main"));
        ws.write("scripts/pre-commit", "#!/bin/sh\n");
        ws.link(".git/hooks/pre-commit", ws.path().join("scripts/pre-commit"));
        assert_eq!(bash("git status", ws.path()), PermissionVerdict::AllowWithoutAsking);
    }

    /// Ordinary pull, rerere, branch and submodule settings are inert for the permitted
    /// read-only subcommands and should not require cards. Submodule update accepts only git's
    /// four named strategies; `!command` and other values still require cards.
    #[test]
    fn benign_keys_pull_rerere_branch_and_submodule_use_leave_stay_card_free() {
        let reads = ["git status", "git log", "git diff", "git show HEAD"];
        for config in [
            "[pull]\n\trebase = true\n",
            "[pull]\n\tff = only\n",
            "[rerere]\n\tenabled = true\n\tautoupdate = true\n",
            "[branch \"main\"]\n\tdescription = what this branch is for\n\trebase = interactive\n",
            "[submodule \"libs/sub\"]\n\tupdate = checkout\n\tbranch = main\n",
            "[submodule \"libs/sub\"]\n\tupdate = rebase\n",
            "[submodule \"libs/sub\"]\n\tupdate = merge\n",
            "[submodule \"libs/sub\"]\n\tupdate = none\n\tbranch = .\n",
            // `git worktree add --relative-paths` writes this (git 2.55.0, measured).
            "[core]\n\trepositoryformatversion = 1\n[extensions]\n\trelativeWorktrees = true\n",
        ] {
            let ws = Workspace::new();
            ws.write(".git/config", config);
            let cards = mismatches(&reads, ws.path(), PermissionVerdict::AllowWithoutAsking);
            assert!(cards.is_empty(), "config {config:?} carded: {cards:#?}");
        }
        for config in [
            "[submodule \"libs/sub\"]\n\tupdate = !sh x\n",
            "[submodule \"libs/sub\"]\n\tupdate = \"!sh x\"\n",
            "[submodule \"libs/sub\"]\n\tupdate\n",
            "[submodule \"libs/sub\"]\n\tupdate = Checkout\n",
            // git-lfs is not installed on this host, so the values `git lfs install --local` writes
            // could not be read off it: `filter.lfs.*` keeps carding, whatever its values.
            "[filter \"lfs\"]\n\tclean = git-lfs clean -- %f\n\tsmudge = git-lfs smudge -- %f\n\
             \tprocess = git-lfs filter-process\n\trequired = true\n",
            // Partial-clone config requires a card.
            "[remote \"origin\"]\n\tpromisor = true\n\tpartialclonefilter = blob:none\n",
            "[extensions]\n\tpartialClone = origin\n",
        ] {
            let ws = Workspace::new();
            ws.write(".git/config", config);
            let leaks = mismatches(&reads, ws.path(), PermissionVerdict::AskTheUser);
            assert!(leaks.is_empty(), "config {config:?} allowed: {leaks:#?}");
        }
    }

    /// Exact `--no-pager` and `-P` global options only disable the pager, so read-only
    /// subcommands following them need no card. Abbreviations, combined forms and other global
    /// options still require cards.
    #[test]
    fn git_no_pager_before_a_read_only_subcommand_needs_no_card_and_other_global_options_still_card() {
        let ws = Workspace::new();
        for command in [
            "git --no-pager log --oneline",
            "git --no-pager status --short",
            "git -P diff",
            "git -P --no-pager show HEAD:src/main.rs",
        ] {
            assert_eq!(
                bash(command, ws.path()),
                PermissionVerdict::AllowWithoutAsking,
                "`{command}`"
            );
        }
        for command in [
            "git --no-pager",
            "git --no-pager commit -m x",
            "git -P push",
            "git -p log",
            "git --paginate log",
            "git -C src log",
            "git --no-pager -C src log",
            "git -c color.ui log",
            "git --git-dir .git log",
            "git --work-tree src log",
            "git --exec-path log",
            "git --namespace x log",
            "git --bare log",
            "git --no-replace-objects log",
            "git --literal-pathspecs log",
            "git --no-pag log",
            "git -Pp log",
            "git -PC src log",
        ] {
            assert_eq!(bash(command, ws.path()), PermissionVerdict::AskTheUser, "`{command}`");
        }
    }

    /// Creates `n` symlinks named `<prefix><i>` in `dir`. Each target is padded with `d/../` up
    /// to about `pad` bytes, and the last names `last`. The padding preserves the destination
    /// while increasing resolution work.
    #[cfg(unix)]
    fn padded_chain(dir: &Path, prefix: &str, n: usize, pad: usize, last: &str) {
        std::fs::create_dir_all(dir.join("d")).unwrap();
        let padding = "d/../".repeat(pad / 5);
        for i in 0..n {
            let next = if i + 1 == n {
                last.to_string()
            } else {
                format!("{prefix}{}", i + 1)
            };
            std::os::unix::fs::symlink(format!("{padding}{next}"), dir.join(format!("{prefix}{i}"))).unwrap();
        }
    }

    /// Project-controlled paths and `.git` links must have bounded resolution costs because
    /// classification runs on the GTK thread. Padded link chains can resolve inside the project
    /// yet make repeated `canonicalize` calls block the UI.
    ///
    /// The resolver reads each link before following it and requires a card for excessive
    /// target length, components, `..`, hops or steps. A git directory with more links than the
    /// policy checks also requires a card.
    #[cfg(unix)]
    #[test]
    fn a_path_whose_links_would_take_long_to_resolve_cards_instead() {
        // The review's shape, scaled down: links in `.git` whose every hop resolves inside it.
        let ws = Workspace::new();
        padded_chain(&ws.path().join(".git/links"), "h", 39, 4000, "../HEAD");
        let started = std::time::Instant::now();
        assert_eq!(
            bash("git status", ws.path()),
            PermissionVerdict::AskTheUser,
            "padded links in .git"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );

        // A short link in `.git` whose chain continues in the project, where the walk never goes.
        let ws = Workspace::new();
        padded_chain(&ws.path().join("chain"), "h", 39, 4000, "../src/main.rs");
        ws.link(".git/hooks/pre-commit", "../../chain/h0");
        assert_eq!(
            bash("git status", ws.path()),
            PermissionVerdict::AskTheUser,
            "a chain in the project"
        );

        // A config value through such a chain, many times over.
        let ws = Workspace::new();
        padded_chain(&ws.path().join("chain"), "w", 39, 4000, "..");
        ws.write(".git/config", "[core]\n\tworktree = ../chain/w0\n".repeat(20));
        assert_eq!(
            bash("git status", ws.path()),
            PermissionVerdict::AskTheUser,
            "core.worktree chains"
        );

        // An argument through one; repeating it costs nothing more.
        let ws = Workspace::new();
        padded_chain(&ws.path().join("chain"), "c", 39, 4000, "../src/main.rs");
        let command = format!("cat {}", "chain/c0 ".repeat(500));
        let started = std::time::Instant::now();
        assert_eq!(
            bash(&command, ws.path()),
            PermissionVerdict::AskTheUser,
            "argument chains"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        // And a Read of one.
        assert_eq!(
            verdict("Read", json!({ "file_path": ws.path().join("chain/c0") }), ws.path()),
            PermissionVerdict::AskTheUser,
            "Read through a chain"
        );
    }

    /// The bounds themselves, each at its limit and one past it. Ordinary layouts sit far inside
    /// them: a hook symlinked into the project is one hop of a short target with two `..`.
    #[cfg(unix)]
    #[test]
    fn each_bound_on_resolving_a_link_holds_at_its_limit_and_cards_past_it() {
        let allowed = |ws: &Workspace, label: &str| {
            assert_eq!(
                bash("git status", ws.path()),
                PermissionVerdict::AllowWithoutAsking,
                "{label}"
            );
        };
        let carded = |ws: &Workspace, label: &str| {
            assert_eq!(bash("git status", ws.path()), PermissionVerdict::AskTheUser, "{label}");
        };
        // The number of links in a git directory.
        for (n, ok) in [
            (MAX_GIT_DIRECTORY_SYMLINKS, true),
            (MAX_GIT_DIRECTORY_SYMLINKS + 1, false),
        ] {
            let ws = Workspace::new();
            ws.write(".git/description", "x\n");
            for i in 0..n {
                ws.link(&format!(".git/l/{i}"), "../description");
            }
            if ok {
                allowed(&ws, "links at the limit")
            } else {
                carded(&ws, "one link too many")
            }
        }
        // A target's `..`: `d/../` repeated, each `..` a component too.
        for (parents, ok) in [(MAX_LINK_TARGET_PARENTS, true), (MAX_LINK_TARGET_PARENTS + 1, false)] {
            let ws = Workspace::new();
            ws.write(".git/l/f", "x\n");
            std::fs::create_dir_all(ws.path().join(".git/l/d")).unwrap();
            ws.link(".git/l/x", format!("{}f", "d/../".repeat(parents)));
            if ok {
                allowed(&ws, "`..` at the limit")
            } else {
                carded(&ws, "one `..` too many")
            }
        }
        // A target's components, as `./` (no `..` at all).
        let limit = MAX_LINK_TARGET_COMPONENTS;
        for (components, ok) in [(limit, true), (limit + 1, false)] {
            let ws = Workspace::new();
            ws.write(".git/l/f", "x\n");
            ws.link(".git/l/x", format!("{}f", "./".repeat(components - 1)));
            if ok {
                allowed(&ws, "components at the limit")
            } else {
                carded(&ws, "one component too many")
            }
        }
        // A target's bytes: one component, a long name.
        for (bytes, ok) in [(MAX_LINK_TARGET_BYTES, true), (MAX_LINK_TARGET_BYTES + 1, false)] {
            let ws = Workspace::new();
            let name = "n".repeat(200);
            let dirs = bytes / 201;
            let rest = bytes - dirs * 201;
            let mut target = format!("{name}/").repeat(dirs);
            target.push_str(&"f".repeat(rest));
            let file = ws.path().join(".git/l").join(&target);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, "x\n").unwrap();
            assert_eq!(target.len(), bytes);
            ws.link(".git/l/x", &target);
            if ok {
                allowed(&ws, "bytes at the limit")
            } else {
                carded(&ws, "one byte too many")
            }
        }
        // Hops, one link naming the next.
        for (hops, ok) in [(MAX_LINK_HOPS, true), (MAX_LINK_HOPS + 1, false)] {
            let ws = Workspace::new();
            ws.write(".git/l/f", "x\n");
            for i in 0..hops {
                let next = if i + 1 == hops {
                    "f".to_string()
                } else {
                    format!("h{}", i + 1)
                };
                ws.link(&format!(".git/l/h{i}"), next);
            }
            // The walk starts every link of the chain; only `h0` is the one that takes every hop.
            if ok {
                allowed(&ws, "hops at the limit")
            } else {
                carded(&ws, "one hop too many")
            }
        }
        // Steps: eight hops of `d/..` pairs, 1 + 8 x (2p + 1) lookups -- 249 with 15 pairs, 265
        // with 16, every target inside its own limits.
        for (pairs, ok) in [(15, true), (16, false)] {
            let ws = Workspace::new();
            ws.write(".git/l/f", "x\n");
            std::fs::create_dir_all(ws.path().join(".git/l/d")).unwrap();
            for i in 0..MAX_LINK_HOPS {
                let next = if i + 1 == MAX_LINK_HOPS {
                    "f".to_string()
                } else {
                    format!("h{}", i + 1)
                };
                ws.link(&format!(".git/l/h{i}"), format!("{}{next}", "d/../".repeat(pairs)));
            }
            if ok {
                allowed(&ws, "steps under the limit")
            } else {
                carded(&ws, "steps past the limit")
            }
        }
        // The depth a lookup is made at: a file whose path has MAX_RESOLVED_DEPTH components (the
        // root directory counted), and one deeper.
        for (extra, ok) in [(0, true), (1, false)] {
            let ws = Workspace::new();
            let root_depth = ws.path().canonicalize().unwrap().components().count();
            let dirs = MAX_RESOLVED_DEPTH - root_depth - 1 + extra;
            let relative = format!("{}f", "a/".repeat(dirs));
            ws.write(&relative, "x\n");
            let expected = if ok {
                PermissionVerdict::AllowWithoutAsking
            } else {
                PermissionVerdict::AskTheUser
            };
            assert_eq!(
                bash(&format!("cat {relative}"), ws.path()),
                expected,
                "depth, {extra} past the limit"
            );
        }
    }

    /// Two places a lexical reading of a path would differ from the kernel's. A gitfile may name
    /// its git directory through a padded chain (resolved within bounds, so it cards rather than
    /// costing seconds); and `<common>/modules` may itself be a link, so an entry in it naming
    /// `../evil` means the directory beside the link's TARGET, as git takes it -- a scan resolving
    /// that against `modules` would check a directory git never uses and miss the one it does.
    #[cfg(unix)]
    #[test]
    fn git_paths_are_resolved_physically_and_within_bounds() {
        let ws = Workspace::without_git();
        fabricate_git_dir(&ws.path().join("real.git"));
        padded_chain(&ws.path().join("chain"), "g", 39, 4000, "../real.git");
        ws.write(".git", "gitdir: chain/g0\n");
        assert_eq!(
            bash("git status", ws.path()),
            PermissionVerdict::AskTheUser,
            "a padded gitdir"
        );
        ws.write(".git", "gitdir: real.git\n");
        assert_eq!(
            bash("git status", ws.path()),
            PermissionVerdict::AllowWithoutAsking,
            "the control"
        );
        // Judge the git directory by its resolved location. A gitfile naming an in-project link
        // to an outside repository still needs the outside repository's back-link check.
        let victim = Workspace::new();
        victim.write("secret", "private text\n");
        ws.link("x", victim.path().join(".git"));
        ws.write(".git", "gitdir: x\n");
        assert_eq!(
            bash("git show HEAD:secret", ws.path()),
            PermissionVerdict::AskTheUser,
            "gitdir: x -> outside"
        );

        let ws = Workspace::new();
        fabricate_git_dir(&ws.path().join(".git/sub/evil"));
        ws.write(".git/sub/evil/config", "[core]\n\tfsmonitor = ./fsm.sh\n");
        std::fs::create_dir_all(ws.path().join(".git/sub/m")).unwrap();
        ws.link(".git/modules", "sub/m");
        ws.link(".git/sub/m/x", "../evil");
        assert_eq!(
            bash("git status", ws.path()),
            PermissionVerdict::AskTheUser,
            "modules/x -> ../evil through a linked modules/"
        );
    }

    /// The work one classification may do is bounded, and running out always cards -- whatever
    /// the check that ran out would have answered. The review's other bound (entries walked) did
    /// not bound time; nor did a config's `include.path` fan-out, followed without limit: four
    /// levels of 30 includes each cost 810,000 file reads, 1.27 s in a release build before it
    /// ALLOWED (measured 2026-09-28), and git's depth limit of 10 lets it reach 30^10.
    #[test]
    fn running_out_of_work_cards_and_include_fan_out_runs_out() {
        let ws = Workspace::new();
        let input = json!({ "command": "git status" });
        let with_budget = |work_budget| Surroundings {
            home: None,
            git_location_from_environment: false,
            work_budget,
        };
        let starved = classify_in("Bash", &input, ws.path(), &with_budget(10));
        assert_eq!(starved.verdict, PermissionVerdict::AskTheUser);
        assert_eq!(starved.reason, REASON_TOO_MUCH_WORK);
        let fed = classify_in("Bash", &input, ws.path(), &with_budget(MAX_WORK_PER_CLASSIFICATION));
        assert_eq!(fed.verdict, PermissionVerdict::AllowWithoutAsking);
        // A tool that touches no file needs no budget at all.
        let todo = classify_in("TodoWrite", &json!({}), ws.path(), &with_budget(0));
        assert_eq!(todo.verdict, PermissionVerdict::AllowWithoutAsking);

        // include.path fan-out: 4 levels of 30 (810,000 reads) runs out and cards.
        let ws = Workspace::new();
        let fan = "\tpath = next.inc\n".repeat(30);
        for level in (0..4).rev() {
            let body = if level == 3 {
                String::new()
            } else {
                format!("[include]\n{}", fan.replace("next", &format!("l{}", level + 1)))
            };
            ws.write(&format!(".git/l{level}.inc"), body);
        }
        ws.write(".git/config", format!("[include]\n{}", fan.replace("next", "l0")));
        let started = std::time::Instant::now();
        let fanned = classify_permission_request("Bash", &input, ws.path());
        assert_eq!(fanned.verdict, PermissionVerdict::AskTheUser);
        assert_eq!(fanned.reason, REASON_TOO_MUCH_WORK);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    /// Leading git global options such as `-C`, `--git-dir`, `--work-tree` and `--namespace`
    /// can select a repository the root's repository check never inspected. Their
    /// separate-argument forms carry no `=`. They require a card with a reason no saved rule
    /// replaces, so `Bash(git -C *)` cannot authorize outside reads. A subcommand such as `git
    /// push` keeps its replaceable reason.
    #[cfg(unix)]
    #[test]
    fn a_saved_rule_never_answers_a_git_global_option() {
        let ws = Workspace::new();
        let outside = Workspace::new();
        outside.write("secret", "private text\n");
        ws.write(
            "sub2/.git",
            format!("gitdir: {}\n", outside.path().join(".git").display()),
        );
        for command in [
            "git -C sub2 show HEAD:secret",
            "git --no-pager -C sub2 log",
            "git -P -C sub2 status",
            "git --git-dir sub2/.git show HEAD:secret",
            "git --work-tree sub2 status",
            "git --namespace n log",
            "git -c color.ui log",
            "git --exec-path log",
        ] {
            let input = json!({ "command": command });
            let plain = classify_permission_request("Bash", &input, ws.path());
            assert_eq!(plain.reason, REASON_GIT_GLOBAL_OPTION, "`{command}`");
            assert!(!REPLACEABLE_BY_A_RULE.contains(&plain.reason));
            let words: Vec<&str> = command.split_whitespace().collect();
            let rule =
                crate::permission_rules::PrefixRule::parse(&format!("Bash({} {} *)", words[0], words[1])).unwrap();
            let with_rule = classify_with_rules(
                "Bash",
                &input,
                ws.path(),
                &crate::permission_rules::PrefixRules::new(vec![rule]),
            );
            assert_eq!(
                with_rule.verdict,
                PermissionVerdict::AskTheUser,
                "`{command}` with its rule saved"
            );
        }
        // A genuine subcommand stays one a rule may answer, its repository checked at the root.
        let input = json!({ "command": "git push origin main" });
        assert_eq!(
            classify_permission_request("Bash", &input, ws.path()).reason,
            REASON_GIT_SUBCOMMAND_NOT_READ_ONLY
        );
        let rule = crate::permission_rules::PrefixRule::parse("Bash(git push *)").unwrap();
        assert_eq!(
            classify_with_rules(
                "Bash",
                &input,
                ws.path(),
                &crate::permission_rules::PrefixRules::new(vec![rule])
            )
            .verdict,
            PermissionVerdict::AllowWithoutAsking
        );
    }
}
