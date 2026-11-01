//! A Claude chat's sidecar handed from one process to another without a restart: the chat moves to
//! the other window while its turn runs.
//!
//! Verdandi's sidecar serves one session to several watch connections at once, takes a permission
//! answer from any connection, keeps a ring of recent events per session that a watch can replay
//! from a sequence, and ends on its stdin's EOF. So the process taking a chat over connects to the
//! same socket, watches from the sequence the giving side last folded, and answers cards; the giving
//! side's stdin copy is a duplicate whose closing ends nothing while the taker's copy lives.
//!
//! The steps, in the order the two windows take them:
//! 1. the giving side's conversation stops folding, and [`ClaudeSidecarProvider::freeze_for_move`]
//!    stops handing events over, stops the output drains and returns the handoff and duplicates of
//!    the sidecar's stdin, stdout and stderr. Its watch stays open: if the move is given back,
//!    [`ClaudeSidecarProvider::unfreeze`] goes on from there and nothing is lost;
//! 2. the taking side's [`ClaudeSidecarProvider::adopt`] checks the process by pid and start time,
//!    connects and handshakes, and holds everything without watching, reading or answering;
//! 3. after the commit, the taking side's [`ClaudeSidecarProvider::commit`] opens the watch from the
//!    handed-over sequence, starts the drains and from then on holds the sidecar; it waits for no
//!    event, since an idle session may send none for minutes;
//! 4. the giving side's [`ClaudeSidecarProvider::forget_for_move`] cancels its watch and closes its
//!    copies, and ends nothing.

use super::spawn::{HandedOutput, SidecarFds, SpawnedSidecar};
use super::{
    handshake_with, lock, requested_cli_mode, ClaudeSidecarProvider, EventQueue, Handshaken, ParkedWatch, WatchMark,
    WatchState,
};
use crate::child::{ChildKind, ChildRecord};
use crate::runtime_thread::RuntimeThread;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Whether a running turn moves too. The sidecar takes a second watcher of a session and an answer
/// from another connection than the one that watched the request, so a mid-turn move is live.
const LIVE_MOVE: bool = true;

/// What the giving side's provider hands over beside the descriptors, as of the last event its
/// conversation folded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeProviderHandoff {
    /// The sidecar's id of the session the watch follows (the conversation's `session_id`).
    pub provider_session_id: String,
    pub socket_path: PathBuf,
    pub child: ChildRecord,
    /// The sequence of the last event folded: the taking side's watch reads on from the next one.
    pub folded_through: u64,
    /// The message id of the last text delta translated up to `folded_through`. Without it the first
    /// delta of the next message would not open a message of its own, and two would merge.
    pub last_text: Option<String>,
    /// The same for each subagent's open reply, by the call that started it, the longest silent first:
    /// without it the first delta of a subagent's next message would join the reply row on screen.
    #[serde(default)]
    pub last_child_replies: Vec<(String, String)>,
    /// Whether the session's CLI-mode line was printed up to `folded_through`.
    pub cli_mode_noted: bool,
    /// Messages reported dropped whose delivery may still come, with their typed text, oldest first.
    pub steers_given_up: Vec<(String, String)>,
    /// The drains' lines in progress, and the stderr tail as newline-ended lines.
    pub stdout_partial: Vec<u8>,
    pub stderr_partial: Vec<u8>,
    pub stderr_tail: Vec<u8>,
    pub build_description: String,
    pub build_warnings: Vec<String>,
}

/// Why a Claude chat's sidecar was not handed over. The words are a note the giving window shows.
#[derive(Debug)]
pub enum MoveRefusedWhy {
    /// A message steered into the turn is waiting to be sent, unanswered, or not yet delivered.
    SteerOpen,
    /// A turn's end is held for the follow-up turn of a steered message.
    HoldingTurnEnd,
    /// A message is being sent to the sidecar right now.
    SteerInFlight,
    /// An answer to Claude's question was sent and its reply not read yet: refused, it has to put the
    /// question back in this window.
    AnswerInFlight,
    /// No session is watched, so there is nothing to go on from.
    NoSession,
    /// More than one session is watched; a move hands over exactly one.
    SeveralSessions,
    /// This process does not drive the session: it handed it on, or took it over and has not committed.
    NotDriving,
    /// The sidecar's output closed or a descriptor could not be duplicated: it is ending.
    SidecarEnding(std::io::Error),
    /// The chat has fallen so far behind the events already read that the sidecar may no longer be
    /// able to replay them from where it stands; it catches up within moments.
    FoldBehind,
}

impl std::fmt::Display for MoveRefusedWhy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MoveRefusedWhy::SteerOpen | MoveRefusedWhy::SteerInFlight => {
                f.write_str("a message is still on its way into the running turn")
            }
            MoveRefusedWhy::AnswerInFlight => f.write_str("an answer to Claude's question is still on its way"),
            MoveRefusedWhy::HoldingTurnEnd => f.write_str("the turn's end waits for a message's follow-up turn"),
            MoveRefusedWhy::NoSession => f.write_str("the chat has no session to move"),
            MoveRefusedWhy::SeveralSessions => f.write_str("the chat watches more than one session"),
            MoveRefusedWhy::NotDriving => f.write_str("this window does not drive the chat"),
            MoveRefusedWhy::SidecarEnding(why) => write!(f, "the agent's sidecar is ending: {why}"),
            MoveRefusedWhy::FoldBehind => f.write_str("the chat has not caught up with its agent's output yet"),
        }
    }
}

impl std::error::Error for MoveRefusedWhy {}

impl ClaudeSidecarProvider {
    /// Whether a chat with a running turn moves now, rather than at its turn's end.
    pub fn live_move(&self) -> bool {
        LIVE_MOVE
    }

    /// Stops handing events over and returns what another process takes the sidecar over with, as of
    /// the last event the conversation folded. Called once the conversation itself has stopped
    /// folding. The watch stays open meanwhile, so [`Self::unfreeze`] loses nothing.
    ///
    /// Refused, with nothing changed, while a message steered into the running turn is open or being
    /// sent, or a turn's end is held for its follow-up (seconds at most), or was at the last pump; when
    /// a message was steered in after the last pump; when the watch has read far past the last pump;
    /// and when the session or the sidecar is not one to hand over.
    ///
    /// Destructures the provider without a rest pattern, so a field added to it has to say whether
    /// the taking side needs it.
    pub fn freeze_for_move(&self) -> Result<(ClaudeProviderHandoff, SidecarFds), MoveRefusedWhy> {
        let ClaudeSidecarProvider {
            sidecar,
            // Rebuilt: the taking side runs its own runtime and connection.
            runtime: _,
            client: _,
            events,
            // Rebuilt: its counters describe this process's stream.
            backpressure: _,
            // Rebuilt from the taking side's own handshake with the same sidecar.
            capabilities: _,
            info: _,
            splits_messages: _,
            provider_prompts: _,
            claude_code_prompt: _,
            max_message_bytes: _,
            // Not carried: only a session create or resume reads it, and the taking side opens none.
            #[cfg(feature = "test-support")]
                questions_override: _,
            watches,
            parked,
            released,
            #[cfg(feature = "test-support")]
                per_task_stop_override: _,
        } = self;
        if released.load(Ordering::SeqCst) || lock(parked).is_some() {
            return Err(MoveRefusedWhy::NotDriving);
        }
        let session_id = {
            let watches = lock(watches);
            let mut live = watches.iter().filter(|watch| !watch.task.is_finished());
            match (live.next(), live.next()) {
                (None, _) => return Err(MoveRefusedWhy::NoSession),
                (Some(_), Some(_)) => return Err(MoveRefusedWhy::SeveralSessions),
                (Some(watch), None) => watch.session_id.clone(),
            }
        };
        let (mark, steers_given_up) = events.freeze()?;
        let mut sidecar = lock(sidecar);
        let taken = sidecar.park_output().and_then(|()| {
            let fds = sidecar.duplicate_fds()?;
            let pid = sidecar.pid();
            let start = crate::child::start_time(pid)?;
            Ok((fds, pid, start))
        });
        let (fds, pid, start) = match taken {
            Ok(taken) => taken,
            Err(why) => {
                let _ = sidecar.restart_output();
                events.thaw();
                return Err(MoveRefusedWhy::SidecarEnding(why));
            }
        };
        let HandedOutput {
            stdout_partial,
            stderr_partial,
            stderr_tail,
        } = sidecar.handed_output();
        let WatchMark {
            sequence,
            last_text,
            child_replies,
            cli_mode_noted,
        } = mark;
        let handoff = ClaudeProviderHandoff {
            provider_session_id: session_id,
            socket_path: sidecar.socket_path.clone(),
            child: ChildRecord {
                pid,
                start,
                kind: ChildKind::Sidecar,
            },
            folded_through: sequence,
            last_text,
            last_child_replies: child_replies,
            cli_mode_noted,
            steers_given_up,
            stdout_partial,
            stderr_partial,
            stderr_tail,
            build_description: sidecar.build_description.clone(),
            build_warnings: sidecar.build_warnings.clone(),
        };
        Ok((handoff, fds))
    }

    /// After a freeze whose move did not happen: the drains read on from where they stopped, and the
    /// events the watch queued meanwhile are handed over at the next pump.
    pub fn unfreeze(&self) -> std::io::Result<()> {
        let restarted = lock(&self.sidecar).restart_output();
        self.events.thaw();
        restarted
    }

    /// Takes over a sidecar another process froze for a move. The process must still be the one
    /// started at the handed-over start time, and the socket must be served by it. Nothing is
    /// watched, read from it or sent to it until [`Self::commit`]; dropping the result before then
    /// closes the received descriptors and ends nothing.
    ///
    /// The session lease is not the provider's: it crosses with the conversation, which holds it.
    pub fn adopt(handoff: ClaudeProviderHandoff, fds: SidecarFds) -> std::io::Result<Self> {
        let ClaudeProviderHandoff {
            provider_session_id: session_id,
            socket_path,
            child,
            folded_through,
            last_text,
            last_child_replies,
            cli_mode_noted,
            steers_given_up,
            stdout_partial,
            stderr_partial,
            stderr_tail,
            build_description,
            build_warnings,
        } = handoff;
        let sidecar = SpawnedSidecar::adopted(
            socket_path,
            &child,
            fds,
            HandedOutput {
                stdout_partial,
                stderr_partial,
                stderr_tail,
            },
            build_description,
            build_warnings,
        )?;
        let runtime = RuntimeThread::spawn();
        let Handshaken {
            client,
            capabilities,
            info,
            splits_messages,
            provider_prompts,
            claude_code_prompt,
            max_message_bytes,
        } = handshake_with(
            &runtime,
            &sidecar,
            Some(child.pid),
            super::spawn::running_exe_dir().as_deref(),
        )?;
        let mark = WatchMark {
            sequence: folded_through,
            last_text,
            child_replies: last_child_replies,
            cli_mode_noted,
        };
        Ok(ClaudeSidecarProvider {
            sidecar: Mutex::new(sidecar),
            runtime,
            client,
            events: Arc::new(EventQueue::taking_over(mark.clone(), steers_given_up)),
            backpressure: Arc::default(),
            capabilities,
            info,
            splits_messages,
            provider_prompts,
            claude_code_prompt,
            max_message_bytes,
            #[cfg(feature = "test-support")]
            questions_override: None,
            watches: Mutex::new(Vec::new()),
            parked: Mutex::new(Some(ParkedWatch { session_id, mark })),
            released: AtomicBool::new(false),
            #[cfg(feature = "test-support")]
            per_task_stop_override: None,
        })
    }

    /// The move is committed: this process holds the sidecar from here on (its end ends it), opens
    /// the session's watch from the handed-over sequence, reads its output and answers its cards. It
    /// waits for no event. Nothing to do for a provider that is not waiting for a commit.
    pub fn commit(&self) -> std::io::Result<()> {
        let Some(ParkedWatch { session_id, mark }) = lock(&self.parked).take() else {
            return Ok(());
        };
        let held = lock(&self.sidecar).start_holding();
        let state = WatchState::resumed(&mark, self.splits_messages, requested_cli_mode(&self.capabilities));
        self.watch_from(session_id, state);
        held
    }

    /// The move is committed and another process holds the sidecar: the watch is cancelled, this
    /// process's copies of the descriptors close, and nothing is ended -- no stdin EOF the other
    /// copy does not outlive, no signal, no socket file deleted, so a later reconnect over there finds
    /// it. This process acts for the session no more.
    pub fn forget_for_move(&self) {
        self.released.store(true, Ordering::SeqCst);
        for watch in lock(&self.watches).drain(..) {
            watch.task.abort();
        }
        self.events.close_for_good();
        lock(&self.sidecar).let_go();
    }
}
