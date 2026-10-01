//! Bringing a window's saved tabs back (`saved_tabs` holds what was open; `TabSet::start_restore`
//! starts them): what `agent.restore` may say, which saved tabs can still be brought back, what the
//! launch dashboard offers, and the tally that ends in one message once every resume has returned.
//!
//! A saved tab is skipped, and said to be skipped, when its record is gone or no longer resumable,
//! when another window holds its session, or when the resume itself fails. Nothing here ever fails a
//! launch.

use std::collections::BTreeMap;

use agent::ResumableSession;

use crate::agent_bridge::SessionModeChoice;
use crate::saved_tabs::SavedTabs;
use crate::tabs::{label_name, TabId};

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

/// The `init.lua` key: `eitri.config.set("agent.restore", "offer" | "auto" | "off")`.
pub const RESTORE_KEY: &str = "agent.restore";

impl RestorePolicy {
    /// The value of [`RESTORE_KEY`] as `init.lua` left it: unset is [`RestorePolicy::Offer`]; anything
    /// but the three words is an error naming the key, as every other configuration key here is.
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.map(str::trim) {
            None | Some("offer") => Ok(RestorePolicy::Offer),
            Some("auto") => Ok(RestorePolicy::Auto),
            Some("off") => Ok(RestorePolicy::Off),
            Some(_) => Err(format!(
                "eitri.config.set(\"{RESTORE_KEY}\", {:?}): must be \"offer\", \"auto\" or \"off\"",
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
}

/// A saved tab that can be resumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedTab {
    pub provider_session_id: String,
    pub name: Option<String>,
    pub title: Option<String>,
    /// The mode the tab was saved in. What the restored tab gets is decided by
    /// `TabSet::begin_restore`, never here.
    pub saved_mode: SessionModeChoice,
    pub label: String,
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

/// Sorts `saved` into what can be resumed and what cannot. `records` are the sessions this project
/// can resume (`agent::resumable_sessions`, with their display titles), `open` the sessions this
/// window already has open (left out without comment: they are already where they should be), and
/// `held` says whether a lease on a session is held right now.
pub fn plan(
    saved: &SavedTabs,
    records: &[ResumableSession],
    open: &[String],
    held: impl Fn(&str) -> bool,
) -> RestorePlan {
    let mut items = Vec::new();
    let mut skipped = Vec::new();
    for tab in &saved.tabs {
        let id = tab.provider_session_id.as_str();
        if open.iter().any(|o| o == id) {
            continue;
        }
        let Some(record) = records.iter().find(|r| r.provider_session_id == id) else {
            skipped.push(Skipped {
                label: label_name(tab.name.as_deref(), None, Some(id)),
                reason: GONE.to_string(),
            });
            continue;
        };
        // The record carries the rename the tab had (a rename is written to it); the saved copy only
        // covers a rename that was made after the record was last read.
        let name = record.name.clone().or_else(|| tab.name.clone());
        let label = label_name(name.as_deref(), record.title.as_deref(), Some(id));
        if held(id) {
            skipped.push(Skipped {
                label,
                reason: HELD_ELSEWHERE.to_string(),
            });
            continue;
        }
        items.push(PlannedTab {
            provider_session_id: id.to_string(),
            name,
            title: record.title.clone(),
            saved_mode: tab.mode,
            label,
        });
    }
    RestorePlan {
        items,
        skipped,
        active_session: saved.tabs.get(saved.active).map(|t| t.provider_session_id.clone()),
    }
}

/// What the launch dashboard says about the tabs it can bring back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreOffer {
    /// One per tab that would come back, in tab-bar order.
    pub labels: Vec<String>,
    /// How many of them were in bypass.
    pub bypass: usize,
}

impl RestorePlan {
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
    pub(crate) pending: BTreeMap<TabId, String>,
    /// The tabs the restore started, in saved order, until one fails or is closed: where the screen
    /// goes when the tab it was on did not come back.
    pub(crate) order: Vec<TabId>,
    pub(crate) restored: usize,
    pub(crate) failed: Vec<Skipped>,
    pub(crate) total: usize,
}

impl RestoreRun {
    pub(crate) fn new(first: TabId, first_mode: SessionModeChoice, total: usize, skipped: Vec<Skipped>) -> Self {
        RestoreRun {
            first,
            first_mode,
            pending: BTreeMap::new(),
            order: Vec::new(),
            restored: 0,
            failed: skipped,
            total,
        }
    }

    /// Whether `tab` is one of this restore's, still connecting.
    pub fn owns(&self, tab: TabId) -> bool {
        self.pending.contains_key(&tab)
    }

    /// Whether `tab` is the one the restore began in.
    pub fn began_in(&self, tab: TabId) -> bool {
        self.first == tab
    }

    /// What `tab` goes back to if its resume fails: `Some(mode)`, an empty tab again in the mode it
    /// had, when it is the one the restore began in; `None`, closed, when the restore made it.
    pub fn back_to(&self, tab: TabId) -> Option<SessionModeChoice> {
        self.began_in(tab).then_some(self.first_mode)
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
    use crate::saved_tabs::SavedTab;

    fn saved(tabs: &[(&str, Option<&str>, SessionModeChoice)], active: usize) -> SavedTabs {
        SavedTabs {
            tabs: tabs
                .iter()
                .map(|(id, name, mode)| SavedTab {
                    conversation_id: "c".to_string(),
                    provider_session_id: id.to_string(),
                    name: name.map(str::to_string),
                    mode: *mode,
                })
                .collect(),
            active,
        }
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
    fn the_policy_reads_its_three_words_and_nothing_else() {
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

    #[test]
    fn every_saved_tab_with_a_record_and_no_holder_comes_back_in_order() {
        let saved = saved(&[("a", None, Auto), ("b", Some("api"), Bypass), ("c", None, Auto)], 1);
        let records = [
            record("c", None, Some("third")),
            record("a", None, Some("first")),
            record("b", Some("api"), None),
        ];
        let plan = plan(&saved, &records, &[], |_| false);
        let ids: Vec<&str> = plan.items.iter().map(|i| i.provider_session_id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"], "tab-bar order, not the records' own ranking");
        assert!(plan.skipped.is_empty());
        assert_eq!(plan.active_session.as_deref(), Some("b"));
        assert_eq!(plan.items[1].saved_mode, Bypass);
        assert_eq!(plan.items[1].name.as_deref(), Some("api"));
        assert_eq!(plan.items[0].label, "first");
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
        let plan = plan(&saved, &records, &[], |id| id == "held");
        assert_eq!(plan.items.len(), 1);
        assert_eq!(
            plan.skipped,
            vec![
                Skipped {
                    label: "gone-012".to_string(),
                    reason: GONE.to_string()
                },
                Skipped {
                    label: "busy".to_string(),
                    reason: HELD_ELSEWHERE.to_string()
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
    fn a_session_this_window_already_has_open_is_left_out_without_comment() {
        let saved = saved(&[("open", None, Auto), ("other", None, Auto)], 0);
        let records = [record("open", None, None), record("other", None, None)];
        let plan = plan(&saved, &records, &["open".to_string()], |_| false);
        assert_eq!(plan.items.len(), 1);
        assert!(plan.skipped.is_empty());
    }

    #[test]
    fn a_rename_saved_after_the_record_was_read_still_names_the_tab() {
        let saved = saved(&[("a", Some("newer"), Auto)], 0);
        let plan = plan(&saved, &[record("a", None, Some("title"))], &[], |_| false);
        assert_eq!(plan.items[0].label, "newer");
        let plan = super::plan(&saved, &[record("a", Some("recorded"), None)], &[], |_| false);
        assert_eq!(plan.items[0].label, "recorded", "the record's own rename is the tab's");
    }

    #[test]
    fn the_offer_names_the_tabs_and_counts_the_bypass_ones_and_is_none_when_nothing_would_come_back() {
        let saved = saved(
            &[("a", Some("one"), Auto), ("b", Some("two"), Bypass), ("x", None, Auto)],
            0,
        );
        let records = [record("a", Some("one"), None), record("b", Some("two"), None)];
        let offer = plan(&saved, &records, &[], |_| false).offer().unwrap();
        assert_eq!(
            offer,
            RestoreOffer {
                labels: vec!["one".to_string(), "two".to_string()],
                bypass: 1
            }
        );

        let nothing = plan(&saved, &[], &[], |_| false);
        assert_eq!(nothing.offer(), None, "every tab would be skipped");
        let held = plan(&saved, &records, &[], |_| true);
        assert_eq!(held.offer(), None, "every session is held elsewhere");
    }

    fn run_of(tabs: &[(u64, &str)], skipped: Vec<Skipped>) -> RestoreRun {
        let mut run = RestoreRun::new(TabId(tabs[0].0), Auto, tabs.len() + skipped.len(), skipped);
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
        let mut run = RestoreRun::new(TabId(1), Auto, 2, Vec::new());
        run.pending.insert(TabId(1), "a".to_string());
        assert_eq!(run.back_to(TabId(1)), Some(Auto));
        assert_eq!(run.back_to(TabId(2)), None);
        let bypass = RestoreRun::new(TabId(4), Bypass, 1, Vec::new());
        assert_eq!(bypass.back_to(TabId(4)), Some(Bypass));
    }

    #[test]
    fn a_restore_with_nothing_started_is_over_at_once() {
        let skipped = vec![Skipped {
            label: "docs".to_string(),
            reason: GONE.to_string(),
        }];
        let run = RestoreRun::new(TabId(1), Auto, 1, skipped);
        assert_eq!(
            run.outcome().unwrap().message(),
            "Restored 0 of 1 tab; 1 could not be: docs (its saved record is gone)"
        );
    }
}
