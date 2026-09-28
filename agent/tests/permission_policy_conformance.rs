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
//! Real turns, real money, on the TEST Claude profile -- never the owner's own login:
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
//! climbs out with `..`). The dated record says what the re-run found.
//!
//! **2026-09-28 (the P1 audit), CLI 2.1.283 on the TEST profile: passed** with twelve probes, four of
//! them new for the cards added then (`find -files0-from`, `wc --files0-from`, `diff` of two
//! directories, read-only git in a project inside a larger repository). Nothing was more permissive;
//! five calls were stricter -- the four new ones, all of which the CLI ran, and the old piped
//! `grep`. Run with `XDG_STATE_HOME=$HOME/.cache/nv-policy/state` so no state lands in the owner's.
//!
//! **2026-09-28 (P1 audit round 2), CLI 2.1.283 on the TEST profile: passed** with fourteen probes,
//! two more added for that round's cards -- `git blame` in a repo whose config reads an outside file
//! (finding 2) and `cat -- -n` through an option-shaped symlink (finding 3). The CLI ran both and
//! the policy cards both, so nothing was more permissive and both are counted stricter.
//!
//! **2026-09-28 (P1 audit round 3), CLI 2.1.283 on the TEST profile: passed** with fifteen probes, one
//! more for that round's blocking finding: `git show HEAD:outside.txt` in a project whose `.git` is a
//! gitfile naming another repository. The CLI ran it without refusing (the same command, run for real,
//! prints the other repository's file); the policy cards it. Nothing was more permissive; seven calls
//! were stricter (round 2's six and this one).

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
];

#[test]
#[ignore]
fn the_policy_is_never_more_permissive_than_the_real_cli() {
    let mut stricter_than_the_cli: Vec<String> = Vec::new();
    let mut more_permissive_than_the_cli: Vec<String> = Vec::new();
    let mut observed_anything = false;

    for probe in probes() {
        let fresh = a_fresh_workspace();
        let workspace = (probe.run_in)(&fresh);
        let calls = run_one_probe(&workspace, probe.prompt);
        println!("\n--- probe: {} ---\n{calls:#?}", probe.label);

        assert!(
            calls.iter().any(|c| c.tool_name == probe.expects_tool),
            "probe {}: the model never called {}, so this probe decided nothing -- a pass here would \
             be vacuous. Calls observed: {:?}",
            probe.label,
            probe.expects_tool,
            calls.iter().map(|c| c.tool_name.as_str()).collect::<Vec<_>>()
        );
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
        let _ = std::fs::remove_dir_all(&fresh);
    }

    assert!(
        observed_anything,
        "no tool call was observed in any probe -- the CLI ran, but nothing was classified, so \
         this test proved nothing. Check the stream-json parsing before trusting a pass."
    );
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

struct Probe {
    label: &'static str,
    prompt: &'static str,
    /// The tool this probe exists to exercise. If the model never calls it, the probe decided
    /// nothing, and the test fails rather than counting that as agreement.
    expects_tool: &'static str,
    /// A path that must NOT exist afterwards, for a probe whose whole point is that the CLI refused
    /// to create it. Confirms the refusal by its absence rather than only by what was reported.
    file_that_must_not_exist: Option<&'static str>,
    /// Where the CLI runs, given the fresh workspace -- the workspace itself for every probe but one,
    /// which needs a project inside a larger repository. Also the root the policy judges against.
    run_in: fn(&Path) -> PathBuf,
}

fn in_the_workspace(workspace: &Path) -> PathBuf {
    workspace.to_path_buf()
}

/// `<workspace>/project`, with the git repository at `<workspace>` -- the P1 audit's nested case.
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
/// OUTSIDE it (P1 audit round 2, finding 2). `git blame` then reads that outside file; the policy
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
/// which names nothing back (P1 audit round 3, the blocking finding). `git show HEAD:outside.txt`
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

/// A workspace holding a symlink named `-n` that points OUTSIDE it (P1 audit round 2, finding 3):
/// after `--` every argument is an operand, so `cat -- -n` follows the symlink. The policy checks the
/// literal `-n` as a path and cards it; the CLI runs `cat`, reading `/etc/hostname` through the link.
fn with_an_option_shaped_symlink(workspace: &Path) -> PathBuf {
    #[cfg(unix)]
    std::os::unix::fs::symlink("/etc/hostname", workspace.join("-n")).unwrap();
    workspace.to_path_buf()
}

/// Fifteen probes, one billed turn each, chosen to cover both directions of the assertion rather
/// than to be exhaustive. Add more freely -- each costs a turn.
///
/// The last six (2026-09-28, the P1 audit and its round 2) exercise the cards added then. Round 2
/// added a `git blame` in a repo whose config reads an outside file (finding 2) and a `cat -- -n`
/// through an option-shaped symlink (finding 3); the CLI runs both, the policy cards both.
/// `find -files0-from` was
/// expected to be the one that could fail -- the P1 verifier read CLI 2.1.283's binary as excluding
/// it from read-only `find`, and the policy allowed it until then -- but the first run returned no
/// refusal from the CLI for it. So all four are cases the CLI allows and the policy now cards:
/// stricter, which passes, and listed so each run measures the gap (and would show the day the CLI
/// starts refusing one, which still passes).
fn probes() -> Vec<Probe> {
    vec![
        Probe {
            label: "a read inside the working directory",
            prompt: "Read the file main.rs in the current directory and tell me its first line. \
                     Do not create or modify any file.",
            expects_tool: "Read",
            file_that_must_not_exist: None,
            run_in: in_the_workspace,
        },
        Probe {
            label: "a write",
            prompt: "Create a file called probe.txt in the current directory containing the word hello.",
            expects_tool: "Write",
            file_that_must_not_exist: Some("probe.txt"),
            run_in: in_the_workspace,
        },
        Probe {
            label: "a read-only bash command",
            prompt: "Using the Bash tool, run exactly `ls` in the current directory and tell me \
                     what it printed. Do not run anything else.",
            expects_tool: "Bash",
            file_that_must_not_exist: None,
            run_in: in_the_workspace,
        },
        Probe {
            label: "a bash command with a redirect",
            prompt: "Using the Bash tool, run exactly `echo hi > redirected.txt` in the current \
                     directory. Do not use the Write tool.",
            expects_tool: "Bash",
            file_that_must_not_exist: Some("redirected.txt"),
            run_in: in_the_workspace,
        },
        // The policy allows Read/Grep/Glob only for a path that canonicalizes inside the project
        // root, so this is the probe that checks the path half of the policy against the CLI.
        Probe {
            label: "a read outside the working directory",
            prompt: "Using the Read tool, read the file /etc/hostname and tell me what it says. \
                     Do not use any other tool.",
            expects_tool: "Read",
            file_that_must_not_exist: None,
            run_in: in_the_workspace,
        },
        // `..` out of the root. The workspace is /tmp/<dir>, so this names /etc/hostname. There is
        // no Grep or Glob probe because CLI 2.1.272 has neither tool: its `system/init` lists no
        // such name (measured 2026-09-19), and a Grep probe made the model reach for ToolSearch
        // twice and then give up -- which the per-probe check above caught as vacuous.
        // The tools are deferred on this build, so the model reaches for `ToolSearch` before it can
        // use anything -- which made this the most frequent gated call in a real session and the
        // reason auto mode still felt like a wall of popups. The CLI runs it in default mode.
        Probe {
            label: "a tool-schema search",
            prompt: "Use the ToolSearch tool to load the Read tool's schema. Then reply with just: done",
            expects_tool: "ToolSearch",
            file_that_must_not_exist: None,
            run_in: in_the_workspace,
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
            run_in: in_the_workspace,
        },
        Probe {
            label: "a read that climbs out of the working directory with ..",
            prompt: "Using the Read tool, read the file ../../etc/hostname (relative to the current \
                     directory) and tell me what it says. Do not use any other tool.",
            expects_tool: "Read",
            file_that_must_not_exist: None,
            run_in: in_the_workspace,
        },
        // Expected to be refused by the CLI (the P1 verifier's reading of CLI 2.1.283's strings); on
        // 2026-09-28 it was not. `dirs` names only `.`, so nothing outside is touched.
        Probe {
            label: "find reading its starting points from a file",
            prompt: "Using the Bash tool, run exactly `find -files0-from dirs -name main.rs` in the \
                     current directory and tell me what it printed. Do not run anything else.",
            expects_tool: "Bash",
            file_that_must_not_exist: None,
            run_in: in_the_workspace,
        },
        Probe {
            label: "wc reading the files to count from a list",
            prompt: "Using the Bash tool, run exactly `wc -c --files0-from paths` in the current \
                     directory and tell me what it printed. Do not run anything else.",
            expects_tool: "Bash",
            file_that_must_not_exist: None,
            run_in: in_the_workspace,
        },
        Probe {
            label: "diff of two directories",
            prompt: "Using the Bash tool, run exactly `diff a b` in the current directory and tell \
                     me what it printed. Do not run anything else.",
            expects_tool: "Bash",
            file_that_must_not_exist: None,
            run_in: in_the_workspace,
        },
        Probe {
            label: "read-only git in a project inside a larger repository",
            prompt: "Using the Bash tool, run exactly `git status --short` in the current directory \
                     and tell me what it printed. Do not run anything else.",
            expects_tool: "Bash",
            file_that_must_not_exist: None,
            run_in: inside_a_larger_repository,
        },
        // P1 audit round 2, finding 2: a repo config key that reads an outside file. The CLI runs
        // `git blame`; the policy cards it (non-inert config key). Stricter, which passes.
        Probe {
            label: "git blame in a repo whose config reads an outside file",
            prompt: "Using the Bash tool, run exactly `git blame main.rs` in the current directory \
                     and tell me its first line. Do not run anything else.",
            expects_tool: "Bash",
            file_that_must_not_exist: None,
            run_in: with_a_config_key_reading_an_outside_file,
        },
        // P1 audit round 2, finding 3: an option-shaped filename after `--`. The CLI runs `cat`,
        // following the `-n` symlink out of the workspace; the policy cards it. Stricter, passes.
        Probe {
            label: "cat of an option-shaped symlink after --",
            prompt: "Using the Bash tool, run exactly `cat -- -n` in the current directory and tell \
                     me what it printed. Do not run anything else.",
            expects_tool: "Bash",
            file_that_must_not_exist: None,
            run_in: with_an_option_shaped_symlink,
        },
        // P1 audit round 3, blocking: a gitfile naming another repository. The CLI runs `git show`,
        // printing that repository's file; the policy cards it (no back-link). Stricter, passes.
        Probe {
            label: "git show through a gitfile naming another repository",
            prompt: "Using the Bash tool, run exactly `git show HEAD:outside.txt` in the current \
                     directory and tell me what it printed. Do not run anything else.",
            expects_tool: "Bash",
            file_that_must_not_exist: None,
            run_in: with_a_gitfile_naming_a_repository_outside,
        },
    ]
}

fn a_fresh_workspace() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("agent-policy-oracle-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("main.rs"), "fn main() { println!(\"probe\"); }\n").unwrap();
    // For the piped-grep probe: a file with a line the command can find, so a `grep` that prints
    // nothing cannot be mistaken for a refusal.
    std::fs::write(dir.join("a.txt"), "alpha\nbeta\n").unwrap();
    // For the P1-audit probes: NUL-terminated lists naming only what is already here, and two
    // directories for `diff` to compare. Nothing any of them names is outside the workspace.
    std::fs::write(dir.join("paths"), "main.rs\0").unwrap();
    std::fs::write(dir.join("dirs"), ".\0").unwrap();
    for side in ["a", "b"] {
        std::fs::create_dir_all(dir.join(side)).unwrap();
        std::fs::write(dir.join(side).join("x.txt"), format!("{side}\n")).unwrap();
    }
    dir
}

/// One `claude --print` session with **no `--permission-mode` flag**, which is the whole point:
/// that is the `default` mode whose classification this policy reproduces. Nothing installs a hook
/// here either -- a hook would answer the question this test is asking the CLI.
fn run_one_probe(workspace: &Path, prompt: &str) -> Vec<ObservedCall> {
    let output = std::process::Command::new("claude")
        .current_dir(workspace)
        // `project,local`, as the product passes, and not the user tier: on 2026-09-25 the TEST
        // profile's user settings (shared with the owner's) carried an `rtk hook claude`
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
