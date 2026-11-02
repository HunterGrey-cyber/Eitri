//! Bringing a window's saved tabs back (`saved_tabs` holds what was open; `TabSet::start_restore`
//! starts them): what `agent.restore` may say, which saved tabs can still be brought back, what the
//! launch dashboard offers, and the tally that ends in one message once every resume has returned.
//!
//! A saved tab is skipped, and said to be skipped, when its record is gone or no longer resumable,
//! when another window holds its session, or when the resume itself fails. Nothing here ever fails a
//! launch.

use std::collections::BTreeMap;

use agent::ResumableSession;

use crate::agent_backend::AgentId;
use crate::agent_bridge::SessionModeChoice;
use crate::saved_tabs::{SavedTab, SavedTabs};
use crate::tabs::{label_name, TabId, ViewId};

/// What happens to the saved tabs at launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestorePolicy {
    /// The empty tab's dashboard offers them (`s`); nothing happens unless it is taken.
    Offer,
    /// They are brought back at launch with no key pressed.
    Auto,
    /// Nothing is offered or restored, and nothing is remembered.
    Off,
}

/// The `init.lua` key: `eitri.config.set("agent.restore", "offer" | "auto" | "off" | false)`.
pub const RESTORE_KEY: &str = "agent.restore";

impl RestorePolicy {
    /// The value of [`RESTORE_KEY`] as `init.lua` left it: unset is [`RestorePolicy::Offer`]; anything
    /// but a listed choice is an error naming the key, as every other configuration key here is.
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.map(str::trim) {
            None | Some("offer") => Ok(RestorePolicy::Offer),
            Some("auto") => Ok(RestorePolicy::Auto),
            Some(value) if crate::config_switch::is_off(value) => Ok(RestorePolicy::Off),
            Some(_) => Err(format!(
                "eitri.config.set(\"{RESTORE_KEY}\", {:?}): must be \"offer\", \"auto\" or \"off\" / false",
                value.unwrap_or_default()
            )),
        }
    }
}

/// A saved tab that is not coming back, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub label: String,
    pub reason: String,
    /// The saved tab, when something was typed for it: it stays in the file while the restore waits for its trust
    /// answer, and its draft comes back in the tab the restore runs from.
    pub kept: Option<SavedTab>,
}

/// A saved tab that can be resumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedTab {
    /// The agent the tab ran, and runs again.
    pub agent: AgentId,
    pub provider_session_id: String,
    pub name: Option<String>,
    pub title: Option<String>,
    /// The mode the tab was saved in. What the restored tab gets is decided by
    /// `TabSet::begin_restore`, never here.
    pub saved_mode: SessionModeChoice,
    pub label: String,
    /// The extra chat view the tab was saved in (`agent#N`, N from 2); `None` for the base view.
    /// Where it comes back is decided by `TabSet::start_restore`, which puts it there only when the
    /// window's layout holds that view and no other tab is bound to it yet.
    pub view: Option<ViewId>,
    /// The tab was saved with a turn running that never ended.
    pub turn_running: bool,
    /// The composer's unsent text as it was saved.
    pub draft: Option<String>,
}

/// Which saved tabs to resume and which are skipped before any resume is tried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePlan {
    pub items: Vec<PlannedTab>,
    pub skipped: Vec<Skipped>,
    /// The saved active tab's session, even when that tab is skipped (the restore then falls back to
    /// the first tab that comes back).
    pub active_session: Option<String>,
}

pub const GONE: &str = "its saved record is gone";
pub const HELD_ELSEWHERE: &str = "open in another window";
pub const NOT_REGISTERED: &str = "its agent is not registered in init.lua";

/// Whether `record` is a session `open` holds: the same agent's session of the same id. Two agents
/// may use the same id, so an open session of one never hides the other's record.
pub fn is_open(open: &[(AgentId, String)], record: &ResumableSession) -> bool {
    open.iter()
        .any(|(agent, id)| *id == record.provider_session_id && agent.record_name() == record.provider)
}

/// Sorts `saved` into what can be resumed and what cannot. `records` are the sessions this project
/// can resume (`agent::resumable_sessions`, with their display titles), of every agent: a saved tab
/// is matched only with a record its own agent wrote. `open` the sessions this window already has
/// open, each with its agent (`TabSet::open_sessions`): a saved tab whose agent has that session open
/// here is left out without comment, as it is already where it should be, while another agent's
/// session that happens to share its id does not keep it from coming back. `held` says whether a
/// lease on an agent's session is held right now. The tabs are planned in the tab bar's order when
/// the file recorded it (`SavedTabs::in_bar_order`), else the Claude tabs first.
pub fn plan(
    saved: &SavedTabs,
    records: &[ResumableSession],
    open: &[(AgentId, String)],
    held: impl Fn(&AgentId, &str) -> bool,
) -> RestorePlan {
    let mut items = Vec::new();
    let mut skipped = Vec::new();
    for tab in saved.in_bar_order() {
        let (agent, id) = (tab.agent.clone(), tab.provider_session_id.as_str());
        let (saved_name, saved_mode, saved_view) = (tab.name.as_deref(), tab.mode, tab.view);
        let turn_running = tab.turn_running;
        let draft = tab.draft.clone();
        if open
            .iter()
            .any(|(open_agent, open_id)| *open_agent == agent && open_id == id)
        {
            continue;
        }
        let provider = agent.record_name();
        let Some(record) = records
            .iter()
            .find(|r| r.provider_session_id == id && r.provider == provider)
        else {
            skipped.push(Skipped {
                label: label_name(saved_name, None, Some(id)),
                reason: GONE.to_string(),
                kept: tab.draft.is_some().then(|| tab.clone()),
            });
            continue;
        };
        // The record carries the rename the tab had (a rename is written to it); the saved copy only
        // covers a rename that was made after the record was last read.
        let name = record.name.clone().or_else(|| saved_name.map(str::to_string));
        let label = label_name(name.as_deref(), record.title.as_deref(), Some(id));
        if held(&agent, id) {
            skipped.push(Skipped {
                label,
                reason: HELD_ELSEWHERE.to_string(),
                kept: tab.draft.is_some().then(|| tab.clone()),
            });
            continue;
        }
        items.push(PlannedTab {
            agent,
            provider_session_id: id.to_string(),
            name,
            title: record.title.clone(),
            saved_mode,
            label,
            view: saved_view.and_then(ViewId::new).filter(|view| !view.is_base()),
            turn_running,
            draft,
        });
    }
    let active_session = saved.active_tab().map(|t| t.provider_session_id.clone());
    RestorePlan {
        items,
        skipped,
        active_session,
    }
}

/// What the user is told of `cut` tabs coming back whose turn was still running when their window ended: it did not finish,
/// and nothing else says so, since a window that loses its compositor is gone before it can ask.
pub fn cut_turns_note(cut: usize) -> Option<String> {
    match cut {
        0 => None,
        1 => Some("a turn was cut off when this window last ended".to_string()),
        cut => Some(format!("{cut} turns were cut off when this window last ended")),
    }
}

/// What the launch dashboard says about the tabs it can bring back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreOffer {
    /// One per tab that would come back, in tab-bar order.
    pub labels: Vec<String>,
    /// How many of them were in bypass.
    pub bypass: usize,
    /// What the dashboard says of the turns of these tabs that never finished, for as long as it offers them.
    pub cut_note: Option<String>,
}

impl RestorePlan {
    /// Moves every planned tab whose agent this window cannot start to the skipped tabs, saying so:
    /// a tab of an ACP agent saved while `init.lua` registered it, and no longer registered there. It
    /// is then never brought back to wait for a question whose answer could not start it.
    pub fn skip_unregistered(&mut self, registered: impl Fn(&AgentId) -> bool) {
        let (kept, unregistered): (Vec<PlannedTab>, Vec<PlannedTab>) = std::mem::take(&mut self.items)
            .into_iter()
            .partition(|item| registered(&item.agent));
        self.items = kept;
        self.skipped.extend(unregistered.into_iter().map(|item| Skipped {
            kept: item.draft.is_some().then(|| SavedTab {
                agent: item.agent,
                conversation_id: String::new(),
                provider_session_id: item.provider_session_id,
                name: item.name,
                mode: item.saved_mode,
                view: item.view.map(|view| view.number()),
                window: None,
                turn_running: item.turn_running,
                draft: item.draft,
            }),
            label: item.label,
            reason: NOT_REGISTERED.to_string(),
        }));
    }

    /// `None` when every saved tab would be skipped: a key that restores nothing is not offered.
    pub fn offer(&self) -> Option<RestoreOffer> {
        if self.items.is_empty() {
            return None;
        }
        Some(RestoreOffer {
            labels: self.items.iter().map(|i| i.label.clone()).collect(),
            bypass: self
                .items
                .iter()
                .filter(|i| i.saved_mode == SessionModeChoice::Bypass)
                .count(),
            cut_note: cut_turns_note(self.items.iter().filter(|i| i.turn_running).count()),
        })
    }
}

/// The tally of one restore: which started tabs are still connecting and how the rest ended.
#[derive(Debug)]
pub struct RestoreRun {
    /// The tab the restore began in. If its resume fails it goes back to being an empty tab; every
    /// other restored tab is a new one and is closed again.
    pub(crate) first: TabId,
    /// The mode that tab had before the restore gave it a session's, so a failed resume leaves an
    /// empty tab as it found it rather than in a mode only the restored session was cleared for.
    pub(crate) first_mode: SessionModeChoice,
    /// The agent that tab ran before, for the same reason.
    pub(crate) first_agent: AgentId,
    pub(crate) pending: BTreeMap<TabId, String>,
    /// The tabs the restore started in the base view, in saved order, until one fails or is closed:
    /// where the screen goes when the tab it was on did not come back. A tab that came back bound to
    /// an extra view is not listed, as the base view cannot show it.
    pub(crate) order: Vec<TabId>,
    /// The tabs brought back holding their resume for their own agent's trust answer: counted as
    /// restored, and started (or not) by that answer, outside this tally.
    pub(crate) awaiting: Vec<TabId>,
    pub(crate) restored: usize,
    pub(crate) failed: Vec<Skipped>,
    pub(crate) total: usize,
}

impl RestoreRun {
    pub(crate) fn new(
        first: TabId,
        first_mode: SessionModeChoice,
        first_agent: AgentId,
        total: usize,
        skipped: Vec<Skipped>,
    ) -> Self {
        RestoreRun {
            first,
            first_mode,
            first_agent,
            pending: BTreeMap::new(),
            order: Vec::new(),
            awaiting: Vec::new(),
            restored: 0,
            failed: skipped,
            total,
        }
    }

    /// Whether `tab` is one of this restore's, still connecting.
    pub fn owns(&self, tab: TabId) -> bool {
        self.pending.contains_key(&tab)
    }

    /// The tabs that came back waiting for their own agent's trust answer, in saved order.
    pub fn awaiting_trust(&self) -> &[TabId] {
        &self.awaiting
    }

    /// Whether `tab` is the one the restore began in.
    pub fn began_in(&self, tab: TabId) -> bool {
        self.first == tab
    }

    /// What `tab` goes back to if its resume fails: `Some((mode, agent))`, an empty tab again in the
    /// mode it had and running the agent it ran, when it is the one the restore began in; `None`,
    /// closed, when the restore made it.
    pub fn back_to(&self, tab: TabId) -> Option<(SessionModeChoice, AgentId)> {
        self.began_in(tab).then(|| (self.first_mode, self.first_agent.clone()))
    }

    /// A tab still connecting that the user closed: nothing is owed for it, and the message neither
    /// counts it restored nor says it failed -- it was never going to be there.
    pub fn forget_closed(&mut self, still_open: impl Fn(TabId) -> bool) {
        let gone: Vec<TabId> = self.pending.keys().copied().filter(|tab| !still_open(*tab)).collect();
        for tab in gone {
            self.pending.remove(&tab);
            self.order.retain(|t| *t != tab);
            self.total -= 1;
        }
        // A tab that came back waiting for its trust answer and was closed is no place to go back to.
        self.order.retain(|t| still_open(*t));
        self.awaiting.retain(|t| still_open(*t));
    }

    /// The first tab the restore started that is still there (connecting or connected): where the
    /// screen goes when the tab it was on, a saved active tab, failed.
    pub fn first_survivor(&self) -> Option<TabId> {
        self.order.first().copied()
    }

    /// The session's backend was installed. A resume that the provider has not yet answered counts:
    /// if it is refused later the tab shows the lost-session state in place, as a resume by hand does.
    pub fn note_installed(&mut self, tab: TabId) {
        if self.pending.remove(&tab).is_some() {
            self.restored += 1;
        }
    }

    /// The resume returned an error.
    pub fn note_failed(&mut self, tab: TabId, reason: &str) {
        if let Some(label) = self.pending.remove(&tab) {
            self.order.retain(|t| *t != tab);
            self.failed.push(Skipped {
                label,
                reason: reason.to_string(),
                kept: None,
            });
        }
    }

    /// `Some` once every resume that was started has returned.
    pub fn outcome(&self) -> Option<RestoreOutcome> {
        self.pending.is_empty().then(|| RestoreOutcome {
            restored: self.restored,
            total: self.total,
            failed: self.failed.clone(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreOutcome {
    pub restored: usize,
    pub total: usize,
    pub failed: Vec<Skipped>,
}

impl RestoreOutcome {
    /// The one message a restore ends in.
    pub fn message(&self) -> String {
        let tabs = |n: usize| if n == 1 { "tab" } else { "tabs" };
        if self.failed.is_empty() {
            return format!("Restored {} {}", self.restored, tabs(self.restored));
        }
        let reasons: Vec<String> = self
            .failed
            .iter()
            .map(|f| format!("{} ({})", f.label, f.reason))
            .collect();
        format!(
            "Restored {} of {} {}; {} could not be: {}",
            self.restored,
            self.total,
            tabs(self.total),
            self.failed.len(),
            reasons.join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(tabs: &[(&str, Option<&str>, SessionModeChoice)], active: usize) -> SavedTabs {
        SavedTabs::from_parts(
            tabs.iter()
                .map(|(id, name, mode)| SavedTab {
                    agent: AgentId::Claude,
                    conversation_id: "c".to_string(),
                    provider_session_id: id.to_string(),
                    name: name.map(str::to_string),
                    mode: *mode,
                    view: None,
                    window: None,
                    turn_running: false,
                    draft: None,
                })
                .collect(),
            active,
            Vec::new(),
            None,
        )
    }

    fn record(id: &str, name: Option<&str>, title: Option<&str>) -> ResumableSession {
        ResumableSession {
            provider: "claude".to_string(),
            provider_session_id: id.to_string(),
            created_at: "1".to_string(),
            updated_at: "2".to_string(),
            title: title.map(str::to_string),
            name: name.map(str::to_string),
        }
    }

    use SessionModeChoice::{Auto, Bypass};

    #[test]
    fn restore_false_is_off() {
        for value in ["false", "off", " false ", " off "] {
            assert_eq!(RestorePolicy::parse(Some(value)), Ok(RestorePolicy::Off));
        }
        for value in ["true", "on", "False", "OFF"] {
            assert!(RestorePolicy::parse(Some(value)).is_err());
        }
    }

    #[test]
    fn the_policy_reads_its_choices_and_rejects_other_values() {
        assert_eq!(RestorePolicy::parse(None), Ok(RestorePolicy::Offer));
        assert_eq!(RestorePolicy::parse(Some("offer")), Ok(RestorePolicy::Offer));
        assert_eq!(RestorePolicy::parse(Some(" auto ")), Ok(RestorePolicy::Auto));
        assert_eq!(RestorePolicy::parse(Some("off")), Ok(RestorePolicy::Off));
        let err = RestorePolicy::parse(Some("always")).unwrap_err();
        assert!(err.contains("agent.restore") && err.contains("\"always\""), "{err}");
        assert!(RestorePolicy::parse(Some("")).is_err());
        assert!(
            RestorePolicy::parse(Some("Auto")).is_err(),
            "case matters, as in every other key"
        );
    }

    /// A Codex tab between two Claude tabs comes back between them, not behind them.
    #[test]
    fn tabs_of_different_agents_come_back_in_the_tab_bars_order() {
        let tab = |agent: AgentId, id: &str| SavedTab {
            agent,
            conversation_id: "c".to_string(),
            provider_session_id: id.to_string(),
            name: None,
            mode: Auto,
            view: None,
            window: None,
            turn_running: false,
            draft: None,
        };
        let bar = vec![
            (AgentId::Claude, "a".to_string()),
            (AgentId::Codex, "x".to_string()),
            (AgentId::Claude, "b".to_string()),
        ];
        let saved = SavedTabs::from_parts(
            vec![tab(AgentId::Claude, "a"), tab(AgentId::Claude, "b")],
            0,
            vec![tab(AgentId::Codex, "x")],
            None,
        );
        let records = [
            record("a", None, None),
            record("b", None, None),
            ResumableSession {
                provider: "codex".to_string(),
                ..record("x", None, None)
            },
        ];
        let ids = |saved: &SavedTabs| -> Vec<String> {
            plan(saved, &records, &[], |_, _| false)
                .items
                .iter()
                .map(|i| i.provider_session_id.clone())
                .collect()
        };
        assert_eq!(ids(&saved.clone().with_bar(bar)), ["a", "x", "b"]);
        assert_eq!(
            ids(&saved),
            ["a", "b", "x"],
            "no record of the order: Claude's tabs first"
        );
    }

    /// The bar reorders the plan: each tab keeps what was typed for it, a skipped one too.
    #[test]
    fn a_draft_stays_with_its_tab_through_the_tab_bars_order() {
        let tab = |agent: AgentId, id: &str| SavedTab {
            agent,
            conversation_id: "c".to_string(),
            provider_session_id: id.to_string(),
            name: None,
            mode: Auto,
            view: None,
            window: None,
            turn_running: false,
            draft: Some(format!("typed for {id}")),
        };
        let saved = SavedTabs::from_parts(
            vec![tab(AgentId::Claude, "a"), tab(AgentId::Claude, "gone")],
            0,
            vec![tab(AgentId::Codex, "x")],
            None,
        )
        .with_bar(vec![
            (AgentId::Codex, "x".to_string()),
            (AgentId::Claude, "gone".to_string()),
            (AgentId::Claude, "a".to_string()),
        ]);
        let records = [
            record("a", None, None),
            ResumableSession {
                provider: "codex".to_string(),
                ..record("x", None, None)
            },
        ];
        let plan = plan(&saved, &records, &[], |_, _| false);
        let planned: Vec<(&str, Option<&str>)> = plan
            .items
            .iter()
            .map(|i| (i.provider_session_id.as_str(), i.draft.as_deref()))
            .collect();
        assert_eq!(planned, [("x", Some("typed for x")), ("a", Some("typed for a"))]);
        let kept = plan.skipped[0].kept.as_ref().expect("the skipped tab's draft is kept");
        assert_eq!(kept.draft.as_deref(), Some("typed for gone"));
    }

    #[test]
    fn every_saved_tab_with_a_record_and_no_holder_comes_back_in_order() {
        let saved = saved(&[("a", None, Auto), ("b", Some("api"), Bypass), ("c", None, Auto)], 1);
        let records = [
            record("c", None, Some("third")),
            record("a", None, Some("first")),
            record("b", Some("api"), None),
        ];
        let plan = plan(&saved, &records, &[], |_, _| false);
        let ids: Vec<&str> = plan.items.iter().map(|i| i.provider_session_id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"], "tab-bar order, not the records' own ranking");
        assert!(plan.skipped.is_empty());
        assert_eq!(plan.active_session.as_deref(), Some("b"));
        assert_eq!(plan.items[1].saved_mode, Bypass);
        assert_eq!(plan.items[1].name.as_deref(), Some("api"));
        assert_eq!(plan.items[0].label, "first");
    }

    #[test]
    fn a_tab_of_an_agent_no_longer_registered_is_skipped_with_its_reason() {
        let mut saved = saved(&[("a", None, Auto)], 0);
        let acp = |name: &str, id: &str| SavedTab {
            agent: AgentId::Acp(name.to_string()),
            conversation_id: "c".to_string(),
            provider_session_id: id.to_string(),
            name: None,
            mode: Auto,
            view: None,
            window: None,
            turn_running: false,
            draft: None,
        };
        saved.tabs.extend([acp("gem", "g-1"), acp("gone", "x-1")]);
        let acp_record = |name: &str, id: &str| ResumableSession {
            provider: format!("acp:{name}"),
            ..record(id, None, Some(id))
        };
        let records = [
            record("a", None, Some("first")),
            acp_record("gem", "g-1"),
            acp_record("gone", "x-1"),
        ];
        let mut plan = plan(&saved, &records, &[], |_, _| false);
        plan.skip_unregistered(|agent| agent != &AgentId::Acp("gone".to_string()));
        let ids: Vec<&str> = plan.items.iter().map(|i| i.provider_session_id.as_str()).collect();
        assert_eq!(ids, ["a", "g-1"]);
        assert_eq!(
            plan.skipped,
            vec![Skipped {
                label: "x-1".to_string(),
                reason: NOT_REGISTERED.to_string(),
                kept: None,
            }]
        );
    }

    #[test]
    fn a_missing_record_and_a_session_held_elsewhere_are_each_skipped_with_their_reason() {
        let saved = saved(
            &[
                ("gone-0123456789", None, Auto),
                ("held", Some("busy"), Auto),
                ("fine", None, Auto),
            ],
            0,
        );
        let records = [record("held", Some("busy"), None), record("fine", None, Some("ok"))];
        let plan = plan(&saved, &records, &[], |_, id| id == "held");
        assert_eq!(plan.items.len(), 1);
        assert_eq!(
            plan.skipped,
            vec![
                Skipped {
                    label: "gone-012".to_string(),
                    reason: GONE.to_string(),
                    kept: None,
                },
                Skipped {
                    label: "busy".to_string(),
                    reason: HELD_ELSEWHERE.to_string(),
                    kept: None,
                },
            ],
            "the short id stands in for a tab with nothing else to call it"
        );
        assert_eq!(
            plan.active_session.as_deref(),
            Some("gone-0123456789"),
            "the saved active tab, though it is skipped"
        );
    }

    #[test]
    fn a_planned_tab_carries_its_saved_view() {
        let mut saved = saved(
            &[
                ("a", None, Auto),
                ("b", None, Auto),
                ("c", None, Auto),
                ("d", None, Auto),
            ],
            0,
        );
        saved.tabs[1].view = Some(3);
        saved.tabs[2].view = Some(1);
        saved.tabs[3].view = Some(42);
        saved.tabs.push(SavedTab {
            agent: AgentId::Codex,
            conversation_id: "c".to_string(),
            provider_session_id: "x".to_string(),
            name: None,
            mode: Auto,
            view: Some(2),
            window: None,
            turn_running: false,
            draft: None,
        });
        let records = [
            record("a", None, None),
            record("b", None, None),
            record("c", None, None),
            record("d", None, None),
            ResumableSession {
                provider: AgentId::Codex.record_name(),
                ..record("x", None, None)
            },
        ];
        let plan = plan(&saved, &records, &[], |_, _| false);
        let views: Vec<(&str, Option<u8>)> = plan
            .items
            .iter()
            .map(|i| (i.provider_session_id.as_str(), i.view.map(ViewId::number)))
            .collect();
        assert_eq!(
            views,
            [("a", None), ("b", Some(3)), ("c", None), ("d", None), ("x", Some(2))],
            "the base view and a number no chat view has are both no view"
        );
    }

    #[test]
    fn a_session_this_window_already_has_open_is_left_out_without_comment() {
        let saved = saved(&[("open", None, Auto), ("other", None, Auto)], 0);
        let records = [record("open", None, None), record("other", None, None)];
        let plan = plan(&saved, &records, &[(AgentId::Claude, "open".to_string())], |_, _| false);
        assert_eq!(plan.items.len(), 1);
        assert!(plan.skipped.is_empty());
    }

    /// Session ids are each agent's own: an open tab of one agent does not stand for another agent's
    /// saved tab that happens to carry the same id, which still comes back.
    #[test]
    fn a_saved_tab_of_another_agent_with_an_open_tabs_id_is_still_restored() {
        let gemini = AgentId::Acp("gemini".to_string());
        let mut saved = saved(&[("shared", None, Auto)], 0);
        saved.tabs.push(SavedTab {
            agent: AgentId::Acp("claude-code".to_string()),
            conversation_id: "c".to_string(),
            provider_session_id: "shared".to_string(),
            name: None,
            mode: Auto,
            view: None,
            window: None,
            turn_running: false,
            draft: None,
        });
        let acp_record = |name: &str| ResumableSession {
            provider: format!("acp:{name}"),
            ..record("shared", None, Some(name))
        };
        let records = [
            record("shared", None, None),
            acp_record("claude-code"),
            acp_record("gemini"),
        ];
        let open = [(gemini, "shared".to_string())];
        let plan = plan(&saved, &records, &open, |_, _| false);
        let planned: Vec<(String, &str)> = plan
            .items
            .iter()
            .map(|i| (i.agent.record_name(), i.provider_session_id.as_str()))
            .collect();
        assert_eq!(
            planned,
            [
                ("claude".to_string(), "shared"),
                ("acp:claude-code".to_string(), "shared")
            ],
            "the open session is the gemini agent's, neither saved tab's"
        );
        assert!(plan.skipped.is_empty());

        // The same agent's session open here is still left out.
        let open = [(AgentId::Acp("claude-code".to_string()), "shared".to_string())];
        let plan = super::plan(&saved, &records, &open, |_, _| false);
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].agent, AgentId::Claude);

        // And a record is hidden only by its own agent's open session.
        assert!(is_open(&open, &acp_record("claude-code")));
        assert!(!is_open(&open, &acp_record("gemini")));
        assert!(!is_open(&open, &record("shared", None, None)));
    }

    /// Only a tab that can come back is told of: a turn whose session has gone has nothing to show for it here.
    #[test]
    fn the_offer_says_how_many_of_its_tabs_had_a_turn_cut_off() {
        let mut saved = saved(&[("a", None, Auto), ("b", None, Auto), ("gone", None, Auto)], 0);
        let records = [record("a", None, None), record("b", None, None)];
        let note = |saved: &SavedTabs| plan(saved, &records, &[], |_, _| false).offer().unwrap().cut_note;
        assert_eq!(note(&saved), None);

        saved.tabs[2].turn_running = true;
        assert_eq!(note(&saved), None, "its session has gone, so it is not on offer");

        saved.tabs[1].turn_running = true;
        assert_eq!(note(&saved).unwrap(), "a turn was cut off when this window last ended");

        saved.tabs[0].turn_running = true;
        assert_eq!(
            note(&saved).unwrap(),
            "2 turns were cut off when this window last ended"
        );
    }

    #[test]
    fn a_rename_saved_after_the_record_was_read_still_names_the_tab() {
        let saved = saved(&[("a", Some("newer"), Auto)], 0);
        let plan = plan(&saved, &[record("a", None, Some("title"))], &[], |_, _| false);
        assert_eq!(plan.items[0].label, "newer");
        let plan = super::plan(&saved, &[record("a", Some("recorded"), None)], &[], |_, _| false);
        assert_eq!(plan.items[0].label, "recorded", "the record's own rename is the tab's");
    }

    #[test]
    fn the_offer_names_the_tabs_and_counts_the_bypass_ones_and_is_none_when_nothing_would_come_back() {
        let saved = saved(
            &[("a", Some("one"), Auto), ("b", Some("two"), Bypass), ("x", None, Auto)],
            0,
        );
        let records = [record("a", Some("one"), None), record("b", Some("two"), None)];
        let offer = plan(&saved, &records, &[], |_, _| false).offer().unwrap();
        assert_eq!(
            offer,
            RestoreOffer {
                labels: vec!["one".to_string(), "two".to_string()],
                bypass: 1,
                cut_note: None,
            }
        );

        let nothing = plan(&saved, &[], &[], |_, _| false);
        assert_eq!(nothing.offer(), None, "every tab would be skipped");
        let held = plan(&saved, &records, &[], |_, _| true);
        assert_eq!(held.offer(), None, "every session is held elsewhere");
    }

    fn run_of(tabs: &[(u64, &str)], skipped: Vec<Skipped>) -> RestoreRun {
        let mut run = RestoreRun::new(
            TabId(tabs[0].0),
            Auto,
            AgentId::Claude,
            tabs.len() + skipped.len(),
            skipped,
        );
        for (id, label) in tabs {
            run.pending.insert(TabId(*id), label.to_string());
        }
        run
    }

    #[test]
    fn a_restore_is_over_only_when_every_resume_has_returned() {
        let mut run = run_of(&[(1, "a"), (2, "b")], Vec::new());
        assert!(run.owns(TabId(1)) && run.owns(TabId(2)) && !run.owns(TabId(3)));
        assert_eq!(run.outcome(), None);
        run.note_installed(TabId(1));
        assert!(!run.owns(TabId(1)));
        assert_eq!(run.outcome(), None, "one is still connecting");
        run.note_installed(TabId(2));
        assert_eq!(run.outcome().unwrap().message(), "Restored 2 tabs");
        run.note_installed(TabId(2));
        assert_eq!(run.outcome().unwrap().restored, 2, "a repeat counts nothing");
    }

    #[test]
    fn the_message_says_how_many_came_back_and_names_each_that_did_not_and_why() {
        let skipped = vec![Skipped {
            label: "docs".to_string(),
            reason: HELD_ELSEWHERE.to_string(),
            kept: None,
        }];
        let mut run = run_of(&[(1, "api"), (2, "web")], skipped);
        run.note_installed(TabId(1));
        run.note_failed(TabId(2), "the provider refused it");
        assert_eq!(
            run.outcome().unwrap().message(),
            "Restored 1 of 3 tabs; 2 could not be: docs (open in another window), web (the provider refused it)"
        );

        let mut one = run_of(&[(1, "api")], Vec::new());
        one.note_installed(TabId(1));
        assert_eq!(one.outcome().unwrap().message(), "Restored 1 tab");

        let mut lost = run_of(&[(1, "api"), (2, "web")], Vec::new());
        lost.note_failed(TabId(1), "no such session");
        lost.note_installed(TabId(2));
        assert_eq!(
            lost.outcome().unwrap().message(),
            "Restored 1 of 2 tabs; 1 could not be: api (no such session)"
        );
    }

    #[test]
    fn a_tab_the_user_closed_while_it_connected_is_neither_restored_nor_failed() {
        let mut run = run_of(&[(1, "a"), (2, "b"), (3, "c")], Vec::new());
        run.note_installed(TabId(1));
        run.forget_closed(|tab| tab != TabId(2));
        assert!(!run.owns(TabId(2)));
        assert_eq!(run.outcome(), None, "c is still connecting");
        run.note_installed(TabId(3));
        assert_eq!(
            run.outcome().unwrap().message(),
            "Restored 2 tabs",
            "b was closed, not lost"
        );
    }

    #[test]
    fn the_first_survivor_is_the_first_started_tab_still_there() {
        let mut run = run_of(&[(1, "a"), (2, "b"), (3, "c")], Vec::new());
        run.order = vec![TabId(1), TabId(2), TabId(3)];
        assert_eq!(run.first_survivor(), Some(TabId(1)));
        run.note_failed(TabId(1), "refused");
        assert_eq!(run.first_survivor(), Some(TabId(2)), "a failed tab is not a survivor");
        run.forget_closed(|tab| tab != TabId(2));
        assert_eq!(run.first_survivor(), Some(TabId(3)), "nor is one the user closed");
        run.note_installed(TabId(3));
        assert_eq!(run.first_survivor(), Some(TabId(3)), "an installed one is");
        run.note_failed(TabId(3), "x");
        assert_eq!(
            run.first_survivor(),
            Some(TabId(3)),
            "an installed tab never fails afterwards"
        );
    }

    #[test]
    fn a_failed_first_tab_goes_back_to_the_mode_it_had_and_a_made_one_goes_away() {
        let mut run = RestoreRun::new(TabId(1), Auto, AgentId::Claude, 2, Vec::new());
        run.pending.insert(TabId(1), "a".to_string());
        assert_eq!(run.back_to(TabId(1)), Some((Auto, AgentId::Claude)));
        assert_eq!(run.back_to(TabId(2)), None);
        let bypass = RestoreRun::new(TabId(4), Bypass, AgentId::Codex, 1, Vec::new());
        assert_eq!(bypass.back_to(TabId(4)), Some((Bypass, AgentId::Codex)));
    }

    #[test]
    fn a_restore_with_nothing_started_is_over_at_once() {
        let skipped = vec![Skipped {
            label: "docs".to_string(),
            reason: GONE.to_string(),
            kept: None,
        }];
        let run = RestoreRun::new(TabId(1), Auto, AgentId::Claude, 1, skipped);
        assert_eq!(
            run.outcome().unwrap().message(),
            "Restored 0 of 1 tab; 1 could not be: docs (its saved record is gone)"
        );
    }
}
