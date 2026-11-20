//! The laptop's end of the helper's `Transcripts` service: the account's session transcripts and the CLI's settings
//! files on a remote project's server.
//!
//! Every answer is the server's word and is checked before anything uses it. A list, a tail and a settings stream are
//! collected within a count and a byte bound ([`agent::remote_answers::AnswerBound`]) and then checked field by field
//! (a session id, a title, a path, a file, a refusal's text); an answer past a bound, cut short, or for another
//! folder is refused whole with a plain line, never cut down and used. Every refusal line these calls return, the
//! server's own words or a failed call's included, is plain: hidden characters escaped, at most
//! `TRANSCRIPT_REFUSAL_MAX` characters. The data itself is what the server sent, raw: a title, the paths in [`Used`],
//! the settings' `home` and an entry's numbers are the server's, so showing one (escaping it) and reading a
//! transcript are the caller's. The calls block, so they belong on a worker thread.

use agent::remote_answers::{checked_len, checked_session_uuid, AnswerBound};
use agent::remote_client::{
    self as wire, read_deny_settings_event::Event, ListTranscriptsResponse, ReadDenySettingsEvent,
    ReadTranscriptRequest, SettingsRead, TranscriptChunk, TranscriptEntry, TranscriptScope, TranscriptStableResponse,
};

use super::RemoteConnection;
use crate::onboarding::display::display_text_cut;

/// The project a tab is in, as the laptop knows it, and the account whose sessions are asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabScope {
    /// The root as the tab has it (the path as typed), which is what is sent.
    pub root: String,
    /// The root as the tab's trust scan named it (`Discovery.root`), the canonical path: what a helper's answer for
    /// this project names, since the helper resolves the root through every link.
    pub canonical_root: String,
    /// The account's name, if the window has one.
    pub account: String,
    /// The config directory the handshake settled on, if it did; else empty and the helper chooses.
    pub config_dir: String,
}

impl TabScope {
    fn wire(&self) -> TranscriptScope {
        TranscriptScope {
            root: self.root.clone(),
            account: self.account.clone(),
            config_dir: self.config_dir.clone(),
        }
    }
}

/// What the helper says it used for a call. `root` and `config_dir` are the server's spellings; the root has been
/// checked to be this project's, the config directory is the caller's to compare with what it asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Used {
    pub config_dir: String,
    pub root: String,
    /// The folder is not on the server (a deleted project keeps its transcripts).
    pub root_gone: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptList {
    /// Newest first; each id is a lowercase hyphenated UUID, each title at most `TRANSCRIPT_TITLE_MAX` bytes and raw.
    /// An entry's `len` and `modified_unix_ms` are the server's numbers, unchecked: use them with checked arithmetic.
    pub entries: Vec<TranscriptEntry>,
    /// The server had more than it sent.
    pub truncated: bool,
    pub used: Used,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptTail {
    /// The last bytes of the transcript, at most `TRANSCRIPT_TAIL_MAX`.
    pub bytes: Vec<u8>,
    /// The file was longer, so `bytes` starts inside a line.
    pub started_mid_file: bool,
    pub used: Used,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stability {
    Stable,
    /// Still being written (the probe saw it change).
    Moving,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptStability {
    pub stability: Stability,
    pub used: Used,
}

/// The CLI's settings files on the server, every one the server's `files_to_read` names, each within its bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSettings {
    /// The server's `$HOME`, which is what `~` in a rule means.
    pub home: String,
    pub used: Used,
    pub files: Vec<SettingsRead>,
}

impl ServerSettings {
    /// The `Read` deny rules these files give, as a local tab's are built from its own (`ReadRules::from_reads`):
    /// anchored at the server's root, with `~` as the server's home. Each file is what its fields say, the first of a
    /// failure, a missing file and its bytes; a failure's words and path are server text, made plain, since the closed
    /// reader names them.
    pub fn read_rules(&self) -> agent::read_deny::ReadRules {
        use agent::read_deny::{FileRead, ReadRules, SettingsFile};
        let reads = self
            .files
            .iter()
            .map(|file| {
                let (path, read) = match (file.failed.is_empty(), file.missing) {
                    (false, _) => (plain(&file.path), FileRead::Failed(plain(&file.failed))),
                    (true, true) => (file.path.clone(), FileRead::Missing),
                    (true, false) => (file.path.clone(), FileRead::Bytes(file.bytes.clone())),
                };
                let file = SettingsFile {
                    path: path.into(),
                    strict: file.strict,
                };
                (file, read)
            })
            .collect();
        let home = (!self.home.is_empty()).then(|| std::path::Path::new(&self.home));
        ReadRules::from_reads(
            std::path::Path::new(&self.used.root),
            home,
            !self.used.config_dir.is_empty(),
            reads,
        )
    }
}

impl RemoteConnection {
    /// The server's sessions for the tab's project. Refused when the helper lacks the `transcripts` capability, past
    /// a bound, with an id not in its canonical form, or for another project.
    pub fn list_transcripts(&self, scope: &TabScope) -> Result<TranscriptList, String> {
        self.require_capability(wire::CAP_TRANSCRIPTS)?;
        named(scope)?;
        let answer = self.helper_client().list_transcripts(scope.wire()).map_err(failed)?;
        check_list(self.host(), scope, answer)
    }

    /// The last bytes of one session's transcript, collected under a bound of `TRANSCRIPT_TAIL_MAX` and its framing;
    /// refused when the stream is cut short (no final chunk), carries a refusal, passes a bound or is for another
    /// project.
    pub fn read_transcript_tail(&self, scope: &TabScope, session_id: &str) -> Result<TranscriptTail, String> {
        self.require_capability(wire::CAP_TRANSCRIPTS)?;
        named(scope)?;
        let request = request(scope, session_id)?;
        let chunks = self
            .helper_client()
            .collect_transcript_tail(request)
            .map_err(|line| plain(&line))?;
        check_tail(self.host(), scope, chunks)
    }

    /// The server's probe of whether one session's transcript is still being written (a quarter of a second there).
    pub fn transcript_stable(&self, scope: &TabScope, session_id: &str) -> Result<TranscriptStability, String> {
        self.require_capability(wire::CAP_TRANSCRIPTS)?;
        named(scope)?;
        let request = request(scope, session_id)?;
        let answer = self.helper_client().transcript_stable(request).map_err(failed)?;
        check_stable(self.host(), scope, answer)
    }

    /// The CLI's settings files on the server for the tab's project. The answer is all of them or a refusal: a
    /// stream that ends with fewer files than its scope announced, passes a bound or carries a refusal is refused, so
    /// the caller never builds rules from part of a list.
    pub fn read_deny_settings(&self, scope: &TabScope) -> Result<ServerSettings, String> {
        self.require_capability(wire::CAP_TRANSCRIPTS)?;
        named(scope)?;
        let events = self
            .helper_client()
            .collect_read_deny_settings(scope.wire())
            .map_err(|line| plain(&line))?;
        check_settings(self.host(), scope, events)
    }
}

/// A tab whose trust scan has not named its folder yet has nothing to compare an answer's root with: nothing is asked.
fn named(scope: &TabScope) -> Result<(), String> {
    match scope.canonical_root.is_empty() {
        true => {
            Err("this tab's folder has not been named by its trust scan yet; nothing was asked of the server".into())
        }
        false => Ok(()),
    }
}

fn request(scope: &TabScope, session_id: &str) -> Result<ReadTranscriptRequest, String> {
    checked_session_uuid("the session id", session_id)?;
    Ok(ReadTranscriptRequest {
        scope: Some(scope.wire()),
        provider_session_id: session_id.to_owned(),
    })
}

/// A server's refusal as the plain line the caller shows; one longer than the bound a refusal has is itself refused.
fn refusal_line(text: &str) -> String {
    match checked_len("the server's refusal", text.len(), wire::TRANSCRIPT_REFUSAL_MAX) {
        Ok(()) => plain(text),
        Err(over) => over,
    }
}

/// A line that may carry server text, as these calls return it: hidden characters escaped, cut to the bound a
/// refusal has.
fn plain(line: &str) -> String {
    display_text_cut(line, wire::TRANSCRIPT_REFUSAL_MAX)
}

/// A call that failed (the transport, or a status the helper sent) as a plain line written here.
fn failed(error: agent::remote_client::terminal_transport::tonic::Status) -> String {
    plain(&format!(
        "the helper's transcripts call failed ({:?}): {}",
        error.code(),
        error.message()
    ))
}

/// What the server used, if the answer is for this project's folder: its root is the tab's canonical root, or, when
/// the folder is gone there, the text that was sent (the helper names a gone folder through the parents that are
/// there, so the tab's canonical root, which was the same path while the folder was, matches as well). Another root
/// is another project's answer.
fn used(host: &str, scope: &TabScope, config_dir: String, root: String, root_gone: bool) -> Result<Used, String> {
    checked_len(
        "the server's config directory",
        config_dir.len(),
        wire::TRANSCRIPT_PATH_MAX,
    )?;
    checked_len("the server's project folder", root.len(), wire::TRANSCRIPT_PATH_MAX)?;
    if !root.starts_with('/') {
        return Err(format!(
            "the answer from the helper on {host} names no absolute project folder; refused"
        ));
    }
    let ours = root == scope.canonical_root || (root_gone && root == scope.root);
    if !ours {
        return Err(format!(
            "the answer from the helper on {host} is for {root:?}, not for this project's folder {:?}; refused",
            scope.canonical_root
        ));
    }
    Ok(Used {
        config_dir,
        root,
        root_gone,
    })
}

fn check_list(host: &str, scope: &TabScope, answer: ListTranscriptsResponse) -> Result<TranscriptList, String> {
    if !answer.refusal.is_empty() {
        return Err(refusal_line(&answer.refusal));
    }
    AnswerBound::transcript_list().check(&answer.entries)?;
    for entry in &answer.entries {
        checked_session_uuid(
            "a session id in the server's transcript list",
            &entry.provider_session_id,
        )?;
        checked_len(
            "a title in the server's transcript list",
            entry.ai_title.len(),
            wire::TRANSCRIPT_TITLE_MAX,
        )?;
    }
    let used = used(host, scope, answer.config_dir, answer.root, answer.root_gone)?;
    Ok(TranscriptList {
        entries: answer.entries,
        truncated: answer.truncated,
        used,
    })
}

fn check_tail(host: &str, scope: &TabScope, chunks: Vec<TranscriptChunk>) -> Result<TranscriptTail, String> {
    let mut chunks = chunks.into_iter();
    let first = chunks
        .next()
        .ok_or("the server's transcript tail ended before its first chunk; refused as incomplete")?;
    let started_mid_file = first.started_mid_file;
    let mut bytes = Vec::new();
    let mut used_by = None;
    let mut ended = false;
    for (index, chunk) in std::iter::once(first).chain(chunks).enumerate() {
        if !chunk.refusal.is_empty() {
            return Err(refusal_line(&chunk.refusal));
        }
        if ended {
            return Err("the server's transcript tail went on after its last chunk; refused".into());
        }
        if index == 0 {
            used_by = Some(used(host, scope, chunk.config_dir, chunk.root, chunk.root_gone)?);
        }
        checked_len(
            "the server's transcript tail",
            bytes.len().saturating_add(chunk.data.len()),
            usize::try_from(wire::TRANSCRIPT_TAIL_MAX).unwrap_or(usize::MAX),
        )?;
        bytes.extend_from_slice(&chunk.data);
        ended = chunk.last;
    }
    if !ended {
        return Err("the server's transcript tail ended without its last chunk; refused as incomplete".into());
    }
    Ok(TranscriptTail {
        bytes,
        started_mid_file,
        used: used_by.expect("the first chunk named what the server used"),
    })
}

fn check_stable(host: &str, scope: &TabScope, answer: TranscriptStableResponse) -> Result<TranscriptStability, String> {
    if !answer.refusal.is_empty() {
        return Err(refusal_line(&answer.refusal));
    }
    let used = used(host, scope, answer.config_dir, answer.root, answer.root_gone)?;
    let stability = match (answer.missing, answer.stable) {
        (true, _) => Stability::Missing,
        (false, true) => Stability::Stable,
        (false, false) => Stability::Moving,
    };
    Ok(TranscriptStability { stability, used })
}

fn check_settings(host: &str, scope: &TabScope, events: Vec<ReadDenySettingsEvent>) -> Result<ServerSettings, String> {
    let mut events = events.into_iter().map(|event| event.event);
    let head = match events.next() {
        None => return Err("the server's settings ended before their scope; refused as incomplete".into()),
        Some(Some(Event::Refusal(text))) => return Err(refusal_line(&text)),
        Some(Some(Event::Scope(head))) => head,
        Some(Some(Event::File(_))) => {
            return Err("the server's settings began with a file, not their scope; refused".into())
        }
        Some(None) => return Err("the server's settings began with an empty event; refused".into()),
    };
    let announced = usize::try_from(head.files).unwrap_or(usize::MAX);
    if announced > wire::SETTINGS_FILES_MAX {
        return Err(format!(
            "the server's settings name {announced} files, past their bound of {}; refused",
            wire::SETTINGS_FILES_MAX
        ));
    }
    checked_len(
        "the server's home directory",
        head.home.len(),
        wire::TRANSCRIPT_PATH_MAX,
    )?;
    let used = used(host, scope, head.config_dir, head.root, head.root_gone)?;
    let mut files: Vec<SettingsRead> = Vec::with_capacity(announced);
    let mut total = 0usize;
    for event in events {
        let file = match event {
            Some(Event::File(file)) => file,
            Some(Event::Refusal(text)) => return Err(refusal_line(&text)),
            Some(Event::Scope(_)) => return Err("the server's settings repeated their scope; refused".into()),
            None => return Err("the server's settings held an empty event; refused".into()),
        };
        if files.len() == announced {
            return Err(format!(
                "the server's settings held more than the {announced} files their scope named; refused"
            ));
        }
        checked_len(
            "a settings path from the server",
            file.path.len(),
            wire::TRANSCRIPT_PATH_MAX,
        )?;
        checked_len(
            "a settings file from the server",
            file.bytes.len(),
            usize::try_from(wire::SETTINGS_FILE_MAX).unwrap_or(usize::MAX),
        )?;
        checked_len(
            "the reason a settings file could not be read, from the server",
            file.failed.len(),
            wire::TRANSCRIPT_REFUSAL_MAX,
        )?;
        total = total.saturating_add(file.bytes.len());
        checked_len(
            "the server's settings files together",
            total,
            usize::try_from(wire::SETTINGS_TOTAL_MAX).unwrap_or(usize::MAX),
        )?;
        files.push(file);
    }
    if files.len() < announced {
        return Err(format!(
            "the server's settings ended after {} of the {announced} files their scope named; refused as incomplete",
            files.len()
        ));
    }
    Ok(ServerSettings {
        home: head.home,
        used,
        files,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agent::remote_client::{read_deny_settings_event, HelperClient, SettingsScope};

    use super::super::{RemoteConnection, RemoteProject};
    use super::*;

    type Items<T> = std::pin::Pin<
        Box<dyn agent::remote_client::terminal_transport::tokio_stream::Stream<Item = Result<T, tonic::Status>> + Send>,
    >;
    use agent::remote_client::terminal_transport::tonic;

    const SESSION: &str = "0b7a7d3e-5f0c-4c52-9a0e-3a1f0c6d2b11";

    /// What the scripted helper answers.
    #[derive(Default)]
    struct Script {
        /// When set, the list call fails with this status message instead of answering.
        list_error: Option<String>,
        list: ListTranscriptsResponse,
        tail: Vec<TranscriptChunk>,
        stable: TranscriptStableResponse,
        settings: Vec<ReadDenySettingsEvent>,
    }

    struct Scripted(Arc<Script>);

    fn stream<T: Clone + Send + 'static>(items: &[T]) -> tonic::Response<Items<T>> {
        let items: Vec<Result<T, tonic::Status>> = items.iter().cloned().map(Ok).collect();
        tonic::Response::new(Box::pin(agent::remote_client::terminal_transport::tokio_stream::iter(
            items,
        )))
    }

    #[tonic::async_trait]
    impl agent::remote_client::terminal_transport::wire::transcripts_server::Transcripts for Scripted {
        async fn list_transcripts(
            &self,
            _: tonic::Request<TranscriptScope>,
        ) -> Result<tonic::Response<ListTranscriptsResponse>, tonic::Status> {
            if let Some(message) = &self.0.list_error {
                return Err(tonic::Status::unavailable(message.clone()));
            }
            Ok(tonic::Response::new(self.0.list.clone()))
        }

        type ReadTranscriptTailStream = Items<TranscriptChunk>;
        async fn read_transcript_tail(
            &self,
            _: tonic::Request<ReadTranscriptRequest>,
        ) -> Result<tonic::Response<Self::ReadTranscriptTailStream>, tonic::Status> {
            Ok(stream(&self.0.tail))
        }

        async fn transcript_stable(
            &self,
            _: tonic::Request<ReadTranscriptRequest>,
        ) -> Result<tonic::Response<TranscriptStableResponse>, tonic::Status> {
            Ok(tonic::Response::new(self.0.stable.clone()))
        }

        type ReadDenySettingsStream = Items<ReadDenySettingsEvent>;
        async fn read_deny_settings(
            &self,
            _: tonic::Request<TranscriptScope>,
        ) -> Result<tonic::Response<Self::ReadDenySettingsStream>, tonic::Status> {
            Ok(stream(&self.0.settings))
        }
    }

    fn connection(capabilities: &[&str], script: Script) -> RemoteConnection {
        let client = HelperClient::serving_transcripts_for_test(
            "example-host",
            "SHA256:example",
            capabilities,
            Scripted(Arc::new(script)),
        );
        let project = RemoteProject {
            version: 1,
            host: "example-host".into(),
            host_key_sha256: "SHA256:example".into(),
            root: "/srv/project".into(),
            helper_version: "0.4.0".into(),
            sync: None,
            created_at: 1,
            queued_stops: Vec::new(),
        };
        RemoteConnection::with_client(client, project, String::new(), String::new())
    }

    fn served(script: Script) -> RemoteConnection {
        connection(&[wire::CAP_TRANSCRIPTS], script)
    }

    fn tab() -> TabScope {
        TabScope {
            root: "/srv/project".into(),
            canonical_root: "/srv/project".into(),
            account: String::new(),
            config_dir: String::new(),
        }
    }

    const CONFIG: &str = "/home/example/.claude";

    fn entry(id: &str, title: &str) -> TranscriptEntry {
        TranscriptEntry {
            provider_session_id: id.into(),
            len: 10,
            modified_unix_ms: 1,
            ai_title: title.into(),
        }
    }

    fn list_of(entries: Vec<TranscriptEntry>) -> ListTranscriptsResponse {
        ListTranscriptsResponse {
            entries,
            config_dir: CONFIG.into(),
            root: "/srv/project".into(),
            ..Default::default()
        }
    }

    fn chunk(data: usize, last: bool) -> TranscriptChunk {
        TranscriptChunk {
            data: vec![b'x'; data],
            last,
            ..Default::default()
        }
    }

    fn first_chunk(data: &[u8], last: bool) -> TranscriptChunk {
        TranscriptChunk {
            data: data.to_vec(),
            last,
            config_dir: CONFIG.into(),
            root: "/srv/project".into(),
            ..Default::default()
        }
    }

    fn scope_event(files: u32) -> ReadDenySettingsEvent {
        ReadDenySettingsEvent {
            event: Some(read_deny_settings_event::Event::Scope(SettingsScope {
                home: "/home/example".into(),
                config_dir: CONFIG.into(),
                files,
                root: "/srv/project".into(),
                root_gone: false,
            })),
        }
    }

    fn file_event(path: &str, bytes: usize) -> ReadDenySettingsEvent {
        ReadDenySettingsEvent {
            event: Some(read_deny_settings_event::Event::File(SettingsRead {
                path: path.into(),
                strict: false,
                bytes: vec![b' '; bytes],
                ..Default::default()
            })),
        }
    }

    fn failed_event(failed: &str) -> ReadDenySettingsEvent {
        ReadDenySettingsEvent {
            event: Some(read_deny_settings_event::Event::File(SettingsRead {
                path: "/srv/project/.claude/settings.json".into(),
                failed: failed.into(),
                ..Default::default()
            })),
        }
    }

    fn refusal_event(text: &str) -> ReadDenySettingsEvent {
        ReadDenySettingsEvent {
            event: Some(read_deny_settings_event::Event::Refusal(text.into())),
        }
    }

    #[test]
    fn a_whole_answer_of_each_call_is_returned() {
        let connection = served(Script {
            list_error: None,
            list: ListTranscriptsResponse {
                truncated: true,
                ..list_of(vec![entry(SESSION, "Fix the build")])
            },
            tail: vec![
                TranscriptChunk {
                    started_mid_file: true,
                    ..first_chunk(b"ab", false)
                },
                TranscriptChunk {
                    started_mid_file: true,
                    ..chunk(0, true)
                },
            ],
            stable: TranscriptStableResponse {
                stable: true,
                config_dir: CONFIG.into(),
                root: "/srv/project".into(),
                ..Default::default()
            },
            settings: vec![scope_event(1), file_event("/srv/project/.claude/settings.json", 3)],
        });
        let listed = connection.list_transcripts(&tab()).unwrap();
        assert_eq!(listed.entries, vec![entry(SESSION, "Fix the build")]);
        assert!(listed.truncated);
        assert_eq!(listed.used.config_dir, CONFIG);
        let tail = connection.read_transcript_tail(&tab(), SESSION).unwrap();
        assert_eq!((tail.bytes.as_slice(), tail.started_mid_file), (&b"ab"[..], true));
        assert_eq!(
            connection.transcript_stable(&tab(), SESSION).unwrap().stability,
            Stability::Stable
        );
        let settings = connection.read_deny_settings(&tab()).unwrap();
        assert_eq!(settings.home, "/home/example");
        assert_eq!(settings.files.len(), 1);
        assert_eq!(settings.files[0].bytes.len(), 3);
    }

    /// One case per bound: each answer is refused with the bound named before any of it is used.
    #[test]
    fn every_bound_refuses_before_use() {
        use wire::{
            SETTINGS_FILES_MAX, SETTINGS_FILE_MAX, TRANSCRIPTS_LIST_MAX, TRANSCRIPT_PATH_MAX, TRANSCRIPT_REFUSAL_MAX,
            TRANSCRIPT_TAIL_MAX, TRANSCRIPT_TITLE_MAX,
        };
        let long_path = format!("/{}", "p".repeat(TRANSCRIPT_PATH_MAX));
        let list = |entries: Vec<TranscriptEntry>| Script {
            list: list_of(entries),
            ..Default::default()
        };
        let tail = |chunks: Vec<TranscriptChunk>| Script {
            tail: chunks,
            ..Default::default()
        };
        let settings = |events: Vec<ReadDenySettingsEvent>| Script {
            settings: events,
            ..Default::default()
        };

        // Lists: the count, a title, the refusal's own text, a path.
        let too_many: Vec<_> = (0..=TRANSCRIPTS_LIST_MAX).map(|_| entry(SESSION, "")).collect();
        assert_eq!(
            served(list(too_many)).list_transcripts(&tab()).unwrap_err(),
            "the server's transcript list passed its bound of 200 entries; refused"
        );
        let long_title = "t".repeat(TRANSCRIPT_TITLE_MAX + 1);
        assert_eq!(
            served(list(vec![entry(SESSION, &long_title)]))
                .list_transcripts(&tab())
                .unwrap_err(),
            "a title in the server's transcript list is 1025 bytes long, past its bound of 1 KiB; refused"
        );
        let at_the_bound = "t".repeat(TRANSCRIPT_TITLE_MAX);
        let listed = served(list(vec![entry(SESSION, &at_the_bound)]))
            .list_transcripts(&tab())
            .unwrap();
        assert_eq!(listed.entries[0].ai_title, at_the_bound);
        let refusal = Script {
            list: ListTranscriptsResponse {
                refusal: "r".repeat(TRANSCRIPT_REFUSAL_MAX + 1),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            served(refusal).list_transcripts(&tab()).unwrap_err(),
            "the server's refusal is 1025 bytes long, past its bound of 1 KiB; refused"
        );
        let far_root = Script {
            list: ListTranscriptsResponse {
                root: long_path.clone(),
                ..list_of(Vec::new())
            },
            ..Default::default()
        };
        assert!(served(far_root)
            .list_transcripts(&tab())
            .unwrap_err()
            .starts_with("the server's project folder is "));

        // Tails: the data past the tail bound (under the transport's own byte allowance), a flood of empty chunks
        // past the byte allowance, a path.
        let over = TRANSCRIPT_TAIL_MAX as usize + 1;
        let mut chunks = vec![TranscriptChunk {
            config_dir: CONFIG.into(),
            root: "/srv/project".into(),
            ..chunk(1 << 20, false)
        }];
        chunks.extend((0..over / (1 << 20)).map(|_| chunk(1 << 20, false)));
        chunks.push(chunk(1, true));
        assert_eq!(
            served(tail(chunks)).read_transcript_tail(&tab(), SESSION).unwrap_err(),
            "the server's transcript tail is 33 MiB long, past its bound of 32 MiB; refused"
        );
        // Each of these costs its 4 KiB of padding and the framing, so the stream passes its 35 MiB allowance while
        // the data (none) stays far under the tail's own.
        let padded = TranscriptChunk {
            root: "r".repeat(4096),
            ..chunk(0, false)
        };
        let flood = vec![padded; (35 << 20) / 4227 + 10];
        assert_eq!(
            served(tail(flood)).read_transcript_tail(&tab(), SESSION).unwrap_err(),
            "the server's transcript tail passed its bound of 35 MiB; refused"
        );
        let far_chunk = TranscriptChunk {
            root: long_path.clone(),
            ..first_chunk(b"a", true)
        };
        assert!(served(tail(vec![far_chunk]))
            .read_transcript_tail(&tab(), SESSION)
            .unwrap_err()
            .starts_with("the server's project folder is "));
        let at_the_tail_bound = vec![first_chunk(&[b'x'; 1 << 20], false), chunk(0, true)];
        assert_eq!(
            served(tail(at_the_tail_bound))
                .read_transcript_tail(&tab(), SESSION)
                .unwrap()
                .bytes
                .len(),
            1 << 20
        );

        // Settings: the file count, a file, the total, a `failed` text, a path, a refusal's text.
        assert_eq!(
            served(settings(vec![scope_event(SETTINGS_FILES_MAX as u32 + 1)]))
                .read_deny_settings(&tab())
                .unwrap_err(),
            "the server's settings name 65 files, past their bound of 64; refused"
        );
        let big = vec![
            scope_event(1),
            file_event("/srv/project/.claude/settings.json", SETTINGS_FILE_MAX as usize + 1),
        ];
        assert_eq!(
            served(settings(big)).read_deny_settings(&tab()).unwrap_err(),
            "a settings file from the server is 4194305 bytes long, past its bound of 4 MiB; refused"
        );
        let four_files: Vec<_> = std::iter::once(scope_event(4))
            .chain((0..4).map(|_| file_event("/srv/project/.claude/settings.json", SETTINGS_FILE_MAX as usize)))
            .collect();
        // Four files of 4 MiB fill the total exactly and pass; one more byte in a fifth file is past it.
        assert_eq!(
            served(settings(four_files.clone()))
                .read_deny_settings(&tab())
                .unwrap()
                .files
                .len(),
            4
        );
        let five_files: Vec<_> = std::iter::once(scope_event(5))
            .chain(four_files.into_iter().skip(1))
            .chain([file_event("/srv/project/.claude/settings.json", 1)])
            .collect();
        assert_eq!(
            served(settings(five_files)).read_deny_settings(&tab()).unwrap_err(),
            "the server's settings files together is 16777217 bytes long, past its bound of 16 MiB; refused"
        );
        assert!(served(settings(vec![
            scope_event(1),
            failed_event(&"f".repeat(TRANSCRIPT_REFUSAL_MAX + 1))
        ]))
        .read_deny_settings(&tab())
        .unwrap_err()
        .starts_with("the reason a settings file could not be read, from the server is 1025 bytes long"));
        assert!(served(settings(vec![scope_event(1), file_event(&long_path, 0)]))
            .read_deny_settings(&tab())
            .unwrap_err()
            .starts_with("a settings path from the server is "));
        assert_eq!(
            served(settings(vec![refusal_event(&"r".repeat(TRANSCRIPT_REFUSAL_MAX + 1))]))
                .read_deny_settings(&tab())
                .unwrap_err(),
            "the server's refusal is 1025 bytes long, past its bound of 1 KiB; refused"
        );
    }

    /// A stream with more events than a scope and its files and a refusal can be is refused whole.
    #[test]
    fn a_settings_stream_with_more_events_than_the_bound_is_refused_whole() {
        let events: Vec<_> = std::iter::once(scope_event(64))
            .chain((0..66).map(|_| file_event("/srv/project/a", 0)))
            .collect();
        assert_eq!(
            served(Script {
                settings: events,
                ..Default::default()
            })
            .read_deny_settings(&tab())
            .unwrap_err(),
            "the server's settings passed its bound of 66 entries; refused"
        );
    }

    /// The wrapper returns a title as the server sent it; showing one escapes it where it is drawn.
    #[test]
    fn titles_are_returned_raw() {
        let title = "\u{1b}[31mred\u{1b}[0m\nsecond line\u{202e}";
        let listed = served(Script {
            list: list_of(vec![entry(SESSION, title)]),
            ..Default::default()
        })
        .list_transcripts(&tab())
        .unwrap();
        assert_eq!(listed.entries[0].ai_title, title);
    }

    #[test]
    fn a_short_settings_stream_is_refused() {
        let events = vec![
            scope_event(3),
            file_event("/srv/project/.claude/settings.json", 1),
            file_event("/srv/project/.claude/settings.local.json", 1),
        ];
        let refused = served(Script {
            settings: events,
            ..Default::default()
        })
        .read_deny_settings(&tab())
        .unwrap_err();
        assert_eq!(
            refused,
            "the server's settings ended after 2 of the 3 files their scope named; refused as incomplete"
        );
        // A stream that ends before its scope, one with a refusal in the middle and one with too many are refused too.
        let none = served(Script::default()).read_deny_settings(&tab()).unwrap_err();
        assert_eq!(
            none,
            "the server's settings ended before their scope; refused as incomplete"
        );
        let cut = vec![
            scope_event(2),
            file_event("/srv/project/a", 1),
            refusal_event("the read took longer than 30 s"),
        ];
        assert_eq!(
            served(Script {
                settings: cut,
                ..Default::default()
            })
            .read_deny_settings(&tab())
            .unwrap_err(),
            "the read took longer than 30 s"
        );
        let extra = vec![
            scope_event(1),
            file_event("/srv/project/a", 1),
            file_event("/srv/project/b", 1),
        ];
        assert_eq!(
            served(Script {
                settings: extra,
                ..Default::default()
            })
            .read_deny_settings(&tab())
            .unwrap_err(),
            "the server's settings held more than the 1 files their scope named; refused"
        );
    }

    #[test]
    fn a_tail_without_its_last_chunk_or_with_a_refusal_is_refused() {
        let tail = |chunks| Script {
            tail: chunks,
            ..Default::default()
        };
        assert_eq!(
            served(tail(Vec::new()))
                .read_transcript_tail(&tab(), SESSION)
                .unwrap_err(),
            "the server's transcript tail ended before its first chunk; refused as incomplete"
        );
        assert_eq!(
            served(tail(vec![first_chunk(b"a", false)]))
                .read_transcript_tail(&tab(), SESSION)
                .unwrap_err(),
            "the server's transcript tail ended without its last chunk; refused as incomplete"
        );
        let refused = vec![
            first_chunk(b"a", false),
            TranscriptChunk {
                refusal: "the transcript could not be read: input/output error".into(),
                ..Default::default()
            },
        ];
        assert_eq!(
            served(tail(refused)).read_transcript_tail(&tab(), SESSION).unwrap_err(),
            "the transcript could not be read: input/output error"
        );
        assert_eq!(
            served(tail(vec![first_chunk(b"a", true), chunk(1, true)]))
                .read_transcript_tail(&tab(), SESSION)
                .unwrap_err(),
            "the server's transcript tail went on after its last chunk; refused"
        );
    }

    #[test]
    fn a_session_id_not_in_canonical_form_refuses_the_list() {
        let refused = "a session id in the server's transcript list is not a lowercase hyphenated UUID; refused";
        for bad in [
            "0B7A7D3E-5F0C-4C52-9A0E-3A1F0C6D2B11",
            "0b7a7d3e5f0c4c529a0e3a1f0c6d2b11",
            "{0b7a7d3e-5f0c-4c52-9a0e-3a1f0c6d2b11}",
            "urn:uuid:0b7a7d3e-5f0c-4c52-9a0e-3a1f0c6d2b11",
            "../../0b7a7d3e-5f0c-4c52-9a0e-3a1f0c6d2b11",
            "",
        ] {
            // One bad id among good ones refuses the whole list: no entry of it is shown.
            let connection = served(Script {
                list: list_of(vec![entry(SESSION, "good"), entry(bad, "bad")]),
                ..Default::default()
            });
            assert_eq!(connection.list_transcripts(&tab()).unwrap_err(), refused, "{bad:?}");
        }
        // The same rule keeps a session id of that form from being sent in a request.
        let connection = served(Script::default());
        let refused = "the session id is not a lowercase hyphenated UUID; refused";
        assert_eq!(connection.read_transcript_tail(&tab(), "../x").unwrap_err(), refused);
        assert_eq!(connection.transcript_stable(&tab(), "../x").unwrap_err(), refused);
    }

    #[test]
    fn an_answer_for_another_root_is_refused() {
        let other = "/srv/other";
        let refused = "the answer from the helper on example-host is for \"/srv/other\", not for this project's folder \"/srv/project\"; refused";
        let connection = served(Script {
            list_error: None,
            list: ListTranscriptsResponse {
                root: other.into(),
                ..list_of(Vec::new())
            },
            tail: vec![TranscriptChunk {
                root: other.into(),
                ..first_chunk(b"a", true)
            }],
            stable: TranscriptStableResponse {
                root: other.into(),
                ..Default::default()
            },
            settings: vec![ReadDenySettingsEvent {
                event: Some(read_deny_settings_event::Event::Scope(SettingsScope {
                    root: other.into(),
                    ..Default::default()
                })),
            }],
        });
        assert_eq!(connection.list_transcripts(&tab()).unwrap_err(), refused);
        assert_eq!(connection.read_transcript_tail(&tab(), SESSION).unwrap_err(), refused);
        assert_eq!(connection.transcript_stable(&tab(), SESSION).unwrap_err(), refused);
        assert_eq!(connection.read_deny_settings(&tab()).unwrap_err(), refused);

        // A tab whose root is reached through a link is told its canonical root: that answer is its own, and the
        // answer naming the tab's own spelling is not.
        let linked = TabScope {
            root: "/home/example/project".into(),
            canonical_root: "/var/home/example/project".into(),
            ..tab()
        };
        let answer = |root: &str, root_gone| ListTranscriptsResponse {
            root: root.into(),
            root_gone,
            ..list_of(Vec::new())
        };
        let connection = |answer| {
            served(Script {
                list: answer,
                ..Default::default()
            })
        };
        assert!(connection(answer("/var/home/example/project", false))
            .list_transcripts(&linked)
            .is_ok());
        assert!(connection(answer("/home/example/project", false))
            .list_transcripts(&linked)
            .is_err());
        // A folder that is gone there is named by the text that was sent, or by the canonical path it had.
        assert!(connection(answer("/home/example/project", true))
            .list_transcripts(&linked)
            .is_ok());
        assert!(connection(answer("/var/home/example/project", true))
            .list_transcripts(&linked)
            .is_ok());
        assert!(connection(answer(other, true)).list_transcripts(&linked).is_err());
    }

    #[test]
    fn a_missing_capability_is_named() {
        let connection = connection(&["sync-files"], Script::default());
        let missing = "the server's Eitri helper on example-host does not offer transcripts";
        assert_eq!(connection.list_transcripts(&tab()).unwrap_err(), missing);
        assert_eq!(connection.read_transcript_tail(&tab(), SESSION).unwrap_err(), missing);
        assert_eq!(connection.transcript_stable(&tab(), SESSION).unwrap_err(), missing);
        assert_eq!(connection.read_deny_settings(&tab()).unwrap_err(), missing);
    }

    #[test]
    fn a_refusal_reaches_the_caller_as_the_servers_plain_line() {
        let connection = served(Script {
            list: ListTranscriptsResponse {
                refusal: "the config directory is not the account's own".into(),
                ..Default::default()
            },
            stable: TranscriptStableResponse {
                refusal: "the transcript is not a regular file".into(),
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(
            connection.list_transcripts(&tab()).unwrap_err(),
            "the config directory is not the account's own"
        );
        assert_eq!(
            connection.transcript_stable(&tab(), SESSION).unwrap_err(),
            "the transcript is not a regular file"
        );
    }

    #[test]
    fn stable_moving_and_missing_are_told_apart() {
        let answer = |stable, missing| TranscriptStableResponse {
            stable,
            missing,
            config_dir: CONFIG.into(),
            root: "/srv/project".into(),
            ..Default::default()
        };
        for (stable, missing, expected) in [
            (true, false, Stability::Stable),
            (false, false, Stability::Moving),
            (false, true, Stability::Missing),
        ] {
            let connection = served(Script {
                stable: answer(stable, missing),
                ..Default::default()
            });
            assert_eq!(
                connection.transcript_stable(&tab(), SESSION).unwrap().stability,
                expected
            );
        }
    }

    fn session(n: usize) -> String {
        format!("0b7a7d3e-5f0c-4c52-9a0e-{n:012x}")
    }

    /// A list is one message: entries that encode to almost nothing are stopped by the decoder's limit for a list,
    /// before they are ever counted, not by the stream limit a settings file needs.
    #[test]
    fn a_list_past_its_decode_limit_is_refused_by_the_decoder() {
        let many = agent::remote_client::TRANSCRIPT_LIST_MESSAGE_MAX / 2 + 100;
        let refused = served(Script {
            list: list_of(vec![TranscriptEntry::default(); many]),
            ..Default::default()
        })
        .list_transcripts(&tab())
        .unwrap_err();
        assert!(refused.contains("too large"), "{refused}");
        assert!(!refused.contains("past its bound"), "{refused}");
    }

    #[test]
    fn the_largest_answers_within_their_bounds_pass() {
        let entries: Vec<_> = (0..wire::TRANSCRIPTS_LIST_MAX)
            .map(|n| entry(&session(n), &"t".repeat(wire::TRANSCRIPT_TITLE_MAX)))
            .collect();
        let listed = served(Script {
            list: list_of(entries),
            ..Default::default()
        })
        .list_transcripts(&tab())
        .unwrap();
        assert_eq!(listed.entries.len(), wire::TRANSCRIPTS_LIST_MAX);

        let chunk_bytes = wire::TRANSCRIPT_CHUNK;
        let count = usize::try_from(wire::TRANSCRIPT_TAIL_MAX).unwrap() / chunk_bytes;
        let mut tail = vec![first_chunk(&vec![b'x'; chunk_bytes], false)];
        tail.extend((1..count - 1).map(|_| chunk(chunk_bytes, false)));
        tail.push(chunk(chunk_bytes, true));
        let read = served(Script {
            tail,
            ..Default::default()
        })
        .read_transcript_tail(&tab(), SESSION)
        .unwrap();
        assert_eq!(read.bytes.len() as u64, wire::TRANSCRIPT_TAIL_MAX);

        let files = wire::SETTINGS_FILES_MAX;
        let each = usize::try_from(wire::SETTINGS_TOTAL_MAX).unwrap() / files;
        let events: Vec<_> = std::iter::once(scope_event(files as u32))
            .chain((0..files).map(|n| file_event(&format!("/srv/project/.claude/s{n}.json"), each)))
            .collect();
        let settings = served(Script {
            settings: events,
            ..Default::default()
        })
        .read_deny_settings(&tab())
        .unwrap();
        assert_eq!(settings.files.len(), files);
    }

    #[test]
    fn every_order_of_settings_events_but_scope_then_files_is_refused() {
        let empty = ReadDenySettingsEvent { event: None };
        for (events, expected) in [
            (vec![], "ended before their scope"),
            (
                vec![file_event("/srv/project/.claude/settings.json", 1)],
                "began with a file",
            ),
            (vec![empty.clone()], "began with an empty event"),
            (vec![scope_event(1), empty.clone()], "held an empty event"),
            (vec![scope_event(1), scope_event(1)], "repeated their scope"),
            (vec![scope_event(1), refusal_event("no")], "no"),
        ] {
            let refused = served(Script {
                settings: events,
                ..Default::default()
            })
            .read_deny_settings(&tab())
            .unwrap_err();
            assert!(refused.contains(expected), "{refused}");
        }
        let none = served(Script {
            settings: vec![scope_event(0)],
            ..Default::default()
        })
        .read_deny_settings(&tab())
        .unwrap();
        assert!(none.files.is_empty());
    }

    #[test]
    fn the_paths_an_answer_names_are_bounded_and_absolute() {
        let long = format!("/{}", "c".repeat(wire::TRANSCRIPT_PATH_MAX));
        let refused = served(Script {
            list: ListTranscriptsResponse {
                config_dir: long.clone(),
                ..list_of(Vec::new())
            },
            ..Default::default()
        })
        .list_transcripts(&tab())
        .unwrap_err();
        assert!(refused.starts_with("the server's config directory"), "{refused}");
        let home = ReadDenySettingsEvent {
            event: Some(read_deny_settings_event::Event::Scope(SettingsScope {
                home: long,
                config_dir: CONFIG.into(),
                files: 0,
                root: "/srv/project".into(),
                root_gone: false,
            })),
        };
        let refused = served(Script {
            settings: vec![home],
            ..Default::default()
        })
        .read_deny_settings(&tab())
        .unwrap_err();
        assert!(refused.starts_with("the server's home directory"), "{refused}");
        for root in ["", "srv/project"] {
            let refused = served(Script {
                list: ListTranscriptsResponse {
                    root: root.into(),
                    ..list_of(Vec::new())
                },
                ..Default::default()
            })
            .list_transcripts(&tab())
            .unwrap_err();
            assert!(refused.contains("names no absolute project folder"), "{refused}");
        }
        let unnamed = TabScope {
            canonical_root: String::new(),
            ..tab()
        };
        let refused = served(Script::default()).list_transcripts(&unnamed).unwrap_err();
        assert!(refused.contains("has not been named by its trust scan"), "{refused}");
    }

    /// Server text in a refusal or a failed call comes back escaped and cut to the refusal's bound.
    #[test]
    fn refusals_and_failed_calls_are_plain_lines() {
        let words = format!("gone\n\u{1b}[31m\u{202e}{}", "w".repeat(5000));
        let failed = served(Script {
            list_error: Some(words),
            ..Default::default()
        })
        .list_transcripts(&tab())
        .unwrap_err();
        assert!(
            failed.starts_with("the helper's transcripts call failed (Unavailable): gone"),
            "{failed}"
        );
        assert!(!failed.chars().any(char::is_control), "{failed}");
        assert!(failed.chars().count() <= wire::TRANSCRIPT_REFUSAL_MAX);
        let refused = served(Script {
            tail: vec![TranscriptChunk {
                refusal: "moved\n\u{1b}away".into(),
                ..first_chunk(b"", false)
            }],
            ..Default::default()
        })
        .read_transcript_tail(&tab(), SESSION)
        .unwrap_err();
        assert!(refused.starts_with("moved"), "{refused}");
        assert!(!refused.chars().any(char::is_control), "{refused}");
    }

    fn settings_of(config_dir: &str, home: &str, files: Vec<SettingsRead>) -> ServerSettings {
        ServerSettings {
            home: home.into(),
            used: Used {
                config_dir: config_dir.into(),
                root: "/srv/project".into(),
                root_gone: false,
            },
            files,
        }
    }

    fn denying(rule: &str) -> Vec<u8> {
        format!(r#"{{"permissions":{{"deny":["{rule}"]}}}}"#).into_bytes()
    }

    /// Each settings file is what its fields say, a failure first, then a missing file, then its bytes; the rules are
    /// anchored at the server's root with `~` as the server's home; no config directory named closes the reader.
    #[test]
    fn server_settings_become_rules_as_a_local_tabs_do() {
        let root = std::path::Path::new("/srv/project");
        let file = |failed: &str, missing: bool, bytes: Vec<u8>| SettingsRead {
            path: "/srv/project/.claude/settings.local.json".into(),
            strict: false,
            bytes,
            missing,
            failed: failed.into(),
        };
        let rules = settings_of(
            CONFIG,
            "/home/example",
            vec![file("", false, denying("Read(secret.txt)"))],
        )
        .read_rules();
        assert_eq!(rules.closed(), None);
        assert!(rules.denies(root, &root.join("secret.txt")));
        assert!(!rules.denies(root, &root.join("a.rs")));
        // Missing wins over bytes: nothing is read from that file.
        let rules = settings_of(
            CONFIG,
            "/home/example",
            vec![file("", true, denying("Read(secret.txt)"))],
        )
        .read_rules();
        assert_eq!(rules.closed(), None);
        assert!(!rules.denies(root, &root.join("secret.txt")));
        // A failure wins over everything, closes the reader, and its words come plain.
        let rules = settings_of(
            CONFIG,
            "/home/example",
            vec![file("no\n\u{1b}[31m", true, denying("Read(x)"))],
        )
        .read_rules();
        let why = rules.closed().expect("a failed file closes the reader");
        assert!(!why.chars().any(char::is_control), "{why}");
        assert!(rules.denies(root, &root.join("a.rs")));
        // No config directory named: the user tier is not known, so nothing can be told.
        assert!(settings_of("", "/home/example", Vec::new())
            .read_rules()
            .closed()
            .is_some());
        // `~` is the server's home.
        let rules = settings_of(
            CONFIG,
            "/srv",
            vec![file("", false, denying("Read(~/project/secret.txt)"))],
        )
        .read_rules();
        assert!(rules.denies(root, &root.join("secret.txt")));
        assert!(!rules.denies(root, &root.join("a.rs")));
    }
}
