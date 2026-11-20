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

use super::spawn::{HandedOutput, SidecarFds};
use super::transport::{self, TransportHandoff, TransportKind};
use super::transport::{CallMetadata, SidecarTransport};
use super::{
    handshake_with, lock, ClaudeSidecarProvider, EventQueue, Handshaken, ParkedWatch, RemoteSide, WatchMark, WatchState,
};
use crate::child::{ChildKind, ChildRecord};
use crate::runtime_thread::RuntimeThread;
use claude_runtime_protocol::v1::runtime_service_client::RuntimeServiceClient;
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
    #[serde(default)]
    pub transport: TransportKind,
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
    /// Whether the main reply `last_text` names is still open for another text block (no main-thread
    /// tool call since its text), so a block that starts after the move is a paragraph of it only if it
    /// was before. Absent from a peer that predates it: taken as open while `last_text` is set.
    #[serde(default)]
    pub last_text_open: Option<bool>,
    /// Whether the session's CLI-mode line was printed up to `folded_through`.
    pub cli_mode_noted: bool,
    /// Whether the session was created with plan mode offered, so the taking side judges the CLI's `plan`
    /// reports as the giving side did. Absent from a peer that predates it: no plan mode.
    #[serde(default)]
    pub plan_offered: bool,
    /// Messages reported dropped whose delivery may still come, with their typed text, oldest first.
    pub steers_given_up: Vec<(String, String)>,
    /// The drains' lines in progress, and the stderr tail as newline-ended lines.
    pub stdout_partial: Vec<u8>,
    pub stderr_partial: Vec<u8>,
    pub stderr_tail: Vec<u8>,
    pub build_description: String,
    pub build_warnings: Vec<String>,
    /// For a remote tab, what its window's handshake with the sidecar and Hello with the helper said, so
    /// the taking window needs no round trip to take it up; `None` for a sidecar on this machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_facts: Option<RemoteFacts>,
}

/// What a remote tab's provider learned from its handshake and its helper's Hello.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteFacts {
    pub capabilities: crate::provider::ProviderCapabilities,
    pub info: crate::provider::ProviderInfo,
    pub splits_messages: bool,
    pub provider_prompts: bool,
    pub claude_code_prompt: bool,
    pub max_message_bytes: u64,
    pub helper_version: String,
    pub helper_warnings: Vec<String>,
}

/// What a freeze took from the transport.
enum Handed {
    Local {
        fds: SidecarFds,
        socket_path: PathBuf,
        child: ChildRecord,
    },
    Helper(super::HelperHandoff),
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
    /// This kind of chat cannot move between windows yet; the text says which.
    Unsupported(String),
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
            MoveRefusedWhy::Unsupported(what) => f.write_str(what),
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
        self.freeze(false)
            .map(|(handoff, fds)| (handoff, fds.expect("a local sidecar hands its descriptors over")))
    }

    /// [`Self::freeze_for_move`] for a chat whose agent may run on a server: a remote tab hands over its
    /// [`HelperHandoff`](super::HelperHandoff) and no descriptors, and the taking window reaches the same
    /// sidecar through its own helper connection ([`Self::adopt_through_helper`]).
    pub fn freeze_for_chat_move(&self) -> Result<(ClaudeProviderHandoff, Option<SidecarFds>), MoveRefusedWhy> {
        self.freeze(true)
    }

    fn freeze(&self, helper_too: bool) -> Result<(ClaudeProviderHandoff, Option<SidecarFds>), MoveRefusedWhy> {
        let ClaudeSidecarProvider {
            transport,
            // Rebuilt: the taking side runs its own runtime and connection.
            runtime: _,
            client: _,
            events,
            // Rebuilt: its counters describe this process's stream.
            backpressure: _,
            // Rebuilt from the taking side's own handshake with the same sidecar; a remote tab's travel
            // in the record, since the taking window takes it up with no round trip.
            capabilities,
            info,
            splits_messages,
            provider_prompts,
            claude_code_prompt,
            max_message_bytes,
            // A sidecar that no longer runs has nothing to move.
            history_only: _,
            // Not carried: only a session create or resume reads it, and the taking side opens none.
            #[cfg(feature = "test-support")]
                questions_override: _,
            #[cfg(feature = "test-support")]
                plan_mode_override: _,
            plan_offered,
            watches,
            parked,
            released,
            #[cfg(feature = "test-support")]
                per_task_stop_override: _,
            // A remote tab's link state stays here; the taking window keeps its own.
            remote: _,
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
        let mut transport = lock(transport);
        let taken = transport.freeze_for_move().and_then(|handoff| match handoff {
            TransportHandoff::Local { fds, socket_path, pid } => {
                let start = crate::child::start_time(pid).map_err(MoveRefusedWhy::SidecarEnding)?;
                Ok(Handed::Local {
                    fds,
                    socket_path,
                    child: ChildRecord {
                        pid,
                        start,
                        kind: ChildKind::Sidecar,
                    },
                })
            }
            TransportHandoff::Helper(handoff) if helper_too => Ok(Handed::Helper(handoff)),
            TransportHandoff::Helper(handoff) => Err(MoveRefusedWhy::Unsupported(format!(
                "a chat whose agent runs on {} moves only through the chat mover",
                handoff.host
            ))),
        });
        let handed = match taken {
            Ok(taken) => taken,
            Err(why) => {
                let _ = transport.restart_output();
                events.thaw();
                return Err(why);
            }
        };
        let HandedOutput {
            stdout_partial,
            stderr_partial,
            stderr_tail,
        } = transport.handed_output();
        let WatchMark {
            sequence,
            last_text,
            last_text_open,
            child_replies,
            cli_mode_noted,
        } = mark;
        let remote_facts = match &handed {
            Handed::Local { .. } => None,
            Handed::Helper(_) => Some(RemoteFacts {
                capabilities: capabilities.clone(),
                info: info.clone(),
                splits_messages: *splits_messages,
                provider_prompts: *provider_prompts,
                claude_code_prompt: *claude_code_prompt,
                max_message_bytes: *max_message_bytes,
                helper_version: transport.helper_version().unwrap_or_default(),
                helper_warnings: transport.build_warnings(),
            }),
        };
        let (kind, fds, socket_path, child) = match handed {
            Handed::Local {
                fds,
                socket_path,
                child,
            } => (TransportKind::Local, Some(fds), socket_path, child),
            // No process here and no descriptors: the record names the sidecar and the session, and
            // where the watch stood.
            Handed::Helper(helper) => (
                TransportKind::Helper(super::HelperHandoff {
                    session_id: session_id.clone(),
                    last_folded_sequence: sequence,
                    ..helper
                }),
                None,
                PathBuf::new(),
                ChildRecord {
                    pid: 0,
                    start: 0,
                    kind: ChildKind::Sidecar,
                },
            ),
        };
        let handoff = ClaudeProviderHandoff {
            transport: kind,
            provider_session_id: session_id,
            socket_path,
            child,
            folded_through: sequence,
            last_text,
            last_text_open: Some(last_text_open),
            last_child_replies: child_replies,
            cli_mode_noted,
            plan_offered: plan_offered.load(Ordering::SeqCst),
            steers_given_up,
            stdout_partial,
            stderr_partial,
            stderr_tail,
            build_description: transport.build_description(),
            build_warnings: transport.build_warnings(),
            remote_facts,
        };
        Ok((handoff, fds))
    }

    /// Takes up a remote tab another window froze for a move: the same sidecar, reached through this
    /// window's own connection to its helper (`client`, opened for the record's host key). Nothing
    /// crosses the network here (it runs where the move is taken up, on the GTK thread): the handshake's
    /// facts come with the record ([`RemoteFacts`]), and nothing is watched, read or answered until
    /// [`Self::commit`], whose watch reads on from the handed-over sequence. Dropping the result ends
    /// nothing on the server.
    pub fn adopt_through_helper(
        handoff: ClaudeProviderHandoff,
        client: crate::remote_client::HelperClient,
    ) -> std::io::Result<Self> {
        let invalid = |why: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, why.to_string());
        let TransportKind::Helper(helper) = &handoff.transport else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the moved chat's agent does not run on a server",
            ));
        };
        if helper.session_id != handoff.provider_session_id || helper.last_folded_sequence != handoff.folded_through {
            return Err(invalid(
                "the moved chat's record names two different places to go on from",
            ));
        }
        let helper = helper.clone();
        let ClaudeProviderHandoff {
            transport: _,
            provider_session_id: session_id,
            socket_path: _,
            child: _,
            folded_through,
            last_text,
            last_text_open,
            last_child_replies,
            cli_mode_noted,
            plan_offered,
            steers_given_up,
            stdout_partial: _,
            stderr_partial: _,
            stderr_tail: _,
            build_description: _,
            build_warnings: _,
            remote_facts,
        } = handoff;
        let RemoteFacts {
            capabilities,
            info,
            splits_messages,
            provider_prompts,
            claude_code_prompt,
            max_message_bytes,
            helper_version,
            helper_warnings,
        } = remote_facts.ok_or_else(|| invalid("the moved chat's record lacks its agent's handshake"))?;
        if client.host() != helper.host {
            return Err(invalid("the connection given for the moved chat is to another host"));
        }
        let client = client.for_known_owner(&helper.daemon)?;
        let checker = client.link_checker(Some(&helper.host_key_sha256))?;
        let transport = super::transport::helper::ThroughHelper::known(
            client.clone(),
            helper.sidecar_id.clone(),
            helper.host_key_sha256.clone(),
            helper_version,
            helper_warnings,
        )
        .with_tools(&helper.declared_client_tools);
        let runtime = client.runtime();
        let channel = transport.channel(&runtime, None)?;
        let runtime_client =
            RuntimeServiceClient::with_interceptor(channel, CallMetadata::new(transport.call_metadata())?);
        let mark = WatchMark {
            sequence: folded_through,
            last_text_open: last_text_open.unwrap_or(last_text.is_some()),
            last_text,
            child_replies: last_child_replies,
            cli_mode_noted,
        };
        let remote = RemoteSide {
            checker,
            link: Arc::default(),
            clock: Arc::new(crate::remote_client::SystemClock),
            asker: client.pending_asker(&helper.sidecar_id),
            ending: Arc::default(),
            // Taken up here with no call to the helper, so with no Hello to judge its facts by: a moved tab keeps
            // the gate without facts until it is reattached.
            facts: None,
        };
        Ok(ClaudeSidecarProvider {
            transport: Mutex::new(Box::new(transport)),
            runtime,
            client: runtime_client,
            events: Arc::new(EventQueue::taking_over(mark.clone(), steers_given_up)),
            backpressure: Arc::default(),
            capabilities,
            info,
            splits_messages,
            provider_prompts,
            claude_code_prompt,
            max_message_bytes,
            history_only: false,
            #[cfg(feature = "test-support")]
            questions_override: None,
            #[cfg(feature = "test-support")]
            plan_mode_override: None,
            plan_offered: AtomicBool::new(plan_offered),
            watches: Mutex::new(Vec::new()),
            parked: Mutex::new(Some(ParkedWatch { session_id, mark })),
            released: AtomicBool::new(false),
            #[cfg(feature = "test-support")]
            per_task_stop_override: None,
            remote: Some(remote),
        })
    }

    /// After a freeze whose move did not happen: the drains read on from where they stopped, and the
    /// events the watch queued meanwhile are handed over at the next pump.
    pub fn unfreeze(&self) -> std::io::Result<()> {
        let restarted = lock(&self.transport).restart_output();
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
        let transport = transport::adopt(&handoff, fds)?;
        let ClaudeProviderHandoff {
            transport: _,
            provider_session_id: session_id,
            socket_path: _,
            child,
            folded_through,
            last_text,
            last_text_open,
            last_child_replies,
            cli_mode_noted,
            plan_offered,
            steers_given_up,
            stdout_partial: _,
            stderr_partial: _,
            stderr_tail: _,
            build_description: _,
            build_warnings: _,
            remote_facts: _,
        } = handoff;
        let runtime = Arc::new(RuntimeThread::spawn());
        let Handshaken {
            client,
            capabilities,
            info,
            splits_messages,
            provider_prompts,
            claude_code_prompt,
            max_message_bytes,
            history_only: _,
        } = handshake_with(
            &runtime,
            transport.as_ref(),
            Some(child.pid),
            super::spawn::running_exe_dir().as_deref(),
        )?;
        let mark = WatchMark {
            sequence: folded_through,
            last_text_open: last_text_open.unwrap_or(last_text.is_some()),
            last_text,
            child_replies: last_child_replies,
            cli_mode_noted,
        };
        Ok(ClaudeSidecarProvider {
            transport: Mutex::new(transport),
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
            history_only: false,
            #[cfg(feature = "test-support")]
            questions_override: None,
            #[cfg(feature = "test-support")]
            plan_mode_override: None,
            plan_offered: AtomicBool::new(plan_offered),
            watches: Mutex::new(Vec::new()),
            parked: Mutex::new(Some(ParkedWatch { session_id, mark })),
            released: AtomicBool::new(false),
            #[cfg(feature = "test-support")]
            per_task_stop_override: None,
            remote: None,
        })
    }

    /// The move is committed: this process holds the sidecar from here on (its end ends it), opens
    /// the session's watch from the handed-over sequence, reads its output and answers its cards. It
    /// waits for no event. Nothing to do for a provider that is not waiting for a commit.
    pub fn commit(&self) -> std::io::Result<()> {
        let Some(ParkedWatch { session_id, mark }) = lock(&self.parked).take() else {
            return Ok(());
        };
        let held = lock(&self.transport).start_holding();
        let state = WatchState::resumed(&mark, self.splits_messages, self.session_expectation());
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
        lock(&self.transport).let_go();
    }
}
