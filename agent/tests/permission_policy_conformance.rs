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
        let workspace = a_fresh_workspace();
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
        let _ = std::fs::remove_dir_all(&workspace);
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
}

/// Six probes, one billed turn each, chosen to cover both directions of the assertion rather than
/// to be exhaustive. Add more freely -- each costs a turn.
fn probes() -> Vec<Probe> {
    vec![
        Probe {
            label: "a read inside the working directory",
            prompt: "Read the file main.rs in the current directory and tell me its first line. \
                     Do not create or modify any file.",
            expects_tool: "Read",
            file_that_must_not_exist: None,
        },
        Probe {
            label: "a write",
            prompt: "Create a file called probe.txt in the current directory containing the word hello.",
            expects_tool: "Write",
            file_that_must_not_exist: Some("probe.txt"),
        },
        Probe {
            label: "a read-only bash command",
            prompt: "Using the Bash tool, run exactly `ls` in the current directory and tell me \
                     what it printed. Do not run anything else.",
            expects_tool: "Bash",
            file_that_must_not_exist: None,
        },
        Probe {
            label: "a bash command with a redirect",
            prompt: "Using the Bash tool, run exactly `echo hi > redirected.txt` in the current \
                     directory. Do not use the Write tool.",
            expects_tool: "Bash",
            file_that_must_not_exist: Some("redirected.txt"),
        },
        // The policy allows Read/Grep/Glob only for a path that canonicalizes inside the project
        // root, so this is the probe that checks the path half of the policy against the CLI.
        Probe {
            label: "a read outside the working directory",
            prompt: "Using the Read tool, read the file /etc/hostname and tell me what it says. \
                     Do not use any other tool.",
            expects_tool: "Read",
            file_that_must_not_exist: None,
        },
        // `..` out of the root. The workspace is /tmp/<dir>, so this names /etc/hostname. There is
        // no Grep or Glob probe because CLI 2.1.272 has neither tool: its `system/init` lists no
        // such name (measured 2026-09-19), and a Grep probe made the model reach for ToolSearch
        // twice and then give up -- which the per-probe check above caught as vacuous.
        Probe {
            label: "a read that climbs out of the working directory with ..",
            prompt: "Using the Read tool, read the file ../../etc/hostname (relative to the current \
                     directory) and tell me what it says. Do not use any other tool.",
            expects_tool: "Read",
            file_that_must_not_exist: None,
        },
    ]
}

fn a_fresh_workspace() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("agent-policy-oracle-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("main.rs"), "fn main() { println!(\"probe\"); }\n").unwrap();
    dir
}

/// One `claude --print` session with **no `--permission-mode` flag**, which is the whole point:
/// that is the `default` mode whose classification this policy reproduces. Nothing installs a hook
/// here either -- a hook would answer the question this test is asking the CLI.
fn run_one_probe(workspace: &Path, prompt: &str) -> Vec<ObservedCall> {
    let output = std::process::Command::new("claude")
        .current_dir(workspace)
        .args(["--print", prompt, "--output-format", "stream-json", "--verbose"])
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
