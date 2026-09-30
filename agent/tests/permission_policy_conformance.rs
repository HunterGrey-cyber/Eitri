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
//!
//! **2026-09-28 (v1 item 4A, the acceptEdits fast path): NOT YET RUN.** `Write`, `Edit` and
//! `NotebookEdit` stopped reproducing `default` (which denies every one of them) and started
//! reproducing `acceptEdits` instead (module doc on `permission_policy`, "The acceptEdits fast
//! path"). Three probes were changed accordingly: the one plain in-project `Write` probe is now run
//! under `--permission-mode acceptEdits` rather than no flag, its expected side effect flipped from
//! "must not exist" to "no longer checked" (the CLI is now expected to create it, same as the
//! policy), and two probes were added alongside it -- an `Edit` of an existing in-project file, and
//! a `Write` under a protected directory (`.vscode/settings.json`), both also under `acceptEdits`,
//! to state the parity claim for BOTH halves of the fast path: an ordinary in-project edit runs, and
//! a protected path still cards, on both sides. Seventeen probes now, up from fifteen. Not run here
//! (this task's own scope excludes running the real CLI); left for the next the test-account wrapper pass.
//!
//! **Fix round 1 (2026-09-28), "minor": the `Edit` probe above was found likely vacuous before it
//! ever ran once.** `Edit`'s own `validateInput` refuses a file the session has not `Read` yet,
//! ahead of hooks and permissions in the CLI's own phase order -- so the original prompt's bare
//! `Edit` call would have failed validation, never reached a permission decision, and still counted
//! as "the CLI allowed it" (a validation error is not a [`REFUSAL_PHRASES`] match) while satisfying
//! `expects_tool: "Edit"` regardless. The prompt now tells the model to `Read` first, and
//! `file_must_contain` (`Probe`'s own doc) asserts the edit was actually applied -- still not run
//! here, so this is a claim about what the next the test-account wrapper pass will actually exercise, not a
//! new passing result.
//!
//! **Fix round 2 (2026-09-28), still NOT RUN.** Two ways this oracle could miscount an edit the CLI
//! refused as one it allowed are closed: [`REFUSAL_PHRASES`] gains "refusing to" (the CLI's own
//! `Write`/`Edit`/`NotebookEdit` denials for a symlink leaf and an undeterminable path read "Refusing
//! to write X: ..." -- a case exists today where the policy allows and the CLI refuses: a write to an
//! in-root symlink leaf, harmless because the CLI's re-check after the hook denies it anyway), and
//! the in-project `Write` probe now checks `probe.txt` really holds "hello". A second `#[ignore]`d
//! test, [`a_users_own_ask_rule_still_stops_an_edit_the_gate_allowed`], checks the one CLI behaviour
//! item 4A newly leans on -- a user's `Edit(...)` ask rule beating the gate's `allow` -- and is owed
//! to the same pass.

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
    // CLI 2.1.283's edit-tool denials, read from its bundle (fix round 2 of item 4A, 2026-09-28; not
    // yet seen in a run): "Refusing to write X: it is a symbolic link. ..." (`Zlt`, from
    // Write/Edit/NotebookEdit's own checkPermissions) and "Refusing to write X: where it leads on
    // disk could not be determined ..." (`xl`). Without this a policy-allows/CLI-refuses edit counted
    // as agreement -- the one divergence this oracle exists to catch. The bundle has about thirty
    // other "Refusing to ..." safety refusals (a link that changed after its check, a clone URL);
    // matching them too can only report a spurious PERMISSIVE failure, the safe direction above.
    "refusing to",
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

/// **NOT YET RUN (item 4A fix round 2, 2026-09-28): owed to the next the test-account wrapper pass.** A
/// user's own `permissions.ask` rule for an edit must still stop a write the gate's hook allowed.
///
/// Why it is load-bearing now: before item 4A every edit carded on Eitri's side, so a human
/// answered it whatever the CLI would have done after. Since 4A an in-project `Write` gets the
/// hook's `allow` with no card, and what stands between the user's `Edit(...)` ask rule and a silent
/// write is the CLI's own re-check. Read from CLI 2.1.283's bundle, not yet observed: after a hook
/// `allow`, `EQn` runs `DR` (checkRuleBasedPermissions); `DR` runs the tool's `checkPermissions`,
/// whose `Lb` returns an ask when `Ca(<spelling>, ..., "edit", "ask")` finds an `Edit(...)` ask rule,
/// and `DR` hands that ask back (`Pve`: a rule-forced ask); `EQn` then calls `canUseTool` instead of
/// taking the hook's allow. Under `--print` with no permission prompt tool -- the legacy backend's
/// shape, and this probe's -- that is a refusal; on the sidecar it is an O3 provider prompt carrying
/// `matched_ask_rule`, which eitri-core cards in every mode
/// (`agent_backend::tests::a_users_own_ask_rule_still_cards_an_edit_the_fast_path_allowed`). The O3
/// record lists "a real permissions.ask rule surviving the gate's allow" as not yet verified on the
/// real CLI; this is that check, for an edit.
///
/// The probe: the product's own CLI posture (`--permission-mode default`, `--setting-sources
/// project,local`, a `PreToolUse` hook with matcher `*` that answers `allow` to every call -- what
/// the policy answers for this `Write`, asserted below), `Bash` withheld so the model cannot create
/// the file another way the rule does not name, a project `.claude/settings.json` asking for
/// `Edit(probe-asked.txt)`, and a request to create that file. The file must not exist afterwards.
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
    /// Added fix round 1 (2026-09-28), "minor": the `Edit` probe below used to pass even if the CLI
    /// never reached a permission decision at all. `Edit`'s own `validateInput` refuses a file that
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
    /// Every probe here was `None` (`default`) until 2026-09-28 (v1 item 4A, the acceptEdits fast
    /// path): `permission_policy` reproduces `default` for every tool EXCEPT `Write`/`Edit`/
    /// `NotebookEdit`, which now reproduce `acceptEdits` instead (module doc). Comparing one of
    /// those three against a `default`-mode CLI run would not test what the policy claims to
    /// reproduce -- the CLI denies every write under `default`, on purpose, regardless of path -- so
    /// a probe whose `expects_tool` is one of those three must set this to `Some("acceptEdits")`.
    /// Every other probe stays compared against `default`, unchanged.
    permission_mode: Option<&'static str>,
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

/// Seventeen probes, one billed turn each, chosen to cover both directions of the assertion rather
/// than to be exhaustive. Add more freely -- each costs a turn.
///
/// The last three (2026-09-28, v1 item 4A) run under `--permission-mode acceptEdits`, not
/// `default`: one in-project `Write`, one in-project `Edit` of an existing file (both expected
/// allowed, same as the policy's new fast path), and one `Write` under a protected directory
/// (expected carded on both sides, same as `default` already carded every write). See the module
/// doc's 2026-09-28 (v1 item 4A) entry.
///
/// The six before those (2026-09-28, the P1 audit and its round 2) exercise the cards added then. Round 2
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
            file_must_contain: None,
            run_in: in_the_workspace,
            permission_mode: None,
        },
        // v1 item 4A (the acceptEdits fast path): a plain in-project `Write` is no longer compared
        // against `default` -- `default` denies every write on purpose, which would report this as
        // "more permissive than the CLI" for a divergence the module doc explains, not a defect.
        // Compared against `acceptEdits` instead, where the CLI's own fast path applies too, so this
        // probe is now a real acceptEdits-parity check rather than a `default`-mode one.
        Probe {
            label: "an in-project write, under acceptEdits",
            prompt: "Create a file called probe.txt in the current directory containing the word hello.",
            expects_tool: "Write",
            file_that_must_not_exist: None,
            // Fix round 2 (2026-09-28): the write must really have happened. A refusal worded
            // outside `REFUSAL_PHRASES` would otherwise count as the CLI allowing it.
            file_must_contain: Some(("probe.txt", "hello")),
            run_in: in_the_workspace,
            permission_mode: Some("acceptEdits"),
        },
        // The other edit tool this fast path covers, and the other shape of target: `Edit` requires
        // an existing file, where `Write` above created a new one.
        Probe {
            label: "an in-project edit of an existing file, under acceptEdits",
            // Fix round 1 (2026-09-28), "minor": `Edit`'s own `validateInput` refuses a file that
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
        // Expected to be refused by the CLI (the P1 verifier's reading of CLI 2.1.283's strings); on
        // 2026-09-28 it was not. `dirs` names only `.`, so nothing outside is touched.
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
        // P1 audit round 2, finding 2: a repo config key that reads an outside file. The CLI runs
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
        // P1 audit round 2, finding 3: an option-shaped filename after `--`. The CLI runs `cat`,
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
        // P1 audit round 3, blocking: a gitfile naming another repository. The CLI runs `git show`,
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
    // Canonical, as the session's own root is: the policy cards every path-judged call under a root
    // that does not resolve to itself (fix round 4, 2026-09-28), and a `$TMPDIR` may pass through a
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
