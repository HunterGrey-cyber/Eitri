//! Moving a module to the other window of the linked pair, for both ends, in the controller.
//!
//! `core::window_link::moves::Moves` is the one judge of where a move stands: every fact that may change it (an
//! answer from the other window, the terminal's freeze, the link's end, the deadline) goes through its method, one
//! transition per fact, and this module only carries out what each answer means. The source keeps the module where it
//! is, on screen, until the destination's `move_done`; then the module's leaf stays as an away placeholder in every
//! workspace and the local copy is dropped. The destination lays the module out off screen before anything of it is
//! frozen, holds what `move` brought unread, and builds the module from it only once its `move_done` is known to be
//! on the socket, so a module is never held by both windows nor by neither.
//!
//! The shell does only the per-kind surface work (`PrepareSurface`, `Terminal(Detach)`, `AdoptMoved`, ...). Every kind
//! but the terminal is refused at [`plan`] until its surface work exists.

use super::chrome;
use super::feedback;
use super::focus::{self, arrive_tail};
use super::host::*;
use super::module::SurfaceDecl;
use super::toasts::ToastKind;
use super::verbs::modules::Cx;
use super::window::{ActivationToken, LinkSide, PeerId, WinId, Window, WindowRole};
use crate::layout::rules::OpenPoint;
use crate::layout::{declared_placement, open_by_rule, FloatRect, ModuleId, Placement, DEFAULT_FLOAT_SHARE};
use crate::window_link::ids::arrival_id;
use crate::window_link::moves::{MoveId, Moves, PeerAbort, Role, SourceVerdict, Window as Side, MOVE_DEADLINE};
use crate::window_link::{FocusTarget, Msg, Shape, Summary};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io;
use std::rc::Rc;
use std::time::Instant;

/// Where a module is asked to go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveTarget {
    /// The other window of the linked pair, in its own process.
    Peer(PeerId),
    /// Another window of this process.
    Local(WinId),
}

/// The kinds of module a move knows; the wire name is `move_offer`'s `kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveKind {
    Terminal,
    Chat,
    PluginPage,
    LuaPanel,
}

impl MoveKind {
    /// The name `move_offer` carries.
    pub fn wire(self) -> &'static str {
        match self {
            MoveKind::Terminal => "terminal",
            MoveKind::Chat => "chat",
            MoveKind::PluginPage => "plugin_page",
            MoveKind::LuaPanel => "lua_panel",
        }
    }

    /// The kind `move_offer` names; `None` for a name this build does not know.
    pub fn from_wire(name: &str) -> Option<MoveKind> {
        match name {
            "terminal" => Some(MoveKind::Terminal),
            "chat" => Some(MoveKind::Chat),
            "plugin_page" => Some(MoveKind::PluginPage),
            "lua_panel" => Some(MoveKind::LuaPanel),
            _ => None,
        }
    }

    /// The kind of a module declared with `surface`; the editor has none, since it never moves.
    pub fn of(surface: &SurfaceKind) -> Option<MoveKind> {
        match surface {
            SurfaceKind::Editor { .. } => None,
            SurfaceKind::Chat { .. } => Some(MoveKind::Chat),
            SurfaceKind::LuaPanel { .. } => Some(MoveKind::LuaPanel),
            SurfaceKind::PluginPage { .. } => Some(MoveKind::PluginPage),
            SurfaceKind::Terminal { .. } => Some(MoveKind::Terminal),
        }
    }
}

/// What a move of one module comes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MovePlan {
    /// The linked window: the link's commit machine carries it, and this window keeps an away placeholder.
    Peer {
        module: ModuleId,
        peer: PeerId,
        kind: MoveKind,
    },
    Refused {
        why: &'static str,
    },
}

/// Whether `module` of window `w` may go to `target`, and as what. Only a terminal may go for now; the other kinds
/// are refused until the other window can take them.
pub fn plan(w: &Window, module: &ModuleId, target: &MoveTarget) -> MovePlan {
    let peer = match target {
        MoveTarget::Local(_) => {
            return MovePlan::Refused {
                why: feedback::LOCAL_MOVE_TEXT,
            }
        }
        MoveTarget::Peer(peer) => *peer,
    };
    if *module == ModuleId::editor() {
        return MovePlan::Refused {
            why: feedback::EDITOR_MOVE_TEXT,
        };
    }
    let Ok(layout) = w.layout.try_borrow() else {
        return MovePlan::Refused {
            why: feedback::LAYOUT_CHANGING_TEXT,
        };
    };
    if !layout.contains(module) {
        return MovePlan::Refused {
            why: feedback::NOT_HELD_TEXT,
        };
    }
    if layout.is_away(module) {
        // Bringing a module back from the other window is asked of the other window, which is not built yet.
        return MovePlan::Refused {
            why: feedback::NO_SECOND_WINDOW_TEXT,
        };
    }
    drop(layout);
    if w.peer.as_ref().is_some_and(|p| p.moving(module)) {
        return MovePlan::Refused {
            why: feedback::MOVING_TEXT,
        };
    }
    match w.lifecycle.kind(module).and_then(MoveKind::of) {
        Some(MoveKind::Terminal) => MovePlan::Peer {
            module: module.clone(),
            peer,
            kind: MoveKind::Terminal,
        },
        Some(MoveKind::Chat | MoveKind::PluginPage | MoveKind::LuaPanel) => MovePlan::Refused {
            why: feedback::NO_SECOND_WINDOW_TEXT,
        },
        None => MovePlan::Refused {
            why: feedback::NOT_HELD_TEXT,
        },
    }
}

/// Where one move stands, as far as this window's own work for it goes; `Moves` says where it stands on the link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Source: offered, the module untouched.
    Offered,
    /// Source: frozen, its record and handoff asked of the host.
    Freezing,
    /// Source: `move` went out; only `move_done`, `move_abort` or the link's end decides it now.
    Sent,
    /// Destination: accepted, its place being prepared.
    Accepted,
    /// Destination: its place is ready and `move_ready` went out.
    Prepared,
    /// Destination: `move` arrived and `move_done` is queued.
    Received,
}

#[derive(Debug)]
struct Entry {
    /// The module here: on the source the moving module, on the destination the id it arrives as.
    module: ModuleId,
    /// The id the source gave the module, which its focus intent names.
    offered: ModuleId,
    kind: MoveKind,
    role: Role,
    /// Source: the keys go with the module once it is committed.
    follow: bool,
    stage: Stage,
    /// Destination: where the module sat in the source.
    shape: Shape,
    /// Destination: the kind's record that `move` carried.
    record: serde_json::Value,
    /// Destination: the descriptors `move` brought, put back here once the commit hands them over.
    fds: Option<Descriptors>,
    /// Source: when the deadline next speaks for this move.
    deadline: Option<Instant>,
}

/// A window's link to the other window of the pair and the moves on it; present while a peer is attached.
pub struct PeerMoves {
    peer: PeerId,
    port: Rc<dyn PeerPort>,
    moves: Moves,
    side: Side,
    entries: BTreeMap<MoveId, Entry>,
    /// The destination's committed arrivals: the id the source gave each, and the id it took here.
    arrived: BTreeMap<ModuleId, ModuleId>,
    /// What each `RaisePeer` still unanswered is for, oldest first: the focus to send once the other window is up.
    raises: VecDeque<FocusTarget>,
    /// The deadline's timer while one is armed, and when it fires.
    deadline: Option<(TimerToken, Instant)>,
    /// The other window's last summary.
    summary: Option<Summary>,
}

impl PeerMoves {
    /// The moves of a window whose role makes it `side` of the pair, talking through `port`.
    pub fn new(peer: PeerId, port: Rc<dyn PeerPort>, side: Side) -> PeerMoves {
        PeerMoves {
            peer,
            port,
            moves: Moves::new(side),
            side,
            entries: BTreeMap::new(),
            arrived: BTreeMap::new(),
            raises: VecDeque::new(),
            deadline: None,
            summary: None,
        }
    }

    pub fn peer(&self) -> PeerId {
        self.peer
    }

    /// The other window's counts and workspace names, as it last sent them.
    pub fn summary(&self) -> Option<&Summary> {
        self.summary.as_ref()
    }

    /// Whether `module` has a move in flight from this window.
    pub fn moving(&self, module: &ModuleId) -> bool {
        self.entries
            .values()
            .any(|e| e.role == Role::Source && e.module == *module)
    }

    /// The ids modules arriving here took and that are not placed yet: no new instance may take them.
    pub(crate) fn arriving(&self) -> Vec<ModuleId> {
        self.entries
            .values()
            .filter(|e| e.role == Role::Destination)
            .map(|e| e.module.clone())
            .collect()
    }

    fn entry_of(&self, role: Role, module: &ModuleId, stages: &[Stage]) -> Option<MoveId> {
        self.entries
            .iter()
            .find(|(_, e)| e.role == role && e.module == *module && stages.contains(&e.stage))
            .map(|(id, _)| *id)
    }

    /// The text of the note a sent move gets when the other window stays silent.
    fn not_answering(&self) -> String {
        feedback::not_answering_text(match self.side {
            Side::One => 2,
            Side::Two => 1,
        })
    }
}

/// The side of the pair a window of `role` is: the second end of a link is window 2, everything else window 1.
pub fn side_of(role: &WindowRole) -> Side {
    match role {
        WindowRole::Linked { side: LinkSide::Second } => Side::Two,
        WindowRole::Main | WindowRole::Companion | WindowRole::Linked { side: LinkSide::First } => Side::One,
    }
}

/// Whether `module` of `w` has a move in flight from this window: until the move ends, nothing here may act on the
/// session the other window may already be taking over.
pub(crate) fn moving(w: &Window, module: &ModuleId) -> bool {
    w.peer.as_ref().is_some_and(|p| p.moving(module))
}

/// The ids modules arriving in `w` took and that are not placed yet; a new instance here must not take one, or the
/// arrival would land on a module that already holds its name.
pub(crate) fn arriving(w: &Window) -> Vec<ModuleId> {
    w.peer.as_ref().map(PeerMoves::arriving).unwrap_or_default()
}

/// Runs `f` on the window's peer, taken out for the call so both can be changed; `None` with no peer attached.
fn with_peer<R>(w: &mut Window, f: impl FnOnce(&mut Window, &mut PeerMoves) -> R) -> Option<R> {
    let mut p = w.peer.take()?;
    let out = f(w, &mut p);
    w.peer = Some(p);
    Some(out)
}

/// A refusal of the move verb, said and toasted.
fn refuse(w: &mut Window, win: WinId, text: &str, batch: &mut Batch) {
    w.say(format!("[move] refused: {text}"));
    w.refuse(win, text.to_owned(), batch);
}

fn send(w: &mut Window, p: &PeerMoves, msg: Msg) {
    let kind = msg.kind();
    if let Err(err) = p.port.send(msg, None) {
        w.warn(format!("[move] {kind} could not be sent: {err}"));
    }
}

/// Where `module` sits now, as the other window lays it out: a float by its rectangle relative to the grid.
fn shape_of(w: &Window, cx: &Cx, win: WinId, module: &ModuleId) -> Shape {
    let Ok(layout) = w.layout.try_borrow() else {
        return Shape::Tiled;
    };
    let Some(float) = layout.float_of(module) else {
        return Shape::Tiled;
    };
    let grid = cx.reads.grid_size(win);
    if grid.w <= 0 || grid.h <= 0 {
        return Shape::Tiled;
    }
    let (fw, fh) = (float.rect.w as f64 / grid.w as f64, float.rect.h as f64 / grid.h as f64);
    Shape::Float {
        x: float.rect.cx as f64 - fw / 2.0,
        y: float.rect.cy as f64 - fh / 2.0,
        w: fw,
        h: fh,
    }
}

// The source's side.

/// `move.to-window` for `module`: planned, and for the other window offered. Nothing is frozen yet; the deadline
/// starts.
pub fn offer(w: &mut Window, cx: &Cx, win: WinId, module: &ModuleId, follow: bool, batch: &mut Batch) {
    let Some(peer) = w.peer.as_ref().map(PeerMoves::peer) else {
        refuse(w, win, feedback::NO_SECOND_WINDOW_TEXT, batch);
        return;
    };
    let kind = match plan(w, module, &MoveTarget::Peer(peer)) {
        MovePlan::Refused { why } => {
            refuse(w, win, why, batch);
            return;
        }
        MovePlan::Peer { kind, .. } => kind,
    };
    let shape = shape_of(w, cx, win, module);
    // Zero when the host cannot tell: the other window then lays the module out at its own size.
    let size = cx
        .reads
        .module_size(win, module)
        .map(|size| (size.w.max(0) as u32, size.h.max(0) as u32))
        .unwrap_or((0, 0));
    with_peer(w, |w, p| {
        let now = w.now();
        let id = p.moves.offer(now);
        let msg = Msg::MoveOffer {
            mv: id.0,
            module: module.to_string(),
            kind: kind.wire().to_owned(),
            shape,
            size,
            session: None,
        };
        if let Err(err) = p.port.send(msg, None) {
            // Nothing was frozen: the module never left.
            p.moves.aborted(id, &err);
            refuse(w, win, &err, batch);
            return;
        }
        p.entries.insert(
            id,
            Entry {
                module: module.clone(),
                offered: module.clone(),
                kind,
                role: Role::Source,
                follow,
                stage: Stage::Offered,
                shape,
                record: serde_json::Value::Null,
                fds: None,
                deadline: Some(now + MOVE_DEADLINE),
            },
        );
        w.say(format!("[move] {module}: offered to the other window as move {}", id.0));
        rearm(w, p);
    });
}

/// The destination is ready: the module is frozen and its record asked of the host. An answer out of turn
/// changes nothing.
fn ready(w: &mut Window, p: &mut PeerMoves, win: WinId, id: MoveId, batch: &mut Batch) {
    if !p.moves.ready(id) {
        return;
    }
    let now = w.now();
    if !p.moves.freeze(id, now) {
        return;
    }
    let Some(entry) = p.entries.get_mut(&id) else {
        return;
    };
    entry.stage = Stage::Freezing;
    entry.deadline = Some(now + MOVE_DEADLINE);
    // The module stays shown where it is; a frozen session reads and writes nothing.
    batch.push_effect(HostOp::Terminal(TermOp::Detach {
        win,
        module: entry.module.clone(),
        mv: id,
    }));
    rearm(w, p);
}

/// The host froze `module` for move `mv` and gave its record and handoff: `move` goes out with the handoff, unread
/// here. The answer is matched to its move by `mv`, never by the module alone: a move that expired while frozen and
/// a retry of the same module may both be waiting for a freeze, and the first one's late answer carries a snapshot
/// taken before the terminal read again.
#[allow(clippy::too_many_arguments)]
pub fn detached(
    w: &mut Window,
    win: WinId,
    module: &ModuleId,
    mv: MoveId,
    record: serde_json::Value,
    handoff: Handoff,
    batch: &mut Batch,
) {
    let stage = w
        .peer
        .as_ref()
        .and_then(|p| p.entries.get(&mv))
        .filter(|e| e.role == Role::Source && e.module == *module)
        .map(|e| e.stage);
    match stage {
        Some(Stage::Freezing) => {}
        Some(Stage::Sent) => {
            // A repeated freeze answer: the other window may already hold the descriptors, so this window must not
            // read them again; only that move's end decides.
            w.warn(format!(
                "[move] {module}: a second freeze answer for move {}, which is sent; ignored",
                mv.0
            ));
            return;
        }
        _ => {
            let frozen_for = w
                .peer
                .as_ref()
                .and_then(|p| p.entry_of(Role::Source, module, &[Stage::Freezing, Stage::Sent]));
            if let Some(current) = frozen_for {
                // Another move of the module has it frozen or sent: reading it again here would let both windows
                // run one session. The stale answer's handoff is dropped unread.
                w.warn(format!(
                    "[move] {module}: a freeze answer for move {}, which has ended, while move {} holds it; ignored",
                    mv.0, current.0
                ));
                return;
            }
            // A freeze no move waits for any more: the terminal reads again, and the handoff is dropped unread.
            w.warn(format!(
                "[move] {module}: froze for move {}, which has ended; resumed",
                mv.0
            ));
            batch.push_effect(HostOp::Terminal(TermOp::Resume {
                win,
                module: module.clone(),
            }));
            return;
        }
    }
    let id = mv;
    with_peer(w, |w, p| {
        match p.port.send(Msg::Move { mv: id.0, record }, Some(handoff)) {
            Ok(()) => {
                let now = w.now();
                if p.moves.sent(id, now) {
                    if let Some(entry) = p.entries.get_mut(&id) {
                        entry.stage = Stage::Sent;
                        entry.deadline = Some(now + MOVE_DEADLINE);
                    }
                    w.say(format!("[move] {module}: sent as move {}", id.0));
                }
            }
            Err(err) => give_up(w, p, win, id, err, batch),
        }
        rearm(w, p);
    });
}

/// The host could not freeze `module`: its move is given up before anything left.
pub fn detach_refused(w: &mut Window, win: WinId, module: &ModuleId, why: String, batch: &mut Batch) {
    with_peer(w, |w, p| {
        if let Some(id) = p.entry_of(Role::Source, module, &[Stage::Freezing]) {
            give_up(w, p, win, id, why, batch);
            rearm(w, p);
        }
    });
}

/// The source gives a move up by itself, only ever before `move` left: the destination is told so it drops what it
/// prepared, and a frozen terminal reads again.
fn give_up(w: &mut Window, p: &mut PeerMoves, win: WinId, id: MoveId, why: String, batch: &mut Batch) {
    p.moves.aborted(id, &why);
    send(
        w,
        p,
        Msg::MoveAbort {
            mv: id.0,
            why: why.clone(),
        },
    );
    if let Some(entry) = p.entries.remove(&id) {
        if entry.stage != Stage::Offered {
            batch.push_effect(HostOp::Terminal(TermOp::Resume {
                win,
                module: entry.module,
            }));
        }
    }
    refuse(w, win, &why, batch);
}

/// What `Moves` said of the source's moves, carried out.
fn verdicts(w: &mut Window, p: &mut PeerMoves, win: WinId, verdicts: Vec<(MoveId, SourceVerdict)>, batch: &mut Batch) {
    for (id, verdict) in verdicts {
        verdict_for(w, p, win, id, verdict, batch);
    }
}

fn verdict_for(w: &mut Window, p: &mut PeerMoves, win: WinId, id: MoveId, verdict: SourceVerdict, batch: &mut Batch) {
    match verdict {
        SourceVerdict::Committed => committed(w, p, win, id, batch),
        SourceVerdict::Resume(why) => {
            let Some(entry) = p.entries.remove(&id) else {
                return;
            };
            // Only offered, nothing was frozen and nothing is to resume.
            if entry.stage != Stage::Offered {
                batch.push_effect(HostOp::Terminal(TermOp::Resume {
                    win,
                    module: entry.module.clone(),
                }));
            }
            w.say(format!("[move] {}: stays here: {why}", entry.module));
            w.refuse(win, why, batch);
        }
        SourceVerdict::NotAnswering => {
            let text = p.not_answering();
            let Some(entry) = p.entries.get_mut(&id) else {
                return;
            };
            // Said once; the module stays frozen, since only the other window or the link's end decides a sent move.
            entry.deadline = None;
            let module = entry.module.clone();
            w.warn(format!("[move] {module}: {text}"));
            chrome::show_toast(w, text, ToastKind::Other);
        }
    }
}

/// The destination holds the module: the local copy goes, its leaf stays as an away placeholder in every workspace,
/// and the keys, if it held them, go first to the module the layout gives them.
fn committed(w: &mut Window, p: &mut PeerMoves, win: WinId, id: MoveId, batch: &mut Batch) {
    let Some(entry) = p.entries.remove(&id) else {
        return;
    };
    let module = entry.module;
    if entry.kind == MoveKind::Terminal {
        batch.push_effect(HostOp::Terminal(TermOp::Forget {
            win,
            module: module.clone(),
        }));
    }
    let layout = w.layout.clone();
    let next = match layout.try_borrow_mut() {
        Ok(mut l) => {
            let had_keys = *l.focus() == module;
            if let Err(err) = l.set_away(&module, true) {
                w.warn(format!("[move] BUG: {module} could not be marked away: {err}"));
            }
            had_keys.then(|| l.focus().clone()).filter(|next| *next != module)
        }
        Err(_) => {
            w.warn(format!("[move] BUG: the layout was borrowed when {module} left"));
            None
        }
    };
    let workspaces = w.workspaces.clone();
    match workspaces.try_borrow_mut() {
        Ok(mut workspaces) => workspaces.set_away_everywhere(&module, true),
        Err(_) => w.warn(format!("[move] BUG: the workspaces were borrowed when {module} left")),
    }
    let focus = next.map(|next| {
        let tail = arrive_tail(win, &next, false);
        (next, tail)
    });
    w.changed_with_focus(win, focus, batch);
    w.say(format!("[move] {module}: in the other window now"));
    if entry.follow {
        batch.push_effect(HostOp::RaisePeer { win, peer: p.peer });
        p.raises.push_back(FocusTarget::Module { id: module.to_string() });
    }
}

// The destination's side.

/// The other window offers a module: accepted and laid out off screen at its size and shape, or refused.
#[allow(clippy::too_many_arguments)]
fn offered(
    w: &mut Window,
    p: &mut PeerMoves,
    win: WinId,
    id: MoveId,
    module: ModuleId,
    kind: MoveKind,
    shape: Shape,
    size: (u32, u32),
    batch: &mut Batch,
) {
    if kind != MoveKind::Terminal {
        send(
            w,
            p,
            Msg::MoveRefused {
                mv: id.0,
                why: feedback::NO_SECOND_WINDOW_TEXT.to_owned(),
            },
        );
        return;
    }
    if !p.moves.accept_offer(id) {
        send(
            w,
            p,
            Msg::MoveRefused {
                mv: id.0,
                why: feedback::NOT_AN_OFFER_TEXT.to_owned(),
            },
        );
        return;
    }
    let (layout, workspaces) = (w.layout.clone(), w.workspaces.clone());
    let (here_live, other_held) = match (layout.try_borrow(), workspaces.try_borrow()) {
        (Ok(layout), Ok(workspaces)) => {
            // Every name a new instance here could not take is one an arrival cannot take either: what any
            // workspace holds, what a later build's kept data names, what is being made and what is arriving.
            let mut live = BTreeSet::new();
            let mut away = BTreeSet::new();
            for (_, layout) in workspaces.each(&layout) {
                let held_away = layout.away();
                for module in layout.modules() {
                    if held_away.contains(&module) {
                        away.insert(module);
                    } else {
                        live.insert(module);
                    }
                }
            }
            live.extend(workspaces.reserved());
            live.extend(w.modules.pending_ids());
            live.extend(p.arriving());
            (
                live.into_iter().collect::<Vec<_>>(),
                away.into_iter().collect::<Vec<_>>(),
            )
        }
        _ => {
            p.moves.source_aborted(id);
            send(
                w,
                p,
                Msg::MoveRefused {
                    mv: id.0,
                    why: feedback::LAYOUT_CHANGING_TEXT.to_owned(),
                },
            );
            return;
        }
    };
    let arrival = match arrival_id(&module, &here_live, &other_held) {
        Ok(arrival) => arrival,
        Err(err) => {
            // Taken back out: an accepted offer nothing was prepared for.
            p.moves.source_aborted(id);
            send(
                w,
                p,
                Msg::MoveRefused {
                    mv: id.0,
                    why: err.to_string(),
                },
            );
            return;
        }
    };
    w.say(format!("[move] {module}: arriving as {arrival} (move {})", id.0));
    p.entries.insert(
        id,
        Entry {
            module: arrival.clone(),
            offered: module,
            kind,
            role: Role::Destination,
            follow: false,
            stage: Stage::Accepted,
            shape,
            record: serde_json::Value::Null,
            fds: None,
            deadline: None,
        },
    );
    // Made before it is mapped: the module is laid out at its final size before the source freezes anything.
    batch.push_layout(HostOp::PrepareSurface {
        win,
        module: arrival,
        shape,
        size,
    });
}

/// The host has the arriving module's place ready: the source may freeze it.
pub fn prepared(w: &mut Window, module: &ModuleId) {
    with_peer(w, |w, p| {
        let Some(id) = p.entry_of(Role::Destination, module, &[Stage::Accepted]) else {
            return;
        };
        if !p.moves.prepared(id) {
            return;
        }
        if let Some(entry) = p.entries.get_mut(&id) {
            entry.stage = Stage::Prepared;
        }
        send(w, p, Msg::MoveReady { mv: id.0 });
    });
}

/// The host could not prepare the arriving module's place: the offer is refused, and the source, which froze
/// nothing yet, keeps it.
pub fn prepare_refused(w: &mut Window, module: &ModuleId, why: String) {
    with_peer(w, |w, p| {
        let Some(id) = p.entry_of(Role::Destination, module, &[Stage::Accepted]) else {
            return;
        };
        // An accepted offer with no `move` yet is dropped as a source's abort drops it; a received one (none can be
        // here) would be closed by `refuse_received`.
        p.moves.refuse_received(id);
        p.moves.source_aborted(id);
        p.entries.remove(&id);
        w.say(format!("[move] {module}: its place could not be made: {why}"));
        send(w, p, Msg::MoveRefused { mv: id.0, why });
    });
}

/// `move` arrived: its descriptors held unread, and `move_done` queued with its confirmation kept.
fn received(
    w: &mut Window,
    p: &mut PeerMoves,
    win: WinId,
    id: MoveId,
    record: serde_json::Value,
    fds: Descriptors,
    batch: &mut Batch,
) {
    if p.entries.get(&id).is_some_and(|e| e.stage == Stage::Received) {
        // A second `move` for a move already received: the first is still the one being committed, so neither the
        // arrival nor the source's copy may be given up for it. Its descriptors are closed unread.
        let _ = p.moves.received(id, fds.take());
        w.warn(format!("[move] move {}: a second move for it was ignored", id.0));
        return;
    }
    if let Err(err) = p.moves.received(id, fds.take()) {
        send(
            w,
            p,
            Msg::MoveAbort {
                mv: id.0,
                why: err.to_string(),
            },
        );
        drop_arrival(w, p, win, id, batch);
        return;
    }
    let Some(entry) = p.entries.get_mut(&id) else {
        return;
    };
    entry.stage = Stage::Received;
    entry.record = record;
    entry.fds = Some(fds);
    match p.port.send_confirmed(Msg::MoveDone { mv: id.0 }) {
        Ok(answer) => {
            p.moves.done_queued(id, answer);
        }
        Err(err) => {
            // `move_done` never reached the link: the source resumes when the link ends.
            p.moves.done_sent(id, Err(io::Error::other(err.clone())));
            w.warn(format!("[move] move_done for move {} could not be sent: {err}", id.0));
            drop_arrival(w, p, win, id, batch);
        }
    }
}

/// The arrival of move `id` is given up: what was prepared for it goes.
fn drop_arrival(w: &mut Window, p: &mut PeerMoves, win: WinId, id: MoveId, batch: &mut Batch) {
    let Some(entry) = p.entries.remove(&id) else {
        return;
    };
    w.say(format!("[move] {}: its arrival was given up", entry.module));
    batch.push_layout(HostOp::DropSurface {
        win,
        module: entry.module,
        why: DropWhy::MoveGivenUp,
    });
}

/// The destination's moves `Moves` settled: each whose `move_done` is on the socket is committed and built from what
/// it carried; each it dropped has its place dropped. A move whose `move_done` is only queued is left as it is.
fn settle(w: &mut Window, p: &mut PeerMoves, cx: &Cx, win: WinId, batch: &mut Batch) {
    p.moves.settle();
    for id in p.moves.confirmed() {
        commit(w, p, cx, win, id, batch);
    }
    let gone: Vec<MoveId> = p
        .entries
        .iter()
        .filter(|(id, e)| e.role == Role::Destination && !p.moves.holds(Role::Destination, **id))
        .map(|(id, _)| *id)
        .collect();
    for id in gone {
        drop_arrival(w, p, win, id, batch);
    }
}

/// Move `id`'s descriptors handed over, and only then the module built from them and placed by its open rules (a
/// float where it floated) and mapped, the keys left where they are.
fn commit(w: &mut Window, p: &mut PeerMoves, cx: &Cx, win: WinId, id: MoveId, batch: &mut Batch) {
    let Ok(fds) = p.moves.commit(id) else {
        return;
    };
    let Some(entry) = p.entries.remove(&id) else {
        w.warn(format!("[move] BUG: move {} committed with no arrival", id.0));
        return;
    };
    let descriptors = entry.fds.unwrap_or_default();
    descriptors.put(fds);
    let arrival = entry.module.clone();
    batch.push_layout(HostOp::AdoptMoved {
        win,
        module: arrival.clone(),
        kind: entry.kind,
        record: entry.record,
        fds: descriptors,
    });
    place_arrival(w, cx, win, &arrival, entry.shape);
    // Only a terminal is accepted as an arrival (`offered`), so its surface is a terminal's.
    w.lifecycle.declare_moved(&SurfaceDecl {
        module: arrival.clone(),
        kind: SurfaceKind::Terminal {
            cwd: cx.setup.project_dir.clone(),
        },
        deferred: false,
    });
    w.changed_by_controller(win, batch);
    focus::modules_changed(w, win, batch);
    p.arrived.insert(entry.offered.clone(), arrival.clone());
    w.say(format!("[move] {}: arrived as {arrival}", entry.offered));
}

/// The arriving module put in the layout: back in its placeholder when it was here before, else where the open
/// rules put a new instance (a float at the rectangle it had), and held hidden by every other workspace.
fn place_arrival(w: &mut Window, cx: &Cx, win: WinId, arrival: &ModuleId, shape: Shape) {
    let layout = w.layout.clone();
    let Ok(mut l) = layout.try_borrow_mut() else {
        w.warn(format!("[move] BUG: the layout was borrowed when {arrival} arrived"));
        return;
    };
    if l.contains(arrival) && l.is_away(arrival) {
        let _ = l.set_away(arrival, false);
        drop(l);
        if let Ok(mut workspaces) = w.workspaces.try_borrow_mut() {
            workspaces.set_away_everywhere(arrival, false);
        }
        return;
    }
    let grid = cx.reads.grid_size(win);
    let keys = l.focus().clone();
    let placed = match shape {
        Shape::Float { x, y, w: fw, h: fh } => {
            let rect = FloatRect {
                cx: (x + fw / 2.0) as f32,
                cy: (y + fh / 2.0) as f32,
                w: (fw * grid.w as f64).round() as i32,
                h: (fh * grid.h as f64).round() as i32,
            }
            .clamped();
            l.add_float(arrival, rect, false).map(|()| l.unzoom()).map(|_| ())
        }
        Shape::Tiled => {
            let mut rule = cx.setup.rules.resolve(arrival, OpenPoint::NewInstance, None).0;
            // An arrival is shown, and the keys stay where they are until the other window gives them.
            rule.focus = false;
            rule.scratchpad = false;
            open_by_rule(&mut l, arrival, &rule, Some(&keys), grid).map(|_| ())
        }
    };
    if let Err(err) = placed {
        // The module exists now and is the only copy there is: it goes where nothing can refuse it.
        w.warn(format!(
            "[move] {arrival}: its place was refused ({err}); floated instead"
        ));
        let _ = l.add_float(arrival, FloatRect::centred(grid, DEFAULT_FLOAT_SHARE), false);
    }
    drop(l);
    let placement = declared_placement(&cx.setup.decls, arrival, Placement::BelowEditor);
    let workspaces = w.workspaces.clone();
    match workspaces.try_borrow_mut() {
        Ok(mut workspaces) => workspaces.add_everywhere_at(arrival, placement),
        Err(_) => w.warn(format!(
            "[move] BUG: the workspaces were borrowed when {arrival} was added to them"
        )),
    };
}

/// The host could not build the arriving module: it was committed and is gone, which the user is told.
pub fn adopt_refused(w: &mut Window, win: WinId, module: &ModuleId, why: String, batch: &mut Batch) {
    w.warn(format!("[move] {module}: could not be built here: {why}"));
    w.refuse(win, format!("{module} could not be built here: {why}"), batch);
}

/// The other window gives this window the keys: an arrival it names by the id it gave it, any module here by its
/// own id, or the module that last had them; with an activation token the window comes forward with it.
fn focus_intent(
    w: &mut Window,
    p: &PeerMoves,
    win: WinId,
    target: FocusTarget,
    token: Option<String>,
    batch: &mut Batch,
) {
    let to = match target {
        FocusTarget::Module { id } => match ModuleId::parse(&id) {
            Ok(named) => p.arrived.get(&named).cloned().unwrap_or(named),
            Err(err) => {
                w.warn(format!("[move] focus for {id:?}: {err}"));
                return;
            }
        },
        FocusTarget::Window => match w.layout.try_borrow() {
            Ok(layout) => layout.focus().clone(),
            Err(_) => return,
        },
        FocusTarget::Edge { side } => {
            w.warn(format!(
                "[move] focus from the other window's {side:?} edge is not decided here yet"
            ));
            return;
        }
    };
    if let Some(token) = token {
        batch.push_layout(HostOp::Present {
            win,
            activation: Some(ActivationToken(token)),
        });
    }
    if let Err(err) = focus::focus_and_arrive(w, win, &to, batch) {
        w.warn(format!("[move] focus for {to}: {err}"));
    }
}

// Both sides.

/// `window.other`: the other window raised, and given the keys once it is up.
pub fn raise_other(w: &mut Window, win: WinId, batch: &mut Batch) -> bool {
    with_peer(w, |_, p| {
        batch.push_effect(HostOp::RaisePeer { win, peer: p.peer });
        p.raises.push_back(FocusTarget::Window);
    })
    .is_some()
}

/// The host could not raise the other window: the focus it was for is not sent.
pub fn raise_refused(w: &mut Window) -> bool {
    w.peer.as_mut().is_some_and(|p| p.raises.pop_front().is_some())
}

/// One fact from the other window or the link, with the destination's moves settled before and after it.
pub fn peer_event(w: &mut Window, cx: &Cx, win: WinId, event: PeerEvent, batch: &mut Batch) {
    with_peer(w, |w, p| {
        // A confirmed `move_done` is committed before the fact is read: the other window may act on the module (a
        // focus intent for it) before this window was woken for the confirmation.
        settle(w, p, cx, win, batch);
        match event {
            PeerEvent::MoveOffer {
                id,
                module,
                kind,
                shape,
                size,
                ..
            } => offered(w, p, win, id, module, kind, shape, size, batch),
            PeerEvent::MoveReady { id, .. } => ready(w, p, win, id, batch),
            PeerEvent::MoveRefused { id, why, .. } => {
                if let Some(verdict) = p.moves.refused(id, &why) {
                    verdict_for(w, p, win, id, verdict, batch);
                }
            }
            PeerEvent::Move { id, record, fds, .. } => received(w, p, win, id, record, fds, batch),
            PeerEvent::MoveDone { id, .. } => {
                // A late `move_done` for a move already resumed is out of turn and answers nothing.
                if let Some(verdict) = p.moves.done(id) {
                    verdict_for(w, p, win, id, verdict, batch);
                }
            }
            PeerEvent::MoveAbort { id, why, .. } => match p.moves.peer_abort(id, &why) {
                PeerAbort::Destination(verdict) => verdict_for(w, p, win, id, verdict, batch),
                PeerAbort::Source => drop_arrival(w, p, win, id, batch),
                PeerAbort::Ignored => {}
            },
            PeerEvent::LinkClosed { .. } | PeerEvent::Bye { .. } => {
                let resumed = p.moves.link_closed();
                verdicts(w, p, win, resumed, batch);
                p.raises.clear();
            }
            PeerEvent::Raised { token, .. } => {
                let target = p.raises.pop_front().unwrap_or(FocusTarget::Window);
                send(w, p, Msg::Focus { target, token });
            }
            PeerEvent::Summary { summary, .. } => p.summary = Some(summary),
            PeerEvent::FocusIntent { target, token, .. } => focus_intent(w, p, win, target, token, batch),
            PeerEvent::Woke { .. } => {}
        }
        settle(w, p, cx, win, batch);
        rearm(w, p);
    });
}

/// The peer goes away: every move ends as the link's end ends it, and nothing is left of the link here.
pub fn detach(w: &mut Window, cx: &Cx, win: WinId, batch: &mut Batch) {
    let Some(peer) = w.peer.as_ref().map(PeerMoves::peer) else {
        return;
    };
    peer_event(w, cx, win, PeerEvent::LinkClosed { peer }, batch);
    if let Some(p) = w.peer.take() {
        if let Some((token, _)) = p.deadline {
            w.cancel_timer(token);
        }
    }
}

/// The deadline's timer, if `token` is it: the moves' deadlines judged now.
pub fn deadline_fired(w: &mut Window, win: WinId, token: TimerToken, batch: &mut Batch) -> bool {
    if w.peer
        .as_ref()
        .is_none_or(|p| p.deadline.map(|(t, _)| t) != Some(token))
    {
        return false;
    }
    with_peer(w, |w, p| {
        p.deadline = None;
        let now = w.now();
        let due = p.moves.tick(now);
        for (id, verdict) in &due {
            if let SourceVerdict::Resume(why) = verdict {
                // Only a move not yet sent expires: the other window may have prepared a place for it, which it drops
                // only when told.
                send(
                    w,
                    p,
                    Msg::MoveAbort {
                        mv: id.0,
                        why: why.clone(),
                    },
                );
            }
        }
        verdicts(w, p, win, due, batch);
        rearm(w, p);
    });
    true
}

/// The deadline's timer armed for the earliest deadline an open move has, or cancelled when none has.
fn rearm(w: &mut Window, p: &mut PeerMoves) {
    let earliest = p.entries.values().filter_map(|e| e.deadline).min();
    if p.deadline.map(|(_, at)| at) == earliest {
        return;
    }
    if let Some((token, _)) = p.deadline.take() {
        w.cancel_timer(token);
    }
    if let Some(at) = earliest {
        let delay = at.saturating_duration_since(w.now());
        let token = w.arm_after(delay);
        p.deadline = Some((token, at));
    }
}

#[cfg(test)]
mod tests;
