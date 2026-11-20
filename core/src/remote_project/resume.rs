//! What a remote tab does with the server's session transcripts: the sessions its chooser lists, the history a resume
//! starts from, and the note a rebuild carries.
//!
//! The calls are [`RemoteConnection`]'s wrappers in [`super::transcripts`], which hand back what the server said,
//! checked and raw. This is where that is turned into what a tab shows: a title is normalised as a local session's is
//! and its hidden characters are written out, a transcript becomes the history a conversation starts from, and a
//! transcript that cannot be had falls to Eitri's own copy as it does on this machine. Everything here blocks on the
//! server, so it runs on a worker; [`list_on_worker`] and [`SessionListing`] are how a window asks without waiting.

use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use agent::remote_client::{self as wire};
use agent::{AgentSessionProjection, ConversationError, ResumableSession};

use super::transcripts::{Stability, TabScope, TranscriptList, Used};
use super::RemoteConnection;

/// How often one tab's chooser asks the server for its sessions: a chooser opened again sooner shows what the last
/// ask found.
pub const LISTING_MIN_INTERVAL: Duration = Duration::from_secs(10);

/// The server's sessions for a tab, as its chooser lists them.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerSessions {
    /// Newest first, with the titles ready to show ([`shown_title`]).
    pub sessions: Vec<ResumableSession>,
    /// The server had more sessions than it sent.
    pub truncated: bool,
}

/// A title the server's transcript recorded, as a row may show it. The server's text is untrusted: it goes through
/// the normaliser a local session's title does (the first line with visible text, whitespace collapsed, cut), then
/// every character that changes how a line looks without looking like anything is written out as a `\u{...}`
/// escape. `None` for a title with nothing visible.
pub fn shown_title(raw: &str) -> Option<String> {
    let title = agent::persistence::title_from_prompt(raw)?;
    Some(crate::onboarding::display::display_text(&title))
}

impl ServerSessions {
    fn from_list(list: &TranscriptList) -> Self {
        let sessions = list
            .entries
            .iter()
            .map(|entry| {
                let stamp = entry.modified_unix_ms.to_string();
                ResumableSession {
                    provider: crate::agent_backend::AgentId::Claude.record_name(),
                    provider_session_id: entry.provider_session_id.clone(),
                    created_at: stamp.clone(),
                    updated_at: stamp,
                    title: shown_title(&entry.ai_title),
                    name: None,
                }
            })
            .collect();
        Self {
            sessions,
            truncated: list.truncated,
        }
    }
}

/// A refusal or error the server's side produced, as a line may show it: the text is the server's, so every hidden
/// or control character (a newline, an escape sequence, a bidi override) is written out and the line is cut.
pub fn shown_refusal(text: &str) -> String {
    crate::onboarding::display::display_text_cut(text, 400)
}

/// Whether two spellings name one config directory lexically: empty and `.` components and a trailing `/` do not
/// count (the helper names the normal form of what it was given).
fn same_config_dir(a: &str, b: &str) -> bool {
    let parts = |path: &str| -> Vec<String> {
        path.split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .map(str::to_owned)
            .collect()
    };
    parts(a) == parts(b)
}

/// Refuses an answer that read another config directory than the one this tab's own agent reported, naming both.
/// A tab with no reported directory (`scope.config_dir` empty) takes the helper's choice.
fn check_config_dir(host: &str, scope: &TabScope, used: &Used) -> Result<(), String> {
    if scope.config_dir.is_empty() || same_config_dir(&scope.config_dir, &used.config_dir) {
        return Ok(());
    }
    Err(format!(
        "the helper on {} read {} for this tab's sessions, but this tab's agent reported {}; refused",
        crate::onboarding::display::display_name(host),
        crate::onboarding::display::display_name(&used.config_dir),
        crate::onboarding::display::display_name(&scope.config_dir),
    ))
}

/// The note a tab gives when its history was rebuilt from the server's transcript.
pub fn rebuilt_note(host: &str) -> String {
    format!(
        "rebuilt from the CLI's transcript on {}",
        crate::onboarding::display::display_name(host)
    )
}

/// [`rebuilt_note`] when `history`, the notice of the history a resumed tab started from, says it was read from the
/// CLI's transcript; `None` when it came from Eitri's own copy or there was none, which is no rebuild.
pub fn rebuilt_note_if_read_from_the_transcript(host: &str, history: Option<&agent::HistoryNotice>) -> Option<String> {
    history
        .is_some_and(|notice| notice.source == agent::HistorySource::ClaudeTranscript)
        .then(|| rebuilt_note(host))
}

impl RemoteConnection {
    /// Whether the helper offers the `transcripts` service, from the Hello this connection already holds. Never calls
    /// the server, so a window's own thread may ask it; a connection with no Hello yet offers nothing.
    pub fn offers_transcripts(&self) -> bool {
        self.transcripts_offered() == Some(true)
    }

    /// [`Self::offers_transcripts`], or `None` while this connection holds no Hello yet, for a caller that must not
    /// take "not yet known" for "not offered". Never calls the server.
    pub fn transcripts_offered(&self) -> Option<bool> {
        self.helper_client()
            .cached_capabilities()
            .map(|capabilities| capabilities.iter().any(|offered| offered == wire::CAP_TRANSCRIPTS))
    }

    /// The tab's project as the transcripts calls name it: the root as recorded, the canonical root the tab's trust
    /// scan named (`Discovery.root`; the answers are checked against it), the window's account and the config
    /// directory a handshake reported, when one did. A tab whose root the scan has not named yet is refused with a
    /// plain line, never given the root as recorded in its place: a root reached through a link would be answered
    /// under its canonical spelling and every answer would then be taken for another project's.
    pub fn tab_scope(
        &self,
        canonical_root: &str,
        account: Option<&str>,
        config_dir: Option<&str>,
    ) -> Result<TabScope, String> {
        if canonical_root.is_empty() {
            return Err(format!(
                "this project's folder on {} has not been checked yet, so its sessions cannot be listed or continued",
                crate::onboarding::display::display_name(self.host())
            ));
        }
        Ok(TabScope {
            root: self.project().root.to_string_lossy().into_owned(),
            canonical_root: canonical_root.to_owned(),
            account: account.unwrap_or_default().to_owned(),
            config_dir: config_dir.unwrap_or_default().to_owned(),
        })
    }

    /// The server's sessions for the tab, titled. Every refusal is returned as a line safe to show. Blocking.
    pub fn list_server_sessions(&self, scope: &TabScope) -> Result<ServerSessions, String> {
        let list = self.list_transcripts(scope).map_err(|why| shown_refusal(&why))?;
        check_config_dir(self.host(), scope, &list.used)?;
        Ok(ServerSessions::from_list(&list))
    }

    /// The history a resume of `session_id` starts from, built from the server's transcript; `None` when the helper
    /// offers no transcripts. As a resume here does it: a transcript the server says is still being written refuses
    /// the resume ([`ConversationError::TranscriptUnstable`]); a refusal or no answer to that question does not. Then
    /// the transcript's last bytes are folded (`history::seed_from_tail`, labelled `<host>:<path on the server>`), and
    /// a transcript that is missing, refused or holds nothing usable falls to Eitri's own copy, saying why in the
    /// history's notice; a tail read from another config directory than the one the tab's agent reported refuses the
    /// resume ([`ConversationError::ResumeRejected`], both paths named). Blocking: up to the probe's quarter second and a transcript's size.
    pub fn resume_seed(
        &self,
        scope: &TabScope,
        session_id: &str,
    ) -> Result<Option<AgentSessionProjection>, ConversationError> {
        if !self.offers_transcripts() {
            return Ok(None);
        }
        // A refusal or no answer to the probe does not block, as a resume here goes on when it cannot probe.
        if let Ok(probe) = self.transcript_stable(scope, session_id) {
            if probe.stability == Stability::Moving && check_config_dir(self.host(), scope, &probe.used).is_ok() {
                return Err(ConversationError::TranscriptUnstable {
                    provider_session_id: session_id.to_owned(),
                });
            }
        }
        let conversation_id = agent::conversation_id_for_cwd(&self.project().root);
        let tail = self
            .read_transcript_tail(scope, session_id)
            .map_err(|why| shown_refusal(&why));
        // A tail read from another directory than the one this tab's agent reported is no history for this
        // session, and Eitri's own copy would hide that: the resume is refused with both paths named.
        if let Ok(tail) = &tail {
            check_config_dir(self.host(), scope, &tail.used).map_err(|reason| ConversationError::ResumeRejected {
                provider_session_id: session_id.to_owned(),
                reason,
            })?;
        }
        let seed = match tail {
            Ok(tail) => {
                let label = self.transcript_label(&tail.used.config_dir, &tail.used.root, session_id);
                agent::history::seed_from_tail(&tail.bytes, tail.started_mid_file, &conversation_id, session_id, &label)
            }
            Err(why) => {
                let label = self.transcript_label(&scope.config_dir, &scope.canonical_root, session_id);
                agent::history::seed_from_stored(&conversation_id, session_id, &label, &why)
            }
        };
        Ok(Some(seed))
    }

    /// Where a transcript is, for the history's notice: the host and the path there, hidden characters written out.
    fn transcript_label(&self, config_dir: &str, root: &str, session_id: &str) -> String {
        let path = agent::transcript::transcript_path_in(&Path::new(config_dir).join("projects"), root, session_id);
        crate::onboarding::display::display_name(&format!("{}:{}", self.host(), path.display()))
    }
}

/// Asks `connection` for the tab's sessions on a thread of its own and returns the receiver the answer arrives on:
/// the caller's thread never waits on the server.
pub fn list_on_worker(connection: RemoteConnection, scope: TabScope) -> mpsc::Receiver<Result<ServerSessions, String>> {
    spawn_listing(move || connection.list_server_sessions(&scope))
}

/// Runs `work` on a thread of its own, its answer arriving on the receiver returned.
pub fn spawn_listing(
    work: impl FnOnce() -> Result<ServerSessions, String> + Send + 'static,
) -> mpsc::Receiver<Result<ServerSessions, String>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    rx
}

/// One tab's list of the server's sessions: what the last ask found, the ask under way, and when it was made, so a
/// chooser opened again within [`LISTING_MIN_INTERVAL`] asks nothing new.
#[derive(Default)]
pub struct SessionListing {
    asked_at: Option<Instant>,
    pending: Option<mpsc::Receiver<Result<ServerSessions, String>>>,
    latest: Option<Result<ServerSessions, String>>,
}

impl SessionListing {
    /// Starts an ask through `start` unless one is under way or the last was made less than
    /// [`LISTING_MIN_INTERVAL`] before `now`. Whether one was started.
    pub fn ask(
        &mut self,
        now: Instant,
        start: impl FnOnce() -> mpsc::Receiver<Result<ServerSessions, String>>,
    ) -> bool {
        let recent = self
            .asked_at
            .is_some_and(|at| now.saturating_duration_since(at) < LISTING_MIN_INTERVAL);
        if self.pending.is_some() || recent {
            return false;
        }
        self.asked_at = Some(now);
        self.pending = Some(start());
        true
    }

    /// Takes in the answer of the ask under way, if it came. Whether the list changed.
    pub fn collect(&mut self) -> bool {
        let Some(pending) = &self.pending else {
            return false;
        };
        let answer = match pending.try_recv() {
            Ok(answer) => answer,
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(mpsc::TryRecvError::Disconnected) => {
                Err("the listing of the server's sessions ended without an answer".to_owned())
            }
        };
        self.pending = None;
        self.latest = Some(answer);
        true
    }

    /// An ask is under way.
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// What the last ask found: the sessions, or why it could not list them. `None` before any answer.
    pub fn latest(&self) -> Option<&Result<ServerSessions, String>> {
        self.latest.as_ref()
    }
}
