//! The panel's hook registrations held to the windows that fill them: each window wires its panel once, through
//! `wire_panel`, and leaves out exactly its declared list. These read the windows' own sources and build the
//! companion's hooks, which only this binary has.

use crate::panel_wiring::*;
use eitri_core::editor_rpc::EditorRpc;
use eitri_core::keymap::TabAction;
use eitri_core::layout::Direction;
use eitri_core::scratch::ScratchRequest;
use eitri_core::source_scan as scan;
use eitri_panel::agent_panel::HintInbound;
use std::rc::Rc;

/// Every `.NAME(` in `code`: a method call of that name, whatever it is called on.
fn method_calls(code: &str, name: &str) -> usize {
    scan::word_ends(code, name)
        .into_iter()
        .filter(|end| code[*end..].trim_start().starts_with('('))
        .filter(|end| code[..end - name.len()].trim_end().ends_with('.'))
        .count()
}

/// A shell file as code: comments and literals blanked, its test modules cut.
fn shell_code(text: &str) -> String {
    let code = scan::code_only(text);
    scan::without(&code, &scan::modules(&code, "tests"))
}

fn sorted(mut names: Vec<&'static str>) -> Vec<&'static str> {
    names.sort_unstable();
    names
}

#[test]
fn the_companion_omits_the_hook() {
    assert!(HOOKS.contains(&"on_choose_project"));
    assert!(COMPANION_OMITS.contains(&"on_choose_project"));
    let window = shell_code(&scan::read_required("shell/src/companion/window.rs").text);
    let hooks = scan::functions(&window, "companion_hooks");
    assert_eq!(hooks.len(), 1);
    assert!(!hooks[0].contains("on_choose_project"));
}

#[test]
fn the_companion_and_the_main_window_wire_the_panel_through_one_function() {
    let sources = scan::rust_sources(&["shell/src", "desk/src"]);
    scan::require_anchors(&sources, &["fn wire_panel(", "fn build_ui(", "fn companion_hooks("]);

    // The registrations happen in `wire_panel`, once each, and nowhere else in the shell.
    let (home, wire_panel) = scan::function_in(&sources, "wire_panel");
    for name in HOOKS {
        assert_eq!(method_calls(&wire_panel, name), 1, "wire_panel registers {name} once");
    }
    for source in &sources {
        let code = shell_code(&source.text);
        let code = if source.path == home.path {
            scan::without(&code, &scan::functions(&code, "wire_panel"))
        } else {
            code
        };
        for name in HOOKS {
            assert_eq!(
                method_calls(&code, name),
                0,
                "{} registers {name} on the panel itself instead of through wire_panel",
                source.path
            );
        }
    }

    // A hook the panel handle gains is one more field here, not a registration a window makes by hand.
    let panel = shell_code(&scan::read_required("panel/src/agent_panel.rs").text);
    for end in scan::word_ends(&panel, "fn") {
        let rest = panel[end..].trim_start();
        let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
        let public = panel[..end - 2].trim_end().ends_with("pub");
        if public && name.starts_with("on_") {
            assert!(
                HOOKS.contains(&name.as_str()),
                "the panel's {name} has no field in PanelHooks"
            );
        }
    }

    // Each window wires its panel once.
    let (_, build_ui) = scan::function_in(&sources, "build_ui");
    assert_eq!(scan::calls(&build_ui, "wire_panel"), 1, "build_ui wires the panel once");
    let window = shell_code(&scan::read_required("shell/src/companion/window.rs").text);
    let build = scan::functions(&window, "build");
    assert_eq!(build.len(), 1, "the companion window is built by one function");
    assert_eq!(
        scan::calls(build[0], "wire_panel"),
        1,
        "the companion window wires its panel once"
    );
    assert_eq!(
        scan::calls(build[0], "companion_hooks"),
        1,
        "with the companion's own hooks"
    );

    // What the companion leaves out is exactly its declared list, the answer gate among it.
    let hooks = crate::companion::window::companion_hooks(crate::companion::window::CompanionHookParts {
        toast: Rc::new(|_: &str| {}),
        run_tab: Rc::new(|_: TabAction| {}),
        hint: Rc::new(|_: HintInbound| {}),
        move_focus: Rc::new(|_: &eitri_core::layout::ModuleId, _: Direction| false),
        editor_rpc: Rc::new(NoEditor),
        review_owner: Rc::new(|| None),
        review_open: Rc::new(|| {}),
        editor_request: Rc::new(|_: &ScratchRequest| Ok(())),
        editor_done: Rc::new(|| {}),
        edit_selection: Rc::new(|| {}),
        reload: crate::companion::window::ReloadCalls::new(|| false),
    });
    assert_eq!(sorted(hooks.missing()), sorted(COMPANION_OMITS.to_vec()));
    for name in ["on_busy_changed", "on_turn_ended"] {
        assert!(HOOKS.contains(&name), "{name} is a hook every window accounts for");
        assert!(
            !COMPANION_OMITS.contains(&name),
            "the companion wires {name}: it drives the reload"
        );
        assert!(!hooks.missing().contains(&name), "the companion's hooks set {name}");
    }
    assert!(
        hooks.missing().contains(&"set_answer_gate"),
        "the companion installs no answer gate"
    );
}

#[test]
fn the_main_windows_panel_hooks_miss_nothing() {
    // The main window's hooks capture its live widgets, so what it sets is read from its code:
    // every field is set once, before the one `wire_panel` call takes them.
    let (_, build_ui) = scan::function_in(&scan::rust_sources(&["shell/src"]), "build_ui");
    let code: String = build_ui.split_whitespace().collect();
    let wired = code.find("wire_panel(").expect("build_ui wires the panel");
    let mut missing = Vec::new();
    for name in HOOKS {
        let set = format!("panel_hooks.{name}=Some(");
        match code.matches(&set).count() {
            0 => missing.push(name),
            1 => assert!(
                code.find(&set).unwrap() < wired,
                "{name} is set after the panel is wired"
            ),
            n => panic!("build_ui sets {name} {n} times"),
        }
    }
    assert_eq!(missing, Vec::<&str>::new(), "the main window leaves no hook out");
    for name in ["on_busy_changed", "on_turn_ended"] {
        assert!(HOOKS.contains(&name), "{name} is a hook the main window accounts for");
    }
    assert_eq!(
        PanelHooks::default().missing(),
        HOOKS.to_vec(),
        "an empty value misses every hook"
    );
}

/// An editor link that answers nothing, for building the companion's hooks without one.
struct NoEditor;

impl EditorRpc for NoEditor {
    fn exec_lua(&self, _code: &'static str, _args: Vec<rmpv::Value>) -> eitri_core::nvim_rpc::Pending {
        eitri_core::nvim_rpc::Pending::failed(eitri_core::nvim_rpc::RpcError::Unavailable("no editor".into()))
    }

    fn target(&self) -> Option<u64> {
        None
    }
}
