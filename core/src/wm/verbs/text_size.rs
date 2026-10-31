//! Text-size decisions, with writes emitted in editor, chat, page, then terminal order.

use crate::keymap::TextChange;
use crate::layout::ModuleId;
use crate::wm::host::{Batch, ChatOp, EditorCall, HostOp, PluginOp, TermOp, TextSizeEvent};
use crate::wm::text_size::{describe_write, route_focused_module, TextSizeTarget, TextStep};
use crate::wm::window::{EditorPresence, WinId, Window};

/// A focused text-size step uses the module holding the keys at the time of the action.
pub fn run(w: &mut Window, win: WinId, change: TextChange, keys: Option<&ModuleId>, batch: &mut Batch) {
    let step = change.into();
    let Some(state) = w.text_size.clone() else { return };
    match route_focused_module(keys) {
        // The keymap's `text.larger`/`text.smaller`/`text.reset` (not bound by default; `Ctrl`+wheel
        // over a pane is the default per-pane route) with the editor focused.
        TextSizeTarget::Editor => {
            // With no nvim to write `g:neovide_scale_factor` to, an editor-only step logs and does nothing.
            if *w.presence() == EditorPresence::None {
                w.warn(format!("[text-size] {step:?} (editor): no editor pane, ignoring"));
                return;
            }
            let (state, write) = state.apply_editor_step(step);
            w.text_size = Some(state);
            write_scale(win, write, batch);
            push_sizes(w, win, batch);
            log_write(w, "editor", write);
        }
        // The keymap's `text.larger`/`text.smaller`/`text.reset` with the panel focused. Never touches
        // nvim: only R moves.
        TextSizeTarget::Panel => {
            w.text_size = Some(state.apply_panel_step(step));
            push_sizes(w, win, batch);
            log_panel(w, "panel");
        }
        TextSizeTarget::Terminal => {
            w.text_size = Some(state.apply_terminal_step(step));
            push_sizes(w, win, batch);
            let level = w.text_size.as_ref().unwrap().terminal_scale();
            w.warn(format!("[text-size] terminal {level:.2}"));
        }
        // The keymap's `text.larger`/`text.smaller`/`text.reset` (and `Ctrl`+wheel) with the page `page`
        // holding the keys. Never touches nvim: only that page's factor moves. A Lua panel with no view
        // attached (WebKit could not start, or a plugin page not built yet) takes the key and does nothing.
        TextSizeTarget::Page(module) => {
            if !w.zoom_pages.contains(&module) {
                w.warn(format!("[text-size] {step:?} ({module}): no page view, ignoring"));
                return;
            }
            w.text_size = Some(state.apply_page_step(&module, step));
            push_sizes(w, win, batch);
            let level = w.text_size.as_ref().unwrap().page_scale(&module);
            w.warn(format!("[text-size] page {module} {level:.2}"));
        }
        TextSizeTarget::Neither => {}
    }
}

/// `Ctrl+=`/`Ctrl+-`/`Ctrl+0`, and a key bound to `text.all.*`: every module that exists moves. A Lua `main` panel
/// hides the full window's editor rather than replacing it; a linked second window has none, and then there is no S
/// to write.
pub(in crate::wm) fn all(w: &mut Window, win: WinId, step: TextStep, batch: &mut Batch) {
    let Some(state) = w.text_size.clone() else { return };
    if *w.presence() == EditorPresence::None {
        w.warn(format!("[text-size] {step:?} (every module): no editor pane"));
        w.text_size = Some(state.apply_app_step(step));
        push_sizes(w, win, batch);
        log_panel(w, "panel");
    } else {
        let (state, write) = state.apply_both_step(step);
        w.text_size = Some(state);
        write_scale(win, write, batch);
        push_sizes(w, win, batch);
        log_write(w, "both", write);
    }
}

pub(in crate::wm) fn event(w: &mut Window, event: TextSizeEvent, batch: &mut Batch) {
    let Some(state) = w.text_size.clone() else { return };
    match event {
        TextSizeEvent::Both { win, step } => all(w, win, step, batch),
        TextSizeEvent::Pane { win, module, step } => {
            let change = match step {
                TextStep::Larger => TextChange::Larger,
                TextStep::Smaller => TextChange::Smaller,
                TextStep::Reset => TextChange::Reset,
            };
            run(w, win, change, Some(&module), batch);
        }
        TextSizeEvent::PageAttached { win, module } => {
            let state = state.page_shown(&module);
            let level = state.page_scale(&module);
            w.text_size = Some(state);
            w.zoom_pages.insert(module.clone());
            batch.push_effect(HostOp::Plugin(PluginOp::SetZoom {
                win,
                module: module.clone(),
                level,
            }));
            w.warn(format!("[text-size] page {module} shown at {level:.2}"));
        }
        TextSizeEvent::Configured { scale, .. } => w.text_size = Some(state.with_configured(scale as f32)),
        TextSizeEvent::Base { win, px } => {
            w.text_size = Some(state.with_base(px));
            push_sizes(w, win, batch);
            let panel = w.text_size.as_ref().unwrap().panel_px();
            w.warn(format!(
                "[text-size] base {px:.2}px (the editor's font), panel {panel:.1}px"
            ));
        }
    }
}

pub(in crate::wm) fn reported(w: &mut Window, win: WinId, scale: f64, batch: &mut Batch) {
    let Some(state) = w.text_size.take() else { return };
    w.text_size = Some(state.nvim_reported_s(scale as f32));
    push_sizes(w, win, batch);
}

fn write_scale(win: WinId, write: Option<f32>, batch: &mut Batch) {
    if let Some(scale) = write {
        batch.push_effect(HostOp::Editor(EditorCall::SetScale {
            win,
            scale: scale as f64,
        }));
    }
}

/// The state moved: the panel's px, every page's zoom and the terminals' zoom follow it.
fn push_sizes(w: &Window, win: WinId, batch: &mut Batch) {
    let Some(state) = &w.text_size else { return };
    batch.push_effect(HostOp::Chat(ChatOp::SetFontSize {
        win,
        px: state.panel_px(),
    }));
    for module in &w.zoom_pages {
        batch.push_effect(HostOp::Plugin(PluginOp::SetZoom {
            win,
            module: module.clone(),
            level: state.page_scale(module),
        }));
    }
    batch.push_effect(HostOp::Terminal(TermOp::SetZoom {
        win,
        level: state.terminal_scale(),
    }));
}

fn log_write(w: &mut Window, target: &str, write: Option<f32>) {
    let state = w.text_size.as_ref().unwrap();
    w.warn(format!(
        "[text-size] {target} {} panel {:.2} ({:.1}px)",
        describe_write(write),
        state.panel_scale(),
        state.panel_px()
    ));
}

fn log_panel(w: &mut Window, target: &str) {
    let state = w.text_size.as_ref().unwrap();
    w.warn(format!(
        "[text-size] {target} {:.2} ({:.1}px)",
        state.panel_scale(),
        state.panel_px()
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::TextChange;
    use crate::layout::{Layout, ModuleId, Node};
    use crate::wm::text_size::{TextSize, TextStep};
    use crate::wm::verbs::rig::{ops, rig, WIN};
    use crate::wm::*;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    fn window() -> Window {
        Window::new(
            WindowSpec {
                role: WindowRole::Main,
                presence: EditorPresence::Owned,
                title: String::new(),
            },
            WindowParts {
                layout: Rc::new(RefCell::new(
                    Layout::new(Node::Leaf(ModuleId::editor()), ModuleId::editor()).unwrap(),
                )),
                workspaces: Rc::new(RefCell::new(crate::layout::Workspaces::single())),
                saver: None,
            },
            Timers {
                scheduler: crate::wm::fake::FakeScheduler::new(),
                next: Rc::new(Cell::new(1)),
            },
        )
    }

    fn font(px: f32) -> HostOp {
        HostOp::Chat(ChatOp::SetFontSize { win: WIN, px })
    }
    fn scale(scale: f64) -> HostOp {
        HostOp::Editor(EditorCall::SetScale { win: WIN, scale })
    }
    fn zoom(module: &ModuleId, level: f32) -> HostOp {
        HostOp::Plugin(PluginOp::SetZoom {
            win: WIN,
            module: module.clone(),
            level,
        })
    }

    fn terminal_zoom(level: f32) -> HostOp {
        HostOp::Terminal(TermOp::SetZoom { win: WIN, level })
    }

    #[test]
    fn a_text_step_routes_by_the_module_with_the_keys_and_emits_the_writes() {
        let page = ModuleId::lua("md");
        for (keys, attached, expected, line) in [
            (
                Some(ModuleId::editor()),
                false,
                vec![scale(1.1_f32 as f64), font(14.0), terminal_zoom(1.0)],
                "[text-size] editor 1.10 panel 1.00 (14.0px)",
            ),
            (
                Some(ModuleId::agent()),
                false,
                vec![font(14.0 * 1.1), terminal_zoom(1.0)],
                "[text-size] panel 1.10 (15.4px)",
            ),
            (
                Some(page.clone()),
                true,
                vec![font(14.0), zoom(&page, 1.1), terminal_zoom(1.0)],
                "[text-size] page lua:md 1.10",
            ),
            (
                Some(page.clone()),
                false,
                vec![],
                "[text-size] Larger (lua:md): no page view, ignoring",
            ),
            (
                Some(ModuleId::terminal()),
                false,
                vec![font(14.0), terminal_zoom(1.1)],
                "[text-size] terminal 1.10",
            ),
            (
                Some(ModuleId::parse("terminal#2").unwrap()),
                false,
                vec![font(14.0), terminal_zoom(1.1)],
                "[text-size] terminal 1.10",
            ),
            (None, false, vec![], ""),
        ] {
            let mut w = window();
            w.text_size = Some(TextSize::new(14.0, 1.0));
            if attached {
                w.zoom_pages.insert(page.clone());
            }
            let mut batch = Batch::default();
            run(&mut w, WIN, TextChange::Larger, keys.as_ref(), &mut batch);
            assert_eq!(ops(&batch), expected);
            if line.is_empty() {
                assert!(w.notes().is_empty());
            } else {
                assert_eq!(w.notes(), &[line]);
            }
        }
    }

    #[test]
    fn the_app_accelerators_move_both_and_nvims_echo_settles_the_pending_write() {
        let mut r = rig(Layout::new(Node::Leaf(ModuleId::editor()), ModuleId::editor()).unwrap());
        r.controller.set_text_size(WIN, 14.0, 1.0);
        let batch = r.controller.handle(HostEvent::TextSize(TextSizeEvent::Both {
            win: WIN,
            step: TextStep::Larger,
        }));
        assert_eq!(
            ops(&batch),
            vec![scale(1.1_f32 as f64), font(14.0 * 1.1), terminal_zoom(1.1)]
        );
        let echo = r
            .controller
            .handle(HostEvent::Editor(EditorEvent::ScaleReported { win: WIN, scale: 1.1 }));
        assert_eq!(ops(&echo), vec![font(14.0 * 1.1), terminal_zoom(1.1)]);
    }

    #[test]
    fn text_all_is_the_accelerators_action_wherever_the_keys_are() {
        for keys in [ModuleId::editor(), ModuleId::agent(), ModuleId::terminal()] {
            let mut r = rig(Layout::new(Node::Leaf(keys.clone()), keys).unwrap());
            r.controller.set_text_size(WIN, 14.0, 1.0);
            let batch = r.run(crate::keymap::Action::TextAll(TextChange::Larger));
            let writes: Vec<_> = ops(&batch)
                .into_iter()
                .filter(|op| {
                    matches!(
                        op,
                        HostOp::Editor(EditorCall::SetScale { .. })
                            | HostOp::Chat(ChatOp::SetFontSize { .. })
                            | HostOp::Terminal(TermOp::SetZoom { .. })
                    )
                })
                .collect();
            assert_eq!(
                writes,
                vec![scale(1.1_f32 as f64), font(14.0 * 1.1), terminal_zoom(1.1)]
            );
        }
    }

    /// A window of one module has one size: the pane keys there are the every-module action, so a reset clears the
    /// app size too rather than leaving a window that only ever had the one size where it was.
    #[test]
    fn in_a_module_window_the_pane_keys_are_the_every_module_action() {
        let mut r = rig(Layout::new(Node::Leaf(ModuleId::agent()), ModuleId::agent()).unwrap());
        let window = r.controller.window_mut(WIN).unwrap();
        window.tiling = crate::wm::window::Tiling::ModuleWindow;
        window.spec.presence = EditorPresence::None;
        r.controller.set_text_size(WIN, 14.0, 1.0);
        r.run(crate::keymap::Action::Text(TextChange::Larger));
        let batch = r.run(crate::keymap::Action::Text(TextChange::Reset));
        assert!(ops(&batch).contains(&font(14.0)), "{:?}", ops(&batch));
    }

    #[test]
    fn the_configured_size_is_what_the_reset_writes_and_it_moves_nothing_by_itself() {
        let mut r = rig(Layout::new(Node::Leaf(ModuleId::editor()), ModuleId::editor()).unwrap());
        r.controller.set_text_size(WIN, 14.0, 1.0);
        let told = r
            .controller
            .handle(HostEvent::TextSize(TextSizeEvent::Configured { win: WIN, scale: 1.25 }));
        assert!(told.is_empty());
        r.controller
            .handle(HostEvent::Editor(EditorEvent::ScaleReported { win: WIN, scale: 1.25 }));
        r.controller.handle(HostEvent::TextSize(TextSizeEvent::Both {
            win: WIN,
            step: TextStep::Larger,
        }));
        let reset = r.controller.handle(HostEvent::TextSize(TextSizeEvent::Both {
            win: WIN,
            step: TextStep::Reset,
        }));
        assert!(ops(&reset).contains(&scale(1.25)), "{:?}", ops(&reset));
    }

    #[test]
    fn page_attachment_and_base_changes_keep_the_existing_zoom() {
        let mut r = rig(Layout::new(Node::Leaf(ModuleId::editor()), ModuleId::editor()).unwrap());
        let page = ModuleId::lua("md");
        r.controller.set_text_size(WIN, 14.0, 1.5);
        assert_eq!(r.controller.page_zoom(WIN, &page), None);
        let shown = r.controller.handle(HostEvent::TextSize(TextSizeEvent::PageAttached {
            win: WIN,
            module: page.clone(),
        }));
        assert_eq!(ops(&shown), vec![zoom(&page, 1.5)]);
        assert_eq!(r.controller.page_zoom(WIN, &page), Some(1.5));
        let base = r
            .controller
            .handle(HostEvent::TextSize(TextSizeEvent::Base { win: WIN, px: 12.0 }));
        assert_eq!(ops(&base), vec![font(18.0), zoom(&page, 1.5), terminal_zoom(1.5)]);
        for op in base.ops() {
            assert_eq!(r.host.apply(op), Applied::Done);
        }
        assert_eq!(r.host.font_size(WIN), Some(18.0));
        assert_eq!(r.host.page_zoom(WIN, &page), Some(1.5));
        assert_eq!(r.host.terminal_zoom(WIN), Some(1.5));
        assert_eq!(r.controller.page_zoom(WinId(999), &page), None);
    }

    #[test]
    fn a_wheel_step_uses_its_named_pane_while_the_keys_stay_elsewhere() {
        for (module, expected) in [
            (
                ModuleId::editor(),
                vec![scale(1.1_f32 as f64), font(14.0), terminal_zoom(1.0)],
            ),
            (ModuleId::agent(), vec![font(14.0 * 1.1), terminal_zoom(1.0)]),
            (ModuleId::terminal(), vec![font(14.0), terminal_zoom(1.1)]),
            (
                ModuleId::parse("terminal#2").unwrap(),
                vec![font(14.0), terminal_zoom(1.1)],
            ),
        ] {
            for keys in [ModuleId::terminal(), ModuleId::editor()] {
                let mut r = rig(Layout::new(Node::Leaf(keys.clone()), keys.clone()).unwrap());
                r.controller.set_text_size(WIN, 14.0, 1.0);
                let batch = r.controller.handle(HostEvent::TextSize(TextSizeEvent::Pane {
                    win: WIN,
                    module: module.clone(),
                    step: TextStep::Larger,
                }));
                assert_eq!(ops(&batch), expected);
                assert_eq!(r.layout.borrow().focus(), &keys);
            }
        }
    }

    #[test]
    fn without_an_editor_the_app_accelerator_steps_the_panel_and_terminals() {
        let mut w = window();
        w.spec.presence = EditorPresence::None;
        w.text_size = Some(TextSize::new(14.0, 1.0));
        let mut batch = Batch::default();
        event(
            &mut w,
            TextSizeEvent::Both {
                win: WIN,
                step: TextStep::Larger,
            },
            &mut batch,
        );
        assert_eq!(ops(&batch), vec![font(14.0 * 1.1), terminal_zoom(1.1)]);
        assert_eq!(
            w.notes(),
            &[
                "[text-size] Larger (every module): no editor pane",
                "[text-size] panel 1.10 (15.4px)",
            ]
        );
        let mut batch = Batch::default();
        run(&mut w, WIN, TextChange::Larger, Some(&ModuleId::editor()), &mut batch);
        assert!(batch.is_empty());
        assert_eq!(
            w.notes().last().unwrap(),
            "[text-size] Larger (editor): no editor pane, ignoring"
        );
    }
}
