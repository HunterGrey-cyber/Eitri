//! A window's layout persistence: which layout it opens with, and keeping the per-project state file current.
//! The file's format, its reconciling and its failure modes are [`crate::layout::persist`]'s; this is the
//! window's half: choosing between the file, a plugin's own-window tree, `init.lua`'s `eitri.layout.default`
//! and the built-in first launch,
//! and writing the file 500ms after the last change and once more when the window closes. The controller decides
//! when ([`Saver`] says what is to be armed or written, the debounce runs on the controller's `Scheduler`); the
//! file is read, decoded and written on a writer thread, never on the UI thread, and the close waits for it, up to
//! five seconds, so the window's final arrangement is the file's last write.
//!
//! **What is written, and when.** The file is written only when this window's *arrangement* has
//! changed since the window opened or last wrote it -- in any of its workspaces, the tree with its
//! shares and pinned lengths, the floats with their rectangles and the ones remembered, what is
//! hidden, what the other window of the pair holds and the base view's tabs that came in with a workspace, and
//! which workspace is current ([`Arrangement`]):
//! 500ms after the last such change, and at close if one is still unwritten. What is
//! written is every workspace of this window, with the keys wherever they are at that moment:
//!
//! - **Moving the keys is not a change.** A click, `Ctrl+h/j/k/l`, a HINT label -- anything that only
//!   moves the keys, raising the float it gives them to included -- writes nothing, at close included; the keys go
//!   into the file with the next arrangement change, wherever they are when that change is written. So a project
//!   reopens with the keys where they were when its last arrangement change was written -- a click inside the
//!   debounce rides along, and so does one made before a close that writes a change still unwritten -- not
//!   where they were when it closed. That is the cost of this rule, and it is accepted.
//! - **A launch is not a change**, nor is a pinned row getting its length on the first frame it is
//!   shown (the grid's settle, `HostEvent::Settled`), nor a zoom (zoom is not stored): a project opened, zoomed,
//!   clicked around in and closed keeps following `eitri.layout.default` -- including one added to
//!   `init.lua` later -- and a file this build could not use is left alone.
//! - **Each window of a project writes its own file** ([`Saver::set_role`]): the pair's window 1, or a
//!   window alone, today's; window 2 `-w2`; a window that runs unpaired `-u`. So no window writes another's.
//!   Unpaired windows share the `-u` file: each writes all of its own workspaces at its own
//!   arrangement changes, and the last write wins (`persist::save`) -- usually the later change, but a
//!   window whose wait a zoom or a drag extended can write an earlier change after another window's
//!   later one. Nothing of the file's arrangement is merged into what is written: a workspace the
//!   other window changed is put back as this window has it, and one this window removed or renamed
//!   stays so. A window that only clicks writes nothing, so it can never put the arrangement it opened
//!   with back over the other's.
//! - **A module this window does not have** -- a Lua panel this launch's `init.lua` no longer
//!   registers, or a later build's `canvas` -- is not in this window's layouts, so it leaves the file
//!   the next time this window changes the arrangement (a click leaves it where it is). Accepted too:
//!   what is written is this window's layouts.
//!
//! **The file is read again before every write**, to know what the write would replace:
//! - one this build cannot use at all -- found so when the window opened, or made so since by a
//!   hand-edit from the nvim inside this very window or by another build -- is set aside whole as
//!   `.v2.json.unusable` (`persist::set_aside_v2`) and never written over;
//! - one only part of which this build can carry (a workspace it cannot use, a container layout it
//!   does not know) has its text copied to `.v2.json.partial` (`persist::copy_aside_v2`) first;
//! - the keys a later build keeps there, which this window can neither read nor change, are taken from
//!   the file as it is now (`Workspaces::retake_kept`), so a window that opened before a later build
//!   wrote them does not erase them; only with no file, or one it cannot use, are they the ones this
//!   window opened with. One that names a module this window places now, in any of its workspaces, is
//!   not taken (`Kept::drop_placed`): written back beside this window's tree it would put that module
//!   in two places, so the file's text is copied to `.v2.json.partial` instead.
//!
//! **The bytes this window opened are copied only while they are still the file.** A file this window
//! opened only in part is copied aside before its first write only if the file on disk is still
//! byte-identical to what it read: another window that wrote since has taken the file over, and what
//! it wrote is not this window's to copy over the copy it made.
//!
//! **This rule replaced a merge.** Three rounds of fixes tried to write only the keys when only the keys had
//! moved, into whatever arrangement the file then held; each round's fix opened the next hole, and the last one
//! dropped a module another window had placed. The kept keys of a later build are the one thing taken from the
//! file at a write: they are not this window's arrangement.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::host::Waker;
use super::window::Tiling;
use crate::layout::persist::{self, Loaded, V2State};
use crate::layout::rules::{At, OpenPoint, NOMINAL_GRID};
use crate::layout::tree::{DEFAULT_EDITOR_SHARE, RIGHT_OF_ROOT_SHARE};
use crate::layout::workspace::{Kept, MAIN};
use crate::layout::{
    apply_launch_rule, first_launch_decls, reconcile_default_floating_with, Con, FloatRect, Frame, Layout, LayoutError,
    ModuleDecl, ModuleId, ModuleKind, Node, OpenRule, OpenRules, Placement, Size, Workspaces,
};
use crate::lua::layout::LuaTree;
use crate::plugin::manifest::WindowFocus;
use crate::window_link::base_tabs::MovedTabKey;
use crate::window_link::pair::Role;

/// Where the saver's lines go: the process's own output, and in tests also a list the test reads, since the
/// writer thread logs too.
#[derive(Clone, Default)]
struct Log {
    #[cfg(test)]
    lines: Arc<Mutex<Vec<String>>>,
}

impl Log {
    fn say(&self, line: String) {
        println!("{line}");
        self.keep(line);
    }

    fn warn(&self, line: String) {
        eprintln!("{line}");
        self.keep(line);
    }

    #[cfg(test)]
    fn keep(&self, line: String) {
        self.lines.lock().unwrap_or_else(|e| e.into_inner()).push(line);
    }

    #[cfg(not(test))]
    fn keep(&self, _line: String) {}

    #[cfg(test)]
    fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// The size a first launch hides the bottom terminal against: the stand-in the grid computes against before its
/// first allocation. Hiding reads no geometry unless the hidden module holds the keys, which at startup the
/// terminal never does, so it is a formality, kept equal to the grid's own so the two can never disagree.
pub const FIRST_LAUNCH_FRAME: Size = Size { w: 1280, h: 721 };

/// Where a window's layout files are, `dir` being `persist::state_dir`'s answer: only a window that keeps its own
/// layout has any. A hub's shape is fixed and a chat or page window holds its one module, so neither reads or writes
/// `eitri` mode's files, which wait there for the next start in that mode.
pub fn layout_dir(dir: Option<PathBuf>, tiling: Tiling) -> Option<PathBuf> {
    dir.filter(|_| tiling == Tiling::Eitri)
}

/// A first launch's layout with no Lua default: the built-in modules and the Lua panels where their
/// declarations put them, and the bottom terminal placed below the editor but hidden.
///
/// `decls` are the window's declarations: an open rule may already declare the terminal somewhere
/// else, and then that place is used instead of below the editor. Only what a first launch places
/// -- the base terminal and the Lua panels -- reaches `Layout::initial`: a chat view's or a numbered
/// instance's declaration only says where that module goes when something else places it, and
/// `Layout::initial` refuses a module it would place twice.
///
/// Hidden through [`crate::layout::hide`], the only door, which chooses where the keys go. At startup the keys
/// are on the editor or a Lua `main` panel, never on the terminal.
pub fn first_launch_layout(decls: &[ModuleDecl]) -> Result<Layout, LayoutError> {
    let mut all = Vec::with_capacity(decls.len() + 1);
    if !decls.iter().any(|d| d.id == ModuleId::terminal()) {
        all.push(ModuleDecl {
            id: ModuleId::terminal(),
            placement: Placement::BelowEditor,
        });
    }
    all.extend_from_slice(decls);
    let mut layout = Layout::initial(&first_launch_decls(&all))?;
    crate::layout::hide(&mut layout, &ModuleId::terminal(), &Frame::new(FIRST_LAUNCH_FRAME, 1))?;
    Ok(layout)
}

/// `tree` with the built-ins it leaves out (the editor, the chat) added where a first launch puts them, each named in
/// `absent` so the caller hides it: its module key still shows it, and then at its first-launch share rather than
/// a sliver, since showing a hidden module keeps the share it was hidden with. The editor goes left of the chat at
/// [`DEFAULT_EDITOR_SHARE`] when the tree has the chat, else right of the whole tree as a `RightOfRoot` module
/// would; the chat goes right of the editor at the same share. A chat the tree floats is left as it is.
fn with_absent_builtins(mut tree: Node, floats: &[crate::layout::Float], absent: &mut Vec<ModuleId>) -> Node {
    let (editor, agent) = (ModuleId::editor(), ModuleId::agent());
    let row = crate::layout::Axis::Row;
    if !tree.leaves().contains(&editor) {
        if tree.leaves().contains(&agent) {
            tree.replace_leaf(&agent, |agent| {
                Node::split(row, DEFAULT_EDITOR_SHARE, Node::Leaf(editor.clone()), agent)
            });
        } else {
            tree = Node::split(row, RIGHT_OF_ROOT_SHARE, tree, Node::Leaf(editor.clone()));
        }
        absent.push(editor.clone());
    }
    if !tree.leaves().contains(&agent) && !floats.iter().any(|float| float.id == agent) {
        tree.replace_leaf(&editor, |editor| {
            Node::split(row, DEFAULT_EDITOR_SHARE, editor, Node::Leaf(agent.clone()))
        });
        absent.push(agent);
    }
    tree
}

/// The module lists a window reads its saved layout with: `lua` and `deferred` as given, except that in a plugin's
/// own window (`page`) the page counts as deferred. A saved file that names the page then keeps it where it was
/// left, and one that lacks it leaves it out instead of placing it where a Lua panel's first launch would, so
/// [`choose_startup_layout`] can put it at its `[window]` place. Pass the same lists to both.
pub fn own_window_lists(
    page: Option<&ModuleId>,
    lua: &[ModuleDecl],
    deferred: &[ModuleId],
) -> (Vec<ModuleDecl>, Vec<ModuleId>) {
    let Some(page) = page else {
        return (lua.to_vec(), deferred.to_vec());
    };
    let lua = lua.iter().filter(|decl| decl.id != *page).cloned().collect();
    let mut deferred = deferred.to_vec();
    if !deferred.contains(page) {
        deferred.push(page.clone());
    }
    (lua, deferred)
}

/// A plugin's own-window layout and its first-launch focus.
#[derive(Clone)]
pub struct PluginWindow<'a> {
    pub page: ModuleId,
    pub tree: &'a LuaTree,
    pub focus: WindowFocus,
}

/// The workspaces and the current layout a window opens with, the lines to log about how they were
/// chosen, and the text of a file this window opened only in part (copied aside before its first
/// write; `None` otherwise).
///
/// - `default`: `eitri.layout.default`'s tree, if `init.lua` gave one; an error there is the
///   caller's startup failure, not handled here.
/// - `plugin`: the own window's declared tree and first-launch focus, absent in a plain window or window 2.
/// - `loaded`: this project's state file, if the state directory is known.
///
/// The file wins whenever it can be used: a project reopens as it was left, a version 1 file
/// migrated into one workspace. The own window alone shows a page the file does not show (absent, or hidden as a
/// kill leaves it) at its `[window]` place, beside its declared siblings, keeping the saved arrangement around it;
/// read the file with [`own_window_lists`] so that an absent page stays absent until then. Otherwise the plugin tree wins, then the Lua default,
/// reconciled like a file (a module this
/// window lacks is left out, the terminal placed if missing); otherwise the built-in first launch
/// ([`first_launch_layout`]). Each first launch is one workspace, `main`.
///
/// Whichever it is, a layout that hides the editor says so: nvim is started by the editor's first
/// frame (`neovide-editor`'s render callback builds it), so neither nvim nor the window's colours,
/// which follow nvim's colorscheme, come until the editor is shown. `prefix x` in the editor makes
/// that one keypress and one relaunch away. `show_editor` is the key that shows it, as the effective
/// keymap binds it (`Ctrl+b e` by default), so the note names it; `None` when nothing binds one, and
/// the note then names no key rather than a wrong one.
///
/// `lua` is the window's module declarations (the Lua panels', and where the open rules put modules
/// when nothing holds the keys). A first launch -- the Lua default included -- then applies `rules`'
/// floats, scratchpads and sizes to what it placed itself: the base terminal and each Lua panel, all
/// of them with no Lua default, else those the default's tree does not name, sized against
/// [`NOMINAL_GRID`] since the window has no size yet. A saved layout is never changed by a rule: it is
/// how the project was left, as it wins over `eitri.layout.default`.
///
/// `deferred` are modules the window never places by itself (they are not among `lua`): a first launch leaves
/// them out, and a Lua default keeps one only where its tree or floats name it. A saved file that names one keeps
/// it when the caller read it with the same list (`persist::load_with`).
///
/// `own` are the own rules of the modules among `lua` that declare a whole rule (a plugin that starts shown): at a
/// first launch each counts where its `position` would, with its `size` and `focus`, not only its place. A module
/// without one counts its declared placement alone, as a Lua panel's `position`.
pub fn choose_startup_layout(
    default: Option<&LuaTree>,
    plugin: Option<PluginWindow<'_>>,
    loaded: Option<Loaded>,
    lua: &[ModuleDecl],
    rules: &OpenRules,
    show_editor: Option<&str>,
    deferred: &[ModuleId],
    own: &[(ModuleId, OpenRule)],
) -> Result<(Workspaces, Layout, Vec<String>, Option<String>), String> {
    let mut notes = Vec::new();
    let first_launch = |notes: &mut Vec<String>| -> Result<Layout, String> {
        let chosen = plugin.as_ref().map(|p| p.tree).or(default);
        let mut absent = Vec::new();
        let (mut layout, named) = match chosen {
            Some(tree) => {
                let label = plugin
                    .as_ref()
                    .map(|p| format!("{} [window] layout", p.page))
                    .unwrap_or_else(|| "eitri.layout.default".into());
                let mut augmented = tree.tiled.clone();
                if plugin.is_some() {
                    augmented = with_absent_builtins(augmented, &tree.floats, &mut absent);
                }
                let r = reconcile_default_floating_with(augmented, tree.floats.clone(), lua, deferred)
                    .map_err(|e| format!("{label}: {e}"))?;
                notes.extend(r.notes.into_iter().map(|n| format!("{label}: {n}")));
                let named: Vec<ModuleId> = tree
                    .tiled
                    .leaves()
                    .into_iter()
                    .chain(tree.floats.iter().map(|float| float.id.clone()))
                    .collect();
                (r.layout, named)
            }
            None => (first_launch_layout(lua).map_err(|e| e.to_string())?, Vec::new()),
        };
        let mut placed: Vec<(ModuleId, Option<Placement>)> = vec![(ModuleId::terminal(), None)];
        placed.extend(
            first_launch_decls(lua)
                .into_iter()
                .filter(|d| d.id.kind() == ModuleKind::LuaWebview)
                .map(|d| (d.id, Some(d.placement))),
        );
        for (id, position) in placed
            .into_iter()
            .filter(|(id, _)| !named.contains(id) && !deferred.contains(id))
        {
            let own_rule = own
                .iter()
                .find(|(owner, _)| *owner == id)
                .map(|(_, rule)| rule.clone())
                .or_else(|| {
                    position.map(|placement| OpenRule {
                        at: Some(At::Placement(placement)),
                        ..OpenRule::default()
                    })
                });
            let (rule, _) = rules.resolve_own(&id, OpenPoint::Launch, own_rule.as_ref());
            match apply_launch_rule(&mut layout, &id, &rule, NOMINAL_GRID) {
                Ok(applied) => notes.extend(applied),
                Err(err) => notes.push(format!("open rule for '{id}' not applied at the first launch: {err}")),
            }
        }
        if let Some(plugin) = &plugin {
            let focus = match plugin.focus {
                WindowFocus::Page => plugin.page.clone(),
                WindowFocus::Editor => ModuleId::editor(),
                WindowFocus::Terminal => ModuleId::terminal(),
            };
            layout
                .show(&focus)
                .map_err(|e| format!("{} [window] focus: {e}", plugin.page))?;
            layout
                .set_focus(&focus)
                .map_err(|e| format!("{} [window] focus: {e}", plugin.page))?;
            for id in absent.into_iter().filter(|id| *id != focus) {
                crate::layout::hide(&mut layout, &id, &Frame::new(FIRST_LAUNCH_FRAME, 1)).map_err(|e| e.to_string())?;
            }
        }
        Ok(layout)
    };
    let (workspaces, layout, keep_copy) = match loaded {
        Some(Loaded::Restored(mut opened)) | Some(Loaded::Migrated(mut opened)) => {
            notes.extend(std::mem::take(&mut opened.notes));
            // Every window has `main`, and the export writes it as `eitri.layout.default`: a file that
            // lost it gets it back as a first launch would build it, so the window never runs without it.
            let main_lost = !opened.workspaces.order().iter().any(|name| name == MAIN);
            if main_lost {
                let main = first_launch(&mut notes)?;
                opened.workspaces.restore_main(main);
                notes.push(format!(
                    "workspace '{MAIN}' could not be read; it is the default layout again"
                ));
            }
            if let Some(plugin) = &plugin {
                // A page the other window holds is that window's; one hidden or absent here comes back.
                let page = &plugin.page;
                if !opened.current.is_shown(page) && !opened.current.is_away(page) {
                    restore_window_page(&mut opened.current, plugin)?;
                    notes.push(format!("{}: shown at its [window] place", plugin.page));
                }
            }
            notes.push("reopened as it was left".to_string());
            // A restored `main` uses the same first-launch order as a project with no saved layout.
            if default.is_some() && !main_lost {
                notes.push("eitri.layout.default shapes a project with no saved layout; this one has one".to_string());
            }
            if opened.keep_copy.is_some() {
                notes.push(
                    "what this build cannot carry of the file is kept: the file is copied as .v2.json.partial \
                     before this window first writes it"
                        .to_string(),
                );
            }
            (opened.workspaces, opened.current, opened.keep_copy)
        }
        Some(Loaded::Unusable(why)) => {
            notes.push(format!(
                "{why}; opening the default layout (a version 2 file is kept: it is set aside as .v2.json.unusable \
                 the first time this window saves; a version 1 file is never written)"
            ));
            (Workspaces::single(), first_launch(&mut notes)?, None)
        }
        Some(Loaded::Missing) | None => (Workspaces::single(), first_launch(&mut notes)?, None),
    };
    if !layout.is_shown(&ModuleId::editor()) {
        let way_back = show_editor.map(|key| format!(" ({key})")).unwrap_or_default();
        notes.push(format!(
            "the editor is hidden in this layout: nvim, and the window's colours that follow it, wait until it \
             is shown{way_back}"
        ));
    }
    Ok((workspaces, layout, notes, keep_copy))
}

/// Shows only the page, at its `[window]` place: taken out of wherever the saved layout keeps it hidden, then placed
/// as an absent page is. Its nearest declared siblings locate the saved subtree to wrap; that subtree keeps its
/// proportions, pins and tab selections, and all other saved modules stay put.
fn restore_window_page(layout: &mut Layout, plugin: &PluginWindow<'_>) -> Result<(), String> {
    fn place(node: &Node, page: &ModuleId, saved: &Node) -> Option<Node> {
        let Node::Con(declared) = node else { return None };
        let index = declared
            .children
            .iter()
            .position(|child| child.node.leaves().contains(page))?;
        if let Some(root) = place(&declared.children[index].node, page, saved) {
            return Some(root);
        }
        let saved_leaves = saved.leaves();
        let anchors: Vec<ModuleId> = declared
            .children
            .iter()
            .enumerate()
            .filter(|(at, _)| *at != index)
            .flat_map(|(_, child)| child.node.leaves())
            .filter(|id| saved_leaves.contains(id))
            .collect();
        if anchors.is_empty() {
            return None;
        }
        let preceding: Vec<_> = declared.children[..index]
            .iter()
            .flat_map(|child| child.node.leaves())
            .filter(|id| anchors.contains(id))
            .collect();
        let following: Vec<_> = anchors.iter().filter(|id| !preceding.contains(id)).cloned().collect();
        let before = preceding.is_empty();
        let share = declared.children[index].share;
        fn wrap(
            saved: &Node,
            anchors: &[ModuleId],
            preceding: &[ModuleId],
            following: &[ModuleId],
            declared: &Con,
            page: &ModuleId,
            before: bool,
            share: f32,
        ) -> Node {
            if let Node::Con(con) = saved {
                if let Some(index) = con
                    .children
                    .iter()
                    .position(|child| anchors.iter().all(|id| child.node.leaves().contains(id)))
                {
                    let mut kept = con.clone();
                    kept.children[index].node = wrap(
                        &con.children[index].node,
                        anchors,
                        preceding,
                        following,
                        declared,
                        page,
                        before,
                        share,
                    );
                    return Node::Con(kept);
                }
                if con.layout == declared.layout && !preceding.is_empty() && !following.is_empty() {
                    let left = con
                        .children
                        .iter()
                        .rposition(|child| preceding.iter().any(|id| child.node.leaves().contains(id)));
                    let right = con
                        .children
                        .iter()
                        .position(|child| following.iter().any(|id| child.node.leaves().contains(id)));
                    if let (Some(left), Some(right)) = (left, right) {
                        if left < right
                            && con.children.len() < crate::layout::MAX_CHILDREN
                            && con
                                .children
                                .iter()
                                .all(|child| child.share * (1.0 - share) >= crate::layout::tree::MIN_RATIO)
                        {
                            let mut kept = con.clone();
                            for child in &mut kept.children {
                                child.share *= 1.0 - share;
                            }
                            kept.children
                                .insert(right, crate::layout::Child::new(Node::Leaf(page.clone()), share));
                            if kept.focus >= right {
                                kept.focus += 1;
                            }
                            return Node::Con(kept);
                        }
                    }
                }
            }
            let page = crate::layout::Child::new(Node::Leaf(page.clone()), share);
            let kept = crate::layout::Child::new(saved.clone(), 1.0 - share);
            let children = if before { vec![page, kept] } else { vec![kept, page] };
            let mut con = Con::new(declared.layout, children);
            con.last_split = declared.last_split;
            con.focus = usize::from(before);
            Node::Con(con)
        }
        Some(wrap(
            saved, &anchors, &preceding, &following, declared, page, before, share,
        ))
    }
    let page = &plugin.page;
    // A hidden page leaves its saved slot (tiled or floating) before it is placed again, so it is never in two places.
    let root = match Node::Con(layout.root().clone()).without(page) {
        Some(saved) => place(&plugin.tree.tiled, page, &saved).unwrap_or_else(|| {
            // With no surviving declared sibling, the saved arrangement is kept together beside the page.
            Node::split(crate::layout::Axis::Row, 0.5, Node::Leaf(page.clone()), saved)
        }),
        // Everything else floats: the page is the only tile.
        None => Node::Leaf(page.clone()),
    };
    let floats: Vec<_> = layout
        .floats()
        .iter()
        .filter(|float| float.id != *page)
        .cloned()
        .collect();
    let mut hidden = layout.saved_hidden();
    hidden.remove(page);
    let mut restored = Layout::from_parts_floating(
        root,
        floats,
        hidden,
        layout.focus().clone(),
        layout.float_memory().clone(),
    )
    .map_err(|e| e.to_string())?;
    for id in layout.away() {
        restored.set_away(&id, true).map_err(|e| e.to_string())?;
    }
    *layout = restored;
    Ok(())
}

/// A window back in `eitri` mode after a run of compositor mode: its chat windows saved their tabs under views, and the
/// layout files were left as they were, so nothing holds those views. Each saved view no workspace holds becomes a tile
/// below the chat, or below the module with the keys when the chat is not shown, so its tab reopens on screen and the next
/// save keeps it there. The keys stay where the layout put them. A view that cannot be placed is said, and its tab comes
/// back in the base view.
pub fn tile_saved_views(layout: &mut Layout, workspaces: &mut Workspaces, views: &[u8]) -> Vec<String> {
    let mut notes = Vec::new();
    let keys = layout.focus().clone();
    for view in views {
        let Ok(module) = ModuleId::instance(ModuleKind::Agent, *view) else {
            continue;
        };
        if workspaces.each(layout).iter().any(|(_, each)| each.contains(&module)) {
            continue;
        }
        let chat = ModuleId::agent();
        let beside = if layout.is_shown(&chat) {
            Some(chat)
        } else {
            layout.tiled_anchor()
        };
        let placed = match beside {
            Some(beside) => crate::layout::place(layout, &module, &beside, crate::layout::Axis::Column)
                .map_err(|err| err.to_string()),
            None => Err("nothing is tiled to place it beside".to_string()),
        };
        match placed {
            Ok(()) => workspaces.add_everywhere(&module),
            Err(why) => notes.push(format!("{module}: its saved tabs stay in the base view: {why}")),
        }
    }
    if let Err(err) = layout.set_focus(&keys) {
        notes.push(format!("the keys could not stay on {keys}: {err}"));
    }
    notes
}

/// Appends missing init.lua templates after saved workspaces, then shares every module across them. A template
/// keeps a `deferred` module it names (as `choose_startup_layout`'s default tree does), and the fit then puts it,
/// hidden, into every workspace that lacks it.
pub fn add_defined_workspaces(
    workspaces: &mut Workspaces,
    live: &mut Layout,
    defined: &[(String, LuaTree)],
    lua: &[ModuleDecl],
    deferred: &[ModuleId],
) -> Result<Vec<String>, String> {
    let mut notes = Vec::new();
    for (name, tree) in defined {
        if workspaces.order().contains(name) {
            notes.push(format!(
                "workspace '{name}': the saved layout wins over eitri.layout.workspace"
            ));
            continue;
        }
        let reconciled = reconcile_default_floating_with(tree.tiled.clone(), tree.floats.clone(), lua, deferred)
            .map_err(|err| format!("eitri.layout.workspace('{name}'): {err}"))?;
        notes.extend(
            reconciled
                .notes
                .into_iter()
                .map(|note| format!("workspace '{name}': {note}")),
        );
        workspaces
            .create(name, reconciled.layout)
            .map_err(|err| format!("eitri.layout.workspace('{name}'): {err}"))?;
    }
    notes.extend(workspaces.fit_all_with(live, lua));
    Ok(notes)
}

/// What the state file keeps of the window besides the keys and a later build's kept keys
/// (`persist::encode_v2`): every workspace's tree with its shares and pinned lengths, its floats and
/// remembered float rectangles, what it hides, which modules the other window of the pair holds (`away`) and
/// which of the base view's tabs came in with it, in order, and which one is current. A module moving to the
/// other window, or back, is so a change, and is written. A change of
/// this, and only of this, writes the file (this module's doc); zoom and the MRU order are not stored
/// at all, and the keys ride along. Container ids are not compared (`Con`'s equality ignores them).
#[derive(Debug, Clone, PartialEq)]
struct Arrangement {
    current: String,
    workspaces: Vec<WorkspaceArrangement>,
}

#[derive(Debug, Clone, PartialEq)]
struct WorkspaceArrangement {
    name: String,
    root: Con,
    hidden: BTreeSet<ModuleId>,
    /// Sorted by module, not in z-order: giving a float the keys raises it, and a move of the keys
    /// must not write the file. The order is still written, as it is when something else is.
    floats: Vec<(ModuleId, FloatRect, bool)>,
    float_memory: BTreeMap<ModuleId, FloatRect>,
    away: BTreeSet<ModuleId>,
    moved_tabs: Vec<MovedTabKey>,
}

impl Arrangement {
    fn of(workspaces: &Workspaces, current: &Layout) -> Arrangement {
        Arrangement {
            current: workspaces.current().to_string(),
            workspaces: workspaces
                .each(current)
                .into_iter()
                .map(|(name, layout)| {
                    let mut floats: Vec<_> = layout
                        .floats()
                        .iter()
                        .map(|float| (float.id.clone(), float.rect, float.scratch))
                        .collect();
                    floats.sort_by(|a, b| a.0.cmp(&b.0));
                    WorkspaceArrangement {
                        name: name.to_string(),
                        root: layout.root().clone(),
                        hidden: layout.saved_hidden(),
                        floats,
                        float_memory: layout.float_memory().clone(),
                        away: layout.away(),
                        moved_tabs: workspaces.moved_tabs_of(name).to_vec(),
                    }
                })
                .collect(),
        }
    }

    /// The current workspace's tree: the one a pinned row settles in on screen.
    fn current_root_mut(&mut self) -> Option<&mut Con> {
        let current = &self.current;
        self.workspaces
            .iter_mut()
            .find(|ws| ws.name == *current)
            .map(|ws| &mut ws.root)
    }
}

/// How much longer the debounce waits, given how long ago the last change was: `None` once
/// [`persist::SAVE_DEBOUNCE_MS`] has passed. The timer is armed once for a burst of changes -- a
/// divider drag reports one per motion event -- and, when it fires early, armed again for what is
/// left, rather than removed and re-added for every change.
fn debounce_left(since_last_change: Duration) -> Option<Duration> {
    Duration::from_millis(persist::SAVE_DEBOUNCE_MS)
        .checked_sub(since_last_change)
        .filter(|left| !left.is_zero())
}

/// `base` with each pinned length it did not have yet taken from `now`: what `settle_pins` just gave
/// the layout. Containers are matched child by child where both have the same orientation and the
/// same number of children; where the two trees differ, `base` is left as it is.
fn take_settled_lengths(base: &mut Con, now: &Con) {
    if base.layout != now.layout || base.children.len() != now.children.len() {
        return;
    }
    for (base_child, now_child) in base.children.iter_mut().zip(&now.children) {
        if let (Some(base_pin), Some(now_pin)) = (base_child.pin.as_mut(), now_child.pin) {
            if base_pin.px.is_none() {
                base_pin.px = now_pin.px;
            }
        }
        if let (Node::Con(base_con), Node::Con(now_con)) = (&mut base_child.node, &now_child.node) {
            take_settled_lengths(base_con, now_con);
        }
    }
}

/// How long the close waits for the state file's writes to finish: far longer than a small file's write takes
/// on a working disk. A filesystem slower than that loses the last arrangement, never the file (`persist::save`
/// writes through a `.tmp` and a rename).
pub const CLOSE_FLUSH_BOUND: Duration = Duration::from_secs(5);

/// Everything one save needs, copied on the caller's thread: no `Rc`, nothing borrowed, so the writer thread can
/// read, decode and write while the window goes on.
struct SaveJob {
    dir: PathBuf,
    project_root: PathBuf,
    /// Whose file in the pair this window writes.
    role: Role,
    /// The registered Lua panels: the file read again before the write is decoded as `load` decodes it, so
    /// that "unusable" and "partly usable" mean what they meant when the window opened.
    lua: Vec<ModuleDecl>,
    /// The modules the window never places by itself, decoded with as at startup (`persist::load_with`).
    deferred: Vec<ModuleId>,
    /// The text of a file this window opened only in part, not yet copied aside.
    keep_copy: Option<String>,
    /// Every workspace, the current one's layout being `current`, as they were when the job was made.
    workspaces: Workspaces,
    current: Layout,
    /// Every module the window places, in any workspace (`Workspaces::placed`).
    placed: BTreeSet<ModuleId>,
    /// The arrangement this job writes: what `base` becomes once the write is taken.
    now: Arrangement,
    /// Which of the window's submits this is, counting up: its result is matched to what the window kept of it.
    job: u64,
    /// Where the job's lines go.
    log: Log,
}

/// What became of one [`SaveJob`].
enum SaveDone {
    /// The file holds `now`. `kept` are the file's kept keys as they were just before the write, for the live
    /// workspaces to take (`None` when there was no file to take them from); `copy_taken` says the job dealt with the bytes this window opened (copied them
    /// aside, or found the file was no longer those bytes), so they are not looked at again.
    Written {
        now: Arrangement,
        job: u64,
        kept: Option<Kept>,
        copy_taken: bool,
    },
    /// Nothing was written; the reason is logged and returned for the window's warning.
    /// The next change, or the close, tries again.
    NotWritten(String),
}

/// The read-decide-write of one save, away from any display: the bytes this window opened copied aside while
/// they are still the file, the file as it is now made safe to write over, then the write.
fn run_save(mut job: SaveJob) -> SaveDone {
    // Not flags set at startup: the file may have changed since -- a hand-edit, another window, another build
    // -- and what was true of it when this window opened says nothing of it now.
    let log = job.log.clone();
    if let Err(why) = copy_what_was_opened(&log, &job.dir, &job.project_root, job.role, job.keep_copy.as_deref()) {
        return SaveDone::NotWritten(why);
    }
    let kept = match make_way(&job) {
        Ok(kept) => kept,
        Err(why) => return SaveDone::NotWritten(why),
    };
    if let Some(kept) = &kept {
        job.workspaces.retake_kept(kept.clone());
    }
    match persist::save_as(&job.dir, &job.project_root, job.role, &job.workspaces, &job.current) {
        Ok(_) => SaveDone::Written {
            now: job.now,
            job: job.job,
            kept,
            copy_taken: job.keep_copy.is_some(),
        },
        Err(err) => {
            let why = format!("could not save {}: {err}", job.dir.display());
            log.warn(format!("[layout] {why}"));
            SaveDone::NotWritten(why)
        }
    }
}

/// Before the first write after opening a file only in part: its bytes (`opened`) copied aside, if the file is
/// still those bytes. An error leaves the file unwritten; the copy is tried
/// again at the next save.
fn copy_what_was_opened(
    log: &Log,
    dir: &Path,
    project_root: &Path,
    role: Role,
    opened: Option<&str>,
) -> Result<(), String> {
    let Some(opened) = opened else {
        return Ok(());
    };
    match persist::read_v2_text_as(dir, project_root, role) {
        Ok(Some(text)) if text == opened => match persist::copy_aside_v2_as(dir, project_root, role, opened) {
            Ok(aside) => log.say(format!(
                "[layout] the file this window opened, of which this build could not carry everything, is kept \
                 as {}",
                aside.display()
            )),
            Err(err) => {
                let why = format!("could not copy aside the file this window opened: {err}");
                log.warn(format!("[layout] {why}; not saved"));
                return Err(why);
            }
        },
        // Another window wrote since, or the file is gone: what is there is not the text this window
        // opened, and not this window's to keep.
        Ok(_) => {}
        Err(err) => {
            let why = format!("could not read the file to copy it aside: {err}");
            log.warn(format!("[layout] {why}; not saved"));
            return Err(why);
        }
    }
    Ok(())
}

/// The file as it is now, made safe to write over: wholly unusable, set aside; partly usable, its text copied
/// aside; usable or partly, its kept keys returned to be taken. A kept key naming a module this window places
/// -- in any of its workspaces now, not only in the file's trees -- is not taken: a later build may have
/// floated a module this window has since tiled, and writing both would put it in two places; the file's text
/// is copied aside instead, as for a partly usable file. An error leaves the file unwritten;
/// `Ok(None)` means there is no file, or it was set aside, so there are no kept keys to take.
fn make_way(job: &SaveJob) -> Result<Option<Kept>, String> {
    let (dir, project_root, role, log) = (job.dir.as_path(), job.project_root.as_path(), job.role, &job.log);
    let (text, mut kept, mut copy) = match persist::v2_state_as(dir, project_root, role, &job.lua, &job.deferred) {
        V2State::Missing => return Ok(None),
        V2State::Usable(text, kept) => (text, kept, false),
        V2State::Partly(text, kept) => (text, kept, true),
        V2State::Unusable(_) => {
            return match persist::set_aside_v2_as(dir, project_root, role) {
                Ok(aside) => {
                    log.say(format!(
                        "[layout] the file this build could not use is kept as {}",
                        aside.display()
                    ));
                    Ok(None)
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(err) => {
                    let why = format!("could not set aside the file this build could not use: {err}");
                    log.warn(format!("[layout] {why}; not saved"));
                    Err(why)
                }
            };
        }
    };
    let dropped = kept.drop_placed(&job.placed);
    copy |= !dropped.is_empty();
    if copy {
        match persist::copy_aside_v2_as(dir, project_root, role, &text) {
            Ok(aside) => log.say(format!(
                "[layout] the file, part of which this build cannot carry, is kept as {}",
                aside.display()
            )),
            Err(err) => {
                let why = format!("could not copy aside the file this build can only partly use: {err}");
                log.warn(format!("[layout] {why}; not saved"));
                return Err(why);
            }
        }
    }
    for note in dropped {
        log.say(format!("[layout] {note}"));
    }
    Ok(Some(kept))
}

/// What the writer thread and the window share.
#[derive(Default)]
struct WriterState {
    /// The next job; a newer one replaces it before it starts, since only the newest arrangement is worth
    /// writing.
    pending: Option<SaveJob>,
    /// Whether a job is being run now.
    running: bool,
    /// A finished job's wake is still being posted; a flush must wait for it too.
    waking: bool,
    /// Results the window has not taken yet, in the order their jobs ran.
    done: Vec<SaveDone>,
    /// The window is gone: the thread runs what is pending and ends.
    closed: bool,
}

struct WriterShared {
    state: Mutex<WriterState>,
    /// Signalled when a job is submitted, when one finishes, and when the window goes.
    changed: std::sync::Condvar,
    /// Told after each job, so the caller's thread takes the result; `None` until the controller gives one.
    waker: Mutex<Option<Arc<dyn Waker>>>,
    log: Log,
    /// How many writes finished, for tests that count them.
    #[cfg(test)]
    written: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    gate: Option<Arc<WriteGate>>,
}

impl WriterShared {
    fn lock(&self) -> std::sync::MutexGuard<'_, WriterState> {
        // A job runs outside the lock, so a poisoned lock holds only finished bookkeeping.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// One thread that runs the window's saves, in order, the newest pending job replacing an older pending one:
/// the state file is read, decoded and written there, never on the UI thread, which only copies what a save
/// needs. When a job finishes the thread tells the waker it was given, and the UI thread takes the result
/// ([`Saver::collect`]).
struct LayoutWriter {
    shared: Arc<WriterShared>,
}

impl LayoutWriter {
    fn spawn(log: Log) -> LayoutWriter {
        LayoutWriter::start(WriterShared {
            state: Default::default(),
            changed: Default::default(),
            waker: Mutex::new(None),
            log,
            #[cfg(test)]
            written: Default::default(),
            #[cfg(test)]
            gate: None,
        })
    }

    fn start(shared: WriterShared) -> LayoutWriter {
        let shared = Arc::new(shared);
        let thread_shared = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("layout-writer".into())
            .spawn(move || LayoutWriter::run(&thread_shared));
        if let Err(err) = spawned {
            // Without the thread nothing is ever written: said once, and every flush then reports it.
            shared.log.warn(format!(
                "[layout] could not start the layout writer ({err}); this window's layout will not be kept"
            ));
        }
        LayoutWriter { shared }
    }

    fn run(shared: &WriterShared) {
        loop {
            let job = {
                let mut state = shared.lock();
                while state.pending.is_none() && !state.closed {
                    state = shared.changed.wait(state).unwrap_or_else(|e| e.into_inner());
                }
                let Some(job) = state.pending.take() else { return };
                state.running = true;
                job
            };
            #[cfg(test)]
            if let Some(gate) = &shared.gate {
                gate.hold();
            }
            let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_save(job))).unwrap_or_else(|_| {
                shared
                    .log
                    .warn("[layout] the layout writer failed while saving; not saved".to_string());
                SaveDone::NotWritten("the layout writer failed while saving".into())
            });
            #[cfg(test)]
            if matches!(done, SaveDone::Written { .. }) {
                shared.written.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            }
            {
                let mut state = shared.lock();
                state.done.push(done);
                state.running = false;
                state.waking = true;
            }
            // Cloned out first, so the waker never runs with the lock held.
            let waker = shared.waker.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if let Some(waker) = waker {
                waker.wake();
            }
            shared.lock().waking = false;
            shared.changed.notify_all();
        }
    }

    fn submit(&self, job: SaveJob) {
        self.shared.lock().pending = Some(job);
        self.shared.changed.notify_all();
    }

    /// Waits for every submitted job and its completion wake, at most `bound`; whether they all finished.
    fn flush(&self, bound: Duration) -> bool {
        let deadline = Instant::now() + bound;
        let mut state = self.shared.lock();
        while state.pending.is_some() || state.running || state.waking {
            let Some(left) = deadline
                .checked_duration_since(Instant::now())
                .filter(|left| !left.is_zero())
            else {
                return false;
            };
            state = self
                .shared
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        true
    }

    /// Results not yet taken, without waiting.
    fn take_done(&self) -> Vec<SaveDone> {
        std::mem::take(&mut self.shared.lock().done)
    }

    /// Whether no job is pending or running: every result there will be is then among those not yet taken.
    fn idle(&self) -> bool {
        let state = self.shared.lock();
        state.pending.is_none() && !state.running
    }
}

impl Drop for LayoutWriter {
    fn drop(&mut self) {
        // A pending job is still run: the thread ends once nothing is left.
        self.shared.lock().closed = true;
        self.shared.changed.notify_all();
        #[cfg(test)]
        if let Some(gate) = &self.shared.gate {
            gate.open();
        }
    }
}

/// Holds the writer before each job until the test opens it, or lets one job through, so a test can change
/// the arrangement, or close, while a write is under way.
#[cfg(test)]
struct WriteGate {
    state: Mutex<GateState>,
    changed: std::sync::Condvar,
}

#[cfg(test)]
#[derive(Default)]
struct GateState {
    /// Every job may run.
    open: bool,
    /// Jobs that may run while the gate is closed, one each.
    passes: usize,
    /// How many jobs have reached the gate.
    held: usize,
}

#[cfg(test)]
impl WriteGate {
    fn closed() -> Arc<WriteGate> {
        Arc::new(WriteGate {
            state: Default::default(),
            changed: std::sync::Condvar::new(),
        })
    }

    fn hold(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.held += 1;
        self.changed.notify_all();
        while !state.open && state.passes == 0 {
            state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        if !state.open {
            state.passes -= 1;
        }
    }

    fn open(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).open = true;
        self.changed.notify_all();
    }

    fn close(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).open = false;
    }

    /// Lets the job held at the gate, or the next to reach it, run while the gate stays closed.
    fn pass_one(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).passes += 1;
        self.changed.notify_all();
    }

    /// Waits until `count` jobs have reached the gate; `false` after a hang guard of ten seconds.
    fn wait_held(&self, count: usize) -> bool {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (state, _) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(10), |state| state.held < count)
            .unwrap_or_else(|e| e.into_inner());
        state.held >= count
    }
}

/// One arrangement given to the writer, as `base` is to hold it once that write is taken: the job's own
/// arrangement with each pinned length that settled on screen since. A length the window changed after the
/// submit is not in it -- that change belongs to a later job, and is written, or retried, as that job's.
struct Submitted {
    job: u64,
    arrangement: Arrangement,
}

/// What a debounce timer that fired leads to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fired {
    /// The last change came after the timer was armed: it is armed again for what is left of the wait.
    Rearm(Duration),
    /// The arrangement changed, and was handed to the writer.
    Saved,
    /// The arrangement is as it was last handed to the writer or written: nothing to do.
    NothingToSave,
}

/// Decides when the state file is written, [`persist::SAVE_DEBOUNCE_MS`] after the last change of the
/// arrangement and at close -- what, and when, is this module's doc -- and hands each write to a
/// [`LayoutWriter`]'s thread, which reads, decodes and writes the file; the caller's thread only copies what a
/// save needs and takes the results ([`Saver::collect`]).
///
/// The saver arms nothing itself: [`Saver::changed`] and [`Saver::timer_fired`] answer how long a timer is to
/// wait, and the controller arms it on its scheduler. It holds no layout either: every call is given the
/// window's current layout and workspaces, read by the caller.
pub struct Saver {
    dir: Option<PathBuf>,
    project_root: PathBuf,
    /// Whose file in the pair this window writes: window 1 (or a window alone) today's, window 2 and an unpaired
    /// window their own.
    role: Role,
    /// The registered Lua panels, for the writer's decode of the file and for `Workspaces::placed`.
    lua: Vec<ModuleDecl>,
    /// The modules the window never places by itself: the file read again before a write is decoded with
    /// them, as it was read at startup, so one the file holds is not taken for a module this build cannot carry.
    deferred: Vec<ModuleId>,
    /// The exact text of a file this window opened only in part, until a write that copied it aside
    /// (or found the file is no longer those bytes) is taken.
    keep_copy: Option<String>,
    /// Whether a debounce timer waits: a change while one does only moves the time it waits for.
    armed: bool,
    /// When the last change was reported: what a debounce timer that fires checks.
    last_change: Option<Instant>,
    /// The arrangement the window opened with (its pinned lengths as they settled), or the one it last
    /// wrote, as far as the results taken so far tell.
    base: Arrangement,
    /// The arrangements given to the writer whose jobs may still be pending, running, or finished and not
    /// taken, oldest first; empty once every result is taken. A change is measured from the last when there
    /// is one, else from `base`: an arrangement changed back to `base` while a different one is being written
    /// is itself a change, and is written after it.
    submitted: Vec<Submitted>,
    /// The number the next submit's job carries.
    next_job: u64,
    failing: bool,
    notes: Vec<String>,
    writer: LayoutWriter,
    log: Log,
}

impl Saver {
    /// `dir` is `persist::state_dir`'s answer; `None` (no usable `XDG_STATE_HOME` or `HOME`) saves
    /// nothing, said once here. `layout` and `workspaces` are the startup ones, not yet changed: a
    /// launch is not a change, whether it opened a file, the Lua default or the first launch.
    /// `keep_copy` is [`choose_startup_layout`]'s.
    pub fn new(
        dir: Option<PathBuf>,
        project_root: &Path,
        lua: Vec<ModuleDecl>,
        keep_copy: Option<String>,
        layout: &Layout,
        workspaces: &Workspaces,
    ) -> Saver {
        Saver::with_writer(
            dir,
            project_root,
            lua,
            keep_copy,
            layout,
            workspaces,
            LayoutWriter::spawn(Log::default()),
        )
    }

    fn with_writer(
        dir: Option<PathBuf>,
        project_root: &Path,
        lua: Vec<ModuleDecl>,
        keep_copy: Option<String>,
        layout: &Layout,
        workspaces: &Workspaces,
        writer: LayoutWriter,
    ) -> Saver {
        let log = writer.shared.log.clone();
        if dir.is_none() {
            log.warn(
                "[layout] no usable state directory (XDG_STATE_HOME absolute, or HOME absolute); this window's \
                 layout will not be kept"
                    .to_string(),
            );
        }
        Saver {
            dir,
            project_root: project_root.to_path_buf(),
            role: Role::Main,
            lua,
            deferred: Vec::new(),
            keep_copy,
            armed: false,
            last_change: None,
            base: Arrangement::of(workspaces, layout),
            submitted: Vec::new(),
            next_job: 0,
            failing: false,
            notes: Vec::new(),
            writer,
            log,
        }
    }

    /// What the writer thread wakes when a write finishes, so the caller's thread takes the result
    /// ([`Saver::collect`]) without waiting for the next change.
    pub fn set_waker(&mut self, waker: Arc<dyn Waker>) {
        *self.writer.shared.waker.lock().unwrap_or_else(|e| e.into_inner()) = Some(waker);
    }

    /// The modules the window never places by itself, as the startup read used them (`persist::load_with`).
    pub fn set_deferred(&mut self, deferred: Vec<ModuleId>) {
        self.deferred = deferred;
    }

    /// Whose file in the pair this window writes (`persist::file_name_v2_as`); window 1's until set.
    pub fn set_role(&mut self, role: Role) {
        self.role = role;
    }

    /// The layout changed at `now`: how long a debounce timer is to wait before the arrangement is compared and
    /// written. `None` when nothing is to be armed: no state directory; a timer already waits (the change only
    /// moves the time it waits for, [`debounce_left`]); or the arrangement is as it was -- a zoom, or its end,
    /// which a move of the keys out of a zoomed module also reports. A launch never reaches this: the shell
    /// reports changes only after its startup layout is applied.
    pub fn changed(&mut self, now: Instant, layout: &Layout, workspaces: &Workspaces) -> Option<Duration> {
        self.dir.as_ref()?;
        self.last_change = Some(now);
        if self.armed || !self.differs(&Arrangement::of(workspaces, layout)) {
            return None;
        }
        self.armed = true;
        Some(Duration::from_millis(persist::SAVE_DEBOUNCE_MS))
    }

    /// The layout changed at `now` while it could not be read (a shell closure held it): the time counts, but
    /// nothing is armed; the next change arms the timer, and the close saves whatever the arrangement is.
    pub fn changed_unread(&mut self, now: Instant) {
        if self.dir.is_some() {
            self.last_change = Some(now);
        }
    }

    /// The debounce timer fired at `now`: armed again for what is left when a change came since it was armed,
    /// else the arrangement handed to the writer if it changed since it was last submitted or written. Returns
    /// without waiting for the write.
    pub fn timer_fired(&mut self, now: Instant, layout: &Layout, workspaces: &Workspaces) -> Fired {
        if let Some(left) = self.fired_early(now) {
            return Fired::Rearm(left);
        }
        if self.submit_if_changed(layout, workspaces) {
            Fired::Saved
        } else {
            Fired::NothingToSave
        }
    }

    /// The debounce timer fired at `now` while the layout could not be read: what is left of the wait, if a
    /// change came since it was armed; else nothing is saved this time (said so), and the next change arms the
    /// timer again.
    pub fn timer_fired_unread(&mut self, now: Instant) -> Option<Duration> {
        let left = self.fired_early(now);
        if left.is_none() {
            self.log
                .warn("[layout] the layout was busy; not saved this time".to_string());
        }
        left
    }

    /// The timer is spent; armed again, for what is left, when the last change was too recent.
    fn fired_early(&mut self, now: Instant) -> Option<Duration> {
        let since = self
            .last_change
            .map_or(Duration::MAX, |at| now.saturating_duration_since(at));
        let left = debounce_left(since);
        self.armed = left.is_some();
        left
    }

    /// A pinned split got its length on screen; `settled` is the current tree as the settle left it. That is
    /// not a change, so the length joins `base`, and each arrangement given to the writer and not yet taken back.
    pub fn settled(&mut self, settled: &Con) {
        if let Some(root) = self.base.current_root_mut() {
            take_settled_lengths(root, settled);
        }
        for submitted in self.submitted.iter_mut() {
            if let Some(root) = submitted.arrangement.current_root_mut() {
                take_settled_lengths(root, settled);
            }
        }
    }

    /// The close's save: the window's workspaces handed to the writer if their arrangement changed since they
    /// were last submitted or written, then a wait of at most [`CLOSE_FLUSH_BOUND`] for the writer to finish
    /// every job, so the window's final arrangement is the file's last write. A timer still armed is the
    /// caller's to cancel. Whether every job finished within the bound (written, or failed and said so);
    /// `false` with no state directory.
    pub fn save_now(&mut self, layout: &Layout, workspaces: &Workspaces) -> bool {
        self.armed = false;
        if self.dir.is_none() {
            return false;
        }
        self.submit_if_changed(layout, workspaces);
        self.flush()
    }

    /// Hands the window's workspaces to the writer now if their arrangement differs from what a change is measured
    /// from, without waiting for the write: for a startup that changed the layout it opened on purpose (the
    /// window's pair taking back what its other window held), which [`Saver::with_base`] measured from before that
    /// change. Whether a job was submitted.
    pub fn submit_now(&mut self, layout: &Layout, workspaces: &Workspaces) -> bool {
        self.submit_if_changed(layout, workspaces)
    }

    /// Measures changes from `layout` and `workspaces` instead of the ones the saver was made with: a startup
    /// change made after them is then a change, written by [`Saver::submit_now`] or the close.
    pub fn with_base(mut self, layout: &Layout, workspaces: &Workspaces) -> Saver {
        self.base = Arrangement::of(workspaces, layout);
        self
    }

    /// Measures changes from `layout` and `workspaces` as they are now: for a launch that changed the layout it
    /// opened on purpose after the saver was made (a file to open shows the editor), which is then no change, so
    /// neither a timer nor the close writes it. Only a change made after this one is a change. With a write
    /// outstanding, the measure is that write's entry and `base` stays: if the write succeeds, `collect` takes the
    /// entry as the base; if it fails, what it was to write is still unwritten, and the close writes it, this
    /// change with it.
    pub fn rebase(&mut self, layout: &Layout, workspaces: &Workspaces) {
        let now = Arrangement::of(workspaces, layout);
        match self.submitted.last_mut() {
            Some(last) => last.arrangement = now,
            None => self.base = now,
        }
    }

    /// The close's save while the layout could not be read: nothing more is handed over (said so), and the
    /// writer is waited for as [`Saver::save_now`] waits.
    pub fn save_now_unread(&mut self) -> bool {
        self.armed = false;
        if self.dir.is_none() {
            return false;
        }
        self.log
            .warn("[layout] the layout was busy; not saved this time".to_string());
        self.flush()
    }

    /// Whether the file holds `layout` and `workspaces` as far as the results taken so far tell: a state directory,
    /// nothing given to the writer whose result is untaken, and the arrangement the same as the last one written
    /// (or the one the window opened with, when it never changed). A failed write leaves the arrangement it was
    /// to write differing, so this is `false` until a later write succeeds.
    pub fn holds(&self, layout: &Layout, workspaces: &Workspaces) -> bool {
        self.dir.is_some() && self.submitted.is_empty() && !self.differs(&Arrangement::of(workspaces, layout))
    }

    fn flush(&mut self) -> bool {
        let finished = self.writer.flush(CLOSE_FLUSH_BOUND);
        if !finished {
            self.log.warn(format!(
                "[layout] the layout was still being written after {}s; not waiting longer",
                CLOSE_FLUSH_BOUND.as_secs()
            ));
        }
        finished
    }

    /// Whether `now` differs from what a change is measured from: the arrangement last submitted while
    /// one is outstanding, else `base`.
    fn differs(&self, now: &Arrangement) -> bool {
        match self.submitted.last() {
            Some(submitted) => *now != submitted.arrangement,
            None => *now != self.base,
        }
    }

    /// Gives the writer this window's workspaces if their arrangement changed since it was last submitted
    /// or written, and returns without waiting for the write. Whether a job was submitted.
    fn submit_if_changed(&mut self, layout: &Layout, workspaces: &Workspaces) -> bool {
        let Some(dir) = &self.dir else { return false };
        let now = Arrangement::of(workspaces, layout);
        if !self.differs(&now) {
            return false;
        }
        let job = SaveJob {
            dir: dir.clone(),
            project_root: self.project_root.clone(),
            role: self.role,
            lua: self.lua.clone(),
            deferred: self.deferred.clone(),
            keep_copy: self.keep_copy.clone(),
            workspaces: workspaces.clone(),
            current: layout.clone(),
            placed: workspaces.placed(layout, &self.lua),
            now,
            job: self.next_job,
            log: self.log.clone(),
        };
        self.next_job = job.job + 1;
        self.submitted.push(Submitted {
            job: job.job,
            arrangement: job.now.clone(),
        });
        self.writer.submit(job);
        true
    }

    /// Takes the writer's finished saves: a write moves `base` and gives the live workspaces the file's
    /// kept keys; once nothing is outstanding, changes are measured from `base` again, so a write that
    /// failed leaves its change to be tried again. Called before every other call, and after the close's
    /// save; `workspaces` is `None` while they cannot be changed, and then they keep the kept keys they have
    /// (the next write takes the file's again).
    pub fn collect(&mut self, mut workspaces: Option<&mut Workspaces>) {
        let mut done = self.writer.take_done();
        let idle = self.writer.idle();
        if idle {
            // Finished between the two calls above: taken now, since no other result can follow.
            done.extend(self.writer.take_done());
        }
        for result in done {
            if let SaveDone::NotWritten(why) = &result {
                if !self.failing {
                    self.notes.push(format!("layout not saved: {why}"));
                }
                self.failing = true;
                continue;
            }
            self.failing = false;
            let SaveDone::Written {
                now,
                job,
                kept,
                copy_taken,
            } = result
            else {
                continue;
            };
            // What the window kept of this job holds the lengths that settled on screen since its submit,
            // which are not a change; jobs submitted before it were run before it or replaced by it.
            self.submitted.retain(|earlier| earlier.job >= job);
            self.base = match self.submitted.first() {
                Some(this) if this.job == job => this.arrangement.clone(),
                _ => now,
            };
            if copy_taken {
                self.keep_copy = None;
            }
            if let (Some(kept), Some(workspaces)) = (kept, workspaces.as_deref_mut()) {
                workspaces.retake_kept(kept);
            }
        }
        if idle {
            self.submitted.clear();
        }
    }

    /// Save failures not yet shown, once per streak of failed writes.
    pub fn take_notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notes)
    }

    /// Every line this saver and its writer printed.
    #[cfg(test)]
    pub(super) fn lines(&self) -> Vec<String> {
        self.log.lines()
    }

    /// How many of this saver's writes finished.
    #[cfg(test)]
    pub(super) fn writes(&self) -> usize {
        self.writer.shared.written.load(std::sync::atomic::Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use crate::layout::{Float, FloatRect};

    fn plugin_tree() -> LuaTree {
        LuaTree {
            tiled: Node::split(
                Axis::Row,
                0.6,
                Node::Leaf(ModuleId::lua("demo")),
                Node::split(
                    Axis::Column,
                    0.7,
                    Node::Leaf(ModuleId::editor()),
                    Node::Leaf(ModuleId::terminal()),
                ),
            ),
            floats: vec![],
        }
    }

    fn plugin_window(tree: &LuaTree) -> PluginWindow<'_> {
        PluginWindow {
            page: ModuleId::lua("demo"),
            tree,
            focus: WindowFocus::Page,
        }
    }

    #[test]
    fn startup_takes_saved_then_plugin_tree_then_default_then_builtin() {
        let plugin = plugin_tree();
        let default = template(ModuleId::agent(), ModuleId::editor());
        let mut saved = first_launch_layout(&[]).unwrap();
        saved.show(&ModuleId::terminal()).unwrap();
        // A saved page's hidden state and position still win.
        crate::layout::place(&mut saved, &ModuleId::lua("demo"), &ModuleId::agent(), Axis::Column).unwrap();
        saved.set_focus(&ModuleId::agent()).unwrap();
        let choose = |tree, plugin, loaded| {
            choose_startup_layout(
                tree,
                plugin,
                loaded,
                &[],
                &OpenRules::default(),
                None,
                &[ModuleId::lua("demo")],
                &[],
            )
            .unwrap()
            .1
        };
        assert_eq!(
            choose(
                Some(&default),
                Some(plugin_window(&plugin)),
                Some(restored(saved.clone()))
            ),
            saved
        );
        let own = choose(Some(&default), Some(plugin_window(&plugin)), None);
        assert!(own.is_shown(&ModuleId::lua("demo")));
        assert_eq!(own.root().children[0].node, Node::Leaf(ModuleId::lua("demo")));
        assert_eq!(own.root().children[0].share, 0.6);
        let lua = choose(Some(&default), None, None);
        assert_eq!(lua.leaves()[0], ModuleId::agent());
        assert!(!lua.contains(&ModuleId::lua("demo")));
        assert_eq!(choose(None, None, None), first_launch_layout(&[]).unwrap());
    }

    #[test]
    fn restore_main_fallback_takes_the_same_order() {
        let plugin = plugin_tree();
        let default = template(ModuleId::agent(), ModuleId::editor());
        for (tree, own) in [(Some(&default), true), (Some(&default), false), (None, false)] {
            let loaded = Loaded::Restored(persist::Opened {
                workspaces: Workspaces::from_parts(
                    vec!["study".into()],
                    Default::default(),
                    "study".into(),
                    Default::default(),
                )
                .unwrap(),
                current: first_launch_layout(&[]).unwrap(),
                notes: vec![],
                keep_copy: None,
            });
            let choose = |loaded| {
                choose_startup_layout(
                    tree,
                    own.then(|| plugin_window(&plugin)),
                    loaded,
                    &[],
                    &OpenRules::default(),
                    None,
                    &[ModuleId::lua("demo")],
                    &[],
                )
                .unwrap()
            };
            let (spaces, _, _, _) = choose(Some(loaded));
            assert_eq!(spaces.order(), ["main", "study"]);
            assert_eq!(spaces.stored("main"), Some(&choose(None).1));
        }
    }

    #[test]
    fn a_saved_layout_without_the_page_gets_it_back_at_its_window_place_in_the_own_window() {
        let tree = plugin_tree();
        let saved = first_launch_layout(&[]).unwrap();
        let text = persist::encode_v2(&Workspaces::single(), &saved, root());
        let opened = persist::decode_v2_with(&text, root(), &[], &[ModuleId::lua("demo")]).unwrap();
        assert!(!opened.current.contains(&ModuleId::lua("demo")));
        let (_, layout, notes, _) = choose_startup_layout(
            None,
            Some(plugin_window(&tree)),
            Some(Loaded::Restored(opened)),
            &[],
            &OpenRules::default(),
            None,
            &[ModuleId::lua("demo")],
            &[],
        )
        .unwrap();
        assert!(layout.is_shown(&ModuleId::lua("demo")));
        let Node::Con(placed) = &layout.root().children[0].node else {
            panic!("page wraps its declared siblings")
        };
        assert_eq!(placed.children[0].node, Node::Leaf(ModuleId::lua("demo")));
        assert_eq!(placed.children[0].share, 0.6);
        assert_eq!(layout.root().children[1].node, Node::Leaf(ModuleId::agent()));
        assert!(
            notes.contains(&"lua:demo: shown at its [window] place".into()),
            "{notes:?}"
        );
    }

    #[test]
    fn nowhere_else() {
        let saved = first_launch_layout(&[]).unwrap();
        let (_, layout, notes, _) = choose_startup_layout(
            None,
            None,
            Some(restored(saved.clone())),
            &[],
            &OpenRules::default(),
            None,
            &[ModuleId::lua("demo")],
            &[],
        )
        .unwrap();
        assert_eq!(layout, saved);
        assert!(!notes.iter().any(|note| note.contains("[window] place")));
    }

    #[test]
    fn nothing_else_in_the_saved_layout_changes() {
        let tree = plugin_tree();
        let mut saved = first_launch_layout(&[]).unwrap();
        saved.show(&ModuleId::terminal()).unwrap();
        saved.set_focus(&ModuleId::terminal()).unwrap();
        let rect = FloatRect {
            cx: 0.7,
            cy: 0.3,
            w: 240,
            h: 180,
        };
        crate::layout::float_at(
            &mut saved,
            &ModuleId::agent(),
            Some(rect),
            &Frame::new(FIRST_LAUNCH_FRAME, 1),
        )
        .unwrap();
        saved.set_focus(&ModuleId::terminal()).unwrap();
        let mut spaces = Workspaces::single();
        spaces.create("other", saved.clone()).unwrap();
        let loaded = Loaded::Restored(persist::Opened {
            workspaces: spaces,
            current: saved.clone(),
            notes: vec![],
            keep_copy: Some("original".into()),
        });
        let (spaces, layout, _, copy) = choose_startup_layout(
            None,
            Some(plugin_window(&tree)),
            Some(loaded),
            &[],
            &OpenRules::default(),
            None,
            &[ModuleId::lua("demo")],
            &[],
        )
        .unwrap();
        assert_eq!(layout.root().children[1].node, Node::Con(saved.root().clone()));
        assert_eq!(layout.hidden(), saved.hidden());
        assert_eq!(layout.floats(), saved.floats());
        assert_eq!(layout.float_memory(), saved.float_memory());
        assert_eq!(layout.focus(), saved.focus());
        assert_eq!(spaces.stored("other"), Some(&saved));
        assert_eq!(copy.as_deref(), Some("original"));
    }

    #[test]
    fn window_focus_names_the_module_that_takes_the_keys_at_the_first_focus() {
        let tree = plugin_tree();
        for (focus, module) in [
            (WindowFocus::Page, ModuleId::lua("demo")),
            (WindowFocus::Editor, ModuleId::editor()),
            (WindowFocus::Terminal, ModuleId::terminal()),
        ] {
            let (_, layout, _, _) = choose_startup_layout(
                None,
                Some(PluginWindow {
                    focus,
                    ..plugin_window(&tree)
                }),
                None,
                &[],
                &OpenRules::default(),
                None,
                &[ModuleId::lua("demo")],
                &[],
            )
            .unwrap();
            assert_eq!(layout.focus(), &module);
            assert!(layout.is_visible(&module));
        }
    }

    #[test]
    fn an_own_window_tree_may_omit_builtins_and_keeps_them_available_hidden() {
        let tree = LuaTree {
            tiled: Node::Leaf(ModuleId::lua("demo")),
            floats: vec![],
        };
        let (_, layout, _, _) = choose_startup_layout(
            None,
            Some(plugin_window(&tree)),
            None,
            &[],
            &OpenRules::default(),
            None,
            &[ModuleId::lua("demo")],
            &[],
        )
        .unwrap();
        assert_eq!(layout.visible_modules(), [ModuleId::lua("demo")]);
        for id in [ModuleId::editor(), ModuleId::agent(), ModuleId::terminal()] {
            assert!(layout.contains(&id));
            assert!(!layout.is_shown(&id));
        }
    }

    /// The share `id` holds in the container that holds it directly, if it is a tile.
    fn share_of(node: &Node, id: &ModuleId) -> Option<f32> {
        let Node::Con(con) = node else { return None };
        con.children.iter().find_map(|child| {
            if child.node == Node::Leaf(id.clone()) {
                Some(child.share)
            } else {
                share_of(&child.node, id)
            }
        })
    }

    #[test]
    fn builtins_a_plugin_tree_leaves_out_show_at_their_first_launch_share() {
        // The tree names the editor but not the chat: the chat waits right of the editor.
        let tree = plugin_tree();
        let (_, mut layout, _, _) = choose_startup_layout(
            None,
            Some(plugin_window(&tree)),
            None,
            &[],
            &OpenRules::default(),
            None,
            &[ModuleId::lua("demo")],
            &[],
        )
        .unwrap();
        assert!(!layout.is_shown(&ModuleId::agent()));
        layout.show(&ModuleId::agent()).unwrap();
        let root = Node::Con(layout.root().clone());
        let chat = share_of(&root, &ModuleId::agent()).expect("the chat is a tile");
        assert!((chat - (1.0 - DEFAULT_EDITOR_SHARE)).abs() < 1e-4, "chat share {chat}");
        // The tree names neither: the editor waits right of the page, the chat right of the editor.
        let tree = LuaTree {
            tiled: Node::Leaf(ModuleId::lua("demo")),
            floats: vec![],
        };
        let (_, mut layout, _, _) = choose_startup_layout(
            None,
            Some(plugin_window(&tree)),
            None,
            &[],
            &OpenRules::default(),
            None,
            &[ModuleId::lua("demo")],
            &[],
        )
        .unwrap();
        layout.show(&ModuleId::editor()).unwrap();
        layout.show(&ModuleId::agent()).unwrap();
        let root = Node::Con(layout.root().clone());
        let page = share_of(&root, &ModuleId::lua("demo")).expect("the page is a tile");
        let chat = share_of(&root, &ModuleId::agent()).expect("the chat is a tile");
        assert!((page - RIGHT_OF_ROOT_SHARE).abs() < 1e-4, "page share {page}");
        assert!((chat - (1.0 - DEFAULT_EDITOR_SHARE)).abs() < 1e-4, "chat share {chat}");
    }

    #[test]
    fn a_saved_hidden_page_comes_back_shown_at_its_window_place() {
        let tree = plugin_tree();
        let mut saved = first_launch_layout(&[]).unwrap();
        crate::layout::place(&mut saved, &ModuleId::lua("demo"), &ModuleId::agent(), Axis::Column).unwrap();
        saved.set_focus(&ModuleId::editor()).unwrap();
        crate::layout::hide(&mut saved, &ModuleId::lua("demo"), &Frame::new(FIRST_LAUNCH_FRAME, 1)).unwrap();
        let (_, layout, notes, _) = choose_startup_layout(
            None,
            Some(plugin_window(&tree)),
            Some(restored(saved.clone())),
            &[],
            &OpenRules::default(),
            None,
            &[ModuleId::lua("demo")],
            &[],
        )
        .unwrap();
        assert!(layout.is_shown(&ModuleId::lua("demo")));
        let Node::Con(placed) = &layout.root().children[0].node else {
            panic!("the page wraps its declared siblings")
        };
        assert_eq!(placed.children[0].node, Node::Leaf(ModuleId::lua("demo")));
        assert_eq!(placed.children[0].share, 0.6);
        assert_eq!(layout.root().children[1].node, Node::Leaf(ModuleId::agent()));
        assert_eq!(layout.focus(), &ModuleId::editor());
        let mut hidden = saved.hidden().clone();
        hidden.remove(&ModuleId::lua("demo"));
        assert_eq!(layout.hidden(), &hidden);
        assert_eq!(
            layout
                .leaves()
                .iter()
                .filter(|id| **id == ModuleId::lua("demo"))
                .count(),
            1
        );
        assert!(
            notes.contains(&"lua:demo: shown at its [window] place".into()),
            "{notes:?}"
        );
    }

    #[test]
    fn a_page_the_other_window_holds_stays_there() {
        let tree = plugin_tree();
        let mut saved = first_launch_layout(&[]).unwrap();
        crate::layout::place(&mut saved, &ModuleId::lua("demo"), &ModuleId::agent(), Axis::Column).unwrap();
        saved.set_focus(&ModuleId::editor()).unwrap();
        saved.set_away(&ModuleId::lua("demo"), true).unwrap();
        let (_, layout, notes, _) = choose_startup_layout(
            None,
            Some(plugin_window(&tree)),
            Some(restored(saved.clone())),
            &[],
            &OpenRules::default(),
            None,
            &[ModuleId::lua("demo")],
            &[],
        )
        .unwrap();
        assert_eq!(layout, saved);
        assert!(!notes.iter().any(|note| note.contains("[window] place")));
    }

    /// The own window's lists for a page that starts shown, declared as a Lua panel right of the root.
    fn own_lists() -> (Vec<ModuleDecl>, Vec<ModuleId>) {
        let decls = [ModuleDecl {
            id: ModuleId::lua("demo"),
            placement: Placement::RightOfRoot,
        }];
        own_window_lists(Some(&ModuleId::lua("demo")), &decls, &[])
    }

    #[test]
    fn a_file_read_for_the_own_window_leaves_an_absent_page_out_until_its_window_place_takes_it() {
        let tree = plugin_tree();
        let decls = [ModuleDecl {
            id: ModuleId::lua("demo"),
            placement: Placement::RightOfRoot,
        }];
        let text = persist::encode_v2(&Workspaces::single(), &first_launch_layout(&[]).unwrap(), root());
        // Read as any other window reads it, the page would be placed, shown, where a Lua panel goes.
        let plain = persist::decode_v2_with(&text, root(), &decls, &[]).unwrap();
        assert!(plain.current.is_shown(&ModuleId::lua("demo")));
        assert_eq!(own_window_lists(None, &decls, &[]), (decls.to_vec(), vec![]));
        let (lua, deferred) = own_lists();
        assert_eq!(deferred, [ModuleId::lua("demo")]);
        let opened = persist::decode_v2_with(&text, root(), &lua, &deferred).unwrap();
        assert!(!opened.current.contains(&ModuleId::lua("demo")));
        let (_, layout, notes, _) = choose_startup_layout(
            None,
            Some(plugin_window(&tree)),
            Some(Loaded::Restored(opened)),
            &lua,
            &OpenRules::default(),
            None,
            &deferred,
            &[],
        )
        .unwrap();
        assert!(layout.is_shown(&ModuleId::lua("demo")));
        assert_eq!(layout.leaves()[0], ModuleId::lua("demo"));
        assert_eq!(
            share_of(&Node::Con(layout.root().clone()), &ModuleId::lua("demo")),
            Some(0.6)
        );
        assert!(
            notes.contains(&"lua:demo: shown at its [window] place".into()),
            "{notes:?}"
        );
    }

    #[test]
    fn a_page_killed_and_saved_that_way_comes_back_at_its_window_place() {
        let tree = plugin_tree();
        let (lua, deferred) = own_lists();
        let choose = |loaded| {
            choose_startup_layout(
                None,
                Some(plugin_window(&tree)),
                loaded,
                &lua,
                &OpenRules::default(),
                None,
                &deferred,
                &[],
            )
            .unwrap()
        };
        let (_, mut first, _, _) = choose(None);
        // A kill puts the page back hidden where its Lua panel declaration places it.
        crate::layout::kill::kill(
            &mut first,
            &ModuleId::lua("demo"),
            crate::layout::kill::Reopen::At(Placement::RightOfRoot),
            &Frame::new(FIRST_LAUNCH_FRAME, 1),
        )
        .unwrap();
        assert!(first.contains(&ModuleId::lua("demo")) && !first.is_shown(&ModuleId::lua("demo")));
        let text = persist::encode_v2(&Workspaces::single(), &first, root());
        let opened = persist::decode_v2_with(&text, root(), &lua, &deferred).unwrap();
        assert!(!opened.current.is_shown(&ModuleId::lua("demo")));
        let (_, layout, notes, _) = choose(Some(Loaded::Restored(opened)));
        assert!(layout.is_shown(&ModuleId::lua("demo")));
        assert_eq!(layout.leaves()[0], ModuleId::lua("demo"));
        assert_eq!(
            share_of(&Node::Con(layout.root().clone()), &ModuleId::lua("demo")),
            Some(0.6)
        );
        assert!(
            notes.contains(&"lua:demo: shown at its [window] place".into()),
            "{notes:?}"
        );
    }

    #[test]
    fn a_missing_page_between_declared_siblings_returns_between_them() {
        use crate::layout::{Child, ConLayout};
        let tree = LuaTree {
            tiled: Node::Con(Con::new(
                ConLayout::SplitH,
                vec![
                    Child::new(Node::Leaf(ModuleId::editor()), 0.4),
                    Child::new(Node::Leaf(ModuleId::lua("demo")), 0.2),
                    Child::new(Node::Leaf(ModuleId::agent()), 0.4),
                ],
            )),
            floats: vec![],
        };
        let saved = first_launch_layout(&[]).unwrap();
        let (_, layout, _, _) = choose_startup_layout(
            None,
            Some(plugin_window(&tree)),
            Some(restored(saved.clone())),
            &[],
            &OpenRules::default(),
            None,
            &[ModuleId::lua("demo")],
            &[],
        )
        .unwrap();
        assert_eq!(layout.root().children[1].node, Node::Leaf(ModuleId::lua("demo")));
        assert_eq!(layout.root().children[1].share, 0.2);
        assert_eq!(layout.root().children[0].node, saved.root().children[0].node);
        assert_eq!(layout.root().children[2].node, saved.root().children[1].node);
    }

    fn template(first: ModuleId, second: ModuleId) -> LuaTree {
        LuaTree {
            tiled: Node::split(Axis::Row, 0.5, Node::Leaf(first), Node::Leaf(second)),
            floats: Vec::new(),
        }
    }

    /// The way back from compositor mode: `agent#3` has a tile again, in the one workspace on screen and hidden in the
    /// others, and a view something already holds stays where it is.
    #[test]
    fn a_saved_view_no_layout_holds_comes_back_as_a_tile() {
        let agent_3 = ModuleId::instance(ModuleKind::Agent, 3).unwrap();
        let agent_2 = ModuleId::instance(ModuleKind::Agent, 2).unwrap();
        let mut layout = Layout::initial(&[]).unwrap();
        let mut workspaces = Workspaces::single();
        workspaces.create("other", Layout::initial(&[]).unwrap()).unwrap();
        let keys = layout.focus().clone();

        assert!(tile_saved_views(&mut layout, &mut workspaces, &[3]).is_empty());
        assert!(layout.is_shown(&agent_3));
        assert_eq!(*layout.focus(), keys, "the keys stay");
        let (name, other) = workspaces
            .each(&layout)
            .into_iter()
            .find(|(name, _)| *name == "other")
            .unwrap();
        assert!(other.contains(&agent_3) && !other.is_shown(&agent_3), "{name}");

        let before = layout.clone();
        assert!(tile_saved_views(&mut layout, &mut workspaces, &[3]).is_empty());
        assert_eq!(layout, before, "once there it is left alone");

        crate::layout::hide(&mut layout, &ModuleId::agent(), &Frame::new(FIRST_LAUNCH_FRAME, 1)).unwrap();
        assert!(tile_saved_views(&mut layout, &mut workspaces, &[2]).is_empty());
        assert!(
            layout.is_shown(&agent_2),
            "with the chat hidden it goes beside what is shown"
        );
    }

    #[test]
    fn defined_workspaces_open_after_main_on_a_first_launch() {
        let (mut workspaces, mut layout, _, _) = choose_startup_layout(
            None,
            None,
            Some(Loaded::Missing),
            &[],
            &OpenRules::default(),
            None,
            &[],
            &[],
        )
        .unwrap();
        let notes = add_defined_workspaces(
            &mut workspaces,
            &mut layout,
            &[("review".to_string(), template(ModuleId::agent(), ModuleId::editor()))],
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(workspaces.order(), ["main", "review"]);
        assert_eq!(workspaces.current(), "main");
        let review = workspaces.stored("review").unwrap();
        assert_eq!(review.visible_leaves(), [ModuleId::agent(), ModuleId::editor()]);
        assert!(review.contains(&ModuleId::terminal()) && !review.is_shown(&ModuleId::terminal()));
        assert!(notes.iter().all(|n| n.starts_with("workspace 'review': ")), "{notes:?}");
    }

    #[test]
    fn a_saved_workspace_wins_over_its_template_and_a_new_one_joins_at_the_end() {
        let main = first_launch_layout(&[]).unwrap();
        let mut study = first_launch_layout(&[]).unwrap();
        study.set_focus(&ModuleId::agent()).unwrap();
        let saved = Workspaces::from_parts(
            vec!["main".into(), "study".into()],
            std::collections::BTreeMap::from([("study".to_string(), study.clone())]),
            "main".into(),
            Default::default(),
        )
        .unwrap();
        let loaded = Loaded::Restored(persist::Opened {
            workspaces: saved,
            current: main,
            notes: vec![],
            keep_copy: None,
        });
        let (mut workspaces, mut layout, _, _) =
            choose_startup_layout(None, None, Some(loaded), &[], &OpenRules::default(), None, &[], &[]).unwrap();
        let notes = add_defined_workspaces(
            &mut workspaces,
            &mut layout,
            &[
                ("study".to_string(), template(ModuleId::agent(), ModuleId::editor())),
                ("review".to_string(), template(ModuleId::editor(), ModuleId::agent())),
            ],
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(workspaces.order(), ["main", "study", "review"]);
        assert_eq!(
            workspaces.stored("study").unwrap(),
            &study,
            "the saved study, not its template"
        );
        assert!(
            notes.contains(&"workspace 'study': the saved layout wins over eitri.layout.workspace".to_string()),
            "{notes:?}"
        );
    }

    /// A saved file whose `main` could not be read opens with `main` back, first and as the default
    /// layout, and the workspace that was current stays current; one that had `main` current opens
    /// the first readable workspace, as the file's reader chose.
    #[test]
    fn a_saved_file_without_main_gets_main_back_first() {
        let mut study = first_launch_layout(&[]).unwrap();
        study.set_focus(&ModuleId::agent()).unwrap();
        let loaded = || {
            Loaded::Restored(persist::Opened {
                workspaces: Workspaces::from_parts(
                    vec!["study".into()],
                    Default::default(),
                    "study".into(),
                    Default::default(),
                )
                .unwrap(),
                current: study.clone(),
                notes: vec![],
                keep_copy: Some("the file".to_string()),
            })
        };
        let (workspaces, layout, notes, keep_copy) =
            choose_startup_layout(None, None, Some(loaded()), &[], &OpenRules::default(), None, &[], &[]).unwrap();
        assert_eq!(workspaces.order(), ["main", "study"]);
        assert_eq!(workspaces.current(), "study");
        assert_eq!(layout, study);
        let (_, first_launch, _, _) = choose_startup_layout(
            None,
            None,
            Some(Loaded::Missing),
            &[],
            &OpenRules::default(),
            None,
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(workspaces.stored("main"), Some(&first_launch));
        assert!(
            notes.contains(&"workspace 'main' could not be read; it is the default layout again".to_string()),
            "{notes:?}"
        );
        assert_eq!(keep_copy.as_deref(), Some("the file"));
        assert!(crate::layout::export_lua(&layout, Some(&workspaces)).contains("eitri.layout.default("));

        // A file whose current workspace was the unreadable `main`: decode made the first readable
        // workspace current, and that holds.
        let text = persist::encode_v2(
            &Workspaces::from_parts(
                vec!["main".into(), "study".into()],
                std::collections::BTreeMap::from([("study".to_string(), study.clone())]),
                "main".into(),
                Default::default(),
            )
            .unwrap(),
            &first_launch,
            root(),
        );
        let mut file: serde_json::Value = serde_json::from_str(&text).unwrap();
        file["workspaces"][0]["tiled"]["children"][0]["share"] = serde_json::json!(0.99);
        let opened = persist::decode_v2(&file.to_string(), root(), &[]).unwrap();
        assert_eq!(opened.workspaces.order(), ["study"]);
        let (workspaces, layout, notes, keep_copy) = choose_startup_layout(
            None,
            None,
            Some(Loaded::Restored(opened)),
            &[],
            &OpenRules::default(),
            None,
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(workspaces.order(), ["main", "study"]);
        assert_eq!(workspaces.current(), "study");
        assert_eq!(layout, study);
        assert_eq!(workspaces.stored("main"), Some(&first_launch));
        assert!(
            notes.contains(&"workspace 'main' was current and cannot be used; 'study' is current".to_string()),
            "{notes:?}"
        );
        assert!(
            notes.contains(&"workspace 'main' could not be read; it is the default layout again".to_string()),
            "{notes:?}"
        );
        assert!(keep_copy.is_some(), "the bytes read are kept");
    }

    /// With `init.lua`'s default, the `main` put back is that default.
    #[test]
    fn main_put_back_is_the_lua_default_when_there_is_one() {
        let tree = LuaTree {
            tiled: Node::split(
                Axis::Column,
                0.5,
                Node::Leaf(ModuleId::agent()),
                Node::Leaf(ModuleId::editor()),
            ),
            floats: Vec::new(),
        };
        let study = first_launch_layout(&[]).unwrap();
        let loaded = Loaded::Restored(persist::Opened {
            workspaces: Workspaces::from_parts(
                vec!["study".into()],
                Default::default(),
                "study".into(),
                Default::default(),
            )
            .unwrap(),
            current: study,
            notes: vec![],
            keep_copy: None,
        });
        let (workspaces, _, notes, _) = choose_startup_layout(
            Some(&tree),
            None,
            Some(loaded),
            &[],
            &OpenRules::default(),
            None,
            &[],
            &[],
        )
        .unwrap();
        let (_, lua_default, _, _) = choose_startup_layout(
            Some(&tree),
            None,
            Some(Loaded::Missing),
            &[],
            &OpenRules::default(),
            None,
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(workspaces.order(), ["main", "study"]);
        assert_eq!(workspaces.stored("main"), Some(&lua_default));
        assert!(
            !notes
                .iter()
                .any(|n| n.starts_with("eitri.layout.default shapes a project with no saved layout")),
            "the default shaped the main put back: {notes:?}"
        );
    }

    #[test]
    fn a_template_naming_an_instance_gives_it_to_every_workspace() {
        let t2 = ModuleId::parse("terminal#2").unwrap();
        let mut review = template(ModuleId::editor(), ModuleId::agent());
        review.floats.push(Float {
            id: t2.clone(),
            rect: FloatRect {
                cx: 0.5,
                cy: 0.5,
                w: 600,
                h: 300,
            },
            scratch: true,
        });
        let (mut workspaces, mut layout, _, _) = choose_startup_layout(
            None,
            None,
            Some(Loaded::Missing),
            &[],
            &OpenRules::default(),
            None,
            &[],
            &[],
        )
        .unwrap();
        add_defined_workspaces(
            &mut workspaces,
            &mut layout,
            &[("review".to_string(), review)],
            &[],
            &[],
        )
        .unwrap();
        assert!(workspaces.stored("review").unwrap().is_floating(&t2));
        assert!(layout.contains(&t2) && layout.hidden().contains(&t2) && !layout.is_floating(&t2));
    }

    #[test]
    fn a_template_without_the_chat_is_an_error_naming_the_call() {
        let (mut workspaces, mut layout, _, _) = choose_startup_layout(
            None,
            None,
            Some(Loaded::Missing),
            &[],
            &OpenRules::default(),
            None,
            &[],
            &[],
        )
        .unwrap();
        let err = add_defined_workspaces(
            &mut workspaces,
            &mut layout,
            &[("x".to_string(), template(ModuleId::editor(), ModuleId::terminal()))],
            &[],
            &[],
        )
        .unwrap_err();
        assert!(
            err.starts_with("eitri.layout.workspace('x'): ") && err.contains("'agent'"),
            "{err}"
        );
    }

    use super::*;
    use crate::layout::{Axis, Direction, Frame, ModuleId, Placement, Size};
    use std::cell::RefCell;
    use std::rc::Rc;

    fn restored(layout: Layout) -> Loaded {
        Loaded::Restored(persist::Opened {
            workspaces: Workspaces::single(),
            current: layout,
            notes: vec!["a note".to_string()],
            keep_copy: None,
        })
    }

    /// A project reopens as it was left, whatever `init.lua`'s default says -- and the log says the
    /// default was not what shaped it, since that is the one surprise here.
    #[test]
    fn a_usable_file_wins_and_says_the_default_did_not_apply() {
        let mut left = first_launch_layout(&[]).unwrap();
        left.show(&ModuleId::terminal()).unwrap();
        let tree = Node::split(
            Axis::Column,
            0.5,
            Node::Leaf(ModuleId::agent()),
            Node::Leaf(ModuleId::editor()),
        );
        let (_, layout, notes, _) = choose_startup_layout(
            Some(&LuaTree {
                tiled: tree.clone(),
                floats: Vec::new(),
            }),
            None,
            Some(restored(left.clone())),
            &[],
            &OpenRules::default(),
            None,
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(layout, left);
        assert_eq!(
            notes,
            [
                "a note",
                "reopened as it was left",
                "eitri.layout.default shapes a project with no saved layout; this one has one"
            ]
        );
    }

    /// No file, or one that cannot be used: the Lua default, reconciled -- the terminal it did not
    /// name is placed hidden, as a first launch places it.
    #[test]
    fn without_a_usable_file_the_lua_default_shapes_the_window() {
        let tree = Node::split(
            Axis::Column,
            0.5,
            Node::Leaf(ModuleId::agent()),
            Node::Leaf(ModuleId::editor()),
        );
        for loaded in [None, Some(Loaded::Missing), Some(Loaded::Unusable("corrupt".into()))] {
            let unusable = matches!(loaded, Some(Loaded::Unusable(_)));
            let (_, layout, notes, _) = choose_startup_layout(
                Some(&LuaTree {
                    tiled: tree.clone(),
                    floats: Vec::new(),
                }),
                None,
                loaded,
                &[],
                &OpenRules::default(),
                None,
                &[],
                &[],
            )
            .unwrap();
            assert_eq!(layout.visible_leaves(), [ModuleId::agent(), ModuleId::editor()]);
            assert!(layout.contains(&ModuleId::terminal()) && !layout.is_shown(&ModuleId::terminal()));
            assert_eq!(notes.iter().any(|n| n.contains("corrupt")), unusable, "{notes:?}");
        }
    }

    /// With no saved file or Lua default, use the built-in initial layout.
    #[test]
    fn with_neither_the_window_opens_as_it_always_did() {
        let side = [ModuleDecl {
            id: ModuleId::lua("side"),
            placement: Placement::RightOfRoot,
        }];
        let (_, layout, notes, _) = choose_startup_layout(
            None,
            None,
            Some(Loaded::Missing),
            &side,
            &OpenRules::default(),
            None,
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(layout, first_launch_layout(&side).unwrap());
        assert!(notes.is_empty());
    }

    /// A layout that reopens with the editor hidden -- `prefix x` in the editor, then a relaunch --
    /// says what waits for it: nvim starts on the editor's first frame,
    /// and the window's colours follow nvim. A layout that shows the editor says nothing of the kind.
    #[test]
    fn a_layout_that_hides_the_editor_says_what_waits_for_it() {
        let mut left = first_launch_layout(&[]).unwrap();
        left.set_focus(&ModuleId::agent()).unwrap();
        crate::layout::hide(
            &mut left,
            &ModuleId::editor(),
            &crate::layout::Frame::new(crate::layout::Size { w: 1280, h: 721 }, 1),
        )
        .unwrap();
        let (_, layout, notes, _) = choose_startup_layout(
            None,
            None,
            Some(restored(left.clone())),
            &[],
            &OpenRules::default(),
            Some("Ctrl+b e"),
            &[],
            &[],
        )
        .unwrap();
        assert!(!layout.is_shown(&ModuleId::editor()));
        assert_eq!(
            notes.last().map(String::as_str),
            Some(
                "the editor is hidden in this layout: nvim, and the window's colours that follow it, wait until \
                 it is shown (Ctrl+b e)"
            )
        );
        // With nothing bound to show the editor, the note names no key rather than a wrong one.
        let (_, _, notes, _) = choose_startup_layout(
            None,
            None,
            Some(restored(left)),
            &[],
            &OpenRules::default(),
            None,
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(
            notes.last().map(String::as_str),
            Some(
                "the editor is hidden in this layout: nvim, and the window's colours that follow it, wait until \
                 it is shown"
            )
        );
        let (_, _, notes, _) = choose_startup_layout(
            None,
            None,
            Some(Loaded::Missing),
            &[],
            &OpenRules::default(),
            None,
            &[],
            &[],
        )
        .unwrap();
        assert!(notes.iter().all(|n| !n.contains("editor is hidden")), "{notes:?}");
    }

    const ROOT: &str = "/home/user/project";

    fn root() -> &'static Path {
        Path::new(ROOT)
    }

    fn saver_for(dir: &Path, layout: &Rc<RefCell<Layout>>) -> Rc<WindowSaver> {
        WindowSaver::new(
            Some(dir.to_path_buf()),
            root(),
            layout.clone(),
            one_workspace(),
            Vec::new(),
            None,
        )
    }

    fn one_workspace() -> Rc<RefCell<Workspaces>> {
        Rc::new(RefCell::new(Workspaces::single()))
    }

    /// A saver with the window's layout and workspaces beside it, called as the controller calls it: the
    /// writer's finished saves taken before each call, and again after the close's save.
    struct WindowSaver {
        layout: Rc<RefCell<Layout>>,
        workspaces: Rc<RefCell<Workspaces>>,
        saver: RefCell<Saver>,
    }

    impl WindowSaver {
        fn new(
            dir: Option<PathBuf>,
            project_root: &Path,
            layout: Rc<RefCell<Layout>>,
            workspaces: Rc<RefCell<Workspaces>>,
            lua: Vec<ModuleDecl>,
            keep_copy: Option<String>,
        ) -> Rc<WindowSaver> {
            WindowSaver::with_writer(
                dir,
                project_root,
                layout,
                workspaces,
                lua,
                keep_copy,
                LayoutWriter::spawn(Log::default()),
            )
        }

        fn with_writer(
            dir: Option<PathBuf>,
            project_root: &Path,
            layout: Rc<RefCell<Layout>>,
            workspaces: Rc<RefCell<Workspaces>>,
            lua: Vec<ModuleDecl>,
            keep_copy: Option<String>,
            writer: LayoutWriter,
        ) -> Rc<WindowSaver> {
            let saver = Saver::with_writer(
                dir,
                project_root,
                lua,
                keep_copy,
                &layout.borrow(),
                &workspaces.borrow(),
                writer,
            );
            Rc::new(WindowSaver {
                layout,
                workspaces,
                saver: RefCell::new(saver),
            })
        }

        fn collect(&self) {
            self.saver.borrow_mut().collect(Some(&mut self.workspaces.borrow_mut()));
        }

        fn save_now(&self) {
            self.collect();
            self.saver
                .borrow_mut()
                .save_now(&self.layout.borrow(), &self.workspaces.borrow());
            self.collect();
        }

        fn settled(&self) {
            self.saver.borrow_mut().settled(self.layout.borrow().root());
        }

        fn set_deferred(&self, deferred: Vec<ModuleId>) {
            self.saver.borrow_mut().set_deferred(deferred);
        }

        fn submit_if_changed(&self) -> bool {
            self.collect();
            self.saver
                .borrow_mut()
                .submit_if_changed(&self.layout.borrow(), &self.workspaces.borrow())
        }

        fn arrangement_changed(&self) -> bool {
            let now = Arrangement::of(&self.workspaces.borrow(), &self.layout.borrow());
            self.saver.borrow().differs(&now)
        }

        fn flush(&self, bound: Duration) -> bool {
            self.saver.borrow().writer.flush(bound)
        }

        fn rebase(&self) {
            self.saver
                .borrow_mut()
                .rebase(&self.layout.borrow(), &self.workspaces.borrow());
        }

        fn base(&self) -> Arrangement {
            self.saver.borrow().base.clone()
        }

        fn holds(&self) -> bool {
            self.saver
                .borrow()
                .holds(&self.layout.borrow(), &self.workspaces.borrow())
        }

        fn shared(&self) -> Arc<WriterShared> {
            self.saver.borrow().writer.shared.clone()
        }
    }

    /// A writer that waits at `gate` before each job until the test opens it.
    fn gated_writer(gate: &Arc<WriteGate>) -> LayoutWriter {
        LayoutWriter::start(WriterShared {
            state: Default::default(),
            changed: Default::default(),
            waker: Mutex::new(None),
            log: Log::default(),
            written: Default::default(),
            gate: Some(gate.clone()),
        })
    }

    /// A window opened on this project's file, as the shell opens one.
    fn opened(dir: &Path, lua: &[ModuleDecl]) -> Rc<RefCell<Layout>> {
        let Loaded::Restored(r) = persist::load(dir, root(), lua) else {
            panic!("a good file")
        };
        Rc::new(RefCell::new(r.current))
    }

    fn file_text(dir: &Path) -> String {
        std::fs::read_to_string(dir.join(persist::file_name_v2(root()))).unwrap()
    }

    /// What the file holds after a write of `layout`, byte for byte: this window's whole layout.
    fn written_by(layout: &Rc<RefCell<Layout>>) -> String {
        persist::encode_v2(&Workspaces::single(), &layout.borrow(), root())
    }

    fn frame() -> Frame<'static> {
        Frame::new(Size { w: 1280, h: 721 }, 1)
    }

    /// A launch is not a change: the saver writes nothing until the
    /// arrangement changes, so a file `load` could not use survives a window opened and closed
    /// untouched -- a click into the chat included -- and the first write that does replace it sets
    /// it aside whole, as `.v2.json.unusable`.
    #[test]
    fn nothing_is_written_until_the_layout_changes_and_an_unusable_file_is_kept() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver");
        let path = dir.join(persist::file_name_v2(root()));
        crate::test_scratch_dir::write_private(&path, "{ a typo").unwrap();
        let layout = Rc::new(RefCell::new(first_launch_layout(&[]).unwrap()));
        let saver = saver_for(&dir, &layout);
        layout.borrow_mut().set_focus(&ModuleId::agent()).unwrap();
        saver.save_now();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ a typo",
            "opened, clicked into the chat, closed: untouched"
        );

        layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
        saver.save_now();
        let aside = dir.join(format!("{}.unusable", persist::file_name_v2(root())));
        assert_eq!(std::fs::read_to_string(&aside).unwrap(), "{ a typo");
        assert!(matches!(persist::load(&dir, root(), &[]), Loaded::Restored(_)));

        crate::test_scratch_dir::write_private(&path, "written by someone else").unwrap();
        saver.save_now();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "written by someone else",
            "nothing changed since the last write, so nothing is written"
        );
    }

    /// Moving the keys is not a change: a click, and a zoom
    /// and its end, write nothing in a project with no file, and nothing in one with a file, at
    /// close included. The keys go into the file with the next arrangement change, so a project
    /// reopens with them where they were when that change was written -- here the terminal, not
    /// nvim, where they were at close. (Where they are when the debounce fires, not when the change
    /// was made: `the_debounce_writes_an_arrangement_change_and_a_click_arms_nothing`.)
    #[test]
    fn moving_the_keys_writes_nothing_with_or_without_a_file() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-keys");
        let path = dir.join(persist::file_name_v2(root()));
        let layout = Rc::new(RefCell::new(first_launch_layout(&[]).unwrap()));
        let saver = saver_for(&dir, &layout);
        layout.borrow_mut().set_focus(&ModuleId::agent()).unwrap();
        saver.save_now();
        layout.borrow_mut().toggle_zoom(&ModuleId::agent());
        saver.save_now();
        layout.borrow_mut().toggle_zoom(&ModuleId::agent());
        saver.save_now();
        assert!(!path.exists(), "a click and a zoom: no file");

        layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
        layout.borrow_mut().set_focus(&ModuleId::terminal()).unwrap();
        saver.save_now();
        let at_the_change = written_by(&layout);
        assert_eq!(
            file_text(&dir),
            at_the_change,
            "`Ctrl+a t`: written, the keys in the terminal"
        );
        layout.borrow_mut().set_focus(&ModuleId::editor()).unwrap();
        saver.save_now();
        saver.save_now();
        assert_eq!(
            file_text(&dir),
            at_the_change,
            "a click into nvim, then close: nothing written"
        );

        let reopened = opened(&dir, &[]);
        assert_eq!(
            reopened.borrow().focus(),
            &ModuleId::terminal(),
            "the keys where they were when the last arrangement change was written, not at close"
        );
        let saver = saver_for(&dir, &reopened);
        reopened.borrow_mut().set_focus(&ModuleId::agent()).unwrap();
        saver.save_now();
        assert_eq!(
            file_text(&dir),
            at_the_change,
            "a restored file does not follow a click either"
        );
    }

    /// An arrangement change writes this window's whole layout, with the keys where they are at
    /// that moment -- a click made before it included.
    #[test]
    fn an_arrangement_change_writes_this_windows_layout_with_the_keys_where_they_are() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-arrangement");
        persist::save(&dir, root(), &Workspaces::single(), &first_launch_layout(&[]).unwrap()).unwrap();
        let layout = opened(&dir, &[]);
        let saver = saver_for(&dir, &layout);
        let before = file_text(&dir);
        layout.borrow_mut().set_focus(&ModuleId::agent()).unwrap();
        saver.save_now();
        assert_eq!(file_text(&dir), before, "the click alone");
        layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
        saver.save_now();
        assert_eq!(file_text(&dir), written_by(&layout));
        let reopened = opened(&dir, &[]);
        assert_eq!(*reopened.borrow().root(), *layout.borrow().root());
        assert!(reopened.borrow().is_shown(&ModuleId::terminal()));
        assert_eq!(reopened.borrow().focus(), &ModuleId::agent(), "the click rode along");
    }

    /// Two windows on one project (`NON_UNIQUE`): each arrangement change writes that window's whole
    /// layout, and the last write wins -- here the later change, since neither window's wait is
    /// extended; a click in the other window writes nothing, before or after,
    /// so it never puts the arrangement it opened with back over the other's, without either window reading
    /// the other's file.
    #[test]
    fn two_windows_the_later_arrangement_change_wins_and_a_click_writes_nothing() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-two-arrangements");
        persist::save(&dir, root(), &Workspaces::single(), &first_launch_layout(&[]).unwrap()).unwrap();
        let (first, second) = (opened(&dir, &[]), opened(&dir, &[]));
        let first_saver = saver_for(&dir, &first);
        let second_saver = saver_for(&dir, &second);

        first.borrow_mut().show(&ModuleId::terminal()).unwrap();
        crate::layout::swap(&mut first.borrow_mut(), &ModuleId::editor(), Direction::Right, &frame()).unwrap();
        first_saver.save_now();
        let firsts = written_by(&first);
        assert_eq!(file_text(&dir), firsts);
        second.borrow_mut().set_focus(&ModuleId::agent()).unwrap();
        second_saver.save_now();
        assert_eq!(file_text(&dir), firsts, "the second window's click: nothing written");

        crate::layout::hide(&mut second.borrow_mut(), &ModuleId::agent(), &frame()).unwrap();
        second_saver.save_now();
        let seconds = written_by(&second);
        assert_ne!(seconds, firsts);
        assert_eq!(file_text(&dir), seconds, "the later write wins, whole");

        first.borrow_mut().set_focus(&ModuleId::terminal()).unwrap();
        first_saver.save_now();
        first_saver.save_now();
        assert_eq!(
            file_text(&dir),
            seconds,
            "the first window's click, then close: nothing written"
        );
    }

    #[test]
    fn a_rebased_arrangement_is_no_change_also_against_a_write_still_outstanding() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-rebase");
        persist::save(&dir, root(), &Workspaces::single(), &first_launch_layout(&[]).unwrap()).unwrap();
        let layout = opened(&dir, &[]);
        let saver = saver_for(&dir, &layout);
        let show = || layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
        let hide = || {
            crate::layout::hide(&mut layout.borrow_mut(), &ModuleId::terminal(), &frame()).unwrap();
        };

        // Nothing outstanding: the measure moves to the arrangement as it is now.
        show();
        assert!(saver.arrangement_changed());
        saver.rebase();
        assert!(!saver.arrangement_changed(), "the rebased arrangement is the measure");
        hide();
        assert!(saver.arrangement_changed(), "a change after the rebase is one");

        // A write handed over before the rebase: the measure is that write's arrangement no longer.
        assert!(saver.submit_if_changed());
        show();
        assert!(saver.arrangement_changed());
        saver.rebase();
        assert!(!saver.arrangement_changed());
        hide();
        assert!(saver.arrangement_changed());
    }

    /// A rebase moves what the next change is measured from, not what the file is known to hold: a write
    /// outstanding at the rebase that then fails leaves its change unwritten, so the next save writes and
    /// `holds` stays false until one did. The write is failed by a directory where the `.tmp` goes.
    #[test]
    fn a_write_outstanding_at_a_rebase_that_fails_is_still_unwritten() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-rebase-fails");
        let path = dir.join(persist::file_name_v2(root()));
        std::fs::create_dir_all(persist::temporary(&path).join("in the way")).unwrap();
        let layout = Rc::new(RefCell::new(first_launch_layout(&[]).unwrap()));
        let opened_with = Arrangement::of(&Workspaces::single(), &layout.borrow());
        let saver = saver_for(&dir, &layout);
        layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
        assert!(saver.submit_if_changed());
        crate::layout::hide(&mut layout.borrow_mut(), &ModuleId::agent(), &frame()).unwrap();
        saver.rebase();
        assert!(!saver.arrangement_changed(), "the rebased arrangement is the measure");

        assert!(saver.flush(Duration::from_secs(10)));
        saver.collect();
        assert!(!path.exists(), "the write failed");
        assert_eq!(saver.base(), opened_with, "a failed write does not move the base");
        assert!(saver.arrangement_changed(), "so the change is still unwritten");
        assert!(!saver.holds(), "and the file is not taken to hold it");

        std::fs::remove_dir_all(persist::temporary(&path)).unwrap();
        saver.save_now();
        assert_eq!(file_text(&dir), written_by(&layout));
        assert!(saver.holds(), "a later write that succeeds settles it");
    }

    /// The same write succeeding: the arrangement rebased over it is the base, so the launch's change that
    /// the write does not hold is not written by the close.
    #[test]
    fn a_write_outstanding_at_a_rebase_that_succeeds_leaves_the_rebased_arrangement_as_the_base() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-rebase-succeeds");
        let layout = Rc::new(RefCell::new(first_launch_layout(&[]).unwrap()));
        let saver = saver_for(&dir, &layout);
        layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
        assert!(saver.submit_if_changed());
        crate::layout::hide(&mut layout.borrow_mut(), &ModuleId::agent(), &frame()).unwrap();
        saver.rebase();
        assert!(saver.flush(Duration::from_secs(10)));
        saver.collect();
        assert_eq!(
            saver.base(),
            Arrangement::of(&Workspaces::single(), &layout.borrow()),
            "the rebased arrangement, not the one the write was given"
        );
        assert!(!saver.arrangement_changed());
        assert!(saver.holds());
    }

    /// A module this window does not have -- here a Lua panel only the other window registered --
    /// stays in the file through a click here, and leaves it with this window's next arrangement
    /// change: what is written is this window's layout (the accepted cost in this module's doc).
    #[test]
    fn a_module_this_window_lacks_leaves_the_file_only_with_its_arrangement_change() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-foreign-module");
        persist::save(&dir, root(), &Workspaces::single(), &first_launch_layout(&[]).unwrap()).unwrap();
        let lua = vec![ModuleDecl {
            id: ModuleId::lua("notes"),
            placement: Placement::RightOfRoot,
        }];
        let (first, second) = (opened(&dir, &lua), opened(&dir, &[]));
        let first_saver = WindowSaver::new(
            Some(dir.clone()),
            root(),
            first.clone(),
            one_workspace(),
            lua.clone(),
            None,
        );
        let second_saver = saver_for(&dir, &second);
        first.borrow_mut().show(&ModuleId::terminal()).unwrap();
        first_saver.save_now();
        let firsts = file_text(&dir);
        assert!(firsts.contains("lua:notes"), "{firsts}");

        second.borrow_mut().set_focus(&ModuleId::agent()).unwrap();
        second_saver.save_now();
        assert_eq!(file_text(&dir), firsts, "a click: the panel stays in the file");

        second.borrow_mut().show(&ModuleId::terminal()).unwrap();
        second_saver.save_now();
        assert_eq!(file_text(&dir), written_by(&second));
        assert!(
            !file_text(&dir).contains("lua:notes"),
            "the second window's layout has no such panel"
        );
    }

    /// A pinned row getting its length on the first frame it is shown is not a change either
    /// (the grid's settle, `HostEvent::Settled`): a Lua default whose bottom terminal is on screen from the
    /// start, then a zoom and its end, writes nothing. Moving that row afterwards is a change.
    #[test]
    fn a_length_settled_on_screen_is_not_a_change_and_moving_it_is() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-settle");
        let path = dir.join(persist::file_name_v2(root()));
        let (_, startup, _, _) = choose_startup_layout(
            Some(&LuaTree {
                tiled: bottom_row_default(),
                floats: Vec::new(),
            }),
            None,
            Some(Loaded::Missing),
            &[],
            &OpenRules::default(),
            None,
            &[],
            &[],
        )
        .unwrap();
        let layout = Rc::new(RefCell::new(startup));
        let saver = saver_for(&dir, &layout);
        assert!(crate::layout::settle_pins(&mut layout.borrow_mut(), &frame()));
        saver.settled();
        layout.borrow_mut().toggle_zoom(&ModuleId::editor());
        saver.save_now();
        layout.borrow_mut().toggle_zoom(&ModuleId::editor());
        saver.save_now();
        assert!(!path.exists(), "the first frame and a zoom: nothing to keep");

        assert!(crate::layout::resize(
            &mut layout.borrow_mut(),
            &ModuleId::editor(),
            Direction::Down,
            5,
            &frame()
        ));
        saver.save_now();
        assert!(path.exists(), "Ctrl+a j moved the bottom row");
    }

    /// `eitri.layout.default{ 'column', {'row', {'editor'}, {'agent'}, share = 0.7}, {'terminal'} }`:
    /// a bottom row pinned with no length yet.
    fn bottom_row_default() -> Node {
        Node::split(
            Axis::Column,
            0.7,
            Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::editor()),
                Node::Leaf(ModuleId::agent()),
            ),
            Node::Leaf(ModuleId::terminal()),
        )
    }

    /// The same for a file whose bottom row is pinned but has no length yet -- written by a window
    /// where it never settled: the row settling here, then a click, writes nothing, so a project
    /// opened and closed untouched keeps its file byte for byte. The next arrangement change writes
    /// this window's layout, with the row's length.
    #[test]
    fn a_length_settled_on_a_restored_file_is_not_a_change_either() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-settled-file");
        let (_, unsettled, _, _) = choose_startup_layout(
            Some(&LuaTree {
                tiled: bottom_row_default(),
                floats: Vec::new(),
            }),
            None,
            Some(Loaded::Missing),
            &[],
            &OpenRules::default(),
            None,
            &[],
            &[],
        )
        .unwrap();
        persist::save(&dir, root(), &Workspaces::single(), &unsettled).unwrap();
        let before = file_text(&dir);
        assert!(before.contains("\"px\": null"), "{before}");
        let layout = opened(&dir, &[]);
        let saver = saver_for(&dir, &layout);
        assert!(crate::layout::settle_pins(&mut layout.borrow_mut(), &frame()));
        saver.settled();
        layout.borrow_mut().set_focus(&ModuleId::terminal()).unwrap();
        saver.save_now();
        assert_eq!(file_text(&dir), before, "a settled length and a click: untouched");

        assert!(crate::layout::resize(
            &mut layout.borrow_mut(),
            &ModuleId::editor(),
            Direction::Down,
            5,
            &frame()
        ));
        saver.save_now();
        assert_eq!(file_text(&dir), written_by(&layout));
        assert!(!file_text(&dir).contains("\"px\": null"), "the moved length is written");
    }

    /// Two windows on one project (`NON_UNIQUE`), both opened on a file this build could not use:
    /// the first to save sets it aside and writes a good one; the second must not then set that good
    /// file aside over the bytes the first one kept.
    #[test]
    fn a_second_window_does_not_set_aside_the_file_the_first_one_wrote() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-two-windows");
        crate::test_scratch_dir::write_private(dir.join(persist::file_name_v2(root())), "{ a typo").unwrap();
        let first = Rc::new(RefCell::new(first_launch_layout(&[]).unwrap()));
        let second = Rc::new(RefCell::new(first_launch_layout(&[]).unwrap()));
        let first_saver = saver_for(&dir, &first);
        let second_saver = saver_for(&dir, &second);
        first.borrow_mut().show(&ModuleId::terminal()).unwrap();
        first_saver.save_now();
        crate::layout::even(&mut second.borrow_mut(), Axis::Column);
        second_saver.save_now();
        let aside = dir.join(format!("{}.unusable", persist::file_name_v2(root())));
        assert_eq!(std::fs::read_to_string(&aside).unwrap(), "{ a typo");
        let Loaded::Restored(saved) = persist::load(&dir, root(), &[]) else {
            panic!("the second window's layout")
        };
        assert_eq!(saved.current.root(), second.borrow().root(), "the last to save wins");
    }

    /// A file that becomes unusable while the window is open -- here a hand-edit with a typo, made
    /// after the window opened on a good file -- is set aside at the next write, never written over
    /// -- a guard on a file unusable at startup alone would write over it. A
    /// click is not a write, so it leaves such a file where it is, as it would leave a good one.
    #[test]
    fn a_file_made_unusable_while_the_window_is_open_is_set_aside_not_overwritten() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-edited");
        let path = dir.join(persist::file_name_v2(root()));
        let aside = dir.join(format!("{}.unusable", persist::file_name_v2(root())));
        persist::save(&dir, root(), &Workspaces::single(), &first_launch_layout(&[]).unwrap()).unwrap();
        let layout = opened(&dir, &[]);
        let saver = saver_for(&dir, &layout);
        crate::test_scratch_dir::write_private(&path, "{ \"version\": 1, a typo made in nvim").unwrap();
        layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
        saver.save_now();
        assert_eq!(
            std::fs::read_to_string(&aside).unwrap(),
            "{ \"version\": 1, a typo made in nvim"
        );
        assert_eq!(file_text(&dir), written_by(&layout), "written after the edit was kept");

        // Edited into garbage again: a click leaves it alone, and the next arrangement change sets
        // it aside too.
        crate::test_scratch_dir::write_private(&path, "garbage").unwrap();
        layout.borrow_mut().set_focus(&ModuleId::agent()).unwrap();
        saver.save_now();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "garbage",
            "a click writes nothing"
        );
        assert_eq!(
            std::fs::read_to_string(&aside).unwrap(),
            "{ \"version\": 1, a typo made in nvim"
        );
        crate::layout::hide(&mut layout.borrow_mut(), &ModuleId::terminal(), &frame()).unwrap();
        saver.save_now();
        assert_eq!(std::fs::read_to_string(&aside).unwrap(), "garbage");
        assert_eq!(file_text(&dir), written_by(&layout));
    }

    /// A file deleted while the window is open -- the user resetting a project's layout -- is not
    /// written again for a click; the next arrangement change writes one.
    #[test]
    fn a_deleted_file_is_not_written_again_for_a_click() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-deleted");
        let path = dir.join(persist::file_name_v2(root()));
        persist::save(&dir, root(), &Workspaces::single(), &first_launch_layout(&[]).unwrap()).unwrap();
        let layout = opened(&dir, &[]);
        let saver = saver_for(&dir, &layout);
        std::fs::remove_file(&path).unwrap();
        layout.borrow_mut().set_focus(&ModuleId::agent()).unwrap();
        saver.save_now();
        assert!(!path.exists());
        layout.borrow_mut().set_focus(&ModuleId::editor()).unwrap();
        saver.save_now();
        assert!(!path.exists(), "and not at the next save either");
        layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
        saver.save_now();
        assert!(path.exists(), "a change of the arrangement is written");
    }

    const WIN: crate::wm::WinId = crate::wm::WinId::MAIN;

    #[test]
    fn a_writer_flush_waits_until_its_completion_wake_is_posted() {
        struct HeldWake(Arc<WriteGate>);
        impl Waker for HeldWake {
            fn wake(&self) {
                self.0.hold();
            }
        }

        let dir = agent::state_dirs::test_workspace_dir("layout-save-wake");
        let mut layout = first_launch_layout(&[]).unwrap();
        let workspaces = Workspaces::single();
        let mut saver = Saver::new(Some(dir), root(), Vec::new(), None, &layout, &workspaces);
        let wake = WriteGate::closed();
        saver.set_waker(Arc::new(HeldWake(wake.clone())));
        layout.show(&ModuleId::terminal()).unwrap();
        assert!(saver.submit_now(&layout, &workspaces));
        let reached_wake = wake.wait_held(1);
        let flushed_before_wake = saver.writer.flush(Duration::ZERO);
        // Release the worker even when an assertion below fails.
        wake.open();
        assert!(reached_wake);
        assert!(!flushed_before_wake, "the write is complete but its wake is still held");
        assert!(saver.writer.flush(Duration::from_secs(10)));
        saver.collect(None);
        assert!(saver.holds(&layout, &workspaces));
        assert_eq!(saver.writes(), 1);
    }

    #[test]
    fn a_layout_write_failure_toasts_once_until_a_write_succeeds() {
        let dir = agent::state_dirs::test_workspace_dir("layout-save-warning");
        let blocked = dir.join("blocked");
        std::fs::write(&blocked, b"not a directory").unwrap();
        let window = Driven::open(&blocked, first_launch_layout(&[]).unwrap(), Workspaces::single());
        let warnings = || {
            window.driver.query(|c| {
                c.notes(WIN)
                    .iter()
                    .filter(|text| text.starts_with("layout not saved:"))
                    .count()
            })
        };
        window.layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
        for _ in 0..2 {
            window.changed();
            window.advance(500);
            assert!(window.finish_writes());
        }
        assert_eq!(warnings(), 1);
        let toast = window.driver.query(|c| c.chrome(WIN).toast).unwrap();
        assert!(toast.text.contains("Not a directory"), "{}", toast.text);
        assert_eq!(toast.kind, crate::wm::toasts::ToastKind::Warning);
        std::fs::remove_file(&blocked).unwrap();
        window.changed();
        window.advance(500);
        assert!(window.finish_writes());
        assert_eq!(window.writes(), 1);
        assert_eq!(warnings(), 1);
        std::fs::remove_dir_all(&blocked).unwrap();
        std::fs::write(&blocked, b"blocked again").unwrap();
        crate::layout::hide(&mut window.layout.borrow_mut(), &ModuleId::terminal(), &frame()).unwrap();
        window.changed();
        window.advance(500);
        assert!(window.finish_writes());
        assert_eq!(warnings(), 2, "a successful write ends the failure streak");
    }

    /// A window opened in a controller as the shell opens one: the controller behind a driver, on a fake host
    /// and a scheduler whose clock moves only with `advance`, the saver given to the window at its opening.
    struct Driven {
        layout: Rc<RefCell<Layout>>,
        workspaces: Rc<RefCell<Workspaces>>,
        scheduler: Rc<crate::wm::fake::FakeScheduler>,
        driver: Rc<crate::wm::Driver>,
        host: Rc<crate::wm::fake::FakeHost>,
    }

    impl Driven {
        fn open(dir: &Path, layout: Layout, workspaces: Workspaces) -> Driven {
            use crate::wm::{Controller, Driver, EditorPresence, HostSink, WindowParts, WindowRole, WindowSpec};
            let host = crate::wm::fake::FakeHost::new();
            let scheduler = crate::wm::fake::FakeScheduler::new();
            let saver = Saver::new(Some(dir.to_path_buf()), root(), Vec::new(), None, &layout, &workspaces);
            let layout = Rc::new(RefCell::new(layout));
            let workspaces = Rc::new(RefCell::new(workspaces));
            let mut controller = Controller::new(scheduler.clone());
            controller.set_reads(host.clone());
            controller.open_window(
                WIN,
                WindowSpec {
                    role: WindowRole::Main,
                    presence: EditorPresence::Owned,
                    title: "main".into(),
                },
                WindowParts {
                    layout: layout.clone(),
                    workspaces: workspaces.clone(),
                    saver: Some(saver),
                },
            );
            let driver = Driver::new(controller, host.clone(), scheduler.clone());
            let sink: Rc<dyn HostSink> = driver.clone();
            host.attach(Rc::downgrade(&sink));
            scheduler.attach(Rc::downgrade(&sink));
            Driven {
                layout,
                workspaces,
                scheduler,
                driver,
                host,
            }
        }

        /// The window's own close, as the user asks for it: the controller stops the request and tears the window
        /// down from its idle. What the teardown told the pair: whether the file now holds the window's arrangement
        /// (not merely a finished writer); `None` when no teardown ran.
        fn close(&self) -> Option<bool> {
            use crate::wm::{CloseBy, CloseVerdict};
            let at = self.host.log().len();
            assert_eq!(self.driver.close_requested(WIN, CloseBy::User), CloseVerdict::Stop);
            self.scheduler.advance(Duration::ZERO);
            self.layout_written(at)
        }

        /// A termination signal's close, torn down at once, before any idle of the driver's has run.
        fn signal(&self) -> Option<bool> {
            let at = self.host.log().len();
            self.driver.post(crate::wm::HostEvent::EndSignal { name: "SIGTERM" });
            self.layout_written(at)
        }

        fn layout_written(&self, since: usize) -> Option<bool> {
            use crate::wm::{fake::Logged, CleanupOp, HostOp};
            self.host.log()[since..].iter().find_map(|logged| match logged {
                Logged::Op(HostOp::Cleanup(CleanupOp::PairOnClose { layout_written }), _) => Some(*layout_written),
                _ => None,
            })
        }

        /// The test's own write of the layout or the workspaces, laid out as a change the controller made.
        fn changed(&self) {
            self.driver.test_batch(|c| c.changed_in_test(WIN));
        }

        /// The grid's allocation and the idle that hands its settle to the controller.
        fn settled(&self) -> bool {
            let settled = self.post_settled();
            self.scheduler.advance(Duration::ZERO);
            settled
        }

        /// Allocates through the driver; its settle waits for the idle that hands it on.
        fn post_settled(&self) -> bool {
            let pending = self.layout.borrow().has_pin_to_settle();
            crate::wm::Allocator::allocate(&*self.driver, WIN, frame().size);
            pending && !self.layout.borrow().has_pin_to_settle()
        }

        fn advance(&self, ms: u64) {
            self.scheduler.advance(Duration::from_millis(ms));
        }

        /// Whether a timer other than the driver's own idle waits.
        fn save_armed(&self) -> bool {
            self.scheduler
                .armed()
                .iter()
                .any(|(token, _)| *token != crate::wm::TimerToken::DRIVER_IDLE)
        }

        /// Waits for the writer to finish what it was given, then lets the controller take the results, as the
        /// writer's wake does in the app. `false` after a hang guard of ten seconds.
        fn finish_writes(&self) -> bool {
            let finished = self.driver.query(|c| {
                c.saver(WIN)
                    .is_none_or(|saver| saver.writer.flush(Duration::from_secs(10)))
            });
            self.scheduler.advance(Duration::ZERO);
            finished
        }

        /// How many writes of the file finished.
        fn writes(&self) -> usize {
            self.driver.query(|c| c.saver(WIN).map_or(0, |saver| saver.writes()))
        }

        /// Every line the window's saver and its writer printed.
        fn lines(&self) -> Vec<String> {
            self.driver
                .query(|c| c.saver(WIN).map_or_else(Vec::new, |saver| saver.lines()))
        }
    }

    #[test]
    fn a_killed_editor_is_saved_shown_and_a_relaunch_shows_it() {
        let dir = agent::state_dirs::test_workspace_dir("editor-ended-saved-shown");
        let layout = first_launch_layout(&[]).unwrap();
        let window = Driven::open(&dir, layout, Workspaces::single());
        window.host.set_editor_facts(crate::wm::EditorFacts {
            exited: true,
            ..Default::default()
        });
        window.host.set_chat_kill_counts(
            crate::tabs::ViewId::BASE,
            crate::wm::ChatKillCounts {
                running: 1,
                ..Default::default()
            },
        );
        window
            .driver
            .post(crate::wm::HostEvent::Editor(crate::wm::EditorEvent::Exited {
                win: WIN,
                settled: None,
            }));
        let key = crate::keymap::KeySpec::parse("n").unwrap();
        window.driver.key(
            WIN,
            &crate::wm::KeyStage::Window,
            crate::wm::KeyPress {
                keycode: 42,
                press: crate::keymap::prefix::Press::Key(key),
                key: Some(key.key),
                mods: crate::wm::Mods::default(),
                super_or_hyper: false,
                super_or_meta: false,
            },
        );
        assert!(!window.layout.borrow().is_shown(&ModuleId::editor()));
        assert!(!window.layout.borrow().saved_hidden().contains(&ModuleId::editor()));
        // A real arrangement change makes the saver write the editor's saved visibility too.
        crate::wm::fake::lua(
            &window.driver,
            WIN,
            crate::lua::layout::LayoutRequest::Show(ModuleId::terminal()),
        );
        window.advance(500);
        assert!(window.finish_writes());
        assert_eq!(window.writes(), 1);
        let Loaded::Restored(saved) = persist::load(&dir, root(), &[]) else {
            panic!("saved layout");
        };
        assert!(saved.current.is_shown(&ModuleId::editor()));
        let relaunched = Driven::open(&dir, saved.current, saved.workspaces);
        assert!(relaunched.layout.borrow().is_shown(&ModuleId::editor()));
    }

    #[test]
    fn the_layout_file_never_records_a_retirement() {
        use crate::wm::{
            ChatKillCounts, EditorCall, EditorEvent, EditorFacts, EditorSpawn, HostEvent, HostOp, KeyPress, KeyStage,
            Mods, Origin,
        };
        for declined in [false, true] {
            let dir = agent::state_dirs::test_workspace_dir("editor-no-retirement");
            let layout = Layout::initial(&[]).unwrap();
            let mut workspaces = Workspaces::single();
            workspaces.create("study", layout.clone()).unwrap();
            let window = Driven::open(&dir, layout, workspaces);
            window.driver.start(
                WIN,
                crate::wm::start::StartStep::Feeds {
                    spawn: EditorSpawn {
                        env: vec![],
                        args: vec![],
                        cwd: None,
                        clean: true,
                        background: None,
                        open: vec![],
                    },
                },
            );
            window.host.set_editor_facts(EditorFacts {
                exited: true,
                ..Default::default()
            });
            if declined {
                window.host.set_chat_kill_counts(
                    crate::tabs::ViewId::BASE,
                    ChatKillCounts {
                        running: 1,
                        ..Default::default()
                    },
                );
                window.driver.post(HostEvent::Editor(EditorEvent::Exited {
                    win: WIN,
                    settled: None,
                }));
            } else {
                window.driver.run(WIN, crate::keymap::Action::ModuleKill, Origin::Lua);
            }
            let key = crate::keymap::KeySpec::parse(if declined { "n" } else { "y" }).unwrap();
            window.driver.key(
                WIN,
                &KeyStage::Window,
                KeyPress {
                    keycode: 42,
                    press: crate::keymap::prefix::Press::Key(key),
                    key: Some(key.key),
                    mods: Mods::default(),
                    super_or_hyper: false,
                    super_or_meta: false,
                },
            );
            assert!(!window.layout.borrow().is_shown(&ModuleId::editor()));
            window.scheduler.advance(Duration::ZERO);
            window
                .driver
                .post(HostEvent::Editor(EditorEvent::Released { win: WIN }));
            for restarted in [false, true] {
                if restarted {
                    crate::wm::fake::lua(
                        &window.driver,
                        WIN,
                        crate::lua::layout::LayoutRequest::Show(ModuleId::editor()),
                    );
                    assert!(window.host.log().iter().any(|logged| matches!(
                        logged,
                        crate::wm::fake::Logged::Op(HostOp::Editor(EditorCall::Restart { .. }), _)
                    )));
                }
                let text = persist::encode_v2(&window.workspaces.borrow(), &window.layout.borrow(), root());
                let saved = persist::decode_v2(&text, root(), &[]).unwrap();
                assert!(!text.contains("gone") && !text.contains("killed"));
                for (_, layout) in saved.workspaces.each(&saved.current) {
                    assert!(
                        layout.is_shown(&ModuleId::editor()),
                        "declined={declined}, restarted={restarted}"
                    );
                }
            }
        }
    }

    /// The debounce end to end, with no display: the change hook's event, the controller's timer, the close's
    /// save. A change that leaves the arrangement as it was -- a click, a zoom -- arms nothing, and nothing is
    /// written however long the window then waits. An arrangement change arms the timer, and the timer, not
    /// only the close, writes it, with the keys where they are when it fires: a click inside the debounce rides
    /// along. The timer is what keeps a change through a SIGKILL, a crash or a power loss, which the close
    /// cannot; without a test that reports changes, a change that never armed the timer, or armed it on every
    /// change, would leave every other test green.
    #[test]
    fn the_debounce_writes_an_arrangement_change_and_a_click_arms_nothing() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-debounce");
        let path = dir.join(persist::file_name_v2(root()));
        let window = Driven::open(&dir, first_launch_layout(&[]).unwrap(), Workspaces::single());
        let layout = window.layout.clone();
        let past_the_debounce = persist::SAVE_DEBOUNCE_MS + 300;

        layout.borrow_mut().set_focus(&ModuleId::agent()).unwrap();
        window.changed();
        layout.borrow_mut().toggle_zoom(&ModuleId::agent());
        window.changed();
        assert!(!window.save_armed(), "a click and a zoom arm nothing");
        window.advance(past_the_debounce);
        assert!(window.finish_writes());
        assert!(
            !path.exists(),
            "a click and a zoom: no file, however long the window waits"
        );
        layout.borrow_mut().toggle_zoom(&ModuleId::agent());
        window.changed();

        layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
        window.changed();
        assert!(window.save_armed(), "an arrangement change arms the timer");
        assert!(!path.exists(), "not written at once: after the debounce");
        layout.borrow_mut().set_focus(&ModuleId::terminal()).unwrap();
        window.advance(past_the_debounce);
        assert!(window.finish_writes());
        assert!(path.exists(), "the timer writes it, not only the close");
        assert!(!window.save_armed(), "the timer is spent");
        assert_eq!(file_text(&dir), written_by(&layout));
        assert_eq!(
            opened(&dir, &[]).borrow().focus(),
            &ModuleId::terminal(),
            "the keys where they were when the timer fired: the click inside the debounce rode along"
        );

        let written = file_text(&dir);
        layout.borrow_mut().set_focus(&ModuleId::editor()).unwrap();
        window.changed();
        assert!(!window.save_armed(), "written, so a click arms nothing again");
        window.advance(past_the_debounce);
        assert!(window.finish_writes());
        assert_eq!(file_text(&dir), written);
    }

    /// Changes at 0, 200 and 400 ms arm one timer, which waits for 500 ms after the last: nothing is written at
    /// 899 ms, one write at 900 ms. A change after that and then the close writes once more, at once, and the
    /// timer that change armed is cancelled, so it writes nothing when it would have fired.
    #[test]
    fn the_debounce_writes_500ms_after_the_last_arrangement_change_and_once_at_close() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-debounce-clock");
        let path = dir.join(persist::file_name_v2(root()));
        let window = Driven::open(&dir, first_launch_layout(&[]).unwrap(), Workspaces::single());
        let layout = window.layout.clone();
        assert_eq!(persist::SAVE_DEBOUNCE_MS, 500);

        layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
        window.changed();
        window.advance(200);
        assert!(crate::layout::resize(
            &mut layout.borrow_mut(),
            &ModuleId::editor(),
            Direction::Down,
            5,
            &frame()
        ));
        window.changed();
        window.advance(200);
        crate::layout::even(&mut layout.borrow_mut(), Axis::Row);
        window.changed();
        window.advance(499);
        assert!(window.finish_writes());
        assert_eq!(window.writes(), 0, "nothing at 899 ms");
        assert!(!path.exists());
        window.advance(1);
        assert!(window.finish_writes());
        assert_eq!(window.writes(), 1, "one write at 900 ms");
        assert_eq!(file_text(&dir), written_by(&layout));

        crate::layout::hide(&mut layout.borrow_mut(), &ModuleId::terminal(), &frame()).unwrap();
        window.changed();
        assert!(window.save_armed());
        assert_eq!(window.close(), Some(true), "the close's save finished");
        assert_eq!(window.writes(), 2, "the close writes at once");
        assert_eq!(file_text(&dir), written_by(&layout));
        assert!(!window.save_armed(), "the close cancelled the timer");
        window.advance(500);
        assert!(window.finish_writes());
        assert_eq!(window.writes(), 2, "the cancelled timer writes nothing at 1,400 ms");
    }

    /// A rename changes what the file holds without changing any layout: reported as a change, it is written
    /// once after the debounce.
    #[test]
    fn a_workspace_rename_is_a_change() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-rename");
        let path = dir.join(persist::file_name_v2(root()));
        let live = first_launch_layout(&[]).unwrap();
        let mut stored = BTreeMap::new();
        stored.insert(MAIN.to_string(), live.clone());
        let workspaces = Workspaces::from_parts(
            vec![MAIN.to_string(), "study".to_string()],
            stored,
            "study".to_string(),
            Default::default(),
        )
        .unwrap();
        let window = Driven::open(&dir, live, workspaces);
        window.workspaces.borrow_mut().rename_current("notes").unwrap();
        window.changed();
        assert!(
            window.save_armed(),
            "the live layout is as it was, and still the file would change"
        );
        window.advance(persist::SAVE_DEBOUNCE_MS);
        assert!(window.finish_writes());
        assert_eq!(window.writes(), 1);
        let Loaded::Restored(back) = persist::load(&dir, root(), &[]) else {
            panic!("the file at {}", path.display())
        };
        assert_eq!(back.workspaces.order(), ["main", "notes"]);
        window.advance(persist::SAVE_DEBOUNCE_MS * 2);
        assert!(window.finish_writes());
        assert_eq!(window.writes(), 1, "written once");
    }

    /// A debounce timer that would fire after the close's save writes nothing: the close cancelled it, so
    /// nothing is written once the state directory is gone, and nothing is said about it either. The
    /// arrangement changes once more after the close's save, as a window's teardown may change it, so a timer
    /// left armed would find something to write.
    #[test]
    fn a_debounce_timer_after_the_close_save_writes_nothing() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-after-close");
        let window = Driven::open(&dir, first_launch_layout(&[]).unwrap(), Workspaces::single());
        window.layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
        window.changed();
        assert!(window.save_armed());
        assert_eq!(window.close(), Some(true));
        assert_eq!(window.writes(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
        crate::layout::hide(&mut window.layout.borrow_mut(), &ModuleId::terminal(), &frame()).unwrap();
        window.advance(1000);
        assert!(window.finish_writes());
        assert_eq!(window.writes(), 1, "no write after the close's");
        assert!(!dir.exists(), "the state directory is not made again");
        assert_eq!(window.lines(), Vec::<String>::new(), "and no line about a failed write");
    }

    /// A close whose write fails says so: what the window holds is not in the file, so nothing that only the file
    /// is to keep may be forgotten. The next close that writes -- the next window's, a window closing once -- says
    /// the file holds it.
    #[test]
    fn a_close_whose_write_fails_does_not_say_the_file_holds_the_window() {
        let base = agent::state_dirs::test_workspace_dir("layout-saver-close-fails");
        let dir = base.join("state");
        crate::test_scratch_dir::write_private(&dir, "a file where the state directory goes").unwrap();
        let window = Driven::open(&dir, first_launch_layout(&[]).unwrap(), Workspaces::single());
        window.layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
        window.changed();
        assert_eq!(window.close(), Some(false), "the write failed");
        assert_eq!(window.writes(), 0);
        std::fs::remove_file(&dir).unwrap();
        let next = Driven::open(&dir, first_launch_layout(&[]).unwrap(), Workspaces::single());
        next.layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
        next.changed();
        assert_eq!(next.close(), Some(true), "written at the next close");
        assert_eq!(next.writes(), 1);
        assert_eq!(file_text(&dir), written_by(&next.layout));
    }

    /// A change made after the allocation settled a pin and before the driver's idle hands the settle on --
    /// the bottom row moved by a key already queued -- is still a change: the settle carries the length it
    /// gave the row, so the moved length is not taken for what the window opened with.
    #[test]
    fn a_change_between_a_settle_and_its_idle_is_written() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-settle-then-move");
        let path = dir.join(persist::file_name_v2(root()));
        let (_, startup, _, _) = choose_startup_layout(
            Some(&LuaTree {
                tiled: bottom_row_default(),
                floats: Vec::new(),
            }),
            None,
            Some(Loaded::Missing),
            &[],
            &OpenRules::default(),
            None,
            &[],
            &[],
        )
        .unwrap();
        let window = Driven::open(&dir, startup, Workspaces::single());
        assert!(window.post_settled());
        assert!(crate::layout::resize(
            &mut window.layout.borrow_mut(),
            &ModuleId::editor(),
            Direction::Down,
            5,
            &frame()
        ));
        window.changed();
        window.advance(0);
        window.advance(persist::SAVE_DEBOUNCE_MS);
        assert!(window.finish_writes());
        assert_eq!(window.writes(), 1, "the moved row is written");
        assert_eq!(file_text(&dir), written_by(&window.layout));
        assert!(path.exists());
    }

    /// A close that comes after the allocation settled a pin and before the driver's idle hands the settle on -- a
    /// termination signal right after the first frame -- writes nothing for a window nobody changed: the close's
    /// save hears the waiting settle first. Writing there would leave a file that later launches follow instead of
    /// `eitri.layout.default`.
    #[test]
    fn a_close_before_a_settles_idle_writes_nothing() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-settle-then-close");
        let path = dir.join(persist::file_name_v2(root()));
        let (_, startup, _, _) = choose_startup_layout(
            Some(&LuaTree {
                tiled: bottom_row_default(),
                floats: Vec::new(),
            }),
            None,
            Some(Loaded::Missing),
            &[],
            &OpenRules::default(),
            None,
            &[],
            &[],
        )
        .unwrap();
        let window = Driven::open(&dir, startup, Workspaces::single());
        crate::wm::Allocator::allocate(&*window.driver, WIN, crate::layout::Size { w: 1280, h: 721 });
        assert!(
            !window.layout.borrow().has_pin_to_settle(),
            "the allocation settled the pin, and its settle waits for the idle"
        );
        assert_eq!(window.signal(), Some(true), "the close's save finished");
        assert_eq!(window.writes(), 0, "the settled length is what the window opened with");
        assert!(!path.exists());
    }

    /// A layout or workspaces a shell closure holds cannot be read: a change then arms nothing, and a timer that
    /// fires then saves nothing, saying so; once they are free, the next change arms the timer again and it writes.
    /// A close while the layout is held saves nothing either, says so, and does not tell the pair the file holds
    /// the window.
    #[test]
    fn a_busy_layout_arms_nothing_and_a_busy_save_says_so() {
        const BUSY: &str = "[layout] the layout was busy; not saved this time";
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-busy");
        let window = Driven::open(&dir, first_launch_layout(&[]).unwrap(), Workspaces::single());
        {
            let mut held = window.layout.borrow_mut();
            held.show(&ModuleId::terminal()).unwrap();
            window.changed();
        }
        assert!(!window.save_armed(), "a change that could not be read arms nothing");

        window.changed();
        assert!(window.save_armed(), "the next change arms the timer");
        {
            let _held = window.workspaces.borrow_mut();
            window.advance(persist::SAVE_DEBOUNCE_MS);
        }
        assert!(!window.save_armed(), "the spent timer is not armed again");
        assert!(window.finish_writes());
        assert_eq!(window.writes(), 0, "nothing saved while the workspaces were held");
        assert_eq!(window.lines(), vec![BUSY.to_string()]);

        window.changed();
        assert!(window.save_armed());
        window.advance(persist::SAVE_DEBOUNCE_MS);
        assert!(window.finish_writes());
        assert_eq!(window.writes(), 1, "the change is written once the layout is free");
        assert_eq!(file_text(&dir), written_by(&window.layout));

        crate::layout::hide(&mut window.layout.borrow_mut(), &ModuleId::terminal(), &frame()).unwrap();
        {
            let _held = window.layout.borrow_mut();
            assert_eq!(
                window.close(),
                Some(false),
                "a layout that could not be read is not known to be in the file"
            );
        }
        assert_eq!(window.writes(), 1, "nor written at a close while the layout was held");
        assert_eq!(window.lines(), vec![BUSY.to_string(), BUSY.to_string()]);
    }

    #[test]
    fn a_settle_without_an_allocation_is_ignored() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-unallocated-settle");
        let layout = Layout::initial(&[ModuleDecl {
            id: ModuleId::terminal(),
            placement: Placement::BelowRoot,
        }])
        .unwrap();
        let window = Driven::open(&dir, layout, Workspaces::single());
        let before = window.driver.query(|c| Some(c.saver(WIN).unwrap().base.clone()));
        window.driver.post(crate::wm::HostEvent::Settled { win: WIN });
        assert_eq!(
            window.driver.query(|c| Some(c.saver(WIN).unwrap().base.clone())),
            before
        );
        assert!(!window.save_armed());
    }

    /// Opening the window and the first settle of its pinned row arm nothing: a launch is not a change.
    #[test]
    fn a_launch_is_not_a_change_for_the_controller() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-launch");
        let path = dir.join(persist::file_name_v2(root()));
        let (_, startup, _, _) = choose_startup_layout(
            Some(&LuaTree {
                tiled: bottom_row_default(),
                floats: Vec::new(),
            }),
            None,
            Some(Loaded::Missing),
            &[],
            &OpenRules::default(),
            None,
            &[],
            &[],
        )
        .unwrap();
        let window = Driven::open(&dir, startup, Workspaces::single());
        assert!(!window.save_armed(), "opening the window arms nothing");
        assert!(window.settled());
        assert!(!window.save_armed(), "the first settle arms nothing");
        window.changed();
        assert!(
            !window.save_armed(),
            "the settled length is what the window opened with"
        );
        window.advance(persist::SAVE_DEBOUNCE_MS * 2);
        assert!(window.finish_writes());
        assert_eq!(window.writes(), 0);
        assert!(!path.exists());
    }

    /// A set-aside that fails for any reason but `NotFound` writes nothing, so an unusable file is
    /// never written over even when it cannot be moved -- here because a directory sits where
    /// `.v2.json.unusable` goes, which fails the rename for root too. The change is not lost: `base`
    /// has not moved, so the next save, the close's included, tries again, and once the way is clear
    /// the file is set aside and this window's layout written.
    #[test]
    fn a_set_aside_that_fails_writes_nothing_over_the_unusable_file() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-set-aside-fails");
        let path = dir.join(persist::file_name_v2(root()));
        let aside = dir.join(format!("{}.unusable", persist::file_name_v2(root())));
        crate::test_scratch_dir::write_private(&path, "{ a typo").unwrap();
        std::fs::create_dir_all(aside.join("in the way")).unwrap();
        let layout = Rc::new(RefCell::new(first_launch_layout(&[]).unwrap()));
        let saver = saver_for(&dir, &layout);
        layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
        saver.save_now();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ a typo",
            "could not be set aside, so not written over"
        );
        assert!(!persist::temporary(&path).exists(), "not even half-written");

        std::fs::remove_dir_all(&aside).unwrap();
        saver.save_now();
        assert_eq!(std::fs::read_to_string(&aside).unwrap(), "{ a typo");
        assert_eq!(file_text(&dir), written_by(&layout), "the close tries again");
    }

    /// A write that fails does not move `base`, so the change is not taken for written: the next
    /// save, the close's included, writes it. The write is failed
    /// by a directory where this process's `.tmp` goes, which fails for root too, where a
    /// read-only directory would not.
    #[test]
    fn a_failed_write_is_tried_again() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-failed-write");
        let path = dir.join(persist::file_name_v2(root()));
        std::fs::create_dir_all(persist::temporary(&path).join("in the way")).unwrap();
        let layout = Rc::new(RefCell::new(first_launch_layout(&[]).unwrap()));
        let saver = saver_for(&dir, &layout);
        layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
        saver.save_now();
        assert!(!path.exists(), "the write failed");

        std::fs::remove_dir_all(persist::temporary(&path)).unwrap();
        saver.save_now();
        assert_eq!(file_text(&dir), written_by(&layout), "the close writes it");
    }

    /// A saver whose writer waits at a gate before each job until the test opens it.
    fn gated_saver(dir: &Path, layout: &Rc<RefCell<Layout>>) -> (Rc<WindowSaver>, Arc<WriteGate>) {
        let gate = WriteGate::closed();
        let writer = gated_writer(&gate);
        let saver = WindowSaver::with_writer(
            Some(dir.to_path_buf()),
            root(),
            layout.clone(),
            one_workspace(),
            Vec::new(),
            None,
            writer,
        );
        (saver, gate)
    }

    /// Opens the gate once a job is queued behind the one the writer holds, from another thread, so the
    /// close's wait can start first; says whether one was queued before its hang guard ran out.
    fn open_when_queued(saver: &WindowSaver, gate: &Arc<WriteGate>) -> std::thread::JoinHandle<bool> {
        let shared = saver.shared();
        let gate = gate.clone();
        std::thread::spawn(move || {
            let state = shared.lock();
            let (state, _) = shared
                .changed
                .wait_timeout_while(state, Duration::from_secs(10), |state| state.pending.is_none())
                .unwrap_or_else(|e| e.into_inner());
            let queued = state.pending.is_some();
            drop(state);
            gate.open();
            queued
        })
    }

    /// The debounce timer's call hands the save to the writer and returns: the file is read, decoded and
    /// written on the writer's thread, so a slow disk never holds the window.
    #[test]
    fn the_debounced_save_returns_before_the_write() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-writer-thread");
        let path = dir.join(persist::file_name_v2(root()));
        let layout = Rc::new(RefCell::new(first_launch_layout(&[]).unwrap()));
        let (saver, gate) = gated_saver(&dir, &layout);
        layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
        assert!(saver.submit_if_changed(), "an arrangement change goes to the writer");
        assert!(gate.wait_held(1), "the writer took the job");
        assert!(!path.exists(), "the timer's call returned while the write still waits");

        gate.open();
        assert!(saver.flush(Duration::from_secs(10)));
        assert_eq!(file_text(&dir), written_by(&layout));
        saver.collect();
        assert!(
            !saver.arrangement_changed(),
            "the write was taken, so nothing is left to write"
        );
    }

    /// The window closes while an earlier arrangement is still being written: the close queues the final
    /// one behind it and waits, so the file ends with the final arrangement, never overtaken by the older.
    #[test]
    fn a_close_during_a_running_write_writes_the_final_arrangement_last() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-close-while-writing");
        let layout = Rc::new(RefCell::new(first_launch_layout(&[]).unwrap()));
        let (saver, gate) = gated_saver(&dir, &layout);
        layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
        let earlier = written_by(&layout);
        assert!(saver.submit_if_changed());
        assert!(gate.wait_held(1), "the earlier write is under way");

        crate::layout::hide(&mut layout.borrow_mut(), &ModuleId::agent(), &frame()).unwrap();
        let opener = open_when_queued(&saver, &gate);
        saver.save_now();
        assert!(
            opener.join().unwrap(),
            "the final arrangement was queued behind the running write"
        );
        let last = written_by(&layout);
        assert_ne!(last, earlier);
        assert_eq!(file_text(&dir), last, "the final arrangement is the last write");
    }

    /// A write that fails leaves `base` where it was, so the change is still unwritten and the next save
    /// submits it again. The write is failed by a directory where this process's `.tmp` goes, which fails
    /// for root too, where a read-only directory would not.
    #[test]
    fn a_write_that_fails_leaves_the_base_and_the_next_change_retries() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-writer-fails");
        let path = dir.join(persist::file_name_v2(root()));
        std::fs::create_dir_all(persist::temporary(&path).join("in the way")).unwrap();
        let layout = Rc::new(RefCell::new(first_launch_layout(&[]).unwrap()));
        let opened_with = Arrangement::of(&Workspaces::single(), &layout.borrow());
        let saver = saver_for(&dir, &layout);
        layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
        assert!(saver.submit_if_changed());
        assert!(saver.flush(Duration::from_secs(10)));
        saver.collect();
        assert!(!path.exists(), "the write failed");
        assert_eq!(saver.base(), opened_with, "a failed write does not move the base");
        assert!(saver.arrangement_changed(), "so the change is still unwritten");

        std::fs::remove_dir_all(persist::temporary(&path)).unwrap();
        assert!(saver.submit_if_changed(), "the next save submits it again");
        assert!(saver.flush(Duration::from_secs(10)));
        assert_eq!(file_text(&dir), written_by(&layout));
    }

    /// Saved A; B is being written; the arrangement goes back to A and the window closes. A is measured
    /// against B, the arrangement last given to the writer, not against the A last written, so it is
    /// written after B and the file ends as the window did.
    #[test]
    fn an_undo_to_the_saved_arrangement_during_a_running_write_is_written_last() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-undo-while-writing");
        let layout = Rc::new(RefCell::new(first_launch_layout(&[]).unwrap()));
        let (saver, gate) = gated_saver(&dir, &layout);
        gate.open();
        layout.borrow_mut().show(&ModuleId::terminal()).unwrap();
        saver.save_now();
        let saved = layout.borrow().clone();
        let a = file_text(&dir);
        assert_eq!(a, written_by(&layout));

        gate.close();
        crate::layout::hide(&mut layout.borrow_mut(), &ModuleId::agent(), &frame()).unwrap();
        assert!(saver.submit_if_changed());
        assert!(gate.wait_held(2), "B is under way");
        *layout.borrow_mut() = saved;
        let opener = open_when_queued(&saver, &gate);
        saver.save_now();
        assert!(opener.join().unwrap(), "the way back to A was queued behind B");
        assert_eq!(file_text(&dir), a, "A, written after B");
    }

    /// A settle copies into `base` only a length `base` did not have (`take_settled_lengths`), so a
    /// real change of a pinned length is not taken for a settle. Here `Ctrl+a k` grows the bottom
    /// terminal, and before the debounce fires a pinned Lua bottom panel is shown -- it settles, so
    /// the grid's settle hook runs -- and hidden again. The terminal's new height is still written
    /// -- a settle that copied every length would swallow it, and it would never be written.
    #[test]
    fn a_settle_does_not_swallow_a_length_the_window_changed() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-settle-after-resize");
        let path = dir.join(persist::file_name_v2(root()));
        let logs = ModuleId::lua("logs");
        let lua = vec![ModuleDecl {
            id: logs.clone(),
            placement: Placement::BelowRoot,
        }];
        // What a launch would find: the terminal shown and settled, the Lua bottom panel hidden and
        // never on screen, so its pin has no length yet.
        let mut start = first_launch_layout(&lua).unwrap();
        start.show(&ModuleId::terminal()).unwrap();
        if start.is_shown(&logs) {
            crate::layout::hide(&mut start, &logs, &frame()).unwrap();
        }
        crate::layout::settle_pins(&mut start, &frame());
        let layout = Rc::new(RefCell::new(start));
        let saver = WindowSaver::new(Some(dir.clone()), root(), layout.clone(), one_workspace(), lua, None);

        layout.borrow_mut().set_focus(&ModuleId::terminal()).unwrap();
        assert!(crate::layout::resize(
            &mut layout.borrow_mut(),
            &ModuleId::terminal(),
            Direction::Up,
            5,
            &frame()
        ));
        layout.borrow_mut().show(&logs).unwrap();
        assert!(
            crate::layout::settle_pins(&mut layout.borrow_mut(), &frame()),
            "the panel's pin settled: the hook runs"
        );
        saver.settled();
        crate::layout::hide(&mut layout.borrow_mut(), &logs, &frame()).unwrap();
        saver.save_now();
        assert!(path.exists(), "the terminal's new height is a change, and is written");
        assert_eq!(file_text(&dir), written_by(&layout));
    }

    /// A write that finishes while a later arrangement waits behind it moves `base` to what that write
    /// held, with only the lengths that settled on screen since, never to the later arrangement: here a
    /// hidden Lua bottom panel with no length yet is written; meanwhile it is shown, settles, is grown
    /// and hidden again, and that later arrangement's write fails. The growth is still unwritten, and
    /// the next save writes it.
    #[test]
    fn a_failed_later_write_leaves_its_resize_unwritten_after_an_earlier_write_succeeds() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-later-write-fails");
        let path = dir.join(persist::file_name_v2(root()));
        let logs = ModuleId::lua("logs");
        let lua = vec![ModuleDecl {
            id: logs.clone(),
            placement: Placement::BelowRoot,
        }];
        let mut start = first_launch_layout(&lua).unwrap();
        start.show(&ModuleId::terminal()).unwrap();
        if start.is_shown(&logs) {
            crate::layout::hide(&mut start, &logs, &frame()).unwrap();
        }
        crate::layout::settle_pins(&mut start, &frame());
        let layout = Rc::new(RefCell::new(start));
        let gate = WriteGate::closed();
        let writer = gated_writer(&gate);
        let saver = WindowSaver::with_writer(
            Some(dir.clone()),
            root(),
            layout.clone(),
            one_workspace(),
            lua,
            None,
            writer,
        );

        crate::layout::hide(&mut layout.borrow_mut(), &ModuleId::agent(), &frame()).unwrap();
        assert!(saver.submit_if_changed(), "the first arrangement goes to the writer");
        assert!(gate.wait_held(1), "and is held before its write");

        layout.borrow_mut().show(&logs).unwrap();
        assert!(
            crate::layout::settle_pins(&mut layout.borrow_mut(), &frame()),
            "the panel's pin settled: the hook runs"
        );
        saver.settled();
        layout.borrow_mut().set_focus(&logs).unwrap();
        assert!(crate::layout::resize(
            &mut layout.borrow_mut(),
            &logs,
            Direction::Up,
            5,
            &frame()
        ));
        crate::layout::hide(&mut layout.borrow_mut(), &logs, &frame()).unwrap();
        assert!(saver.submit_if_changed(), "the grown panel is a change");

        gate.pass_one();
        assert!(gate.wait_held(2), "the first write ran and the second waits");
        let first = file_text(&dir);
        std::fs::create_dir_all(persist::temporary(&path).join("in the way")).unwrap();
        gate.open();
        assert!(saver.flush(Duration::from_secs(10)));
        saver.collect();
        assert_eq!(file_text(&dir), first, "the second write failed");
        assert!(
            saver.arrangement_changed(),
            "the panel's new length was never written, so it is still a change"
        );

        std::fs::remove_dir_all(persist::temporary(&path)).unwrap();
        saver.save_now();
        assert_eq!(file_text(&dir), written_by(&layout), "the next save writes it");
    }

    /// A pinned child of a container with three children, one pin settled in `now` and not in `base`.
    fn three_children(settled: Option<i32>, layout: crate::layout::ConLayout) -> Con {
        use crate::layout::{Child, Pin};
        let leaf = |name: &str| Node::Leaf(ModuleId::lua(name));
        let mut bottom = Child::new(leaf("c"), 0.2);
        bottom.pin = Some(Pin { px: settled });
        Con::new(
            layout,
            vec![Child::new(leaf("a"), 0.5), Child::new(leaf("b"), 0.3), bottom],
        )
    }

    /// The settle hook copies a pin's length into `base` where `base` had none, in a container of any
    /// size; a base whose shape differs from the layout's keeps its own pins as they are.
    #[test]
    fn take_settled_lengths_copies_a_settled_pin_into_the_base() {
        use crate::layout::ConLayout::{SplitH, SplitV};
        let pin_of = |con: &Con| con.children[2].pin.unwrap().px;

        let mut base = three_children(None, SplitV);
        take_settled_lengths(&mut base, &three_children(Some(240), SplitV));
        assert_eq!(pin_of(&base), Some(240), "the unsettled pin took the settled length");

        let mut kept = three_children(Some(100), SplitV);
        take_settled_lengths(&mut kept, &three_children(Some(240), SplitV));
        assert_eq!(
            pin_of(&kept),
            Some(100),
            "a length the base already had is not replaced"
        );

        let mut other_axis = three_children(None, SplitV);
        take_settled_lengths(&mut other_axis, &three_children(Some(240), SplitH));
        assert_eq!(
            pin_of(&other_axis),
            None,
            "a container of another orientation is left alone"
        );

        let mut other_count = three_children(None, SplitV);
        let mut shorter = three_children(Some(240), SplitV);
        shorter.children.remove(1);
        take_settled_lengths(&mut other_count, &shorter);
        assert_eq!(
            pin_of(&other_count),
            None,
            "a container with another child count is left alone"
        );

        // The same walk reaches a pin below the first level.
        let nested = |settled| {
            Con::new(
                SplitH,
                vec![
                    crate::layout::Child::new(Node::Con(three_children(settled, SplitV)), 0.5),
                    crate::layout::Child::new(Node::Leaf(ModuleId::lua("d")), 0.5),
                ],
            )
        };
        let mut base = nested(None);
        take_settled_lengths(&mut base, &nested(Some(77)));
        let Node::Con(inner) = &base.children[0].node else {
            unreachable!()
        };
        assert_eq!(pin_of(inner), Some(77));
    }

    /// The debounce's one timer: armed once, and when it fires before 500ms have passed since the
    /// last change, armed again for what is left.
    #[test]
    fn the_debounce_waits_for_what_is_left_of_500ms() {
        assert_eq!(debounce_left(Duration::ZERO), Some(Duration::from_millis(500)));
        assert_eq!(
            debounce_left(Duration::from_millis(120)),
            Some(Duration::from_millis(380))
        );
        assert_eq!(debounce_left(Duration::from_millis(500)), None);
        assert_eq!(debounce_left(Duration::from_secs(3)), None);
        assert_eq!(debounce_left(Duration::MAX), None, "no change was ever reported");
    }

    /// What arms the timer and what writes: the tree, its ratios and what is hidden; never a zoom or
    /// the keys.
    #[test]
    fn only_the_arrangement_is_a_change() {
        let layout = first_launch_layout(&[]).unwrap();
        let base = Arrangement::of(&Workspaces::single(), &layout);
        let mut keys = layout.clone();
        keys.set_focus(&ModuleId::agent()).unwrap();
        let mut zoomed = layout.clone();
        zoomed.toggle_zoom(&ModuleId::agent());
        let mut shown = layout.clone();
        shown.show(&ModuleId::terminal()).unwrap();
        let mut even = layout.clone();
        crate::layout::even(&mut even, Axis::Row);
        assert_eq!(Arrangement::of(&Workspaces::single(), &keys), base, "a click");
        assert_eq!(Arrangement::of(&Workspaces::single(), &zoomed), base, "a zoom");
        assert_ne!(Arrangement::of(&Workspaces::single(), &shown), base, "a module shown");
        assert_ne!(Arrangement::of(&Workspaces::single(), &even), base, "a ratio");

        // With two workspaces: a change in the stored one is a change, and so is which one is current.
        let two = |current: &str, study: &Layout| {
            let mut stored = std::collections::BTreeMap::new();
            stored.insert(
                if current == "main" { "study" } else { "main" }.to_string(),
                study.clone(),
            );
            Workspaces::from_parts(
                vec!["main".to_string(), "study".to_string()],
                stored,
                current.to_string(),
                Default::default(),
            )
            .unwrap()
        };
        let base = Arrangement::of(&two("main", &layout), &layout);
        assert_eq!(Arrangement::of(&two("main", &layout), &keys), base, "a click");
        assert_ne!(
            Arrangement::of(&two("main", &shown), &layout),
            base,
            "a module shown in the other workspace"
        );
        assert_ne!(
            Arrangement::of(&two("study", &layout), &layout),
            base,
            "the other workspace current"
        );
    }

    /// A Lua default naming a Lua panel that did not register is not a failure: the panel is left
    /// out and the log says so, as for a state file.
    #[test]
    fn a_lua_default_naming_a_missing_panel_opens_without_it() {
        let tree = Node::split(
            Axis::Row,
            0.5,
            Node::Leaf(ModuleId::editor()),
            Node::split(
                Axis::Column,
                0.5,
                Node::Leaf(ModuleId::agent()),
                Node::Leaf(ModuleId::lua("gone")),
            ),
        );
        let (_, layout, notes, _) = choose_startup_layout(
            Some(&LuaTree {
                tiled: tree.clone(),
                floats: Vec::new(),
            }),
            None,
            None,
            &[],
            &OpenRules::default(),
            None,
            &[],
            &[],
        )
        .unwrap();
        assert!(!layout.contains(&ModuleId::lua("gone")));
        assert_eq!(
            notes,
            [
                "eitri.layout.default: left out 'lua:gone': this window has no such module",
                "eitri.layout.default: placed 'terminal' below the editor, hidden, as a first launch does"
            ]
        );
    }

    /// A window opened on this project's file as the shell opens one: the current layout, every
    /// workspace, and a saver given what `choose_startup_layout` gives it.
    struct Window {
        layout: Rc<RefCell<Layout>>,
        workspaces: Rc<RefCell<Workspaces>>,
        saver: Rc<WindowSaver>,
    }

    impl Window {
        fn open(dir: &Path) -> Window {
            let loaded = persist::load(dir, root(), &[]);
            let (workspaces, layout, _, keep_copy) =
                choose_startup_layout(None, None, Some(loaded), &[], &OpenRules::default(), None, &[], &[]).unwrap();
            Window::with(dir, workspaces, layout, keep_copy)
        }

        fn with(dir: &Path, workspaces: Workspaces, layout: Layout, keep_copy: Option<String>) -> Window {
            let layout = Rc::new(RefCell::new(layout));
            let workspaces = Rc::new(RefCell::new(workspaces));
            let saver = WindowSaver::new(
                Some(dir.to_path_buf()),
                root(),
                layout.clone(),
                workspaces.clone(),
                Vec::new(),
                keep_copy,
            );
            Window {
                layout,
                workspaces,
                saver,
            }
        }

        /// What a write of this window puts in the file, byte for byte.
        fn written(&self) -> String {
            persist::encode_v2(&self.workspaces.borrow(), &self.layout.borrow(), root())
        }

        /// A change of the arrangement: the terminal shown, or hidden if it is shown.
        fn change(&self) {
            let mut layout = self.layout.borrow_mut();
            if layout.is_shown(&ModuleId::terminal()) {
                crate::layout::hide(&mut layout, &ModuleId::terminal(), &frame()).unwrap();
            } else {
                layout.show(&ModuleId::terminal()).unwrap();
            }
        }
    }

    fn partial(dir: &Path) -> PathBuf {
        dir.join(format!("{}.partial", persist::file_name_v2(root())))
    }

    fn two_workspaces(study: &Layout) -> Workspaces {
        let mut stored = std::collections::BTreeMap::new();
        stored.insert("study".to_string(), study.clone());
        Workspaces::from_parts(
            vec!["main".to_string(), "study".to_string()],
            stored,
            "main".to_string(),
            Default::default(),
        )
        .unwrap()
    }

    /// `main` and `study`, the second naming a module no build has: a file this build opens only in
    /// part.
    fn partly_usable_text() -> String {
        let main = first_launch_layout(&[]).unwrap();
        let text = persist::encode_v2(&two_workspaces(&main), &main, root());
        let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value["workspaces"][1]["tiled"]["children"][1]["node"]["module"],
            "agent"
        );
        value["workspaces"][1]["tiled"]["children"][1]["node"]["module"] = serde_json::json!("chat");
        serde_json::to_string_pretty(&value).unwrap()
    }

    /// A project last left by a build before workspaces opens from its version 1 file, says so, and
    /// writes only the version 2 file.
    #[test]
    fn a_migrated_file_says_so() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-migrated");
        let v1 = dir.join(persist::file_name(root()));
        let text = r#"{"version": 1, "project_root": "/home/user/project",
            "root": {"type": "split", "axis": "row", "ratio": 0.5942142,
                "first": {"type": "split", "axis": "column", "ratio": 0.6666667,
                    "pin": {"side": "second", "px": 240},
                    "first": {"type": "leaf", "module": "editor"},
                    "second": {"type": "leaf", "module": "terminal"}},
                "second": {"type": "leaf", "module": "agent"}},
            "hidden": [], "focus": "editor"}"#;
        crate::test_scratch_dir::write_private(&v1, text).unwrap();
        let (workspaces, layout, notes, keep_copy) = choose_startup_layout(
            None,
            None,
            Some(persist::load(&dir, root(), &[])),
            &[],
            &OpenRules::default(),
            None,
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(workspaces, Workspaces::single());
        assert!(layout.is_shown(&ModuleId::terminal()));
        assert_eq!(
            notes,
            ["migrated from the version 1 layout file", "reopened as it was left"]
        );
        assert_eq!(keep_copy, None);

        let window = Window::with(&dir, workspaces, layout, keep_copy);
        window.change();
        window.saver.save_now();
        assert_eq!(file_text(&dir), window.written());
        assert_eq!(
            std::fs::read_to_string(&v1).unwrap(),
            text,
            "the version 1 file is never written"
        );
    }

    /// A file this window opened only in part has the bytes it read copied aside before its first
    /// write, and then the window's own workspaces are written.
    #[test]
    fn the_saver_copies_a_partly_unusable_file_before_its_first_write() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-partial");
        let text = partly_usable_text();
        crate::test_scratch_dir::write_private(dir.join(persist::file_name_v2(root())), &text).unwrap();
        let window = Window::open(&dir);
        assert_eq!(window.workspaces.borrow().order(), ["main"]);
        window.layout.borrow_mut().set_focus(&ModuleId::agent()).unwrap();
        window.saver.save_now();
        assert!(!partial(&dir).exists(), "a click writes nothing, and copies nothing");
        assert_eq!(file_text(&dir), text);

        window.change();
        window.saver.save_now();
        assert_eq!(std::fs::read_to_string(partial(&dir)).unwrap(), text);
        assert_eq!(file_text(&dir), window.written());
    }

    /// Two windows that opened the same partly usable file: the first to write copies it aside; the
    /// second finds the file is the first one's now, not the text it opened, and copies nothing over
    /// the copy -- here a later copy, of another partly usable text the first window found and kept,
    /// which a second window copying what it opened would replace.
    #[test]
    fn a_second_window_does_not_copy_aside_the_file_the_first_one_wrote() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-partial-two");
        let text = partly_usable_text();
        crate::test_scratch_dir::write_private(dir.join(persist::file_name_v2(root())), &text).unwrap();
        let (first, second) = (Window::open(&dir), Window::open(&dir));
        first.change();
        first.saver.save_now();
        assert_eq!(std::fs::read_to_string(partial(&dir)).unwrap(), text);
        assert_eq!(file_text(&dir), first.written());

        // The file made partly unusable again, differently, and the first window writing over it:
        // the copy is of that text now.
        let later = text.replace("\"chat\"", "\"pane\"");
        assert_ne!(later, text);
        crate::test_scratch_dir::write_private(dir.join(persist::file_name_v2(root())), &later).unwrap();
        first.change();
        first.saver.save_now();
        assert_eq!(std::fs::read_to_string(partial(&dir)).unwrap(), later);
        assert_eq!(file_text(&dir), first.written());

        crate::layout::even(&mut second.layout.borrow_mut(), Axis::Column);
        second.saver.save_now();
        assert_eq!(
            std::fs::read_to_string(partial(&dir)).unwrap(),
            later,
            "the file is not the text the second window opened, so that text is not copied over the copy"
        );
        assert_eq!(file_text(&dir), second.written(), "the later writer wins, whole");
        assert_ne!(second.written(), first.written());
    }

    fn preview() -> ModuleId {
        ModuleId::lua("demo")
    }

    #[test]
    fn a_deferred_module_is_not_placed_at_a_first_launch() {
        let (_, layout, notes, _) = choose_startup_layout(
            None,
            None,
            Some(Loaded::Missing),
            &[],
            &OpenRules::default(),
            None,
            &[preview()],
            &[],
        )
        .unwrap();
        assert!(!layout.contains(&preview()), "{layout:?}");
        assert!(notes.iter().all(|n| !n.contains("lua:demo")), "{notes:?}");
    }

    #[test]
    fn a_file_or_default_without_a_deferred_module_does_not_place_it() {
        let deferred = [preview()];
        let dir = agent::state_dirs::test_workspace_dir("layout-deferred-absent");
        persist::save(&dir, root(), &Workspaces::single(), &first_launch_layout(&[]).unwrap()).unwrap();
        let loaded = persist::load_with(&dir, root(), &[], &deferred);
        assert!(matches!(loaded, Loaded::Restored(_)));
        let (_, layout, notes, _) = choose_startup_layout(
            None,
            None,
            Some(loaded),
            &[],
            &OpenRules::default(),
            None,
            &deferred,
            &[],
        )
        .unwrap();
        assert!(!layout.contains(&preview()));
        assert!(notes.iter().all(|n| !n.contains("lua:demo")), "{notes:?}");

        let default = |tiled: Node| {
            choose_startup_layout(
                Some(&LuaTree {
                    tiled,
                    floats: Vec::new(),
                }),
                None,
                Some(Loaded::Missing),
                &[],
                &OpenRules::default(),
                None,
                &deferred,
                &[],
            )
            .unwrap()
        };
        let editor_and_agent = || {
            Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::editor()),
                Node::Leaf(ModuleId::agent()),
            )
        };
        let (_, layout, notes, _) = default(editor_and_agent());
        assert!(!layout.contains(&preview()));
        assert!(notes.iter().all(|n| !n.contains("lua:demo")), "{notes:?}");

        let (_, layout, notes, _) = default(Node::split(Axis::Row, 0.6, editor_and_agent(), Node::Leaf(preview())));
        assert!(layout.contains(&preview()) && layout.is_shown(&preview()), "{notes:?}");
        assert!(notes.iter().all(|n| !n.contains("lua:demo")), "{notes:?}");
    }

    /// An `eitri.layout.workspace` template that names a deferred module keeps it where it says, shown, and the fit
    /// puts it hidden into the workspaces that lack it; without the list the template's leaf is left out.
    #[test]
    fn a_template_keeps_a_deferred_module_it_names_and_the_others_hold_it_hidden() {
        let deferred = [preview()];
        let review = || LuaTree {
            tiled: Node::split(
                Axis::Row,
                0.6,
                Node::split(
                    Axis::Row,
                    0.5,
                    Node::Leaf(ModuleId::editor()),
                    Node::Leaf(ModuleId::agent()),
                ),
                Node::Leaf(preview()),
            ),
            floats: Vec::new(),
        };
        let (mut workspaces, mut layout, _, _) = choose_startup_layout(
            None,
            None,
            Some(Loaded::Missing),
            &[],
            &OpenRules::default(),
            None,
            &deferred,
            &[],
        )
        .unwrap();
        let notes = add_defined_workspaces(
            &mut workspaces,
            &mut layout,
            &[("review".to_string(), review())],
            &[],
            &deferred,
        )
        .unwrap();
        let made = workspaces.stored("review").unwrap();
        assert!(made.is_shown(&preview()), "{made:?}");
        assert_eq!(made.visible_leaves().last(), Some(&preview()));
        assert!(
            layout.contains(&preview()) && !layout.is_shown(&preview()),
            "{layout:?}"
        );
        assert!(
            notes
                .iter()
                .any(|n| n.starts_with("workspace 'main': ") && n.contains("'lua:demo'") && n.contains("hidden")),
            "{notes:?}"
        );

        let (mut workspaces, mut layout, _, _) = choose_startup_layout(
            None,
            None,
            Some(Loaded::Missing),
            &[],
            &OpenRules::default(),
            None,
            &[],
            &[],
        )
        .unwrap();
        let notes = add_defined_workspaces(
            &mut workspaces,
            &mut layout,
            &[("review".to_string(), review())],
            &[],
            &[],
        )
        .unwrap();
        assert!(!workspaces.stored("review").unwrap().contains(&preview()));
        assert!(!layout.contains(&preview()));
        assert!(notes.iter().any(|n| n.contains("left out 'lua:demo'")), "{notes:?}");
    }

    /// A plugin that starts shown declares a whole rule: at a first launch its `size` applies, not only its place.
    #[test]
    fn a_shown_plugins_declared_size_applies_at_a_first_launch() {
        let notes_panel = ModuleId::lua("notes");
        let decls = [ModuleDecl {
            id: notes_panel.clone(),
            placement: Placement::RightOfRoot,
        }];
        let own = OpenRule {
            at: at("side"),
            size: Some(crate::layout::rules::RuleSize::Share(0.4)),
            focus: false,
            ..OpenRule::default()
        };
        let width = |own: &[(ModuleId, OpenRule)]| {
            let (_, layout, notes, _) = choose_startup_layout(
                None,
                None,
                Some(Loaded::Missing),
                &decls,
                &OpenRules::default(),
                None,
                &[],
                own,
            )
            .unwrap();
            let arranged = crate::layout::arrange(&layout, &frame());
            let rect = arranged
                .modules
                .iter()
                .find(|(id, _)| *id == notes_panel)
                .map(|(_, rect)| *rect)
                .unwrap_or_else(|| panic!("{layout:?} {notes:?}"));
            rect.w as f32 / 1280.0
        };
        let declared = width(&[(notes_panel.clone(), own.clone())]);
        assert!((declared - 0.4).abs() < 0.02, "{declared}");
        let placed_only = width(&[]);
        assert!(
            placed_only < 0.36,
            "the built-in third, not the declared share: {placed_only}"
        );

        // A user's rule for the module wins over the declared one, whole.
        let rules = rules_of(&[(
            "lua:notes",
            OpenRule {
                size: Some(crate::layout::rules::RuleSize::Share(0.25)),
                ..OpenRule::default()
            },
        )]);
        let (_, layout, _, _) = choose_startup_layout(
            None,
            None,
            Some(Loaded::Missing),
            &decls,
            &rules,
            None,
            &[],
            &[(notes_panel.clone(), own)],
        )
        .unwrap();
        let arranged = crate::layout::arrange(&layout, &frame());
        let rect = arranged.modules.iter().find(|(id, _)| *id == notes_panel).unwrap().1;
        let users = rect.w as f32 / 1280.0;
        assert!((users - 0.25).abs() < 0.02, "{users}");
    }

    /// The save path reads the file with the window's deferred modules, so a floating one there is not taken for a
    /// module this build cannot carry, and nothing is copied aside; without them the same file is copied.
    #[test]
    fn the_saver_reads_the_file_with_the_deferred_modules() {
        let deferred = [preview()];
        let with_float = || {
            let mut layout = first_launch_layout(&[]).unwrap();
            layout
                .add_float(
                    &preview(),
                    FloatRect {
                        cx: 0.5,
                        cy: 0.5,
                        w: 400,
                        h: 300,
                    },
                    false,
                )
                .unwrap();
            layout
        };
        let open = |label: &str| {
            let dir = agent::state_dirs::test_workspace_dir(label);
            persist::save(&dir, root(), &Workspaces::single(), &with_float()).unwrap();
            let (workspaces, layout, _, keep_copy) = choose_startup_layout(
                None,
                None,
                Some(persist::load_with(&dir, root(), &[], &deferred)),
                &[],
                &OpenRules::default(),
                None,
                &deferred,
                &[],
            )
            .unwrap();
            assert!(layout.is_floating(&preview()));
            assert_eq!(keep_copy, None);
            let window = Window::with(&dir, workspaces, layout, keep_copy);
            (dir, window)
        };

        let (dir, window) = open("layout-saver-deferred");
        window.saver.set_deferred(deferred.to_vec());
        window.change();
        window.saver.save_now();
        assert!(!partial(&dir).exists(), "the file is read as this window read it");
        assert_eq!(file_text(&dir), window.written());
        assert!(file_text(&dir).contains("lua:demo"));

        let (dir, window) = open("layout-saver-deferred-unset");
        window.change();
        window.saver.save_now();
        assert!(
            partial(&dir).exists(),
            "read without the deferred modules, the file holds one this build cannot carry"
        );
    }

    /// A file made partly unusable after this window opened it -- a hand edit, another build -- has its
    /// text copied aside before this window writes over it.
    #[test]
    fn a_file_made_partly_unusable_while_open_is_copied_before_the_write() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-partial-later");
        persist::save(&dir, root(), &Workspaces::single(), &first_launch_layout(&[]).unwrap()).unwrap();
        let window = Window::open(&dir);
        let text = partly_usable_text();
        crate::test_scratch_dir::write_private(dir.join(persist::file_name_v2(root())), &text).unwrap();
        window.change();
        window.saver.save_now();
        assert_eq!(std::fs::read_to_string(partial(&dir)).unwrap(), text);
        assert_eq!(file_text(&dir), window.written());
    }

    /// Keys a later build wrote into the file after this window opened it -- a workspace-level key, a
    /// top-level key -- are taken from the file when this window writes, so its write does not erase
    /// them; nothing is copied aside for them.
    #[test]
    fn unknown_keys_written_after_this_window_loaded_survive_its_save() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-kept-later");
        persist::save(&dir, root(), &Workspaces::single(), &first_launch_layout(&[]).unwrap()).unwrap();
        let window = Window::open(&dir);
        let mut value: serde_json::Value = serde_json::from_str(&file_text(&dir)).unwrap();
        let overlays = serde_json::json!([{"module": "terminal#2", "opacity": 0.8}]);
        value["workspaces"][0]["overlays"] = overlays.clone();
        value["future"] = serde_json::json!({"a": 1});
        crate::test_scratch_dir::write_private(
            dir.join(persist::file_name_v2(root())),
            serde_json::to_string_pretty(&value).unwrap(),
        )
        .unwrap();

        window.change();
        window.saver.save_now();
        let again: serde_json::Value = serde_json::from_str(&file_text(&dir)).unwrap();
        assert_eq!(again["workspaces"][0]["overlays"], overlays);
        assert_eq!(again["future"], serde_json::json!({"a": 1}));
        assert_eq!(file_text(&dir), window.written());
        assert!(!partial(&dir).exists(), "carried, so nothing is copied");
    }

    /// Two builds on one project: this window tiles `terminal#2` after it opened the file, and a later
    /// build then puts a `terminal#2` of its own in a key this build does not know. The file's tree does not hold it, so the file
    /// alone does not show the clash; this window's own layout does. Its write keeps `terminal#2` in
    /// one place, its tree, and the later build's text survives as `.v2.json.partial`.
    #[test]
    fn a_kept_key_naming_a_module_this_window_placed_since_it_opened_is_not_written_back() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-kept-placed-since");
        persist::save(&dir, root(), &Workspaces::single(), &first_launch_layout(&[]).unwrap()).unwrap();
        let window = Window::open(&dir);
        let second = ModuleId::parse("terminal#2").unwrap();
        crate::layout::place(&mut window.layout.borrow_mut(), &second, &ModuleId::editor(), Axis::Row).unwrap();

        let mut value: serde_json::Value = serde_json::from_str(&file_text(&dir)).unwrap();
        value["workspaces"][0]["overlays"] = serde_json::json!([{"module": "terminal#2", "w": 900, "h": 420}]);
        value["workspaces"][0]["overlay_memory"] = serde_json::json!({"x": 1});
        let later = serde_json::to_string_pretty(&value).unwrap();
        crate::test_scratch_dir::write_private(dir.join(persist::file_name_v2(root())), &later).unwrap();

        window.saver.save_now();
        let again: serde_json::Value = serde_json::from_str(&file_text(&dir)).unwrap();
        assert_eq!(again["workspaces"][0].get("overlays"), None, "terminal#2 is tiled here");
        assert_eq!(
            again["workspaces"][0]["overlay_memory"],
            serde_json::json!({"x": 1}),
            "a key naming no module of this window is still carried"
        );
        assert!(file_text(&dir).contains("\"terminal#2\""), "in the tree");
        assert_eq!(file_text(&dir), window.written());
        assert_eq!(std::fs::read_to_string(partial(&dir)).unwrap(), later);
        assert!(window.workspaces.borrow().reserved().is_empty());
        let Loaded::Restored(back) = persist::load(&dir, root(), &[]) else {
            panic!("the file")
        };
        assert!(back.keep_copy.is_none(), "what was written holds terminal#2 once");
    }

    #[test]
    fn the_saver_writes_every_workspace() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-every-workspace");
        let mut study = first_launch_layout(&[]).unwrap();
        crate::layout::even(&mut study, Axis::Row);
        let window = Window::with(&dir, two_workspaces(&study), first_launch_layout(&[]).unwrap(), None);
        window.change();
        window.saver.save_now();
        assert_eq!(file_text(&dir), window.written());
        let Loaded::Restored(back) = persist::load(&dir, root(), &[]) else {
            panic!("the file")
        };
        assert_eq!(back.workspaces.order(), ["main", "study"]);
        assert_eq!(back.workspaces.stored("study").unwrap().root(), study.root());
    }

    /// With several workspaces too, a move of the keys writes nothing; a change in any workspace,
    /// the one not on screen included, is written.
    #[test]
    fn a_move_of_the_keys_writes_nothing() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-keys-workspaces");
        let path = dir.join(persist::file_name_v2(root()));
        let layout = first_launch_layout(&[]).unwrap();
        let window = Window::with(&dir, two_workspaces(&layout), layout, None);
        window.layout.borrow_mut().set_focus(&ModuleId::agent()).unwrap();
        window.saver.save_now();
        assert!(!path.exists(), "a click");

        for stored in window.workspaces.borrow_mut().stored_mut() {
            stored.show(&ModuleId::terminal()).unwrap();
        }
        window.saver.save_now();
        assert_eq!(
            file_text(&dir),
            window.written(),
            "a change in the workspace not on screen"
        );
    }

    #[test]
    fn a_lua_default_with_a_float_opens_with_it() {
        use crate::layout::{Float, FloatRect};
        let tree = LuaTree {
            tiled: Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::agent()),
                Node::Leaf(ModuleId::editor()),
            ),
            floats: vec![Float {
                id: ModuleId::terminal(),
                rect: FloatRect {
                    cx: 0.5,
                    cy: 0.5,
                    w: 600,
                    h: 300,
                },
                scratch: false,
            }],
        };
        let (_, layout, notes, copy) = choose_startup_layout(
            Some(&tree),
            None,
            Some(Loaded::Missing),
            &[],
            &OpenRules::default(),
            None,
            &[],
            &[],
        )
        .unwrap();
        assert!(layout.is_floating(&ModuleId::terminal()));
        assert!(!layout.leaves().contains(&ModuleId::terminal()));
        assert!(notes.is_empty(), "{notes:?}");
        assert!(copy.is_none());
    }

    #[test]
    fn a_lua_default_whose_chat_floats_opens_with_it() {
        use crate::layout::{Float, FloatRect};
        let tree = LuaTree {
            tiled: Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::editor()),
                Node::Leaf(ModuleId::terminal()),
            ),
            floats: vec![Float {
                id: ModuleId::agent(),
                rect: FloatRect {
                    cx: 0.5,
                    cy: 0.5,
                    w: 600,
                    h: 300,
                },
                scratch: false,
            }],
        };
        let (_, layout, notes, copy) = choose_startup_layout(
            Some(&tree),
            None,
            Some(Loaded::Missing),
            &[],
            &OpenRules::default(),
            None,
            &[],
            &[],
        )
        .unwrap();
        assert!(layout.is_floating(&ModuleId::agent()));
        assert!(!layout.leaves().contains(&ModuleId::agent()));
        assert!(notes.is_empty(), "{notes:?}");
        assert!(copy.is_none());
    }

    /// `editor | agent` tiled, with the terminal floating under `terminal#2` and the keys in nvim.
    fn two_floats() -> Layout {
        use crate::layout::Float;
        let float = |id: ModuleId, cx: f32| Float {
            id,
            rect: FloatRect {
                cx,
                cy: 0.5,
                w: 600,
                h: 300,
            },
            scratch: false,
        };
        Layout::from_parts_floating(
            Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::editor()),
                Node::Leaf(ModuleId::agent()),
            ),
            vec![
                float(ModuleId::terminal(), 0.4),
                float(ModuleId::parse("terminal#2").unwrap(), 0.6),
            ],
            BTreeSet::new(),
            ModuleId::editor(),
            BTreeMap::new(),
        )
        .unwrap()
    }

    fn reopened_floats(dir: &Path) -> Layout {
        let Loaded::Restored(back) = persist::load(dir, root(), &[]) else {
            panic!("the file")
        };
        back.current
    }

    /// Giving the lower float the keys raises it, and that is a move of the keys: nothing is written.
    /// The order goes into the file with the next change, as it is then.
    #[test]
    fn moving_the_keys_between_floats_writes_nothing() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-float-keys");
        let path = dir.join(persist::file_name_v2(root()));
        let window = Window::with(&dir, Workspaces::single(), two_floats(), None);
        window.layout.borrow_mut().set_focus(&ModuleId::terminal()).unwrap();
        assert_eq!(
            window.layout.borrow().floats().last().map(|f| f.id.clone()),
            Some(ModuleId::terminal()),
            "raised"
        );
        window.saver.save_now();
        assert!(!path.exists(), "a raise is not a change");

        let second = ModuleId::parse("terminal#2").unwrap();
        assert!(crate::layout::move_float(
            &mut window.layout.borrow_mut(),
            &second,
            Direction::Down,
            40,
            &frame()
        ));
        window.saver.save_now();
        assert_eq!(file_text(&dir), window.written());
        let ids: Vec<ModuleId> = reopened_floats(&dir).floats().iter().map(|f| f.id.clone()).collect();
        assert_eq!(
            ids,
            [second, ModuleId::terminal()],
            "bottom first, as it was at the write"
        );
    }

    #[test]
    fn a_moved_float_is_written() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-float-moved");
        let window = Window::with(&dir, Workspaces::single(), two_floats(), None);
        let before = window.layout.borrow().float_of(&ModuleId::terminal()).unwrap().rect;
        assert!(crate::layout::move_float(
            &mut window.layout.borrow_mut(),
            &ModuleId::terminal(),
            Direction::Right,
            40,
            &frame()
        ));
        window.saver.save_now();
        let moved = window.layout.borrow().float_of(&ModuleId::terminal()).unwrap().rect;
        assert_ne!(moved, before);
        assert_eq!(
            reopened_floats(&dir).float_of(&ModuleId::terminal()).map(|f| f.rect),
            Some(moved)
        );
    }

    #[test]
    fn a_stowed_float_is_written_hidden() {
        let dir = agent::state_dirs::test_workspace_dir("layout-saver-float-stowed");
        let window = Window::with(&dir, Workspaces::single(), two_floats(), None);
        crate::layout::scratchpad_move(&mut window.layout.borrow_mut(), &ModuleId::terminal(), &frame()).unwrap();
        window.saver.save_now();
        let back = reopened_floats(&dir);
        assert!(back.is_stowed(&ModuleId::terminal()));
        assert!(back.hidden().contains(&ModuleId::terminal()));
        assert!(back.float_of(&ModuleId::terminal()).unwrap().scratch);
    }

    fn rules_of(entries: &[(&str, crate::layout::OpenRule)]) -> OpenRules {
        let mut rules = OpenRules::default();
        for (target, rule) in entries {
            rules
                .add(crate::layout::rules::RuleTarget::parse(target).unwrap(), rule.clone())
                .unwrap();
        }
        rules
    }

    fn at(name: &str) -> Option<crate::layout::rules::At> {
        Some(crate::layout::rules::parse_at(name).unwrap())
    }

    #[test]
    fn a_first_launch_stows_the_terminal_a_scratchpad_rule_names() {
        use crate::layout::OpenRule;
        let rules = rules_of(&[(
            "terminal",
            OpenRule {
                scratchpad: true,
                ..OpenRule::default()
            },
        )]);
        let (decls, _) = rules.launch_decls(&[]);
        let (_, layout, notes, _) =
            choose_startup_layout(None, None, Some(Loaded::Missing), &decls, &rules, None, &[], &[]).unwrap();
        let terminal = ModuleId::terminal();
        assert!(layout.is_floating(&terminal), "{layout:?}");
        assert!(layout.float_of(&terminal).unwrap().scratch);
        assert!(!layout.leaves().contains(&terminal));
        assert!(!layout.is_shown(&terminal));
        assert_eq!(layout.focus(), &ModuleId::editor());
        assert_eq!(layout.visible_leaves(), [ModuleId::editor(), ModuleId::agent()]);
        assert!(
            notes.contains(&"open rule for 'terminal': a first launch floats it, stowed in the scratchpad".to_string()),
            "{notes:?}"
        );
    }

    /// A chat view's or a numbered instance's declaration only says where that module goes when
    /// something else places it: a first launch places neither, and its tree is today's.
    #[test]
    fn a_first_launch_with_rules_for_chat_views_and_instances_places_only_its_own() {
        use crate::layout::OpenRule;
        let rules = rules_of(&[
            (
                "agent",
                OpenRule {
                    at: at("below"),
                    ..OpenRule::default()
                },
            ),
            (
                "terminal#3",
                OpenRule {
                    at: at("side"),
                    ..OpenRule::default()
                },
            ),
        ]);
        let (decls, _) = rules.launch_decls(&[]);
        assert!(decls.iter().any(|d| d.id == ModuleId::agent()));
        assert!(decls.iter().any(|d| d.id == ModuleId::parse("terminal#3").unwrap()));
        let (_, layout, _, _) =
            choose_startup_layout(None, None, Some(Loaded::Missing), &decls, &rules, None, &[], &[]).unwrap();
        let today = first_launch_layout(&[]).unwrap();
        assert_eq!(layout.leaves(), today.leaves());
        assert_eq!(layout, today);
    }

    #[test]
    fn a_first_launch_puts_the_terminal_where_its_rule_says() {
        use crate::layout::OpenRule;
        let rules = rules_of(&[(
            "terminal",
            OpenRule {
                at: at("bottom"),
                ..OpenRule::default()
            },
        )]);
        let (decls, _) = rules.launch_decls(&[]);
        let (_, layout, _, _) =
            choose_startup_layout(None, None, Some(Loaded::Missing), &decls, &rules, None, &[], &[]).unwrap();
        assert_eq!(
            layout.leaves(),
            [ModuleId::editor(), ModuleId::agent(), ModuleId::terminal()],
            "below everything, not below the editor"
        );
        assert!(!layout.is_shown(&ModuleId::terminal()));
    }

    #[test]
    fn a_saved_layout_is_not_moved_by_a_rule() {
        use crate::layout::OpenRule;
        let mut left = first_launch_layout(&[]).unwrap();
        left.show(&ModuleId::terminal()).unwrap();
        let rules = rules_of(&[(
            "terminal",
            OpenRule {
                at: at("bottom"),
                float: true,
                ..OpenRule::default()
            },
        )]);
        let (decls, _) = rules.launch_decls(&[]);
        let (_, layout, notes, _) =
            choose_startup_layout(None, None, Some(restored(left.clone())), &decls, &rules, None, &[], &[]).unwrap();
        assert_eq!(layout, left);
        assert!(notes.iter().all(|n| !n.starts_with("open rule")), "{notes:?}");
    }

    /// A Lua default's tree is the user's own arrangement: what it names stays as it says, and only what
    /// the first launch adds to it -- here a Lua panel and the terminal it leaves out -- follows a rule.
    #[test]
    fn a_lua_default_places_what_it_names_and_the_rules_place_the_rest() {
        use crate::layout::OpenRule;
        let named = ModuleId::lua("named");
        let other = ModuleId::lua("other");
        let floats = OpenRule {
            float: true,
            ..OpenRule::default()
        };
        let rules = rules_of(&[
            ("lua:named", floats.clone()),
            ("lua:other", floats.clone()),
            ("terminal", floats),
        ]);
        let panels = [
            ModuleDecl {
                id: named.clone(),
                placement: Placement::RightOfRoot,
            },
            ModuleDecl {
                id: other.clone(),
                placement: Placement::RightOfRoot,
            },
        ];
        let (decls, _) = rules.launch_decls(&panels);
        let tree = LuaTree {
            tiled: Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(ModuleId::editor()),
                Node::split(
                    Axis::Column,
                    0.5,
                    Node::Leaf(ModuleId::agent()),
                    Node::Leaf(named.clone()),
                ),
            ),
            floats: Vec::new(),
        };
        let (_, layout, notes, _) =
            choose_startup_layout(Some(&tree), None, Some(Loaded::Missing), &decls, &rules, None, &[], &[]).unwrap();
        assert!(layout.leaves().contains(&named) && !layout.is_floating(&named));
        assert!(layout.is_floating(&other) && !layout.leaves().contains(&other));
        assert!(layout.is_floating(&ModuleId::terminal()));
        assert!(
            !layout.is_shown(&ModuleId::terminal()),
            "a first launch's terminal stays hidden"
        );
        for floated in [&other, &ModuleId::terminal()] {
            let note = format!("open rule for '{floated}': a first launch floats it");
            assert!(notes.contains(&note), "{note} in {notes:?}");
        }
        assert!(
            notes
                .iter()
                .all(|n| !n.starts_with(&format!("open rule for '{named}'"))),
            "{notes:?}"
        );
    }
}
