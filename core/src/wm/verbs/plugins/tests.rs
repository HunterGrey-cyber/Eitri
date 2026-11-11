use crate::keymap::action::{parse, Action, OptValue};
use crate::layout::fixture::fixture;
use crate::layout::rules::At;
use crate::layout::{Direction, Frame, ModuleId, OpenRule, Size};
use crate::wm::module::ModuleState;
use crate::wm::moves::{plan, MovePlan, MoveTarget};
use crate::wm::verbs::rig::*;
use crate::wm::window::Tiling;
use crate::wm::*;

fn load(name: &str) -> Action {
    parse("plugin.load", &[("name".into(), OptValue::Str(name.into()))], &[])
        .unwrap()
        .action
}

fn loaded(r: &mut Rig, shown_while: bool, own_rule: Option<OpenRule>) -> Batch {
    r.controller.handle(HostEvent::Plugin(PluginEvent::Loaded {
        win: WIN,
        module: ModuleId::lua("demo"),
        title: "Markdown".into(),
        shown_while,
        own_rule,
    }))
}

fn toast(r: &Rig) -> String {
    r.said().toast.unwrap().1
}

#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "plugin pages are not available on the Mac: plugin.load is refused there"
)]
fn plugin_load_requests_a_worker_and_refuses_a_duplicate_pending_name() {
    let mut r = rig(fixture("row(editor, agent)"));
    let batch = r.run(load("demo"));
    assert_eq!(
        ops(&batch),
        vec![HostOp::Plugin(PluginOp::Load {
            win: WIN,
            name: "demo".into()
        })]
    );
    let before = r.said();
    let batch = r.run(load("demo"));
    r.assert_refused(&batch, &before, "demo is already loading");
    assert_eq!(
        ops(&r.run(load("other"))),
        vec![HostOp::Plugin(PluginOp::Load {
            win: WIN,
            name: "other".into()
        })]
    );
}

/// The Mac has no plugin pages: a load is refused with that reason and starts no worker.
#[cfg(target_os = "macos")]
#[test]
fn plugin_load_is_refused_on_the_mac() {
    let mut r = rig(fixture("row(editor, agent)"));
    let before = r.said();
    let batch = r.run(load("demo"));
    r.assert_refused(&batch, &before, "plugin pages are not available on the Mac");
}

#[test]
fn plugin_load_is_refused_in_the_compositor_hub() {
    let mut r = rig(fixture("row(editor, agent)"));
    r.controller.window_mut(WIN).unwrap().tiling = Tiling::Hub;
    let before = r.said();
    let batch = r.run(load("demo"));
    r.assert_refused(&batch, &before, &super::compositor_refusal("demo"));
}

#[test]
fn plugin_load_is_refused_in_a_module_window() {
    let mut r = rig(fixture("row(editor, agent)"));
    r.controller.window_mut(WIN).unwrap().tiling = Tiling::ModuleWindow;
    let before = r.said();
    let batch = r.run(load("demo"));
    r.assert_refused(&batch, &before, &super::compositor_refusal("demo"));
}

#[test]
fn plugin_load_is_refused_in_window_two() {
    let mut r = rig(fixture("row(editor, agent)"));
    r.controller.window_mut(WIN).unwrap().spec.role = WindowRole::Linked { side: LinkSide::Second };
    let before = r.said();
    let batch = r.run(load("demo"));
    r.assert_refused(&batch, &before, "plugins load in window 1");
}

#[test]
fn plugin_load_is_refused_in_a_companion_window() {
    let mut r = rig(fixture("row(editor, agent)"));
    r.controller.window_mut(WIN).unwrap().spec.role = WindowRole::Companion;
    let before = r.said();
    let batch = r.run(load("demo"));
    r.assert_refused(&batch, &before, "plugins load in window 1");
}

#[test]
fn plugin_loaded_declares_then_opens_by_its_rule_and_builds_without_taking_keys() {
    let mut r = rig(fixture("row(editor, agent)"));
    r.run(load("demo"));
    let page = ModuleId::lua("demo");
    let keys = r.layout.borrow().focus().clone();
    let batch = loaded(
        &mut r,
        false,
        Some(OpenRule {
            at: Some(At::Beside(Direction::Down)),
            focus: true,
            ..OpenRule::default()
        }),
    );
    let all = ops(&batch);
    let create = all.iter().position(|op| matches!(op, HostOp::CreateSurface { module, kind: SurfaceKind::PluginPage { plugin }, .. } if module == &page && plugin == "demo")).unwrap();
    let map = all
        .iter()
        .position(|op| matches!(op, HostOp::Map { module, .. } if module == &page))
        .unwrap();
    let build = all
        .iter()
        .position(|op| matches!(op, HostOp::Plugin(PluginOp::Build { module, .. }) if module == &page))
        .unwrap();
    assert!(create < map && map < build, "{all:#?}");
    assert!(!all.iter().any(|op| matches!(op, HostOp::Focus { .. })));
    assert_eq!(*r.layout.borrow().focus(), keys);
    assert_eq!(
        r.controller.lifecycle(WIN).unwrap().state(&page),
        Some(&ModuleState::Shown)
    );
    assert_eq!(toast(&r), "Markdown loaded for this run");
    let layout = r.layout.borrow();
    let frame = crate::layout::arrange(&layout, &Frame::new(Size { w: 1000, h: 800 }, 1));
    let anchor = frame.rect_of(&keys).unwrap();
    let rect = frame.rect_of(&page).unwrap();
    assert!(rect.y >= anchor.y + anchor.h, "the page opens below its anchor");
}

#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "plugin pages are not available on the Mac: plugin.load is refused there"
)]
fn plugin_shown_while_is_deferred_until_a_show_and_keeps_its_own_rule() {
    let mut r = rig(fixture("row(editor, agent)"));
    r.run(load("demo"));
    let page = ModuleId::lua("demo");
    let batch = loaded(
        &mut r,
        true,
        Some(OpenRule {
            at: Some(At::Beside(Direction::Down)),
            ..OpenRule::default()
        }),
    );
    assert!(ops(&batch).iter().any(
        |op| matches!(op, HostOp::CreateSurface { module, kind: SurfaceKind::PluginPage { .. }, .. } if module == &page)
    ));
    assert!(!ops(&batch)
        .iter()
        .any(|op| matches!(op, HostOp::Map { .. } | HostOp::Plugin(PluginOp::Build { .. }))));
    assert!(!r.layout.borrow().contains(&page));
    assert_eq!(
        r.controller.lifecycle(WIN).unwrap().state(&page),
        Some(&ModuleState::Hosted)
    );
    assert_eq!(toast(&r), "Markdown loaded; it shows while an agent waits");
    let batch = r.run(load("demo"));
    assert!(ops(&batch)
        .iter()
        .any(|op| matches!(op, HostOp::Plugin(PluginOp::Build { module, .. }) if module == &page)));
    assert_eq!(toast(&r), "Markdown is already loaded; shown");
}

#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "plugin pages are not available on the Mac: plugin.load is refused there"
)]
fn plugin_already_loaded_is_shown_without_a_second_load_or_build() {
    let mut r = rig(fixture("row(editor, agent)"));
    r.run(load("demo"));
    loaded(&mut r, false, None);
    let page = ModuleId::lua("demo");
    r.run(Action::Module(page.clone()));
    r.run(Action::ModuleHide);
    let batch = r.run(load("demo"));
    assert!(r.layout.borrow().is_shown(&page));
    assert!(!ops(&batch)
        .iter()
        .any(|op| matches!(op, HostOp::Plugin(PluginOp::Load { .. } | PluginOp::Build { .. }))));
    assert_eq!(toast(&r), "Markdown is already loaded; shown");
}

#[test]
fn plugin_session_only_pages_cannot_move_to_another_window() {
    let mut r = rig(fixture("row(editor, agent)"));
    r.run(load("demo"));
    loaded(&mut r, false, None);
    let w = r.controller.window_mut(WIN).unwrap();
    assert!(
        matches!(plan(w, &ModuleId::lua("demo"), &MoveTarget::Peer(PeerId(2))), MovePlan::Refused { why } if why == "Markdown is loaded for this run in this window only")
    );
}

#[test]
#[cfg_attr(
    target_os = "macos",
    ignore = "plugin pages are not available on the Mac: plugin.load is refused there"
)]
fn plugin_load_failed_toasts_the_reason_and_allows_retry() {
    let mut r = rig(fixture("row(editor, agent)"));
    r.run(load("demo"));
    let batch = r.controller.handle(HostEvent::Plugin(PluginEvent::LoadFailed {
        win: WIN,
        name: "demo".into(),
        why: "no such plugin: demo".into(),
    }));
    assert!(ops(&batch).is_empty());
    assert_eq!(toast(&r), "no such plugin: demo");
    assert_eq!(
        ops(&r.run(load("demo"))),
        vec![HostOp::Plugin(PluginOp::Load {
            win: WIN,
            name: "demo".into()
        })]
    );
}
