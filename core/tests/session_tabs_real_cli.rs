//! Two real sidecar sessions in one process, as two session tabs (session tabs spec §3.10).
//! `#[ignore]`d: it spawns two real Verdandi sidecars and two real `claude` processes and bills the
//! account they run as.
//!
//! **Run it only on the TEST profile, through the resolved binary, never a bundled one** (CLAUDE.md,
//! "Working conventions": the account is part of the brief, never an assumption), **and only with a
//! scratch `XDG_STATE_HOME`** (this plan's Global Constraints: "Every cargo test runs with a scratch
//! state home"). `AgentConversation`'s ingestion thread persists a `ConversationRecord` the moment
//! either backend's first `SessionOpened` folds (`agent::ingestion::persist_record`,
//! `agent::state_dirs::conversations_dir`), which this test drives twice, for real, on purpose --
//! without `XDG_STATE_HOME` set, `conversations_dir()` falls back to the operator's own
//! `$HOME/.local/state/eitri/conversations/` and two real, never-cleaned-up records land there
//! (the test-account wrapper does not set or clear it either, so it is left exactly as the caller's shell
//! has it):
//!
//! ```sh
//! claude --version    # record the build; nothing prints it for you
//! XDG_STATE_HOME=/tmp/nv-tabs-real-state \
//!     cargo test -p eitri-core --test session_tabs_real_cli -- --ignored --nocapture
//! ```
//!
//! The test-account wrapper sets `VERDANDI_CLAUDE_CLI_PATH` to the `claude-wrapper` launcher, which is what
//! decides the sidecar's CLI (CLAUDE.md, the environment table); `PATH` alone does not.

use eitri_core::agent_backend::{AgentBackend, BackendKind};
use eitri_core::agent_bridge::SessionModeChoice;
use eitri_core::tab_set::{TabBackend, TabSet};
use std::time::{Duration, Instant};

fn project() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("nv-tabs-real-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("main.rs"), "fn marmalade() {}\n").unwrap();
    dir.canonicalize().unwrap()
}

fn start(dir: &std::path::Path) -> AgentBackend {
    AgentBackend::start(BackendKind::Sidecar, dir, None)
        .map_err(|e| e.message)
        .expect("a sidecar session starts; is this running under a test-account wrapper?")
}

fn transcript(set: &TabSet, tab: eitri_core::tabs::TabId) -> String {
    let backend = set.get(tab).unwrap().live().unwrap();
    let projection = backend.projection();
    projection
        .transcript
        .iter()
        .map(|m| m.text.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
#[ignore = "real Claude, two sidecars; run under a test-account wrapper, see the module doc"]
fn two_tabs_run_at_once_and_a_background_card_reaches_its_own_session() {
    let dir = project();
    let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
    let background = set.active();
    set.get_mut(background).unwrap().backend = TabBackend::Live(start(&dir));
    let foreground = set.open();
    set.get_mut(foreground).unwrap().backend = TabBackend::Live(start(&dir));

    // 1. The background tab reads a file inside the project (the policy answers it, no card) while
    //    the active tab streams a reply.
    set.get_mut(background)
        .unwrap()
        .live_mut()
        .unwrap()
        .send_turn(
            "Read main.rs and tell me the name of the function in it.",
            "read main.rs",
        )
        .map_err(|e| e.message)
        .unwrap();
    set.get_mut(foreground)
        .unwrap()
        .live_mut()
        .unwrap()
        .send_turn("Count from 1 to 30, one number per line.", "count")
        .map_err(|e| e.message)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        let out = set.pump(&dir, true);
        if let Some(payload) = out.active_payload {
            let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
            assert_eq!(value["tab"], foreground.0, "only the active tab is ever dispatched");
        }
        let done = !set.get(background).unwrap().turn_running() && transcript(&set, background).contains("marmalade");
        if done && !set.get(foreground).unwrap().turn_running() {
            break;
        }
        assert!(Instant::now() < deadline, "the two turns did not both finish");
        std::thread::sleep(Duration::from_millis(33));
    }
    assert_eq!(
        set.get(background).unwrap().attention.attention().pending,
        0,
        "the Read needed nobody"
    );
    assert!(transcript(&set, foreground).contains("30"));

    // 2. A card in the background tab, answered after a switch, reaches that tab's session.
    set.get_mut(background)
        .unwrap()
        .live_mut()
        .unwrap()
        .send_turn(
            "Create a file named tabs-proof.txt containing the word ok.",
            "write a file",
        )
        .map_err(|e| e.message)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(180);
    while set.get(background).unwrap().attention.attention().pending == 0 {
        set.pump(&dir, true);
        assert!(Instant::now() < deadline, "the Write never became a card");
        std::thread::sleep(Duration::from_millis(33));
    }
    assert!(set.select(background));
    let snapshot: serde_json::Value = serde_json::from_str(&set.active_state_payloads()[0]).unwrap();
    let permission_id = snapshot["state"]["pendingPermissions"][0]["permissionId"]
        .as_str()
        .unwrap()
        .to_string();
    set.get_mut(background)
        .unwrap()
        .live_mut()
        .unwrap()
        .respond_permission(&permission_id, agent::PermissionDecision::Allow)
        .map_err(|e| e.message)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(180);
    while !dir.join("tabs-proof.txt").exists() || set.get(background).unwrap().turn_running() {
        set.pump(&dir, true);
        assert!(Instant::now() < deadline, "the approved Write did not land");
        std::thread::sleep(Duration::from_millis(33));
    }
    assert!(
        !set.get(foreground).unwrap().turn_running(),
        "the other session was never asked"
    );

    for mut tab in set.take_all() {
        if let TabBackend::Live(backend) = &mut tab.backend {
            backend.shutdown();
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}
