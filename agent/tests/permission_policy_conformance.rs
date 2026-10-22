//! The permission policy, checked against the real Claude CLI instead of against a table copied
//! out of documentation.
//!
//! # The oracle, and why one exists at all
//!
//! `agent::permission_policy` reproduces the CLI's *interactive* `default`-mode rules: which tool
//! calls it would put in front of a human. An interactive prompt is not something a test can read
//! -- but on 2026-09-19, on CLI 2.1.272, the headless CLI was measured doing the same
//! classification with a different ending:
//!
//! - `claude --print` with **no** `--permission-mode` flag (`init.permissionMode` reports
//!   `default`) ran `Read` and **denied** `Write` -- `is_error: true`, "Claude requested permissions
//!   to write to ..., but you haven't granted it yet", and the file was really not created.
//! - `claude --print --permission-mode auto` ran `Bash`, `Read` **and** `Write` with no prompt,
//!   because `auto` means "a classifier model approves in your place".
//!
//! So under `default`, with no hook installed, **what the CLI allows is what needs no human, and
//! what it denies is what does**. That is a real oracle, and it is why this file exists: a table
//! transcribed from docs drifts silently when a release changes the rules, and a test against the
//! binary does not.
//!
//! # What this asserts, exactly
//!
//! Not equality. The policy is deliberately **stricter** than the CLI in several places (it refuses
//! to parse compound shell syntax, it always cards `WebFetch`, it cards any tool name it has never
//! heard of), and each of those is a documented decision rather than a defect. So the assertion is
//! one-sided:
//!
//! > the policy is never MORE PERMISSIVE than the CLI.
//!
//! A call the CLI denied and the policy would have auto-allowed is a security defect and fails the
//! test, naming the call. A call the CLI allowed and the policy cards is printed, counted, and
//! passes -- that is the cost side of rule (c), and seeing it listed is the point.
//!
//! # Running it
//!
//! Real turns, real money, on the TEST Claude profile -- never a personal login:
//!
//! ```sh
//! claude --version    # record the build; nothing prints it for you
//! cargo test -p agent --test permission_policy_conformance -- --ignored --nocapture
//! ```
//!
//! **First run: 2026-09-19, CLI 2.1.272 on the TEST profile, passed** with four probes: the CLI and
//! the policy agreed on four calls, and the policy was stricter on one (`pwd; readlink -f /tmp; ls
//! -la`, which the CLI ran and the policy cards because it contains `;`). Nothing was more
//! permissive. That run could not have noticed a probe that produced no call at all -- the only
//! vacuity check was global -- and no probe reached a path outside the project, which is the one
//! place the policy allows by *reasoning about a path* rather than by name. Both were fixed the same
//! day: every probe now names the tool it exists to exercise and fails if that tool was never
//! called, and two probes were added (a `Read` of an absolute path outside the root, and one that
//! climbs out with `..`).
//!
//! **2026-09-28 (indirect reads), CLI 2.1.283 on the TEST profile: passed** with twelve probes, four of
//! them new for the cards added then (`find -files0-from`, `wc --files0-from`, `diff` of two
//! directories, read-only git in a project inside a larger repository). Nothing was more permissive;
//! five calls were stricter -- the four new ones, all of which the CLI ran, and the old piped
//! `grep`. Run with `XDG_STATE_HOME=$HOME/.cache/nv-policy/state` so test state stays separate.
//!
//! **2026-09-28 (config reads and option-shaped paths), CLI 2.1.283 on the TEST profile: passed** with fourteen probes,
//! two more covering indirect reads -- `git blame` in a repo whose config reads an outside file
//! and `cat -- -n` through an option-shaped symlink. The CLI ran both and
//! the policy cards both, so nothing was more permissive and both are counted stricter.
//!
//! **2026-09-28 (repository back-links), CLI 2.1.283 on the TEST profile: passed** with fifteen probes, one
//! more for the repository boundary: `git show HEAD:outside.txt` in a project whose `.git` is a
//! gitfile naming another repository. The CLI ran it without refusing (the same command, run for real,
//! prints the other repository's file); the policy cards it. Nothing was more permissive; seven calls
//! were stricter (the previous six and this one).
//!
//! **2026-09-28 (the acceptEdits fast path): NOT YET RUN.** `Write`, `Edit` and
//! `NotebookEdit` stopped reproducing `default` (which denies every one of them) and started
//! reproducing `acceptEdits` instead (module doc on `permission_policy`, "The acceptEdits fast
//! path"). Three probes were changed accordingly: the one plain in-project `Write` probe is now run
//! under `--permission-mode acceptEdits` rather than no flag, its expected side effect flipped from
//! "must not exist" to "no longer checked" (the CLI is now expected to create it, same as the
//! policy), and two probes were added alongside it -- an `Edit` of an existing in-project file, and
//! a `Write` under a protected directory (`.vscode/settings.json`), both also under `acceptEdits`,
//! to state the parity claim for BOTH halves of the fast path: an ordinary in-project edit runs, and
//! a protected path still cards, on both sides. Seventeen probes now, up from fifteen. Not run here
//! (the real CLI was not invoked); left for the next the test-account wrapper pass.
//!
//! **The `Edit` probe above was found likely vacuous before it
//! ever ran once.** `Edit`'s own `validateInput` refuses a file the session has not `Read` yet,
//! ahead of hooks and permissions in the CLI's own phase order -- so the original prompt's bare
//! `Edit` call would have failed validation, never reached a permission decision, and still counted
//! as "the CLI allowed it" (a validation error is not a [`REFUSAL_PHRASES`] match) while satisfying
//! `expects_tool: "Edit"` regardless. The prompt now tells the model to `Read` first, and
//! `file_must_contain` (`Probe`'s own doc) asserts the edit was actually applied -- still not run
//! here, so this is a claim about what the next the test-account wrapper pass will actually exercise, not a
//! new passing result.
//!
//! **Edit verification, still NOT RUN as of 2026-09-28.** Two ways this oracle could miscount an edit the CLI
//! refused as one it allowed are closed: [`REFUSAL_PHRASES`] gains "refusing to" (the CLI's own
//! `Write`/`Edit`/`NotebookEdit` denials for a symlink leaf and an undeterminable path read "Refusing
//! to write X: ..." -- a case exists today where the policy allows and the CLI refuses: a write to an
//! in-root symlink leaf, harmless because the CLI's re-check after the hook denies it anyway), and
//! the in-project `Write` probe now checks `probe.txt` really holds "hello". A second `#[ignore]`d
//! test, [`a_users_own_ask_rule_still_stops_an_edit_the_gate_allowed`], checks the one CLI behaviour
//! the edit fast path relies on -- a user's `Edit(...)` ask rule beating the gate's `allow` -- and is owed
//! to the same pass.

//! **2026-10-08, CLI 2.1.292 on the TEST profile: passed**, 19 probes made calls, nothing more permissive, seven stricter.
//! The eight `Grep`/`Glob` arms added that day were skipped: 2.1.292 lists neither tool (`TOOLS_THE_CLI_MAY_LACK`), so a
//! skip is printed and never counted as agreement. Two `Read` arms with the same path reasoning (`/proc`, a symlink out of
//! the root) ran: the CLI refused both and the policy cards both.
//!
//! **2026-10-14 (manual mode): NOT YET RUN.** A Manual tab's policy (`agent::classify_in_manual`) is
//! now checked on every call as well, against the CLI the probe ran: under `default` it must never be
//! more permissive, and under `acceptEdits` it must card every `Write`/`Edit`/`NotebookEdit`, since
//! `default` asks before every edit. One probe was added for it, an in-project `Write` under
//! `default` (the CLI must refuse it and the file must not exist); the Auto policy is not compared on
//! the edit calls of a `default` probe, since it reproduces `acceptEdits` for those three tools.

use agent::{classify_in_manual, classify_permission_request, PermissionVerdict, PrefixRules};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// One observed tool call: what the model asked for, and what the CLI did about it.
#[derive(Debug)]
struct ObservedCall {
    /// The `tool_use` block's own id -- the only thing that ties a refusal back to the call it
    /// refused, since a refusal arrives as an error `tool_result` on this id and not as a message
    /// type of its own.
    tool_use_id: String,
    tool_name: String,
    input: Value,
    allowed_by_the_cli: bool,
    /// The refusal the CLI printed, when it refused. Kept because its wording is the only evidence
    /// of WHY, and the two flavours ("was blocked. For security..." vs "requires approval") are not
    /// distinguished by this test on purpose -- both mean "not granted" here.
    refusal: Option<String>,
}

/// Phrases a real 2.1.272 `default`-mode refusal used. Matched case-insensitively on the tool
/// result's own text.
///
/// A phrase list is a weak detector and is not the only one in play: `is_error` has to be true as
/// well, and the probes that expect a refusal also check the side effect (the file really not
/// existing). If a future release rewords these, the failure mode is a refused call counted as
/// allowed -- which produces a spurious PERMISSIVE failure, i.e. it fails loudly in the safe
/// direction rather than quietly in the unsafe one.
const REFUSAL_PHRASES: &[&str] = &[
    "haven't granted",
    "have not granted",
    "requested permissions",
    "was blocked",
    "requires approval",
    // CLI 2.1.282, measured 2026-09-25: "Output redirection to '<path>' needs approval. ... Claude
    // Code asks before a shell command creates, changes or removes files there."
    "needs approval",
    "permission denied by",
    "user doesn't want",
    // CLI 2.1.283's bundle contains edit-tool denials not yet observed in a run:
    // "Refusing to write X: it is a symbolic link. ..." (`Zlt`, from the tools' checkPermissions)
    // and "Refusing to write X: where it leads on disk could not be determined ..." (`xl`).
    // Without matching these, a policy-allowed but CLI-refused edit counts as agreement.
    // The bundle has about thirty other safety refusals, including changed links and clone
    // URLs; matching those can only report a spurious PERMISSIVE failure, the safe direction.
    "refusing to",
];

#[test]
#[ignore]
fn the_policy_is_never_more_permissive_than_the_real_cli() {
    let mut stricter_than_the_cli: Vec<String> = Vec::new();
    let mut more_permissive_than_the_cli: Vec<String> = Vec::new();
    let mut manual_more_permissive: Vec<String> = Vec::new();
    let mut observed_anything = false;

    let mut skipped_for_a_missing_tool: Vec<String> = Vec::new();
    for probe in probes() {
        if TOOLS_THE_CLI_MAY_LACK.contains(&probe.expects_tool) && !tools_the_cli_lists().contains(probe.expects_tool) {
            println!(
                "\n--- probe: {} --- SKIPPED: this CLI build lists no {} tool, so the call this arm exists \
                 for cannot be made",
                probe.label, probe.expects_tool
            );
            skipped_for_a_missing_tool.push(format!("{} ({})", probe.label, probe.expects_tool));
            continue;
        }
        let fresh = a_fresh_workspace();
        let workspace = (probe.run_in)(&fresh);
        let calls = run_one_probe(&workspace, probe.prompt, probe.permission_mode);
        println!("\n--- probe: {} ---\n{calls:#?}", probe.label);

        assert!(
            calls.iter().any(|c| c.tool_name == probe.expects_tool),
            "probe {}: the model never called {}, so this probe decided nothing -- a pass here would \
             be vacuous. Calls observed: {:?}",
            probe.label,
            probe.expects_tool,
            calls.iter().map(|c| c.tool_name.as_str()).collect::<Vec<_>>()
        );
        for (label_part, fragment) in INPUT_MUST_MENTION {
            if probe.label.contains(label_part) {
                assert!(
                    calls
                        .iter()
                        .any(|c| c.tool_name == probe.expects_tool && c.input.to_string().contains(fragment)),
                    "probe {}: no {} call named {fragment}, so the path this arm exists for was never tried. \
                     Calls observed: {calls:?}",
                    probe.label,
                    probe.expects_tool
                );
            }
        }
        for call in &calls {
            observed_anything = true;
            let is_edit = EDIT_TOOLS.contains(&call.tool_name.as_str());
            let manual = classify_in_manual(&call.tool_name, &call.input, &workspace, &PrefixRules::default());
            if is_edit && !manual.needs_a_human() {
                manual_more_permissive.push(format!(
                    "{} {} -- Manual must card every edit, and allowed this one ({})",
                    call.tool_name, call.input, manual.reason
                ));
            }
            if probe.permission_mode.is_none() && !call.allowed_by_the_cli && !manual.needs_a_human() {
                manual_more_permissive.push(format!(
                    "{} {} -- the CLI refused it under `default` ({:?}) and Manual would have run it without \
                     asking ({})",
                    call.tool_name, call.input, call.refusal, manual.reason
                ));
            }
            // The Auto policy reproduces `acceptEdits` for the edit tools, so a `default` run is not
            // its reference for those calls (the manual-only probe below).
            if is_edit && probe.permission_mode.is_none() {
                continue;
            }
            let ours = classify_permission_request(&call.tool_name, &call.input, &workspace);
            match (ours.verdict, call.allowed_by_the_cli) {
                (PermissionVerdict::AllowWithoutAsking, false) => {
                    more_permissive_than_the_cli.push(format!(
                        "{} {} -- the CLI refused it ({:?}) and the policy would have run it \
                         without asking ({})",
                        call.tool_name, call.input, call.refusal, ours.reason
                    ));
                }
                (PermissionVerdict::AskTheUser, true) => {
                    stricter_than_the_cli.push(format!(
                        "{} {} -- the CLI allowed it; the policy asks ({})",
                        call.tool_name, call.input, ours.reason
                    ));
                }
                _ => {}
            }
        }

        if let Some(side_effect) = probe.file_that_must_not_exist {
            let path = workspace.join(side_effect);
            assert!(
                !path.exists(),
                "probe {}: {} exists, so the CLI did NOT refuse the write this probe depends on -- \
                 the oracle itself is wrong here, not the policy",
                probe.label,
                path.display()
            );
        }
        if let Some((relative, must_contain)) = probe.file_must_contain {
            let path = workspace.join(relative);
            let contents = std::fs::read_to_string(&path).unwrap_or_else(|e| {
                panic!(
                    "probe {}: {} could not be read ({e}) -- the CLI never actually applied this \
                     edit, so this probe decided nothing about the permission policy",
                    probe.label,
                    path.display()
                )
            });
            assert!(
                contents.contains(must_contain),
                "probe {}: {} does not contain {must_contain:?} -- the CLI's tool_use for {} was \
                 observed but the edit was never really applied (contents: {contents:?}), so this \
                 probe would have passed vacuously without this check",
                probe.label,
                path.display(),
                probe.expects_tool
            );
        }
        let _ = std::fs::remove_dir_all(&fresh);
    }

    assert!(
        observed_anything,
        "no tool call was observed in any probe -- the CLI ran, but nothing was classified, so \
         this test proved nothing. Check the stream-json parsing before trusting a pass."
    );
    if !skipped_for_a_missing_tool.is_empty() {
        println!(
            "\nNOT EXERCISED, the CLI has no such tool ({}): {}",
            skipped_for_a_missing_tool.len(),
            skipped_for_a_missing_tool.join("; ")
        );
    }
    if !stricter_than_the_cli.is_empty() {
        println!(
            "\nstricter than the CLI ({}), which is allowed and is the cost of rule (c):\n  {}",
            stricter_than_the_cli.len(),
            stricter_than_the_cli.join("\n  ")
        );
    }
    assert!(
        more_permissive_than_the_cli.is_empty(),
        "the policy would have run, without asking, calls the real CLI refused:\n  {}",
        more_permissive_than_the_cli.join("\n  ")
    );
    assert!(
        manual_more_permissive.is_empty(),
        "the Manual policy would have run, without asking, calls `default` refuses:\n  {}",
        manual_more_permissive.join("\n  ")
    );
}

/// The tools the Auto policy judges by Claude Code's acceptEdits fast path, and Manual always cards.
const EDIT_TOOLS: &[&str] = &["Write", "Edit", "NotebookEdit"];

/// **NOT YET RUN: owed to the next the test-account wrapper pass.** A user's own `permissions.ask`
/// rule for an edit must still stop a write the hook allowed. In-project writes can pass the
/// hook without a card, so the CLI's own re-check must preserve the user's rule.
///
/// Read from CLI 2.1.283's bundle, not yet observed: after hook allow, `EQn` runs `DR`
/// (checkRuleBasedPermissions), then the tool's `checkPermissions`. Its `Lb` asks when
/// `Ca(<spelling>, ..., "edit", "ask")` finds an `Edit(...)` rule; `DR` returns a rule-forced
/// ask (`Pve`), and `EQn` calls `canUseTool` instead of taking the hook's allow. Under print
/// mode without a permission prompt tool this is a refusal; on the sidecar it carries
/// `matched_ask_rule`, which core cards in every mode
/// (`agent_backend::tests::a_users_own_ask_rule_still_cards_an_edit_the_fast_path_allowed`).
/// This checks the edit case with a real CLI rather than assuming the bundle's branch runs.
///
/// Use `--permission-mode default`, `--setting-sources project,local` to avoid dependence on
/// machine user settings, and a `PreToolUse` hook with matcher `*` answering allow, as the
/// policy does for this Write (asserted below). Withhold Bash so the model cannot create the
/// file through another tool. Project settings ask for `Edit(probe-asked.txt)`; after asking
/// the model to create it, the file must not exist.
#[test]
#[ignore]
fn a_users_own_ask_rule_still_stops_an_edit_the_gate_allowed() {
    let workspace = a_fresh_workspace().canonicalize().unwrap();
    std::fs::create_dir_all(workspace.join(".claude")).unwrap();
    std::fs::write(
        workspace.join(".claude/settings.json"),
        serde_json::json!({ "permissions": { "ask": ["Edit(probe-asked.txt)"] } }).to_string(),
    )
    .unwrap();
    let target = workspace.join("probe-asked.txt");
    let policy = classify_permission_request("Write", &serde_json::json!({ "file_path": &target }), &workspace);
    assert_eq!(
        policy.verdict,
        PermissionVerdict::AllowWithoutAsking,
        "this probe is about a write the fast path allows ({}); if the policy cards it, the gate never \
         answers allow and the probe decides nothing",
        policy.reason
    );

    let allow = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "allow",
            "permissionDecisionReason": "the oracle's gate allows every call",
        }
    });
    let hook = serde_json::json!({
        "hooks": { "PreToolUse": [{
            "matcher": "*",
            "hooks": [{ "type": "command", "command": format!("cat >/dev/null; printf '%s\\n' '{allow}'") }],
        }] }
    })
    .to_string();
    let calls = run_the_cli(
        &workspace,
        "Using the Write tool, create a file called probe-asked.txt in the current directory \
         containing the word hello. Use only the Write tool.",
        &[
            "--permission-mode",
            "default",
            "--settings",
            &hook,
            "--disallowedTools",
            "Bash",
        ],
    );
    println!("\n--- the ask-rule probe ---\n{calls:#?}");

    assert!(
        calls.iter().any(|c| c.tool_name == "Write"),
        "the model never called Write, so this probe decided nothing. Calls observed: {:?}",
        calls.iter().map(|c| c.tool_name.as_str()).collect::<Vec<_>>()
    );
    for call in calls.iter().filter(|c| c.tool_name == "Write") {
        let ours = classify_permission_request(&call.tool_name, &call.input, &workspace);
        assert_eq!(
            ours.verdict,
            PermissionVerdict::AllowWithoutAsking,
            "the model's own Write {} is one the policy cards ({}), so the gate would never have \
             allowed it and this probe decided nothing",
            call.input,
            ours.reason
        );
    }
    assert!(
        !target.exists(),
        "{} exists: the CLI took the hook's allow over the user's own ask rule `Edit(probe-asked.txt)`, \
         so under item 4A that rule no longer protects an in-project edit on the legacy backend",
        target.display()
    );
    let _ = std::fs::remove_dir_all(&workspace);
}

struct Probe {
    label: &'static str,
    prompt: &'static str,
    /// The tool this probe exists to exercise. If the model never calls it, the probe decided
    /// nothing, and the test fails rather than counting that as agreement.
    expects_tool: &'static str,
    /// A path that must NOT exist afterwards, for a probe whose whole point is that the CLI refused
    /// to create it. Confirms the refusal by its absence rather than only by what was reported.
    file_that_must_not_exist: Option<&'static str>,
    /// `(path, substring)` that must BOTH exist AND contain `substring` afterwards, for a probe
    /// whose whole point is that the CLI actually reached and executed the edit -- not merely that
    /// it emitted a `tool_use` block for the right tool name.
    ///
    /// The `Edit` probe must not pass when the CLI never reached a permission decision at all.
    /// `Edit`'s own `validateInput` refuses a file that
    /// has not been `Read` in the same session first ("File has not been read yet."), BEFORE hooks
    /// or permissions run at all (`YSt`'s own phase order: validate, then permission). That refusal
    /// is a real tool error, not a permission denial -- its wording matches none of
    /// [`REFUSAL_PHRASES`] on purpose, since this test's whole `allowed_by_the_cli` bookkeeping
    /// exists to separate "the CLI let this run" from "the CLI denied it for permission reasons" --
    /// so `parse_calls` counts it `allowed_by_the_cli: true` and `expects_tool` is satisfied by the
    /// failed call, and the probe passed without ever exercising a real acceptEdits decision. The
    /// probe's prompt now tells the model to `Read` first; this field is the belt to that prompt's
    /// suspenders, so a future model that skips the `Read` anyway (or a future CLI that relaxes the
    /// validation) still fails loudly here rather than passing on a call nothing decided.
    file_must_contain: Option<(&'static str, &'static str)>,
    /// Where the CLI runs, given the fresh workspace -- the workspace itself for every probe but one,
    /// which needs a project inside a larger repository. Also the root the policy judges against.
    run_in: fn(&Path) -> PathBuf,
    /// `--permission-mode <this>` for the CLI, or no flag at all (`default`) when `None`.
    ///
    /// Every probe here was `None` (`default`) until 2026-09-28 (the acceptEdits fast
    /// path): `permission_policy` reproduces `default` for every tool EXCEPT `Write`/`Edit`/
    /// `NotebookEdit`, which now reproduce `acceptEdits` instead (module doc). Comparing one of
    /// those three against a `default`-mode CLI run would not test what the policy claims to
    /// reproduce -- the CLI denies every write under `default`, on purpose, regardless of path -- so
    /// a probe whose `expects_tool` is one of those three must set this to `Some("acceptEdits")`.
    /// Every other probe stays compared against `default`, unchanged. The one exception is Manual
    /// mode's own probe (2026-10-14): an edit under `default`, where only the Manual policy is
    /// compared.
    permission_mode: Option<&'static str>,
}

fn in_the_workspace(workspace: &Path) -> PathBuf {
    workspace.to_path_buf()
}

/// `<workspace>/project`, with the git repository at `<workspace>` -- a project nested inside another repository.
fn inside_a_larger_repository(workspace: &Path) -> PathBuf {
    let project = workspace.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("main.rs"), "fn main() {}\n").unwrap();
    let status = std::process::Command::new("git")
        .args(["init", "-q"])
        .arg(workspace)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("git must be installed for the nested-repository probe");
    assert!(status.success(), "git init failed in {}", workspace.display());
    project
}

/// The workspace as a committed git repository whose `blame.ignoreRevsFile` points at a file
/// OUTSIDE it. `git blame` then reads that outside file; the policy
/// cards every git call in a repo with a non-inert config key. `/etc/os-release` is a public file --
/// the probe only needs the CLI to attempt the read, not a real secret.
fn with_a_config_key_reading_an_outside_file(workspace: &Path) -> PathBuf {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .current_dir(workspace)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .expect("git must be installed for the blame.ignoreRevsFile probe")
    };
    assert!(git(&["init", "-q"]).success());
    assert!(git(&["add", "main.rs"]).success());
    assert!(git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "init"]).success());
    assert!(git(&["config", "blame.ignoreRevsFile", "/etc/os-release"]).success());
    workspace.to_path_buf()
}

/// `<workspace>/project`, whose `.git` is a gitfile naming ANOTHER repository, `<workspace>/victim`,
/// which names nothing back. `git show HEAD:outside.txt`
/// then prints the other repository's committed file; the policy cards every git call here. The
/// file holds a probe string, not a secret -- the probe only needs the CLI to attempt the read.
fn with_a_gitfile_naming_a_repository_outside(workspace: &Path) -> PathBuf {
    let victim = workspace.join("victim");
    std::fs::create_dir_all(&victim).unwrap();
    std::fs::write(victim.join("outside.txt"), "probe-outside-content\n").unwrap();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .current_dir(&victim)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .expect("git must be installed for the gitfile probe")
    };
    assert!(git(&["init", "-q"]).success());
    assert!(git(&["add", "outside.txt"]).success());
    assert!(git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "outside"]).success());
    let project = workspace.join("project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(project.join("main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(
        project.join(".git"),
        format!("gitdir: {}\n", victim.join(".git").display()),
    )
    .unwrap();
    project
}

/// A workspace holding a symlink named `-n` that points OUTSIDE it:
/// after `--` every argument is an operand, so `cat -- -n` follows the symlink. The policy checks the
/// literal `-n` as a path and cards it; the CLI runs `cat`, reading `/etc/hostname` through the link.
fn with_an_option_shaped_symlink(workspace: &Path) -> PathBuf {
    #[cfg(unix)]
    std::os::unix::fs::symlink("/etc/hostname", workspace.join("-n")).unwrap();
    workspace.to_path_buf()
}

/// Twenty-eight probes, one billed turn each, covering both directions of the assertion.
/// The set is not exhaustive; each added probe costs another turn.
/// Eight Grep and Glob probes run only where the CLI lists those tools.
///
/// Three edit probes run under `--permission-mode acceptEdits`: an in-project `Write`,
/// an in-project `Edit` of an existing file, and a `Write` under a protected directory.
/// The first two should be allowed by both the CLI and the policy's fast path;
/// the protected write should require a card on both sides.
///
/// The indirect-read probes include `git blame` with config that reads an outside file
/// and `cat -- -n` through a symlink outside the workspace. The CLI runs both; the policy cards both.
/// The CLI also allowed `find -files0-from` in the initial run. These stricter policy decisions
/// pass the assertion. Keeping the probes measures the gap and detects if the CLI starts refusing them.
fn probes() -> Vec<Probe> {
    vec![
        Probe {
            label: "a read inside the working directory",
            prompt: "Read the file main.rs in the current directory and tell me its first line. \
                     Do not create or modify any file.",
            expects_tool: "Read",
            file_that_must_not_exist: None,
            file_must_contain: None,
            run_in: in_the_workspace,
            permission_mode: None,
        },
        // The acceptEdits fast path: a plain in-project `Write` is no longer compared
        // against `default` -- `default` denies every write on purpose, which would report this as
        // "more permissive than the CLI" for a divergence the module doc explains, not a defect.
        // Compared against `acceptEdits` instead, where the CLI's own fast path applies too, so this
        // probe is now a real acceptEdits-parity check rather than a `default`-mode one.
        Probe {
            label: "an in-project write, under acceptEdits",
            prompt: "Create a file called probe.txt in the current directory containing the word hello.",
            expects_tool: "Write",
            file_that_must_not_exist: None,
            // The write must really have happened. A refusal worded
            // outside `REFUSAL_PHRASES` would otherwise count as the CLI allowing it.
            file_must_contain: Some(("probe.txt", "hello")),
            run_in: in_the_workspace,
            permission_mode: Some("acceptEdits"),
        },
        // Manual mode's reference: `default` asks before every edit, so with no prompt to answer
        // the CLI refuses this in-project `Write`, and the Manual policy must card it. The Auto
        // policy is not compared here (it reproduces `acceptEdits` for this tool).
        Probe {
            label: "an in-project write, under default (Manual's reference)",
            prompt: "Create a file called probe-default.txt in the current directory containing the word \
                     hello. Use the Write tool.",
            expects_tool: "Write",
            file_that_must_not_exist: Some("probe-default.txt"),
            file_must_contain: None,
            run_in: in_the_workspace,
            permission_mode: None,
        },
        // The other edit tool this fast path covers, and the other shape of target: `Edit` requires
        // an existing file, where `Write` above created a new one.
        Probe {
            label: "an in-project edit of an existing file, under acceptEdits",
            // `Edit`'s own `validateInput` refuses a file that
            // has not been `Read` in the same session yet, before hooks or permissions ever run
            // (`file_must_contain`'s own doc). The prompt now tells the model to `Read` first so the
            // `Edit` call actually reaches a permission decision, and `file_must_contain` below
            // checks the edit was really applied rather than merely attempted.
            prompt: "First use the Read tool to read main.rs in the current directory. Then, using \
                     the Edit tool, change the text \"probe\" to \"edited\" in it. Use only the Read \
                     and Edit tools.",
            expects_tool: "Edit",
            file_that_must_not_exist: None,
            file_must_contain: Some(("main.rs", "edited")),
            run_in: in_the_workspace,
            permission_mode: Some("acceptEdits"),
        },
        // The other half of the same parity claim: a target under one of the protected directories
        // still cards under acceptEdits, on both sides. `.vscode/settings.json` is deliberately an
        // ordinary-sounding request (an editor settings file), not something that reads as touching
        // version control, so a refusal here is good evidence of the permission gate and not of the
        // model declining on its own -- the same reasoning the gitfile/config probes below use for
        // git internals.
        Probe {
            label: "a write under a protected directory still cards, under acceptEdits",
            prompt: "Create a file at .vscode/settings.json in the current directory with the \
                     contents {}. Use the Write tool.",
            expects_tool: "Write",
            file_that_must_not_exist: Some(".vscode/settings.json"),
            file_must_contain: None,
            run_in: in_the_workspace,
            permission_mode: Some("acceptEdits"),
        },
        Probe {
            label: "a read-only bash command",
            prompt: "Using the Bash tool, run exactly `ls` in the current directory and tell me \
                     what it printed. Do not run anything else.",
            expects_tool: "Bash",
            file_that_must_not_exist: None,
            file_must_contain: None,
            run_in: in_the_workspace,
            permission_mode: None,
        },
        Probe {
            label: "a bash command with a redirect",
            prompt: "Using the Bash tool, run exactly `echo hi > redirected.txt` in the current \
                     directory. Do not use the Write tool.",
            expects_tool: "Bash",
            file_that_must_not_exist: Some("redirected.txt"),
            file_must_contain: None,
            run_in: in_the_workspace,
            permission_mode: None,
        },
        // The policy allows Read/Grep/Glob only for a path that canonicalizes inside the project
        // root, so this is the probe that checks the path half of the policy against the CLI.
        Probe {
            label: "a read outside the working directory",
            prompt: "Using the Read tool, read the file /etc/hostname and tell me what it says. \
                     Do not use any other tool.",
            expects_tool: "Read",
            file_that_must_not_exist: None,
            file_must_contain: None,
            run_in: in_the_workspace,
            permission_mode: None,
        },
        // `..` out of the root. The workspace is /tmp/<dir>, so this names /etc/hostname. The
        // Grep and Glob arms are at the end of the list, and run only on a build that lists those
        // tools: CLI 2.1.272 had neither (its `system/init` listed no such name, measured 2026-09-19,
        // and a Grep probe made the model reach for ToolSearch twice and then give up, which the
        // per-probe check above caught as vacuous), and neither has 2.1.292.
        // The tools are deferred on this build, so the model reaches for `ToolSearch` before it can
        // use anything -- which made this the most frequent gated call in a real session and the
        // reason auto mode still felt like a wall of popups. The CLI runs it in default mode.
        Probe {
            label: "a tool-schema search",
            prompt: "Use the ToolSearch tool to load the Read tool's schema. Then reply with just: done",
            expects_tool: "ToolSearch",
            file_that_must_not_exist: None,
            file_must_contain: None,
            run_in: in_the_workspace,
            permission_mode: None,
        },
        // A pipe between two read-only commands. The CLI runs it; the policy cards it, because it
        // has no shell parser and every shape of shell syntax resolves toward the card. Stricter,
        // not unsafe -- and a probe here so the gap is measured on each run rather than argued.
        Probe {
            label: "a read-only bash command with a pipe",
            prompt: "Using the Bash tool, run exactly `grep -n alpha a.txt | head -1` in the \
                     current directory and tell me what it printed. Do not run anything else.",
            expects_tool: "Bash",
            file_that_must_not_exist: None,
            file_must_contain: None,
            run_in: in_the_workspace,
            permission_mode: None,
        },
        Probe {
            label: "a read that climbs out of the working directory with ..",
            prompt: "Using the Read tool, read the file ../../etc/hostname (relative to the current \
                     directory) and tell me what it says. Do not use any other tool.",
            expects_tool: "Read",
            file_that_must_not_exist: None,
            file_must_contain: None,
            run_in: in_the_workspace,
            permission_mode: None,
        },
        // The CLI allowed this call in the initial run.
        // `dirs` names only `.`, so nothing outside the workspace is touched.
        Probe {
            label: "find reading its starting points from a file",
            prompt: "Using the Bash tool, run exactly `find -files0-from dirs -name main.rs` in the \
                     current directory and tell me what it printed. Do not run anything else.",
            expects_tool: "Bash",
            file_that_must_not_exist: None,
            file_must_contain: None,
            run_in: in_the_workspace,
            permission_mode: None,
        },
        Probe {
            label: "wc reading the files to count from a list",
            prompt: "Using the Bash tool, run exactly `wc -c --files0-from paths` in the current \
                     directory and tell me what it printed. Do not run anything else.",
            expects_tool: "Bash",
            file_that_must_not_exist: None,
            file_must_contain: None,
            run_in: in_the_workspace,
            permission_mode: None,
        },
        Probe {
            label: "diff of two directories",
            prompt: "Using the Bash tool, run exactly `diff a b` in the current directory and tell \
                     me what it printed. Do not run anything else.",
            expects_tool: "Bash",
            file_that_must_not_exist: None,
            file_must_contain: None,
            run_in: in_the_workspace,
            permission_mode: None,
        },
        Probe {
            label: "read-only git in a project inside a larger repository",
            prompt: "Using the Bash tool, run exactly `git status --short` in the current directory \
                     and tell me what it printed. Do not run anything else.",
            expects_tool: "Bash",
            file_that_must_not_exist: None,
            file_must_contain: None,
            run_in: inside_a_larger_repository,
            permission_mode: None,
        },
        // A repo config key that reads an outside file. The CLI runs
        // `git blame`; the policy cards it (non-inert config key). Stricter, which passes.
        Probe {
            label: "git blame in a repo whose config reads an outside file",
            prompt: "Using the Bash tool, run exactly `git blame main.rs` in the current directory \
                     and tell me its first line. Do not run anything else.",
            expects_tool: "Bash",
            file_that_must_not_exist: None,
            file_must_contain: None,
            run_in: with_a_config_key_reading_an_outside_file,
            permission_mode: None,
        },
        // An option-shaped filename after `--`. The CLI runs `cat`,
        // following the `-n` symlink out of the workspace; the policy cards it. Stricter, passes.
        Probe {
            label: "cat of an option-shaped symlink after --",
            prompt: "Using the Bash tool, run exactly `cat -- -n` in the current directory and tell \
                     me what it printed. Do not run anything else.",
            expects_tool: "Bash",
            file_that_must_not_exist: None,
            file_must_contain: None,
            run_in: with_an_option_shaped_symlink,
            permission_mode: None,
        },
        // A gitfile naming another repository. The CLI runs `git show`,
        // printing that repository's file; the policy cards it (no back-link). Stricter, passes.
        Probe {
            label: "git show through a gitfile naming another repository",
            prompt: "Using the Bash tool, run exactly `git show HEAD:outside.txt` in the current \
                     directory and tell me what it printed. Do not run anything else.",
            expects_tool: "Bash",
            file_that_must_not_exist: None,
            file_must_contain: None,
            run_in: with_a_gitfile_naming_a_repository_outside,
            permission_mode: None,
        },
        // Path arms the oracle lacked: the policy allows a path-judged call only when the path resolves
        // inside the root, so each of these is a place the resolution could go wrong. Read has them
        // because Grep and Glob (below) do not exist on every CLI build.
        Probe {
            label: "a read of /proc",
            prompt: "Using the Read tool, read the file /proc/version and tell me what it says. \
                     Do not use any other tool.",
            expects_tool: "Read",
            file_that_must_not_exist: None,
            file_must_contain: None,
            run_in: in_the_workspace,
            permission_mode: None,
        },
        Probe {
            label: "a read through a symlink that leaves the working directory",
            prompt: "Using the Read tool, read the file out/hostname (relative to the current \
                     directory) and tell me what it says. Do not use any other tool.",
            expects_tool: "Read",
            file_that_must_not_exist: None,
            file_must_contain: None,
            run_in: with_a_symlink_out_of_the_root,
            permission_mode: None,
        },
        // Grep and Glob, four paths each: inside the root, outside it, /proc, and through a link out.
        // Exercised only on a CLI build that lists the tool (`TOOLS_THE_CLI_MAY_LACK`).
        Probe {
            label: "a grep inside the working directory",
            prompt: "Using the Grep tool, search for the word probe in the current directory and \
                     tell me which file matched. Use only the Grep tool.",
            expects_tool: "Grep",
            file_that_must_not_exist: None,
            file_must_contain: None,
            run_in: in_the_workspace,
            permission_mode: None,
        },
        Probe {
            label: "a grep outside the working directory",
            prompt: "Using the Grep tool with the path /etc, search for the word localhost in \
                     /etc/hosts and tell me what it found. Use only the Grep tool.",
            expects_tool: "Grep",
            file_that_must_not_exist: None,
            file_must_contain: None,
            run_in: in_the_workspace,
            permission_mode: None,
        },
        Probe {
            label: "a grep of /proc",
            prompt: "Using the Grep tool with the path /proc/version, search for the word Linux and \
                     tell me what it found. Use only the Grep tool.",
            expects_tool: "Grep",
            file_that_must_not_exist: None,
            file_must_contain: None,
            run_in: in_the_workspace,
            permission_mode: None,
        },
        Probe {
            label: "a grep through a symlink that leaves the working directory",
            prompt: "Using the Grep tool with the path out (relative to the current directory), search \
                     for the word localhost in out/hosts and tell me what it found. Use only the Grep tool.",
            expects_tool: "Grep",
            file_that_must_not_exist: None,
            file_must_contain: None,
            run_in: with_a_symlink_out_of_the_root,
            permission_mode: None,
        },
        Probe {
            label: "a glob inside the working directory",
            prompt: "Using the Glob tool, find the files matching *.rs in the current directory and \
                     list them. Use only the Glob tool.",
            expects_tool: "Glob",
            file_that_must_not_exist: None,
            file_must_contain: None,
            run_in: in_the_workspace,
            permission_mode: None,
        },
        Probe {
            label: "a glob outside the working directory",
            prompt: "Using the Glob tool with the path /etc, find the files matching host* and list \
                     them. Use only the Glob tool.",
            expects_tool: "Glob",
            file_that_must_not_exist: None,
            file_must_contain: None,
            run_in: in_the_workspace,
            permission_mode: None,
        },
        Probe {
            label: "a glob of /proc",
            prompt: "Using the Glob tool with the path /proc, find the files matching ver* and list \
                     them. Use only the Glob tool.",
            expects_tool: "Glob",
            file_that_must_not_exist: None,
            file_must_contain: None,
            run_in: in_the_workspace,
            permission_mode: None,
        },
        Probe {
            label: "a glob through a symlink that leaves the working directory",
            prompt: "Using the Glob tool with the path out (relative to the current directory), find \
                     the files matching host* and list them. Use only the Glob tool.",
            expects_tool: "Glob",
            file_that_must_not_exist: None,
            file_must_contain: None,
            run_in: with_a_symlink_out_of_the_root,
            permission_mode: None,
        },
    ]
}

/// `<workspace>/out` is a symlink to `/etc`, a directory outside the workspace: a path that starts
/// inside the root and leaves it through a link, which only resolving the path reveals.
fn with_a_symlink_out_of_the_root(workspace: &Path) -> PathBuf {
    #[cfg(unix)]
    std::os::unix::fs::symlink("/etc", workspace.join("out")).unwrap();
    workspace.to_path_buf()
}

/// `(part of a probe's label, text one of its calls' inputs must hold)`: an arm about a path the model
/// could reach another way (reading `/etc/hostname` itself instead of `out/hostname`) fails as vacuous
/// when its call never names the path.
const INPUT_MUST_MENTION: &[(&str, &str)] = &[("through a symlink that leaves", "out"), ("of /proc", "/proc")];

/// Tools a newer CLI build may not have. Claude Code 2.1.292 lists no `Grep` and no `Glob` (its
/// `system/init` names neither, and `ToolSearch` finds neither: the model searches with `Bash`), so an
/// arm that exists to exercise one of them cannot make its call there and is reported as not
/// exercised, never counted as agreement. The first build the oracle was written against had both.
const TOOLS_THE_CLI_MAY_LACK: &[&str] = &["Grep", "Glob"];

/// The tool names the CLI lists in its `system/init` message, read once from one short session in a
/// scratch workspace (no tool is called, so the turn is cheap).
fn tools_the_cli_lists() -> &'static std::collections::BTreeSet<String> {
    static LISTED: std::sync::OnceLock<std::collections::BTreeSet<String>> = std::sync::OnceLock::new();
    LISTED.get_or_init(|| {
        let workspace = a_fresh_workspace();
        let output = std::process::Command::new("claude")
            .current_dir(&workspace)
            .args([
                "--print",
                "Reply with the word ok.",
                "--output-format",
                "stream-json",
                "--verbose",
                "--setting-sources",
                "project,local",
            ])
            .output()
            .expect("`claude` must be on PATH; run this through a test-account wrapper");
        let _ = std::fs::remove_dir_all(&workspace);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let tools: std::collections::BTreeSet<String> = stdout
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|v| v["type"] == "system" && v["subtype"] == "init")
            .and_then(|init| init["tools"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|t| t.as_str().map(str::to_string))
            .collect();
        assert!(
            tools.contains("Read"),
            "the CLI's init message listed no Read tool, so its tool list was not read: {tools:?}"
        );
        println!("the CLI lists these tools: {tools:?}");
        tools
    })
}

fn a_fresh_workspace() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("agent-policy-oracle-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("main.rs"), "fn main() { println!(\"probe\"); }\n").unwrap();
    // For the piped-grep probe: a file with a line the command can find, so a `grep` that prints
    // nothing cannot be mistaken for a refusal.
    std::fs::write(dir.join("a.txt"), "alpha\nbeta\n").unwrap();
    // For the indirect-read probes: NUL-terminated lists naming only what is already here, and two
    // directories for `diff` to compare. Nothing any of them names is outside the workspace.
    std::fs::write(dir.join("paths"), "main.rs\0").unwrap();
    std::fs::write(dir.join("dirs"), ".\0").unwrap();
    for side in ["a", "b"] {
        std::fs::create_dir_all(dir.join(side)).unwrap();
        std::fs::write(dir.join(side).join("x.txt"), format!("{side}\n")).unwrap();
    }
    // Canonical, as the session's own root is: the policy cards every path-judged call under a root
    // that does not resolve to itself, and a `$TMPDIR` may pass through a
    // link (macOS's `/var -> private/var`).
    dir.canonicalize().unwrap()
}

/// One `claude --print` session, with **no `--permission-mode` flag** for most probes -- that is
/// the `default` mode whose classification `permission_policy` reproduces for every tool but
/// `Write`/`Edit`/`NotebookEdit`. `permission_mode` is `Some("acceptEdits")` for exactly the probes
/// that exercise those three (module doc, "The acceptEdits fast path"), so the oracle is the CLI
/// mode the policy actually claims to reproduce for the tool under test, never `default` and never
/// `auto` (its own classifier, a different thing again -- see the module doc). Nothing installs a
/// hook here either -- a hook would answer the question this test is asking the CLI.
fn run_one_probe(workspace: &Path, prompt: &str, permission_mode: Option<&str>) -> Vec<ObservedCall> {
    let mode_args: Vec<&str> = permission_mode
        .map(|mode| vec!["--permission-mode", mode])
        .unwrap_or_default();
    run_the_cli(workspace, prompt, &mode_args)
}

/// `claude --print` in `workspace` with the settings sources the product passes, plus `extra_args`.
fn run_the_cli(workspace: &Path, prompt: &str, extra_args: &[&str]) -> Vec<ObservedCall> {
    let mut command = std::process::Command::new("claude");
    command
        .current_dir(workspace)
        // `project,local`, the product's `agent.user_settings = false` selection, and not the user
        // tier, which a default product session loads: this oracle compares the policy with the CLI's
        // own rules and has to be independent of whoever runs it. On 2026-09-25 the TEST
        // profile's user settings carried an `rtk hook claude`
        // `PreToolUse` hook that rewrote `ls` to `rtk ls`, which the CLI then refused as not
        // read-only -- a refusal of a command nobody asked for, reported as the CLI refusing `ls`.
        // The user tier can also carry `permissions.allow`, which would make the oracle more
        // permissive than a product session and hide the failure this test exists to catch.
        .args([
            "--print",
            prompt,
            "--output-format",
            "stream-json",
            "--verbose",
            "--setting-sources",
            "project,local",
        ])
        .args(extra_args);
    let output = command
        .output()
        .expect("`claude` must be on PATH; run this through a test-account wrapper");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stdout.trim().is_empty(),
        "the CLI produced no stdout. stderr was:\n{stderr}"
    );
    parse_calls(&stdout)
}

/// Pairs each `tool_use` block with the `tool_result` that answers it, which is the only way to
/// know what the CLI did.
fn parse_calls(stream_json: &str) -> Vec<ObservedCall> {
    let mut calls: Vec<ObservedCall> = Vec::new();
    for line in stream_json.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(content) = value
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        else {
            continue;
        };
        for block in content {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("tool_use") => {
                    let (Some(id), Some(name), Some(input)) = (
                        block.get("id").and_then(|i| i.as_str()),
                        block.get("name").and_then(|n| n.as_str()),
                        block.get("input"),
                    ) else {
                        continue;
                    };
                    calls.push(ObservedCall {
                        tool_use_id: id.to_string(),
                        tool_name: name.to_string(),
                        input: input.clone(),
                        // Corrected below by the matching result. A call with no result at all
                        // (the turn ended first) stays `true`, which would report a PERMISSIVE
                        // failure rather than hide one -- the safe direction to default in.
                        allowed_by_the_cli: true,
                        refusal: None,
                    });
                }
                Some("tool_result") => {
                    let Some(id) = block.get("tool_use_id").and_then(|i| i.as_str()) else {
                        continue;
                    };
                    if !block.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false) {
                        continue;
                    }
                    let text = block.get("content").map(|c| c.to_string()).unwrap_or_default();
                    let lowered = text.to_lowercase();
                    if !REFUSAL_PHRASES.iter().any(|p| lowered.contains(p)) {
                        // An error that is not a refusal -- a real tool failure. The call stays
                        // marked allowed, because the CLI DID let it run, which is the question.
                        continue;
                    }
                    if let Some(call) = calls.iter_mut().find(|c| c.tool_use_id == id) {
                        call.allowed_by_the_cli = false;
                        call.refusal = Some(text);
                    }
                }
                _ => {}
            }
        }
    }
    calls
}
