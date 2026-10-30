//! What a window of a linked pair does with the modules it holds away for the other window: marking them, telling
//! the other window what it holds itself, and taking them back when that window ends. Plain functions over the
//! window's layout and workspaces; the window controller carries them out on its own layout, and the pair's start
//! on the layouts a window opens with.

use std::collections::BTreeSet;

use crate::layout::workspace::Workspaces;
use crate::layout::{Layout, ModuleId, ModuleKind};

/// Marks `id` away (or back) in the live layout and in every stored workspace that holds it.
pub fn hold_away(layout: &mut Layout, workspaces: &mut Workspaces, id: &ModuleId, away: bool) {
    if layout.contains(id) {
        // Refused only when nothing else could be shown, and the chat is always there to be shown.
        let _ = layout.set_away(id, away);
    }
    workspaces.set_away_everywhere(id, away);
}

/// The modules this window holds itself, in every workspace: every module its layouts name but those held
/// away. What `away{ids}` tells the other window.
pub fn held(layout: &Layout, workspaces: &Workspaces) -> BTreeSet<String> {
    let away: BTreeSet<ModuleId> = workspaces
        .each(layout)
        .into_iter()
        .flat_map(|(_, layout)| layout.away())
        .collect();
    workspaces
        .each(layout)
        .into_iter()
        .flat_map(|(_, layout)| layout.modules())
        .filter(|id| !away.contains(id) && !layout.is_gone(id))
        .map(|id| id.as_str().to_string())
        .collect()
}

/// Whether a peer ended or a restore never made its modules leave this window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndKind {
    LinkEnded,
    RestoreFailed,
}

/// What a window does with its placeholders when the other window ends, cleanly or not.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EndPlan {
    /// Back in this window with its previous visibility: base modules, and pages after a failed restore.
    pub back: Vec<ModuleId>,
    /// Pages and Lua panels taken back without changing what the surviving window shows.
    pub back_hidden: Vec<ModuleId>,
    /// A terminal or chat view the other window held: its process ended with that window, so its placeholder
    /// goes. A chat is still in the other window's saved tabs.
    pub dropped: Vec<ModuleId>,
}

/// The placeholders `away` (the modules this window holds away for the other window) as the other window's end
/// leaves them.
pub fn end_plan(away: &BTreeSet<ModuleId>, kind: EndKind) -> EndPlan {
    let mut plan = EndPlan::default();
    for id in away {
        match id.kind() {
            ModuleKind::Terminal | ModuleKind::Agent if id.number() >= 2 => plan.dropped.push(id.clone()),
            ModuleKind::LuaWebview if kind == EndKind::LinkEnded => plan.back_hidden.push(id.clone()),
            _ => plan.back.push(id.clone()),
        }
    }
    plan
}

/// Every module held away in any workspace of the window.
pub fn away_everywhere(layout: &Layout, workspaces: &Workspaces) -> BTreeSet<ModuleId> {
    workspaces
        .each(layout)
        .into_iter()
        .flat_map(|(_, layout)| layout.away())
        .collect()
}

/// The modules among `plan.back` whose page must be loaded again when it comes back: every plugin page and Lua
/// panel. A page built before it was held away still shows what it read then, while the other window may have
/// changed its store since; loading it again makes it a new document, which says hello and reads the store as it
/// is now. A base module is the window's own all along and is only shown again.
pub fn pages_back(plan: &EndPlan) -> Vec<ModuleId> {
    plan.back
        .iter()
        .filter(|id| id.kind() == ModuleKind::LuaWebview)
        .cloned()
        .collect()
}

/// Carries out `plan` in the window's layouts: `back` comes back in every workspace where it was, `dropped` leaves
/// every workspace. `back_hidden` keeps its place hidden until explicitly shown. The controller lays the change
/// out as any change of its own, and its module lifecycle then rebuilds what came back as it is shown.
pub fn apply_end(plan: &EndPlan, layout: &mut Layout, workspaces: &mut Workspaces) {
    for id in &plan.back {
        hold_away(layout, workspaces, id, false);
    }
    for id in &plan.back_hidden {
        if layout.contains(id) {
            let _ = layout.set_back_hidden(id);
        }
        workspaces.set_back_hidden_everywhere(id);
    }
    for id in &plan.dropped {
        if layout.contains(id) {
            // An away module is hidden, so taking its leaf out never leaves the window with nothing shown.
            if let Err(err) = layout.remove(id) {
                eprintln!("[window] {id}'s placeholder could not be dropped: {err}");
            }
        }
        workspaces.remove_everywhere(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::kill::{kill, Reopen};
    use crate::layout::{Frame, ModuleDecl, Placement, Size};
    use std::collections::BTreeMap;

    pub(crate) fn id(text: &str) -> ModuleId {
        ModuleId::parse(text).unwrap()
    }

    fn frame() -> Frame<'static> {
        Frame::new(Size { w: 1280, h: 720 }, 1)
    }

    /// The editor and chat in a row, a terminal and a terminal instance below the editor, a Lua panel and a
    /// plugin page at the side.
    pub(crate) fn window(extra: &[&str]) -> Layout {
        let mut decls = vec![ModuleDecl {
            id: ModuleId::terminal(),
            placement: Placement::BelowEditor,
        }];
        for text in extra {
            let placement = match id(text).kind() {
                ModuleKind::LuaWebview => Placement::RightOfRoot,
                _ => Placement::BelowEditor,
            };
            decls.push(ModuleDecl {
                id: id(text),
                placement,
            });
        }
        Layout::initial(&decls).unwrap()
    }

    pub(crate) fn two_workspaces(stored: Layout) -> Workspaces {
        Workspaces::from_parts(
            vec!["main".into(), "2".into()],
            BTreeMap::from([("2".to_string(), stored)]),
            "main".into(),
            Default::default(),
        )
        .unwrap()
    }

    #[test]
    fn window_1s_end_brings_its_pages_and_lua_panels_back_in_window_2_and_drops_its_terminals_placeholders() {
        // Window 2 at its start, then a terminal instance and a chat view it handed to window 1 left as
        // placeholders.
        let mut live = window(&["lua:notes", "terminal#3", "agent#2"]);
        let mut stored = window(&["lua:notes", "terminal#3"]);
        stored.show(&id("lua:notes")).unwrap();
        let mut workspaces = two_workspaces(stored);
        kill(&mut live, &ModuleId::editor(), Reopen::Never, &frame()).unwrap();
        workspaces.retire_everywhere(&ModuleId::editor(), &frame());
        hold_away(&mut live, &mut workspaces, &id("lua:notes"), true);
        hold_away(&mut live, &mut workspaces, &id("terminal#3"), true);
        hold_away(&mut live, &mut workspaces, &id("agent#2"), true);

        let plan = end_plan(&away_everywhere(&live, &workspaces), EndKind::LinkEnded);
        assert_eq!(plan.back_hidden, vec![id("lua:notes")]);
        assert_eq!(plan.dropped, vec![id("agent#2"), id("terminal#3")]);
        apply_end(&plan, &mut live, &mut workspaces);

        for (name, layout) in workspaces.each(&live) {
            assert!(layout.contains(&id("lua:notes")), "{name}: the panel keeps its place");
            assert!(!layout.is_away(&id("lua:notes")), "{name}: the panel is window 2's now");
            assert!(
                !layout.contains(&id("terminal#3")),
                "{name}: the terminal ended with window 1"
            );
            assert!(
                !layout.contains(&id("agent#2")),
                "{name}: the chat view's placeholder goes"
            );
            assert!(layout.is_gone(&ModuleId::editor()), "{name}: still no editor here");
        }
        assert!(
            !workspaces.stored("2").unwrap().is_shown(&id("lua:notes")),
            "hidden where it was shown when it left"
        );
        assert!(away_everywhere(&live, &workspaces).is_empty());
    }

    #[test]
    fn window_2s_end_brings_its_pages_back_hidden_and_its_own_modules_as_they_were() {
        // Window 1 with a Lua panel and a terminal instance held away for window 2, and its own terminal.
        let mut live = window(&["lua:notes", "terminal#2"]);
        live.show(&id("lua:notes")).unwrap();
        let mut workspaces = Workspaces::single();
        hold_away(&mut live, &mut workspaces, &id("lua:notes"), true);
        hold_away(&mut live, &mut workspaces, &id("terminal#2"), true);
        hold_away(&mut live, &mut workspaces, &ModuleId::terminal(), true);

        let plan = end_plan(&away_everywhere(&live, &workspaces), EndKind::LinkEnded);
        assert_eq!(plan.back, vec![ModuleId::terminal()]);
        assert_eq!(plan.back_hidden, vec![id("lua:notes")]);
        assert!(pages_back(&plan).is_empty());
        assert_eq!(plan.dropped, vec![id("terminal#2")]);
        apply_end(&plan, &mut live, &mut workspaces);
        assert!(live.contains(&id("lua:notes")) && !live.is_away(&id("lua:notes")));
        assert!(!live.is_shown(&id("lua:notes")), "the panel keeps its place hidden");
        assert!(live.is_shown(&ModuleId::terminal()));
        assert!(
            !live.is_away(&ModuleId::terminal()),
            "the window's own terminal is back"
        );
        assert!(!live.contains(&id("terminal#2")));
        assert!(live.contains(&ModuleId::editor()) && !live.is_gone(&ModuleId::editor()));
    }

    #[test]
    fn a_restore_that_failed_brings_everything_back_as_it_was() {
        let mut live = window(&["lua:notes", "lua:markdown"]);
        live.show(&id("lua:notes")).unwrap();
        crate::layout::hide(&mut live, &id("lua:markdown"), &frame()).unwrap();
        let before = live.clone();
        let mut workspaces = two_workspaces(live.clone());
        for module in [id("lua:notes"), id("lua:markdown"), ModuleId::terminal()] {
            hold_away(&mut live, &mut workspaces, &module, true);
        }
        let plan = end_plan(&away_everywhere(&live, &workspaces), EndKind::RestoreFailed);
        assert!(plan.back_hidden.is_empty());
        apply_end(&plan, &mut live, &mut workspaces);
        for (_, layout) in workspaces.each(&live) {
            for module in before.modules() {
                assert_eq!(layout.is_shown(&module), before.is_shown(&module));
                assert!(!layout.is_away(&module));
            }
            assert_eq!(layout.root(), before.root());
        }
    }

    #[test]
    fn the_pages_that_come_back_are_loaded_again_and_the_base_modules_only_shown() {
        // Window 2 at its start, with a plugin page built before it was held away and a terminal instance it
        // handed to window 1.
        let mut live = window(&["lua:notes", "lua:markdown", "terminal#3"]);
        let mut workspaces = Workspaces::single();
        kill(&mut live, &ModuleId::editor(), Reopen::Never, &frame()).unwrap();
        hold_away(&mut live, &mut workspaces, &id("lua:notes"), true);
        hold_away(&mut live, &mut workspaces, &id("lua:markdown"), true);
        hold_away(&mut live, &mut workspaces, &id("terminal#3"), true);
        hold_away(&mut live, &mut workspaces, &ModuleId::terminal(), true);
        let plan = end_plan(&away_everywhere(&live, &workspaces), EndKind::RestoreFailed);
        assert_eq!(pages_back(&plan), vec![id("lua:markdown"), id("lua:notes")]);
        assert!(
            plan.back.contains(&ModuleId::terminal()) && !pages_back(&plan).contains(&ModuleId::terminal()),
            "the window's own terminal is shown again, not restarted"
        );
        assert!(
            !pages_back(&plan).contains(&id("terminal#3")),
            "a dropped placeholder is not loaded"
        );
    }
}
