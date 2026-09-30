//! The GTK-side half of the panel's typing cadence (`eitri_core::panel_cadence`): every
//! envelope bound for the agent panel's WebView passes through one [`Pacer`] per WebView, which
//! decides -- with the pure rules from that module -- whether it goes now or waits for the next
//! slot, and keeps the wire order either way.
//!
//! **Where the funnel is.** `agent_panel::evaluate_js_dispatch` (the free function the whole
//! module dispatches through) and `AgentPanelHandle::dispatch` (the guarded twin for the focus,
//! HINT and arrival envelopes) both hand over whatever is waiting before they send, so nothing can
//! overtake a held envelope -- in particular no snapshot (a tab switch, a `ready`) can be applied
//! to the page before the events that came ahead of it, which would draw the same text twice
//! (the P1-A2 class of bug). Only the pump's own stream payload is ever held
//! ([`Pacer::send_stream`], classed by `TabSet::pump`).
//!
//! **What discards.** A document reload and a fresh `ready` throw the held envelopes away
//! ([`Pacer::discard_pending`]): the `ready` batch carries a snapshot read from canonical state,
//! which already holds everything they say. Sending them after it would draw it twice. A turn's
//! held first text is handed back to the caller for its trace, which would otherwise wait for a
//! paint report the snapshot's text never gets.
//!
//! **Not testable without a display**, and so not tested here: that `start_pump_timer` calls this
//! at all, and the WebKit frames the released envelopes then cause. The rules are all in this
//! file's tests, against a recording [`Sink`].

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use eitri_core::panel_cadence::{EnvelopeClass, PanelCadence, Route, DEFAULT_CADENCE_HZ};
use eitri_core::tabs::TabId;
use gtk4::glib;
use gtk4::prelude::*;
use webkit6::prelude::*;
use webkit6::WebView;

/// Where a released envelope goes: the page, or a recording stand-in in a test.
pub(crate) trait Sink {
    fn send(&self, payload: &str);
}

/// Whose first text a stream envelope carries: the tab it was pumped for and that tab's turn
/// (the trace's `submitted_at`). Kept with the envelope because by the time a flush releases it the
/// active tab, or the tab's turn, may be another one (fix round 1: a `tabs` dispatch after
/// `prefix n` used to hand tab A's stamp to tab B).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FirstText {
    pub tab: TabId,
    pub turn: Instant,
}

/// A held stream envelope, with what the caller needs when it is finally sent.
struct Paced {
    payload: String,
    /// It carries a turn's first text: the turn trace stamps "dispatched" when it really goes.
    first_text: Option<FirstText>,
}

/// What reached the sink since the caller last asked ([`Pacer::take_sent`]).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Sent {
    /// At least one stream envelope was sent.
    pub any: bool,
    /// The first texts among them, each with the moment it was handed to the page.
    pub first_text: Vec<(FirstText, Instant)>,
}

pub(crate) struct Pacer {
    cadence: PanelCadence<Paced>,
    /// Mirrored so `set_cadence` can carry it over to the new pacer.
    editor_has_keys: bool,
    /// What the page was last told about typing (see [`Pacer::poll`]).
    typing_told: bool,
    sent: Sent,
}

impl Pacer {
    pub(crate) fn new(cadence_hz: Option<u32>) -> Self {
        Self {
            cadence: PanelCadence::new(cadence_hz),
            editor_has_keys: false,
            typing_told: false,
            sent: Sent::default(),
        }
    }

    /// Replaces the cadence (after `init.lua` ran) keeping what is held and the focus state.
    pub(crate) fn set_cadence(&mut self, cadence_hz: Option<u32>) {
        let held = self.cadence.drain_pending();
        self.cadence = PanelCadence::new(cadence_hz);
        self.cadence.set_editor_has_keys(self.editor_has_keys);
        for item in held {
            self.cadence.push(item);
        }
    }

    /// A key was pressed in the editor and sent to nvim. O(1) and touches nothing else: it runs in
    /// the press's path.
    pub(crate) fn note_editor_key(&mut self, now: Instant) {
        self.cadence.note_editor_key(now);
    }

    /// Whether the editor holds the window's keys (`pane_focus`).
    pub(crate) fn set_editor_has_keys(&mut self, has: bool) {
        self.editor_has_keys = has;
        self.cadence.set_editor_has_keys(has);
    }

    fn transmit(&mut self, sink: &dyn Sink, item: Paced) {
        sink.send(&item.payload);
        self.sent.any = true;
        if let Some(first) = item.first_text {
            self.sent.first_text.push((first, Instant::now()));
        }
    }

    /// Sends everything held, oldest first. For an envelope that must not overtake them.
    pub(crate) fn flush(&mut self, sink: &dyn Sink) {
        for item in self.cadence.drain_pending() {
            self.transmit(sink, item);
        }
    }

    /// An envelope that is never delayed: whatever is held goes first, then this.
    pub(crate) fn send_immediate(&mut self, sink: &dyn Sink, payload: &str) {
        self.flush(sink);
        sink.send(payload);
    }

    /// The pump's stream payload, of `class`. Held for the next slot while the user types in the
    /// editor and the class allows it; otherwise sent now behind anything already held.
    pub(crate) fn send_stream(
        &mut self,
        sink: &dyn Sink,
        payload: String,
        first_text: Option<FirstText>,
        class: EnvelopeClass,
        now: Instant,
    ) {
        let item = Paced { payload, first_text };
        match self.cadence.route(class, now) {
            Route::SendNow => self.transmit(sink, item),
            Route::Pace => self.cadence.push(item),
            Route::FlushPendingThenSend => {
                self.flush(sink);
                self.transmit(sink, item);
            }
        }
    }

    /// The tick: releases what a slot or the end of the typing window has made due, then tells the
    /// page when typing starts or stops -- if its own motion needs to know
    /// ([`PanelCadence::page_needs_typing_state`]) and it has said `ready` (`page_ready`; until then
    /// the state is kept and told once it has).
    ///
    /// The typing message is sent on its own, not through [`send_immediate`](Self::send_immediate):
    /// it only sets an attribute, so it need not wait behind the stream, and handing the stream
    /// over with it would release the first envelopes of a burst the moment the burst begins.
    pub(crate) fn poll(&mut self, sink: &dyn Sink, now: Instant, page_ready: bool) {
        for item in self.cadence.due(now) {
            self.transmit(sink, item);
        }
        if !page_ready {
            return;
        }
        let typing = self.cadence.typing(now);
        if typing != self.typing_told {
            self.typing_told = typing;
            if let Some(period_ms) = self.cadence.page_needs_typing_state() {
                sink.send(&eitri_core::agent_bridge::serialize_editor_typing_for_js(
                    typing, period_ms,
                ));
            }
        }
    }

    /// Throws away what is held: the document is being replaced, and its `ready` batch carries a
    /// snapshot of everything that was. The page must be told about typing again.
    ///
    /// Returns the first-text markers of what was thrown away. That text will reach the new page
    /// inside the snapshot, which the page never reports a paint for, so the caller must tell each
    /// turn's trace (`agent_panel::trace_discarded_first_texts`) or its `EITRI_AGENT_TRACE` line
    /// waits for a report that is never sent and never prints.
    #[must_use = "the turn traces of the discarded first texts must be told (a line that never prints)"]
    pub(crate) fn discard_pending(&mut self) -> Vec<FirstText> {
        let held = self.cadence.drain_pending();
        self.typing_told = false;
        held.into_iter().filter_map(|item| item.first_text).collect()
    }

    /// What reached the sink from a stream payload since the last call.
    pub(crate) fn take_sent(&mut self) -> Sent {
        std::mem::take(&mut self.sent)
    }

    /// Only the first-text stamps of [`take_sent`](Self::take_sent), leaving `any` for the pump:
    /// for a `turn_rendered` report, which can arrive before the next tick has applied them.
    pub(crate) fn take_first_text(&mut self) -> Vec<(FirstText, Instant)> {
        std::mem::take(&mut self.sent.first_text)
    }
}

// ---- the WebView side -------------------------------------------------------------------------

/// The panel's page as a [`Sink`]: the script `agent_panel::evaluate_js_dispatch` always ran.
pub(crate) struct WebViewSink<'a>(pub(crate) &'a WebView);

impl Sink for WebViewSink<'_> {
    fn send(&self, payload: &str) {
        let script = format!(
            "window.__eitriDispatch({});",
            serde_json::to_string(payload).unwrap_or_default()
        );
        self.0
            .evaluate_javascript(&script, None, None, None::<&gtk4::gio::Cancellable>, |result| {
                if let Err(e) = result {
                    eprintln!("[agent_panel] evaluate_javascript failed: {e}");
                }
            });
    }
}

type Registry = Vec<(glib::WeakRef<WebView>, Rc<RefCell<Pacer>>)>;

thread_local! {
    /// One pacer per WebView, found by the WebView because the dispatch functions everywhere in
    /// `agent_panel` take only that. GTK's main thread is the only one that dispatches.
    static PACERS: RefCell<Registry> = const { RefCell::new(Vec::new()) };
}

/// `webview`'s pacer, made (at the default cadence) on first use. Entries whose WebView is gone are
/// dropped here, so an address reused by a later WebView never finds a stale pacer.
pub(crate) fn pacer_for(webview: &WebView) -> Rc<RefCell<Pacer>> {
    PACERS.with(|pacers| {
        let mut pacers = pacers.borrow_mut();
        pacers.retain(|(weak, _)| weak.upgrade().is_some());
        if let Some((_, pacer)) = pacers
            .iter()
            .find(|(weak, _)| weak.upgrade().is_some_and(|known| &known == webview))
        {
            return pacer.clone();
        }
        let pacer = Rc::new(RefCell::new(Pacer::new(Some(DEFAULT_CADENCE_HZ))));
        pacers.push((webview.downgrade(), pacer.clone()));
        pacer
    })
}

/// Sends `payload` to the page, never delayed, behind anything held (`Pacer::send_immediate`). This
/// is `agent_panel::evaluate_js_dispatch`. A re-entrant call -- the pacer is borrowed only across
/// plain sink sends, so none is expected -- goes straight out rather than panicking the tick.
pub(crate) fn send_immediate(webview: &WebView, payload: &str) {
    let pacer = pacer_for(webview);
    match pacer.try_borrow_mut() {
        Ok(mut pacer) => pacer.send_immediate(&WebViewSink(webview), payload),
        Err(_) => {
            eprintln!("[panel_pacer] BUG: the pacer was borrowed when an envelope was sent; sending it unordered");
            WebViewSink(webview).send(payload);
        }
    };
}

/// Hands the page whatever is held, for a caller that sends its own (guarded) script.
pub(crate) fn flush(webview: &WebView) {
    let pacer = pacer_for(webview);
    if let Ok(mut pacer) = pacer.try_borrow_mut() {
        pacer.flush(&WebViewSink(webview));
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[derive(Default)]
    struct Recording {
        sent: RefCell<Vec<String>>,
    }

    impl Sink for Recording {
        fn send(&self, payload: &str) {
            self.sent.borrow_mut().push(payload.to_string());
        }
    }

    impl Recording {
        fn take(&self) -> Vec<String> {
            std::mem::take(&mut *self.sent.borrow_mut())
        }
    }

    /// A pacer at `hz` whose editor holds the keys, with the origin instant.
    fn typing_pacer(hz: Option<u32>) -> (Pacer, Instant) {
        let mut pacer = Pacer::new(hz);
        pacer.set_editor_has_keys(true);
        (pacer, Instant::now())
    }

    fn stream(pacer: &mut Pacer, sink: &Recording, name: &str, now: Instant) {
        pacer.send_stream(sink, name.to_string(), None, EnvelopeClass::Stream, now);
    }

    fn typing_envelopes(sent: &[String]) -> Vec<&String> {
        sent.iter().filter(|p| p.contains("editor_typing")).collect()
    }

    #[test]
    fn stream_envelopes_wait_for_the_slot_while_typing_and_then_go_back_to_back_in_order() {
        let (mut pacer, t0) = typing_pacer(Some(8));
        let sink = Recording::default();
        pacer.note_editor_key(t0);
        stream(&mut pacer, &sink, "a", t0 + MS(10));
        stream(&mut pacer, &sink, "b", t0 + MS(40));
        pacer.poll(&sink, t0 + MS(66), true);
        assert!(sink.take().is_empty(), "held until the first slot at 125 ms");
        stream(&mut pacer, &sink, "c", t0 + MS(99));
        pacer.poll(&sink, t0 + MS(132), true);
        assert_eq!(sink.take(), vec!["a", "b", "c"], "whole, in order, one after another");
    }

    #[test]
    fn a_stream_envelope_outside_the_typing_window_goes_at_once() {
        let (mut pacer, t0) = typing_pacer(Some(8));
        let sink = Recording::default();
        stream(&mut pacer, &sink, "a", t0);
        assert_eq!(sink.take(), vec!["a"], "no key yet");
        pacer.note_editor_key(t0);
        pacer.poll(&sink, t0 + MS(600), true);
        stream(&mut pacer, &sink, "b", t0 + MS(600));
        assert_eq!(sink.take(), vec!["b"], "the window is over");
    }

    #[test]
    fn an_immediate_envelope_hands_over_what_waits_first() {
        // The case that matters: a card, a `tabs` switch, a snapshot mid-burst reaches the page
        // at once, and never ahead of the stream that came before it.
        let (mut pacer, t0) = typing_pacer(Some(8));
        let sink = Recording::default();
        pacer.note_editor_key(t0);
        stream(&mut pacer, &sink, "a", t0 + MS(10));
        stream(&mut pacer, &sink, "b", t0 + MS(20));
        pacer.send_immediate(&sink, "card");
        assert_eq!(sink.take(), vec!["a", "b", "card"]);
        pacer.send_immediate(&sink, "tabs");
        assert_eq!(sink.take(), vec!["tabs"], "nothing left to hand over");
    }

    #[test]
    fn an_immediate_class_pump_payload_flushes_behind_nothing_and_ahead_of_nothing() {
        let (mut pacer, t0) = typing_pacer(Some(8));
        let sink = Recording::default();
        pacer.note_editor_key(t0);
        stream(&mut pacer, &sink, "a", t0 + MS(10));
        pacer.send_stream(&sink, "snapshot".into(), None, EnvelopeClass::Immediate, t0 + MS(20));
        assert_eq!(sink.take(), vec!["a", "snapshot"]);
        stream(&mut pacer, &sink, "b", t0 + MS(30));
        assert!(sink.take().is_empty(), "the window is still open: b is held again");
    }

    #[test]
    fn flush_hands_over_everything_and_only_once() {
        let (mut pacer, t0) = typing_pacer(Some(8));
        let sink = Recording::default();
        pacer.note_editor_key(t0);
        stream(&mut pacer, &sink, "a", t0 + MS(10));
        pacer.flush(&sink);
        assert_eq!(sink.take(), vec!["a"]);
        pacer.flush(&sink);
        assert!(sink.take().is_empty());
    }

    #[test]
    fn the_end_of_the_window_releases_what_waits_and_the_page_hears_of_it_after() {
        // 2 per second: slower than the turn meter's 300 ms step, so the page is told.
        let (mut pacer, t0) = typing_pacer(Some(2));
        let sink = Recording::default();
        pacer.note_editor_key(t0);
        pacer.poll(&sink, t0 + MS(33), true);
        let told = sink.take();
        assert_eq!(told.len(), 1, "{told:?}");
        assert!(
            told[0].contains("\"typing\":true") && told[0].contains("\"periodMs\":500"),
            "{told:?}"
        );
        stream(&mut pacer, &sink, "a", t0 + MS(100));
        assert!(sink.take().is_empty());
        // 500 ms after the last key: before the 500 ms slot could ever be reached.
        pacer.poll(&sink, t0 + MS(500), true);
        let out = sink.take();
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(out[0], "a", "the stream first");
        assert!(out[1].contains("\"typing\":false"), "then the page is told: {out:?}");
        pacer.poll(&sink, t0 + MS(533), true);
        assert!(sink.take().is_empty(), "told once");
    }

    #[test]
    fn the_default_cadence_never_tells_the_page_anything() {
        // The default (5/s, 200 ms) is faster than the turn meter's 300 ms step: telling the page
        // would cost repaints and buy nothing.
        let (mut pacer, t0) = typing_pacer(Some(DEFAULT_CADENCE_HZ));
        let sink = Recording::default();
        for step in 0..40u64 {
            let now = t0 + MS(step * 33);
            pacer.note_editor_key(now);
            stream(&mut pacer, &sink, "x", now);
            pacer.poll(&sink, now, true);
        }
        pacer.poll(&sink, t0 + MS(2_000), true);
        let sent = sink.take();
        assert!(typing_envelopes(&sent).is_empty(), "{sent:?}");
        assert_eq!(sent.len(), 40, "and nothing was lost: {}", sent.len());
    }

    #[test]
    fn losing_the_editors_keys_tells_the_page_typing_stopped() {
        // Focus moving to the panel or the terminal ends the window at once; the slowed meter must
        // not outlive it waiting for the 500 ms timeout.
        let (mut pacer, t0) = typing_pacer(Some(1));
        let sink = Recording::default();
        pacer.note_editor_key(t0);
        pacer.poll(&sink, t0 + MS(20), true);
        assert_eq!(typing_envelopes(&sink.take()).len(), 1);
        pacer.set_editor_has_keys(false);
        pacer.poll(&sink, t0 + MS(40), true);
        let sent = sink.take();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert!(sent[0].contains("\"typing\":false"), "{sent:?}");
    }

    #[test]
    fn the_typing_message_does_not_release_what_is_held() {
        let (mut pacer, t0) = typing_pacer(Some(1));
        let sink = Recording::default();
        pacer.note_editor_key(t0);
        stream(&mut pacer, &sink, "a", t0 + MS(5));
        pacer.poll(&sink, t0 + MS(20), true);
        let sent = sink.take();
        assert_eq!(typing_envelopes(&sent).len(), 1, "{sent:?}");
        assert!(!sent.contains(&"a".to_string()), "a waits for its slot: {sent:?}");
    }

    #[test]
    fn discarding_forgets_what_waits_and_tells_the_new_page_again() {
        let (mut pacer, t0) = typing_pacer(Some(1));
        let sink = Recording::default();
        pacer.note_editor_key(t0);
        pacer.poll(&sink, t0 + MS(20), true);
        stream(&mut pacer, &sink, "a", t0 + MS(30));
        sink.take();
        assert!(pacer.discard_pending().is_empty(), "a held envelope with no first text");
        pacer.poll(&sink, t0 + MS(60), true);
        let sent = sink.take();
        assert!(
            !sent.contains(&"a".to_string()),
            "the reloaded page gets a snapshot, not a: {sent:?}"
        );
        assert_eq!(
            typing_envelopes(&sent).len(),
            1,
            "still typing: the new page is told: {sent:?}"
        );
        pacer.poll(&sink, t0 + MS(1_000), true);
        let sent = sink.take();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert!(sent[0].contains("\"typing\":false"), "then the window ends: {sent:?}");
    }

    #[test]
    fn discarding_reports_the_first_text_it_threw_away_so_its_trace_can_still_finish() {
        // The trace waits for a paint report of the first text; the reloaded page paints it from
        // the snapshot and reports nothing, so the caller must be told which turn's first text
        // will never go out as an envelope.
        let (mut pacer, t0) = typing_pacer(Some(1));
        let sink = Recording::default();
        pacer.note_editor_key(t0);
        let first = FirstText {
            tab: TabId(3),
            turn: t0,
        };
        pacer.send_stream(&sink, "a".into(), Some(first), EnvelopeClass::Stream, t0 + MS(10));
        stream(&mut pacer, &sink, "b", t0 + MS(20));
        assert!(sink.take().is_empty(), "both wait for the slot");
        assert_eq!(pacer.discard_pending(), vec![first]);
        assert!(pacer.discard_pending().is_empty(), "nothing is reported twice");
        assert!(
            pacer.take_sent().first_text.is_empty(),
            "a discarded first text was never dispatched"
        );
    }

    #[test]
    fn the_page_is_told_about_typing_only_once_it_has_said_ready() {
        let (mut pacer, t0) = typing_pacer(Some(1));
        let sink = Recording::default();
        pacer.note_editor_key(t0);
        pacer.poll(&sink, t0 + MS(20), false);
        assert!(sink.take().is_empty(), "a page that is still loading is not sent to");
        pacer.poll(&sink, t0 + MS(40), true);
        let sent = sink.take();
        assert_eq!(typing_envelopes(&sent).len(), 1, "{sent:?}");
    }

    fn first_of(tab: u64, turn: Instant) -> Option<FirstText> {
        Some(FirstText { tab: TabId(tab), turn })
    }

    #[test]
    fn first_text_is_reported_when_its_envelope_really_goes() {
        let (mut pacer, t0) = typing_pacer(Some(8));
        let sink = Recording::default();
        pacer.note_editor_key(t0);
        pacer.send_stream(
            &sink,
            "first".into(),
            first_of(1, t0),
            EnvelopeClass::Stream,
            t0 + MS(10),
        );
        assert_eq!(pacer.take_sent(), Sent::default(), "held: nothing has reached the page");
        let before = Instant::now();
        pacer.poll(&sink, t0 + MS(130), true);
        let sent = pacer.take_sent();
        assert!(sent.any);
        assert_eq!(sent.first_text.len(), 1, "{sent:?}");
        assert_eq!(sent.first_text[0].0, first_of(1, t0).unwrap());
        assert!(
            sent.first_text[0].1 >= before,
            "stamped when it went, not when it was pumped"
        );
        assert_eq!(pacer.take_sent(), Sent::default(), "reported once");
    }

    #[test]
    fn first_text_is_reported_when_an_immediate_envelope_pushed_it_out() {
        let (mut pacer, t0) = typing_pacer(Some(8));
        let sink = Recording::default();
        pacer.note_editor_key(t0);
        pacer.send_stream(
            &sink,
            "first".into(),
            first_of(1, t0),
            EnvelopeClass::Stream,
            t0 + MS(10),
        );
        pacer.send_immediate(&sink, "card");
        let sent = pacer.take_sent();
        assert!(sent.any);
        assert_eq!(
            sent.first_text.iter().map(|(f, _)| *f).collect::<Vec<_>>(),
            vec![first_of(1, t0).unwrap()]
        );
    }

    /// Fix round 1: tab A's first text is held, `prefix n` switches to tab B and its `tabs`
    /// dispatch flushes A's text. The stamp names A (and A's turn), whatever tab is active when the
    /// pump reads it; and a `turn_rendered` handler can take it before that tick does, leaving the
    /// pump's `any` alone.
    #[test]
    fn a_first_text_released_by_another_tabs_dispatch_still_names_its_own_tab() {
        let (mut pacer, t0) = typing_pacer(Some(8));
        let sink = Recording::default();
        pacer.note_editor_key(t0);
        pacer.send_stream(
            &sink,
            "a-first".into(),
            first_of(1, t0),
            EnvelopeClass::Stream,
            t0 + MS(10),
        );
        pacer.send_immediate(&sink, "tabs: switch to 2");
        assert_eq!(sink.take(), vec!["a-first", "tabs: switch to 2"]);
        let stamps = pacer.take_first_text();
        assert_eq!(stamps.iter().map(|(f, _)| f.tab).collect::<Vec<_>>(), vec![TabId(1)]);
        let sent = pacer.take_sent();
        assert!(sent.any, "the pump still learns that stream went out");
        assert!(sent.first_text.is_empty(), "and the stamp is not applied twice");
    }

    #[test]
    fn with_pacing_off_everything_goes_at_once_whatever_the_keys_do() {
        let (mut pacer, t0) = typing_pacer(None);
        let sink = Recording::default();
        pacer.note_editor_key(t0);
        stream(&mut pacer, &sink, "a", t0 + MS(1));
        stream(&mut pacer, &sink, "b", t0 + MS(2));
        pacer.poll(&sink, t0 + MS(3), true);
        assert_eq!(sink.take(), vec!["a", "b"]);
    }

    #[test]
    fn a_key_when_the_panel_holds_the_keys_paces_nothing() {
        let mut pacer = Pacer::new(Some(8));
        let sink = Recording::default();
        let t0 = Instant::now();
        pacer.set_editor_has_keys(false);
        pacer.note_editor_key(t0);
        stream(&mut pacer, &sink, "a", t0 + MS(1));
        assert_eq!(sink.take(), vec!["a"]);
    }

    #[test]
    fn losing_the_editors_keys_releases_what_waits_at_the_next_tick() {
        let (mut pacer, t0) = typing_pacer(Some(8));
        let sink = Recording::default();
        pacer.note_editor_key(t0);
        stream(&mut pacer, &sink, "a", t0 + MS(10));
        pacer.set_editor_has_keys(false);
        pacer.poll(&sink, t0 + MS(33), true);
        assert_eq!(sink.take(), vec!["a"]);
    }

    /// The code lines of `source` (comments dropped) up to its test module.
    fn code_lines(source: &str) -> Vec<&str> {
        source
            .split("\n#[cfg(test)]\nmod tests")
            .next()
            .expect("split always yields one part")
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect()
    }

    #[test]
    fn agent_panel_puts_envelopes_in_the_page_only_through_the_funnel() {
        // `evaluate_js_dispatch` and `AgentPanelHandle::dispatch` hand over what the cadence holds
        // before they send; `set_theme`'s own guarded send is the third and carries no stream
        // state. A fourth direct `evaluate_javascript` would let an envelope overtake a held one --
        // a snapshot ahead of the events before it draws them twice.
        let code = code_lines(include_str!("agent_panel.rs"));
        let direct = code
            .iter()
            .filter(|line| line.contains(".evaluate_javascript("))
            .count();
        assert_eq!(direct, 2, "agent_panel.rs sends to the page outside the funnel");
        assert!(
            code.iter()
                .any(|line| line.contains("crate::panel_pacer::flush(webview)")),
            "AgentPanelHandle::dispatch hands over what is held first"
        );
    }

    #[test]
    fn only_the_pumps_stream_payload_is_ever_held() {
        let code = code_lines(include_str!("agent_panel.rs"));
        let sites = code.iter().filter(|line| line.contains(".send_stream(")).count();
        assert_eq!(
            sites, 1,
            "`send_stream` is the pump's alone; everything else is never delayed"
        );
    }

    #[test]
    fn a_new_cadence_keeps_what_is_held_and_who_has_the_keys() {
        let (mut pacer, t0) = typing_pacer(Some(8));
        let sink = Recording::default();
        pacer.note_editor_key(t0);
        stream(&mut pacer, &sink, "a", t0 + MS(10));
        pacer.set_cadence(Some(4));
        pacer.note_editor_key(t0 + MS(20));
        stream(&mut pacer, &sink, "b", t0 + MS(30));
        assert!(sink.take().is_empty());
        pacer.poll(&sink, t0 + MS(700), true);
        assert_eq!(
            sink.take(),
            vec!["a", "b"],
            "the editor still has the keys, nothing lost"
        );
    }
}
