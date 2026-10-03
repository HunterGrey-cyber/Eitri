//! Real-CLI check that a project's own hook and MCP server start only in a session the trust gate
//! started as `Trusted`. `#[ignore]`d: it spawns real Claude sessions and bills the account it runs
//! as. **Run by hand, only on the TEST profile, with a sidecar built from a Verdandi checkout at the
//! pin and a scratch state home:**
//!
//! ```sh
//! claude --version    # record the build; nothing prints it for you
//! EITRI_VERDANDI_CHECKOUT=$HOME/src/verdandi \
//! XDG_STATE_HOME=$HOME/.cache/nv-workspace-trust/state \
//!     cargo test -p eitri-core --test workspace_trust_real_cli -- --ignored --nocapture --test-threads=1
//! # and the legacy arms, which need the compile-time feature:
//! cargo test -p eitri-core --features legacy-backend --test workspace_trust_real_cli \
//!     -- --ignored --nocapture --test-threads=1
//! ```
//!
//! The test-account wrapper sets `VERDANDI_CLAUDE_CLI_PATH`, which is what decides the sidecar's `claude`.
//! The legacy arms spawn the `claude` on `PATH`, which the same profile shims.
//!
//! **What it measures.** A scratch git project that ships its own `.claude/settings.json` (a
//! `SessionStart` and a `UserPromptSubmit` hook) and an `.mcp.json` stdio server, each of which
//! touches a marker file outside the project. One turn is run per arm and the markers are polled
//! for 20 s afterwards:
//!
//! - `Untrusted` (`[USER]`; on the legacy argv `--setting-sources user`): no marker, and the turn
//!   still completes;
//! - `Trusted` (`[USER, PROJECT, LOCAL]`): all three markers. This is also the positive control
//!   for the arm above: a CLI that never starts these things would pass the untrusted arm too.
//!
//! The arm with the user tier switched off (`[]`, `--setting-sources ""`) is its own binary,
//! `workspace_trust_no_user_real_cli`, because that setting is process-wide.
//!
//! If the hooks run but the MCP server does not, the trusted arm says so and names
//! `enabledMcpjsonServers` / `enableAllProjectMcpServers`; the fixture is not changed silently.

use agent::setting_sources::ProjectTrust;
use eitri_core::agent_backend::{AgentBackend, BackendKind};
use eitri_core::agent_bridge::SessionModeChoice;
use eitri_core::tab_set::{TabBackend, TabSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// The three things a repository's own configuration can start: a `SessionStart` hook, a
/// `UserPromptSubmit` hook and a stdio MCP server. Each touches a file of its own.
const MARKERS: [&str; 3] = ["hook", "prompt", "mcp"];

/// How long the markers are polled after the turn: an MCP server and a hook are started by the
/// CLI on its own schedule, so a missing marker proves nothing the moment the turn ends.
const MARKER_WAIT: Duration = Duration::from_secs(20);

/// One arm's scratch tree: a git project and, outside it, the directory the markers land in.
struct Fixture {
    root: PathBuf,
    project: PathBuf,
    markers: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .status()
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}

/// `$HOME/.cache/nv-workspace-trust/<pid>-<arm>/` -- never `/tmp`, namespaced by this process's pid
/// and the arm, so two arms never share markers. The fixture has the shape of a repository that
/// ships its own Claude configuration: a `SessionStart` hook, a `UserPromptSubmit` hook and an
/// `.mcp.json` stdio server, each leaving a marker file outside the project.
fn fixture(arm: &str) -> Fixture {
    let home = std::env::var("HOME").expect("HOME must be set");
    let root = PathBuf::from(home)
        .join(".cache")
        .join("nv-workspace-trust")
        .join(format!("{}-{arm}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    let markers = root.join("markers");
    std::fs::create_dir_all(project.join(".claude")).unwrap();
    std::fs::create_dir_all(&markers).unwrap();
    let (project, markers) = (project.canonicalize().unwrap(), markers.canonicalize().unwrap());
    let touch = |name: &str| format!("touch \"{}\"", markers.join(name).display());
    let settings = serde_json::json!({
        "hooks": {
            "SessionStart": [{"hooks": [{"type": "command", "command": touch("hook")}]}],
            "UserPromptSubmit": [{"hooks": [{"type": "command", "command": touch("prompt")}]}],
        }
    });
    std::fs::write(
        project.join(".claude").join("settings.json"),
        serde_json::to_string_pretty(&settings).unwrap(),
    )
    .unwrap();
    let mcp = serde_json::json!({
        "mcpServers": {
            "marker": {
                "command": "sh",
                "args": ["-c", format!("{}; exec sleep 60", touch("mcp"))],
            }
        }
    });
    std::fs::write(project.join(".mcp.json"), serde_json::to_string_pretty(&mcp).unwrap()).unwrap();
    std::fs::write(project.join("main.rs"), "fn marmalade() {}\n").unwrap();
    git(&project, &["init", "-q", "-b", "main"]);
    git(&project, &["config", "user.name", "Eitri Test"]);
    git(&project, &["config", "user.email", "eitri-test@example.invalid"]);
    git(&project, &["config", "commit.gpgsign", "false"]);
    git(&project, &["add", "."]);
    git(
        &project,
        &["commit", "-q", "-m", "a project with its own configuration"],
    );
    Fixture { root, project, markers }
}

/// Shuts the backend down when the test ends, a failing assertion included, so a real sidecar and
/// `claude` never outlive it. `shutdown` ends only the child this backend itself spawned.
struct Live(TabSet);

impl Drop for Live {
    fn drop(&mut self) {
        for mut tab in self.0.take_all() {
            if let TabBackend::Live(backend) = &mut tab.backend {
                backend.shutdown();
            }
        }
    }
}

/// Starts one session of `kind` with `trust`, runs one turn to its end (a cap of 120 s), and
/// returns which markers appeared within [`MARKER_WAIT`] after it. `expect_all` stops the wait as
/// soon as every marker is there; without it the whole wait is spent, because a marker that
/// should never appear can only be shown absent by waiting.
///
/// Asserts that the turn completed: a session that never got as far as answering has measured
/// nothing, and the failure says so, with the turn's ending and whatever the backend reported.
fn run_arm(kind: BackendKind, trust: ProjectTrust, fx: &Fixture, expect_all: bool) -> Vec<&'static str> {
    let mut set = TabSet::new(kind, SessionModeChoice::Auto);
    let tab = set.active();
    let backend = AgentBackend::start(kind, &fx.project, None, trust)
        .map_err(|e| e.message)
        .unwrap_or_else(|e| {
            panic!(
                "the session did not start ({kind:?}, {trust:?}): {e}\n(is EITRI_VERDANDI_CHECKOUT set, and is \
                 this running under a test-account wrapper? On the legacy backend an empty --setting-sources value \
                 the CLI rejects fails here or at the first turn: its stderr is the cause)"
            )
        });
    set.get_mut(tab).unwrap().backend = TabBackend::Live(backend);
    let mut live = Live(set);
    let set = &mut live.0;
    set.get_mut(tab)
        .unwrap()
        .live_mut()
        .unwrap()
        .send_turn("Reply with the single word ok.", "Reply with the single word ok.")
        .map_err(|e| e.message)
        .unwrap();
    let mut started = false;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let out = set.pump(&fx.project, true);
        if let Some(mut tripped) = out.tripped.into_iter().next() {
            let reason = tripped.reason.clone();
            tripped.backend.shutdown();
            panic!("the session closed before the turn ended ({kind:?}, {trust:?}): {reason}");
        }
        let running = set.get(tab).unwrap().turn_running();
        started |= running;
        if started && !running {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the turn did not finish within 120 s ({kind:?}, {trust:?}); backend: {:?}",
            set.get(tab).unwrap().live().unwrap().terminated_before_opening()
        );
        std::thread::sleep(Duration::from_millis(33));
    }
    {
        let tab_ref = set.get(tab).unwrap();
        let projection = tab_ref.live().unwrap().projection();
        assert!(
            projection.turn_endings.is_empty(),
            "the turn did not complete ({kind:?}, {trust:?}), so no marker below is evidence: {:?}",
            projection.turn_endings
        );
    }
    let seen = wait_for_markers(fx, expect_all);
    drop(live);
    seen
}

fn wait_for_markers(fx: &Fixture, expect_all: bool) -> Vec<&'static str> {
    let deadline = Instant::now() + MARKER_WAIT;
    loop {
        let seen: Vec<&'static str> = MARKERS
            .into_iter()
            .filter(|name| fx.markers.join(name).exists())
            .collect();
        if (expect_all && seen.len() == MARKERS.len()) || Instant::now() >= deadline {
            return seen;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// The failure message for a trusted arm that did not start everything.
fn explain_missing(seen: &[&str]) -> String {
    let missing: Vec<&str> = MARKERS.into_iter().filter(|m| !seen.contains(m)).collect();
    if seen.contains(&"hook") && missing == ["mcp"] {
        "the project hook ran but its MCP server did not start: the CLI may be asking for approval of \
         .mcp.json servers first (enabledMcpjsonServers / enableAllProjectMcpServers in a settings tier). \
         The fixture is not changed here; the main session decides."
            .to_string()
    } else {
        format!("markers missing: {missing:?} (seen: {seen:?})")
    }
}

fn assert_nothing_started(seen: &[&str], what: &str) {
    assert!(
        seen.is_empty(),
        "{what}: the project's own configuration started something: {seen:?} (the tiers must exclude \
         the project's .claude/ and .mcp.json)"
    );
}

fn assert_everything_started(seen: &[&str], what: &str) {
    assert_eq!(seen.len(), MARKERS.len(), "{what}: {}", explain_missing(seen));
}

#[test]
#[ignore = "real Claude; run under with EITRI_VERDANDI_CHECKOUT, see the module doc"]
fn untrusted_loads_no_project_hook_and_no_mcp_server() {
    assert!(
        agent::setting_sources::loads_user_settings(),
        "this binary measures the shipped default"
    );
    let fx = fixture("sidecar-untrusted");
    let seen = run_arm(BackendKind::Sidecar, ProjectTrust::Untrusted, &fx, false);
    assert_nothing_started(&seen, "sidecar, untrusted, setting sources [USER]");
}

#[test]
#[ignore = "real Claude; run under with EITRI_VERDANDI_CHECKOUT, see the module doc"]
fn trusted_runs_the_project_hook_and_starts_the_mcp_server() {
    assert!(
        agent::setting_sources::loads_user_settings(),
        "this binary measures the shipped default"
    );
    let fx = fixture("sidecar-trusted");
    let seen = run_arm(BackendKind::Sidecar, ProjectTrust::Trusted, &fx, true);
    assert_everything_started(&seen, "sidecar, trusted, setting sources [USER, PROJECT, LOCAL]");
}

#[cfg(feature = "legacy-backend")]
#[test]
#[ignore = "real Claude; run under with --features legacy-backend, see the module doc"]
fn legacy_untrusted_loads_no_project_hook_and_no_mcp_server() {
    let fx = fixture("legacy-untrusted");
    let seen = run_arm(BackendKind::Legacy, ProjectTrust::Untrusted, &fx, false);
    assert_nothing_started(&seen, "legacy, untrusted, --setting-sources user");
}

#[cfg(feature = "legacy-backend")]
#[test]
#[ignore = "real Claude; run under with --features legacy-backend, see the module doc"]
fn legacy_trusted_runs_the_project_hook_and_starts_the_mcp_server() {
    let fx = fixture("legacy-trusted");
    let seen = run_arm(BackendKind::Legacy, ProjectTrust::Trusted, &fx, true);
    assert_everything_started(&seen, "legacy, trusted, --setting-sources user,project,local");
}
