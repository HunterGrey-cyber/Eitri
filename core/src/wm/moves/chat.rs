//! A chat's own steps of a move, beside the steps every kind shares in `moves`.
//!
//! A chat moves one tab. A numbered view (`agent#N`) holds one tab and moves as a module, as a terminal does. The
//! base view (`agent`) is never away: its tab moves into the other window's base view, which is always there, so the
//! source keeps the module and marks nothing away, and the destination prepares no place for it. While the move is
//! under way the editor's review events of the session are held by the window that runs nvim, and delivered to
//! whichever window holds the session once the move has ended. The destination sends `move_done` only once its own
//! presence holds the arriving session's running turn, so at no moment does neither window count it. A chat whose
//! agent cannot keep a running turn across a move waits for its turn's end, with a note saying so.

use super::*;

/// A chat whose move waits for its turn's end.
#[derive(Debug, Clone)]
pub(super) struct Queued {
    module: ModuleId,
    view: ViewId,
    session: Option<String>,
    follow: bool,
}

/// The chat view of a chat module: the base view for `agent`, else its numbered view.
pub(super) fn view_of(module: &ModuleId) -> ViewId {
    ViewId::of_module(module).unwrap_or(ViewId::BASE)
}

/// Whether `entry` moves a tab of a base view, which stays where it is in both windows.
pub(super) fn is_base_tab(entry: &Entry) -> bool {
    entry.kind == MoveKind::Chat && entry.module == ModuleId::agent()
}

/// In the window that runs nvim, the editor's review events of the moving session are held from the offer on, so
/// a hunk the user reverts while the chat is frozen reaches whichever window ends up holding it.
pub(super) fn hold_review(w: &Window, win: WinId, entry: &mut Entry, batch: &mut Batch) {
    if entry.kind != MoveKind::Chat || entry.review_held || *w.presence() != EditorPresence::Owned {
        return;
    }
    let Some(session) = entry.session.clone() else {
        return;
    };
    entry.review_held = true;
    batch.push_effect(HostOp::Chat(ChatOp::ReviewHold {
        win,
        session,
        hold: true,
        here: true,
    }));
}

/// The move of `entry` has ended, however it ended: the held review events go to the session's holder, this window
/// when `here`.
pub(super) fn release_review(win: WinId, entry: &Entry, here: bool, batch: &mut Batch) {
    if !entry.review_held {
        return;
    }
    if let Some(session) = entry.session.clone() {
        batch.push_effect(HostOp::Chat(ChatOp::ReviewHold {
            win,
            session,
            hold: false,
            here,
        }));
    }
}

/// A chat asked to move while its turn runs, whose agent cannot keep the turn across a move: it is offered once the
/// turn ends, and the user is told so.
pub(super) fn queue(w: &mut Window, module: &ModuleId, facts: &ChatMoveFacts, follow: bool) {
    let Some(p) = w.peer.as_mut() else {
        return;
    };
    let view = view_of(module);
    let text = feedback::moves_at_turn_end_text(other_number(p.side));
    p.queued.retain(|q| !(q.view == view && q.session == facts.session));
    p.queued.push(Queued {
        module: module.clone(),
        view,
        session: facts.session.clone(),
        follow,
    });
    w.say(format!("[move] {module}: its turn runs; {text}"));
    chrome::show_toast(w, text, ToastKind::Other);
}

/// The turn of chat view `view` has ended: a move that waited for it is offered now.
pub fn turn_ended(w: &mut Window, cx: &Cx, win: WinId, view: ViewId, batch: &mut Batch) {
    let Some(p) = w.peer.as_mut() else {
        return;
    };
    let Some(at) = p.queued.iter().position(|q| q.view == view) else {
        return;
    };
    let queued = p.queued.remove(at);
    w.say(format!("[move] {}: its turn ended; offered now", queued.module));
    offer_tab(
        w,
        cx,
        win,
        &queued.module,
        queued.session.as_deref(),
        queued.follow,
        batch,
    );
}

/// The link has ended: no queued move can be offered any more.
pub(super) fn link_ended(w: &mut Window, p: &mut PeerMoves) {
    for queued in p.queued.drain(..) {
        w.say(format!(
            "[move] {}: its move waited for its turn's end; the other window is gone",
            queued.module
        ));
    }
}

/// The other window offers a tab of its base view, and this window has accepted the offer: the tab comes into this
/// window's base view, which is always here, so there is nothing to prepare and `move_ready` goes at once.
pub(super) fn base_tab_offered(
    w: &mut Window,
    p: &mut PeerMoves,
    win: WinId,
    id: MoveId,
    session: Option<String>,
    batch: &mut Batch,
) {
    let mut entry = Entry {
        module: ModuleId::agent(),
        offered: ModuleId::agent(),
        kind: MoveKind::Chat,
        role: Role::Destination,
        follow: false,
        stage: Stage::Accepted,
        shape: Shape::Tiled,
        record: serde_json::Value::Null,
        fds: None,
        deadline: None,
        silent: false,
        tab: None,
        session,
        review_held: false,
        presence_asked: false,
    };
    hold_review(w, win, &mut entry, batch);
    w.say(format!(
        "[move] a tab of the other window's base view arrives here (move {})",
        id.0
    ));
    p.entries.insert(id, entry);
    if p.moves.prepared(id) {
        if let Some(entry) = p.entries.get_mut(&id) {
            entry.stage = Stage::Prepared;
        }
        send(w, p, Msg::MoveReady { mv: id.0 });
    }
}

/// This window's presence answered for the session arriving with move `mv`: held, `move_done` goes; not held, the
/// arrival is given up and the other window keeps the chat.
pub fn presence_answered(w: &mut Window, win: WinId, mv: MoveId, held: Result<(), String>, batch: &mut Batch) {
    with_peer(w, |w, p| {
        let holding = p
            .entries
            .get(&mv)
            .is_some_and(|e| e.role == Role::Destination && e.stage == Stage::Holding);
        if !holding {
            w.warn(format!(
                "[move] a presence answer for move {}, which waits for none; ignored",
                mv.0
            ));
            return;
        }
        match held {
            Ok(()) => send_done(w, p, win, mv, batch),
            Err(why) => give_up_arrival(w, p, win, mv, why, batch),
        }
        rearm(w, p);
    });
}

/// The destination gives up an arrival it received and has not committed: what came is closed unread, and the
/// source, told, keeps the chat.
fn give_up_arrival(w: &mut Window, p: &mut PeerMoves, win: WinId, id: MoveId, why: String, batch: &mut Batch) {
    p.moves.refuse_received(id);
    send(
        w,
        p,
        Msg::MoveAbort {
            mv: id.0,
            why: why.clone(),
        },
    );
    w.warn(format!("[move] move {}: given up here: {why}", id.0));
    drop_arrival(w, p, win, id, batch);
}

/// Arrivals whose presence did not answer by their deadline are given up.
pub(super) fn holding_expired(w: &mut Window, p: &mut PeerMoves, win: WinId, now: Instant, batch: &mut Batch) {
    let due: Vec<MoveId> = p
        .entries
        .iter()
        .filter(|(_, e)| e.role == Role::Destination && e.stage == Stage::Holding)
        .filter(|(_, e)| e.deadline.is_some_and(|at| now >= at))
        .map(|(id, _)| *id)
        .collect();
    for id in due {
        give_up_arrival(w, p, win, id, feedback::PRESENCE_TOO_SLOW_TEXT.to_owned(), batch);
    }
}

/// What an arrival given up leaves to undo for a chat: the presence asked for it, and the held review events.
pub(super) fn arrival_given_up(win: WinId, id: MoveId, entry: &Entry, batch: &mut Batch) {
    if entry.presence_asked {
        if let Some(session) = entry.session.clone() {
            batch.push_effect(HostOp::Chat(ChatOp::DropPresence { win, session, mv: id }));
        }
    }
    // Given up here: the other window keeps the session.
    release_review(win, entry, false, batch);
}

/// The commit's adopt has been asked for: what the presence held for the arriving session is let go behind it. An
/// adopt that took the tab up has moved the hold onto the tab already, so this lets nothing go; one that failed
/// (an unreadable record, an agent that would not be taken up, a host that could not build it), or whose record
/// names another session than the offer did, would otherwise leave this window counting a turn no tab here runs,
/// and every revert of either window refused for as long as it lives. The drop is an effect, which the host
/// carries out after the batch's layout work, the adopt among it.
pub(super) fn presence_after_adopt(win: WinId, id: MoveId, entry: &Entry, batch: &mut Batch) {
    if !entry.presence_asked {
        return;
    }
    if let Some(session) = entry.session.clone() {
        batch.push_effect(HostOp::Chat(ChatOp::DropPresence { win, session, mv: id }));
    }
}

/// The chooser picked a session the other window holds: that window is asked to move it here, the keys following
/// it. False with no other window.
pub fn ask_session(w: &mut Window, session: String) -> bool {
    with_peer(w, |w, p| {
        let msg = Msg::MoveAsk {
            module: None,
            session: Some(session.clone()),
            follow: true,
        };
        match p.port.send(msg, None) {
            Ok(()) => w.say(format!(
                "[move] session {session:?}: the other window holds it; asked it to move it here"
            )),
            Err(err) => w.warn(format!("[move] session {session:?}: move_ask could not be sent: {err}")),
        }
    })
    .is_some()
}
