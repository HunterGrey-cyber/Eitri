//! Session tabs' pure rules (docs/superpowers/specs/2026-09-25-keymap-tabs-panel-design.md §3):
//! numbering, names and labels, markers, which `n`/`p` a key reaches, how a tab closes, and what
//! the window owes the user across tabs. GTK-free and backend-free: `crate::tab_set` holds the
//! tabs themselves and asks these functions every question that has one right answer.

use crate::attention::{AgentPlace, Attention};
use crate::layout::ModuleKind;

/// A tab's identity for its whole life, and the bridge's: monotonic, never reused (spec §3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TabId(pub u64);

/// A rename longer than this is cut (spec §3.9, ruling 11).
pub const NAME_MAX_CHARS: usize = 40;
/// A title in a label is cut to this many characters, the last one `…` (spec §3.2, ruling 11).
pub const LABEL_TITLE_CHARS: usize = 18;
/// The short Claude id a label falls back to.
pub const SHORT_ID_CHARS: usize = 8;

/// The number a new tab takes: the lowest one no open tab has, from 1. tmux without
/// `renumber-windows`: numbers never shift when a tab closes (spec §3.2).
pub fn lowest_free_number(used: &[u16]) -> u16 {
    (1..=u16::MAX).find(|n| !used.contains(n)).unwrap_or(u16::MAX)
}

/// A rename as stored: trimmed, `None` when empty, cut to `NAME_MAX_CHARS` characters.
pub fn normalize_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(NAME_MAX_CHARS).collect())
}

fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max - 1).collect();
    format!("{kept}…")
}

/// What a label calls the tab: the rename, else the title (cut), else the short Claude id, else
/// `new` (spec §3.2). Claude Code keeps a set name apart from the generated title the same way.
pub fn label_name(name: Option<&str>, title: Option<&str>, provider_session_id: Option<&str>) -> String {
    if let Some(name) = name.filter(|n| !n.is_empty()) {
        return name.to_string();
    }
    if let Some(title) = title.map(str::trim).filter(|t| !t.is_empty()) {
        return cut(title, LABEL_TITLE_CHARS);
    }
    if let Some(id) = provider_session_id.filter(|i| !i.is_empty()) {
        return id.chars().take(SHORT_ID_CHARS).collect();
    }
    "new".to_string()
}

/// `<n> <name>`.
pub fn label(number: u16, name_part: &str) -> String {
    format!("{number} {name_part}")
}

/// What a marker is decided from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TabFacts {
    pub pending: usize,
    pub working: bool,
    pub unread: bool,
    pub ended: bool,
}

/// One glyph per tab, from Claude Code's agent-view states (spec §3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    /// `⚑`, or `⚑N`.
    NeedsInput(usize),
    /// A dim label and `✕`: the session ended, was lost, or never started.
    Ended,
    /// `TurnActivity`'s motion, `…` under reduced motion.
    Working,
    /// `•`.
    Unread,
}

impl Marker {
    /// The bridge's spelling (`TabMarker` in `agent-ui/web/src/types.ts`).
    pub fn wire(self) -> &'static str {
        match self {
            Marker::NeedsInput(_) => "needs_input",
            Marker::Ended => "ended",
            Marker::Working => "working",
            Marker::Unread => "unread",
        }
    }
}

/// The one marker a tab shows. Precedence `⚑` > `✕` > working > `•`.
pub fn marker(facts: TabFacts) -> Option<Marker> {
    if facts.pending > 0 {
        Some(Marker::NeedsInput(facts.pending))
    } else if facts.ended {
        Some(Marker::Ended)
    } else if facts.working {
        Some(Marker::Working)
    } else if facts.unread {
        Some(Marker::Unread)
    } else {
        None
    }
}

/// The tab `delta` steps from `current`, in number order, wrapping (ruling 10). `None` when
/// `current` is not one of `numbers`.
pub fn step(numbers: &[u16], current: u16, delta: i32) -> Option<u16> {
    let mut sorted = numbers.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let at = sorted.iter().position(|n| *n == current)? as i32;
    let next = (at + delta).rem_euclid(sorted.len() as i32);
    Some(sorted[next as usize])
}

/// What `prefix n`/`p` act on (D8, spec §3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextPrevTarget {
    /// The chat's tabs. From outside the chat, the keys stay where they are (D3 B).
    ChatTabs,
    /// The canvas's revisions, once the canvas exists (modules P6). Until then: a flash.
    CanvasRevisions,
    /// The terminal's terminals, once there is more than one.
    Terminals,
    /// One terminal: the key is taken and does nothing.
    Swallowed,
}

/// `keys_in` is the kind of the module holding the keys, `None` for the top bar.
pub fn next_prev_target(keys_in: Option<ModuleKind>, terminals: usize) -> NextPrevTarget {
    match keys_in {
        Some(ModuleKind::Canvas) => NextPrevTarget::CanvasRevisions,
        Some(ModuleKind::Terminal) if terminals > 1 => NextPrevTarget::Terminals,
        Some(ModuleKind::Terminal) => NextPrevTarget::Swallowed,
        Some(ModuleKind::Agent) | Some(ModuleKind::Editor) | Some(ModuleKind::LuaWebview) | None => {
            NextPrevTarget::ChatTabs
        }
    }
}

/// A tab changed from outside the chat: show a hidden chat where it was, keys unmoved, as
/// `modules.chat.on_permission = "reveal"` does. Under a zoom only the tray chip changes.
pub fn reveal_on_switch(place: AgentPlace) -> bool {
    place == AgentPlace::Hidden
}

/// What closing one tab involves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseFacts {
    pub number: u16,
    /// `label_name` of the tab, for the prompt.
    pub label_name: String,
    pub turn_running: bool,
    /// Always 0 until phase 3's queue.
    pub queued: usize,
    pub legacy: bool,
    /// Whether there is a backend to shut down (an empty tab has none).
    pub has_backend: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseStep {
    Interrupt,
    QueueToHistory,
    Shutdown,
    Remove,
}

/// `y` to `prefix &` runs these, in this order (spec §3.5). The record is never deleted.
pub fn close_steps(facts: &CloseFacts) -> Vec<CloseStep> {
    let mut steps = Vec::new();
    if facts.turn_running {
        steps.push(CloseStep::Interrupt);
    }
    if facts.queued > 0 {
        steps.push(CloseStep::QueueToHistory);
    }
    if facts.has_backend {
        steps.push(CloseStep::Shutdown);
    }
    steps.push(CloseStep::Remove);
    steps
}

/// Which prompt `TabSet::cycle_mode`/`cycle_default_mode` builds for a move into bypass (R06/S2,
/// D2): the text depends on whether the tab already has a session, or whether this is the window
/// default rather than any one tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptScope {
    /// A tab with a live or starting session: this entry only ever applies to that one tab.
    LiveTab,
    /// A `NotStarted` tab: entering bypass here also moves the window default (D13), so the text
    /// says so.
    EmptyTab,
    /// The chooser's own default, not any open tab.
    Default,
}

/// R06's four prompts, in English (K09, 2026-09-29). The band and every other prompt and flash it
/// draws (`close 2 "docs"? (y/n)`, `close window? ...`) are English; these four, written in the
/// owner's Chinese at first, and `bypass_staying_line` under them were the only text in it that was
/// not. R06's structure is unchanged: what is asked and when, and `(y/n)` ends every one. `waiting`
/// -- the number of delivered cards a `LiveTab` entry would approve -- is read only for `LiveTab`;
/// the other two scopes never have cards to approve (a `NotStarted` tab has no session and the
/// window default is not a tab at all).
pub fn bypass_prompt(scope: PromptScope, waiting: usize) -> String {
    match scope {
        PromptScope::LiveTab if waiting == 0 => "Switch to bypass? (y/n)".to_string(),
        PromptScope::LiveTab => {
            let cards = if waiting == 1 { "card" } else { "cards" };
            format!("Switch to bypass and approve the {waiting} waiting {cards}? (y/n)")
        }
        PromptScope::EmptyTab => "Switch to bypass? New sessions in this window start in bypass too (y/n)".to_string(),
        PromptScope::Default => "Start new sessions in bypass? (y/n)".to_string(),
    }
}

/// The consequence line under a bypass prompt when cards stay waiting after `y` (O3 review #6): a
/// CLI prompt the user's own ask rule forced, or one of a kind this build does not know, is a card
/// in bypass too, and `y` does not approve it -- so the prompt says so rather than leaving it to be
/// found afterwards. English like the prompt above it (K09); the verb agrees with the count.
pub fn bypass_staying_line(staying: usize) -> String {
    let (cards, stay) = if staying == 1 {
        ("card", "stays")
    } else {
        ("cards", "stay")
    };
    format!("{staying} {cards} your own ask rules force {stay} waiting after the switch")
}

/// tmux's `confirm-before`, plus a line for each consequence that applies (spec §3.5).
pub fn close_prompt(facts: &CloseFacts) -> Vec<String> {
    let mut lines = vec![format!("close {} \"{}\"? (y/n)", facts.number, facts.label_name)];
    if facts.turn_running {
        lines.push("a turn is running — it will be interrupted".to_string());
    }
    match facts.queued {
        0 => {}
        1 => lines.push("1 queued message goes to history".to_string()),
        n => lines.push(format!("{n} queued messages go to history")),
    }
    if facts.legacy {
        lines.push("not resumable (legacy backend)".to_string());
    }
    lines
}

/// D11 A: the window asks only if some tab is running or has a queued message (ruling 15).
pub fn window_close_prompt(running: usize, queued: usize) -> Option<String> {
    let mut parts = Vec::new();
    if running > 0 {
        parts.push(format!("{running} running"));
    }
    if queued > 0 {
        parts.push(format!("{queued} queued"));
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!("close window? {} (y/n)", parts.join(", ")))
}

/// `<leader>bo` (panel round 2 plan's Owner answers Q2, "yes"): every tab but `active`, and the one
/// y/n naming how many close and how many of them are running, e.g. `close 3 other tabs? 1 running
/// (y/n)`. `entries` is every open tab's id paired with whether it is running (a turn in progress,
/// or still connecting -- the same test `TabSet::running_count` uses). `None` when there is nothing
/// to close (the active tab is the only one open): the caller should not prompt at all, the same as
/// `window_close_prompt`'s `None` for "nothing running or queued".
pub fn close_others(entries: &[(TabId, bool)], active: TabId) -> Option<(Vec<TabId>, String)> {
    let others: Vec<(TabId, bool)> = entries.iter().copied().filter(|(id, _)| *id != active).collect();
    if others.is_empty() {
        return None;
    }
    let count = others.len();
    let running = others.iter().filter(|(_, running)| *running).count();
    let ids = others.into_iter().map(|(id, _)| id).collect();
    let plural = if count == 1 { "tab" } else { "tabs" };
    let mut prompt = format!("close {count} other {plural}?");
    if running > 0 {
        prompt.push_str(&format!(" {running} running"));
    }
    prompt.push_str(" (y/n)");
    Some((ids, prompt))
}

/// The tray's `agent ⚑N` sums every tab's cards and is unread if any tab is (spec §3.7).
pub fn sum_attention(each: &[Attention]) -> Attention {
    each.iter().fold(Attention::default(), |sum, a| Attention {
        pending: sum.pending + a.pending,
        unread: sum.unread || a.unread,
        arrived: sum.arrived + a.arrived,
    })
}

/// The tab holding the card that arrived first, of every card still pending (`prefix a`, spec §3.4).
pub fn oldest_card(tabs: &[(TabId, Option<u64>)]) -> Option<TabId> {
    tabs.iter()
        .filter_map(|(id, stamp)| stamp.map(|s| (s, *id)))
        .min()
        .map(|(_, id)| id)
}

/// The tab holding the most recent pending card (the toast names it).
pub fn newest_card(tabs: &[(TabId, Option<u64>)]) -> Option<TabId> {
    tabs.iter()
        .filter_map(|(id, stamp)| stamp.map(|s| (s, *id)))
        .max()
        .map(|(_, id)| id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attention::{AgentPlace, Attention};
    use crate::layout::ModuleKind;

    #[test]
    fn a_new_tab_takes_the_lowest_free_number_from_one() {
        assert_eq!(lowest_free_number(&[]), 1, "numbered from 1 (owner: 按1开始)");
        assert_eq!(lowest_free_number(&[1, 2, 3]), 4);
        assert_eq!(
            lowest_free_number(&[1, 3]),
            2,
            "a closed tab's number is reused, lowest first"
        );
        assert_eq!(lowest_free_number(&[2, 3]), 1);
    }

    #[test]
    fn a_rename_is_trimmed_cleared_when_empty_and_cut_at_forty_characters() {
        assert_eq!(normalize_name("  docs  "), Some("docs".to_string()));
        assert_eq!(normalize_name("   "), None, "an empty rename clears the name");
        assert_eq!(normalize_name(""), None);
        let long = "修".repeat(45);
        assert_eq!(
            normalize_name(&long).unwrap().chars().count(),
            40,
            "cut by character, not byte"
        );
    }

    #[test]
    fn the_label_takes_the_rename_then_the_title_then_the_short_id_then_new() {
        assert_eq!(
            label_name(Some("docs"), Some("fix the parser"), Some("1857dcd5-973b")),
            "docs"
        );
        assert_eq!(
            label_name(None, Some("fix the parser"), Some("1857dcd5-973b")),
            "fix the parser"
        );
        assert_eq!(label_name(None, None, Some("1857dcd5-973b")), "1857dcd5");
        assert_eq!(label_name(None, None, None), "new");
        assert_eq!(label_name(None, Some("  "), None), "new", "a blank title is no title");
        let cut = label_name(None, Some("rewrite the whole permission policy"), None);
        assert_eq!(cut.chars().count(), 18);
        assert!(cut.ends_with('…'), "{cut}");
        assert_eq!(label(2, "docs"), "2 docs");
    }

    #[test]
    fn markers_rank_needs_input_then_ended_then_working_then_unread() {
        let all = TabFacts {
            pending: 2,
            working: true,
            unread: true,
            ended: true,
        };
        assert_eq!(marker(all), Some(Marker::NeedsInput(2)));
        assert_eq!(marker(TabFacts { pending: 0, ..all }), Some(Marker::Ended));
        assert_eq!(
            marker(TabFacts {
                pending: 0,
                ended: false,
                ..all
            }),
            Some(Marker::Working)
        );
        assert_eq!(
            marker(TabFacts {
                pending: 0,
                ended: false,
                working: false,
                unread: true
            }),
            Some(Marker::Unread)
        );
        assert_eq!(marker(TabFacts::default()), None);
        assert_eq!(Marker::NeedsInput(1).wire(), "needs_input");
        assert_eq!(Marker::Ended.wire(), "ended");
        assert_eq!(Marker::Working.wire(), "working");
        assert_eq!(Marker::Unread.wire(), "unread");
    }

    #[test]
    fn next_and_previous_go_in_number_order_and_wrap() {
        let numbers = [3, 1, 2, 12];
        assert_eq!(step(&numbers, 1, 1), Some(2));
        assert_eq!(step(&numbers, 3, 1), Some(12), "past 9 is reachable by n");
        assert_eq!(step(&numbers, 12, 1), Some(1), "wraps, as tmux's next-window does");
        assert_eq!(step(&numbers, 1, -1), Some(12));
        assert_eq!(step(&numbers, 7, 1), None, "not a tab of this window");
        assert_eq!(step(&[1], 1, 1), Some(1));
    }

    #[test]
    fn n_and_p_act_on_what_the_module_with_the_keys_owns() {
        use NextPrevTarget::*;
        assert_eq!(next_prev_target(Some(ModuleKind::Agent), 1), ChatTabs);
        assert_eq!(
            next_prev_target(Some(ModuleKind::Editor), 1),
            ChatTabs,
            "D3 B: from the editor, focus unmoved"
        );
        assert_eq!(next_prev_target(Some(ModuleKind::LuaWebview), 1), ChatTabs);
        assert_eq!(next_prev_target(None, 1), ChatTabs, "the top bar holds the keys");
        assert_eq!(next_prev_target(Some(ModuleKind::Canvas), 1), CanvasRevisions);
        assert_eq!(
            next_prev_target(Some(ModuleKind::Terminal), 1),
            Swallowed,
            "one terminal: swallowed"
        );
        assert_eq!(next_prev_target(Some(ModuleKind::Terminal), 2), Terminals);
    }

    #[test]
    fn a_switch_from_elsewhere_reveals_a_hidden_chat_but_not_under_a_zoom() {
        assert!(reveal_on_switch(AgentPlace::Hidden));
        assert!(
            !reveal_on_switch(AgentPlace::HiddenUnderZoom),
            "only the tray chip changes"
        );
        assert!(!reveal_on_switch(AgentPlace::ZoomedAway));
        assert!(!reveal_on_switch(AgentPlace::OnScreen));
    }

    fn facts(turn_running: bool, queued: usize, legacy: bool) -> CloseFacts {
        CloseFacts {
            number: 2,
            label_name: "docs".into(),
            turn_running,
            queued,
            legacy,
            has_backend: true,
        }
    }

    #[test]
    fn closing_interrupts_then_queues_to_history_then_shuts_down_then_removes() {
        use CloseStep::*;
        assert_eq!(
            close_steps(&facts(true, 2, false)),
            vec![Interrupt, QueueToHistory, Shutdown, Remove]
        );
        assert_eq!(close_steps(&facts(false, 0, false)), vec![Shutdown, Remove]);
        let empty = CloseFacts {
            has_backend: false,
            ..facts(false, 0, false)
        };
        assert_eq!(
            close_steps(&empty),
            vec![Remove],
            "an empty tab has nothing to shut down"
        );
    }

    #[test]
    fn the_close_prompt_says_only_what_applies() {
        assert_eq!(close_prompt(&facts(false, 0, false)), vec!["close 2 \"docs\"? (y/n)"]);
        assert_eq!(
            close_prompt(&facts(true, 2, true)),
            vec![
                "close 2 \"docs\"? (y/n)",
                "a turn is running — it will be interrupted",
                "2 queued messages go to history",
                "not resumable (legacy backend)",
            ]
        );
        assert_eq!(
            close_prompt(&facts(false, 1, false))[1],
            "1 queued message goes to history"
        );
    }

    /// K09: R06's four prompts, in English like the rest of the band, byte for byte -- one card is
    /// "the 1 waiting card" and two are "the 2 waiting cards" -- and none carries any non-ASCII
    /// character (the Chinese ones' full-width `？` included).
    #[test]
    fn the_bypass_prompts_are_english_and_pluralize_the_card_count() {
        assert_eq!(bypass_prompt(PromptScope::LiveTab, 0), "Switch to bypass? (y/n)");
        assert_eq!(
            bypass_prompt(PromptScope::LiveTab, 1),
            "Switch to bypass and approve the 1 waiting card? (y/n)"
        );
        assert_eq!(
            bypass_prompt(PromptScope::LiveTab, 2),
            "Switch to bypass and approve the 2 waiting cards? (y/n)"
        );
        assert_eq!(
            bypass_prompt(PromptScope::EmptyTab, 0),
            "Switch to bypass? New sessions in this window start in bypass too (y/n)"
        );
        assert_eq!(
            bypass_prompt(PromptScope::Default, 0),
            "Start new sessions in bypass? (y/n)"
        );
        for scope in [PromptScope::LiveTab, PromptScope::EmptyTab, PromptScope::Default] {
            for waiting in [0, 1, 2, 12] {
                let text = bypass_prompt(scope, waiting);
                assert!(text.is_ascii(), "{scope:?}/{waiting}: {text:?}");
                assert!(text.ends_with("(y/n)"), "{scope:?}/{waiting}: {text:?}");
            }
        }
    }

    /// Only a live tab has cards to approve: the other two scopes read the same whatever count they
    /// are handed (a `NotStarted` tab has no session and the window default is no tab at all).
    #[test]
    fn only_a_live_tabs_prompt_reads_the_card_count() {
        for waiting in [0, 1, 2, 12] {
            assert_eq!(
                bypass_prompt(PromptScope::EmptyTab, waiting),
                bypass_prompt(PromptScope::EmptyTab, 0)
            );
            assert_eq!(
                bypass_prompt(PromptScope::Default, waiting),
                bypass_prompt(PromptScope::Default, 0)
            );
        }
        assert_eq!(
            bypass_prompt(PromptScope::LiveTab, 12),
            "Switch to bypass and approve the 12 waiting cards? (y/n)"
        );
    }

    /// The consequence line under the prompt (O3 review #6), in English and agreeing with its count:
    /// one card "stays", several "stay".
    #[test]
    fn the_staying_line_agrees_with_its_count() {
        assert_eq!(
            bypass_staying_line(1),
            "1 card your own ask rules force stays waiting after the switch"
        );
        assert_eq!(
            bypass_staying_line(2),
            "2 cards your own ask rules force stay waiting after the switch"
        );
        for staying in [1, 2, 7] {
            assert!(bypass_staying_line(staying).is_ascii(), "{staying}");
        }
    }

    #[test]
    fn the_window_asks_only_when_something_is_running_or_queued() {
        assert_eq!(window_close_prompt(0, 0), None);
        assert_eq!(
            window_close_prompt(2, 1).as_deref(),
            Some("close window? 2 running, 1 queued (y/n)")
        );
        assert_eq!(
            window_close_prompt(2, 0).as_deref(),
            Some("close window? 2 running (y/n)")
        );
        assert_eq!(
            window_close_prompt(0, 3).as_deref(),
            Some("close window? 3 queued (y/n)")
        );
    }

    #[test]
    fn close_others_names_the_count_and_how_many_run_and_never_includes_the_active_tab() {
        let entries = [
            (TabId(1), false),
            (TabId(2), true),
            (TabId(3), false),
            (TabId(4), false),
        ];
        let (ids, prompt) = close_others(&entries, TabId(1)).unwrap();
        assert_eq!(ids, vec![TabId(2), TabId(3), TabId(4)], "never the active tab");
        assert_eq!(prompt, "close 3 other tabs? 1 running (y/n)");
    }

    #[test]
    fn close_others_omits_the_running_clause_when_none_are_and_singularizes_one() {
        let entries = [(TabId(1), false), (TabId(2), false)];
        let (ids, prompt) = close_others(&entries, TabId(1)).unwrap();
        assert_eq!(ids, vec![TabId(2)]);
        assert_eq!(prompt, "close 1 other tab? (y/n)");
    }

    #[test]
    fn close_others_is_none_with_only_one_tab_open() {
        assert_eq!(close_others(&[(TabId(1), false)], TabId(1)), None);
    }

    #[test]
    fn attention_sums_cards_and_is_unread_if_any_tab_is() {
        let a = Attention {
            pending: 1,
            unread: false,
            arrived: 3,
        };
        let b = Attention {
            pending: 2,
            unread: true,
            arrived: 4,
        };
        assert_eq!(
            sum_attention(&[a, b]),
            Attention {
                pending: 3,
                unread: true,
                arrived: 7
            }
        );
        assert_eq!(sum_attention(&[]), Attention::default());
    }

    #[test]
    fn the_oldest_card_across_tabs_is_the_smallest_stamp() {
        let tabs = [(TabId(1), Some(9)), (TabId(2), None), (TabId(3), Some(4))];
        assert_eq!(oldest_card(&tabs), Some(TabId(3)));
        assert_eq!(newest_card(&tabs), Some(TabId(1)));
        assert_eq!(oldest_card(&[(TabId(1), None)]), None);
    }
}
