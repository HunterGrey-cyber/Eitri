//! Which pane has keyboard focus, and how the window says so without drawing anything extra.
//!
//! Before this existed nothing in the window said which pane was focused. The composer's caret
//! used to do it by accident: `Ctrl+l` put focus in the panel's textarea, and a blinking caret
//! appeared there. The panel-as-document rework (`9dd39f2`) lands focus in BROWSE instead, with no
//! caret, so that signal went away. The owner hit this on an installed build: "切换到右边之后没有提示
//! ... 我没法确定我在哪个pane". The three-mode keyboard model is kept. This module puts a signal back.
//!
//! **The signal is the cursor, not a frame** (2026-09-19, the same day). The first version drew a
//! 2px outline around the focused pane, then a line under it; the owner found the first ugly and
//! asked for a design that needs no extra lines at all. The rule now is the one terminals use:
//! **a solid cursor means the keys go here, a hollow one means they do not -- and it shows where
//! you will land when you come back.**
//!
//! - The editor forwards focus to Neovide (`NeovideEditorPane::set_focused`, fork `8f043a2`),
//!   which also tells nvim (`FocusGained`/`FocusLost`). **Correction (2026-09-19, later): the
//!   editor draws no cursor at all when unfocused, not a hollow one.** Neovide's own hollow block
//!   was what shipped first; the owner saw it on an installed build and asked for it to go.
//!   `neovide_editor::HIDE_UNFOCUSED_CURSOR_CMD` (fork `0862c35`) removes it; the panel's hollow
//!   sign cell below is unchanged. So "where you will land" is shown by the panel, not the editor.
//! - The agent panel gets a `pane_focus` envelope (`AgentPanelHandle::set_pane_focused`) and draws
//!   its current row's sign cell the same way: solid with focus, hollow without.
//! - **Correction (2026-09-19, later): the status bar that named the focused pane is deleted**, at
//!   the owner's choice. The cursors above are the whole signal now.
//!
//! **One source.** GTK's own focus widget (`GtkWindow:focus-widget`) is the only input, and every
//! change to it re-runs one function that updates all of the above, so they agree by construction.
//!
//! **What "focused" means for the cursors: the pane holds the window's focus widget AND the window
//! is active.** That is what a real Neovide window does -- its cursor goes hollow when you alt-tab
//! away -- and what the panel's mode block claims: a bright BROWSE says keys typed now go there.
//! `notify::is-active` re-runs the same function. The module that last held focus is written to
//! the layout's `focus` (`install`'s `on_owner`) whether or not the window is active: it is where
//! the top bar's `Ctrl+j` goes back to, and which module the `Ctrl+a` prefix zooms. The layout
//! refuses a hidden module there (`Layout::set_focus`). **Not looked at on a screen.**
//!
//! **Keyed by `ModuleId` since the modules design's P1** (docs/superpowers/specs/
//! 2026-09-23-modules-and-canvas-design.md): a pane is a module, named by what it is rather than by
//! which of three fixed slots it sat in. Index 0/1/2 meant editor/panel/bottom only because the
//! layout was three slots; once the layout is data, an index says nothing.
//!
//! **Read live since modules P2.** `install` asks the grid for its modules on every focus change
//! (`ModuleGrid::live_hosts`), and remembers each module's last answer by its id, so a module added
//! after the window is up -- P3's canvas -- is tracked from its first focus change, where P1 handed
//! this a list taken once at startup.

use std::collections::BTreeMap;

use eitri_core::layout::ModuleId;
use gtk4::prelude::*;

use crate::module_grid::ModuleHosts;

/// The module whose host is nearest to the focus widget on its way up to the window, walking `chain`
/// in order (the focus widget first, then each ancestor). `None` means focus is in none of them: the
/// top bar's buttons, say, or no focus widget at all. (Modules P2 folded the index-answering
/// `owning_pane` into this: nothing asks by position any more.)
///
/// Generic so the decision can be unit-tested without a display. The GTK half only builds the
/// chain.
pub(crate) fn owning_module<T: PartialEq>(
    chain: impl IntoIterator<Item = T>,
    modules: &[(ModuleId, T)],
) -> Option<ModuleId> {
    chain
        .into_iter()
        .find_map(|w| modules.iter().find(|(_, host)| *host == w))
        .map(|(id, _)| id.clone())
}

/// Which modules' "has the keys" answer changed, as `(id, now)`, in `modules`' order. `last` holds
/// each module's previous answer (none before its first report), `focused` is the module holding
/// focus, and `active` is whether the window is. Only changes are reported, so a focus move between
/// two widgets inside the editor sends nothing, and every module gets one report the first time it
/// is seen -- at startup, or whenever it joined the grid.
pub(crate) fn focus_changes(
    last: &BTreeMap<ModuleId, bool>,
    modules: &[ModuleId],
    focused: Option<&ModuleId>,
    active: bool,
) -> Vec<(ModuleId, bool)> {
    modules
        .iter()
        .filter_map(|id| {
            let now = focused == Some(id) && active;
            (last.get(id) != Some(&now)).then(|| (id.clone(), now))
        })
        .collect()
}

/// Wires the tracker to `window` and applies it once for the current focus. `hosts` answers, each
/// time, with every module and the host whose subtree counts as that module.
///
/// `on_owner(id)` is called whenever focus lands inside module `id`, whether or not the window is
/// active: that module is where the top bar's `Ctrl+j` goes back to and what the `Ctrl+a` prefix
/// zooms. `on_has_keys(id, has_keys)` is called when a module's "has the keys" answer changes.
pub(crate) fn install(
    window: &gtk4::ApplicationWindow,
    hosts: ModuleHosts,
    on_owner: impl Fn(&ModuleId) + 'static,
    on_has_keys: impl Fn(&ModuleId, bool) + 'static,
) {
    let last: std::cell::RefCell<BTreeMap<ModuleId, bool>> = std::cell::RefCell::new(BTreeMap::new());
    let apply = move |window: &gtk4::ApplicationWindow| {
        // Asked on every focus change, never once before this closure: a module added after
        // `present()` (P3's canvas) must be found. known limit: nothing headless holds that --
        // hoisting this line above `let apply` compiles and every test passes -- and P2 adds no
        // module after startup, so no P2 GUI pass can see it either; P3's checklist owes focus
        // tracking, `Ctrl+a Ctrl+a` and `Ctrl+h/j/k/l` on a module added late (Task 8's review,
        // minor 2).
        let modules = hosts();
        let chain = std::iter::successors(gtk4::prelude::GtkWindowExt::focus(window), |w| w.parent());
        let focused = owning_module(chain, &modules);
        if let Some(id) = &focused {
            on_owner(id);
        }
        // Said on every focus change, not only on a module's own transition, because the two states
        // that look identical from the per-module callback are exactly the two a "I cannot type"
        // report has to tell apart (2026-09-20, after one such report this log could not explain):
        // the window went inactive -- ordinary, the keys are in another application -- or the
        // window is STILL ACTIVE and focus is sitting on a widget that is in no module, in which
        // case the keys are going nowhere and that is a bug. `has_keys` is false either way.
        // The widget's type name is what names the culprit: the top bar after `Ctrl+k`, HINT's
        // overlay, or `None` after a control removed itself.
        println!(
            "[pane_focus] active={} focus={} owning_module={:?}",
            window.is_active(),
            gtk4::prelude::GtkWindowExt::focus(window)
                .map(|w| w.type_().name().to_string())
                .unwrap_or_else(|| "none".to_string()),
            focused.as_ref().map(ModuleId::as_str),
        );
        let ids: Vec<ModuleId> = modules.into_iter().map(|(id, _)| id).collect();
        let changes = focus_changes(&last.borrow(), &ids, focused.as_ref(), window.is_active());
        for (id, now) in changes {
            last.borrow_mut().insert(id.clone(), now);
            on_has_keys(&id, now);
        }
    };
    apply(window);
    let apply = std::rc::Rc::new(apply);
    let on_active = apply.clone();
    window.connect_notify_local(Some("focus-widget"), move |window, _| apply(window));
    window.connect_notify_local(Some("is-active"), move |window, _| on_active(window));
}

/// The module holding the window's focus widget right now, or `None` (the top bar, or nothing).
/// Unlike the remembered owner, this is where a key typed now would go -- what `Ctrl+a Ctrl+a`
/// hands its `Ctrl+a` to (spec 2026-09-19-window-modes-design.md §3.2).
pub(crate) fn focused_module(
    window: &gtk4::ApplicationWindow,
    modules: &[(ModuleId, gtk4::Widget)],
) -> Option<ModuleId> {
    owning_module(
        std::iter::successors(gtk4::prelude::GtkWindowExt::focus(window), |w| w.parent()),
        modules,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn last(answers: &[(ModuleId, bool)]) -> BTreeMap<ModuleId, bool> {
        answers.iter().cloned().collect()
    }

    #[test]
    fn every_module_gets_one_first_report_then_only_changes() {
        let (e, a) = (ModuleId::editor(), ModuleId::agent());
        let both = [e.clone(), a.clone()];
        assert_eq!(
            focus_changes(&last(&[]), &both, Some(&e), true),
            vec![(e.clone(), true), (a.clone(), false)]
        );
        assert_eq!(
            focus_changes(&last(&[(e.clone(), true), (a.clone(), false)]), &both, Some(&e), true),
            vec![]
        );
        assert_eq!(
            focus_changes(&last(&[(e.clone(), true), (a.clone(), false)]), &both, Some(&a), true),
            vec![(e, false), (a, true)]
        );
    }

    /// Modules P2 (the P1 plan's open item): a module the grid gains after `install` -- P3's canvas
    /// -- is reported the first time a focus change sees it, and only it: the modules already known
    /// report nothing new.
    #[test]
    fn a_module_added_later_gets_its_first_report_and_nothing_else_moves() {
        let (e, a, canvas) = (
            ModuleId::editor(),
            ModuleId::agent(),
            ModuleId::parse("canvas").unwrap(),
        );
        let known = last(&[(e.clone(), true), (a.clone(), false)]);
        assert_eq!(
            focus_changes(&known, &[e.clone(), a.clone(), canvas.clone()], Some(&e), true),
            vec![(canvas, false)]
        );
    }

    #[test]
    fn the_owning_module_is_named_by_its_id_not_its_position() {
        let modules = [
            (ModuleId::agent(), "agent-host"),
            (ModuleId::editor(), "editor-host"),
            (ModuleId::lua("notes"), "notes-host"),
        ];
        assert_eq!(
            owning_module(["editor-host", "grid", "window"], &modules),
            Some(ModuleId::editor())
        );
        assert_eq!(
            owning_module(["textarea", "notes-host", "grid"], &modules),
            Some(ModuleId::lua("notes"))
        );
        assert_eq!(owning_module(["reload-button", "topbar"], &modules), None);
        assert_eq!(
            owning_module(Vec::<&str>::new(), &modules),
            None,
            "no focus widget at all"
        );
    }

    #[test]
    fn a_module_nested_in_another_module_owns_its_own_focus() {
        let modules = [(ModuleId::agent(), "outer"), (ModuleId::lua("inner"), "inner")];
        assert_eq!(
            owning_module(["inner", "outer", "window"], &modules),
            Some(ModuleId::lua("inner"))
        );
    }

    #[test]
    fn an_inactive_window_gives_no_pane_the_keys() {
        let (e, a) = (ModuleId::editor(), ModuleId::agent());
        let both = [e.clone(), a.clone()];
        // Alt-tab away: the editor stops drawing its cursor.
        assert_eq!(
            focus_changes(&last(&[(e.clone(), true), (a.clone(), false)]), &both, Some(&e), false),
            vec![(e.clone(), false)]
        );
        // Focus on the top bar: neither module has the keys.
        assert_eq!(
            focus_changes(&last(&[(e, false), (a.clone(), true)]), &both, None, true),
            vec![(a, false)]
        );
    }
}
