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
    Classification { verdict: PermissionVerdict::AllowWithoutAsking, reason }
}

fn ask(reason: &'static str) -> Classification {
    Classification { verdict: PermissionVerdict::AskTheUser, reason }
}

/// Tools the CLI asks about every time in `default` mode, decidable from the name alone.
///
/// `WebFetch` is here for a different reason from the rest -- see the module doc's rule (d): the
/// CLI does exempt some documentation domains, and this version refuses to guess which.
pub const ALWAYS_ASK_TOOLS: &[&str] =
    &["Write", "Edit", "MultiEdit", "NotebookEdit", "WebSearch", "WebFetch"];

/// Tools that change no file, run no command and reach no network -- the CLI does not ask about
/// these and neither does this.
///
/// **`Task` is here on documentation alone and is the weakest row in this table.** The CLI does not
/// prompt for a subagent launch because the subagent's own tool calls are checked individually;
/// that is stated in the docs and is *not* something this project has observed -- nothing here has
/// confirmed that a subagent's inner calls reach this same `PreToolUse` gate. If that assumption is
/// ever disproved, this entry is a hole and must be the first thing removed.
pub const NEVER_ASK_TOOLS: &[&str] = &["TodoWrite", "Task"];

/// The read-only `Bash` commands the CLI's own documentation names, verbatim and in its order.
///
/// Being on this list is necessary and **nowhere near sufficient**: see
/// `classify_bash` below and the module doc. `echo` is on this list and `echo hi >
/// c.txt` was measured DENIED.
pub const READ_ONLY_BASH_COMMANDS: &[&str] = &[
    "ls", "cat", "echo", "pwd", "head", "tail", "grep", "find", "wc", "which", "diff", "stat", "du",
    "cd",
];

/// `git` subcommands that only read. Deliberately narrower than "read-only git forms" in the docs:
/// `branch`, `tag`, `config` and `stash` all have writing forms distinguished only by their
/// arguments, and distinguishing them is exactly the kind of partial parse this module refuses to
/// do. `git commit` was measured denied; nothing here would have allowed it anyway.
pub const READ_ONLY_GIT_SUBCOMMANDS: &[&str] =
    &["status", "log", "diff", "show", "blame", "rev-parse", "ls-files", "shortlog", "describe"];

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
    '|', '&', ';', '<', '>', '$', '`', '(', ')', '{', '}', '[', ']', '*', '?', '~', '!', '#', '\\',
    '"', '\'', '\n', '\r',
];

/// `find` actions that write or execute. `find` is on the CLI's read-only list, and `find . -delete`
/// is on nobody's.
const FIND_ACTIONS_THAT_ARE_NOT_READS: &[&str] =
    &["-exec", "-execdir", "-ok", "-okdir", "-delete", "-fprint", "-fprintf", "-fls"];

/// The one entry point. `tool_name` and `input` are the `PermissionRequested` event's own fields,
/// verbatim; `project_root` is the session's canonical project directory
/// (`neovibe_core::project_root`), which is the boundary `Read`/`Grep`/`Glob` are judged against.
///
/// Total: every input produces a verdict, and every path that is not a positive, confident match
/// produces [`PermissionVerdict::AskTheUser`].
pub fn classify_permission_request(tool_name: &str, input: &Value, project_root: &Path) -> Classification {
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

    match tool_name {
        // `file_path` is required by the tool's own schema, so its absence is a malformed call.
        "Read" => classify_path(input, "file_path", PathField::Required, project_root),
        // `path` is optional for both, and its absence means "the working directory" -- which is
        // inside the root by definition.
        "Grep" | "Glob" => classify_path(input, "path", PathField::Optional, project_root),
        "Bash" => classify_bash(input),
        _ => ask("this tool is not in the policy's table"),
    }
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
/// - the root itself will not canonicalize -- if the boundary is unknown, nothing can be judged
///   against it.
///
/// Resolving BEFORE comparing is the whole design. A textual check would be defeated by
/// `src/../../etc/passwd` and by any symlink; `canonicalize` is a real syscall that follows both.
fn classify_path(
    input: &Value,
    field: &str,
    requirement: PathField,
    project_root: &Path,
) -> Classification {
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

    let Ok(root) = project_root.canonicalize() else {
        return ask("the project root itself could not be resolved");
    };
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
    if resolved.starts_with(&root) {
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
fn classify_bash(input: &Value) -> Classification {
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

    match program {
        "git" => match arguments.first() {
            Some(sub) if READ_ONLY_GIT_SUBCOMMANDS.contains(sub) => {
                allow("a read-only git subcommand")
            }
            _ => ask("this git subcommand is not one of the read-only forms"),
        },
        "find" if arguments.iter().any(|a| FIND_ACTIONS_THAT_ARE_NOT_READS.contains(a)) => {
            ask("this find call carries an action that writes or executes")
        }
        other if READ_ONLY_BASH_COMMANDS.contains(&other) => {
            allow("a read-only command from the CLI's own list, with no shell syntax")
        }
        _ => ask("this command is not on the CLI's read-only list"),
    }
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
            let root = std::env::temp_dir()
                .join(format!("agent-permission-policy-{}", uuid::Uuid::new_v4()));
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
            assert_eq!(verdict(tool, json!([1, 2, 3]), ws.path()), PermissionVerdict::AskTheUser);
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
            verdict("Read", json!({ "file_path": "src/../../../../etc/hostname" }), ws.path()),
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
        for command in READ_ONLY_BASH_COMMANDS {
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
            "echo hi > c.txt",         // redirect
            "echo hi >> c.txt",        // appending redirect
            "cat < a.txt",             // input redirect
            "ls | tee out.txt",        // pipeline
            "ls && rm -rf .",          // chain
            "ls; rm -rf .",            // sequence
            "echo $(rm -rf .)",        // command substitution
            "echo `rm -rf .`",         // backtick substitution
            "ls *.rs",                 // unquoted glob
            "cat 'a b.txt'",           // quoting
            "ls ~",                    // home expansion
            "ls \\\n -l",              // escape and newline
            "ls & ",                   // background
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
        for command in ["rm b.txt", "mv a b", "curl http://example.com", "python script.py", "npm ci"] {
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
        for sub in ["commit", "push", "checkout", "branch", "tag", "config", "stash", "reset"] {
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
        assert_eq!(bash("find . -name main.rs", ws.path()), PermissionVerdict::AllowWithoutAsking);
        for command in ["find . -delete", "find . -exec rm -rf . +", "find . -fprint out.txt"] {
            assert_eq!(bash(command, ws.path()), PermissionVerdict::AskTheUser, "`{command}`");
        }
    }
}
