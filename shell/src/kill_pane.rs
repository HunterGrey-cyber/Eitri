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
//! This file holds only what is decided without a display: the prompt's text and where each kind of
//! module is left (`Reopen`). `main.rs` does the rest.

use neovibe_core::layout::{ModuleDecl, ModuleId, ModuleKind, Placement, Reopen};

/// The y/n `prefix x` asks. `title` is the module's name as the tray and the strip show it;
/// `running`/`queued` are the agent's tabs with a turn or a connect in flight and its queued
/// messages (the window-close prompt's two counts), ignored for any other module.
pub(crate) fn prompt(id: &ModuleId, title: &str, running: usize, queued: usize) -> String {
    let mut consequences = Vec::new();
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
            prompt(&ModuleId::terminal(), "terminal", 0, 0),
            "kill-pane terminal? (y/n)"
        );
        assert_eq!(prompt(&ModuleId::lua("notes"), "Notes", 3, 3), "kill-pane Notes? (y/n)");
    }

    #[test]
    fn the_agents_prompt_says_how_many_tabs_are_running_or_queued() {
        let agent = ModuleId::agent();
        assert_eq!(prompt(&agent, "agent", 0, 0), "kill-pane agent? (y/n)");
        assert_eq!(prompt(&agent, "agent", 1, 0), "kill-pane agent? 1 running (y/n)");
        assert_eq!(
            prompt(&agent, "agent", 2, 1),
            "kill-pane agent? 2 running, 1 queued (y/n)"
        );
    }

    #[test]
    fn the_editors_prompt_says_it_will_not_come_back() {
        assert_eq!(
            prompt(&ModuleId::editor(), "editor", 5, 5),
            "kill-pane editor? nvim cannot be reopened in this window (y/n)"
        );
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
