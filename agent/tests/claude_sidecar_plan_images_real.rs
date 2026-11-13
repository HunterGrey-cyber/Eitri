//! Real, `#[ignore]`d, billed: plan mode and an attached image through the sidecar provider and a conversation,
//! against a real model: entering plan mode, a plan approved into Manual, a revised plan, and an image the model
//! reads (R-P1, R-P2, R-P3, R-A1 below). Run only on a login set aside for testing, with every input stated:
//!
//! - `VERDANDI_CLAUDE_CONFIG_DIR`: an absolute, private configuration directory holding only the login's
//!   `.credentials.json` (no settings, no rules, no plugins); the CLI may refresh the login there, so the run's
//!   driver writes a refreshed login back to where it came from (a refresh can retire the old one);
//! - `CLAUDE_CONFIG_DIR`: the same directory, with `VERDANDI_CLAUDE_ACCOUNT` unset (with no account named the
//!   sidecar reads this one, not the variable above);
//! - and nothing else that could sign the CLI in: no `CLAUDE_PROFILE` or `CLAUDE_SECURESTORAGE_CONFIG_DIR`, no
//!   `ANTHROPIC_*`, `CLAUDE_CODE_*`, `AWS_*`, `CLOUD_ML_*`, `VERTEX_*`, `GOOGLE_*` or `AZURE_*` variable;
//! - `VERDANDI_CLAUDE_CLI_PATH`: the CLI to run; `EITRI_VERDANDI_CHECKOUT`: the sidecar's checkout;
//! - `EITRI_REAL_SCRATCH`: an empty absolute directory the sessions work in (`hello.txt` is written there);
//! - `EITRI_REAL_MARKER_PNG`: a PNG with the text `C42-MARKER-7` drawn in it.
//!
//! The test is the host: it answers each permission request itself (reads, `EnterPlanMode` and the plan file
//! allowed, `Bash` and any path outside the scratch directory or the plan files denied, judged on the path the
//! tool names) and records what it saw as `EVIDENCE {json}` lines. Three sessions, one turn each. Nothing connects
//! until every input above is stated and no other login rides in on the environment.
#![cfg(target_os = "linux")]

use agent::setting_sources::ProjectTrust;
use agent::{
    AgentConversation, AgentDomainEvent, AgentProvider, AttachmentMeta, ClaudeSidecarProvider, InteractionResolution,
    PermissionDecision, PlanExecutionMode, PlanPhase, PromptOrigin, ResolvePermissionRequest, TurnImage,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long one wait may take before the run is called stuck; a real model is slow, and this is no speed bound.
const STUCK: Duration = Duration::from_secs(300);

fn stated(name: &str) -> PathBuf {
    let value = std::env::var(name).unwrap_or_else(|_| panic!("{name} must be set (see the file's header)"));
    let path = PathBuf::from(value);
    assert!(path.is_absolute(), "{name} must be absolute");
    path
}

/// The inputs a run must state before anything connects: without them the provider would find a CLI and a login
/// on its own, and a billed turn could spend (and refresh) a login nobody handed to this test.
fn stated_run() -> (PathBuf, PathBuf) {
    let config = stated("VERDANDI_CLAUDE_CONFIG_DIR");
    assert!(
        config.is_dir(),
        "VERDANDI_CLAUDE_CONFIG_DIR must be an existing directory"
    );
    // The sidecar uses VERDANDI_CLAUDE_CONFIG_DIR only for a named account; with none named it takes the inherited
    // CLAUDE_CONFIG_DIR, or the default login when that is unset. Naming no account and both variables the same
    // directory leaves it one place to look, whichever way it reads them.
    assert!(
        std::env::var_os("VERDANDI_CLAUDE_ACCOUNT").is_none_or(|value| value.is_empty()),
        "VERDANDI_CLAUDE_ACCOUNT must be unset: the stated directory is the login"
    );
    assert_eq!(
        std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).as_deref(),
        Some(config.as_path()),
        "CLAUDE_CONFIG_DIR must name the same directory as VERDANDI_CLAUDE_CONFIG_DIR"
    );
    // Nothing inherited may pick another login or provider for the CLI: the rest of its account tuple
    // (CLAUDE_SECURESTORAGE_CONFIG_DIR alone moves where the login is read and refreshed, even empty), a key or
    // token, or a cloud provider's selector and credentials. Refused as whole families, not name by name.
    let foreign = foreign_login_vars(std::env::vars_os().map(|(name, _)| name.to_string_lossy().into_owned()));
    assert!(
        foreign.is_empty(),
        "these must be unset, or the CLI may sign in with them: {foreign:?}"
    );
    // Settings could carry an apiKeyHelper (another way to sign in) or rules; the directory holds the login only.
    for settings in ["settings.json", "settings.local.json"] {
        assert!(
            !config.join(settings).exists(),
            "{settings} must not be in the stated directory"
        );
    }
    let cli = stated("VERDANDI_CLAUDE_CLI_PATH");
    assert!(cli.is_file(), "VERDANDI_CLAUDE_CLI_PATH must name the CLI to run");
    assert!(
        stated("EITRI_VERDANDI_CHECKOUT").is_dir(),
        "EITRI_VERDANDI_CHECKOUT must be the sidecar's checkout"
    );
    (config, cli)
}

/// The inherited variables that could give the CLI a login or provider other than the stated directory's.
fn foreign_login_vars(names: impl Iterator<Item = String>) -> Vec<String> {
    const FAMILIES: [&str; 7] = [
        "ANTHROPIC_",
        "CLAUDE_CODE_",
        "AWS_",
        "CLOUD_ML_",
        "VERTEX_",
        "GOOGLE_",
        "AZURE_",
    ];
    const NAMES: [&str; 2] = ["CLAUDE_PROFILE", "CLAUDE_SECURESTORAGE_CONFIG_DIR"];
    let mut found: Vec<String> = names
        .filter(|name| NAMES.contains(&name.as_str()) || FAMILIES.iter().any(|family| name.starts_with(family)))
        .collect();
    found.sort();
    found
}

/// What the host answers a permission request with.
#[derive(Debug, PartialEq)]
enum Answer {
    Allow,
    Deny,
    /// Left waiting, as the card a person would see; only an edit of the scratch directory's `hello.txt`.
    Hold,
}

/// The host's whole rule, on the path a tool names: reads and writes inside the scratch directory or the plan
/// files, reads with no path and a relative pattern (they work in the session's directory, the scratch one), the
/// bookkeeping tools; everything else, `Bash` included, is denied.
fn decide(tool: &str, input: &serde_json::Value, scratch: &Path, plans: &Path, hold_edits: bool) -> Answer {
    let at = target(input);
    let inside = at.as_deref().is_some_and(|path| path.starts_with(scratch));
    let plan_file = at.as_deref().is_some_and(|path| path.starts_with(plans));
    let edit = matches!(tool, "Write" | "Edit" | "NotebookEdit");
    if edit && hold_edits && inside && at.as_deref().is_some_and(|path| path.ends_with("hello.txt")) {
        return Answer::Hold;
    }
    let pathless = ["path", "file_path", "notebook_path"]
        .iter()
        .all(|key| input.get(*key).is_none());
    let relative_pattern = input.get("pattern").and_then(|v| v.as_str()).is_none_or(|pattern| {
        let pattern = Path::new(pattern);
        pattern.is_relative()
            && pattern
                .components()
                .all(|part| !matches!(part, std::path::Component::ParentDir))
    });
    match tool {
        "ToolSearch" | "TodoWrite" | "EnterPlanMode" => Answer::Allow,
        // A pattern is checked even with an allowed path: an absolute one decides where Glob searches.
        "Read" | "Glob" | "Grep" | "LS" if (inside || plan_file || pathless) && relative_pattern => Answer::Allow,
        "Write" | "Edit" if inside || plan_file => Answer::Allow,
        _ => Answer::Deny,
    }
}

/// The path a file tool names, when it is absolute and has no `..` or `.` part; anything else is no target.
fn target(input: &serde_json::Value) -> Option<PathBuf> {
    let named = ["file_path", "notebook_path", "path"]
        .iter()
        .find_map(|key| input.get(*key).and_then(|v| v.as_str()))?;
    let path = PathBuf::from(named);
    let plain = path
        .components()
        .all(|part| matches!(part, std::path::Component::RootDir | std::path::Component::Normal(_)));
    (path.is_absolute() && plain).then_some(path)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn evidence(what: &str, value: serde_json::Value) {
    println!("EVIDENCE {}", serde_json::json!({ "what": what, "value": value }));
}

/// A conversation on the real sidecar, its events kept, its permission requests answered by `answer`.
struct Real {
    conversation: AgentConversation,
    provider: Arc<ClaudeSidecarProvider>,
    events: Vec<AgentDomainEvent>,
    scratch: PathBuf,
    /// Where the CLI writes its plan files: the stated configuration directory's `plans`.
    plans: PathBuf,
    /// Permission requests seen, by id: (tool, input as JSON, what this host answered).
    asked: Vec<(String, String, String, String)>,
    answered: std::collections::HashSet<String>,
    /// Requests left unanswered on purpose (the card the person would see), by id.
    held: Vec<String>,
    hold_edits: bool,
}

impl Real {
    fn new(name: &str) -> Self {
        let (config, _cli) = stated_run();
        let scratch = stated("EITRI_REAL_SCRATCH").join(name);
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(scratch.join("hello.txt"), "hello\n").unwrap();
        let provider =
            Arc::new(ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string()).expect("the sidecar"));
        let conversation =
            AgentConversation::create(provider.clone(), &scratch, ProjectTrust::Untrusted).expect("a conversation");
        let mut real = Self {
            conversation,
            provider,
            events: Vec::new(),
            scratch,
            plans: config.join("plans"),
            asked: Vec::new(),
            answered: Default::default(),
            held: Vec::new(),
            hold_edits: false,
        };
        // The real CLI opens its session (`SessionOpened`) only once the first turn starts, so nothing is waited
        // for here: the offer is declared when the conversation is created.
        assert!(
            real.conversation.projection().planning.offered,
            "the session must offer plan mode (sidecar 0.5.0 or later)"
        );
        real
    }

    fn pump(&mut self) {
        match self.conversation.take_ui_delivery() {
            agent::ingestion::UiDelivery::Events(events) => self.events.extend(events),
            agent::ingestion::UiDelivery::Resync | agent::ingestion::UiDelivery::Nothing => {}
        }
        let session = self.conversation.projection().session_id.clone();
        let pending: Vec<(String, String, serde_json::Value)> = self
            .conversation
            .projection()
            .pending_permissions
            .iter()
            .map(|(id, p)| (id.clone(), p.tool_name.clone(), p.input.clone()))
            .collect();
        for (id, tool, value) in pending {
            let input = value.to_string();
            if self.answered.contains(&id) || self.held.contains(&id) {
                continue;
            }
            let decision = match decide(&tool, &value, &self.scratch, &self.plans, self.hold_edits) {
                Answer::Hold => {
                    self.held.push(id.clone());
                    self.asked.push((id, tool, input, "held (a card)".into()));
                    continue;
                }
                Answer::Allow => PermissionDecision::Allow,
                Answer::Deny => PermissionDecision::Deny {
                    reason: Some("not in this check".into()),
                },
            };
            self.asked.push((id.clone(), tool, input, format!("{decision:?}")));
            self.answered.insert(id.clone());
            self.provider
                .resolve_permission(ResolvePermissionRequest {
                    session_id: session.clone().expect("a session"),
                    permission_id: id,
                    decision,
                })
                .expect("the answer goes out");
        }
    }

    fn until(&mut self, what: &str, done: impl Fn(&Self) -> bool) {
        let started = Instant::now();
        loop {
            self.pump();
            if done(self) {
                return;
            }
            assert!(started.elapsed() < STUCK, "never saw {what}; events {:#?}", self.events);
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn turn_ended(&self, before: Option<u64>) -> bool {
        let p = self.conversation.projection();
        p.active_turn_id.is_none() && p.last_turn_end_seq.is_some() && p.last_turn_end_seq != before
    }

    fn waiting_plan(&self) -> Option<(String, String, String)> {
        let p = self.conversation.projection();
        p.waiting_plan()
            .map(|plan| (plan.interaction_id.clone(), plan.plan_sha256.clone(), plan.plan.clone()))
    }

    fn text(&self) -> String {
        self.events
            .iter()
            .filter_map(|e| match e {
                AgentDomainEvent::ContentDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn cli_modes(&self) -> Vec<String> {
        self.events
            .iter()
            .filter_map(|e| match e {
                AgentDomainEvent::CliPermissionMode { reported } => Some(reported.clone()),
                AgentDomainEvent::CliPlanMode { .. } => Some("plan".into()),
                _ => None,
            })
            .collect()
    }

    fn close(self) {
        let session = self.conversation.projection().session_id.clone();
        if let Some(session_id) = session {
            let _ = self.provider.close_session(agent::CloseSessionRequest { session_id });
        }
    }
}

const PLAN_ASK: &str = "In this directory, hello.txt says \"hello\". Plan a one-line change that makes it say \
    \"hello, plan\". Do not change anything yet: write a very short plan (two lines at most) and call ExitPlanMode \
    with it. Once the plan is approved, make the change and reply with the word DONE.";

/// R-P1 and R-P3 in one session: the host's own entry before the first turn, a plan, "keep planning" with feedback
/// (the model revises), the revision approved into Auto; the CLI then reports `auto` again.
#[test]
#[ignore]
fn real_host_entry_keep_planning_then_approve_auto() {
    let mut real = Real::new("rp1");
    let send = real
        .conversation
        .set_planning(true, now_ms())
        .expect("the entry goes out");
    evidence(
        "R-P1 entry acknowledged",
        serde_json::json!(format!("{:?}", send.wait(Duration::from_secs(30)))),
    );
    let before = real.conversation.projection().last_turn_end_seq;
    real.conversation.send_turn(PLAN_ASK).expect("the turn goes");
    real.until("the CLI to report plan", |r| {
        r.conversation.projection().planning.phase == PlanPhase::Planning
    });
    real.until("the first plan", |r| r.waiting_plan().is_some());
    let (first_id, _, first) = real.waiting_plan().unwrap();
    evidence(
        "R-P3 first plan",
        serde_json::json!({"interaction": first_id, "plan": first}),
    );
    let feedback = "Also add a second line to the plan that says: then reply DONE-REVISED instead of DONE.";
    real.conversation
        .resolve_plan(
            &first_id,
            InteractionResolution::PlanKeep {
                feedback: feedback.into(),
            },
        )
        .expect("keep planning goes");
    real.until("the revised plan", |r| {
        r.waiting_plan().is_some_and(|(id, _, _)| id != first_id)
    });
    let (second_id, digest, second) = real.waiting_plan().unwrap();
    evidence(
        "R-P3 revised plan",
        serde_json::json!({"interaction": second_id, "sha256": digest, "plan": second}),
    );
    assert_ne!(first, second, "R-P3: the model revised its plan");
    let modes_before = real.cli_modes().len();
    let _approval = real
        .conversation
        .resolve_plan(
            &second_id,
            InteractionResolution::PlanApprove {
                mode: PlanExecutionMode::Auto,
                plan_sha256: digest,
            },
        )
        .expect("the approval goes");
    real.until("the turn's end", |r| r.turn_ended(before));
    let after: Vec<String> = real.cli_modes()[modes_before..].to_vec();
    evidence(
        "R-P1 modes the CLI reported after the approval",
        serde_json::json!(after),
    );
    evidence(
        "R-P1 phase",
        serde_json::json!(format!("{:?}", real.conversation.projection().planning.phase)),
    );
    evidence("R-P1 permissions", serde_json::json!(real.asked));
    evidence("R-P1 text", serde_json::json!(real.text()));
    assert_eq!(
        after.last().map(String::as_str),
        Some("auto"),
        "R-P1: the CLI reports auto again"
    );
    assert_eq!(real.conversation.projection().planning.phase, PlanPhase::Executing);
    real.close();
}

/// R-P2: the model enters plan mode itself and proposes; approved into Manual; the edit after it reaches this host
/// and waits for an answer (the card a Manual tab draws), and is then allowed.
#[test]
#[ignore]
fn real_model_plan_approved_manual_then_the_edit_waits() {
    let mut real = Real::new("rp2");
    let before = real.conversation.projection().last_turn_end_seq;
    real.conversation
        .send_turn(&format!("Enter plan mode first. {PLAN_ASK}"))
        .expect("the turn goes");
    real.until("the plan", |r| r.waiting_plan().is_some());
    real.until("the CLI to report plan", |r| {
        r.conversation.projection().planning.phase == PlanPhase::Planning
    });
    let (id, digest, plan) = real.waiting_plan().unwrap();
    evidence(
        "R-P2 plan",
        serde_json::json!({"interaction": id, "sha256": digest, "plan": plan}),
    );
    real.hold_edits = true;
    real.conversation
        .resolve_plan(
            &id,
            InteractionResolution::PlanApprove {
                mode: PlanExecutionMode::Manual,
                plan_sha256: digest,
            },
        )
        .expect("the approval goes");
    real.until("the edit's request", |r| !r.held.is_empty());
    let held = real.held[0].clone();
    std::thread::sleep(Duration::from_secs(3));
    real.pump();
    assert!(
        real.conversation.projection().pending_permissions.contains_key(&held),
        "R-P2: the edit still waits for the person 3 s later"
    );
    evidence(
        "R-P2 the edit waited",
        serde_json::json!({"permission": held, "asked": real.asked}),
    );
    let session_id = real.conversation.projection().session_id.clone().unwrap();
    real.provider
        .resolve_permission(ResolvePermissionRequest {
            session_id,
            permission_id: held.clone(),
            decision: PermissionDecision::Allow,
        })
        .unwrap();
    real.answered.insert(held);
    real.hold_edits = false;
    real.until("the turn's end", |r| r.turn_ended(before));
    evidence(
        "R-P2 file",
        serde_json::json!(std::fs::read_to_string(real.scratch.join("hello.txt")).ok()),
    );
    evidence("R-P2 modes", serde_json::json!(real.cli_modes()));
    evidence("R-P2 text", serde_json::json!(real.text()));
    real.close();
}

/// R-A1: a PNG with `C42-MARKER-7` drawn in it, attached to a message; the reply names the marker.
#[test]
#[ignore]
fn real_the_model_reads_an_attached_image() {
    let png = std::fs::read(stated("EITRI_REAL_MARKER_PNG")).expect("the marker PNG");
    let sha = {
        use sha2::Digest as _;
        format!("{:x}", sha2::Sha256::digest(&png))
    };
    let (width, height) = (
        u32::from_be_bytes(png[16..20].try_into().unwrap()),
        u32::from_be_bytes(png[20..24].try_into().unwrap()),
    );
    let mut real = Real::new("ra1");
    let before = real.conversation.projection().last_turn_end_seq;
    let image = TurnImage {
        meta: AttachmentMeta {
            media_type: "image/png".into(),
            bytes: png.len() as u64,
            width,
            height,
            sha256: sha.clone(),
        },
        data: Arc::from(png.into_boxed_slice()),
    };
    let text = "What text is drawn in the attached image? Reply with that text only.";
    real.conversation
        .send_turn_as(
            text,
            text,
            PromptOrigin::Typed,
            &uuid::Uuid::new_v4().to_string(),
            vec![image],
        )
        .expect("the turn with the image goes");
    real.until("the turn's end", |r| r.turn_ended(before));
    let reply = real.text();
    evidence(
        "R-A1 image",
        serde_json::json!({"sha256": sha, "width": width, "height": height}),
    );
    evidence("R-A1 reply", serde_json::json!(reply));
    assert!(
        reply.contains("C42-MARKER-7"),
        "R-A1: the reply names the marker: {reply}"
    );
    real.close();
}

/// Runs without a login: the path rule the host answers by.
#[test]
fn a_write_target_is_judged_on_its_path() {
    let at = |v: serde_json::Value| target(&v);
    let scratch = PathBuf::from("/tmp/plan-images/rp1");
    assert!(
        at(serde_json::json!({"file_path": "/tmp/plan-images/rp1/hello.txt"})).is_some_and(|p| p.starts_with(&scratch))
    );
    assert_eq!(
        at(serde_json::json!({"file_path": "/tmp/plan-images/rp1/../outside.txt"})),
        None
    );
    assert_eq!(at(serde_json::json!({"file_path": "relative.txt"})), None);
    let outside = at(serde_json::json!({"file_path": "/tmp/outside.txt", "content": "/tmp/plan-images/rp1"}));
    assert!(outside.is_some_and(|p| !p.starts_with(&scratch)));
}

/// Runs without a login: every answer the host gives, on each side of its paths.
#[test]
fn the_host_answers_by_tool_and_path() {
    let scratch = Path::new("/tmp/plan-images/rp2");
    let plans = Path::new("/tmp/cfg/plans");
    let answer = |tool: &str, input: serde_json::Value, hold: bool| decide(tool, &input, scratch, plans, hold);
    // A held edit is only the scratch directory's hello.txt; one elsewhere is denied, held or not.
    assert_eq!(
        answer(
            "Edit",
            serde_json::json!({"file_path": "/tmp/plan-images/rp2/hello.txt"}),
            true
        ),
        Answer::Hold
    );
    assert_eq!(
        answer(
            "Write",
            serde_json::json!({"file_path": "/home/someone/hello.txt"}),
            true
        ),
        Answer::Deny
    );
    assert_eq!(
        answer(
            "NotebookEdit",
            serde_json::json!({"notebook_path": "/elsewhere/hello.txt"}),
            true
        ),
        Answer::Deny
    );
    assert_eq!(
        answer(
            "Edit",
            serde_json::json!({"file_path": "/tmp/plan-images/rp2/hello.txt"}),
            false
        ),
        Answer::Allow
    );
    assert_eq!(
        answer("Write", serde_json::json!({"file_path": "/tmp/cfg/plans/p.md"}), false),
        Answer::Allow
    );
    assert_eq!(
        answer(
            "Write",
            serde_json::json!({"file_path": "/tmp/cfg/.credentials.json"}),
            false
        ),
        Answer::Deny
    );
    assert_eq!(
        answer(
            "Write",
            serde_json::json!({"file_path": "/tmp/plan-images/rp2/../x"}),
            false
        ),
        Answer::Deny
    );
    assert_eq!(
        answer(
            "NotebookEdit",
            serde_json::json!({"notebook_path": "/tmp/plan-images/rp2/n.ipynb"}),
            false
        ),
        Answer::Deny
    );
    // Reads: inside, or pathless with a relative pattern.
    assert_eq!(
        answer(
            "Read",
            serde_json::json!({"file_path": "/tmp/plan-images/rp2/hello.txt"}),
            false
        ),
        Answer::Allow
    );
    assert_eq!(
        answer(
            "Read",
            serde_json::json!({"file_path": "/tmp/cfg/.credentials.json"}),
            false
        ),
        Answer::Deny
    );
    assert_eq!(
        answer("Glob", serde_json::json!({"pattern": "**/*.txt"}), false),
        Answer::Allow
    );
    assert_eq!(
        answer("Glob", serde_json::json!({"pattern": "/home/**/x"}), false),
        Answer::Deny
    );
    assert_eq!(
        answer(
            "Glob",
            serde_json::json!({"path": "/tmp/plan-images/rp2", "pattern": "/etc/*"}),
            false
        ),
        Answer::Deny
    );
    assert_eq!(
        answer(
            "Glob",
            serde_json::json!({"path": "/tmp/plan-images/rp2", "pattern": "*.txt"}),
            false
        ),
        Answer::Allow
    );
    assert_eq!(
        answer("Grep", serde_json::json!({"pattern": "../*"}), false),
        Answer::Deny
    );
    assert_eq!(
        answer("Grep", serde_json::json!({"pattern": "hello", "path": "/etc"}), false),
        Answer::Deny
    );
    assert_eq!(
        answer("Bash", serde_json::json!({"command": "true"}), false),
        Answer::Deny
    );
    assert_eq!(answer("EnterPlanMode", serde_json::json!({}), false), Answer::Allow);
}

/// Runs without a login: the inherited variables the run refuses, by family.
#[test]
fn inherited_logins_and_providers_are_refused() {
    let names = |list: &[&str]| foreign_login_vars(list.iter().map(|name| name.to_string()));
    assert_eq!(
        names(&[
            "CLAUDE_CODE_USE_BEDROCK",
            "AWS_BEARER_TOKEN_BEDROCK",
            "PATH",
            "CLAUDE_CONFIG_DIR"
        ]),
        ["AWS_BEARER_TOKEN_BEDROCK", "CLAUDE_CODE_USE_BEDROCK"]
    );
    assert_eq!(
        names(&[
            "ANTHROPIC_API_KEY",
            "CLAUDE_SECURESTORAGE_CONFIG_DIR",
            "CLAUDE_PROFILE",
            "HOME"
        ]),
        ["ANTHROPIC_API_KEY", "CLAUDE_PROFILE", "CLAUDE_SECURESTORAGE_CONFIG_DIR"]
    );
    assert!(names(&["VERDANDI_CLAUDE_CONFIG_DIR", "EITRI_REAL_SCRATCH", "TMPDIR"]).is_empty());
}
