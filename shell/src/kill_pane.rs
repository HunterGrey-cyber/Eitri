//! `prefix x`, tmux's `kill-pane` (owner, 2026-09-26: "prefix x应该是直接关掉pane而不是隐藏"). Stock
//! tmux binds `x` to `confirm-before -p "kill-pane #P? (y/n)" kill-pane` (checked against a
//! throwaway `tmux -L xprobe -f /dev/null` server, tmux next-3.7), so neovibe asks first too, in the
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
//!   nvim quits does the module go. It cannot come back in this window (`neovibe_core::layout::kill`'s
//!   module doc: the fork's `LiveHarness` owns a winit event loop, which is once per process), and
//!   the prompt says so. Cancelled in nvim, nothing closes.
//!
//! - **the last module on screen** (2026-09-26, later; owner: "prefix x对neovide窗口不生效，不能触发
//!   neovibe关闭"): tmux closes the window when its last pane is killed, and ends when its last window
//!   is, so `x` here closes neovibe ([`KillScope::Window`]). One question: it names what the window
//!   close would have asked about (`N running, M queued`), and `y` is taken as the answer to both. The
//!   editor still goes through `:confirm qall` first, and the window closes when nvim exits.
//!
//! This file holds only what is decided without a display: the prompt's text and where each kind of
//! module is left (`Reopen`). `main.rs` does the rest.

use neovibe_core::layout::{KillScope, ModuleDecl, ModuleId, ModuleKind, Placement, Reopen};

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
/// ([`KillScope::Module`]), or close the window ([`KillScope::Window`]), carrying the window close's
/// prompt as it stood when the kill was asked ([`close_is_confirmed`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EditorQuit {
    Module,
    Window { confirmed: Option<String> },
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
        ModuleKind::Terminal => Reopen::At(Placement::BelowRoot),
        ModuleKind::LuaWebview => decls
            .iter()
            .find(|d| d.id == *id)
            .map_or(Reopen::InPlace, |d| Reopen::At(d.placement)),
        ModuleKind::Agent | ModuleKind::Canvas => Reopen::InPlace,
        ModuleKind::Editor => Reopen::Never,
    }
}

/// The nvim command a kill of the editor types (`neovibe_core::layout::kill::EDITOR_QUIT_KEYS`: in
/// core, because `shell/src` holds no key-notation literal, `shell_src_writes_no_accelerator_literal`).
pub(crate) use neovibe_core::layout::kill::EDITOR_QUIT_KEYS;

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(reopen(&ModuleId::terminal(), &decls), Reopen::At(Placement::BelowRoot));
        assert_eq!(
            reopen(&ModuleId::lua("side"), &decls),
            Reopen::At(Placement::RightOfRoot)
        );
        assert_eq!(reopen(&ModuleId::lua("unknown"), &decls), Reopen::InPlace);
        assert_eq!(reopen(&ModuleId::agent(), &decls), Reopen::InPlace);
        assert_eq!(reopen(&ModuleId::editor(), &decls), Reopen::Never);
    }
}
