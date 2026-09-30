//! `prefix x`, tmux's `kill-pane` (owner, 2026-09-26: "prefix x应该是直接关掉pane而不是隐藏"). Stock
//! tmux binds `x` to `confirm-before -p "kill-pane #P? (y/n)" kill-pane` (checked against a
//! throwaway `tmux -L xprobe -f /dev/null` server, tmux next-3.7), so Eitri asks first too, in the
//! window's one y/n (`close_prompt`), naming the module where tmux names the pane's number.
//!
//! What a kill ends, per module (the rulings, recorded in the dated record, 2026-09-26):
//! - **terminal**: its shell is hung up (SIGHUP, as closing a terminal does) and the module goes back
//!   where a first launch puts it, hidden; `prefix t` starts a fresh shell there.
//! - **a Lua panel**: its web process is terminated and the module goes back where its `position`
//!   puts it, hidden; its key loads the page afresh.
//! - **agent**: every session tab is closed as `prefix &` closes one (records stay resumable), and
//!   the module is hidden in place; `prefix a` opens the chat with the session chooser.
//! - **editor**: nvim is asked to `:confirm qall`, so unsaved buffers get nvim's own prompt; only if
//!   nvim quits does the module go. It cannot come back in this window (`eitri_core::layout::kill`'s
//!   module doc: the fork's `LiveHarness` owns a winit event loop, which is once per process), and
//!   the prompt says so. Cancelled in nvim, nothing closes.
//!
//! - **the last module on screen** (2026-09-26, later; owner: "prefix x对neovide窗口不生效，不能触发
//!   neovibe关闭"): tmux closes the window when its last pane is killed, and ends when its last window
//!   is, so `x` here closes Eitri ([`KillScope::Window`]). One question: it names what the window
//!   close would have asked about (`N running, M queued`), and `y` is taken as the answer to both. The
//!   editor still goes through `:confirm qall` first, and the window closes when nvim exits.
//!
//! - **every window close** (v1 hardening Task 6, ruling R3: "closing the window never discards
//!   unsaved editor changes silently"): the top bar's close, the compositor's, the last module's
//!   kill, the terminal's `exit` as the last module -- each asks nvim to `:confirm qall` exactly as
//!   `prefix x` on the editor does ([`EditorQuit::Window`]), and the window closes only when nvim
//!   exits. Until then the fork's quit ran `:qa!`, which discards unsaved buffers AND deletes their
//!   swap files. The precedent is stock Neovide's own `g:neovide_confirm_quit`, on by default
//!   (`src/window/settings.rs` in the fork), which makes its window close run `confirm qa` -- and,
//!   like Neovide's, the quit is an RPC request (`nvim_exec_lua`), never typed keys, which a key nvim
//!   is waiting for (after `f`) would swallow (`eitri_core::layout::kill::editor_quit_lua`). An
//!   editor that is not on screen -- hidden, or zoomed away -- is brought on screen for the prompt
//!   ([`reveal_for_prompt`]); one that was hidden is hidden again if nvim quits, so the saved
//!   arrangement stays the user's. A second request while nvim is asking is refused, as a second `x`
//!   is, with what nvim is doing ([`NvimState`]); only when nvim's own loop has not answered for
//!   [`NVIM_QUIT_PATIENCE`] does it kill nvim instead ([`quit_request`]).
//! - **a declined window close after nvim exited on its own** (R1-2): nvim is gone and the window
//!   stays, so the editor is retired as `prefix x`'s kill of it retires it ([`Reopen::Never`]) --
//!   never left on screen holding the keys with nothing behind it ([`retire_on_declined_close`]).
//!   It cannot be restarted in this window (the winit event loop above); `prefix e` then says so.
//!
//! This file holds the pure pieces decided without a display: the prompt's text, where each kind of
//! module is left (`Reopen`), and each single decision of the editor's quit. The flow that strings
//! those decisions together is `editor_quit` (also display-free, tested against a fake host);
//! `main.rs` only carries it out.

use std::time::{Duration, Instant};

use eitri_core::layout::{KillScope, Layout, ModuleDecl, ModuleId, ModuleKind, Placement, Reopen};
use neovide_editor::CallWatch;

/// The y/n `prefix x` asks. `title` is the module's name as the tray and the strip show it;
/// `running`/`queued` are the agent's tabs with a turn or a connect in flight and its queued
/// messages (the window-close prompt's two counts). For [`KillScope::Module`] they are the agent's
/// own consequences, ignored for any other module; for [`KillScope::Window`] -- the last module on
/// screen, whose kill closes Eitri -- they are the window close's, for every module, so this one
/// question stands in for that one too.
pub(crate) fn prompt(id: &ModuleId, title: &str, scope: KillScope, running: usize, queued: usize) -> String {
    let mut consequences = Vec::new();
    if scope == KillScope::Window {
        consequences.push("closes Eitri".to_string());
        if running > 0 {
            consequences.push(format!("{running} running"));
        }
        if queued > 0 {
            consequences.push(format!("{queued} queued"));
        }
        return format!("kill-pane {title}? {} (y/n)", consequences.join(", "));
    }
    match id.kind() {
        ModuleKind::Agent => {
            if running > 0 {
                consequences.push(format!("{running} running"));
            }
            if queued > 0 {
                consequences.push(format!("{queued} queued"));
            }
        }
        ModuleKind::Editor => consequences.push("nvim cannot be reopened in this window".to_string()),
        ModuleKind::Terminal | ModuleKind::Canvas | ModuleKind::LuaWebview => {}
    }
    if consequences.is_empty() {
        format!("kill-pane {title}? (y/n)")
    } else {
        format!("kill-pane {title}? {} (y/n)", consequences.join(", "))
    }
}

/// Why nvim was asked to `:confirm qall`, for what its exit does: close the editor for this window
/// ([`KillScope::Module`]), or close the window ([`KillScope::Window`], and every window close),
/// carrying the window close's prompt as it stood when nvim was asked ([`close_is_confirmed`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EditorQuit {
    Module,
    Window { confirmed: Option<String> },
}

/// A quit of the editor in flight: nvim was asked to run `editor_quit_lua(generation)`
/// (`NeovideEditorPane::exec_lua_watched`, whose watch says how nvim is taking it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Asked {
    /// Bumped by `main.rs` before each send, so a cancel's own generation can be told apart from a
    /// later, different quit of the same editor ([`clear_if_cancelled`]).
    pub(crate) generation: u32,
    /// What nvim's exit then does.
    pub(crate) quit: EditorQuit,
    /// The editor was hidden and was shown for nvim's prompt ([`Reveal::rehide`]): hidden again if
    /// nvim quits, so the arrangement the window saves on close is the user's, not the close's.
    pub(crate) revealed: bool,
    /// Its request was sent to nvim, so the pane's watch is this quit's own (every send replaces the
    /// watch, and only a quit sends one): when that watch says nvim answered it, nvim did not quit
    /// and this is cleared (`editor_quit`, codex finding C1). `false` for a quit joined to an nvim
    /// that had exited already, which has no request of its own.
    pub(crate) sent: bool,
    /// A kill of the editor that sent nothing (nvim out of reach, or just exited) and joined an
    /// older window close nvim was sent and had not answered: the close wins ([`merge`]), and this
    /// is that close's generation, since its request -- not this quit's, which has none -- says
    /// whether it still stands. Its cancel letter, or its request coming back answered, however
    /// late either is read, takes the close back out ([`Asked::withdraw_lent_close`]): the kill is
    /// what is left (the quit follow-up's fix round, codex: a settle wait's bound is no proof). Only
    /// ever set on an unsent `Window` whose own request was a kill, so what it withdraws to is
    /// always [`EditorQuit::Module`].
    pub(crate) lent_by: Option<u32>,
}

impl Asked {
    /// The window close this quit carries was cancelled in nvim ([`Asked::lent_by`]): nvim's exit
    /// then does what the kill asked, retiring the editor, and never closes the window on that
    /// close's `y`.
    pub(crate) fn withdraw_lent_close(&mut self) {
        if self.lent_by.take().is_some() {
            self.quit = EditorQuit::Module;
        }
    }
}

pub(crate) type QuitInFlight = Option<Asked>;

/// Clears the quit in flight if `generation` is its own, returning it; a letter from an earlier,
/// already superseded quit leaves a newer one alone.
pub(crate) fn clear_if_cancelled(quitting: &mut QuitInFlight, generation: u32) -> Option<Asked> {
    if quitting.as_ref().is_some_and(|asked| asked.generation == generation) {
        return quitting.take();
    }
    None
}

/// What `main.rs`'s `ask_nvim_to_quit` does with the editor before nvim draws its prompt in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reveal {
    /// Not on screen (hidden, or zoomed away): shown, which ends a zoom. `Layout::is_visible`, not
    /// `is_shown`, which ignores a zoom -- a close with the agent zoomed left nvim's prompt waiting
    /// unseen while the editor took the keys, and the next `n` or `d` answered it blind (review
    /// finding I2).
    pub(crate) show: bool,
    /// Hidden (not merely zoomed away): hidden again if nvim quits. A zoom is not saved
    /// (`eitri_core::layout::persist`), so ending one changes nothing the window keeps.
    pub(crate) rehide: bool,
}

pub(crate) fn reveal_for_prompt(layout: &Layout, editor: &ModuleId) -> Reveal {
    Reveal {
        show: !layout.is_visible(editor),
        rehide: !layout.is_shown(editor),
    }
}

/// How long nvim's own loop may go without answering before a second request takes it as not
/// answering: the fork's own `LiveHarness::shutdown` wait (`SHUTDOWN_WAIT`, 5 s). An nvim whose
/// loop runs answers the pane's watch every 250 ms, whatever it is showing.
pub(crate) const NVIM_QUIT_PATIENCE: Duration = Duration::from_secs(5);

/// Where nvim stands with the quit it was sent, read off the pane's watch on that request
/// (`neovide_editor::CallWatch`: `nvim_get_mode`, one of nvim's fast calls, asked every 250 ms).
/// Not from redraws: the dialog draws once and then waits for the user, drawing nothing, exactly
/// as a hung nvim draws nothing (review finding I1, second half).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NvimState {
    /// The request came back -- its Lua returned, so nvim did not quit -- or nothing was sent.
    Returned,
    /// nvim's loop has not answered for [`NVIM_QUIT_PATIENCE`]: stuck (a Lua busy loop, a stopped
    /// process), or waiting on an external command (`system()`, `:!`, a formatter a save hook runs),
    /// during which nvim answers nothing (see [`NvimState::Busy`]). A second request then ends it
    /// (`editor_quit`'s `QuitHost::end_nvim`: SIGTERM and stdin closed, SIGKILL by pid after the
    /// patience), and [`not_answering_text`] says so.
    NotAnswering,
    /// Its `:confirm` dialog is up (`nvim_get_mode` says `r?`).
    Asking,
    /// It waits for a key before it takes the request (`blocking`: after `f`, at a hit-enter prompt).
    WaitingForKey,
    /// Answering, and neither: about to ask, or busy in something that keeps its loop running (a
    /// write hook in `vim.wait()`), which `Ctrl+C` interrupts. NOT an external command: measured on
    /// nvim 0.12.5 (the Opus review's T6-3), `vim.fn.system('sleep 3')` and `:silent !sleep 3` leave
    /// `nvim_get_mode` unanswered until they end, exactly as a Lua busy loop does, so a save hook
    /// that runs one for [`NVIM_QUIT_PATIENCE`] or longer reads [`NvimState::NotAnswering`].
    Busy,
}

pub(crate) fn nvim_state(watch: Option<&CallWatch>, now: Instant) -> NvimState {
    let Some(watch) = watch else {
        return NvimState::Returned;
    };
    if watch.done {
        return NvimState::Returned;
    }
    if now.saturating_duration_since(watch.last_alive()) >= NVIM_QUIT_PATIENCE {
        return NvimState::NotAnswering;
    }
    match &watch.last_answer {
        Some(answer) if answer.mode == "r?" => NvimState::Asking,
        Some(answer) if answer.blocking => NvimState::WaitingForKey,
        _ => NvimState::Busy,
    }
}

/// What a request to quit the editor -- a window close, `prefix x` on it -- does, given the quit
/// already in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QuitRequest {
    /// Nothing in flight, or the one in flight came back (cancelled): nvim is asked.
    Ask,
    /// nvim has the quit already: a second `:confirm qall` would nest inside the first instead of
    /// answering it (measured), so the user deals with the one in the editor ([`answer_there_text`]).
    AnswerThere(NvimState),
    /// nvim's loop has not answered for [`NVIM_QUIT_PATIENCE`]: ended (never `:qa!`), its swap
    /// files kept.
    Kill,
}

/// `watch` is the pane's watch on the request in flight (`NeovideEditorPane::watched_call`). It
/// also says when that request came back, so a cancel is seen even with no pane-switch shim to send
/// the letter.
pub(crate) fn quit_request(asked: Option<&Asked>, watch: Option<&CallWatch>, now: Instant) -> QuitRequest {
    if asked.is_none() {
        return QuitRequest::Ask;
    }
    match nvim_state(watch, now) {
        NvimState::Returned => QuitRequest::Ask,
        NvimState::NotAnswering => QuitRequest::Kill,
        state => QuitRequest::AnswerThere(state),
    }
}

/// The refusal of a second quit request, by what nvim is doing with the first.
pub(crate) fn answer_there_text(state: NvimState) -> &'static str {
    match state {
        NvimState::WaitingForKey => {
            "nvim will ask once it has the key it is waiting for \u{2014} type it, or Esc, in the editor"
        }
        NvimState::Busy => "nvim is busy and asks when it is done \u{2014} Ctrl+C in the editor interrupts it",
        NvimState::Asking | NvimState::Returned | NvimState::NotAnswering => {
            "nvim is already asking whether to quit; answer it there"
        }
    }
}

/// The quit in flight once a second request joins it: a window close wins over the editor's own
/// kill, since the user asked for the window to go.
pub(crate) fn merge(current: &EditorQuit, incoming: EditorQuit) -> EditorQuit {
    match (current, incoming) {
        (_, incoming @ EditorQuit::Window { .. }) => incoming,
        (current, EditorQuit::Module) => current.clone(),
    }
}

/// The toast once nvim's loop has not answered for [`NVIM_QUIT_PATIENCE`]: what a second request
/// then does -- ends nvim, which keeps its swap file (round 4: stdin closed and SIGTERM, which
/// nvim answers by writing what it had not synced into the swap file; only a SIGKILL after 5 s
/// loses that, the Opus review's T6-3: nvim may only be running a long external command).
pub(crate) fn not_answering_text(quit: &EditorQuit) -> &'static str {
    match quit {
        EditorQuit::Window { .. } => {
            "nvim is not responding to the quit \u{2014} close the window again to end it \
             (its swap file keeps your changes)"
        }
        EditorQuit::Module => {
            "nvim is not responding to the quit \u{2014} kill-pane the editor again to end it \
             (its swap file keeps your changes)"
        }
    }
}

/// Whether a declined window close retires the editor (R1-2): only once nvim has exited, and only
/// if it is not retired already. A close declined while nvim runs leaves a working editor.
pub(crate) fn retire_on_declined_close(nvim_exited: bool, editor_gone: bool) -> bool {
    nvim_exited && !editor_gone
}

/// Whether a window close after a `y` to [`KillScope::Window`] may skip its own y/n. `confirmed` is
/// the window close's prompt (`eitri_core::tabs::window_close_prompt`) as it stood when the kill
/// was asked, `now` as it stands at the close: the editor's close waits for nvim, whose own
/// `:confirm qall` may be answered much later. Nothing worth asking about now, or exactly what was
/// said yes to: no second question. Anything else is asked.
pub(crate) fn close_is_confirmed(confirmed: Option<&str>, now: Option<&str>) -> bool {
    now.is_none() || now == confirmed
}

/// Where the layout leaves a killed module (`eitri_core::layout::Reopen`). `decls` are the Lua
/// panels as `init.lua` registered them.
pub(crate) fn reopen(id: &ModuleId, decls: &[ModuleDecl]) -> Reopen {
    match id.kind() {
        // `BelowEditor`, not `BelowRoot`: a terminal killed next to a Lua `side` panel comes back
        // below the editor's own leaf only (as a first launch puts it, v1 trial item 6, 2026-09-28),
        // not full width below the side panel too (`eitri_core::layout::tree::place_new`'s own doc).
        ModuleKind::Terminal => Reopen::At(Placement::BelowEditor),
        ModuleKind::LuaWebview => decls
            .iter()
            .find(|d| d.id == *id)
            .map_or(Reopen::InPlace, |d| Reopen::At(d.placement)),
        ModuleKind::Agent | ModuleKind::Canvas => Reopen::InPlace,
        ModuleKind::Editor => Reopen::Never,
    }
}

/// The Lua nvim is asked to run to quit the editor (`eitri_core::layout::kill::editor_quit_lua`).
pub(crate) use eitri_core::layout::kill::editor_quit_lua;

#[cfg(test)]
mod tests {
    use super::*;

    fn asked(generation: u32, quit: EditorQuit) -> Asked {
        Asked {
            generation,
            quit,
            revealed: false,
            sent: true,
            lent_by: None,
        }
    }

    /// A watch on a request sent at `sent_at`, last answered `answered` after it with `mode`.
    fn watch(sent_at: Instant, done: bool, answer: Option<(Duration, &str, bool)>) -> CallWatch {
        CallWatch {
            sent_at,
            done,
            returned: done,
            last_answer: answer.map(|(after, mode, blocking)| neovide_editor::NvimMode {
                at: sent_at + after,
                mode: mode.to_string(),
                blocking,
            }),
        }
    }

    #[test]
    fn a_cancelled_quit_clears_its_own_kill() {
        let mut quitting: QuitInFlight = Some(asked(3, EditorQuit::Module));
        assert_eq!(
            clear_if_cancelled(&mut quitting, 3).map(|a| a.quit),
            Some(EditorQuit::Module)
        );
        assert_eq!(quitting, None);

        let window = EditorQuit::Window {
            confirmed: Some("x".into()),
        };
        let mut quitting: QuitInFlight = Some(asked(3, window.clone()));
        assert_eq!(clear_if_cancelled(&mut quitting, 3).map(|a| a.quit), Some(window));
        assert_eq!(quitting, None);
    }

    #[test]
    fn a_late_letter_from_an_earlier_kill_leaves_the_newer_one() {
        let newer = asked(4, EditorQuit::Module);
        let mut quitting: QuitInFlight = Some(newer.clone());
        assert_eq!(clear_if_cancelled(&mut quitting, 3), None);
        assert_eq!(quitting, Some(newer));
    }

    #[test]
    fn a_letter_with_nothing_in_flight_changes_nothing() {
        let mut quitting: QuitInFlight = None;
        assert_eq!(clear_if_cancelled(&mut quitting, 1), None);
        assert_eq!(quitting, None);
    }

    #[test]
    fn a_letter_from_the_future_is_not_this_kill() {
        let older = asked(2, EditorQuit::Module);
        let mut quitting: QuitInFlight = Some(older.clone());
        assert_eq!(clear_if_cancelled(&mut quitting, 5), None);
        assert_eq!(quitting, Some(older));
    }

    /// A kill carrying an older close takes it back out to itself; a close of its own, which no
    /// request of nvim's can cancel, is never withdrawn.
    #[test]
    fn withdrawing_a_lent_close_leaves_the_kill_and_only_a_lent_one() {
        let window = EditorQuit::Window {
            confirmed: Some("x".into()),
        };
        let mut lent = Asked {
            sent: false,
            lent_by: Some(3),
            ..asked(4, window.clone())
        };
        lent.withdraw_lent_close();
        assert_eq!((lent.quit, lent.lent_by), (EditorQuit::Module, None));

        let mut own = Asked {
            sent: false,
            ..asked(5, window.clone())
        };
        own.withdraw_lent_close();
        assert_eq!(own.quit, window);
    }

    /// R3: with nothing in flight, a quit of the editor -- a window close included -- asks nvim. A
    /// second one while nvim has it is refused (a second `:confirm qall` nests), saying what nvim is
    /// doing; once the first came back (cancelled -- with or without the shim's letter) nvim is
    /// asked again.
    #[test]
    fn a_quit_asks_nvim_and_a_second_one_is_answered_where_nvim_is() {
        let sent = Instant::now();
        let now = sent + Duration::from_secs(1);
        let in_flight = asked(1, EditorQuit::Window { confirmed: None });
        assert_eq!(quit_request(None, None, now), QuitRequest::Ask);
        let dialog = watch(sent, false, Some((Duration::from_millis(900), "r?", false)));
        assert_eq!(quit_request(None, Some(&dialog), now), QuitRequest::Ask);

        assert_eq!(
            quit_request(Some(&in_flight), Some(&dialog), now),
            QuitRequest::AnswerThere(NvimState::Asking)
        );
        let after_f = watch(sent, false, Some((Duration::from_millis(900), "n", true)));
        assert_eq!(
            quit_request(Some(&in_flight), Some(&after_f), now),
            QuitRequest::AnswerThere(NvimState::WaitingForKey),
            "held behind a key nvim waits for: it asks once it has it"
        );
        let busy = watch(sent, false, Some((Duration::from_millis(900), "n", false)));
        assert_eq!(
            quit_request(Some(&in_flight), Some(&busy), now),
            QuitRequest::AnswerThere(NvimState::Busy)
        );
        let returned = watch(sent, true, Some((Duration::from_millis(900), "n", false)));
        assert_eq!(
            quit_request(Some(&in_flight), Some(&returned), now),
            QuitRequest::Ask,
            "cancelled, its letter not read yet (or no shim to send one)"
        );
        assert_eq!(quit_request(Some(&in_flight), None, now), QuitRequest::Ask);
    }

    /// Review finding I1 (and codex's hang after Save): a dialog that waits for its answer draws
    /// nothing, as a hung nvim draws nothing, so "not answering" is nvim's own loop not answering the
    /// watch -- never redraws, and never timed from the send alone. A dialog left up for minutes is
    /// still the user's; an nvim that stopped answering after drawing it (Save, then a hook that
    /// hangs) is killed on the next request.
    #[test]
    fn only_an_nvim_whose_loop_stopped_answering_is_killed() {
        let sent = Instant::now();
        let in_flight = asked(1, EditorQuit::Window { confirmed: None });

        let dialog_for_minutes = watch(sent, false, Some((Duration::from_secs(300), "r?", false)));
        let now = sent + Duration::from_secs(300) + Duration::from_millis(250);
        assert_eq!(
            quit_request(Some(&in_flight), Some(&dialog_for_minutes), now),
            QuitRequest::AnswerThere(NvimState::Asking)
        );

        // Drew its dialog, answered, then went quiet: killed once the quiet lasts the patience.
        let hung_after_save = watch(sent, false, Some((Duration::from_secs(2), "r?", false)));
        let quiet_since = sent + Duration::from_secs(2);
        assert_eq!(
            quit_request(
                Some(&in_flight),
                Some(&hung_after_save),
                quiet_since + NVIM_QUIT_PATIENCE - Duration::from_millis(1)
            ),
            QuitRequest::AnswerThere(NvimState::Asking),
            "not quiet long enough"
        );
        assert_eq!(
            quit_request(
                Some(&in_flight),
                Some(&hung_after_save),
                quiet_since + NVIM_QUIT_PATIENCE
            ),
            QuitRequest::Kill
        );

        // Never answered at all: timed from the send.
        let never = watch(sent, false, None);
        assert_eq!(
            quit_request(Some(&in_flight), Some(&never), sent + NVIM_QUIT_PATIENCE),
            QuitRequest::Kill
        );
        assert_eq!(nvim_state(Some(&never), sent + Duration::from_secs(1)), NvimState::Busy);
    }

    #[test]
    fn a_refused_second_request_says_what_nvim_is_doing() {
        assert!(answer_there_text(NvimState::Asking).contains("answer it there"));
        assert!(answer_there_text(NvimState::WaitingForKey).contains("Esc"));
        assert!(answer_there_text(NvimState::Busy).contains("Ctrl+C"));
    }

    /// Review finding I2: an editor zoomed away is shown too -- it is shown in the layout's terms
    /// (`is_shown`) yet off screen -- and, not having been hidden, is not hidden again afterwards.
    #[test]
    fn a_hidden_or_zoomed_away_editor_is_brought_on_screen_for_the_prompt() {
        use eitri_core::layout::{hide, Frame, Size};
        let editor = ModuleId::editor();
        let mut layout = Layout::initial(&[]).unwrap();
        assert_eq!(
            reveal_for_prompt(&layout, &editor),
            Reveal {
                show: false,
                rehide: false
            }
        );

        layout.toggle_zoom(&ModuleId::agent());
        assert!(layout.is_shown(&editor) && !layout.is_visible(&editor), "the case");
        assert_eq!(
            reveal_for_prompt(&layout, &editor),
            Reveal {
                show: true,
                rehide: false
            }
        );

        layout.toggle_zoom(&ModuleId::agent());
        hide(&mut layout, &editor, &Frame::new(Size { w: 1280, h: 721 }, 1)).unwrap();
        assert_eq!(
            reveal_for_prompt(&layout, &editor),
            Reveal {
                show: true,
                rehide: true
            }
        );
    }

    #[test]
    fn a_window_close_joining_a_kill_of_the_editor_closes_the_window() {
        let window = EditorQuit::Window {
            confirmed: Some("close window? 1 running (y/n)".into()),
        };
        assert_eq!(merge(&EditorQuit::Module, window.clone()), window);
        assert_eq!(merge(&window, EditorQuit::Module), window);
        assert_eq!(merge(&EditorQuit::Module, EditorQuit::Module), EditorQuit::Module);
        assert_eq!(
            merge(&window, EditorQuit::Window { confirmed: None }),
            EditorQuit::Window { confirmed: None },
            "the newer close's prompt is what was asked last"
        );
    }

    #[test]
    fn the_not_answering_toast_says_what_the_second_request_is() {
        assert!(not_answering_text(&EditorQuit::Window { confirmed: None }).contains("close the window again"));
        assert!(not_answering_text(&EditorQuit::Module).contains("kill-pane the editor again"));
        // T6-3: nvim may only be waiting on an external command; the kill loses what its swap file
        // does not have yet, and the toast says so before the second request.
        for quit in [EditorQuit::Window { confirmed: None }, EditorQuit::Module] {
            assert!(not_answering_text(&quit).contains("swap file"), "{quit:?}");
        }
    }

    /// R1-2: a close declined once nvim has exited retires the editor; declined while nvim runs, or
    /// with the editor already retired, it changes nothing.
    #[test]
    fn a_declined_close_retires_the_editor_only_once_nvim_is_gone() {
        assert!(retire_on_declined_close(true, false));
        assert!(
            !retire_on_declined_close(false, false),
            "a running nvim is a working editor"
        );
        assert!(!retire_on_declined_close(true, true), "already retired");
    }

    /// tmux's own text, with the module's name where tmux puts the pane's number.
    #[test]
    fn the_prompt_is_tmuxs_kill_pane_prompt() {
        assert_eq!(
            prompt(&ModuleId::terminal(), "terminal", KillScope::Module, 0, 0),
            "kill-pane terminal? (y/n)"
        );
        assert_eq!(
            prompt(&ModuleId::lua("notes"), "Notes", KillScope::Module, 3, 3),
            "kill-pane Notes? (y/n)"
        );
    }

    #[test]
    fn the_agents_prompt_says_how_many_tabs_are_running_or_queued() {
        let agent = ModuleId::agent();
        assert_eq!(
            prompt(&agent, "agent", KillScope::Module, 0, 0),
            "kill-pane agent? (y/n)"
        );
        assert_eq!(
            prompt(&agent, "agent", KillScope::Module, 1, 0),
            "kill-pane agent? 1 running (y/n)"
        );
        assert_eq!(
            prompt(&agent, "agent", KillScope::Module, 2, 1),
            "kill-pane agent? 2 running, 1 queued (y/n)"
        );
    }

    #[test]
    fn the_editors_prompt_says_it_will_not_come_back() {
        assert_eq!(
            prompt(&ModuleId::editor(), "editor", KillScope::Module, 5, 5),
            "kill-pane editor? nvim cannot be reopened in this window (y/n)"
        );
    }

    /// The last module on screen: the kill closes Eitri, and the one question also carries the
    /// window close's own `N running, M queued` (it is not asked a second time), for every module.
    #[test]
    fn the_last_modules_prompt_says_it_closes_eitri_and_what_is_running() {
        assert_eq!(
            prompt(&ModuleId::editor(), "editor", KillScope::Window, 0, 0),
            "kill-pane editor? closes Eitri (y/n)"
        );
        assert_eq!(
            prompt(&ModuleId::editor(), "editor", KillScope::Window, 2, 1),
            "kill-pane editor? closes Eitri, 2 running, 1 queued (y/n)"
        );
        assert_eq!(
            prompt(&ModuleId::terminal(), "terminal", KillScope::Window, 1, 0),
            "kill-pane terminal? closes Eitri, 1 running (y/n)"
        );
        assert_eq!(
            prompt(&ModuleId::agent(), "agent", KillScope::Window, 0, 3),
            "kill-pane agent? closes Eitri, 3 queued (y/n)"
        );
    }

    /// What `y` said yes to is the window close's prompt as it stood when asked. The close does not
    /// ask again unless that changed into something else still worth asking about.
    #[test]
    fn the_close_asks_again_only_if_what_was_running_changed() {
        assert!(close_is_confirmed(None, None));
        assert!(
            close_is_confirmed(Some("close window? 1 running (y/n)"), None),
            "it finished"
        );
        assert!(close_is_confirmed(
            Some("close window? 1 running (y/n)"),
            Some("close window? 1 running (y/n)")
        ));
        assert!(!close_is_confirmed(None, Some("close window? 1 running (y/n)")));
        assert!(!close_is_confirmed(
            Some("close window? 1 running (y/n)"),
            Some("close window? 2 running (y/n)")
        ));
    }

    #[test]
    fn a_killed_module_goes_back_to_its_first_launch_place_or_stays() {
        let decls = [ModuleDecl {
            id: ModuleId::lua("side"),
            placement: Placement::RightOfRoot,
        }];
        assert_eq!(
            reopen(&ModuleId::terminal(), &decls),
            Reopen::At(Placement::BelowEditor)
        );
        assert_eq!(
            reopen(&ModuleId::lua("side"), &decls),
            Reopen::At(Placement::RightOfRoot)
        );
        assert_eq!(reopen(&ModuleId::lua("unknown"), &decls), Reopen::InPlace);
        assert_eq!(reopen(&ModuleId::agent(), &decls), Reopen::InPlace);
        assert_eq!(reopen(&ModuleId::editor(), &decls), Reopen::Never);
    }
}
