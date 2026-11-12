//! What a failed tab says about the sidecar that ran it. A tab whose `claude` was untested says the sentence that
//! points at `eitri sidecar check`; a tab that ran on a selected sidecar update says which one and how to go back.
//! Both are added to the failure text, after the text itself, which is never changed: the panel recognises a
//! failure by substrings of it.
#![cfg(target_os = "linux")]

use agent::setting_sources::ProjectTrust;
use agent::{
    AgentConversation, AgentDomainEvent, AgentProvider, ProviderCapabilities, ProviderInfo, UNTESTED_CLAUDE_NOTE,
};
use eitri_core::agent_backend::{AgentBackend, AgentId, BackendKind};
use eitri_core::agent_bridge::SessionModeChoice;
use eitri_core::tab_set::{FailedHow, TabBackend, TabSet};
use eitri_core::tabs::TabId;
use eitri_core::test_providers::RecordingProvider;
use std::path::PathBuf;
use std::sync::Arc;

const UPDATE_LINE: &str = "Sidecar update 0.3.2 ran this tab. To go back: eitri sidecar use bundled";

const DIAGNOSTIC: &str =
    "claude-sidecar: host CLI version diagnostic: claude 2.1.320 is inside the supported range but was not tested with this sidecar";

/// The failures the panel's recogniser matches by substring (`agent-ui/web/src/problems.ts`), which must
/// still be found in the text after the sentences are added.
const RECOGNISED: [&str; 4] = [
    "the connection to the provider ended before this session did",
    "refusing to start: host claude CLI version 3.0.0: claude 3.0.0 is outside this sidecar's supported range (>=2.1.0 <2.2.0)",
    "could not determine the installed claude CLI version",
    "Please run /login",
];

/// A recording provider that reports `info` as the sidecar's own handshake would, and refuses to open a
/// session when `refuses`.
struct Informed {
    inner: RecordingProvider,
    info: ProviderInfo,
    refuses: bool,
}

impl AgentProvider for Informed {
    fn capabilities(&self) -> ProviderCapabilities {
        self.inner.capabilities()
    }
    fn info(&self) -> ProviderInfo {
        self.info.clone()
    }
    fn create_session(&self, request: agent::CreateSessionRequest) -> Result<String, agent::ProviderError> {
        if self.refuses {
            return Err(agent::ProviderError::UnsupportedCapability("a session"));
        }
        self.inner.create_session(request)
    }
    fn resume_session(&self, request: agent::ResumeSessionRequest) -> Result<String, agent::ProviderError> {
        self.inner.resume_session(request)
    }
    fn send_turn(&self, request: agent::SendTurnRequest) -> Result<String, agent::ProviderError> {
        self.inner.send_turn(request)
    }
    fn interrupt_turn(&self, request: agent::InterruptTurnRequest) -> Result<(), agent::ProviderError> {
        self.inner.interrupt_turn(request)
    }
    fn resolve_permission(&self, request: agent::ResolvePermissionRequest) -> Result<(), agent::ProviderError> {
        self.inner.resolve_permission(request)
    }
    fn close_session(&self, request: agent::CloseSessionRequest) -> Result<(), agent::ProviderError> {
        self.inner.close_session(request)
    }
    fn pump(&self) -> Vec<AgentDomainEvent> {
        self.inner.pump()
    }
}

fn project_dir() -> PathBuf {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("sidecar-failed-note-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// The info of a sidecar that ran a tab: `untested` says the `claude` was inside the range but untested,
/// `update` the version of the selected update the tab ran on.
fn info(untested: bool, update: Option<&str>) -> ProviderInfo {
    let mut info = ProviderInfo {
        sidecar_version: update.unwrap_or("0.4.0").to_string(),
        protocol_major: 3,
        protocol_minor: 25,
        ..ProviderInfo::default()
    };
    if untested {
        info.startup_diagnostics = vec![DIAGNOSTIC.to_string(), UNTESTED_CLAUDE_NOTE.to_string()];
    }
    info.sidecar_update = update.map(str::to_string);
    info
}

/// A set with one tab live on a backend that reports `info`.
fn tab_running(info: ProviderInfo) -> (TabSet, TabId) {
    agent::state_dirs::redirect_state_to_a_test_root();
    let provider = Arc::new(Informed {
        inner: RecordingProvider::default(),
        info,
        refuses: false,
    });
    let conversation = AgentConversation::create(provider, &project_dir(), ProjectTrust::Trusted).unwrap();
    let backend = AgentBackend::Conversation {
        agent: AgentId::Claude,
        conversation: Box::new(conversation),
    };
    let mut tabs = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Manual);
    let tab = tabs.active();
    tabs.get_mut(tab)
        .expect("the tab")
        .set_backend(TabBackend::Live(backend));
    (tabs, tab)
}

/// The failure text the tab shows once its session is lost, `how`.
fn failure_after(tabs: &mut TabSet, tab: TabId, reason: &str, how: FailedHow) -> String {
    if let Some(retired) = tabs.fail_session(tab, reason.to_string(), how, false) {
        retired.into_backend().shutdown();
    }
    let TabBackend::Failed { reason } = tabs.get(tab).expect("the tab").backend() else {
        panic!("the tab did not fail");
    };
    reason.clone()
}

const BOTH_WAYS: [FailedHow; 2] = [FailedHow::AfterStart, FailedHow::StartNeverOpened];

/// The failure of a session that would not open on a connected sidecar, new or resumed.
fn opening_failure(info: ProviderInfo, resume: Option<&str>) -> String {
    agent::state_dirs::redirect_state_to_a_test_root();
    let provider = Arc::new(Informed {
        inner: RecordingProvider::default(),
        info,
        refuses: true,
    });
    AgentBackend::open_claude_session(provider, &project_dir(), resume, None, ProjectTrust::Trusted, None)
        .err()
        .expect("the session did not open")
        .message
}

const OPENINGS: [(Option<&str>, &str); 2] = [
    (None, "failed to create a Claude session: "),
    (
        Some("0193e5f0-7d3a-7000-8000-000000000001"),
        "could not continue the previous session: ",
    ),
];

#[test]
fn a_session_that_will_not_open_on_an_update_says_which_and_how_to_go_back() {
    for (resume, opening) in OPENINGS {
        let text = opening_failure(info(false, Some("0.3.2")), resume);
        assert!(text.starts_with(opening), "the text itself comes first: {text}");
        assert!(text.contains(UPDATE_LINE), "{text}");
        assert!(!text.contains(UNTESTED_CLAUDE_NOTE), "that claude was tested: {text}");
    }
}

#[test]
fn a_session_that_will_not_open_on_an_untested_claude_leaves_the_note() {
    for (resume, opening) in OPENINGS {
        let text = opening_failure(info(true, None), resume);
        assert!(text.starts_with(opening), "the text itself comes first: {text}");
        assert!(text.contains(UNTESTED_CLAUDE_NOTE), "{text}");
        assert!(!text.contains("Sidecar update"), "no update ran this tab: {text}");
    }
}

#[test]
fn a_session_that_will_not_open_with_neither_adds_nothing() {
    for (resume, opening) in OPENINGS {
        let text = opening_failure(info(false, None), resume);
        assert!(text.starts_with(opening), "{text}");
        assert!(!text.contains('\n'), "{text}");
    }
}

#[test]
fn a_backend_whose_claude_was_untested_leaves_the_note_in_the_failure() {
    for how in BOTH_WAYS {
        let (mut tabs, tab) = tab_running(info(true, None));
        let text = failure_after(&mut tabs, tab, "claude exited", how);
        assert!(
            text.starts_with("claude exited"),
            "the text itself comes first ({how:?}): {text}"
        );
        assert!(text.contains(UNTESTED_CLAUDE_NOTE), "({how:?}) {text}");
        assert!(
            !text.contains("Sidecar update"),
            "no update ran this tab ({how:?}): {text}"
        );
    }
}

#[test]
fn a_tab_that_ran_on_an_update_says_which_and_how_to_go_back() {
    let (mut tabs, tab) = tab_running(info(false, Some("0.3.2")));
    let text = failure_after(&mut tabs, tab, "claude exited", FailedHow::AfterStart);
    assert!(text.starts_with("claude exited"), "the text itself comes first: {text}");
    assert!(text.contains(UPDATE_LINE), "{text}");
    assert_eq!(text.matches("Sidecar update").count(), 1, "said once: {text}");
    assert!(!text.contains(UNTESTED_CLAUDE_NOTE), "that claude was tested: {text}");
}

#[test]
fn a_tab_on_an_update_with_an_untested_claude_says_both() {
    let (mut tabs, tab) = tab_running(info(true, Some("0.3.2")));
    let text = failure_after(&mut tabs, tab, "claude exited", FailedHow::AfterStart);
    assert!(text.starts_with("claude exited"), "{text}");
    assert!(text.contains(UNTESTED_CLAUDE_NOTE), "{text}");
    assert!(text.contains(UPDATE_LINE), "{text}");
}

#[test]
fn a_backend_with_neither_leaves_the_failure_as_it_was() {
    for how in BOTH_WAYS {
        let (mut tabs, tab) = tab_running(info(false, None));
        assert_eq!(
            failure_after(&mut tabs, tab, "claude exited", how),
            "claude exited",
            "({how:?})"
        );
    }
}

#[test]
fn a_tab_with_no_live_backend_adds_nothing() {
    let mut tabs = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Manual);
    let tab = tabs.active();
    assert_eq!(
        failure_after(&mut tabs, tab, "claude exited", FailedHow::AfterStart),
        "claude exited"
    );
}

#[test]
fn what_the_panel_recognises_stays_whole_in_the_text() {
    for recognised in RECOGNISED {
        for (untested, update) in [(true, None), (false, Some("0.3.2")), (true, Some("0.3.2"))] {
            let (mut tabs, tab) = tab_running(info(untested, update));
            let text = failure_after(&mut tabs, tab, recognised, FailedHow::AfterStart);
            assert!(
                text.starts_with(recognised),
                "the failure is kept whole at the start ({untested}, {update:?}): {text}"
            );
            assert_eq!(
                text.matches(recognised).count(),
                1,
                "and not repeated ({untested}, {update:?}): {text}"
            );
        }
    }
}
