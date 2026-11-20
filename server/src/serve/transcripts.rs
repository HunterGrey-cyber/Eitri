//! The daemon's `Transcripts` service: the account's session transcripts and the CLI's settings files on this
//! machine, read-only. The helper never interprets a transcript's content; the laptop parses what it receives.

use std::sync::Arc;
use std::time::Duration;

use eitri_server_proto::read_deny_settings_event::Event;
use eitri_server_proto::transcripts_server::{Transcripts, TranscriptsServer};
use eitri_server_proto::*;
use tonic::{Request, Response, Status};

use super::super::login_env::Captured;
use super::super::sidecars::Sidecars;
use super::super::{mark_activity, off_runtime, transcripts, Outbox, Streamed};

enum Login {
    Sidecars(Arc<Sidecars>),
    /// An environment a test names itself.
    #[cfg(test)]
    Fixed(Arc<Captured>),
    /// A login environment that cannot be had, for the reason a test gives.
    #[cfg(test)]
    Unavailable(String),
}

/// The `Transcripts` service. The account's `$HOME` and `CLAUDE_CONFIG_DIR` are the login environment's, the one the
/// sidecars start in.
pub struct TranscriptsService {
    login: Login,
    /// How long a streamed answer may take to be read.
    deadline: Duration,
}

impl TranscriptsService {
    pub fn new(sidecars: Arc<Sidecars>) -> Self {
        Self {
            login: Login::Sidecars(sidecars),
            deadline: SYNC_STREAM_DEADLINE,
        }
    }

    #[cfg(test)]
    pub fn with_environment(env: super::super::login_env::Environment) -> Self {
        Self {
            login: Login::Fixed(Arc::new(Captured { env, fallback: None })),
            deadline: SYNC_STREAM_DEADLINE,
        }
    }

    /// A service whose login environment is unavailable for `why`, as a sidecar's refusal words it.
    #[cfg(test)]
    pub fn without_environment(why: &str) -> Self {
        Self {
            login: Login::Unavailable(why.to_owned()),
            deadline: SYNC_STREAM_DEADLINE,
        }
    }

    /// The same service with streamed answers that must be read within `deadline`.
    #[cfg(test)]
    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }

    async fn environment(&self) -> Result<Arc<Captured>, String> {
        match &self.login {
            Login::Sidecars(sidecars) => sidecars
                .login_environment()
                .await
                .map_err(|status| format!("the login environment is unavailable: {}", status.message())),
            #[cfg(test)]
            Login::Fixed(captured) => Ok(captured.clone()),
            #[cfg(test)]
            Login::Unavailable(why) => Err(format!("the login environment is unavailable: {why}")),
        }
    }
}

/// The service as the daemon registers it.
pub fn server(sidecars: Arc<Sidecars>) -> TranscriptsServer<TranscriptsService> {
    TranscriptsServer::new(TranscriptsService::new(sidecars))
}

fn event(event: Event) -> ReadDenySettingsEvent {
    ReadDenySettingsEvent { event: Some(event) }
}

#[tonic::async_trait]
impl Transcripts for TranscriptsService {
    async fn list_transcripts(
        &self,
        request: Request<TranscriptScope>,
    ) -> Result<Response<ListTranscriptsResponse>, Status> {
        mark_activity(&request);
        let scope = request.into_inner();
        let env = match self.environment().await {
            Ok(captured) => captured,
            Err(refusal) => {
                return Ok(Response::new(ListTranscriptsResponse {
                    refusal: transcripts::bounded(refusal),
                    ..Default::default()
                }))
            }
        };
        let response = off_runtime(move || transcripts::list(&scope, &env.env)).await?;
        Ok(Response::new(response))
    }

    type ReadTranscriptTailStream = Streamed<TranscriptChunk>;
    async fn read_transcript_tail(
        &self,
        request: Request<ReadTranscriptRequest>,
    ) -> Result<Response<Self::ReadTranscriptTailStream>, Status> {
        mark_activity(&request);
        let request = request.into_inner();
        let env = self.environment().await;
        let deadline = self.deadline;
        let (mut outbox, stream) = Outbox::new(deadline);
        tokio::task::spawn_blocking(move || {
            // The last chunk is held back and ends the stream through the outbox's own slot.
            let mut last: Option<TranscriptChunk> = None;
            let outcome = (|| {
                let env = env?;
                let scope = request.scope.unwrap_or_default();
                let (used, path) = transcripts::session_file(&scope, &request.provider_session_id, &env.env)?;
                let tail = transcripts::open_tail(&path)?;
                let started_mid_file = tail.started_mid_file();
                // What the call used goes out on the first chunk only.
                let mut first = Some(used);
                tail.chunks(&mut |data, is_last| {
                    let used = first.take().unwrap_or_default();
                    let chunk = TranscriptChunk {
                        data,
                        started_mid_file,
                        last: is_last,
                        refusal: String::new(),
                        config_dir: used.config_dir,
                        root: used.root,
                        root_gone: used.root_gone,
                    };
                    if is_last {
                        last = Some(chunk);
                        true
                    } else {
                        outbox.send(chunk)
                    }
                })
                .map_err(|error| format!("the transcript could not be read: {error}"))
            })();
            let refusal = match outcome {
                Err(refusal) => refusal,
                Ok(()) if outbox.late => format!("the read took longer than {} s", deadline.as_secs()),
                Ok(()) => {
                    // Nobody reading any more leaves no last chunk to send.
                    if let Some(last) = last {
                        outbox.finish(last);
                    }
                    return;
                }
            };
            outbox.finish(TranscriptChunk {
                refusal: transcripts::bounded(refusal),
                ..Default::default()
            });
        });
        Ok(Response::new(stream))
    }

    async fn transcript_stable(
        &self,
        request: Request<ReadTranscriptRequest>,
    ) -> Result<Response<TranscriptStableResponse>, Status> {
        mark_activity(&request);
        let request = request.into_inner();
        let env = match self.environment().await {
            Ok(captured) => captured,
            Err(refusal) => {
                return Ok(Response::new(TranscriptStableResponse {
                    refusal: transcripts::bounded(refusal),
                    ..Default::default()
                }))
            }
        };
        let response = off_runtime(move || {
            let scope = request.scope.unwrap_or_default();
            let probed =
                transcripts::session_file(&scope, &request.provider_session_id, &env.env).and_then(|(used, path)| {
                    transcripts::stable(&path, transcripts::STABLE_PROBE).map(|found| (used, found))
                });
            match probed {
                Ok((used, found)) => TranscriptStableResponse {
                    stable: found == transcripts::Stability::Stable,
                    missing: found == transcripts::Stability::Missing,
                    refusal: String::new(),
                    config_dir: used.config_dir,
                    root: used.root,
                    root_gone: used.root_gone,
                },
                Err(refusal) => TranscriptStableResponse {
                    refusal: transcripts::bounded(refusal),
                    ..Default::default()
                },
            }
        })
        .await?;
        Ok(Response::new(response))
    }

    type ReadDenySettingsStream = Streamed<ReadDenySettingsEvent>;
    async fn read_deny_settings(
        &self,
        request: Request<TranscriptScope>,
    ) -> Result<Response<Self::ReadDenySettingsStream>, Status> {
        mark_activity(&request);
        let scope = request.into_inner();
        let env = self.environment().await;
        let deadline = self.deadline;
        let (mut outbox, stream) = Outbox::new(deadline);
        tokio::task::spawn_blocking(move || {
            let answer = env.and_then(|captured| transcripts::settings(&scope, &captured.env));
            let (head, files) = match answer {
                Ok(answer) => answer,
                Err(refusal) => return outbox.finish(event(Event::Refusal(transcripts::bounded(refusal)))),
            };
            // Every file goes out, or the stream ends in a refusal: a list the laptop could take for complete while
            // it is short is the one thing this must not send.
            let mut events: Vec<_> = std::iter::once(event(Event::Scope(head)))
                .chain(files.into_iter().map(|file| event(Event::File(file))))
                .collect();
            let last = events.pop().expect("the scope is always there");
            for item in events {
                if !outbox.send(item) {
                    if outbox.late {
                        let refusal = format!("the read took longer than {} s", deadline.as_secs());
                        outbox.finish(event(Event::Refusal(refusal)));
                    }
                    return;
                }
            }
            outbox.finish(last);
        });
        Ok(Response::new(stream))
    }
}
