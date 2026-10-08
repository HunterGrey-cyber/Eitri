//! Tests that the permission policy is never more permissive than the real Claude CLI.
//! The headless CLI provides an observable permission result: under `default`, with no hook,
//! `Read` ran and `Write` was denied without creating a file. Under `auto`, a classifier
//! approved `Bash`, `Read` and `Write`, so that mode cannot serve as the same comparison.
//!
//! A CLI denial that the policy would auto-allow fails the test. A CLI allowance that the
//! policy sends for approval is printed and counted, but passes. The policy requires approval
//! for compound shell syntax, `WebFetch` and unknown tool names, so equality is not required.
//! Each probe must call its expected tool to avoid passing without exercising it.
//! Reads outside the root, including paths containing `..`, exercise path-based classification.
//!
//! These ignored tests make real, billed CLI calls. Use an isolated profile and state directory.
//! ```sh
//! claude --version    # record the build; nothing prints it for you
//! cargo test -p agent --test permission_policy_conformance -- --ignored --nocapture
//! ```
//!
//! `Write`, `Edit` and `NotebookEdit` are compared under `acceptEdits`; other tools use `default`.
//! The edit probes check that ordinary in-project edits run while protected paths need approval.
//! `Edit` must follow `Read` in the same session so validation reaches the permission check.
//! `file_must_contain` checks the actual result, since a validation error can still satisfy
//! `expects_tool` without matching `REFUSAL_PHRASES`. Matching "refusing to" also catches
//! edit denials for symlink leaves and paths whose disk location cannot be determined.
//! A separate ignored test checks that a user's `Edit(...)` ask rule overrides the gate's allow.
//! These edit checks have not yet been run against the real CLI.

//! **2026-10-08, CLI 2.1.292 on the TEST profile: passed**, 19 probes made calls, nothing more permissive, seven stricter.
//! The eight `Grep`/`Glob` arms added that day were skipped: 2.1.292 lists neither tool (`TOOLS_THE_CLI_MAY_LACK`), so a
//! skip is printed and never counted as agreement. Two `Read` arms with the same path reasoning (`/proc`, a symlink out of
//! the root) ran: the CLI refused both and the policy cards both.

use agent::{classify_permission_request, PermissionVerdict};
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
    // The CLI's edit tools can deny a write with "Refusing to write" when the target is
    // a symlink or its disk location cannot be determined. Match this wording so a denied
    // edit is not counted as allowed when the policy allows it. Other safety refusals can
    // also match, causing a conservative failure rather than hiding an unsafe allowance.
    "refusing to",
];

#[test]
#[ignore]
fn the_policy_is_never_more_permissive_than_the_real_cli() {
    let mut stricter_than_the_cli: Vec<String> = Vec::new();
    let mut more_permissive_than_the_cli: Vec<String> = Vec::new();
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
}

/// Checks that a user's `permissions.ask` rule stops an edit even when the gate's hook allows it.
/// This probe has not yet been run. An in-project `Write` can receive the hook's `allow`
/// without a card, so the CLI must re-check the user's `Edit(...)` ask rule before writing.
/// Under `--print` with no permission prompt tool, the expected result is a refusal.
///
/// The probe uses `--permission-mode default`, `--setting-sources project,local`, and a
/// `PreToolUse` hook with matcher `*` that answers `allow`. This selection excludes machine-specific
/// user settings so the probe is independent of them. `Bash` is withheld so the model
/// cannot create the file through a tool the rule does not cover. The project's
/// `.claude/settings.json` asks for `Edit(probe-asked.txt)`. The requested file must not exist
/// afterwards.
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
    /// `(path, substring)` that must exist and contain `substring` after the probe.
    /// This confirms that the CLI executed the edit, beyond emitting a `tool_use` block.
    ///
    /// `Edit` validation rejects a file not yet `Read` in the session before permissions run.
    /// That validation error does not match [`REFUSAL_PHRASES`], so `parse_calls` can mark it
    /// `allowed_by_the_cli: true` and satisfy `expects_tool` without a permission decision.
    /// The prompt requests `Read` first; this check also fails if the expected edit never happens.
    file_must_contain: Option<(&'static str, &'static str)>,
    /// Where the CLI runs, given the fresh workspace -- the workspace itself for every probe but one,
    /// which needs a project inside a larger repository. Also the root the policy judges against.
    run_in: fn(&Path) -> PathBuf,
    /// `--permission-mode <this>` for the CLI, or no flag (`default`) when `None`.
    ///
    /// `permission_policy` reproduces `acceptEdits` for `Write`, `Edit`, and `NotebookEdit`,
    /// and `default` for other tools. The CLI denies writes under `default` regardless of path,
    /// so parity probes for those edit tools use `Some("acceptEdits")` to test the matching mode.
    /// Other probes use `default`.
    permission_mode: Option<&'static str>,
}

fn in_the_workspace(workspace: &Path) -> PathBuf {
    workspace.to_path_buf()
}

/// `<workspace>/project`, with the enclosing git repository rooted at `<workspace>`.
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

/// The workspace as a committed git repository whose `blame.ignoreRevsFile` points outside it.
/// `git blame` reads that file, so the policy cards every git call with this non-inert config key.
/// The probe uses `/etc/os-release`; it only needs the CLI to attempt the read.
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

/// `<workspace>/project`, whose `.git` file names `<workspace>/victim` without a back-link.
/// `git show HEAD:outside.txt` prints that repository's committed file; the policy cards every
/// git call here. The file holds a probe string so the test can detect the read.
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

/// A workspace holding a symlink named `-n` that points outside it.
/// After `--`, every argument is an operand, so `cat -- -n` follows the symlink.
/// The policy checks `-n` as a path and cards it; the CLI reads `/etc/hostname` through the link.
fn with_an_option_shaped_symlink(workspace: &Path) -> PathBuf {
    #[cfg(unix)]
    std::os::unix::fs::symlink("/etc/hostname", workspace.join("-n")).unwrap();
    workspace.to_path_buf()
}

/// Twenty-seven probes, one billed turn each, covering both directions of the assertion.
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
        // Compare an in-project `Write` against `acceptEdits`, where the CLI's fast path also applies.
        // `default` denies every write, so it would report the policy as more permissive without
        // exercising the edit behavior this probe tests.
        Probe {
            label: "an in-project write, under acceptEdits",
            prompt: "Create a file called probe.txt in the current directory containing the word hello.",
            expects_tool: "Write",
            file_that_must_not_exist: None,
            // The write must really have happened. A refusal worded outside `REFUSAL_PHRASES`
            // would otherwise count as the CLI allowing it.
            file_must_contain: Some(("probe.txt", "hello")),
            run_in: in_the_workspace,
            permission_mode: Some("acceptEdits"),
        },
        // The other edit tool this fast path covers, and the other shape of target: `Edit` requires
        // an existing file, where `Write` above created a new one.
        Probe {
            label: "an in-project edit of an existing file, under acceptEdits",
            // `Edit` validation rejects a file not yet `Read` in the session before permissions run.
            // The prompt requests `Read` first so `Edit` reaches a permission decision.
            // `file_must_contain` checks that the edit was applied.
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
        // A non-inert repo config key makes `git blame` read an outside file.
        // The CLI runs it; the policy cards it. This stricter result passes.
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
        // After `--`, `-n` is a filename. The CLI runs `cat` and follows the symlink outside the workspace.
        // The policy cards it. This stricter result passes.
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
        // A gitfile names another repository without a back-link. The CLI runs `git show` and prints its file.
        // The policy cards it. This stricter result passes.
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
    // NUL-terminated lists name only workspace entries, and two directories let `diff` compare files.
    // Nothing named by these fixtures is outside the workspace.
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
