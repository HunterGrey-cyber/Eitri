//! `prefix x` kills the selected module after a y/n question in `close_prompt`.
//! The question names the module. Each module ends its running content as follows.
//!
//! A terminal receives SIGHUP and returns to its initial hidden placement; `prefix t` starts
//! a fresh shell. A Lua panel's web process is terminated and the module returns to its hidden
//! `position`; its key loads the page again. The agent closes all session tabs as `prefix &`
//! closes one, keeping sessions resumable, then hides in place. `prefix a` opens its session chooser.
//!
//! The editor receives `:confirm qall`, so nvim prompts for unsaved buffers. Cancelling keeps it
//! open. Once nvim quits, the editor cannot restart in this window because `LiveHarness` has
//! a winit event loop that can run only once per process. The confirmation prompt explains this.
//!
//! Killing the last visible module closes the window ([`KillScope::Window`]). A single question
//! includes the window close's running and queued counts, and `y` confirms both actions.
//! The editor still goes through `:confirm qall`, and the window waits for nvim to exit.
//!
//! Every window close follows the same editor quit path ([`EditorQuit::Window`]), including the
//! top bar, compositor, last-module kill, and the last terminal's `exit`. `:qa!` would discard
//! unsaved buffers and delete swap files. The quit request uses `nvim_exec_lua` rather than typed
//! keys, which nvim could consume as input for a pending command such as `f`.
//! A hidden or zoomed-away editor is revealed for its prompt ([`reveal_for_prompt`]); one that was
//! hidden is hidden again if nvim quits, preserving the saved arrangement. A second request while
//! nvim is asking is refused with its current [`NvimState`]. Only after nvim has failed to answer
//! for [`NVIM_QUIT_PATIENCE`] does [`quit_request`] kill it instead.
//!
//! If a window close is declined after nvim exits on its own, the editor is retired with
//! [`Reopen::Never`] through [`retire_on_declined_close`]. This avoids leaving an empty editor
//! holding keyboard focus. `prefix e` explains that it cannot restart in this window.
//!
//! This module holds display-free decisions: prompt text, each module's `Reopen` behavior, and
//! individual editor quit decisions. `editor_quit` coordinates them against a host interface;
//! `main.rs` carries them out.

use std::time::{Duration, Instant};

use neovibe_core::layout::{KillScope, Layout, ModuleDecl, ModuleId, ModuleKind, Placement, Reopen};
use neovide_editor::CallWatch;

/// The y/n `prefix x` asks. `title` is the module's name as the tray and the strip show it;
/// `running`/`queued` are the agent's tabs with a turn or a connect in flight and its queued
/// messages (the window-close prompt's two counts). For [`KillScope::Module`] they are the agent's
/// own consequences, ignored for any other module; for [`KillScope::Window`] -- the last module on
/// screen, whose kill closes neovibe -- they are the window close's, for every module, so this one
/// question stands in for that one too.
pub(crate) fn prompt(id: &ModuleId, title: &str, scope: KillScope, running: usize, queued: usize) -> String {
    let mut consequences = Vec::new();
    if scope == KillScope::Window {
        consequences.push("closes neovibe".to_string());
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
    /// Whether this quit sent a request to nvim. Each send replaces the pane's watch, so a
    /// returned request means this quit was cancelled without nvim exiting and must be cleared.
    /// A quit joined to nvim after it already exited has no request of its own and sets this
    /// to `false`.
    pub(crate) sent: bool,
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
    /// Show an editor that is hidden or zoomed away, ending any zoom. Use `Layout::is_visible`:
    /// `is_shown` ignores zoom and could leave nvim's prompt off screen while the editor holds
    /// the keys, letting the next `n` or `d` answer an unseen prompt.
    pub(crate) show: bool,
    /// Hidden (not merely zoomed away): hidden again if nvim quits. A zoom is not saved
    /// (`neovibe_core::layout::persist`), so ending one changes nothing the window keeps.
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

/// Nvim's state during a quit, read from the pane's watch on that request.
/// `CallWatch` asks the fast `nvim_get_mode` call every 250 ms. Redraws cannot distinguish
/// a hung nvim from a dialog waiting for the user: both stop drawing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NvimState {
    /// The request came back -- its Lua returned, so nvim did not quit -- or nothing was sent.
    Returned,
    /// nvim's loop has not answered for [`NVIM_QUIT_PATIENCE`]: stuck (a Lua busy loop, a stopped
    /// process), or waiting on an external command (`system()`, `:!`, a formatter a save hook runs),
    /// during which nvim answers nothing (see [`NvimState::Busy`]). A second request then kills it,
    /// and [`not_answering_text`] says what that costs.
    NotAnswering,
    /// Its `:confirm` dialog is up (`nvim_get_mode` says `r?`).
    Asking,
    /// It waits for a key before it takes the request (`blocking`: after `f`, at a hit-enter prompt).
    WaitingForKey,
    /// Nvim is answering but is neither in a dialog nor waiting for a key. It may be about to
    /// ask or running work that keeps its loop alive, such as a write hook in `vim.wait()`,
    /// which `Ctrl+C` interrupts. On nvim 0.12.5, `vim.fn.system('sleep 3')` and `:silent !sleep 3`
    /// leave `nvim_get_mode` unanswered until they finish. External commands lasting at least
    /// [`NVIM_QUIT_PATIENCE`] therefore read as [`NvimState::NotAnswering`].
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
    /// nvim's loop has not answered for [`NVIM_QUIT_PATIENCE`]: killed, its swap files kept.
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

/// Toast for nvim's loop not answering for [`NVIM_QUIT_PATIENCE`]. It explains that a second
/// request kills nvim. SIGKILL keeps the swap file but loses changes not yet written to it;
/// nvim may be waiting on a long external command rather than stuck.
pub(crate) fn not_answering_text(quit: &EditorQuit) -> &'static str {
    match quit {
        EditorQuit::Window { .. } => {
            "nvim is not responding to the quit \u{2014} close the window again to kill it \
             (changes not yet in its swap file are lost)"
        }
        EditorQuit::Module => {
            "nvim is not responding to the quit \u{2014} kill-pane the editor again to kill it \
             (changes not yet in its swap file are lost)"
        }
    }
}

/// Whether a declined window close retires the editor: nvim must have exited and the
/// editor must not already be retired. Declining while nvim runs leaves a working editor.
pub(crate) fn retire_on_declined_close(nvim_exited: bool, editor_gone: bool) -> bool {
    nvim_exited && !editor_gone
}

/// Whether a window close after a `y` to [`KillScope::Window`] may skip its own y/n. `confirmed` is
/// the window close's prompt (`neovibe_core::tabs::window_close_prompt`) as it stood when the kill
/// was asked, `now` as it stands at the close: the editor's close waits for nvim, whose own
/// `:confirm qall` may be answered much later. Nothing worth asking about now, or exactly what was
/// said yes to: no second question. Anything else is asked.
pub(crate) fn close_is_confirmed(confirmed: Option<&str>, now: Option<&str>) -> bool {
    now.is_none() || now == confirmed
}

/// Where the layout leaves a killed module (`neovibe_core::layout::Reopen`). `decls` are the Lua
/// panels as `init.lua` registered them.
pub(crate) fn reopen(id: &ModuleId, decls: &[ModuleDecl]) -> Reopen {
    match id.kind() {
        // `BelowEditorAndAgent`, not `BelowRoot`: a terminal killed next to a Lua `side` panel
        // comes back under `[editor | agent]` only (as a first launch puts it), not full width
        // below the side panel too (`neovibe_core::layout::tree::place_new`'s own doc, task 6).
        ModuleKind::Terminal => Reopen::At(Placement::BelowEditorAndAgent),
        ModuleKind::LuaWebview => decls
            .iter()
            .find(|d| d.id == *id)
            .map_or(Reopen::InPlace, |d| Reopen::At(d.placement)),
        ModuleKind::Agent | ModuleKind::Canvas => Reopen::InPlace,
        ModuleKind::Editor => Reopen::Never,
    }
}

/// The Lua nvim is asked to run to quit the editor (`neovibe_core::layout::kill::editor_quit_lua`).
pub(crate) use neovibe_core::layout::kill::editor_quit_lua;

#[cfg(test)]
mod tests {
    use super::*;

    fn asked(generation: u32, quit: EditorQuit) -> Asked {
        Asked {
            generation,
            quit,
            revealed: false,
            sent: true,
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

    /// With no quit in flight, an editor quit, including a window close, asks nvim. Refuse a
    /// second request while it is pending because a second `:confirm qall` would nest. Say what
    /// nvim is doing. Once the first request returns without quitting, ask again, whether or
    /// not the shim's cancel letter arrived.
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

    /// A dialog waiting for the user and a hung nvim can both stop drawing. Detect a stopped
    /// loop through the watch's unanswered calls, not redraws or elapsed time since the send.
    /// A dialog left open for minutes still belongs to the user. If nvim stops answering after
    /// drawing it, such as after Save enters a hanging hook, the next request kills it.
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

    /// Show an editor that is zoomed away even though `is_shown` is true. Since it was not
    /// hidden before the prompt, do not hide it afterwards.
    #[test]
    fn a_hidden_or_zoomed_away_editor_is_brought_on_screen_for_the_prompt() {
        use neovibe_core::layout::{hide, Frame, Size};
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

    /// A declined close retires the editor only after nvim exits. It changes nothing while
    /// nvim runs or after the editor is already retired.
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

    /// The last module on screen: the kill closes neovibe, and the one question also carries the
    /// window close's own `N running, M queued` (it is not asked a second time), for every module.
    #[test]
    fn the_last_modules_prompt_says_it_closes_neovibe_and_what_is_running() {
        assert_eq!(
            prompt(&ModuleId::editor(), "editor", KillScope::Window, 0, 0),
            "kill-pane editor? closes neovibe (y/n)"
        );
        assert_eq!(
            prompt(&ModuleId::editor(), "editor", KillScope::Window, 2, 1),
            "kill-pane editor? closes neovibe, 2 running, 1 queued (y/n)"
        );
        assert_eq!(
            prompt(&ModuleId::terminal(), "terminal", KillScope::Window, 1, 0),
            "kill-pane terminal? closes neovibe, 1 running (y/n)"
        );
        assert_eq!(
            prompt(&ModuleId::agent(), "agent", KillScope::Window, 0, 3),
            "kill-pane agent? closes neovibe, 3 queued (y/n)"
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
            Reopen::At(Placement::BelowEditorAndAgent)
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
