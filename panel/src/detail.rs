//! The chooser and the detail popover: the rows that say what a tab runs, under which settings and
//! mode, and the titles the CLI's transcript gives them.

use super::*;

/// Titles the worker read: `hello` is owed again after a change, and a tab resumed with its record's title
/// takes the CLI's once it is known (the `tabs` envelope that follows carries it).
pub(super) fn apply_title_reads(state_ref: &mut AgentPanelState) {
    if state_ref.titles.take_changed() {
        state_ref.hello_titles_stale = true;
        state_ref.chooser_titles_stale = true;
    }
    // Every tick, not only after a read: a read can land before the tab it titles is installed or knows
    // its session, and a tab with no record's title left is skipped at no cost.
    let cwd = state_ref.canonical_project_dir.clone();
    let titles = &state_ref.titles;
    state_ref.tabs.retitle_from(|session| titles.known_title(&cwd, session));
}

/// The titles of the records the last chooser listed that the cache knows better since it was sent, to the
/// page it was opened in. The page applies them only to a chooser still open, so sending them after it
/// closed reopens nothing.
pub(super) fn send_chooser_titles_if_changed(state_ref: &mut AgentPanelState, out: &mut Outbox) {
    if !std::mem::take(&mut state_ref.chooser_titles_stale) {
        return;
    }
    let cwd = state_ref.canonical_project_dir.as_str();
    let titles = &state_ref.titles;
    let mut changed = Vec::new();
    for (id, shown) in &mut state_ref.chooser_listed {
        if let Some(title) = titles.known_title(cwd, id) {
            if shown.as_deref() != Some(title.as_str()) {
                *shown = Some(title.clone());
                changed.push((id.clone(), title));
            }
        }
    }
    if changed.is_empty() {
        return;
    }
    out.send(
        state_ref.chooser_view,
        eitri_core::agent_bridge::serialize_chooser_titles_for_js(&changed),
    );
}

/// The chooser's envelope (`prefix w` in `view`'s page): every open tab in number order, a tab bound
/// to an extra view naming it, then every record open in no tab, each marked if another window holds
/// its lease. Also each record listed with the title shown.
pub(super) fn chooser_payload(state: &AgentPanelState, view: ViewId) -> (String, Vec<(String, Option<String>)>) {
    // The records offered are the agent's of the tab the opening page shows, as the resume that
    // follows starts in it.
    let agent = hello_agent(state, view);
    let greeting = BackendGreeting::for_agent_titled(
        state.backend_kind,
        &agent,
        state.project_dir.clone(),
        eitri_core::agent_backend::Titles::Cached(&state.titles),
    );
    let lease_name = agent.record_name();
    let open: Vec<ChooserTab> = state
        .tabs
        .tabs()
        .iter()
        .map(|t| {
            let facts = t.facts();
            ChooserTab {
                tab: t.id,
                label: ChooserTab::label(t.number, &t.label_name(), t.view_id()),
                marker: eitri_core::tabs::marker(facts),
                pending: facts.pending,
                resumable: t.resumable(state.backend_kind),
            }
        })
        .collect();
    let canonical = state.canonical_project_dir.as_str();
    let records = chooser_records(&greeting.resumable, &state.tabs.open_sessions(), |id| {
        agent::lease::SessionLease::is_held(&lease_name, canonical, id).unwrap_or(false)
    });
    let listed = records
        .iter()
        .map(|r| (r.provider_session_id.clone(), r.title.clone()))
        .collect();
    (
        eitri_core::agent_bridge::serialize_chooser_for_js(&open, &records),
        listed,
    )
}

/// The detail popover's envelope for `tab` (`prefix i`, `open_detail{tab}`).
pub(super) fn detail_payload(state: &AgentPanelState, tab: TabId) -> String {
    let rows = match state.tabs.get(tab) {
        Some(t) => detail_rows(
            t,
            state.backend_kind,
            agent::account::configured().map(agent::account::ClaudeAccount::name),
            &state.project_dir,
            state
                .rules_dir
                .as_ref()
                .map(|d| eitri_core::permission_store::path(d, &state.project_dir))
                .as_deref(),
            &trust_row(state, t.agent()),
            super::starts::project_trust_at_root(
                t.agent(),
                state
                    .trust
                    .next_session_trust(&trust_profile(&state.acp_agents, t.agent())),
                &state.project_dir,
            ),
            &state.acp_agents,
            stop_background_keys(state.keymap_help.as_deref()).as_deref(),
        ),
        None => Vec::new(),
    };
    eitri_core::agent_bridge::serialize_tab_detail_for_js(tab, &rows)
}

/// The backend a tab command acts on, or the benign "no active session" refusal (an empty,
/// starting or failed tab). Never another tab's.
pub(super) fn backend_for(
    tabs: &mut eitri_core::tab_set::TabSet,
    tab: TabId,
) -> Result<&mut AgentBackend, eitri_core::agent_backend::BackendError> {
    tabs.get_mut(tab)
        .and_then(|t| t.live_mut())
        .ok_or_else(no_session_error)
}

/// The chooser's records: every resumable session open in no tab, newest first as `hello` ranks
/// them, each marked if another window holds its lease.
pub(super) fn chooser_records(
    sessions: &[agent::ResumableSession],
    open: &[(AgentId, String)],
    held: impl Fn(&str) -> bool,
) -> Vec<ChooserRecord> {
    sessions
        .iter()
        .filter(|s| !eitri_core::tab_restore::is_open(open, s))
        .map(|s| ChooserRecord {
            provider_session_id: s.provider_session_id.clone(),
            name: s.name.clone(),
            title: s.title.clone(),
            created_at: s.created_at.clone(),
            updated_at: s.updated_at.clone(),
            held_elsewhere: held(&s.provider_session_id),
            held_by_partner: false,
            partner_window: None,
        })
        .collect()
}

pub(super) fn mark_partner_records(records: &mut [ChooserRecord], window: Option<u8>, held: impl Fn(&str) -> bool) {
    for record in records {
        record.held_by_partner = window.is_some() && record.held_elsewhere && held(&record.provider_session_id);
        record.partner_window = window.filter(|_| record.held_by_partner);
    }
}

/// `prefix i`'s rows for one tab, in the order they are shown, `—` for anything unknown. `trust_row` is the
/// window's latest trust check in words, and `next` the tiers a session started now would get. A tab
/// that runs another agent than Claude gets that agent's rows (`external_detail_rows`); the Claude rows
/// below are the same for a Claude tab on either backend.
///
/// `provider_session_id()` is read into a local BEFORE `projection()` is taken: on the sidecar path
/// both lock the same ingestion mutex (the 2026-09-15 GTK freeze; `tab_set`'s module doc).
pub(super) fn detail_rows(
    tab: &Tab,
    kind: BackendKind,
    account: Option<&str>,
    project_dir: &Path,
    rules_file: Option<&Path>,
    trust_row: &str,
    next: ProjectTrust,
    registered: &[AgentRegistration],
    stop_keys: Option<&str>,
) -> Vec<DetailRow> {
    if tab.agent().is_external() {
        return external_detail_rows(tab, kind, project_dir, trust_row, next, registered, stop_keys);
    }
    const UNKNOWN: &str = "—";
    let known = |value: Option<String>| value.filter(|v| !v.is_empty()).unwrap_or_else(|| UNKNOWN.to_string());
    let backend = tab.live();
    let provider_session_id = backend.and_then(AgentBackend::provider_session_id);
    let (model, cwd) = match backend {
        Some(b) => {
            let projection = b.projection();
            (projection.model.clone(), projection.cwd.clone())
        }
        None => (None, None),
    };
    let conversation_id = backend.and_then(|b| b.conversation_id().map(str::to_string));
    let session_id = backend.and_then(|b| b.session_id().map(str::to_string));
    let info = backend.and_then(|b| b.provider_info());
    let cli = info.map(|i| {
        let mut line = if i.actual_claude_code_version.is_empty() {
            UNKNOWN.to_string()
        } else {
            i.actual_claude_code_version.clone()
        };
        for diagnostic in &i.startup_diagnostics {
            line.push_str(" · ");
            line.push_str(diagnostic);
        }
        line
    });
    let revision = info.map(|i| {
        format!(
            "{} · expected {}",
            i.build_description.as_deref().unwrap_or(UNKNOWN),
            agent::EXPECTED_VERDANDI_REVISION
        )
    });
    let record = match (&conversation_id, &provider_session_id) {
        (Some(conversation), Some(session)) => agent::persistence::load_conversation_record(conversation, session).ok(),
        _ => None,
    };
    let resumable = if tab.resumable(kind) {
        "yes"
    } else {
        "no (legacy backend)"
    };
    let rows: [(&str, String); 20] = [
        ("name", known(tab.name.clone())),
        ("title", known(tab.session.title.clone())),
        ("state", tab.wire_state().as_str().to_string()),
        ("mode", tab.mode().as_str().to_string()),
        ("model", known(model)),
        ("backend", kind.as_str().to_string()),
        ("account", known(account.map(str::to_string))),
        (
            "cwd",
            known(cwd.or_else(|| Some(project_dir.to_string_lossy().into_owned()))),
        ),
        ("conversation", known(conversation_id)),
        ("verdandi session", known(session_id)),
        ("claude session", known(provider_session_id)),
        ("CLI", known(cli)),
        // A fact about every session on either backend, so it is drawn on an empty tab too: a
        // started tab says what its session was given, an empty one what its next would get.
        (
            "settings",
            agent::setting_sources::note_for_session(
                agent::setting_sources::loads_user_settings(),
                tab.session.project_tiers.unwrap_or(next),
            )
            .to_string(),
        ),
        ("trust", trust_row.to_string()),
        ("Verdandi revision", known(revision)),
        ("resumable", resumable.to_string()),
        (
            "created",
            record_stamp_row(record.as_ref().map(|r| r.created_at.as_str())),
        ),
        (
            "updated",
            record_stamp_row(record.as_ref().map(|r| r.updated_at.as_str())),
        ),
        ("queued", tab.session.queue.len().to_string()),
        ("permission rules", known(rules_file.map(|p| p.display().to_string()))),
    ];
    rows.into_iter()
        .map(|(label, value)| DetailRow {
            label: label.to_string(),
            value,
        })
        .collect()
}

/// The `trust` row of a tab: the answer on record, and whether it counts for this tab at this root.
fn trust_row(state: &AgentPanelState, agent: &AgentId) -> String {
    let row = state.trust.row(&trust_profile(&state.acp_agents, agent));
    let counts = super::starts::project_trust_at_root(agent, ProjectTrust::Trusted, &state.project_dir);
    if counts == ProjectTrust::Trusted {
        row
    } else {
        format!("{row}; not applied here, this folder is no boundary")
    }
}

/// What Codex does without asking, for `prefix i`: the sandbox's reach and every route around it, as
/// they were measured, not softened. `root` is the project the sandbox is rooted at; the second half
/// of the sentence follows the tab's mode: in auto Codex's reviewer decides, in manual every such
/// request is a card, and in bypass Eitri allows it. At a root that is no boundary the sandbox is
/// read-only and auto's requests are cards too.
pub(super) fn codex_runs_without_asking(root: &Path, mode: SessionModeChoice, root_is_a_boundary: bool) -> String {
    let who_decides = match mode {
        SessionModeChoice::Bypass => "Eitri allows anything else",
        SessionModeChoice::Auto if root_is_a_boundary => "its reviewer decides anything else",
        SessionModeChoice::Auto | SessionModeChoice::Manual => "you decide anything else on a card",
    };
    let sandbox = if root_is_a_boundary {
        format!(
            "Codex runs commands and edits inside {} without asking, reads anywhere, no network or sockets",
            root.display()
        )
    } else {
        format!(
            "{} is no boundary, so Codex runs read-only: it reads anywhere and writes nothing, no network or sockets",
            root.display()
        )
    };
    format!(
        "{sandbox}; your exec-policy allow rules run unsandboxed; {who_decides}, which then runs with your full \
         rights; MCP tools follow their servers' approval modes"
    )
}

/// What a mode change does to a Codex tab and when: Eitri's own answers follow it at once, Codex's
/// reviewer only at the next turn. Codex's reviewer is the user's in manual and bypass, its own in auto.
pub(super) const CODEX_MODE_LAG: &str =
    "a mode change reaches Codex's reviewer at the next turn; a running turn keeps the \
     reviewer it started with (a turn started in manual or bypass: Codex asks about everything outside its \
     sandbox for the rest of it, and Eitri draws a card, or accepts in bypass; a turn started in auto: Codex's \
     reviewer still decides for the rest of it, and Eitri draws a card for what still arrives, or accepts it \
     in bypass)";

/// [`CODEX_MODE_LAG`] at a root that is no boundary, where Codex's reviewer is never its own.
pub(super) const CODEX_MODE_LAG_NO_BOUNDARY: &str =
    "none for the reviewer: at this folder it is you in every mode; Eitri's own answers follow the mode at once \
     (it accepts what Codex asks in bypass, and draws a card otherwise)";

/// What an ACP agent runs without a question. Eitri sets only the mode the registration names, once at the
/// start, and does not know what any mode of the agent lets through; without one the agent's own default
/// stays and Eitri cannot say even which mode that is.
pub(super) fn acp_runs_without_asking(auto_mode: Option<&str>) -> String {
    match auto_mode {
        Some(mode) => format!(
            "whatever the agent's mode {mode} lets through, set at the start; Eitri shows every request it receives"
        ),
        None => "Eitri does not know what this agent runs without asking (no auto_mode registered, so its own \
                 default mode); Eitri shows every request it receives"
            .to_string(),
    }
}

/// What a mode change does to an ACP tab: Eitri answers by the tab's mode at once, and the agent's own mode,
/// set once at the start, never changes with it.
pub(super) const ACP_MODE_LAG: &str =
    "none: Eitri answers by the tab's mode at once (entering bypass approves the cards \
     waiting); the agent's own mode is not changed";

/// How full an agent last said its context window is, in words.
pub(super) fn context_row(gauge: agent::external::ContextGauge) -> String {
    if gauge.size == 0 {
        return format!("{} used", gauge.used);
    }
    let percent = gauge.used.saturating_mul(100) / gauge.size;
    format!("{percent}% full ({} of {})", gauge.used, gauge.size)
}

/// The account limit an agent last reported, in words.
pub(super) fn limits_row(limits: &agent::AccountLimitsRecord) -> String {
    let mut line = format!("{:.0}% used", limits.used_percent);
    if let Some(minutes) = limits.window_minutes {
        line.push_str(&format!(" of the {minutes}-minute window"));
    }
    if let Some(resets) = limits.resets_at.and_then(|t| u64::try_from(t).ok()) {
        line.push_str(&format!(
            " · resets {} {:02}:{:02} UTC",
            eitri_core::project_trust::civil_date(resets),
            resets % 86_400 / 3_600,
            resets % 3_600 / 60
        ));
    }
    if let Some(plan) = limits.plan.as_deref().filter(|p| !p.is_empty()) {
        line.push_str(&format!(" · plan {plan}"));
    }
    line
}

/// A `created`/`updated` row of `prefix i`: the record's stamp as local date and time, or a dash for a tab
/// with no record.
pub(super) fn record_stamp_row(raw: Option<&str>) -> String {
    raw.map_or_else(|| "\u{2014}".to_string(), eitri_core::tabs::record_stamp)
}

/// The `settings` row of a Codex tab: Codex's own configuration directory, the one it reported (a
/// `CODEX_HOME` the user set, not always `~/.codex`), and what of the project is loaded. Before a session
/// has reported one it says where Codex looks.
pub(super) fn codex_settings_row(configuration_home: Option<&str>, project_loaded: &str) -> String {
    let whose = match configuration_home {
        Some(home) => format!("your Codex configuration in {home} (always;"),
        None => "your Codex configuration (CODEX_HOME, else ~/.codex; always;".to_string(),
    };
    format!("{whose} skills off while this project is untrusted); this project's: {project_loaded}")
}

/// `prefix i`'s rows for a tab that runs another agent than Claude. The Claude-only rows (Verdandi
/// session, Claude session, CLI, Verdandi revision, permission rules) are not shown: they describe
/// things such a tab does not have.
pub(super) fn external_detail_rows(
    tab: &Tab,
    kind: BackendKind,
    project_dir: &Path,
    trust_row: &str,
    next: ProjectTrust,
    registered: &[AgentRegistration],
    stop_keys: Option<&str>,
) -> Vec<DetailRow> {
    const UNKNOWN: &str = "—";
    let known = |value: Option<String>| value.filter(|v| !v.is_empty()).unwrap_or_else(|| UNKNOWN.to_string());
    let agent = tab.agent();
    let is_codex = matches!(agent, AgentId::Codex);
    let backend = tab.live();
    // As on the Claude rows: the session id is read before the projection is taken.
    let provider_session_id = backend.and_then(|b| b.provider_session_id());
    let (cwd, limits, background) = match backend {
        Some(b) => {
            let projection = b.projection();
            (
                projection.cwd.clone(),
                projection.account_limits.clone(),
                projection.background_calls,
            )
        }
        None => (None, None, 0),
    };
    let conversation_id = backend.and_then(|b| b.conversation_id().map(str::to_string));
    let version = backend.and_then(|b| b.provider_info()).map(|info| {
        let mut line = info.build_description.clone().unwrap_or_else(|| UNKNOWN.to_string());
        for diagnostic in &info.startup_diagnostics {
            line.push_str(" · ");
            line.push_str(diagnostic);
        }
        line
    });
    let configuration_home = backend
        .and_then(|b| b.provider_info())
        .and_then(|info| info.configuration_home.clone());
    let record = match (&conversation_id, &provider_session_id) {
        (Some(conversation), Some(session)) => agent::persistence::load_conversation_record(conversation, session).ok(),
        _ => None,
    };
    let project_loaded = match tab.session.project_tiers.unwrap_or(next) {
        ProjectTrust::Trusted => "loaded",
        ProjectTrust::Untrusted | ProjectTrust::NothingFound => "not loaded",
    };
    let boundary = eitri_core::agent_backend::root_is_a_boundary(project_dir);
    let (backend_name, session_label, settings, runs_without_asking, mode_lag, limits_label, limits_value) = if is_codex
    {
        (
            "codex app-server".to_string(),
            "thread",
            codex_settings_row(configuration_home.as_deref(), project_loaded),
            codex_runs_without_asking(project_dir, tab.mode(), boundary),
            if boundary {
                CODEX_MODE_LAG
            } else {
                CODEX_MODE_LAG_NO_BOUNDARY
            }
            .to_string(),
            "limits",
            limits.as_ref().map(limits_row),
        )
    } else {
        let auto_mode = match agent {
            AgentId::Acp(name) => {
                super::trust::acp_spec(registered, name).and_then(|spec| spec.at_root(boundary).auto_mode)
            }
            _ => None,
        };
        let context = match backend {
            Some(AgentBackend::Conversation { conversation, .. }) => conversation.context_gauge(),
            _ => None,
        };
        (
            "acp".to_string(),
            "session",
            format!("the agent's own configuration (always); this project's: {project_loaded}"),
            acp_runs_without_asking(auto_mode.as_deref()),
            ACP_MODE_LAG.to_string(),
            "context",
            context.map(context_row),
        )
    };
    let rows: [(&str, String); 18] = [
        ("name", known(tab.name.clone())),
        ("title", known(tab.session.title.clone())),
        ("state", tab.wire_state().as_str().to_string()),
        ("mode", tab.mode().as_str().to_string()),
        ("agent", known(version.or_else(|| Some(agent.label())))),
        ("backend", backend_name),
        (
            "cwd",
            known(cwd.or_else(|| Some(project_dir.to_string_lossy().into_owned()))),
        ),
        ("conversation", known(conversation_id)),
        (session_label, known(provider_session_id)),
        ("settings", settings),
        ("trust", trust_row.to_string()),
        ("runs without asking", runs_without_asking),
        ("mode lag", mode_lag),
        (limits_label, known(limits_value)),
        ("resumable", if tab.resumable(kind) { "yes" } else { "no" }.to_string()),
        (
            "created",
            record_stamp_row(record.as_ref().map(|r| r.created_at.as_str())),
        ),
        (
            "updated",
            record_stamp_row(record.as_ref().map(|r| r.updated_at.as_str())),
        ),
        ("queued", tab.session.queue.len().to_string()),
    ];
    let mut rows: Vec<DetailRow> = rows
        .into_iter()
        .map(|(label, value)| DetailRow {
            label: label.to_string(),
            value,
        })
        .collect();
    // Only Codex counts what it leaves running after a turn, and only its tab can stop it.
    if is_codex {
        rows.push(DetailRow {
            label: "background".to_string(),
            value: background_row(background, stop_keys),
        });
    }
    rows
}

/// The `background` row of `prefix i`: how many commands the agent left running after its turns ended,
/// and the key that stops them (`stop_keys`, as the user presses it; `None` when no key is bound).
pub(super) fn background_row(running: u32, stop_keys: Option<&str>) -> String {
    if running == 0 {
        return "none".to_string();
    }
    let them = if running == 1 { "it" } else { "them" };
    match stop_keys {
        Some(keys) => format!("{running} running ({keys} stops {them} with the session)"),
        None => {
            format!("{running} running (no key is bound to tab.stop-background, which stops {them} with the session)")
        }
    }
}

/// How the user presses `tab.stop-background`, read from the panel table the window last sent (the
/// leader by its own name): the default `Space k` before any was sent, `None` when no key is bound.
pub(super) fn stop_background_keys(keymap_help: Option<&str>) -> Option<String> {
    let Some(help) = keymap_help else {
        return Some("Space k".to_string());
    };
    let help: serde_json::Value = serde_json::from_str(help).ok()?;
    let panel = &help["panel"];
    let leader = panel["leaderLabel"].as_str().unwrap_or("Space");
    let binding = panel["bindings"]
        .as_array()?
        .iter()
        .find(|b| b["action"] == "tab.stop-background")?;
    let keys: Vec<String> = binding["keys"]
        .as_array()?
        .iter()
        .filter_map(|k| k.as_str())
        .map(|k| match k {
            "<leader>" => leader.to_string(),
            " " => "Space".to_string(),
            other => other.to_string(),
        })
        .collect();
    (!keys.is_empty()).then(|| keys.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_panel::tests::*;

    #[test]
    fn the_detail_popover_counts_the_queue_and_names_the_rules_file() {
        let mut set = TabSet::new(BackendKind::Legacy, eitri_core::agent_bridge::SessionModeChoice::Auto);
        let tab = set.active();
        set.get_mut(tab)
            .unwrap()
            .session
            .queue
            .push(eitri_core::tab_set::Queued::held("a", "a".into(), 0));
        let rules = std::path::Path::new("/s/eitri/permissions/0123456789abcdef.json");
        let rows = detail_rows(
            set.active_tab(),
            BackendKind::Legacy,
            None,
            std::path::Path::new("/p"),
            Some(rules),
            "not checked yet",
            ProjectTrust::Untrusted,
            &[],
            Some("Space k"),
        );
        let value = |label: &str| rows.iter().find(|r| r.label == label).unwrap().value.clone();
        assert_eq!(value("queued"), "1");
        assert_eq!(value("permission rules"), rules.display().to_string());
        let rows = detail_rows(
            set.active_tab(),
            BackendKind::Legacy,
            None,
            std::path::Path::new("/p"),
            None,
            "not checked yet",
            ProjectTrust::Untrusted,
            &[],
            Some("Space k"),
        );
        assert_eq!(rows.iter().find(|r| r.label == "permission rules").unwrap().value, "—");
    }

    #[test]
    fn the_chooser_names_a_session_held_by_the_other_window() {
        let sessions = vec![agent::ResumableSession {
            provider: "claude".into(),
            provider_session_id: "peer".into(),
            name: None,
            title: Some("<b>work</b>".into()),
            created_at: "1".into(),
            updated_at: "2".into(),
        }];
        let mut records = chooser_records(&sessions, &[], |_| true);
        mark_partner_records(&mut records, Some(2), |id| id == "peer");
        assert!(records[0].held_by_partner);
        let payload = eitri_core::agent_bridge::serialize_chooser_for_js(&[], &records);
        let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(payload["records"][0]["heldByPartner"], true);
        assert_eq!(payload["records"][0]["partnerWindow"], 2);
        assert_eq!(payload["records"][0]["title"], "<b>work</b>");
        mark_partner_records(&mut records, None, |_| true);
        assert!(!records[0].held_by_partner);
    }

    #[test]
    fn the_chooser_lists_records_open_in_no_tab_and_marks_those_held_elsewhere() {
        let sessions: Vec<agent::ResumableSession> = ["open-here", "held", "free"]
            .iter()
            .map(|id| agent::ResumableSession {
                provider: "claude".into(),
                provider_session_id: id.to_string(),
                created_at: "1".into(),
                updated_at: "2".into(),
                title: Some(format!("about {id}")),
                name: None,
            })
            .collect();
        let records = chooser_records(&sessions, &[(AgentId::Claude, "open-here".to_string())], |id| {
            id == "held"
        });
        let ids: Vec<(&str, bool)> = records
            .iter()
            .map(|r| (r.provider_session_id.as_str(), r.held_elsewhere))
            .collect();
        assert_eq!(ids, vec![("held", true), ("free", false)]);
    }

    #[test]
    fn the_detail_popover_names_every_identity_and_the_account() {
        let set = TabSet::new(BackendKind::Legacy, eitri_core::agent_bridge::SessionModeChoice::Bypass);
        let rows = detail_rows(
            set.active_tab(),
            BackendKind::Legacy,
            Some("work"),
            std::path::Path::new("/p"),
            None,
            "not checked yet",
            ProjectTrust::Untrusted,
            &[],
            Some("Space k"),
        );
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        for want in [
            "name",
            "title",
            "state",
            "mode",
            "model",
            "backend",
            "account",
            "cwd",
            "conversation",
            "verdandi session",
            "claude session",
            "CLI",
            "settings",
            "Verdandi revision",
            "resumable",
            "created",
            "updated",
            "queued",
            "permission rules",
        ] {
            assert!(labels.contains(&want), "missing {want}: {labels:?}");
        }
        let resumable = rows.iter().find(|r| r.label == "resumable").unwrap();
        assert_eq!(resumable.value, "no (legacy backend)");
        let account = rows.iter().find(|r| r.label == "account").unwrap();
        assert_eq!(account.value, "work");

        // What every session loads. The value is the sentence `build_create_request` and the legacy
        // spawn are held to, not a copy of it, and the row sits right after the CLI it describes.
        let settings = rows.iter().find(|r| r.label == "settings").unwrap();
        assert_eq!(
            settings.value,
            agent::setting_sources::note_for_session(true, ProjectTrust::Untrusted)
        );
        let position = |label: &str| labels.iter().position(|l| *l == label).unwrap();
        assert_eq!(position("settings"), position("CLI") + 1, "{labels:?}");
    }

    /// The `settings` row is a fact about every session, so it does not depend on there being one:
    /// an empty tab (no backend, no provider info) says it too, and on either backend.
    #[test]
    fn the_settings_row_is_the_same_on_an_empty_tab_and_on_either_backend() {
        for kind in [BackendKind::Legacy, BackendKind::Sidecar] {
            let set = TabSet::new(kind, eitri_core::agent_bridge::SessionModeChoice::Auto);
            let rows = detail_rows(
                set.active_tab(),
                kind,
                None,
                std::path::Path::new("/p"),
                None,
                "not checked yet",
                ProjectTrust::Untrusted,
                &[],
                Some("Space k"),
            );
            let settings = rows
                .iter()
                .find(|r| r.label == "settings")
                .expect("the row is always drawn");
            assert_eq!(
                settings.value,
                agent::setting_sources::note_for_session(true, ProjectTrust::Untrusted),
                "{}",
                kind.as_str()
            );
        }
    }

    /// Points `CLAUDE_CONFIG_DIR` at one disposable root for this whole test binary, and returns it.
    ///
    /// A process-wide variable set from a multi-threaded test binary, which this project normally
    /// refuses (`agent::state_dirs`'s own module doc says so). It is set exactly ONCE, to a value that
    /// never changes, from inside a `OnceLock` initializer, so no thread can observe it being two different
    /// things; `eitri-core`'s test binary does the same for the same reason. Nothing else in this binary
    /// reads a transcript, so any other test that reaches a transcript path finds an empty directory,
    /// which is what it would have found anyway. The root lives under the agent crate's test state root,
    /// which the next run prunes once this process is gone, so nothing is left behind for good.
    fn a_claude_config_root() -> PathBuf {
        static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        ROOT.get_or_init(|| {
            let root = agent::state_dirs::redirect_state_to_a_test_root().join("claude-config");
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            std::env::set_var("CLAUDE_CONFIG_DIR", &root);
            root
        })
        .clone()
    }

    /// `hello` names a row by its record's own title at once, and the CLI's own title, read on the
    /// title worker, arrives with a wake and a second `hello`; the window never read the transcript.
    #[test]
    fn the_clis_title_reaches_hello_after_the_worker_read_it() {
        a_claude_config_root();
        let (state, dir) = restore_panel("titles-later", &[], &["s-titled"]);
        let transcript = agent::transcript::transcript_path(&dir.to_string_lossy(), "s-titled").unwrap();
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, "{\"type\":\"ai-title\",\"aiTitle\":\"the CLI's title\"}\n").unwrap();
        let page = Rc::new(RecordingPage::default());
        page.visible.set(true);
        let handle = AgentPanelHandle::over(
            state.clone(),
            Some(Rc::new(PanelPage::new(page.clone() as Rc<dyn PageSurface>))),
        );
        let titles = |page: &RecordingPage| -> Vec<String> {
            page.sent
                .borrow()
                .iter()
                .filter(|p| kind_of(p) == "hello")
                .map(|p| {
                    let hello: serde_json::Value = serde_json::from_str(p).unwrap();
                    hello["resumableSessions"][0]["title"].as_str().unwrap_or("").to_owned()
                })
                .collect()
        };
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r1"}"#);
        assert_eq!(
            titles(&page),
            vec!["title of s-titled".to_string()],
            "the record's own, at once"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while titles(&page).len() < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "no second hello: {:?}",
                titles(&page)
            );
            if let Some(due) = handle.take_wake(std::time::Instant::now()) {
                handle.run_tick(due);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(titles(&page)[1], "the CLI's title");
        let _ = std::fs::remove_file(transcript);
    }

    /// A chooser sent before the CLI's title of one of its records was read gets that title once the worker
    /// has read it: a `chooser_titles` envelope naming the record, and nothing more for it afterwards.
    #[test]
    fn a_title_read_after_the_chooser_opened_is_sent_to_it() {
        a_claude_config_root();
        let (state, dir) = restore_panel("titles-chooser", &[], &["s-listed"]);
        let transcript = agent::transcript::transcript_path(&dir.to_string_lossy(), "s-listed").unwrap();
        let _ = std::fs::remove_file(&transcript);
        let page = Rc::new(RecordingPage::default());
        page.visible.set(true);
        let handle = AgentPanelHandle::over(
            state.clone(),
            Some(Rc::new(PanelPage::new(page.clone() as Rc<dyn PageSurface>))),
        );
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r1"}"#);
        // The greeting's read finds no transcript yet; the title is written after.
        assert!(state.borrow().titles.wait_read(std::time::Duration::from_secs(5)));
        let tick_pending = |handle: &AgentPanelHandle| {
            if let Some(due) = handle.take_wake(std::time::Instant::now()) {
                handle.run_tick(due);
            }
        };
        tick_pending(&handle);
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(
            &transcript,
            "{\"type\":\"ai-title\",\"aiTitle\":\"the CLI's late title\"}\n",
        )
        .unwrap();
        handle.open_chooser(ViewId::BASE);
        let chooser = page
            .sent
            .borrow()
            .iter()
            .rfind(|p| kind_of(p) == "chooser")
            .cloned()
            .expect("the chooser was sent");
        assert!(
            chooser.contains("title of s-listed"),
            "the record's own title: {chooser}"
        );
        let retitled = |page: &RecordingPage| {
            page.sent
                .borrow()
                .iter()
                .filter(|p| kind_of(p) == "chooser_titles")
                .cloned()
                .collect::<Vec<_>>()
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while retitled(&page).is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "the chooser was never told the title"
            );
            tick_pending(&handle);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let sent: serde_json::Value = serde_json::from_str(&retitled(&page)[0]).unwrap();
        assert_eq!(
            sent["titles"],
            serde_json::json!([{ "providerSessionId": "s-listed", "title": "the CLI's late title" }])
        );
        // Told once: a later title read that changes nothing for it sends nothing more.
        handle.state.borrow_mut().chooser_titles_stale = true;
        handle.run_tick(std::time::Instant::now() + std::time::Duration::from_secs(1));
        assert_eq!(retitled(&page).len(), 1);
        let _ = std::fs::remove_file(transcript);
    }

    /// A tab restored with its record's title (the CLI's not read yet at launch) takes the CLI's title once
    /// the worker has read it, and the `tabs` envelope carries it.
    #[test]
    fn a_restored_tab_takes_the_clis_title_when_it_is_read() {
        a_claude_config_root();
        let (state, dir) = restore_panel("titles-tab", &[], &["s-tabbed"]);
        let transcript = agent::transcript::transcript_path(&dir.to_string_lossy(), "s-tabbed").unwrap();
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, "{\"type\":\"ai-title\",\"aiTitle\":\"the CLI's own\"}\n").unwrap();
        let tab = state.borrow().tabs.active();
        let (provider, backend) = live_backend(&dir);
        {
            let mut s = state.borrow_mut();
            let t = s.tabs.get_mut(tab).unwrap();
            t.set_backend(TabBackend::Live(backend));
            t.session.title = Some("title of s-tabbed".into());
            t.session.title_from_record = true;
        }
        let page = Rc::new(RecordingPage::default());
        page.visible.set(true);
        let handle = AgentPanelHandle::over(
            state.clone(),
            Some(Rc::new(PanelPage::new(page.clone() as Rc<dyn PageSurface>))),
        );
        // The greeting asks the worker to read the session's transcript, which may land before the session
        // has opened, as at a restore.
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r1"}"#);
        assert!(state.borrow().titles.wait_read(std::time::Duration::from_secs(5)));
        provider.open_session("s-tabbed", &dir);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while state.borrow().tabs.get(tab).unwrap().session.title.as_deref() != Some("the CLI's own") {
            assert!(std::time::Instant::now() < deadline, "the tab kept the record's title");
            if let Some(due) = handle.take_wake(std::time::Instant::now()) {
                handle.run_tick(due);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            page.sent
                .borrow()
                .iter()
                .any(|p| kind_of(p) == "tabs" && p.contains("the CLI's own")),
            "the tabs envelope carries it"
        );
        let _ = std::fs::remove_file(transcript);
        for t in state.borrow_mut().tabs.take_all() {
            if let Some(retired) = t.retired {
                retired.into_backend().shutdown();
            }
        }
    }

    /// `settings` says what the tab's own session was given once it has one, and what the next would
    /// get before; `trust` (the window's latest check) sits right after it.
    #[test]
    fn the_settings_and_trust_rows_are_per_tab() {
        let user = agent::setting_sources::loads_user_settings();
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let rows_for = |set: &TabSet, next: ProjectTrust| {
            detail_rows(
                set.active_tab(),
                BackendKind::Sidecar,
                None,
                std::path::Path::new("/p"),
                None,
                "trusted since 2026-10-02",
                next,
                &[],
                Some("Space k"),
            )
        };
        let value = |rows: &[DetailRow], label: &str| rows.iter().find(|r| r.label == label).unwrap().value.clone();

        let rows = rows_for(&set, ProjectTrust::Trusted);
        assert_eq!(
            value(&rows, "settings"),
            agent::setting_sources::note_for_session(user, ProjectTrust::Trusted),
            "an empty tab: what its next session would get"
        );
        assert_eq!(value(&rows, "trust"), "trusted since 2026-10-02");
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        let position = |label: &str| labels.iter().position(|l| *l == label).unwrap();
        assert_eq!(position("settings"), position("CLI") + 1, "{labels:?}");
        assert_eq!(position("trust"), position("settings") + 1, "{labels:?}");
        assert_eq!(rows.len(), 20);

        set.active_tab_mut().session.project_tiers = Some(ProjectTrust::Untrusted);
        let rows = rows_for(&set, ProjectTrust::Trusted);
        assert_eq!(
            value(&rows, "settings"),
            agent::setting_sources::note_for_session(user, ProjectTrust::Untrusted),
            "a started tab keeps what its session was given"
        );
    }

    /// The rows of a Codex tab at a root that exists and only this account can write: a boundary.
    fn codex_rows(mode: SessionModeChoice, next: ProjectTrust) -> Vec<DetailRow> {
        codex_rows_at(mode, next, private_root())
    }

    /// A folder of this test run's, under a private parent; never removed, as the tests share it.
    fn private_root() -> &'static std::path::Path {
        static ROOT: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
        ROOT.get_or_init(|| agent::state_dirs::test_workspace_dir("detail-root"))
    }

    fn codex_rows_at(mode: SessionModeChoice, next: ProjectTrust, root: &std::path::Path) -> Vec<DetailRow> {
        // A window never starts in bypass; only the user's own `agent.default_mode` puts new tabs there.
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        set.apply_configured_default(mode);
        let tab = set.open(AgentId::Codex);
        detail_rows(
            set.get(tab).unwrap(),
            BackendKind::Sidecar,
            Some("work"),
            root,
            Some(std::path::Path::new("/s/eitri/permissions/x.json")),
            "trusted since 2026-10-02",
            next,
            &[],
            Some("Space k"),
        )
    }

    /// A root that cannot be resolved is no boundary, so what `prefix i` says of it is what the sandbox does there:
    /// nothing written, and a person deciding in every mode but bypass.
    #[test]
    fn detail_rows_for_a_codex_tab_at_a_root_that_is_no_boundary_say_it_is_read_only() {
        let root = std::path::Path::new("/nonexistent/eitri-detail-root");
        let row = |rows: &[DetailRow], label: &str| rows.iter().find(|r| r.label == label).unwrap().value.clone();
        let auto = codex_rows_at(SessionModeChoice::Auto, ProjectTrust::Untrusted, root);
        assert_eq!(
            row(&auto, "runs without asking"),
            "/nonexistent/eitri-detail-root is no boundary, so Codex runs read-only: it reads anywhere and writes \
             nothing, no network or sockets; your exec-policy allow rules run unsandboxed; you decide anything else \
             on a card, which then runs with your full rights; MCP tools follow their servers' approval modes"
        );
        assert_eq!(row(&auto, "mode lag"), CODEX_MODE_LAG_NO_BOUNDARY);
        let manual = codex_rows_at(SessionModeChoice::Manual, ProjectTrust::Untrusted, root);
        assert!(row(&manual, "runs without asking").contains("you decide anything else on a card"));
        let bypass = codex_rows_at(SessionModeChoice::Bypass, ProjectTrust::Untrusted, root);
        assert!(row(&bypass, "runs without asking").contains("Eitri allows anything else"));
    }

    /// A Codex tab says, in `prefix i`, what its sandbox lets through without a question and how a mode
    /// change reaches its reviewer; and shows none of the rows that describe Claude's machinery.
    #[test]
    fn detail_rows_for_a_codex_tab_state_what_runs_without_asking_and_the_mode_lag() {
        let rows = codex_rows(SessionModeChoice::Auto, ProjectTrust::Untrusted);
        let value = |label: &str| rows.iter().find(|r| r.label == label).unwrap().value.clone();
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "name",
                "title",
                "state",
                "mode",
                "agent",
                "backend",
                "cwd",
                "conversation",
                "thread",
                "settings",
                "trust",
                "runs without asking",
                "mode lag",
                "limits",
                "resumable",
                "created",
                "updated",
                "queued",
                "background"
            ]
        );
        for claude_only in [
            "verdandi session",
            "claude session",
            "CLI",
            "Verdandi revision",
            "permission rules",
            "account",
            "model",
        ] {
            assert!(
                !labels.contains(&claude_only),
                "{claude_only} is a Claude row: {labels:?}"
            );
        }
        assert_eq!(value("backend"), "codex app-server");
        assert_eq!(value("agent"), "Codex", "no version until a session reports one");
        assert_eq!(value("trust"), "trusted since 2026-10-02");
        assert_eq!(
            value("settings"),
            "your Codex configuration (CODEX_HOME, else ~/.codex; always; skills off while this project is untrusted); \
             this project's: not loaded"
        );
        assert_eq!(
            value("runs without asking"),
            format!(
                "Codex runs commands and edits inside {} without asking, reads anywhere, no network or sockets; your \
                 exec-policy allow rules run unsandboxed; its reviewer decides anything else, which then runs with \
                 your full rights; MCP tools follow their servers' approval modes",
                private_root().display()
            )
        );
        assert!(value("mode lag").contains("next turn"), "{}", value("mode lag"));
        assert!(value("mode lag").contains("keeps the reviewer it started with"));
        assert_eq!(value("limits"), "—");
        assert_eq!(value("resumable"), "yes");
        assert_eq!(value("background"), "none");

        // Bypass keeps the sandbox and answers for the reviewer; the project's tier follows the answer.
        let rows = codex_rows(SessionModeChoice::Bypass, ProjectTrust::Trusted);
        let value = |label: &str| rows.iter().find(|r| r.label == label).unwrap().value.clone();
        assert!(value("runs without asking").contains("; Eitri allows anything else, which then runs"));
        assert!(!value("runs without asking").contains("reviewer"));
        assert!(value("settings").ends_with("this project's: loaded"));

        // Manual hands every review to the user: a card, never Codex's reviewer.
        let rows = codex_rows(SessionModeChoice::Manual, ProjectTrust::Trusted);
        let value = |label: &str| rows.iter().find(|r| r.label == label).unwrap().value.clone();
        assert_eq!(value("mode"), "manual");
        assert!(value("runs without asking").contains("; you decide anything else on a card, which then runs"));
        assert!(!value("runs without asking").contains("reviewer"));
        assert!(value("mode lag").contains("a turn started in manual or bypass"));
    }

    /// An ACP tab says what Eitri knows of what the agent runs unasked (only the mode it registered, or
    /// nothing at all without one), that a mode change applies at once, and how full its context is.
    #[test]
    fn the_settings_row_names_the_directory_codex_reported_and_the_stamps_read_as_dates() {
        assert_eq!(
            codex_settings_row(Some("/home/user/.config/codex"), "loaded"),
            "your Codex configuration in /home/user/.config/codex (always; skills off while this project is \
             untrusted); this project's: loaded"
        );
        assert!(
            codex_settings_row(None, "not loaded").starts_with("your Codex configuration (CODEX_HOME, else ~/.codex;")
        );
        assert_eq!(record_stamp_row(None), "\u{2014}");
        let shown = record_stamp_row(Some("1791522750000"));
        assert!(shown.starts_with("2026-10-0") && shown.len() == 16, "{shown}");
        assert!(!shown.contains("1791522750000"));
    }

    /// The mode a session at this root was set to, which is not always the one named `auto_mode`.
    #[test]
    fn the_row_of_an_acp_tab_names_the_mode_its_root_called_for() {
        let row_at = |root: &std::path::Path, auto_mode: Option<&str>| {
            let registered = [AgentRegistration {
                name: "gem".into(),
                command: vec!["gem-acp".into()],
                project_files: vec![".gem/".into()],
                auto_mode: auto_mode.map(str::to_string),
                read_only_mode: Some("plan".into()),
            }];
            let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
            let tab = set.open(AgentId::Acp("gem".into()));
            let rows = detail_rows(
                set.get(tab).unwrap(),
                BackendKind::Sidecar,
                None,
                root,
                None,
                "not checked yet",
                ProjectTrust::Untrusted,
                &registered,
                Some("Space k"),
            );
            rows.iter()
                .find(|r| r.label == "runs without asking")
                .unwrap()
                .value
                .clone()
        };
        let no_boundary = std::path::Path::new("/nonexistent/eitri-detail-root");
        assert!(row_at(no_boundary, Some("auto")).contains("mode plan"));
        assert!(row_at(no_boundary, None).contains("mode plan"));
        assert!(row_at(private_root(), Some("auto")).contains("mode auto"));
    }

    #[test]
    fn detail_rows_for_an_acp_tab_follow_its_registration() {
        let registration = |auto_mode: Option<&str>| AgentRegistration {
            name: "gem".into(),
            command: vec!["gem-acp".into()],
            project_files: vec![".gem/".into()],
            auto_mode: auto_mode.map(str::to_string),
            read_only_mode: None,
        };
        let rows_for = |registered: &[AgentRegistration]| {
            let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
            let tab = set.open(AgentId::Acp("gem".into()));
            detail_rows(
                set.get(tab).unwrap(),
                BackendKind::Sidecar,
                None,
                std::path::Path::new("/p/root"),
                None,
                "not checked yet",
                ProjectTrust::Untrusted,
                registered,
                Some("Space k"),
            )
        };
        let value = |rows: &[DetailRow], label: &str| rows.iter().find(|r| r.label == label).unwrap().value.clone();

        let rows = rows_for(&[registration(None)]);
        assert_eq!(rows.len(), 18);
        assert_eq!(value(&rows, "backend"), "acp");
        assert!(
            value(&rows, "runs without asking").starts_with("Eitri does not know what this agent runs without asking"),
            "{}",
            value(&rows, "runs without asking")
        );
        let lag = value(&rows, "mode lag");
        assert!(
            lag.starts_with("none: Eitri answers by the tab's mode at once"),
            "{lag}"
        );
        assert!(!lag.contains("next turn"), "{lag}");
        assert_eq!(value(&rows, "context"), "—", "nothing reported before a session");

        let rows = rows_for(&[registration(Some("auto_edit"))]);
        let runs = value(&rows, "runs without asking");
        assert!(runs.contains("auto_edit"), "{runs}");
        assert!(!runs.contains("does not know what this agent"), "{runs}");

        // An agent init.lua no longer registers is one Eitri knows nothing about.
        let rows = rows_for(&[]);
        assert!(value(&rows, "runs without asking").starts_with("Eitri does not know"));
    }

    /// The `background` row says how many commands a Codex tab's agent left running and the key that
    /// stops them, as the user presses it: the leader by its own name, a rebinding followed, a deleted
    /// binding said.
    #[test]
    fn the_background_row_counts_and_names_the_key_that_stops_them() {
        assert_eq!(background_row(0, Some("Space k")), "none");
        assert_eq!(
            background_row(1, Some("Space k")),
            "1 running (Space k stops it with the session)"
        );
        assert_eq!(
            background_row(2, Some(", k")),
            "2 running (, k stops them with the session)"
        );
        assert!(background_row(3, None).contains("no key is bound to tab.stop-background"));

        assert_eq!(stop_background_keys(None).as_deref(), Some("Space k"));
        let help = |leader: &str, bindings: serde_json::Value| {
            serde_json::json!({"kind": "keymap", "panel": {"leaderLabel": leader, "bindings": bindings}}).to_string()
        };
        let bound = help(
            ",",
            serde_json::json!([{"keys": ["<leader>", "m"], "action": "mode.cycle"},
                               {"keys": ["<leader>", "x", "k"], "action": "tab.stop-background"}]),
        );
        assert_eq!(stop_background_keys(Some(&bound)).as_deref(), Some(", x k"));
        let unbound = help(
            "Space",
            serde_json::json!([{"keys": ["<leader>", "m"], "action": "mode.cycle"}]),
        );
        assert_eq!(stop_background_keys(Some(&unbound)), None);
    }

    /// A Codex tab's `background` row reads the count its agent last reported.
    #[test]
    fn a_codex_tab_background_row_reads_its_count() {
        use eitri_core::test_providers::{empty_trust_plan, ExternalRecordingProvider};
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("panel-background-row");
        let mut set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let tab = set.open(AgentId::Codex);
        let provider = std::sync::Arc::new(ExternalRecordingProvider::codex());
        set.get_mut(tab).unwrap().set_backend(TabBackend::Live(
            AgentBackend::start_external_with(
                AgentId::Codex,
                provider.clone(),
                &dir,
                empty_trust_plan(),
                SessionModeChoice::Auto,
                None,
            )
            .unwrap(),
        ));
        provider
            .inner
            .queue(agent::AgentDomainEvent::BackgroundCalls { running: 2 });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while set.get(tab).unwrap().live().unwrap().projection().background_calls != 2 {
            assert!(std::time::Instant::now() < deadline, "the count never arrived");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let rows = detail_rows(
            set.get(tab).unwrap(),
            BackendKind::Sidecar,
            None,
            &dir,
            None,
            "not checked yet",
            ProjectTrust::Untrusted,
            &[],
            Some("Space k"),
        );
        let row = rows.iter().find(|r| r.label == "background").unwrap();
        assert_eq!(row.value, "2 running (Space k stops them with the session)");
    }

    #[test]
    fn the_context_row_is_a_gauge_in_words() {
        let gauge = |used, size| agent::external::ContextGauge { used, size };
        assert_eq!(context_row(gauge(50_000, 200_000)), "25% full (50000 of 200000)");
        assert_eq!(context_row(gauge(7, 0)), "7 used");
    }

    /// The limit an agent reported is in the `limits` row of its tab, in words.
    #[test]
    fn the_limits_row_names_the_window_and_the_reset() {
        let limits = agent::AccountLimitsRecord {
            used_percent: 42.5,
            window_minutes: Some(300),
            resets_at: Some(1_790_000_000),
            plan: Some("pro".into()),
        };
        assert_eq!(
            limits_row(&limits),
            "42% used of the 300-minute window · resets 2026-09-21 14:13 UTC · plan pro"
        );
        let bare = agent::AccountLimitsRecord {
            used_percent: 3.0,
            window_minutes: None,
            resets_at: None,
            plan: None,
        };
        assert_eq!(limits_row(&bare), "3% used");
    }

    /// A Claude tab's rows are the same twenty, in the same order, whatever else the window can run.
    #[test]
    fn claude_detail_rows_unchanged() {
        let set = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let rows = detail_rows(
            set.active_tab(),
            BackendKind::Sidecar,
            Some("work"),
            std::path::Path::new("/p"),
            Some(std::path::Path::new("/s/rules.json")),
            "not checked yet",
            ProjectTrust::Untrusted,
            &[],
            Some("Space k"),
        );
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "name",
                "title",
                "state",
                "mode",
                "model",
                "backend",
                "account",
                "cwd",
                "conversation",
                "verdandi session",
                "claude session",
                "CLI",
                "settings",
                "trust",
                "Verdandi revision",
                "resumable",
                "created",
                "updated",
                "queued",
                "permission rules"
            ]
        );
        let value = |label: &str| rows.iter().find(|r| r.label == label).unwrap().value.clone();
        assert_eq!(value("backend"), "sidecar");
        assert_eq!(value("resumable"), "yes");
        assert_eq!(value("permission rules"), "/s/rules.json");
    }

    #[test]
    fn the_chooser_lists_every_tab_and_names_a_bound_tabs_view() {
        let (handle, base, second) = handle_with_two_pages();
        handle.state.borrow_mut().tabs.open(AgentId::Claude);
        let bound = handle.break_out(view(2)).unwrap();
        let other = handle.state.borrow_mut().tabs.open(AgentId::Claude);
        let every: Vec<u64> = handle.state.borrow().tabs.tabs().iter().map(|t| t.id.0).collect();
        for (shown_in, page) in [(ViewId::BASE, &base), (view(2), &second)] {
            clear(&[page]);
            handle.open_chooser(shown_in);
            let sent = page.sent.borrow();
            let chooser = sent
                .iter()
                .map(|e| json_of(e))
                .find(|v| v["kind"] == "chooser")
                .expect("the chooser, in the page it was opened in");
            let rows = chooser["open"].as_array().unwrap();
            let listed: Vec<u64> = rows.iter().map(|r| r["tab"].as_u64().unwrap()).collect();
            assert_eq!(listed, every, "every tab, from any page");
            for row in rows {
                let label = row["label"].as_str().unwrap();
                assert_eq!(label.ends_with(" · agent 2"), row["tab"] == bound.0, "{label}");
            }
        }
        assert!(handle.state.borrow().tabs.get(other).is_some());
    }

    #[test]
    fn a_chooser_pick_of_a_bound_tab_asks_the_window_to_show_its_view() {
        let (handle, base, _second) = handle_with_two_pages();
        let first = handle.state.borrow().tabs.active();
        handle.state.borrow_mut().tabs.open(AgentId::Claude);
        let bound = handle.break_out(view(2)).unwrap();
        let shown = Rc::new(RefCell::new(Vec::<ViewId>::new()));
        handle.on_show_view({
            let shown = shown.clone();
            move |view| shown.borrow_mut().push(view)
        });
        clear(&[&base]);
        handle.page_message(
            ViewId::BASE,
            &format!(r#"{{"type":"select_tab","request_id":"p1","tab":{}}}"#, bound.0),
        );
        assert_eq!(*shown.borrow(), vec![view(2)]);
        assert_eq!(
            handle.state.borrow().tabs.active(),
            first,
            "a bound tab is never selected"
        );
        assert_eq!(result_for(&base.sent.borrow(), "p1"), Some(Ok(())));
        let number = handle.state.borrow().tabs.get(bound).unwrap().number;
        assert_eq!(handle.select_number(number), Some(TabPlace::View(view(2), bound)));
        assert_eq!(*shown.borrow(), vec![view(2), view(2)]);
    }

    // ---- Answers per page: each from the page that shows its tab, through the window's gate. ----
}
