//! Real-CLI probe of two places workspace trust's discovery does not read: memory files above the
//! git top level, and Claude Code's own worktree checkouts under `.claude/worktrees/`. `#[ignore]`d:
//! it spawns real Claude sessions and bills the account it runs as. **Run by hand, only on the TEST
//! profile, with a sidecar at the pin and a scratch state home:**
//!
//! ```sh
//! claude --version    # record the build; nothing prints it for you
//! XDG_STATE_HOME=$HOME/.cache/nv-trust-memory/state \
//!     cargo test -p eitri-core --test workspace_trust_memory_real_cli -- --ignored --nocapture --test-threads=1
//! ```
//!
//! (`EITRI_VERDANDI_CHECKOUT` or the `eitri setup` artifact locates the sidecar, as for the other
//! `workspace_trust_*_real_cli` probes; the test-account wrapper sets `VERDANDI_CLAUDE_CLI_PATH`.)
//!
//! **Why it matters.** Discovery walks from the root up to the git top level and reads `.claude/`,
//! `.mcp.json`, `CLAUDE.md` and `CLAUDE.local.md` in each directory, except `.claude/worktrees/`. What
//! the trusted session loads that the prompt never showed is loaded on the strength of an answer the
//! user gave about something else; what the untrusted session loads at all is the project's reach into
//! a session that trusted nothing.
//!
//! **The fixture.** `outer/` holds a git repository `outer/project/` (the root, which is its own top
//! level). Above the top level, `outer/` carries every memory and listing place the CLI might read
//! from a parent directory: `CLAUDE.md`, `CLAUDE.local.md`, `.claude/CLAUDE.md`, `.claude/rules/`, a
//! skill, an agent and a command. Inside the root, `.claude/worktrees/w/` is a real `git worktree add`
//! checkout that carries the same places plus a `.claude/settings.json` with hooks and an `.mcp.json`
//! server, each of which touches a marker file outside the project. Each memory file, skill, agent and
//! command names its own token; nothing but its own file holds it.
//!
//! **How a token is seen.** Model-visible text is the only window onto what the CLI put in the
//! context, so each turn asks the model to repeat every token it can see. Turn 1 uses no tool: what it
//! lists was loaded at the start. Turn 2 reads `sub/notes.txt` and `.claude/worktrees/w/notes.txt`
//! (neither holds a token) and lists again: what is new was loaded because a file in that directory was
//! read. A token can only be repeated if it was in the context, so a listed token is evidence; an
//! absent one is evidence only beside the controls, which the trusted arm asserts:
//! - the root's own `CLAUDE.md` and skill appear at the start (the model reports memory and listings);
//! - `sub/CLAUDE.md` appears after `sub/notes.txt` is read (the CLI loads a subdirectory's memory then).
//!
//! - the root's own agent and command appear at the start, and its `SessionStart` and
//!   `UserPromptSubmit` hooks and `.mcp.json` server run (the markers `root-*`).
//!
//! The untrusted arm (`[USER]`) asserts that none of the root's own places or markers appears: the
//! project tier was off. It has no positive control of its own (nothing the TEST profile's user tier
//! holds is written here), so its absences lean on the trusted arm's controls. The measured places are printed per arm as a table, then judged against
//! [`EXPECTED`]: a CLI build that starts loading something the product says it does not fails here.
//!
//! **Measured on CLI 2.1.293 (2026-10-09).** Trusted (`[USER, PROJECT, LOCAL]`): above the top level,
//! `CLAUDE.md`, `CLAUDE.local.md`, `.claude/CLAUDE.md`, `.claude/rules/` and the `.mcp.json` server all
//! load at the start; that directory's `.claude/settings.json` and `settings.local.json` hooks, skills,
//! agents and commands do not. Nothing in the worktree checkout loads, at the start or after a file in it
//! is read, and its hooks and server never run. `sub/CLAUDE.md` loads once `sub/notes.txt` is read.
//! Untrusted (`[USER]`): none of it, the root's own memory, skill and server included. Discovery reads
//! the places above the top level that load (`claude_above_top` in `core/src/project_trust/discover.rs`);
//! a subdirectory's `CLAUDE.md` is not discovered, and the trust prompt says so.
//!
//! A turn that used a tool it was told not to, or read another file than the two, measured nothing and
//! fails as INCONCLUSIVE. A card is answered `allow` only for a `Read` of one of the two notes, else
//! `deny`.

use agent::setting_sources::ProjectTrust;
use agent::PermissionDecision;
use eitri_core::agent_backend::{AgentBackend, BackendKind};
use eitri_core::agent_bridge::SessionModeChoice;
use eitri_core::tab_set::{TabBackend, TabSet};
use eitri_core::tabs::TabId;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Every place a token is written, by a short name. `outer/...` is above the git top level,
/// `wt/...` is `project/.claude/worktrees/w/...`, the rest are the root's own (the controls).
const PLACES: [&str; 18] = [
    "root/CLAUDE.md",
    "root/skill",
    "root/agent",
    "root/command",
    "sub/CLAUDE.md",
    "outer/CLAUDE.md",
    "outer/CLAUDE.local.md",
    "outer/.claude/CLAUDE.md",
    "outer/.claude/rules",
    "outer/skill",
    "outer/agent",
    "outer/command",
    "wt/CLAUDE.md",
    "wt/CLAUDE.local.md",
    "wt/.claude/CLAUDE.md",
    "wt/skill",
    "wt/agent",
    "wt/command",
];

/// The marker files a hook or MCP server touches when the CLI runs it: the worktree checkout's own
/// (`wt-*`), the directory above the top level's (`outer-*`), and the root's own hooks and server
/// (`root-*`, the controls: a CLI that runs no project hook or server would pass every other line).
const MARKERS: [&str; 9] = [
    "root-hook",
    "root-prompt",
    "root-mcp",
    "outer-hook",
    "outer-local-hook",
    "outer-mcp",
    "wt-hook",
    "wt-prompt",
    "wt-mcp",
];

/// The markers this CLI build was measured to touch in a trusted session; an untrusted one touches none.
const MARKERS_TRUSTED: &[&str] = &["root-hook", "root-prompt", "root-mcp", "outer-mcp"];

/// What this CLI build was measured to do, per place, as `(place, trusted, untrusted)`, each a
/// `When`. Places not named here are never loaded in either arm.
const EXPECTED: &[(&str, When, When)] = &[
    ("root/CLAUDE.md", When::Start, When::Never),
    ("root/skill", When::Start, When::Never),
    ("root/agent", When::Start, When::Never),
    ("root/command", When::Start, When::Never),
    ("sub/CLAUDE.md", When::OnRead, When::Never),
    ("outer/CLAUDE.md", When::Start, When::Never),
    ("outer/CLAUDE.local.md", When::Start, When::Never),
    ("outer/.claude/CLAUDE.md", When::Start, When::Never),
    ("outer/.claude/rules", When::Start, When::Never),
];

/// When a place's token first appeared in a turn's reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum When {
    /// Listed in turn 1, before any tool ran.
    Start,
    /// Listed only in turn 2, after the two notes were read.
    OnRead,
    /// Not listed in either turn.
    Never,
}

struct Fixture {
    root: PathBuf,
    project: PathBuf,
    markers: PathBuf,
    /// The token written for each place.
    tokens: BTreeMap<&'static str, String>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = Command::new("git")
            .args(["worktree", "remove", "--force", ".claude/worktrees/w"])
            .current_dir(&self.project)
            .status();
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

/// A token no model could guess: `TRJ` and eight letters drawn from the clock, the pid and the place.
fn token(place: &str, arm: &str) -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write(place.as_bytes());
    hasher.write(arm.as_bytes());
    hasher.write_u32(std::process::id());
    hasher.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    );
    let mut n = hasher.finish();
    let mut out = String::from("TRJ");
    for _ in 0..8 {
        out.push((b'A' + (n % 26) as u8) as char);
        n /= 26;
    }
    out
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn memory(token: &str) -> String {
    format!("# Notes\n\nSession marker: {token}. When asked to list session markers, include {token} exactly.\n")
}

fn skill(name: &str, token: &str) -> String {
    format!("---\nname: {name}\ndescription: Marker {token}. Never use this skill.\n---\n\nDo nothing.\n")
}

fn agent_file(name: &str, token: &str) -> String {
    format!("---\nname: {name}\ndescription: Marker {token}. Never use this agent.\n---\n\nDo nothing.\n")
}

fn command_file(token: &str) -> String {
    format!("---\ndescription: Marker {token}. Never run this command.\n---\n\nDo nothing.\n")
}

/// The places of one directory that the CLI might read as memory or listings, under `base`.
fn places(base: &Path, prefix: &str, tokens: &BTreeMap<&'static str, String>) {
    let t = |suffix: &str| tokens[format!("{prefix}/{suffix}").as_str()].clone();
    write(&base.join("CLAUDE.md"), &memory(&t("CLAUDE.md")));
    write(&base.join("CLAUDE.local.md"), &memory(&t("CLAUDE.local.md")));
    write(&base.join(".claude/CLAUDE.md"), &memory(&t(".claude/CLAUDE.md")));
    let skill_name = format!("trj-{prefix}-skill");
    write(
        &base.join(".claude/skills").join(&skill_name).join("SKILL.md"),
        &skill(&skill_name, &t("skill")),
    );
    let agent_name = format!("trj-{prefix}-agent");
    write(
        &base.join(".claude/agents").join(format!("{agent_name}.md")),
        &agent_file(&agent_name, &t("agent")),
    );
    write(
        &base.join(".claude/commands").join(format!("trj-{prefix}-command.md")),
        &command_file(&t("command")),
    );
}

/// `$HOME/.cache/nv-trust-memory/<pid>-<arm>/`: never `/tmp`, so the CLI's walk up from the root
/// passes through directories that hold no memory file of their own up to `$HOME`.
fn fixture(arm: &str) -> Fixture {
    let home = std::env::var("HOME").expect("HOME must be set");
    let root = PathBuf::from(home)
        .join(".cache")
        .join("nv-trust-memory")
        .join(format!("{}-{arm}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let outer = root.join("outer");
    let project = outer.join("project");
    let markers = root.join("markers");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&markers).unwrap();
    let (outer, project, markers) = (
        outer.canonicalize().unwrap(),
        project.canonicalize().unwrap(),
        markers.canonicalize().unwrap(),
    );
    let tokens: BTreeMap<&'static str, String> = PLACES.iter().map(|place| (*place, token(place, arm))).collect();

    let touch = |name: &str| format!("touch \"{}\"", markers.join(name).display());
    let session_start = |name: &str| {
        serde_json::to_string_pretty(&serde_json::json!({
            "hooks": {"SessionStart": [{"hooks": [{"type": "command", "command": touch(name)}]}]}
        }))
        .unwrap()
    };
    let server = |name: &str, marker: &str| {
        serde_json::to_string_pretty(&serde_json::json!({
            "mcpServers": {name: {"command": "sh", "args": ["-c", format!("{}; exec sleep 60", touch(marker))]}}
        }))
        .unwrap()
    };

    // Above the top level.
    places(&outer, "outer", &tokens);
    write(
        &outer.join(".claude/rules/above.md"),
        &memory(&tokens["outer/.claude/rules"]),
    );
    write(&outer.join(".claude/settings.json"), &session_start("outer-hook"));
    write(
        &outer.join(".claude/settings.local.json"),
        &session_start("outer-local-hook"),
    );
    write(&outer.join(".mcp.json"), &server("trj-outer", "outer-mcp"));

    // The root, a repository of its own.
    write(&project.join("CLAUDE.md"), &memory(&tokens["root/CLAUDE.md"]));
    write(
        &project.join(".claude/skills/trj-root-skill/SKILL.md"),
        &skill("trj-root-skill", &tokens["root/skill"]),
    );
    write(&project.join("sub/CLAUDE.md"), &memory(&tokens["sub/CLAUDE.md"]));
    write(&project.join("sub/notes.txt"), &format!("{}\n", NOTE_TEXTS[0]));
    write(&project.join(".gitignore"), ".claude/worktrees/\n");
    write(&project.join(".mcp.json"), &server("trj-root", "root-mcp"));
    let root_hooks = serde_json::json!({
        "hooks": {
            "SessionStart": [{"hooks": [{"type": "command", "command": touch("root-hook")}]}],
            "UserPromptSubmit": [{"hooks": [{"type": "command", "command": touch("root-prompt")}]}],
        }
    });
    write(
        &project.join(".claude/settings.json"),
        &serde_json::to_string_pretty(&root_hooks).unwrap(),
    );
    write(
        &project.join(".claude/agents/trj-root-agent.md"),
        &agent_file("trj-root-agent", &tokens["root/agent"]),
    );
    write(
        &project.join(".claude/commands/trj-root-command.md"),
        &command_file(&tokens["root/command"]),
    );
    git(&project, &["init", "-q", "-b", "main"]);
    git(&project, &["config", "user.name", "Eitri Test"]);
    git(&project, &["config", "user.email", "eitri-test@example.invalid"]);
    git(&project, &["config", "commit.gpgsign", "false"]);
    git(&project, &["add", "."]);
    git(&project, &["commit", "-q", "-m", "the root"]);

    // `.claude/worktrees/w`, a real worktree on its own branch, as `claude --worktree` makes one.
    git(&project, &["worktree", "add", "-q", "-b", "wt", ".claude/worktrees/w"]);
    let wt = project.join(".claude/worktrees/w");
    // The checkout carries the root's committed files; give every place its own token.
    let _ = std::fs::remove_dir_all(wt.join(".claude/skills/trj-root-skill"));
    let _ = std::fs::remove_file(wt.join(".claude/agents/trj-root-agent.md"));
    let _ = std::fs::remove_file(wt.join(".claude/commands/trj-root-command.md"));
    let _ = std::fs::remove_dir_all(wt.join("sub"));
    places(&wt, "wt", &tokens);
    write(&wt.join("notes.txt"), &format!("{}\n", NOTE_TEXTS[1]));
    let settings = serde_json::json!({
        "hooks": {
            "SessionStart": [{"hooks": [{"type": "command", "command": touch("wt-hook")}]}],
            "UserPromptSubmit": [{"hooks": [{"type": "command", "command": touch("wt-prompt")}]}],
        }
    });
    write(
        &wt.join(".claude/settings.json"),
        &serde_json::to_string_pretty(&settings).unwrap(),
    );
    write(&wt.join(".mcp.json"), &server("trj-wt", "wt-mcp"));
    git(&wt, &["add", "."]);
    git(&wt, &["commit", "-q", "-m", "the worktree's own configuration"]);

    Fixture {
        root,
        project,
        markers,
        tokens,
    }
}

/// Shuts the backend down when the test ends, a failing assertion included.
struct Live(TabSet);

impl Drop for Live {
    fn drop(&mut self) {
        for tab in self.0.take_all() {
            if let Some(retired) = tab.retired {
                retired.into_backend().shutdown();
            }
        }
    }
}

const NOTES: [&str; 2] = ["sub/notes.txt", ".claude/worktrees/w/notes.txt"];
/// What each note says, so a read is known to have returned it.
const NOTE_TEXTS: [&str; 2] = ["Nothing to see here.", "Nothing to see here either."];

/// Whether a tool call is a `Read` of one of the two notes.
fn reads_a_note(name: &str, input: &serde_json::Value, project: &Path) -> bool {
    let path = input["file_path"].as_str().unwrap_or_default();
    name == "Read"
        && NOTES
            .iter()
            .any(|note| path == *note || Path::new(path) == project.join(note))
}

/// One tool call of a turn: its name, its input, and whether it completed without an error or a
/// refusal and its result holds `expect` (the note's text, for a `Read` of a note).
struct Call {
    name: String,
    input: serde_json::Value,
    result: String,
    succeeded: bool,
}

impl std::fmt::Debug for Call {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {} (succeeded: {})", self.name, self.input, self.succeeded)
    }
}

/// Sends `prompt`, pumps until the turn ends, and returns the reply text of this turn and the calls it
/// made.
fn drive_turn(set: &mut TabSet, tab: TabId, project: &Path, prompt: &str, label: &str) -> (String, Vec<Call>) {
    let (texts_before, calls_before) = {
        let projection = set.get(tab).unwrap().live().unwrap().projection();
        (projection.transcript.len(), projection.tool_calls.len())
    };
    set.get_mut(tab)
        .unwrap()
        .live_mut()
        .unwrap()
        .send_turn(prompt, prompt, agent::PromptOrigin::Composed)
        .map_err(|e| e.message)
        .unwrap();
    let mut started = false;
    let mut answered = std::collections::HashSet::new();
    let deadline = Instant::now() + Duration::from_secs(240);
    loop {
        let out = set.pump(project, true);
        if let Some(tripped) = out.tripped.into_iter().next() {
            let reason = tripped.reason.clone();
            tripped.retired.into_backend().shutdown();
            panic!("{label}: the session closed before the turn ended: {reason}");
        }
        let waiting: Vec<(String, String, serde_json::Value)> = {
            let projection = set.get(tab).unwrap().live().unwrap().projection();
            projection
                .pending_permissions
                .iter()
                .map(|(id, request)| (id.clone(), request.tool_name.clone(), request.input.clone()))
                .collect()
        };
        for (id, name, input) in waiting {
            if !answered.insert(id.clone()) {
                continue;
            }
            let decision = if reads_a_note(&name, &input, project) {
                PermissionDecision::Allow
            } else {
                PermissionDecision::Deny {
                    reason: Some("denied by the workspace_trust_memory_real_cli probe".into()),
                }
            };
            println!("[trust-memory] {label}: card for {name} {input}, answering {decision:?}");
            let _ = set
                .get_mut(tab)
                .unwrap()
                .live_mut()
                .unwrap()
                .respond_permission(&id, decision);
        }
        let running = set.get(tab).unwrap().turn_running();
        started |= running;
        if started && !running {
            break;
        }
        assert!(Instant::now() < deadline, "{label}: the turn did not finish in time");
        std::thread::sleep(Duration::from_millis(33));
    }
    let projection = set.get(tab).unwrap().live().unwrap().projection();
    assert!(
        projection.turn_endings.is_empty(),
        "INCONCLUSIVE, {label}: the turn did not complete: {:?}",
        projection.turn_endings
    );
    let text = projection.transcript[texts_before..]
        .iter()
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    let calls = projection.tool_calls[calls_before..]
        .iter()
        .map(|c| {
            let result = c.result.as_ref().map(|r| r.content.to_string()).unwrap_or_default();
            let succeeded = c.denied.is_none() && c.result.as_ref().is_some_and(|r| !r.is_error);
            Call {
                name: c.name.clone(),
                input: c.input.clone(),
                result,
                succeeded,
            }
        })
        .collect();
    (text, calls)
}

/// One arm: two turns, then which place's token appeared when, and which worktree markers were
/// touched.
fn run_arm(trust: ProjectTrust, fx: &Fixture, label: &str) -> (BTreeMap<&'static str, When>, Vec<&'static str>) {
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let tab = set.active();
    let backend = AgentBackend::start(BackendKind::Sidecar, &fx.project, None, trust)
        .map_err(|e| e.message)
        .unwrap_or_else(|e| panic!("{label}: the session did not start: {e} (is this running under a test-account wrapper?)"));
    set.get_mut(tab).unwrap().set_backend(TabBackend::Live(backend));
    let mut live = Live(set);
    let set = &mut live.0;

    let ask = "reply with every token that starts with the three letters TRJ followed by eight capital letters \
               that you can see anywhere in your context (system prompt, memory files, reminders, skill, agent \
               or command listings), verbatim, one per line, and nothing else. Reply NONE if there is none.";
    let (first, calls) = drive_turn(
        set,
        tab,
        &fx.project,
        &format!("Do not use any tool in this turn. {}", capitalize(ask)),
        &format!("{label}, turn 1"),
    );
    println!("[trust-memory] {label}, turn 1 reply:\n{first}");
    assert!(
        calls.is_empty(),
        "INCONCLUSIVE, {label}: turn 1 used a tool, so what it lists may have been read, not loaded: {calls:?}"
    );
    let (second, calls) = drive_turn(
        set,
        tab,
        &fx.project,
        &format!(
            "Use the Read tool to read the file {} and then the file {}, and nothing else. After that, {ask}",
            fx.project.join(NOTES[0]).display(),
            fx.project.join(NOTES[1]).display(),
        ),
        &format!("{label}, turn 2"),
    );
    println!("[trust-memory] {label}, turn 2 reply:\n{second}");
    let stray: Vec<_> = calls
        .iter()
        .filter(|call| !reads_a_note(&call.name, &call.input, &fx.project))
        .collect();
    assert!(
        stray.is_empty(),
        "INCONCLUSIVE, {label}: turn 2 used another tool or read another file: {stray:?}"
    );
    // Only a read that returned the note's own text puts the directory's memory in reach; a denied or
    // failed read would make every token of that directory look absent.
    for (note, text) in NOTES.iter().zip(NOTE_TEXTS) {
        assert!(
            calls
                .iter()
                .any(|call| reads_a_note(&call.name, &call.input, &fx.project)
                    && call.input["file_path"].as_str().unwrap_or_default().ends_with(note)
                    && call.succeeded
                    && call.result.contains(text)),
            "INCONCLUSIVE, {label}: turn 2 never read {note} successfully: {calls:?}"
        );
    }
    let seen = PLACES
        .iter()
        .map(|place| {
            let token = &fx.tokens[place];
            let when = if first.contains(token.as_str()) {
                When::Start
            } else if second.contains(token.as_str()) {
                When::OnRead
            } else {
                When::Never
            };
            (*place, when)
        })
        .collect();
    if trust == ProjectTrust::Trusted {
        let deadline = Instant::now() + Duration::from_secs(120);
        while !MARKERS_TRUSTED.iter().all(|name| fx.markers.join(name).exists()) {
            assert!(
                Instant::now() < deadline,
                "the trusted hooks and servers never reported their starts"
            );
            std::thread::yield_now();
        }
    }
    // Shutdown reaps the CLI before the final negative checks of its hooks and server starts.
    drop(live);
    let markers = MARKERS
        .into_iter()
        .filter(|name| fx.markers.join(name).exists())
        .collect();
    (seen, markers)
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

fn expected(place: &str, trusted: bool) -> When {
    EXPECTED
        .iter()
        .find(|(p, _, _)| *p == place)
        .map(|(_, t, u)| if trusted { *t } else { *u })
        .unwrap_or(When::Never)
}

fn report_and_judge(seen: &BTreeMap<&'static str, When>, markers: &[&str], trusted: bool, label: &str) {
    println!("[trust-memory] {label}: place, seen, expected");
    let mut wrong = Vec::new();
    for (place, when) in seen {
        let want = expected(place, trusted);
        println!("[trust-memory]   {place:<26} {when:?} (expected {want:?})");
        if *when != want {
            wrong.push(format!("{place}: {when:?}, expected {want:?}"));
        }
    }
    println!("[trust-memory] {label}: markers touched: {markers:?}");
    let want: Vec<&str> = if trusted { MARKERS_TRUSTED.to_vec() } else { Vec::new() };
    assert_eq!(markers, want, "{label}: the hooks and servers that ran");
    assert!(wrong.is_empty(), "{label}: {wrong:#?}");
}

#[test]
#[ignore = "real Claude; run under a test-account wrapper, see the module doc"]
fn trusted_memory_above_the_top_level_and_in_worktree_checkouts() {
    assert!(
        agent::setting_sources::loads_user_settings(),
        "this binary measures the shipped default"
    );
    let fx = fixture("trusted");
    let (seen, markers) = run_arm(ProjectTrust::Trusted, &fx, "trusted");
    // The controls: without them an absent token proves nothing.
    assert_eq!(
        seen["root/CLAUDE.md"],
        When::Start,
        "INCONCLUSIVE: the root's own CLAUDE.md was not reported"
    );
    assert_eq!(
        seen["root/skill"],
        When::Start,
        "INCONCLUSIVE: the root's own skill was not reported"
    );
    for place in ["root/agent", "root/command"] {
        assert_eq!(
            seen[place],
            When::Start,
            "INCONCLUSIVE: the root's own {place} was not reported"
        );
    }
    assert_eq!(
        seen["sub/CLAUDE.md"],
        When::OnRead,
        "INCONCLUSIVE: a subdirectory's CLAUDE.md was not reported after a file in it was read"
    );
    report_and_judge(&seen, &markers, true, "trusted, [USER, PROJECT, LOCAL]");
}

#[test]
#[ignore = "real Claude; run under a test-account wrapper, see the module doc"]
fn untrusted_memory_above_the_top_level_and_in_worktree_checkouts() {
    assert!(
        agent::setting_sources::loads_user_settings(),
        "this binary measures the shipped default"
    );
    let fx = fixture("untrusted");
    let (seen, markers) = run_arm(ProjectTrust::Untrusted, &fx, "untrusted");
    report_and_judge(&seen, &markers, false, "untrusted, [USER]");
}
