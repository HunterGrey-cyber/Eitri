//! Workspace groups compose the module handovers and publish an arrangement only after every handover decides.

use super::*;
use crate::layout::{persist, workspace::MAIN, Layout, ModuleKind, Workspaces};
use crate::window_link::base_tabs::ArrivedTabs;
use crate::window_link::workspace_move::{free_workspace_name, map_module_ids, Swap, Verdict, WorkspaceMove};

const GROUP_LIMIT: usize = 256;
const TREE_LIMIT: usize = 256 * 1024;
/// Verdicts kept for moves no group or close names yet: an arrival refused before its workspace's packet is read.
const UNCLAIMED_LIMIT: usize = 4 * GROUP_LIMIT;

#[derive(Clone)]
pub(super) struct Outcome {
    offered: ModuleId,
    arrived: ModuleId,
    active: bool,
    verdict: Verdict,
}

pub(super) struct Group {
    state: WorkspaceMove,
    tree: serde_json::Value,
    ids: Vec<MoveId>,
    labels: BTreeMap<MoveId, String>,
    source: bool,
    workspace_gone: bool,
    follow: bool,
    swap: Option<u64>,
    published: bool,
    active: Option<TabId>,
    move_order: BTreeMap<MoveId, usize>,
    immediate: Vec<(String, String)>,
}

#[derive(Default)]
pub(super) struct Groups {
    next: u64,
    pub(super) groups: BTreeMap<u64, Group>,
    pub(super) outcomes: BTreeMap<MoveId, Outcome>,
    /// Moves a closing window waits on, so their verdicts are kept for it.
    watched: BTreeSet<MoveId>,
    arrived_tabs: ArrivedTabs,
}

impl Groups {
    /// No group can settle once the link is gone; the record of the tabs that came with each workspace stays.
    pub(super) fn link_ended(&mut self) {
        self.groups.clear();
        self.outcomes.clear();
        self.watched.clear();
    }

    fn claimed(&self, id: MoveId) -> bool {
        self.watched.contains(&id) || self.groups.values().any(|g| g.ids.contains(&id))
    }

    /// A source's verdict matters only to a group or a close that names it; an arrival's may come before its
    /// workspace's packet does, so it is kept, the oldest unclaimed ones dropped past a bound.
    fn keep(&mut self, id: MoveId, role: Role, outcome: Outcome) {
        if role == Role::Source && !self.claimed(id) {
            return;
        }
        self.outcomes.insert(id, outcome);
        let unclaimed: Vec<_> = self.outcomes.keys().copied().filter(|id| !self.claimed(*id)).collect();
        for id in unclaimed.iter().take(unclaimed.len().saturating_sub(UNCLAIMED_LIMIT)) {
            self.outcomes.remove(id);
        }
    }

    /// Drops the verdicts of `ids` that nothing else still waits on.
    fn forget(&mut self, ids: &[MoveId]) {
        for id in ids {
            if !self.claimed(*id) {
                self.outcomes.remove(id);
            }
        }
    }
}

pub(super) fn decided(p: &mut PeerMoves, id: MoveId, entry: &Entry, verdict: Verdict) {
    p.groups.keep(
        id,
        entry.role,
        Outcome {
            offered: entry.offered.clone(),
            arrived: entry.module.clone(),
            active: entry.record["workspace_tab"]["active"] == true,
            verdict,
        },
    );
}

pub(super) fn refused_offer(p: &mut PeerMoves, id: MoveId, module: &ModuleId, why: String) {
    p.groups.keep(
        id,
        Role::Destination,
        Outcome {
            offered: module.clone(),
            arrived: module.clone(),
            active: false,
            verdict: Verdict::Refused(why),
        },
    );
}

/// The close flow waits on `id`: its verdict is kept until [`unwatch`].
pub(in crate::wm) fn watch(w: &mut Window, id: MoveId) {
    if let Some(p) = w.peer.as_mut() {
        p.groups.watched.insert(id);
    }
}

pub(in crate::wm) fn unwatch(w: &mut Window, ids: &[MoveId]) {
    if let Some(p) = w.peer.as_mut() {
        for id in ids {
            p.groups.watched.remove(id);
        }
        p.groups.forget(ids);
    }
}

/// Whether arriving move `id` belongs to a workspace this window is making.
pub(super) fn arrives_with_group(p: &PeerMoves, id: MoveId) -> bool {
    p.groups.groups.values().any(|g| !g.source && g.ids.contains(&id))
}

/// A workspace deleted here takes its record of the tabs that came with it along, so a later one of the same name
/// does not inherit it. An outgoing group no longer owns a workspace here, even if its name is taken again.
pub(in crate::wm) fn workspace_deleted(w: &mut Window, name: &str) {
    if let Some(p) = w.peer.as_mut() {
        p.groups.arrived_tabs.take_back(name, &[]);
        for g in p.groups.groups.values_mut() {
            if g.source && g.state.name() == name {
                g.workspace_gone = true;
            }
        }
    }
}

/// A renamed workspace keeps its record of the tabs that came with it, renamed to its own name included.
/// Pages still waiting and outgoing groups follow the name too, so ready and completion find the same workspace.
pub(in crate::wm) fn workspace_renamed(w: &mut Window, old: &str, new: &str) {
    if let Some(p) = w.peer.as_mut() {
        p.groups.arrived_tabs.workspace_renamed(old, new);
        for g in p.groups.groups.values_mut() {
            if g.source && g.state.name() == old {
                g.state.rename(new);
            }
        }
        for waiting in p.pages_waiting.values_mut() {
            if waiting.workspace == old {
                waiting.workspace = new.to_owned();
            }
        }
    }
}

fn label(module: &ModuleId, tab: Option<TabId>) -> String {
    tab.map_or_else(|| module.to_string(), |tab| format!("{module} tab {}", tab.0))
}

fn snapshot(w: &Window, cx: &Cx) -> Option<(String, serde_json::Value, Vec<ModuleId>)> {
    let layout = w.layout.try_borrow().ok()?;
    let workspaces = w.workspaces.try_borrow().ok()?;
    let mut saved = layout.clone();
    if !saved.contains(&ModuleId::editor()) {
        crate::layout::workspace::fit_to(&mut saved, &[ModuleId::editor(), ModuleId::agent()]);
    }
    let text = persist::encode_v2(&workspaces, &saved, &cx.setup.project_dir);
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let name = workspaces.current().to_owned();
    let tree = value["workspaces"]
        .as_array()?
        .iter()
        .find(|v| v["name"] == name)?
        .clone();
    let modules = layout
        .modules()
        .into_iter()
        .filter(|m| layout.is_shown(m) && !layout.is_away(m))
        .collect();
    Some((name, tree, modules))
}

/// Offer the current workspace; a swap uses the group id from the other window as its rendezvous.
pub fn offer(w: &mut Window, cx: &Cx, win: WinId, follow: bool, swap: bool, batch: &mut Batch) {
    if w.peer.is_none() {
        refuse(w, win, feedback::no_other_window_text(w.role()), batch);
        return;
    }
    if w.peer
        .as_ref()
        .is_some_and(|p| p.groups.groups.values().any(|g| g.source))
    {
        refuse(w, win, "a workspace is already moving", batch);
        return;
    }
    if let Some(group) = start(w, cx, win, follow, swap.then_some(0), batch) {
        if swap {
            with_peer(w, |w, p| {
                p.groups.groups.get_mut(&group).unwrap().swap = Some(0);
                send(
                    w,
                    p,
                    Msg::Swap {
                        move_a: group,
                        move_b: 0,
                    },
                );
            });
        }
    }
}

fn start(w: &mut Window, cx: &Cx, win: WinId, follow: bool, swap: Option<u64>, batch: &mut Batch) -> Option<u64> {
    let (name, mut tree, modules) = snapshot(w, cx)?;
    if tree.to_string().len() > TREE_LIMIT {
        refuse(w, win, "a workspace tree is too large", batch);
        return None;
    }
    let existing = cx.reads.base_tabs(win);
    let recorded = w.workspaces.borrow().moved_tabs_of(&name).to_vec();
    let has_record = !w.peer.as_ref()?.groups.arrived_tabs.recorded(&name).is_empty();
    let recorded_tabs = w
        .peer
        .as_ref()?
        .groups
        .arrived_tabs
        .recorded(&name)
        .iter()
        .copied()
        .filter(|t| existing.contains(t))
        .collect();
    let tabs: Vec<_> = if has_record {
        recorded_tabs
    } else if !recorded.is_empty() {
        existing
            .into_iter()
            .filter(|t| cx.reads.moved_tab_key(win, *t).is_some_and(|k| recorded.contains(&k)))
            .collect()
    } else {
        existing
    };
    let active = cx.reads.active_base_tab(win);
    let mut requests = Vec::new();
    for module in modules {
        if module == ModuleId::editor() {
            continue;
        }
        if module == ModuleId::agent() {
            requests.extend(tabs.iter().map(|tab| (module.clone(), Some(*tab))));
        } else {
            requests.push((module, None));
        }
    }
    if requests.len() > GROUP_LIMIT {
        refuse(w, win, "too many modules to move in one workspace", batch);
        return None;
    }
    // The active flag and stable order travel with each tab's ordinary move record.
    let active_key = active;
    if let Some(object) = tree.as_object_mut() {
        object.remove("moved_tabs");
    }
    let p = w.peer.as_mut()?;
    p.groups.next += 1;
    let group = (p.groups.next << 1) | u64::from(p.side == Side::Two);
    let g = Group {
        state: WorkspaceMove::new(&name, []),
        tree,
        ids: Vec::new(),
        labels: BTreeMap::new(),
        source: true,
        workspace_gone: false,
        follow,
        swap,
        published: false,
        active: active_key,
        move_order: BTreeMap::new(),
        immediate: Vec::new(),
    };
    offer_group(w, cx, win, group, g, requests, batch);
    Some(group)
}

/// Every module is offered at once and the packet follows: a workspace waits for no turn, so a chat whose running
/// turn cannot move live stays, as any refusal does.
fn offer_group(
    w: &mut Window,
    cx: &Cx,
    win: WinId,
    group: u64,
    mut g: Group,
    requests: Vec<(ModuleId, Option<TabId>)>,
    batch: &mut Batch,
) {
    for (order, (module, tab)) in requests.into_iter().enumerate() {
        let facts = match tab {
            Some(tab) => Some(cx.reads.tab_move(win, tab)),
            None if module.kind() == ModuleKind::Agent => Some(cx.reads.chat_move(win, chat::view_of(&module), None)),
            None => None,
        };
        let offered = if facts
            .as_ref()
            .is_some_and(|facts| matches!(facts, Ok(f) if f.running && !f.live))
        {
            w.say(format!("[move] {module}: {}", feedback::TURN_RUNS_STAYS_TEXT));
            Err(feedback::TURN_RUNS_STAYS_TEXT.to_owned())
        } else {
            offer_selected(w, cx, win, &module, None, false, facts, batch)
        };
        match offered {
            Ok(Some(id)) => {
                g.move_order.insert(id, order);
                if let Some(entry) = w.peer.as_mut().and_then(|p| p.entries.get_mut(&id)) {
                    if tab.is_some() {
                        entry.record = serde_json::json!({"workspace_tab": {"group": group, "order": order,
                            "active": entry.tab == g.active}});
                    }
                }
                g.labels.insert(id, label(&module, tab));
                g.ids.push(id);
            }
            // Immediate refusals have no wire move; the destination keeps their leaves away.
            Err(why) => g.immediate.push((label(&module, tab), why)),
            Ok(None) => g
                .immediate
                .push((label(&module, tab), "the module could not be offered".into())),
        }
    }
    if !g.published {
        g.ids.sort_by_key(|id| g.move_order[id]);
        let mut state = WorkspaceMove::new(
            g.state.name(),
            g.ids
                .iter()
                .map(|id| g.labels[id].clone())
                .chain(g.immediate.iter().map(|(m, _)| m.clone())),
        );
        for (module, why) in &g.immediate {
            state.record(module, Verdict::Refused(why.clone()));
        }
        g.state = state;
        with_peer(w, |w, p| {
            send(
                w,
                p,
                Msg::WorkspaceMove {
                    group,
                    name: g.state.name().to_owned(),
                    tree: g.tree.clone(),
                    moves: g.ids.iter().map(|id| id.0).collect(),
                    swap: g.swap,
                },
            )
        });
        g.published = true;
    }
    if let Some(p) = w.peer.as_mut() {
        p.groups.groups.insert(group, g);
    }
}

/// A peer name and saved tree pass the same bounded reader as local persistence before they reach the layout.
pub(super) fn receive(
    w: &mut Window,
    p: &mut PeerMoves,
    cx: &Cx,
    win: WinId,
    group: u64,
    name: String,
    tree: serde_json::Value,
    ids: Vec<u64>,
    swap: Option<u64>,
    batch: &mut Batch,
) {
    let checked = if p.groups.groups.len() >= GROUP_LIMIT || ids.len() > GROUP_LIMIT {
        Err("too many workspaces are moving".into())
    } else if p.groups.groups.contains_key(&group) {
        Err("a workspace move was sent twice".into())
    } else if name.len() > 128 {
        Err("a workspace name is too large".into())
    } else {
        Workspaces::valid_name(&name).map_err(|e| e.to_string()).and_then(|_| {
            let candidate = free_workspace_name(&name, w.workspaces.borrow().order());
            validate_tree(&candidate, &tree, cx)
        })
    };
    if let Err(why) = checked {
        // Arrivals already taken for this workspace are dropped here as well as called off there.
        for id in ids.into_iter().take(GROUP_LIMIT) {
            let id = MoveId(id);
            if p.groups.groups.values().any(|g| g.ids.contains(&id)) {
                continue;
            }
            send(
                w,
                p,
                Msg::MoveAbort {
                    mv: id.0,
                    why: why.clone(),
                },
            );
            if p.entries.get(&id).is_some_and(|e| e.role == Role::Destination) {
                p.moves.refuse_received(id);
                p.moves.source_aborted(id);
                drop_arrival(w, p, win, id, batch);
            }
        }
        refuse(w, win, &why, batch);
        return;
    }
    if let Some(other) = swap.filter(|other| *other != 0) {
        if let Some(g) = p.groups.groups.get_mut(&other) {
            g.swap = Some(group);
        }
    }
    let ids: Vec<_> = ids.into_iter().map(MoveId).collect();
    p.groups.groups.insert(
        group,
        Group {
            state: WorkspaceMove::new(&name, ids.iter().map(|id| id.0.to_string())),
            tree,
            ids,
            labels: BTreeMap::new(),
            source: false,
            workspace_gone: false,
            follow: false,
            swap,
            published: true,
            active: None,
            move_order: BTreeMap::new(),
            immediate: Vec::new(),
        },
    );
}

fn validate_tree(name: &str, tree: &serde_json::Value, cx: &Cx) -> Result<Layout, String> {
    if name.len() > 128 {
        return Err("a workspace name is too large".into());
    }
    Workspaces::valid_name(name).map_err(|e| e.to_string())?;
    let mut tree = tree.clone();
    let object = tree.as_object_mut().ok_or("a workspace tree is not an object")?;
    object.insert("name".into(), MAIN.into());
    let text = serde_json::json!({"version": 2, "project_root": cx.setup.project_dir.to_string_lossy(),
        "current": MAIN, "workspaces": [tree]})
    .to_string();
    if text.len() > TREE_LIMIT {
        return Err("a workspace tree is too large".into());
    }
    let opened = persist::decode_v2_with(
        &text,
        &cx.setup.project_dir,
        &cx.setup.decls,
        &cx.setup
            .start_hidden
            .iter()
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>(),
    )?;
    if opened.keep_copy.is_some() {
        return Err("a workspace tree cannot be read without losing data".into());
    }
    Ok(opened.current)
}

/// `swap{a, 0}` asks for this window's half of the swap whose other half is group `a`; sent back to the window whose
/// group `a` is, it says no half will come, and that group goes on as a plain move. `swap{a, b}` pairs the two.
pub(super) fn swap(w: &mut Window, cx: &Cx, win: WinId, move_a: u64, move_b: u64, batch: &mut Batch) {
    if move_b == 0
        && w.peer
            .as_ref()
            .and_then(|p| p.groups.groups.get(&move_a))
            .is_some_and(|g| g.source)
    {
        let text = "the other window's workspace could not move; this one moves alone";
        w.say(format!("[move] {text}"));
        chrome::show_toast(w, text.to_owned(), ToastKind::Other);
        if let Some(g) = w.peer.as_mut().and_then(|p| p.groups.groups.get_mut(&move_a)) {
            g.swap = None;
        }
        return;
    }
    if move_b == 0 {
        let outgoing = w
            .peer
            .as_ref()
            .and_then(|p| p.groups.groups.iter().find(|(_, g)| g.source).map(|(id, _)| *id));
        let Some(group) = outgoing.or_else(|| start(w, cx, win, false, Some(move_a), batch)) else {
            // The other window's workspace still arrives here, as a plain move.
            with_peer(w, |w, p| {
                if let Some(incoming) = p.groups.groups.get_mut(&move_a) {
                    incoming.swap = None;
                }
                send(w, p, Msg::Swap { move_a, move_b: 0 });
            });
            return;
        };
        with_peer(w, |w, p| {
            if let Some(outgoing) = p.groups.groups.get_mut(&group) {
                outgoing.swap = Some(move_a);
            }
            if let Some(incoming) = p.groups.groups.get_mut(&move_a) {
                incoming.swap = Some(group);
            }
            send(w, p, Msg::Swap { move_a, move_b: group });
        });
    } else if let Some(p) = w.peer.as_mut() {
        if let Some(g) = p.groups.groups.get_mut(&move_a) {
            g.swap = Some(move_b);
        }
        if let Some(g) = p.groups.groups.get_mut(&move_b) {
            g.swap = Some(move_a);
        }
    }
}

/// Settle groups after the batch, including host answers that froze or refused a module without a peer event.
pub fn progress(w: &mut Window, cx: &Cx, win: WinId, batch: &mut Batch) {
    with_peer(w, |w, p| {
        for g in p.groups.groups.values_mut() {
            for id in &g.ids {
                if let Some(outcome) = p.groups.outcomes.get(id) {
                    g.state.record(
                        &g.labels.get(id).cloned().unwrap_or_else(|| id.0.to_string()),
                        outcome.verdict.clone(),
                    );
                }
            }
        }
        let settled: Vec<_> = p
            .groups
            .groups
            .iter()
            .filter(|(_, g)| {
                g.published
                    && g.state.settled()
                    // Zero is the other half not answered yet. A half that was sent and is not here was refused
                    // whole (its packet failed to read), so this one goes on as a plain move.
                    && match g.swap {
                        None => true,
                        Some(0) => false,
                        Some(other) => p.groups.groups.get(&other).is_none_or(|b| {
                            Swap {
                                to_window1: g.state.clone(),
                                to_window2: b.state.clone(),
                            }
                            .settled()
                                && b.published
                        }),
                    }
            })
            .map(|(id, _)| *id)
            .collect();
        // Outgoing workspaces leave before incoming ones switch, so a swap never deletes its new current view.
        let outgoing: Vec<_> = settled
            .iter()
            .copied()
            .filter(|id| p.groups.groups.get(id).is_some_and(|g| g.source))
            .collect();
        for id in outgoing {
            let g = p.groups.groups.get(&id).unwrap();
            if let Some(text) = g.state.toast() {
                w.say(text.clone());
                chrome::show_toast(w, text, ToastKind::Other);
            }
            if !g.workspace_gone && !g.state.source_keeps_workspace() && g.state.name() != MAIN {
                let name = g.state.name().to_owned();
                let current = w.workspaces.borrow().current() == name;
                if current {
                    let _ = super::super::verbs::workspaces::switch(w, win, MAIN, cx.setup, batch);
                }
                let _ = w.workspaces.borrow_mut().delete(&name);
                p.groups.arrived_tabs.take_back(&name, &[]);
                w.chrome_state.dirty.workspaces += 1;
                w.saver_changed();
            }
            if g.follow {
                queue_raise(p, win, FocusTarget::Window, batch);
            }
        }
        let incoming: Vec<_> = settled
            .iter()
            .copied()
            .filter(|id| p.groups.groups.get(id).is_some_and(|g| !g.source))
            .collect();
        for id in &incoming {
            let g = p.groups.groups.remove(id).unwrap();
            if let Err(why) = install(w, p, cx, win, &g, batch) {
                reveal_arrivals(w, p, &g);
                refuse(w, win, &why, batch);
            }
            p.groups.forget(&g.ids);
        }
        for id in settled {
            if let Some(g) = p.groups.groups.remove(&id) {
                p.groups.forget(&g.ids);
            }
        }
    });
}

/// A workspace still being made cannot settle after the link ends; its committed arrivals stay visible here.
pub(super) fn link_ended(w: &mut Window, p: &mut PeerMoves, win: WinId, batch: &mut Batch) {
    let mut revealed = false;
    for g in p.groups.groups.values().filter(|g| !g.source) {
        reveal_arrivals(w, p, g);
        revealed = true;
    }
    p.groups.link_ended();
    if revealed {
        w.changed_by_controller(win, batch);
    }
}

/// The modules that came for a workspace that could not be made here are shown in the one on screen instead, or
/// nothing would show them.
fn reveal_arrivals(w: &Window, p: &PeerMoves, g: &Group) {
    let Ok(mut layout) = w.layout.try_borrow_mut() else {
        return;
    };
    for id in &g.ids {
        if let Some(o) = p.groups.outcomes.get(id).filter(|o| o.verdict == Verdict::Committed) {
            if o.arrived != ModuleId::agent() && layout.contains(&o.arrived) {
                let _ = layout.show(&o.arrived);
            }
        }
    }
}

fn install(w: &mut Window, p: &mut PeerMoves, cx: &Cx, win: WinId, g: &Group, batch: &mut Batch) -> Result<(), String> {
    let taken = w.workspaces.borrow().order().to_vec();
    let name = free_workspace_name(g.state.name(), &taken);
    Workspaces::valid_name(&name).map_err(|e| e.to_string())?;
    let ids = g
        .ids
        .iter()
        .filter_map(|id| p.groups.outcomes.get(id))
        .filter(|o| o.verdict == Verdict::Committed)
        .map(|o| (o.offered.to_string(), o.arrived.to_string()))
        .collect();
    let mut layout = validate_tree(&name, &map_module_ids(&g.tree, &ids), cx)?;
    let committed: BTreeSet<_> = g
        .ids
        .iter()
        .filter_map(|id| p.groups.outcomes.get(id))
        .filter(|o| o.verdict == Verdict::Committed)
        .map(|o| o.arrived.clone())
        .collect();
    let editor = ModuleId::editor();
    if matches!(w.presence(), super::super::window::EditorPresence::Owned) {
        // The editor never moves: its leaf follows this window's running state.
        if !layout.contains(&editor) {
            crate::layout::workspace::fit_to(&mut layout, std::slice::from_ref(&editor));
        }
        if matches!(
            w.editor_run,
            super::super::window::EditorRun::Ending | super::super::window::EditorRun::Ended
        ) {
            frame_with(cx.reads, win, cx.reads.grid_size(win), |frame| {
                super::super::verbs::workspaces::hide_ended_editor(&mut layout, frame);
            });
        } else if w.layout.try_borrow().is_ok_and(|l| l.is_shown(&editor)) {
            let _ = layout.show(&editor);
        }
    } else {
        frame_with(cx.reads, win, cx.reads.grid_size(win), |frame| {
            if layout.contains(&editor) {
                if layout.visible_modules() == vec![editor.clone()] {
                    let _ = layout.show(&ModuleId::agent());
                }
                let _ = crate::layout::kill(&mut layout, &editor, crate::layout::Reopen::Never, frame);
            }
        });
    }
    for module in layout.modules() {
        if module != editor && module != ModuleId::agent() && !committed.contains(&module) {
            let _ = layout.set_away(&module, true);
        }
    }
    let tabs: Vec<_> = g.ids.iter().filter_map(|id| cx.reads.arrived_tab(win, *id)).collect();
    let active = g
        .ids
        .iter()
        .find(|id| p.groups.outcomes.get(id).is_some_and(|o| o.active))
        .and_then(|id| cx.reads.arrived_tab(win, *id));
    let existing: Vec<_> = cx
        .reads
        .base_tabs(win)
        .into_iter()
        .filter(|t| !tabs.contains(t))
        .collect();
    w.workspaces
        .borrow_mut()
        .create(&name, layout)
        .map_err(|e| e.to_string())?;
    if !tabs.is_empty() {
        batch.push_effect(HostOp::OrderBaseTabs {
            win,
            tabs: crate::window_link::base_tabs::order_after_arrival(&existing, None, &tabs),
            active,
        });
        p.groups.arrived_tabs.arrived(&name, &tabs);
    }
    let mut keys = w.workspaces.borrow().moved_tabs().clone();
    keys.insert(
        name.clone(),
        tabs.iter().filter_map(|t| cx.reads.moved_tab_key(win, *t)).collect(),
    );
    w.workspaces.borrow_mut().set_moved_tabs(keys);
    w.chrome_state.dirty.workspaces += 1;
    w.saver_changed();
    super::super::verbs::workspaces::switch(w, win, &name, cx.setup, batch)?;
    Ok(())
}
