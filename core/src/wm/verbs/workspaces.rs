//! Workspace switches, chooser picks and rename entry rules.

use super::modules::ModuleSetup;
use crate::keymap::{prefix::WorkspacePick, Action};
use crate::layout::workspace::MAIN;
use crate::layout::{self, Layout, ModuleDecl, ModuleId, Placement, WorkspaceError, Workspaces};
use crate::lua::layout::LuaTree;
use crate::wm::{
    chrome, feedback, focus,
    host::*,
    window::{WinId, Window},
};

/// Workspace definitions in configuration order, including a definition's validation error.
pub type Definitions = Vec<(String, Result<LuaTree, String>)>;

fn changed(w: &mut Window) {
    w.chrome_state.dirty.workspaces += 1;
    w.saver_changed();
}

/// Why a workspace verb did nothing: a step with nowhere to step to, or any other refusal.
enum Refusal {
    Alone(String),
    Text(String),
}

impl From<String> for Refusal {
    fn from(text: String) -> Self {
        Refusal::Text(text)
    }
}

fn refuse(w: &mut Window, win: WinId, refusal: Refusal, batch: &mut Batch) {
    let (alone, text) = match refusal {
        Refusal::Alone(text) => (true, text),
        Refusal::Text(text) => (false, text),
    };
    let feedback::Feedback::Refuse { log, text } = feedback::workspaces(alone, text) else {
        return;
    };
    w.say(log);
    w.refuse(win, text, batch);
}

/// Switch to an existing workspace, releasing the model before computing any host operation.
pub fn switch(w: &mut Window, win: WinId, name: &str, setup: &ModuleSetup, batch: &mut Batch) -> Result<bool, String> {
    let next = {
        let mut workspaces = w
            .workspaces
            .try_borrow_mut()
            .map_err(|_| "workspaces are borrowed".to_string())?;
        if workspaces.current() == name {
            return Ok(false);
        }
        let mut layout = w
            .layout
            .try_borrow_mut()
            .map_err(|_| "layout is borrowed".to_string())?;
        workspaces
            .switch(&mut layout, name, &setup.decls)
            .map_err(|err| err.to_string())?
    };
    w.focus.keys_return.superseded();
    changed(w);
    // The change's maps and unmaps are made apart and put in the switch's order below; the keys are asked for
    // here, against the batch they go into, and everything else the change makes is carried over whole.
    let mut change = Batch::default();
    w.changed_with_focus(win, None, &mut change);
    let (mut maps, mut unmaps, mut focuses, mut rest) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for op in change.ops() {
        match op {
            HostOp::Map { .. } => maps.push(op.clone()),
            HostOp::Unmap { .. } => unmaps.push(op.clone()),
            HostOp::Focus { .. } => focuses.push(op.clone()),
            // A switch restacks even when the arriving floats are the same set in another order, and allocates
            // last.
            HostOp::Restack { .. } | HostOp::Allocate { .. } => {}
            _ => rest.push(op.clone()),
        }
    }
    use crate::wm::grid::order::{switch_order, Step};
    for step in switch_order(next.as_ref()) {
        match step {
            Step::Map(_) => maps.drain(..).for_each(|op| batch.push_layout(op)),
            Step::Focus(id) => {
                w.focus.ask(&id, batch);
                batch.push_layout(HostOp::Focus {
                    win,
                    to: FocusTo::Module(id),
                    on_grab: vec![],
                });
                focuses.drain(..).for_each(|op| batch.push_layout(op));
            }
            Step::Unmap => {
                // With nothing given the keys, the arriving workspace's hosts still go on screen first.
                maps.drain(..).for_each(|op| batch.push_layout(op));
                unmaps.drain(..).for_each(|op| batch.push_layout(op));
            }
            Step::Restack => w.restack(win, batch),
        }
    }
    batch.push_layout(HostOp::Allocate { win });
    for op in rest {
        batch.push_effect(op);
    }
    change.lua().iter().for_each(|op| batch.push_lua(op.clone()));
    w.say(format!("[workspace] switched to {name}"));
    Ok(true)
}

/// A new workspace keeps an ended editor off screen in place until a show starts it again.
pub(in crate::wm) fn hide_ended_editor(layout: &mut Layout, frame: &layout::Frame<'_>) {
    let editor = ModuleId::editor();
    if layout.contains(&editor) && !layout.is_away(&editor) {
        if let Err(layout::LayoutError::LastVisible(_)) = layout::kill(layout, &editor, layout::Reopen::InPlace, frame)
        {
            let _ = layout.show(&ModuleId::agent());
            let _ = layout::kill(layout, &editor, layout::Reopen::InPlace, frame);
        }
    }
}

/// A named workspace is made from its definition, or a fresh copy of the current arrangement.
pub fn switch_or_make(
    w: &mut Window,
    win: WinId,
    name: &str,
    setup: &ModuleSetup,
    reads: &dyn HostReads,
    batch: &mut Batch,
) -> Result<bool, String> {
    let exists = w
        .workspaces
        .try_borrow()
        .map_err(|_| "workspaces are borrowed".to_string())?
        .order()
        .iter()
        .any(|n| n == name);
    if !exists {
        Workspaces::valid_name(name).map_err(|err| err.to_string())?;
        let template = setup
            .workspaces
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, tree)| tree.clone());
        let deferred: Vec<_> = setup.start_hidden.iter().map(|(id, _)| id.clone()).collect();
        let (made, notes, gone, spread) = {
            let live = w.layout.try_borrow().map_err(|_| "layout is borrowed".to_string())?;
            let (made, notes) = new_workspace_layout(template, &live, &setup.decls, &deferred)?;
            let gone: Vec<_> = live.modules().into_iter().filter(|id| live.is_gone(id)).collect();
            let spread: Vec<_> = deferred
                .iter()
                .filter(|id| made.contains(id) && !live.contains(id))
                .cloned()
                .collect();
            (made, notes, gone, spread)
        };
        let size = reads.grid_size(win);
        let ended_editor = matches!(
            w.editor_run,
            crate::wm::window::EditorRun::Ending | crate::wm::window::EditorRun::Ended
        );
        let mut notes_live = Vec::new();
        {
            let mut workspaces = w
                .workspaces
                .try_borrow_mut()
                .map_err(|_| "workspaces are borrowed".to_string())?;
            let mut live = w
                .layout
                .try_borrow_mut()
                .map_err(|_| "layout is borrowed".to_string())?;
            let mut made = made;
            if ended_editor {
                frame_with(reads, win, size, |frame| hide_ended_editor(&mut made, frame));
            }
            workspaces.create(name, made).map_err(|err| err.to_string())?;
            frame_with(reads, win, size, |frame| {
                for id in gone {
                    workspaces.retire_everywhere(&id, frame);
                }
            });
            for id in spread {
                let mut modules = live.modules();
                modules.push(id.clone());
                for note in layout::fit_to_with(&mut live, &modules, &setup.decls) {
                    notes_live.push(format!("[layout] workspace '{}': {note}", workspaces.current()));
                }
                workspaces.add_everywhere_at(
                    &id,
                    layout::declared_placement(&setup.decls, &id, Placement::BelowEditor),
                );
            }
        }
        for note in notes {
            w.say(format!("[layout] workspace '{name}': {note}"));
        }
        for note in notes_live {
            w.say(note);
        }
        w.say(format!("[workspace] made {name}"));
        changed(w);
    }
    switch(w, win, name, setup, batch)
}

/// The workspace family's actions all enter through the controller's runner.
pub fn run(w: &mut Window, reads: &dyn HostReads, win: WinId, action: &Action, setup: &ModuleSetup, batch: &mut Batch) {
    let result: Result<(), Refusal> = match action {
        Action::WorkspaceStep { forward } => {
            let next = w.workspaces.borrow().step(*forward);
            match next {
                Some(name) => switch(w, win, &name, setup, batch).map(|_| ()).map_err(Refusal::from),
                None => Err(Refusal::Alone(format!(
                    "{}: this window has one workspace",
                    action.name()
                ))),
            }
        }
        Action::WorkspaceLast => {
            let next = w.workspaces.borrow().last().map(str::to_string);
            match next {
                Some(name) => switch(w, win, &name, setup, batch).map(|_| ()).map_err(Refusal::from),
                None => Err(Refusal::Text("no workspace to go back to".into())),
            }
        }
        Action::WorkspaceNew => {
            let name = w.workspaces.borrow_mut().create_copy(&w.layout.borrow());
            w.say(format!("[workspace] made {name}"));
            changed(w);
            switch(w, win, &name, setup, batch).map(|_| ()).map_err(Refusal::from)
        }
        Action::WorkspaceSwitch { name } => switch_or_make(w, win, name, setup, reads, batch)
            .map(|_| ())
            .map_err(Refusal::from),
        Action::WorkspaceChoose => {
            crate::wm::keys::await_workspace(w, batch);
            Ok(())
        }
        Action::WorkspaceRename => {
            let name = w.workspaces.borrow().current().to_string();
            if name == MAIN {
                Err(Refusal::Text(WorkspaceError::Main.to_string()))
            } else {
                w.chrome.rename = Some(name);
                // The open entry is one of the top bar's reveal inputs.
                w.chrome_state.dirty.modes += 1;
                chrome::touched(w);
                batch.push_layout(HostOp::Focus {
                    win,
                    to: FocusTo::RenameEntry,
                    on_grab: vec![],
                });
                Ok(())
            }
        }
        _ => unreachable!("only workspace actions enter this runner"),
    };
    if let Err(refusal) = result {
        refuse(w, win, refusal, batch);
    }
}

/// The chooser resolves its digit against the workspace order at the moment it is pressed.
pub fn pick(
    w: &mut Window,
    reads: &dyn HostReads,
    win: WinId,
    choice: WorkspacePick,
    setup: &ModuleSetup,
    batch: &mut Batch,
) {
    if let WorkspacePick::Switch(index) = choice {
        let local = w.workspaces.borrow().order().len();
        if index >= local {
            let name = w.group_summary().and_then(|s| s.workspaces.get(index - local).cloned());
            if let Some(name) = name {
                // The names past the local ones are window 2's.
                if let Some(peer) = w.second_peer() {
                    crate::wm::moves::raise_for(w, win, peer, crate::wm::moves::Raise::Workspace(name), batch);
                }
                return;
            }
        }
    }
    let picked = {
        let ws = w.workspaces.borrow();
        resolve_pick(ws.order(), ws.current(), choice)
    };
    let result = match picked {
        Pick::Switch(name) => switch(w, win, &name, setup, batch).map(|_| ()),
        Pick::New => {
            run(w, reads, win, &Action::WorkspaceNew, setup, batch);
            Ok(())
        }
        Pick::Delete(name) => {
            let result = w.workspaces.borrow_mut().delete(&name).map_err(|err| err.to_string());
            if result.is_ok() {
                w.say(format!("[workspace] deleted {name}"));
                crate::wm::moves::workspace::workspace_deleted(w, &name);
                changed(w);
            }
            result
        }
        Pick::Refuse(text) => Err(text),
    };
    if let Err(text) = result {
        refuse(w, win, Refusal::Text(text), batch);
    }
}

/// Closing an entry returns the keys before the chrome removes it; losing the keys needs no new grab.
pub fn entry(w: &mut Window, win: WinId, ev: EntryEv, batch: &mut Batch) {
    if w.chrome.rename.take().is_none() {
        return;
    }
    if !matches!(ev, EntryEv::LostKeys) {
        focus::return_to_pane(w, win, batch);
    }
    if let EntryEv::Enter(to) = ev {
        let from = w.workspaces.borrow().current().to_owned();
        let result = w.workspaces.borrow_mut().rename_current(&to);
        match result {
            Ok(()) => {
                w.say(format!("[workspace] renamed to {to}"));
                crate::wm::moves::workspace::workspace_renamed(w, &from, &to);
                changed(w);
            }
            Err(err) => refuse(w, win, Refusal::Text(err.to_string()), batch),
        }
    }
    w.chrome_state.dirty.modes += 1;
    chrome::touched(w);
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Pick {
    Switch(String),
    New,
    Delete(String),
    /// Nothing happens; the text says why.
    Refuse(String),
}

/// The chooser's key against `order` (the window's workspaces, in order) and `current`: a digit past
/// the order reaches no workspace, and `main` and the current workspace are never deleted.
fn resolve_pick(order: &[String], current: &str, pick: WorkspacePick) -> Pick {
    let named = |index: usize| {
        order.get(index).cloned().ok_or_else(|| {
            let digit =
                crate::wm::prefix_strip::workspace_digit(index).map_or_else(|| (index + 1).to_string(), String::from);
            format!("no workspace {digit}")
        })
    };
    match pick {
        WorkspacePick::New => Pick::New,
        WorkspacePick::Switch(index) => match named(index) {
            Ok(name) => Pick::Switch(name),
            Err(text) => Pick::Refuse(text),
        },
        WorkspacePick::Delete(index) => match named(index) {
            Ok(name) if name == MAIN => Pick::Refuse(WorkspaceError::Main.to_string()),
            Ok(name) if name == current => Pick::Refuse(WorkspaceError::Current(name).to_string()),
            Ok(name) => Pick::Delete(name),
            Err(text) => Pick::Refuse(text),
        },
    }
}

/// The layout a workspace made by name at runtime starts from: `init.lua`'s definition of that name,
/// reconciled as the window's startup reconciles it and brought to the window's modules (`fit_to_with`:
/// an instance the window lacks is left out, a module the definition does not place is hidden at its
/// declared place in `lua`, else below the editor), else a copy of `live`, the current arrangement. A
/// `deferred` module the definition names is kept, as the startup keeps it. The notes say what was left
/// out or added.
fn new_workspace_layout(
    template: Option<Result<crate::lua::layout::LuaTree, String>>,
    live: &Layout,
    lua: &[ModuleDecl],
    deferred: &[ModuleId],
) -> Result<(Layout, Vec<String>), String> {
    let Some(template) = template else {
        return Ok((live.fresh_copy(), Vec::new()));
    };
    let tree = template?;
    let reconciled = crate::layout::reconcile_default_floating_with(tree.tiled, tree.floats, lua, deferred)
        .map_err(|err| format!("eitri.layout.workspace: {err}"))?;
    let mut layout = reconciled.layout;
    let mut notes = reconciled.notes;
    notes.extend(crate::layout::fit_to_with(&mut layout, &live.modules(), lua));
    for id in live.away() {
        if layout.contains(&id) {
            // Refused only when nothing else could be shown; the workspace switch can still proceed.
            let _ = layout.set_away(&id, true);
        }
    }
    Ok((layout, notes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::prefix::WorkspacePick;
    use crate::layout::{Axis, ModuleId, WorkspaceError};
    use crate::wm::verbs::rig::{self, allocate, map, moves, ops, unmap};
    use crate::wm::{Decide, EditorPresence, EntryEv, FocusTo, HostEvent, HostOp, WindowParts, WindowRole, WindowSpec};

    #[test]
    fn regression_template_switch_with_the_only_other_module_away_succeeds() {
        let editor = ModuleId::editor();
        let agent = ModuleId::agent();
        let template = crate::lua::layout::LuaTree {
            tiled: crate::layout::Node::split(
                Axis::Row,
                0.5,
                crate::layout::Node::Leaf(editor.clone()),
                crate::layout::Node::Leaf(agent.clone()),
            ),
            floats: vec![],
        };
        let mut live = Layout::new(template.tiled.clone(), editor.clone()).unwrap();
        assert_eq!(live.modules(), vec![editor.clone(), agent.clone()]);
        live.set_away(&agent, true).unwrap();
        let mut r = rig::rig(live);
        r.controller.set_module_setup(
            rig::WIN,
            crate::wm::verbs::modules::ModuleSetup {
                workspaces: vec![("study".into(), Ok(template))],
                ..Default::default()
            },
        );
        r.run(Action::WorkspaceSwitch { name: "study".into() });
        assert_eq!(
            r.controller.chrome(rig::WIN).top_bar.workspace.as_deref(),
            Some("study")
        );
        assert!(r.layout.borrow().is_away(&agent));
        assert_eq!(r.layout.borrow().visible_modules(), vec![editor]);
        r.run(Action::WorkspaceSwitch { name: "main".into() });
        assert_eq!(r.controller.chrome(rig::WIN).top_bar.workspace.as_deref(), Some("main"));
        r.controller.workspace_pick(rig::WIN, WorkspacePick::Delete(1), 7);
        r.run(Action::WorkspaceSwitch { name: "study".into() });
        assert_eq!(
            r.controller.chrome(rig::WIN).top_bar.workspace.as_deref(),
            Some("study")
        );
        assert!(r.layout.borrow().is_away(&agent));
    }

    #[test]
    fn regression_recreated_template_keeps_away_modules() {
        let two = ModuleId::parse("terminal#2").unwrap();
        let template = crate::lua::layout::LuaTree {
            tiled: crate::layout::Node::split(
                Axis::Row,
                0.5,
                crate::layout::Node::Leaf(ModuleId::editor()),
                crate::layout::Node::split(
                    Axis::Column,
                    0.5,
                    crate::layout::Node::Leaf(ModuleId::agent()),
                    crate::layout::Node::Leaf(two.clone()),
                ),
            ),
            floats: vec![],
        };
        let live = crate::layout::reconcile_default_floating(template.tiled.clone(), vec![], &[])
            .unwrap()
            .layout;
        let mut r = rig::rig(live);
        r.controller.set_module_setup(
            rig::WIN,
            crate::wm::verbs::modules::ModuleSetup {
                workspaces: vec![("study".into(), Ok(template))],
                ..Default::default()
            },
        );
        r.run(Action::WorkspaceSwitch { name: "study".into() });
        r.run(Action::WorkspaceSwitch { name: "main".into() });
        r.layout.borrow_mut().set_away(&two, true).unwrap();
        r.controller.workspace_pick(rig::WIN, WorkspacePick::Delete(1), 7);
        r.run(Action::WorkspaceSwitch { name: "study".into() });
        assert!(r.layout.borrow().away().contains(&two));
        assert!(!r.layout.borrow().is_visible(&two));
    }

    #[test]
    fn workspace_picks_resolve_against_the_order() {
        let order: Vec<String> = ["main", "study", "review"].iter().map(|n| n.to_string()).collect();
        let pick = |p| resolve_pick(&order, "study", p);
        assert_eq!(pick(WorkspacePick::Switch(0)), Pick::Switch("main".into()));
        assert_eq!(pick(WorkspacePick::Switch(2)), Pick::Switch("review".into()));
        assert_eq!(pick(WorkspacePick::Switch(9)), Pick::Refuse("no workspace 0".into()));
        assert_eq!(pick(WorkspacePick::Switch(3)), Pick::Refuse("no workspace 4".into()));
        assert_eq!(pick(WorkspacePick::New), Pick::New);
        assert_eq!(pick(WorkspacePick::Delete(2)), Pick::Delete("review".into()));
        assert_eq!(
            pick(WorkspacePick::Delete(0)),
            Pick::Refuse(WorkspaceError::Main.to_string())
        );
        assert_eq!(
            pick(WorkspacePick::Delete(1)),
            Pick::Refuse(WorkspaceError::Current("study".into()).to_string())
        );
        assert_eq!(pick(WorkspacePick::Delete(5)), Pick::Refuse("no workspace 6".into()));
    }

    #[test]
    fn a_workspace_made_by_name_is_its_definition_fitted_to_the_window_else_a_copy() {
        let live = crate::wm::persist::first_launch_layout(&[]).unwrap();
        let (copy, notes) = new_workspace_layout(None, &live, &[], &[]).unwrap();
        assert_eq!(copy.root(), live.root(), "a copy keeps the arrangement");
        assert!(notes.is_empty());

        let two = ModuleId::parse("terminal#2").unwrap();
        let template = crate::lua::layout::LuaTree {
            tiled: crate::layout::Node::split(
                Axis::Row,
                0.5,
                crate::layout::Node::Leaf(ModuleId::editor()),
                crate::layout::Node::split(
                    Axis::Column,
                    0.5,
                    crate::layout::Node::Leaf(ModuleId::agent()),
                    crate::layout::Node::Leaf(two.clone()),
                ),
            ),
            floats: Vec::new(),
        };
        let (made, notes) = new_workspace_layout(Some(Ok(template)), &live, &[], &[]).unwrap();
        assert!(
            !made.contains(&two),
            "an instance the window lacks is left out: {notes:?}"
        );
        for id in live.modules() {
            assert!(made.contains(&id), "{id} is in the made workspace");
        }
        assert!(made.is_visible(&ModuleId::editor()));
        assert_eq!(
            new_workspace_layout(Some(Err("bad tree".into())), &live, &[], &[]).map(|_| ()),
            Err("bad tree".to_string())
        );
    }

    #[test]
    fn a_workspace_switch_by_a_defined_name_makes_the_definition_from_the_window_setup() {
        let mut r = rig::rig(crate::wm::persist::first_launch_layout(&[]).unwrap());
        let template = crate::lua::layout::LuaTree {
            tiled: crate::layout::Node::split(
                Axis::Column,
                0.3,
                crate::layout::Node::Leaf(ModuleId::agent()),
                crate::layout::Node::Leaf(ModuleId::editor()),
            ),
            floats: Vec::new(),
        };
        let live = r.layout.borrow().clone();
        let (expected, _) = new_workspace_layout(Some(Ok(template.clone())), &live, &[], &[]).unwrap();
        assert_ne!(
            expected.root(),
            live.root(),
            "the definition differs from a copy of the window"
        );
        r.controller.set_module_setup(
            rig::WIN,
            crate::wm::verbs::modules::ModuleSetup {
                workspaces: vec![("study".into(), Ok(template))],
                ..Default::default()
            },
        );
        r.run(Action::WorkspaceSwitch { name: "study".into() });
        assert_eq!(r.layout.borrow().root(), expected.root());
    }

    #[test]
    fn a_workspace_made_by_name_keeps_a_deferred_module_its_definition_names() {
        let live = crate::wm::persist::first_launch_layout(&[]).unwrap();
        let preview = ModuleId::lua("markdown");
        let template = || crate::lua::layout::LuaTree {
            tiled: crate::layout::Node::split(
                Axis::Row,
                0.6,
                crate::layout::Node::split(
                    Axis::Row,
                    0.5,
                    crate::layout::Node::Leaf(ModuleId::editor()),
                    crate::layout::Node::Leaf(ModuleId::agent()),
                ),
                crate::layout::Node::Leaf(preview.clone()),
            ),
            floats: Vec::new(),
        };
        let (made, notes) =
            new_workspace_layout(Some(Ok(template())), &live, &[], std::slice::from_ref(&preview)).unwrap();
        assert!(made.is_shown(&preview), "{notes:?}");
        let (made, notes) = new_workspace_layout(Some(Ok(template())), &live, &[], &[]).unwrap();
        assert!(!made.contains(&preview));
        assert!(notes.iter().any(|n| n.contains("left out 'lua:markdown'")), "{notes:?}");
    }

    #[test]
    fn a_rename_entry_takes_the_keys_and_gives_them_back_on_escape() {
        let mut r = rig::rig(crate::wm::persist::first_launch_layout(&[]).unwrap());
        r.run(crate::keymap::Action::WorkspaceNew);
        let batch = r.run(crate::keymap::Action::WorkspaceRename);
        assert_eq!(r.controller.chrome(rig::WIN).rename.as_deref(), Some("2"));
        assert_eq!(
            moves(&batch),
            vec![HostOp::Focus {
                win: rig::WIN,
                to: FocusTo::RenameEntry,
                on_grab: vec![],
            }]
        );
        let batch = r.controller.handle(HostEvent::TopBarEntry {
            win: rig::WIN,
            ev: EntryEv::Escape,
        });
        assert_eq!(r.controller.chrome(rig::WIN).rename, None);
        assert_eq!(
            moves(&batch),
            vec![HostOp::Focus {
                win: rig::WIN,
                to: FocusTo::Module(r.layout.borrow().focus().clone()),
                on_grab: vec![],
            }]
        );
    }

    #[test]
    fn a_chooser_delete_of_main_or_the_current_workspace_is_refused_with_todays_text() {
        let mut r = rig::rig(crate::wm::persist::first_launch_layout(&[]).unwrap());
        r.run(crate::keymap::Action::WorkspaceNew);
        for (index, text) in [
            (0, WorkspaceError::Main.to_string()),
            (1, WorkspaceError::Current("2".into()).to_string()),
        ] {
            let batch = r.controller.workspace_pick(rig::WIN, WorkspacePick::Delete(index), 7);
            chrome_only(&batch);
            assert_eq!(r.controller.chrome(rig::WIN).toast.as_ref().unwrap().text, text);
        }
    }

    #[test]
    fn a_lua_switch_makes_the_workspace_it_names_or_logs_its_refusal() {
        let mut r = rig::rig(crate::wm::persist::first_launch_layout(&[]).unwrap());
        r.controller
            .lua_request(rig::WIN, crate::lua::layout::LayoutRequest::Switch("study".into()));
        r.run(crate::keymap::Action::WorkspaceRename);
        assert_eq!(r.controller.chrome(rig::WIN).rename.as_deref(), Some("study"));
        let batch = r
            .controller
            .lua_request(rig::WIN, crate::lua::layout::LayoutRequest::Switch("".into()));
        assert!(ops(&batch).is_empty());
        assert!(r
            .controller
            .notes(rig::WIN)
            .iter()
            .any(|line| line.contains("[lua] layout: Switch(\"\") refused:")));
    }
    #[test]
    fn a_workspace_switch_gives_the_keys_before_it_unmaps_a_float_that_held_them() {
        let terminal = ModuleId::terminal();
        let agent = ModuleId::agent();
        let mut main = crate::wm::persist::first_launch_layout(&[]).unwrap();
        main.show(&terminal).unwrap();
        crate::layout::float_at(
            &mut main,
            &terminal,
            Some(crate::layout::FloatRect::centred(crate::wm::grid::UNALLOCATED, 0.6)),
            &crate::layout::Frame::new(crate::wm::grid::UNALLOCATED, 1),
        )
        .unwrap();
        crate::layout::hide(
            &mut main,
            &agent,
            &crate::layout::Frame::new(crate::wm::grid::UNALLOCATED, 1),
        )
        .unwrap();
        main.set_focus(&terminal).unwrap();
        let mut study = crate::wm::persist::first_launch_layout(&[]).unwrap();
        study.set_focus(&agent).unwrap();
        let mut r = rig::rig(main);
        let workspaces = Workspaces::from_parts(
            vec!["main".into(), "study".into()],
            [("study".into(), study)].into(),
            "main".into(),
            Default::default(),
        )
        .unwrap();
        r.controller.open_window(
            rig::WIN,
            WindowSpec {
                role: WindowRole::Main,
                presence: EditorPresence::Owned,
                title: "project".into(),
            },
            WindowParts {
                layout: r.layout.clone(),
                workspaces: std::rc::Rc::new(std::cell::RefCell::new(workspaces)),
                saver: None,
            },
        );
        let hosted: Vec<_> = r.layout.borrow().modules().iter().map(rig::decl).collect();
        r.controller.first_apply(rig::WIN, &hosted);
        let batch = r.run(crate::keymap::Action::WorkspaceSwitch { name: "study".into() });
        assert_eq!(
            moves(&batch),
            vec![
                map(&agent),
                HostOp::Focus {
                    win: rig::WIN,
                    to: FocusTo::Module(agent.clone()),
                    on_grab: vec![],
                },
                unmap(&terminal),
                HostOp::Restack {
                    win: rig::WIN,
                    moves: vec![]
                },
                allocate()
            ]
        );
        assert_eq!(r.layout.borrow().focus(), &agent);
        r.run(Action::WorkspaceLast);
        assert!(r.layout.borrow().is_floating(&terminal));
        assert_eq!(r.layout.borrow().focus(), &terminal);
    }

    #[test]
    fn switch_order_maps_and_focuses_before_unmapping_and_restacking() {
        use crate::wm::grid::order::{switch_order, Step};
        let next = ModuleId::agent();
        assert_eq!(
            switch_order(Some(&next)),
            vec![Step::Map(next.clone()), Step::Focus(next), Step::Unmap, Step::Restack]
        );
        assert_eq!(switch_order(None), vec![Step::Unmap, Step::Restack]);
    }
    fn new_workspace() -> rig::Rig {
        let mut r = rig::rig(crate::wm::persist::first_launch_layout(&[]).unwrap());
        r.run(Action::WorkspaceNew);
        r
    }

    /// Nothing moved and nothing ran but the chrome's announcement.
    fn chrome_only(batch: &Batch) {
        assert!(batch.ops().all(|op| matches!(op, HostOp::Chrome { .. })), "{batch:?}");
    }

    #[test]
    fn workspace_step_wraps_and_alone_refuses_with_its_own_text() {
        for forward in [false, true] {
            let mut r = rig::rig(crate::wm::persist::first_launch_layout(&[]).unwrap());
            let action = Action::WorkspaceStep { forward };
            let batch = r.run(action.clone());
            chrome_only(&batch);
            assert_eq!(
                r.controller.chrome(rig::WIN).toast.unwrap().text,
                format!("{}: this window has one workspace", action.name())
            );
            r.run(Action::WorkspaceNew);
            let batch = r.run(action);
            assert_eq!(
                moves(&batch),
                vec![
                    HostOp::Focus {
                        win: rig::WIN,
                        to: FocusTo::Module(ModuleId::editor()),
                        on_grab: vec![]
                    },
                    HostOp::Restack {
                        win: rig::WIN,
                        moves: vec![]
                    },
                    allocate()
                ]
            );
            assert_eq!(r.controller.chrome(rig::WIN).top_bar.workspace.as_deref(), Some("main"));
        }
    }

    #[test]
    fn workspace_last_returns_and_refuses_when_none_was_left() {
        let mut r = rig::rig(crate::wm::persist::first_launch_layout(&[]).unwrap());
        let batch = r.run(Action::WorkspaceLast);
        chrome_only(&batch);
        assert_eq!(
            r.controller.chrome(rig::WIN).toast.unwrap().text,
            "no workspace to go back to"
        );
        r.run(Action::WorkspaceNew);
        let batch = r.run(Action::WorkspaceLast);
        assert_eq!(
            moves(&batch),
            vec![
                HostOp::Focus {
                    win: rig::WIN,
                    to: FocusTo::Module(ModuleId::editor()),
                    on_grab: vec![]
                },
                HostOp::Restack {
                    win: rig::WIN,
                    moves: vec![]
                },
                allocate()
            ]
        );
    }

    #[test]
    fn workspace_new_is_a_fresh_copy_switched_to_at_once() {
        let mut r = rig::rig(crate::wm::persist::first_launch_layout(&[]).unwrap());
        let before = r.layout.borrow().clone();
        let batch = r.run(Action::WorkspaceNew);
        assert_eq!(
            moves(&batch),
            vec![
                HostOp::Focus {
                    win: rig::WIN,
                    to: FocusTo::Module(ModuleId::editor()),
                    on_grab: vec![]
                },
                HostOp::Restack {
                    win: rig::WIN,
                    moves: vec![]
                },
                allocate()
            ]
        );
        assert_ne!(r.layout.borrow().root().id, before.root().id);
        assert!(r
            .controller
            .notes(rig::WIN)
            .iter()
            .any(|line| line == "[workspace] made 2"));
    }

    #[test]
    fn workspace_choose_enters_the_wait_and_its_digit_switches_in_core() {
        let mut r = new_workspace();
        r.controller.set_keymap(
            rig::WIN,
            std::rc::Rc::new(crate::keymap::Keymap::defaults()),
            std::rc::Rc::new(crate::layout::ModuleKeys::built_in()),
        );
        let batch = r.run(Action::WorkspaceChoose);
        chrome_only(&batch);
        assert_eq!(
            r.controller.prefix_shown(rig::WIN).unwrap().0,
            crate::keymap::prefix::Waiting::Workspace { deleting: false }
        );
        let key = crate::keymap::KeySpec::parse("1").unwrap();
        let (_, batch) = r.controller.key(
            rig::WIN,
            &KeyStage::Window,
            &KeyPress {
                keycode: 10,
                press: crate::keymap::prefix::Press::Key(key),
                key: Some(key.key),
                mods: Mods::default(),
                super_or_hyper: false,
                super_or_meta: false,
            },
        );
        assert_eq!(
            r.controller.prefix_shown(rig::WIN).unwrap().0,
            crate::keymap::prefix::Waiting::No
        );
        assert!(moves(&batch).iter().any(|op| matches!(op, HostOp::Focus { .. })));
    }

    #[test]
    fn workspace_switch_current_is_silent_and_a_bad_definition_makes_nothing() {
        let mut r = new_workspace();
        let batch = r.run(Action::WorkspaceSwitch { name: "2".into() });
        assert!(batch.is_empty());
        r.controller.set_module_setup(
            rig::WIN,
            crate::wm::verbs::modules::ModuleSetup {
                workspaces: vec![("bad".into(), Err("bad tree".into()))],
                ..Default::default()
            },
        );
        let before = r.layout.borrow().clone();
        let batch = r.run(Action::WorkspaceSwitch { name: "bad".into() });
        chrome_only(&batch);
        assert_eq!(r.controller.chrome(rig::WIN).toast.unwrap().text, "bad tree");
        assert_eq!(*r.layout.borrow(), before);
    }

    #[test]
    fn rename_enter_preserves_text_rules_and_lost_keys_closes_without_a_grab() {
        for to in ["study", "2", "", "main"] {
            let mut r = new_workspace();
            r.run(Action::WorkspaceRename);
            let batch = r.controller.handle(HostEvent::TopBarEntry {
                win: rig::WIN,
                ev: EntryEv::Enter(to.into()),
            });
            assert_eq!(
                moves(&batch),
                vec![HostOp::Focus {
                    win: rig::WIN,
                    to: FocusTo::Module(ModuleId::editor()),
                    on_grab: vec![]
                }]
            );
            assert!(r.controller.chrome(rig::WIN).rename.is_none());
            assert_eq!(batch.ops().filter(|op| matches!(op, HostOp::Chrome { .. })).count(), 1);
            if to == "study" || to == "2" {
                assert!(r
                    .controller
                    .notes(rig::WIN)
                    .iter()
                    .any(|line| line == &format!("[workspace] renamed to {to}")));
            } else {
                assert!(r.controller.chrome(rig::WIN).toast.is_some());
            }
        }
        let mut r = new_workspace();
        r.run(Action::WorkspaceRename);
        let batch = r.controller.handle(HostEvent::TopBarEntry {
            win: rig::WIN,
            ev: EntryEv::LostKeys,
        });
        chrome_only(&batch);
        assert!(r.controller.chrome(rig::WIN).rename.is_none());
        let mut r = rig::rig(crate::wm::persist::first_launch_layout(&[]).unwrap());
        let batch = r.run(Action::WorkspaceRename);
        chrome_only(&batch);
        assert_eq!(
            r.controller.chrome(rig::WIN).toast.unwrap().text,
            WorkspaceError::Main.to_string()
        );
    }

    /// The window driven over the fake host, which reports the keys leaving the rename entry as a native one does,
    /// with a second workspace made and its rename entry open.
    fn renaming() -> (
        Rc<crate::wm::Driver<crate::wm::Controller>>,
        Rc<crate::wm::fake::FakeHost>,
    ) {
        use crate::wm::fake::{FakeHost, FakeScheduler};
        let host = FakeHost::new();
        let scheduler = FakeScheduler::new();
        let mut controller = crate::wm::Controller::new(scheduler.clone());
        controller.set_reads(host.clone());
        let layout = crate::wm::persist::first_launch_layout(&[]).unwrap();
        let hosted: Vec<_> = layout.modules().iter().map(rig::decl).collect();
        // Every module's host is built already, as the window's startup builds them before its first batch.
        for decl in &hosted {
            host.with_surface(rig::WIN, decl.module.clone(), decl.kind.clone(), false);
        }
        controller.open_window(
            rig::WIN,
            WindowSpec {
                role: WindowRole::Main,
                presence: EditorPresence::Owned,
                title: "project".into(),
            },
            WindowParts {
                layout: Rc::new(std::cell::RefCell::new(layout)),
                workspaces: Rc::new(std::cell::RefCell::new(Workspaces::single())),
                saver: None,
            },
        );
        let driver = crate::wm::Driver::new(controller, host.clone(), scheduler.clone());
        let sink: Rc<dyn crate::wm::HostSink> = driver.clone();
        host.attach(Rc::downgrade(&sink));
        scheduler.attach(Rc::downgrade(&sink));
        driver.first_apply(rig::WIN, hosted);
        let key = crate::wm::Origin::Key { keycode: 7 };
        driver.run(rig::WIN, Action::WorkspaceNew, key.clone());
        driver.run(rig::WIN, Action::WorkspaceRename, key);
        assert!(
            host.log().contains(&Logged::Op(
                HostOp::Focus {
                    win: rig::WIN,
                    to: FocusTo::RenameEntry,
                    on_grab: vec![]
                },
                crate::wm::Applied::Done
            )),
            "the premise: the entry holds the keys"
        );
        (driver, host)
    }

    fn renamed_lines(driver: &crate::wm::Driver<crate::wm::Controller>) -> usize {
        driver
            .query(|c| c.notes(rig::WIN))
            .iter()
            .filter(|line| line.starts_with("[workspace] renamed to"))
            .count()
    }

    /// Where in the fake's log `entry` was first logged after `from`.
    fn logged_after(host: &crate::wm::fake::FakeHost, from: usize, entry: impl Fn(&Logged) -> bool) -> Option<usize> {
        host.log()
            .iter()
            .enumerate()
            .skip(from)
            .find(|(_, l)| entry(l))
            .map(|(at, _)| at)
    }

    use crate::wm::fake::Logged;
    use std::rc::Rc;

    #[test]
    fn a_lost_focus_on_the_rename_entry_is_judged_after_the_batch() {
        let lost = |l: &Logged| {
            *l == Logged::Event(HostEvent::TopBarEntry {
                win: rig::WIN,
                ev: EntryEv::LostKeys,
            })
        };
        let back_to_a_module = |l: &Logged| {
            matches!(
                l,
                Logged::Op(
                    HostOp::Focus {
                        to: FocusTo::Module(_),
                        ..
                    },
                    _
                )
            )
        };
        let chrome = |l: &Logged| matches!(l, Logged::Op(HostOp::Chrome { .. }, _));

        // Enter: the batch renames, gives the keys back, which the entry hears as lost, and closes the entry. The
        // loss is judged once that batch is done, against an entry already closed: nothing is renamed again.
        let (driver, host) = renaming();
        let from = host.log().len();
        driver.post(HostEvent::TopBarEntry {
            win: rig::WIN,
            ev: EntryEv::Enter("study".into()),
        });
        let focus = logged_after(&host, from, back_to_a_module).expect("the keys go back to a module");
        let heard = logged_after(&host, focus, lost).expect("the entry hears the keys leave it");
        let closed = logged_after(&host, focus, chrome).expect("the batch closes the entry");
        assert!(
            heard < closed,
            "the loss is heard while the batch applies, before the batch's own chrome closes the entry"
        );
        assert_eq!(renamed_lines(&driver), 1, "one rename for one Enter");
        let model = driver.query(|c| c.chrome(rig::WIN));
        assert_eq!(model.rename, None);
        assert_eq!(model.top_bar.workspace.as_deref(), Some("study"));
        assert!(
            model.toast.is_none(),
            "no refusal follows the rename: {:?}",
            model.toast
        );

        // The keys moved away by anything else while the entry is open: it closes and renames nothing.
        let (driver, host) = renaming();
        let from = host.log().len();
        driver.post(HostEvent::TopBarLeft { win: rig::WIN });
        let focus = logged_after(&host, from, back_to_a_module).expect("the keys go back to a module");
        let heard = logged_after(&host, focus, lost).expect("the entry hears the keys leave it");
        assert!(
            logged_after(&host, heard, chrome).is_some(),
            "judged after the batch that moved the keys, the loss closes the entry"
        );
        assert_eq!(renamed_lines(&driver), 0);
        let model = driver.query(|c| c.chrome(rig::WIN));
        assert_eq!(model.rename, None);
        assert_eq!(model.top_bar.workspace.as_deref(), Some("2"));
    }

    #[test]
    fn rename_keeps_the_top_bar_revealed_when_modes_are_derived_again() {
        let mut r = new_workspace();
        r.run(Action::WindowImmersive);
        r.run(Action::WorkspaceRename);
        r.run(Action::WindowImmersive);
        assert!(r.controller.chrome(rig::WIN).modes.top_bar_revealed);
    }

    #[test]
    fn chooser_delete_updates_the_workspace_label_without_allocating() {
        let mut r = new_workspace();
        r.run(Action::WorkspaceLast);
        let batch = r.controller.workspace_pick(rig::WIN, WorkspacePick::Delete(1), 7);
        chrome_only(&batch);
        let batch = r.run(Action::WorkspaceLast);
        chrome_only(&batch);
        assert_eq!(
            r.controller.chrome(rig::WIN).toast.unwrap().text,
            "no workspace to go back to"
        );
    }
    #[test]
    fn workspace_revisions_rederive_names_for_switch_rename_and_delete() {
        let mut r = new_workspace();
        let before = r.controller.chrome_derivations(rig::WIN).top_bar;
        r.run(Action::WorkspaceRename);
        r.controller.handle(HostEvent::TopBarEntry {
            win: rig::WIN,
            ev: EntryEv::Enter("study".into()),
        });
        assert_eq!(
            r.controller.chrome(rig::WIN).top_bar.workspace.as_deref(),
            Some("study")
        );
        assert!(r.controller.chrome_derivations(rig::WIN).top_bar > before);
        r.run(Action::WorkspaceLast);
        assert_eq!(r.controller.chrome(rig::WIN).top_bar.workspace.as_deref(), Some("main"));
        let before = r.controller.chrome_derivations(rig::WIN).top_bar;
        r.controller.workspace_pick(rig::WIN, WorkspacePick::Delete(1), 7);
        assert_eq!(r.controller.chrome(rig::WIN).top_bar.workspace, None);
        assert_eq!(r.controller.chrome_derivations(rig::WIN).top_bar, before + 1);
    }
}
