//! One display-only archive page per tab. Disk reads and wire conversion run on the worker.
use super::*;
use crate::agent_bridge::{HistoryAction, SnapshotView};
use agent::history::archive::{ArchivedRecord, Page};
use agent::ingestion::ReloadTicket;

#[derive(Default)]
pub(super) struct HistoryReload {
    generation: u64,
    job: Option<Job>,
    worker: Option<std::thread::JoinHandle<()>>,
    held: Option<(ReloadTicket, String, u64)>,
    cleared: bool,
    released: Option<u64>,
    #[cfg(test)]
    before_read: Option<Box<dyn FnOnce() + Send>>,
}

struct Job {
    generation: u64,
    request_id: String,
    ticket: ReloadTicket,
    cancel: Arc<AtomicBool>,
    rx: mpsc::Receiver<Reply>,
}

struct Reply {
    generation: u64,
    request_id: String,
    base: u64,
    page: Option<(u64, String)>,
}

impl Drop for Job {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

fn empty(tab: TabId, request_id: &str) -> String {
    serde_json::json!({"kind":"history_page", "tab":tab.0, "requestId":request_id, "empty":true}).to_string()
}

fn cleared(tab: TabId) -> String {
    serde_json::json!({"kind":"history_page", "tab":tab.0, "cleared":true}).to_string()
}

// Use the snapshot's conversions for every record type, without feeding any of them to the live projection.
fn envelope(tab: TabId, request_id: &str, page: Page) -> String {
    let to_turn = page.from_turn + page.turns.len() as u64;
    let records: Vec<_> = page
        .turns
        .into_iter()
        .map(|turn| {
            let mut p = agent::AgentSessionProjection::default();
            for record in turn.records {
                match record {
                    ArchivedRecord::UserPrompt(r) => p.user_prompts.push(r),
                    ArchivedRecord::Transcript(r) => p.transcript.push(r),
                    ArchivedRecord::Thinking(r) => p.thinking.push(r),
                    ArchivedRecord::ToolCall(r) => p.tool_calls.push(r),
                    ArchivedRecord::TurnStart(r) => p.turn_starts.push(r),
                    ArchivedRecord::TurnEnding(r) => p.turn_endings.push(r),
                    ArchivedRecord::Notice(r) => p.notices.push(r),
                    ArchivedRecord::Question(r) => p.questions.push(r),
                    ArchivedRecord::Plan(r) => p.plans.push(r),
                }
            }
            let view = SnapshotView {
                backend: "history",
                conversation_id: None,
                session_id: None,
                provider_session_id: None,
                capabilities: Default::default(),
                provider: None,
                projection: crate::agent_backend::ProjectionRef::Borrowed(&p),
                hidden_pending: None,
            };
            let wire = crate::agent_bridge::serialize_snapshot_for_js(tab, &view, None);
            let mut wire: serde_json::Value = serde_json::from_str(&wire).expect("serialized snapshot");
            wire["state"].take()
        })
        .collect();
    serde_json::json!({"kind":"history_page", "tab":tab.0, "requestId":request_id,
        "fromTurn":page.from_turn, "toTurn":to_turn, "records":records})
    .to_string()
}

impl Tab {
    fn history_ingest(&self) -> Option<&agent::ingestion::ConversationIngest> {
        self.live()?.history_ingest()
    }

    pub(super) fn clear_history(&mut self) {
        if let Some(ingest) = self.history_ingest() {
            ingest.clear_reload();
        }
        self.history.released = None;
        self.history.cleared |= self.history.job.is_some() || self.history.held.is_some();
        self.history.generation += 1;
        self.history.job = None;
        self.history.held = None;
    }

    pub(super) fn history_payload(&self) -> Option<String> {
        let (ticket, payload, _) = self.history.held.as_ref()?;
        self.history_ingest()?
            .finish_reload(ticket, true, || {})
            .then(|| payload.clone())
    }
}

impl TabSet {
    pub(super) fn release_history(&mut self, id: TabId) {
        let Some(tab) = self.get_mut(id) else { return };
        tab.clear_history();
        if std::mem::take(&mut tab.history.cleared) {
            let view = tab.view;
            self.history_clears.push((view, cleared(id)));
        }
    }

    /// Reserves the base under the ingest lock; only the worker resolves paths or reads the archive.
    pub fn request_history(
        &mut self,
        tab: TabId,
        request_id: &str,
        action: HistoryAction,
    ) -> Result<Option<String>, String> {
        if self.is_moving(tab) {
            return Err(MOVING.into());
        }
        let tab = self.get_mut(tab).ok_or("no active session")?;
        if action == HistoryAction::Latest {
            tab.clear_history();
            tab.history.cleared = false;
            return Ok(Some(cleared(tab.id)));
        }
        if let Some(job) = &tab.history.job {
            if tab
                .history_ingest()
                .is_some_and(|i| i.finish_reload(&job.ticket, true, || {}))
            {
                return Ok(None);
            }
        }
        if tab.history.worker.as_ref().is_some_and(|worker| !worker.is_finished()) {
            return Ok(Some(empty(tab.id, request_id)));
        }
        tab.history.worker = None;
        let before = tab.history.held.as_ref().map(|h| h.2);
        let Some(ticket) = tab.history_ingest().and_then(|ingest| ingest.begin_reload(before)) else {
            return Ok(Some(empty(tab.id, request_id)));
        };
        // Replace the reservation without releasing it: the visible page survives pending and empty reads.
        tab.history.job = None;
        if let Some(held) = &mut tab.history.held {
            held.0 = ticket.clone();
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(1);
        let worker_ticket = ticket.clone();
        let worker_cancel = cancel.clone();
        let request = request_id.to_owned();
        let id = tab.id;
        let generation = tab.history.generation;
        #[cfg(test)]
        let before_read = tab.history.before_read.take();
        let worker = std::thread::Builder::new()
            .name("eitri-history-page".into())
            .spawn(move || {
                #[cfg(test)]
                if let Some(before) = before_read {
                    before();
                }
                let page = agent::ingestion::read_reload(&worker_ticket, &worker_cancel)
                    .ok()
                    .flatten();
                if !worker_cancel.load(Ordering::SeqCst) {
                    let page = page
                        .filter(|p| !p.turns.is_empty())
                        .map(|page| (page.from_turn, envelope(id, &request, page)));
                    let _ = tx.send(Reply {
                        generation,
                        request_id: request,
                        base: worker_ticket.base,
                        page,
                    });
                }
            });
        match worker {
            Ok(worker) => tab.history.worker = Some(worker),
            Err(error) => {
                if tab.history.held.is_none() {
                    tab.clear_history();
                }
                return Err(error.to_string());
            }
        }
        tab.history.cleared = false;
        tab.history.job = Some(Job {
            generation: tab.history.generation,
            request_id: request_id.into(),
            ticket,
            cancel,
            rx,
        });
        Ok(None)
    }

    pub fn history_pending(&self) -> bool {
        !self.history_clears.is_empty()
            || self
                .tabs
                .iter()
                .any(|tab| tab.history.job.is_some() || tab.history.cleared)
    }

    /// Takes ready replies only. The reservation check and held-page installation share the ingest lock.
    pub fn poll_history(&mut self) -> Vec<(ViewId, String)> {
        let mut out = std::mem::take(&mut self.history_clears);
        for tab in &mut self.tabs {
            let ticket = tab
                .history
                .job
                .as_ref()
                .map(|j| &j.ticket)
                .or_else(|| tab.history.held.as_ref().map(|h| &h.0));
            if let Some(ticket) = ticket {
                if !tab
                    .history_ingest()
                    .is_some_and(|i| i.finish_reload(ticket, true, || {}))
                {
                    let released = tab.history_ingest().and_then(|i| i.reload_was_released(ticket));
                    let pending = tab
                        .history
                        .job
                        .as_ref()
                        .filter(|_| tab.history.held.is_none() && released.is_some())
                        .map(|j| j.request_id.clone());
                    tab.clear_history();
                    tab.history.released = if pending.is_some() { None } else { released };
                    if let Some(request) = pending {
                        tab.history.cleared = false;
                        out.push((tab.view, empty(tab.id, &request)));
                    }
                }
            }
            if let Some(job) = &tab.history.job {
                let reply = match job.rx.try_recv() {
                    Ok(reply) => Some(reply),
                    Err(mpsc::TryRecvError::Empty) => None,
                    Err(mpsc::TryRecvError::Disconnected) => Some(Reply {
                        generation: job.generation,
                        request_id: job.request_id.clone(),
                        base: job.ticket.base,
                        page: None,
                    }),
                };
                if let Some(reply) = reply {
                    let job = tab.history.job.take().unwrap();
                    if reply.generation == tab.history.generation
                        && reply.request_id == job.request_id
                        && reply.base == job.ticket.base
                    {
                        let ingest = match &tab.backend {
                            TabBackend::Live(backend) => backend.history_ingest(),
                            _ => None,
                        };
                        let history = &mut tab.history;
                        if let Some(ingest) = ingest {
                            let installed = ingest.finish_reload(
                                &job.ticket,
                                reply.page.is_some() || history.held.is_some(),
                                || {
                                    if let Some((from, payload)) = reply.page {
                                        out.push((tab.view, payload.clone()));
                                        history.held = Some((job.ticket.clone(), payload, from));
                                    } else {
                                        out.push((tab.view, empty(tab.id, &job.request_id)));
                                    }
                                },
                            );
                            if !installed {
                                let released = ingest.reload_was_released(&job.ticket);
                                let first_page = history.held.is_none() && released.is_some();
                                tab.clear_history();
                                tab.history.released = if first_page { None } else { released };
                                if first_page {
                                    tab.history.cleared = false;
                                    out.push((tab.view, empty(tab.id, &job.request_id)));
                                }
                            }
                        }
                    } else if tab.history.held.is_none() {
                        tab.clear_history();
                    }
                }
            }
            if std::mem::take(&mut tab.history.cleared) {
                let payload = if let Some(turns) = tab.history.released.take() {
                    serde_json::json!({"kind":"history_page", "tab":tab.id.0, "cleared":true, "reason":"released", "turns":turns})
                        .to_string()
                } else {
                    cleared(tab.id)
                };
                out.push((tab.view, payload));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_providers::RecordingProvider;
    use agent::{AgentConversation, ContentKind, TurnOutcome};
    use std::time::{Duration, Instant};

    fn until(mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !done() {
            assert!(Instant::now() < deadline, "worker did not finish");
            std::thread::yield_now();
        }
    }
    fn turns(provider: &RecordingProvider, from: usize, count: usize) {
        for n in from..from + count {
            provider.queue_all(vec![
                AgentDomainEvent::UserPromptSubmitted {
                    text: format!("prompt {n}"),
                },
                AgentDomainEvent::TurnStarted {
                    turn_id: format!("t{n}"),
                },
                AgentDomainEvent::ContentDelta {
                    turn_id: format!("t{n}"),
                    kind: ContentKind::Text,
                    text: format!("reply {n}"),
                },
                AgentDomainEvent::TurnCompleted {
                    turn_id: format!("t{n}"),
                    outcome: TurnOutcome::Completed,
                    result_text: String::new(),
                    stop_reason: None,
                    usage: None,
                    detail: Default::default(),
                },
            ]);
        }
    }
    fn fixture() -> (TabSet, TabId, Arc<RecordingProvider>) {
        agent::state_dirs::redirect_state_to_a_test_root();
        let dir = agent::state_dirs::test_workspace_dir("reload");
        let provider = Arc::new(RecordingProvider::default());
        let conversation =
            AgentConversation::create(provider.clone(), &dir, agent::setting_sources::ProjectTrust::Untrusted).unwrap();
        provider.open_session(&uuid::Uuid::new_v4().to_string(), &dir);
        let mut tabs = TabSet::new(BackendKind::Sidecar, SessionModeChoice::Auto);
        let id = tabs.active();
        tabs.get_mut(id)
            .unwrap()
            .set_backend(TabBackend::Live(AgentBackend::Conversation {
                agent: AgentId::Claude,
                conversation: Box::new(conversation),
            }));
        let n = agent::history::history_turns();
        turns(&provider, 0, n + n / 10 + 1);
        until(|| tabs.get(id).unwrap().live().unwrap().projection().evicted.is_some());
        (tabs, id, provider)
    }
    fn loaded(tabs: &mut TabSet, id: TabId) -> serde_json::Value {
        until(|| {
            tabs.poll_history();
            tabs.get(id).unwrap().history.held.is_some()
        });
        until(|| tabs.get(id).unwrap().history.worker.as_ref().unwrap().is_finished());
        serde_json::from_str(&tabs.get(id).unwrap().history.held.as_ref().unwrap().1).unwrap()
    }
    fn hold(tabs: &mut TabSet, id: TabId) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (entered, wait) = mpsc::sync_channel(1);
        let (release, gate) = mpsc::channel();
        tabs.get_mut(id).unwrap().history.before_read = Some(Box::new(move || {
            entered.send(()).unwrap();
            gate.recv_timeout(Duration::from_secs(30)).unwrap();
        }));
        (wait, release)
    }

    fn fixture_pages() -> (TabSet, TabId, Arc<RecordingProvider>) {
        let (tabs, id, provider) = fixture();
        let n = agent::history::history_turns();
        let start = n + n / 10 + 1;
        turns(&provider, start, n * 2);
        until(|| {
            let p = tabs.get(id).unwrap().live().unwrap().projection();
            p.user_prompts
                .last()
                .is_some_and(|r| r.text == format!("prompt {}", start + n * 2 - 1))
                && p.evicted.as_ref().is_some_and(|e| e.turns > n as u64)
                && p.active_turn_id.is_none()
        });
        (tabs, id, provider)
    }

    fn reply(tabs: &mut TabSet) -> serde_json::Value {
        let mut result = None;
        until(|| {
            for (_, payload) in tabs.poll_history() {
                result = Some(serde_json::from_str(&payload).unwrap());
            }
            tabs.tabs.iter().all(|t| t.history.job.is_none())
        });
        until(|| {
            tabs.tabs
                .iter()
                .all(|t| t.history.worker.as_ref().is_none_or(|w| w.is_finished()))
        });
        result.expect("worker finished without a history reply")
    }

    #[test]
    fn older_replaces_a_held_page_only_after_install_and_keeps_the_reservation() {
        let (mut tabs, id, provider) = fixture_pages();
        tabs.request_history(id, "first", HistoryAction::Older).unwrap();
        let first = loaded(&mut tabs, id);
        assert!(first["fromTurn"].as_u64().unwrap() > 0);
        let base = tabs.get(id).unwrap().history.held.as_ref().unwrap().0.base;
        assert_eq!(first["toTurn"], base);
        let (entered, release) = hold(&mut tabs, id);
        tabs.request_history(id, "second", HistoryAction::Older).unwrap();
        assert!(
            tabs.get(id).unwrap().history.job.is_some(),
            "held pages must allow another older read"
        );
        entered.recv_timeout(Duration::from_secs(30)).unwrap();
        let tab = tabs.get(id).unwrap();
        let ticket = &tab.history.job.as_ref().unwrap().ticket;
        assert_eq!(ticket.before, first["fromTurn"].as_u64().unwrap());
        assert_eq!(ticket.base, base);
        assert_eq!(tab.history_payload().unwrap(), first.to_string());
        let live_count = tab.live().unwrap().projection().user_prompts.len();
        let n = agent::history::history_turns();
        turns(&provider, 3 * n + n / 10 + 1, n / 5);
        until(|| tabs.get(id).unwrap().live().unwrap().projection().user_prompts.len() == live_count + n / 5);
        assert_eq!(
            tabs.get(id)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .evicted
                .as_ref()
                .unwrap()
                .turns,
            base
        );
        assert!(tabs.poll_history().is_empty());
        release.send(()).unwrap();
        let second = reply(&mut tabs);
        assert_eq!(second["requestId"], "second");
        assert_eq!(second["toTurn"], first["fromTurn"]);
        assert!(second["fromTurn"].as_u64().unwrap() < first["fromTurn"].as_u64().unwrap());
        assert_eq!(tabs.get(id).unwrap().history_payload().unwrap(), second.to_string());
    }

    #[test]
    fn an_empty_second_read_keeps_the_held_page_and_answers_empty() {
        let (mut tabs, id, _) = fixture_pages();
        tabs.request_history(id, "first", HistoryAction::Older).unwrap();
        let first = loaded(&mut tabs, id);
        let ticket = tabs.get(id).unwrap().history.held.as_ref().unwrap().0.clone();
        std::fs::remove_dir_all(
            agent::state_dirs::redirect_state_to_a_test_root()
                .join("history")
                .join(&ticket.id.conversation_id),
        )
        .unwrap();
        tabs.request_history(id, "empty", HistoryAction::Older).unwrap();
        assert_eq!(
            reply(&mut tabs),
            serde_json::json!({"kind":"history_page", "tab":id.0, "requestId":"empty", "empty":true})
        );
        assert_eq!(tabs.get(id).unwrap().history_payload().unwrap(), first.to_string());
        assert!(tabs.poll_history().is_empty());
    }

    #[test]
    fn stale_replacement_reply_is_dropped_without_releasing_the_visible_page() {
        let (mut tabs, id, _) = fixture_pages();
        tabs.request_history(id, "held", HistoryAction::Older).unwrap();
        let first = loaded(&mut tabs, id);
        let (entered, release) = hold(&mut tabs, id);
        tabs.request_history(id, "replaced", HistoryAction::Older).unwrap();
        assert!(
            tabs.get(id).unwrap().history.job.is_some(),
            "held pages must allow another older read"
        );
        entered.recv_timeout(Duration::from_secs(30)).unwrap();
        let flag = tabs.get(id).unwrap().history.job.as_ref().unwrap().cancel.clone();
        tabs.request_history(id, "current", HistoryAction::Older).unwrap();
        assert!(
            !flag.load(Ordering::SeqCst),
            "a duplicate leaves the current read running"
        );
        release.send(()).unwrap();
        let job = tabs.get_mut(id).unwrap().history.job.as_mut().unwrap();
        let mut old = job.rx.recv_timeout(Duration::from_secs(30)).unwrap();
        old.request_id = "stale".into();
        let (tx, rx) = mpsc::sync_channel(1);
        tx.send(old).unwrap();
        job.rx = rx;
        assert!(tabs.poll_history().is_empty());
        assert_eq!(tabs.get(id).unwrap().history_payload().unwrap(), first.to_string());
        until(|| tabs.get(id).unwrap().history.worker.as_ref().unwrap().is_finished());
        tabs.request_history(id, "retry", HistoryAction::Older).unwrap();
        assert_eq!(reply(&mut tabs)["requestId"], "retry");
    }

    #[test]
    fn page_envelopes_report_exclusive_bounds_for_adjacent_and_gapped_pages() {
        for before in [3, 8] {
            let raw = envelope(
                TabId(1),
                "page",
                Page {
                    from_turn: before - 2,
                    turns: vec![
                        agent::history::archive::ArchivedTurn {
                            turn: before - 2,
                            records: vec![],
                        },
                        agent::history::archive::ArchivedTurn {
                            turn: before - 1,
                            records: vec![],
                        },
                    ],
                    cut: Default::default(),
                },
            );
            let wire: serde_json::Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(wire["toTurn"], before);
            assert_eq!(wire["fromTurn"], before - 2);
            assert_eq!(wire["records"].as_array().unwrap().len(), 2);
        }
    }

    #[test]
    fn archive_request_round_trip_and_snapshot_resends_only_one_held_page() {
        let (mut tabs, id, _) = fixture();
        tabs.request_history(id, "older-1", HistoryAction::Older).unwrap();
        let page = loaded(&mut tabs, id);
        assert_eq!(page["requestId"], "older-1");
        assert_eq!(page["fromTurn"], 0);
        assert_eq!(page["records"][0]["userPrompts"][0]["text"], "prompt 0");
        assert_eq!(page["records"][0]["transcript"][0]["text"], "reply 0");
        assert_eq!(
            tabs.get(id).unwrap().live().unwrap().projection().user_prompts.len(),
            agent::history::history_turns()
        );
        assert!(tabs
            .state_payloads(id)
            .iter()
            .any(|s| serde_json::from_str::<serde_json::Value>(s).unwrap() == page));
        tabs.request_history(id, "ignored", HistoryAction::Older).unwrap();
        assert!(tabs.get(id).unwrap().history.job.is_none());
        assert_eq!(tabs.get(id).unwrap().history_payload().unwrap(), page.to_string());
        let ticket = tabs.get(id).unwrap().history.held.as_ref().unwrap().0.clone();
        let cleared = tabs
            .request_history(id, "latest", HistoryAction::Latest)
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&cleared).unwrap()["cleared"],
            true
        );
        assert!(!tabs
            .get(id)
            .unwrap()
            .history_ingest()
            .unwrap()
            .finish_reload(&ticket, true, || panic!("stale")));
        assert!(tabs.get(id).unwrap().history.held.is_none());
    }

    #[test]
    fn stalled_reader_is_unique_across_duplicates_and_latest() {
        let (mut tabs, id, _) = fixture();
        let (entered, release) = hold(&mut tabs, id);
        tabs.request_history(id, "first", HistoryAction::Older).unwrap();
        entered.recv_timeout(Duration::from_secs(30)).unwrap();
        let cancel = tabs.get(id).unwrap().history.job.as_ref().unwrap().cancel.clone();
        for _ in 0..20 {
            assert!(tabs
                .request_history(id, "duplicate", HistoryAction::Older)
                .unwrap()
                .is_none());
            assert!(Arc::ptr_eq(
                &cancel,
                &tabs.get(id).unwrap().history.job.as_ref().unwrap().cancel
            ));
        }
        assert!(!cancel.load(Ordering::SeqCst));
        tabs.request_history(id, "latest", HistoryAction::Latest).unwrap();
        assert!(cancel.load(Ordering::SeqCst));
        for _ in 0..20 {
            let answer = tabs.request_history(id, "busy", HistoryAction::Older).unwrap().unwrap();
            let wire: serde_json::Value = serde_json::from_str(&answer).unwrap();
            assert_eq!(wire["empty"], true);
            assert!(tabs.get(id).unwrap().history.job.is_none());
        }
        release.send(()).unwrap();
        until(|| tabs.get(id).unwrap().history.worker.as_ref().unwrap().is_finished());
        tabs.request_history(id, "third", HistoryAction::Older).unwrap();
        assert_eq!(loaded(&mut tabs, id)["requestId"], "third");
    }

    #[test]
    fn reset_move_and_close_cancel_a_held_worker_before_its_reply() {
        for how in ["reset", "move", "handoff", "close", "window close"] {
            let (mut tabs, id, provider) = fixture();
            let (entered, release) = hold(&mut tabs, id);
            tabs.request_history(id, "old", HistoryAction::Older).unwrap();
            entered.recv_timeout(Duration::from_secs(30)).unwrap();
            let cancel = tabs.get(id).unwrap().history.job.as_ref().unwrap().cancel.clone();
            match how {
                "reset" => {
                    provider.queue(AgentDomainEvent::ConversationReset {
                        new_conversation_id: "new-session".into(),
                        trigger: None,
                    });
                    until(|| tabs.get(id).unwrap().live().unwrap().projection().evicted.is_none());
                }
                "move" => {
                    tabs.open(AgentId::Claude);
                    tabs.select(id);
                    tabs.break_out(ViewId::new(2).unwrap()).unwrap();
                }
                "handoff" => {
                    let cwd = tabs.get(id).unwrap().live().unwrap().projection().cwd.clone().unwrap();
                    tabs.pump(Path::new(&cwd), false);
                    until(|| match tabs.freeze_for_move(id) {
                        Ok(_) => true,
                        Err(MoveRefusedWhy::Folding) => false,
                        Err(why) => panic!("{why}"),
                    });
                }
                "close" => {
                    tabs.remove(id).unwrap();
                }
                "window close" => {
                    tabs.take_all();
                }
                _ => unreachable!(),
            }
            let out = tabs.poll_history();
            if how != "window close" {
                assert!(out.iter().any(|(_, p)| p.contains("\"cleared\":true")), "{how}");
            }
            assert!(cancel.load(Ordering::SeqCst), "{how}");
            release.send(()).unwrap();
            assert!(tabs.poll_history().iter().all(|(_, p)| !p.contains("records")));
        }
    }

    #[test]
    fn refused_freeze_preserves_the_held_page_and_reload_reservation() {
        let (mut tabs, id, provider) = fixture();
        tabs.request_history(id, "held", HistoryAction::Older).unwrap();
        let page = loaded(&mut tabs, id);
        // A window must have pumped once before its tabs can move.
        assert!(matches!(tabs.freeze_for_move(id), Err(MoveRefusedWhy::Conversation(_))));
        assert_eq!(tabs.get(id).unwrap().history_payload().unwrap(), page.to_string());
        assert!(tabs.poll_history().is_empty());
        let cwd = tabs.get(id).unwrap().live().unwrap().projection().cwd.clone().unwrap();
        tabs.pump(Path::new(&cwd), false);
        let (entered, release) = provider.hold_next_pump();
        entered.recv_timeout(Duration::from_secs(30)).unwrap();
        let refused = tabs.freeze_for_move(id);
        release.send(()).unwrap();
        assert_eq!(refused.unwrap_err(), MoveRefusedWhy::Folding);
        assert_eq!(tabs.get(id).unwrap().history_payload().unwrap(), page.to_string());
        assert!(tabs.poll_history().is_empty());
        let deadline = Instant::now() + Duration::from_secs(30);
        let frozen = loop {
            assert!(Instant::now() < deadline, "freeze did not finish");
            match tabs.freeze_for_move(id) {
                Ok(frozen) => break frozen,
                Err(MoveRefusedWhy::Folding) => std::thread::yield_now(),
                Err(why) => panic!("{why}"),
            }
        };
        assert!(tabs.get(id).unwrap().history.held.is_none());
        assert!(tabs.poll_history().iter().any(|(_, p)| p.contains("\"cleared\":true")));
        drop(frozen);
    }

    #[test]
    fn requested_page_defers_eviction_until_ceiling_then_sends_cleared() {
        let (mut tabs, id, provider) = fixture();
        let (entered, release) = hold(&mut tabs, id);
        tabs.request_history(id, "held", HistoryAction::Older).unwrap();
        entered.recv_timeout(Duration::from_secs(30)).unwrap();
        let n = agent::history::history_turns();
        let start = n + n / 10 + 1;
        let base = tabs.get(id).unwrap().history.job.as_ref().unwrap().ticket.base;
        turns(&provider, start, n - 1);
        until(|| tabs.get(id).unwrap().live().unwrap().projection().user_prompts.len() == 2 * n - 1);
        assert_eq!(
            tabs.get(id)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .evicted
                .as_ref()
                .unwrap()
                .turns,
            base
        );
        assert!(tabs.poll_history().is_empty());
        release.send(()).unwrap();
        let page = loaded(&mut tabs, id);
        assert_eq!(page["records"].as_array().unwrap().len(), base as usize);
        turns(&provider, start + n - 1, 1);
        until(|| {
            tabs.get(id)
                .unwrap()
                .live()
                .unwrap()
                .projection()
                .evicted
                .as_ref()
                .unwrap()
                .turns
                > base
        });
        let released = tabs.poll_history();
        assert!(released.iter().any(|(_, p)| {
            let wire: serde_json::Value = serde_json::from_str(p).unwrap();
            wire["cleared"] == true && wire["reason"] == "released" && wire["turns"] == base + n as u64
        }));
        assert!(tabs.get(id).unwrap().history.held.is_none());
    }

    #[test]
    fn polling_during_ceiling_write_keeps_the_page_and_older_replies_until_the_cut_folds() {
        for abandon in [false, true] {
            let (mut tabs, id, provider) = fixture();
            tabs.request_history(id, "held", HistoryAction::Older).unwrap();
            let page = loaded(&mut tabs, id);
            let base = tabs.get(id).unwrap().history.held.as_ref().unwrap().0.base;
            let conversation = tabs.get(id).unwrap().live().unwrap().conversation_id().unwrap();
            let (entered, release) = agent::ingestion::hold_eviction_write_for_test(conversation);
            let n = agent::history::history_turns();
            turns(&provider, n + n / 10 + 1, n);
            entered.recv_timeout(Duration::from_secs(30)).unwrap();
            // These are the same reads a window makes while the archive filesystem is stalled.
            assert!(tabs.poll_history().is_empty());
            assert_eq!(tabs.get(id).unwrap().history_payload().unwrap(), page.to_string());
            let answer = tabs
                .request_history(id, "during", HistoryAction::Older)
                .unwrap()
                .unwrap();
            let answer: serde_json::Value = serde_json::from_str(&answer).unwrap();
            assert_eq!(answer["empty"], true);
            assert_eq!(answer["cleared"], serde_json::Value::Null);
            let (next_pump, resume) = provider.hold_next_pump();
            if abandon {
                let AgentBackend::Conversation { conversation, .. } = tabs.get(id).unwrap().live().unwrap() else {
                    panic!("conversation")
                };
                conversation.fold_locally(AgentDomainEvent::EffortReported { effort: None });
            }
            release.send(()).unwrap();
            next_pump.recv_timeout(Duration::from_secs(30)).unwrap();
            let replies = tabs.poll_history();
            if abandon {
                assert!(replies.is_empty());
                assert_eq!(tabs.get(id).unwrap().history_payload().unwrap(), page.to_string());
            } else {
                let wire: serde_json::Value = serde_json::from_str(&replies[0].1).unwrap();
                assert_eq!(wire["cleared"], true);
                assert_eq!(wire["reason"], "released");
                assert_eq!(wire["turns"], base + n as u64);
            }
            resume.send(()).unwrap();
        }
    }

    #[test]
    fn first_older_during_ceiling_write_answers_empty_after_the_cut() {
        let (mut tabs, id, provider) = fixture();
        let conversation = tabs.get(id).unwrap().live().unwrap().conversation_id().unwrap();
        let (entered, release) = agent::ingestion::hold_eviction_write_for_test(conversation);
        let n = agent::history::history_turns();
        turns(&provider, n + n / 10 + 1, n);
        entered.recv_timeout(Duration::from_secs(30)).unwrap();
        let (reading, finish_read) = hold(&mut tabs, id);
        assert!(tabs
            .request_history(id, "first-during", HistoryAction::Older)
            .unwrap()
            .is_none());
        reading.recv_timeout(Duration::from_secs(30)).unwrap();
        let (next_pump, resume) = provider.hold_next_pump();
        release.send(()).unwrap();
        next_pump.recv_timeout(Duration::from_secs(30)).unwrap();
        let replies = tabs.poll_history();
        finish_read.send(()).unwrap();
        resume.send(()).unwrap();
        assert_eq!(replies.len(), 1);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&replies[0].1).unwrap(),
            serde_json::json!({"kind":"history_page", "tab":id.0, "requestId":"first-during", "empty":true})
        );
    }

    #[test]
    fn empty_archive_result_releases_reservation_without_a_page() {
        let (mut tabs, id, _) = fixture();
        let ticket = tabs
            .get(id)
            .unwrap()
            .history_ingest()
            .unwrap()
            .begin_reload(None)
            .unwrap();
        tabs.get(id).unwrap().history_ingest().unwrap().clear_reload();
        let dir = agent::state_dirs::redirect_state_to_a_test_root()
            .join("history")
            .join(&ticket.id.conversation_id);
        // The settled fixture has synced its archive and has no further turn ends to write.
        std::fs::remove_dir_all(dir).unwrap();
        tabs.request_history(id, "empty", HistoryAction::Older).unwrap();
        assert_eq!(
            reply(&mut tabs),
            serde_json::json!({"kind":"history_page", "tab":id.0, "requestId":"empty", "empty":true})
        );
        assert!(tabs.get(id).unwrap().history.held.is_none());
        assert!(tabs
            .get(id)
            .unwrap()
            .history_ingest()
            .unwrap()
            .begin_reload(None)
            .is_some());
    }

    #[test]
    fn stale_generation_and_base_never_install_and_latest_cancels_before_read() {
        for field in ["generation", "base", "latest"] {
            let (mut tabs, id, _) = fixture();
            let (entered, release) = hold(&mut tabs, id);
            tabs.request_history(id, "request", HistoryAction::Older).unwrap();
            entered.recv_timeout(Duration::from_secs(30)).unwrap();
            let ticket = tabs.get(id).unwrap().history.job.as_ref().unwrap().ticket.clone();
            if field == "latest" {
                let flag = tabs.get(id).unwrap().history.job.as_ref().unwrap().cancel.clone();
                tabs.request_history(id, "latest", HistoryAction::Latest).unwrap();
                assert!(flag.load(Ordering::SeqCst));
                release.send(()).unwrap();
            } else {
                release.send(()).unwrap();
                let job = tabs.get_mut(id).unwrap().history.job.as_mut().unwrap();
                let mut reply = job.rx.recv_timeout(Duration::from_secs(30)).unwrap();
                if field == "generation" {
                    reply.generation += 1;
                } else {
                    reply.base += 1;
                }
                let (tx, rx) = mpsc::sync_channel(1);
                tx.send(reply).unwrap();
                job.rx = rx;
            }
            tabs.poll_history();
            assert!(tabs.get(id).unwrap().history.held.is_none());
            assert!(!tabs
                .get(id)
                .unwrap()
                .history_ingest()
                .unwrap()
                .finish_reload(&ticket, true, || panic!("stale")));
        }
    }

    #[test]
    fn history_wire_names_its_tab_and_accepts_only_two_actions() {
        use crate::agent_bridge::{parse_inbound_message, TabRef};
        for action in ["older", "latest"] {
            let raw = serde_json::json!({"type":"history_request", "request_id":"load", "tab":7, "action":action});
            let request = parse_inbound_message(&raw.to_string()).unwrap();
            assert_eq!(request.request_id(), "load");
            assert_eq!(request.tab_ref(), TabRef::Named(TabId(7)));
        }
        assert!(
            parse_inbound_message(r#"{"type":"history_request","request_id":"load","tab":7,"action":"all"}"#).is_none()
        );
    }
}
