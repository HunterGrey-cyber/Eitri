//! The commit machine of a move between the two windows, for both ends, without GTK and with the clock passed in.
//!
//! A move commits at one point only: the destination's `move_done`, written to the socket before the destination
//! reads or acts for the module. Before the source sends `move`, a refusal, the link's end or a 10 s deadline gives
//! the module back to the source. After `move` no timer gives it back: the source learns the outcome from `move_done`
//! (the destination holds it), from `move_abort` (the destination read nothing and closed what it received), or from
//! the link's end-of-file without `move_done` (seqpacket delivers a peer's last packets before its end-of-file, so a
//! destination that committed has its `move_done` read first). A destination that answers none of these leaves the
//! module frozen, and the source is told once that the other window is not answering.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::OwnedFd;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};

use super::msg::LinkError;

/// How long a move may take before the source sends `move`, and how long after it the source waits before saying
/// that the other window is not answering.
pub const MOVE_DEADLINE: Duration = Duration::from_secs(10);

/// A move's number, chosen by its source; the destination answers with the same number. Window 1 numbers its moves
/// odd and window 2 even, so a number never names two moves going opposite ways, and a message that carries only the
/// number (`move_abort`, `swap`) still says which window offered the move.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MoveId(pub u64);

/// One of the two windows of a pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    /// The window that owns nvim and starts the other.
    One,
    /// The window started by window 1.
    Two,
}

impl MoveId {
    /// The window that offered this move. Zero is no window's number.
    pub fn source(self) -> Option<Window> {
        match self.0 {
            0 => None,
            n if n % 2 == 1 => Some(Window::One),
            _ => Some(Window::Two),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Source,
    Destination,
}

/// What the source does with a module it offered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceVerdict {
    /// The destination holds the module: drop the local copy without side effects.
    Committed,
    /// The move did not happen: unfreeze and read again, telling the user why.
    Resume(String),
    /// The destination has neither committed nor refused for a while after `move`; the module stays frozen.
    NotAnswering,
}

/// What a `move_abort` from the other window meant, told apart by the move's number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerAbort {
    /// The other window, as destination, gave back a move this window offered: the module resumes here.
    Destination(SourceVerdict),
    /// The other window, as source, gave up an offer this window accepted before sending `move`: what was prepared
    /// for it is dropped, and for a chat the session's held review events are released.
    Source,
    /// No move of that number is in a state an abort applies to; nothing changed.
    Ignored,
}

/// A destination's `commit` before its `move_done` is known to be on the socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotConfirmed;

#[derive(Debug)]
enum SourceState {
    Offered { deadline: Instant },
    Ready { deadline: Instant },
    Frozen { deadline: Instant },
    Sent { notice_at: Instant, noticed: bool },
}

#[derive(Debug)]
enum DestState {
    Accepted,
    Prepared,
    /// `move` arrived; its descriptors are held unread until the commit. `done` is the answer of the confirmed send
    /// that carries `move_done`, once it is queued.
    Received {
        fds: Vec<OwnedFd>,
        done: Option<Receiver<io::Result<()>>>,
    },
    /// `move_done` is on the socket: the descriptors may be handed over.
    Confirmed(Vec<OwnedFd>),
}

#[derive(Debug)]
pub struct Moves {
    me: Window,
    next: u64,
    source: BTreeMap<MoveId, SourceState>,
    destination: BTreeMap<MoveId, DestState>,
}

const LINK_ENDED: &str = "the other window closed";
const TOO_SLOW: &str = "the move took too long";

impl Moves {
    /// The moves of window `me`; it numbers the moves it offers from its own half of the numbers.
    pub fn new(me: Window) -> Moves {
        Moves {
            me,
            next: 0,
            source: BTreeMap::new(),
            destination: BTreeMap::new(),
        }
    }

    /// Whether this window has a move of that number in progress in that role.
    pub fn holds(&self, role: Role, id: MoveId) -> bool {
        match role {
            Role::Source => self.source.contains_key(&id),
            Role::Destination => self.destination.contains_key(&id),
        }
    }

    // The source's side.

    /// A new move this window offers; the caller sends `move_offer` with its number.
    pub fn offer(&mut self, now: Instant) -> MoveId {
        let id = MoveId(self.next * 2 + if self.me == Window::One { 1 } else { 2 });
        self.next += 1;
        self.source.insert(
            id,
            SourceState::Offered {
                deadline: now + MOVE_DEADLINE,
            },
        );
        id
    }

    /// The destination answered `move_ready`. False when the move is not waiting for that answer.
    pub fn ready(&mut self, id: MoveId) -> bool {
        let Some(state) = self.source.get_mut(&id) else {
            return false;
        };
        let SourceState::Offered { deadline } = *state else {
            return false;
        };
        *state = SourceState::Ready { deadline };
        true
    }

    /// The destination answered `move_refused`. Before `move` the source resumes; after it, the destination's
    /// refusal means nothing (only `move_abort` gives the module back) and the module stays frozen.
    pub fn refused(&mut self, id: MoveId, why: &str) -> Option<SourceVerdict> {
        match self.source.get(&id) {
            Some(SourceState::Sent { .. }) | None => None,
            Some(_) => {
                self.source.remove(&id);
                Some(SourceVerdict::Resume(why.to_owned()))
            }
        }
    }

    /// The module is frozen and the source is about to take its record. False when the destination is not ready.
    pub fn freeze(&mut self, id: MoveId, now: Instant) -> bool {
        match self.source.get_mut(&id) {
            Some(state @ SourceState::Ready { .. }) => {
                *state = SourceState::Frozen {
                    deadline: now + MOVE_DEADLINE,
                };
                true
            }
            _ => false,
        }
    }

    /// `move` went out with the module's descriptors: from here no deadline gives the module back. False when the
    /// module was not frozen.
    pub fn sent(&mut self, id: MoveId, now: Instant) -> bool {
        match self.source.get_mut(&id) {
            Some(state @ SourceState::Frozen { .. }) => {
                *state = SourceState::Sent {
                    notice_at: now + MOVE_DEADLINE,
                    noticed: false,
                };
                true
            }
            _ => false,
        }
    }

    /// The destination's `move_done`. Only a move already sent can be committed; a `move_done` for anything else
    /// is out of turn and changes nothing.
    pub fn done(&mut self, id: MoveId) -> Option<SourceVerdict> {
        match self.source.get(&id) {
            Some(SourceState::Sent { .. }) => {
                self.source.remove(&id);
                Some(SourceVerdict::Committed)
            }
            _ => None,
        }
    }

    /// A `move_abort` from the other window, routed by the number's half: a move this window offered was given back
    /// by its destination ([`Moves::aborted`]), or one this window accepted was given up by its source
    /// ([`Moves::source_aborted`]).
    pub fn peer_abort(&mut self, id: MoveId, why: &str) -> PeerAbort {
        if id.source() == Some(self.me) {
            match self.aborted(id, why) {
                Some(verdict) => PeerAbort::Destination(verdict),
                None => PeerAbort::Ignored,
            }
        } else if self.source_aborted(id) {
            PeerAbort::Source
        } else {
            PeerAbort::Ignored
        }
    }

    /// The destination's `move_abort`: it read nothing and closed what it received, so the source resumes.
    pub fn aborted(&mut self, id: MoveId, why: &str) -> Option<SourceVerdict> {
        self.source.remove(&id).map(|_| SourceVerdict::Resume(why.to_owned()))
    }

    /// The link ended. Every move this window offered and the destination did not commit resumes; every move this
    /// window was receiving is handled as [`Moves::link_closing`] says (dropped unless its `move_done` was written).
    pub fn link_closed(&mut self) -> Vec<(MoveId, SourceVerdict)> {
        self.link_closing();
        std::mem::take(&mut self.source)
            .into_keys()
            .map(|id| (id, SourceVerdict::Resume(LINK_ENDED.to_owned())))
            .collect()
    }

    /// Deadlines: a move not yet sent resumes 10 s after its offer or its freeze; a sent move is reported as not
    /// answering once, 10 s after `move`, and stays frozen.
    pub fn tick(&mut self, now: Instant) -> Vec<(MoveId, SourceVerdict)> {
        let mut out = Vec::new();
        let mut expired = Vec::new();
        for (id, state) in self.source.iter_mut() {
            match state {
                SourceState::Offered { deadline }
                | SourceState::Ready { deadline }
                | SourceState::Frozen { deadline } => {
                    if now >= *deadline {
                        expired.push(*id);
                    }
                }
                SourceState::Sent { notice_at, noticed } => {
                    if !*noticed && now >= *notice_at {
                        *noticed = true;
                        out.push((*id, SourceVerdict::NotAnswering));
                    }
                }
            }
        }
        for id in expired {
            self.source.remove(&id);
            out.push((id, SourceVerdict::Resume(TOO_SLOW.to_owned())));
        }
        out.sort_by_key(|(id, _)| *id);
        out
    }

    // The destination's side.

    /// This window takes the offer `id` and starts preparing. False, and nothing taken, for a number that is not the
    /// other window's or an offer already taken: the caller refuses it.
    pub fn accept_offer(&mut self, id: MoveId) -> bool {
        let theirs = matches!(id.source(), Some(window) if window != self.me);
        if !theirs || self.destination.contains_key(&id) {
            return false;
        }
        self.destination.insert(id, DestState::Accepted);
        true
    }

    /// The source's `move_abort` for an offer this window accepted: the source sends it only before `move`, so an
    /// accepted or prepared move is dropped (true; the caller drops what it prepared and, for a chat, releases the
    /// session's held review events). A move already received is out of turn for it and is kept.
    pub fn source_aborted(&mut self, id: MoveId) -> bool {
        match self.destination.get(&id) {
            Some(DestState::Accepted | DestState::Prepared) => {
                self.destination.remove(&id);
                true
            }
            _ => false,
        }
    }

    /// The module's place is ready; the caller sends `move_ready`. False for an offer this window did not accept.
    pub fn prepared(&mut self, id: MoveId) -> bool {
        match self.destination.get_mut(&id) {
            Some(state @ DestState::Accepted) => {
                *state = DestState::Prepared;
                true
            }
            _ => false,
        }
    }

    /// `move` arrived with the module's descriptors. They are held unread until [`Moves::commit`]. A move this window
    /// did not prepare, or one that already arrived, is refused and its descriptors are closed.
    pub fn received(&mut self, id: MoveId, fds: Vec<OwnedFd>) -> Result<(), LinkError> {
        match self.destination.get_mut(&id) {
            Some(state @ DestState::Prepared) => {
                *state = DestState::Received { fds, done: None };
                Ok(())
            }
            _ => {
                drop(fds);
                Err(LinkError::Malformed(format!(
                    "move {}: not an offer this window prepared",
                    id.0
                )))
            }
        }
    }

    /// The answer of the confirmed send that carried `move_done`. Written: the move may commit. Not written: the
    /// destination drops the move and closes its descriptors; the source resumes when the link ends.
    pub fn done_sent(&mut self, id: MoveId, confirmed: io::Result<()>) {
        let Some(state) = self.destination.remove(&id) else {
            return;
        };
        match (state, confirmed) {
            (DestState::Received { fds, .. }, Ok(())) => {
                self.destination.insert(id, DestState::Confirmed(fds));
            }
            (DestState::Received { fds, .. }, Err(_)) => drop(fds),
            (other, _) => {
                self.destination.insert(id, other);
            }
        }
    }

    /// `move_done` is queued: `answer` is the receiver [`LinkHandle::send_confirmed`] returned for it. The machine
    /// keeps it and settles the move when the answer arrives ([`Moves::settle`]), so a link that ends before the
    /// caller looked at the answer still knows whether `move_done` reached the socket. False when the move is not
    /// one this window received and has not settled.
    ///
    /// [`LinkHandle::send_confirmed`]: super::link::LinkHandle::send_confirmed
    pub fn done_queued(&mut self, id: MoveId, answer: Receiver<io::Result<()>>) -> bool {
        match self.destination.get_mut(&id) {
            Some(DestState::Received { done, .. }) if done.is_none() => {
                *done = Some(answer);
                true
            }
            _ => false,
        }
    }

    /// Takes every `move_done` answer that has arrived: a written one makes its move ready to commit (returned, in
    /// order), a failed one, or one whose link thread is gone without answering, drops its move and closes its
    /// descriptors. A move whose answer has not arrived is left as it is.
    pub fn settle(&mut self) -> Vec<MoveId> {
        let waiting: Vec<MoveId> = self
            .destination
            .iter()
            .filter(|(_, state)| matches!(state, DestState::Received { done: Some(_), .. }))
            .map(|(id, _)| *id)
            .collect();
        waiting.into_iter().filter(|id| self.settle_one(*id)).collect()
    }

    /// Settles one move from its queued answer; true when it became ready to commit.
    fn settle_one(&mut self, id: MoveId) -> bool {
        let answer = match self.destination.get(&id) {
            Some(DestState::Received { done: Some(done), .. }) => match done.try_recv() {
                Ok(answer) => answer,
                Err(TryRecvError::Empty) => return false,
                Err(TryRecvError::Disconnected) => Err(io::Error::from(io::ErrorKind::NotConnected)),
            },
            _ => return false,
        };
        let written = answer.is_ok();
        self.done_sent(id, answer);
        written
    }

    /// The module's descriptors, once `move_done` is known to be on the socket. Before that it refuses and keeps
    /// them, so nothing acts on a commit the source might never read.
    pub fn commit(&mut self, id: MoveId) -> Result<Vec<OwnedFd>, NotConfirmed> {
        self.settle_one(id);
        match self.destination.remove(&id) {
            Some(DestState::Confirmed(fds)) => Ok(fds),
            Some(other) => {
                self.destination.insert(id, other);
                Err(NotConfirmed)
            }
            None => Err(NotConfirmed),
        }
    }

    /// The destination gives up on a move it received but did not commit: its descriptors are closed unread. The
    /// caller then sends `move_abort`.
    pub fn refuse_received(&mut self, id: MoveId) {
        if matches!(self.destination.get(&id), Some(DestState::Received { .. })) {
            self.destination.remove(&id);
        }
    }

    /// The link has ended or is about to: every move this window is receiving whose `move_done` is not known to be
    /// on the socket is dropped, its descriptors closed, so none outlives the link half-moved. A move whose
    /// `move_done` was written stays for [`Moves::commit`]: the source reads that `move_done` before the end-of-file
    /// and drops its copy, so this window is the module's only holder.
    ///
    /// Answers queued with [`Moves::done_queued`] are settled first. One passed by hand to [`Moves::done_sent`]
    /// must be passed before this is called: the link thread answers every written send before it passes on
    /// [`Inbound::Closed`](super::link::Inbound::Closed), and a closing link answers every send before its thread is
    /// joined, so an answer still missing here is one that was not written.
    pub fn link_closing(&mut self) {
        self.settle();
        self.destination
            .retain(|_, state| matches!(state, DestState::Confirmed(_)));
    }

    /// The moves this window received whose `move_done` is on the socket and that it has not committed yet.
    pub fn confirmed(&self) -> Vec<MoveId> {
        self.destination
            .iter()
            .filter(|(_, state)| matches!(state, DestState::Confirmed(_)))
            .map(|(id, _)| *id)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{PipeWriter, Write};

    fn pipe() -> (OwnedFd, PipeWriter) {
        let (r, w) = std::io::pipe().unwrap();
        (r.into(), w)
    }

    /// Whether the read end of the pipe whose write end this is has been closed everywhere. A child that another
    /// test is starting holds a copy of every descriptor of this process until it execs, so the end is waited for.
    fn reader_gone(w: &mut PipeWriter) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match w.write(b"x") {
                Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return true,
                _ if std::time::Instant::now() >= deadline => return false,
                _ => std::thread::sleep(std::time::Duration::from_millis(5)),
            }
        }
    }

    /// Whether the read end is still open somewhere: a write succeeds at once.
    fn reader_open(w: &mut PipeWriter) -> bool {
        w.write(b"x").is_ok()
    }

    fn sent_move(moves: &mut Moves, t0: Instant) -> MoveId {
        let id = moves.offer(t0);
        assert!(moves.ready(id));
        assert!(moves.freeze(id, t0));
        assert!(moves.sent(id, t0));
        id
    }

    /// Window 1's `n`th move, as window 2 (the destination in these tests) receives it.
    fn w1(n: u64) -> MoveId {
        MoveId(2 * n - 1)
    }

    fn received_move(moves: &mut Moves, id: MoveId) -> PipeWriter {
        let (r, w) = pipe();
        assert!(moves.accept_offer(id));
        assert!(moves.prepared(id));
        moves.received(id, vec![r]).unwrap();
        w
    }

    #[test]
    fn a_move_never_answered_after_it_was_sent_stays_frozen_past_any_deadline() {
        let t0 = Instant::now();
        let mut moves = Moves::new(Window::Two);
        let id = sent_move(&mut moves, t0);
        assert!(moves.tick(t0 + Duration::from_secs(9)).is_empty());
        assert_eq!(
            moves.tick(t0 + Duration::from_secs(10)),
            vec![(id, SourceVerdict::NotAnswering)]
        );
        assert!(
            moves.tick(t0 + Duration::from_secs(11)).is_empty(),
            "the notice comes once"
        );
        assert!(
            moves.tick(t0 + Duration::from_secs(3600)).is_empty(),
            "no timer resumes after move"
        );
        assert!(moves.holds(Role::Source, id), "the module stays frozen");
        assert_eq!(
            moves.done(id),
            Some(SourceVerdict::Committed),
            "a late commit still counts"
        );
    }

    #[test]
    fn before_move_a_refusal_a_link_end_or_ten_seconds_abort_and_resume() {
        let t0 = Instant::now();
        let mut moves = Moves::new(Window::Two);

        let refused = moves.offer(t0);
        assert_eq!(
            moves.refused(refused, "no room"),
            Some(SourceVerdict::Resume("no room".into()))
        );
        assert!(!moves.holds(Role::Source, refused));

        let slow_offer = moves.offer(t0);
        let slow_ready = moves.offer(t0);
        assert!(moves.ready(slow_ready));
        let slow_frozen = moves.offer(t0);
        assert!(moves.ready(slow_frozen));
        assert!(moves.freeze(slow_frozen, t0 + Duration::from_secs(5)));
        let verdicts = moves.tick(t0 + Duration::from_secs(10));
        let ids: Vec<MoveId> = verdicts.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![slow_offer, slow_ready], "a freeze starts its own ten seconds");
        assert!(verdicts.iter().all(|(_, v)| matches!(v, SourceVerdict::Resume(_))));
        assert_eq!(
            moves.tick(t0 + Duration::from_secs(15)),
            vec![(slow_frozen, SourceVerdict::Resume(TOO_SLOW.into()))]
        );

        let ended = moves.offer(t0);
        assert!(moves.ready(ended));
        assert_eq!(
            moves.link_closed(),
            vec![(ended, SourceVerdict::Resume(LINK_ENDED.into()))]
        );

        let after = sent_move(&mut moves, t0);
        assert_eq!(
            moves.refused(after, "late"),
            None,
            "a refusal after move gives nothing back"
        );
        assert!(moves.holds(Role::Source, after));
    }

    #[test]
    fn an_end_of_file_after_move_done_was_read_is_a_commit() {
        let t0 = Instant::now();
        let mut moves = Moves::new(Window::Two);
        let id = sent_move(&mut moves, t0);
        assert_eq!(moves.done(id), Some(SourceVerdict::Committed));
        assert!(
            moves.link_closed().is_empty(),
            "nothing resumes after the commit was read"
        );
        assert_eq!(moves.done(id), None, "committed once");
    }

    #[test]
    fn an_end_of_file_without_move_done_resumes_the_source() {
        let t0 = Instant::now();
        let mut moves = Moves::new(Window::Two);
        let id = sent_move(&mut moves, t0);
        assert_eq!(
            moves.link_closed(),
            vec![(id, SourceVerdict::Resume(LINK_ENDED.into()))]
        );
        assert_eq!(moves.done(id), None);
    }

    #[test]
    fn a_destination_abort_after_move_resumes_the_source() {
        let t0 = Instant::now();
        let mut moves = Moves::new(Window::Two);
        let id = sent_move(&mut moves, t0);
        assert_eq!(
            moves.aborted(id, "could not take it"),
            Some(SourceVerdict::Resume("could not take it".into()))
        );
        assert!(moves.link_closed().is_empty());
    }

    #[test]
    fn out_of_turn_answers_change_nothing() {
        let t0 = Instant::now();
        let mut moves = Moves::new(Window::Two);
        let id = moves.offer(t0);
        assert!(!moves.freeze(id, t0), "not ready yet");
        assert!(!moves.sent(id, t0), "not frozen yet");
        assert_eq!(moves.done(id), None, "nothing was sent to commit");
        assert!(moves.ready(id));
        assert!(!moves.ready(id));
        assert!(moves.holds(Role::Source, id));
        assert!(!moves.holds(Role::Destination, id));
    }

    #[test]
    fn a_move_for_no_accepted_offer_is_refused_and_its_descriptors_closed() {
        let mut moves = Moves::new(Window::Two);
        let (r, mut w) = pipe();
        assert!(moves.received(w1(9), vec![r]).is_err());
        assert!(reader_gone(&mut w), "the descriptor of an unknown move is closed");

        assert!(moves.accept_offer(w1(3)));
        let (r, mut w) = pipe();
        assert!(moves.received(w1(3), vec![r]).is_err(), "accepted but not prepared");
        assert!(reader_gone(&mut w));

        let mut first = received_move(&mut moves, w1(4));
        let (r, mut w) = pipe();
        assert!(moves.received(w1(4), vec![r]).is_err(), "a second move of one number");
        assert!(reader_gone(&mut w));
        assert!(reader_open(&mut first), "the first delivery is kept");
    }

    #[test]
    fn the_destination_closes_received_descriptors_of_uncommitted_moves_before_the_link_goes() {
        let mut moves = Moves::new(Window::Two);
        let mut uncommitted = received_move(&mut moves, w1(1));
        let mut confirmed = received_move(&mut moves, w1(2));
        moves.done_sent(w1(2), Ok(()));
        let mut committed = received_move(&mut moves, w1(3));
        moves.done_sent(w1(3), Ok(()));
        let kept = moves.commit(w1(3)).unwrap();
        moves.link_closing();
        assert!(reader_gone(&mut uncommitted));
        assert!(!moves.holds(Role::Destination, w1(1)));
        assert!(
            reader_open(&mut committed),
            "a committed move's descriptors belong to the module"
        );
        drop(kept);
        assert!(
            reader_open(&mut confirmed),
            "the source drops its copy once move_done is written, so this window keeps the module"
        );
        assert_eq!(moves.confirmed(), vec![w1(2)]);
        assert_eq!(
            moves.commit(w1(2)).unwrap().len(),
            1,
            "still handed over after the link's end"
        );
    }

    #[test]
    fn a_move_whose_move_done_was_written_survives_the_peers_end_of_file() {
        let t0 = Instant::now();
        let mut moves = Moves::new(Window::Two);
        let mut written = received_move(&mut moves, w1(7));
        moves.done_sent(w1(7), Ok(()));
        let mut unwritten = received_move(&mut moves, w1(8));
        let offered = moves.offer(t0);
        let verdicts = moves.link_closed();
        assert!(
            verdicts.iter().all(|(id, _)| *id == offered),
            "only this window's own offer resumes: {verdicts:?}"
        );
        assert!(
            reader_gone(&mut unwritten),
            "a move without a written move_done is dropped"
        );
        assert!(reader_open(&mut written));
        assert_eq!(moves.commit(w1(7)).unwrap().len(), 1);
    }

    #[test]
    fn a_queued_move_done_settles_from_its_answer() {
        let mut moves = Moves::new(Window::Two);
        let (ok_tx, ok_rx) = std::sync::mpsc::channel();
        let mut ok = received_move(&mut moves, w1(1));
        assert!(moves.done_queued(w1(1), ok_rx));
        let (_err_tx_kept, second) = std::sync::mpsc::channel::<io::Result<()>>();
        assert!(!moves.done_queued(w1(1), second), "one answer per move");
        assert_eq!(moves.commit(w1(1)).unwrap_err(), NotConfirmed, "no answer yet");
        assert!(moves.settle().is_empty());
        ok_tx.send(Ok(())).unwrap();
        assert_eq!(moves.settle(), vec![w1(1)]);
        assert!(reader_open(&mut ok));
        assert_eq!(moves.commit(w1(1)).unwrap().len(), 1);

        let (err_tx, err_rx) = std::sync::mpsc::channel();
        let mut failed = received_move(&mut moves, w1(2));
        assert!(moves.done_queued(w1(2), err_rx));
        err_tx.send(Err(io::Error::from(io::ErrorKind::BrokenPipe))).unwrap();
        assert_eq!(moves.commit(w1(2)).unwrap_err(), NotConfirmed);
        assert!(reader_gone(&mut failed), "an unwritten move_done drops the move");

        let (gone_tx, gone_rx) = std::sync::mpsc::channel::<io::Result<()>>();
        let mut orphaned = received_move(&mut moves, w1(3));
        assert!(moves.done_queued(w1(3), gone_rx));
        drop(gone_tx);
        assert!(moves.settle().is_empty());
        assert!(
            reader_gone(&mut orphaned),
            "a link thread gone without answering wrote nothing"
        );

        // At the link's end, an answer that arrived but was never looked at still counts, and a missing one does not.
        let (late_tx, late_rx) = std::sync::mpsc::channel();
        let mut answered = received_move(&mut moves, w1(4));
        assert!(moves.done_queued(w1(4), late_rx));
        late_tx.send(Ok(())).unwrap();
        let (_silent_tx, silent_rx) = std::sync::mpsc::channel::<io::Result<()>>();
        let mut silent = received_move(&mut moves, w1(5));
        assert!(moves.done_queued(w1(5), silent_rx));
        moves.link_closing();
        assert!(reader_open(&mut answered));
        assert!(reader_gone(&mut silent));
        assert_eq!(moves.confirmed(), vec![w1(4)]);
    }

    #[test]
    fn commit_refuses_until_move_done_was_confirmed_written() {
        let mut moves = Moves::new(Window::Two);
        let mut w = received_move(&mut moves, w1(1));
        assert_eq!(moves.commit(w1(1)).unwrap_err(), NotConfirmed);
        assert!(reader_open(&mut w), "a refused commit keeps the descriptors");
        moves.done_sent(w1(1), Ok(()));
        let fds = moves.commit(w1(1)).unwrap();
        assert_eq!(fds.len(), 1);
        assert_eq!(moves.commit(w1(1)).unwrap_err(), NotConfirmed, "handed over once");

        let mut w = received_move(&mut moves, w1(2));
        moves.done_sent(w1(2), Err(io::Error::from(io::ErrorKind::BrokenPipe)));
        assert_eq!(moves.commit(w1(2)).unwrap_err(), NotConfirmed);
        assert!(reader_gone(&mut w), "an unwritten commit drops the move");
        assert!(!moves.holds(Role::Destination, w1(2)));

        let mut w = received_move(&mut moves, w1(3));
        moves.refuse_received(w1(3));
        assert!(reader_gone(&mut w), "a refused move is closed unread");
        assert_eq!(moves.commit(w1(3)).unwrap_err(), NotConfirmed);
    }

    #[test]
    fn the_two_windows_number_their_moves_apart() {
        let t0 = Instant::now();
        let mut one = Moves::new(Window::One);
        let mut two = Moves::new(Window::Two);
        let a: Vec<MoveId> = (0..3).map(|_| one.offer(t0)).collect();
        let b: Vec<MoveId> = (0..3).map(|_| two.offer(t0)).collect();
        assert!(a.iter().all(|id| !b.contains(id)), "{a:?} {b:?}");
        assert!(a.iter().all(|id| id.source() == Some(Window::One)));
        assert!(b.iter().all(|id| id.source() == Some(Window::Two)));
        assert!(!one.accept_offer(a[0]), "a window's own number is not an offer to it");
        assert!(!one.accept_offer(MoveId(0)), "zero is no window's number");
        assert!(one.accept_offer(b[0]));
        assert!(!one.accept_offer(b[0]), "one offer of one number");
    }

    #[test]
    fn a_sources_abort_drops_the_destinations_offer_and_leaves_its_own_sent_move_frozen() {
        let t0 = Instant::now();
        let mut one = Moves::new(Window::One);
        let mut two = Moves::new(Window::Two);

        // Window 2 sends a chat to window 1 while window 1 offers a terminal to window 2.
        let chat = two.offer(t0);
        assert!(one.accept_offer(chat));
        assert!(one.prepared(chat));
        let term = one.offer(t0);
        assert!(two.accept_offer(term));
        assert!(two.prepared(term));
        assert!(two.ready(chat));
        assert!(two.freeze(chat, t0));
        assert!(two.sent(chat, t0));

        // Window 1's terminal move runs out of time before its freeze, so window 1 sends `move_abort` for it.
        assert_eq!(
            one.tick(t0 + MOVE_DEADLINE),
            vec![(term, SourceVerdict::Resume(TOO_SLOW.into()))]
        );
        assert_eq!(two.peer_abort(term, TOO_SLOW), PeerAbort::Source);
        assert!(!two.holds(Role::Destination, term), "what window 2 prepared is dropped");
        assert!(
            two.holds(Role::Source, chat),
            "window 2's own sent chat is not resumed by an abort of another move"
        );
        assert_eq!(two.peer_abort(term, TOO_SLOW), PeerAbort::Ignored, "once");
    }

    #[test]
    fn a_destinations_abort_resumes_the_source_and_a_late_source_abort_after_move_changes_nothing() {
        let t0 = Instant::now();
        let mut one = Moves::new(Window::One);
        let mut two = Moves::new(Window::Two);

        // Window 2, the source, gives up before `move`: window 1 drops the offer it prepared.
        let gave_up = two.offer(t0);
        assert!(one.accept_offer(gave_up));
        assert!(one.prepared(gave_up));
        assert_eq!(one.peer_abort(gave_up, "too slow"), PeerAbort::Source);
        assert!(!one.holds(Role::Destination, gave_up));

        // Window 1 as destination gives back a move it received: window 2 resumes it.
        let sent = two.offer(t0);
        let mut w = {
            let (r, w) = pipe();
            assert!(one.accept_offer(sent));
            assert!(one.prepared(sent));
            assert!(two.ready(sent));
            assert!(two.freeze(sent, t0));
            assert!(two.sent(sent, t0));
            one.received(sent, vec![r]).unwrap();
            w
        };
        assert!(!one.source_aborted(sent), "a source sends no abort after move");
        assert!(
            reader_open(&mut w),
            "a received move is kept against an out-of-turn abort"
        );
        one.refuse_received(sent);
        assert_eq!(
            two.peer_abort(sent, "could not take it"),
            PeerAbort::Destination(SourceVerdict::Resume("could not take it".into()))
        );
        assert!(!two.holds(Role::Source, sent));
    }
}
