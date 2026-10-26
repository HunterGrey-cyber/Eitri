//! The prompt choice through real gRPC requests and a provider handoff. No real CLI runs.
#![cfg(target_os = "linux")]

use agent::providers::claude_sidecar::{MoveRefusedWhy, EXPECTED_VERDANDI_REVISION};
use agent::setting_sources::ProjectTrust;
use agent::{
    AgentDomainEvent, AgentProvider, ClaudeSidecarProvider, CloseSessionRequest, CreateSessionRequest,
    ResumeSessionRequest, StreamingPreference,
};
use claude_runtime_protocol::v1::CreateSessionRequest as ProtoCreateSessionRequest;
use prost::Message;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[path = "../src/providers/claude_sidecar/system_prompt.rs"]
mod system_prompt;

const HANG: Duration = Duration::from_secs(30);

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn wait_for(mut done: impl FnMut() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(started.elapsed() < HANG, "stand-in operation never completed");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn opened(provider: &ClaudeSidecarProvider, session: &str) -> String {
    let mut id = None;
    wait_for(|| {
        for event in provider.pump() {
            if let AgentDomainEvent::SessionOpened {
                session_id,
                provider_session_id,
                ..
            } = event
            {
                assert_eq!(session_id, session);
                id = Some(provider_session_id);
            }
        }
        id.is_some()
    });
    id.unwrap()
}

fn close_and_await_watch(provider: &ClaudeSidecarProvider, session: String) {
    provider
        .close_session(CloseSessionRequest { session_id: session })
        .unwrap();
    wait_for(|| {
        provider.pump();
        match provider.freeze_for_move() {
            Err(MoveRefusedWhy::NoSession) => true,
            Ok((_, fds)) => {
                // The watch has read the close but has not finished returning yet.
                drop(fds);
                provider.unfreeze().unwrap();
                false
            }
            other => panic!("unexpected state after closing the only session: {other:?}"),
        }
    });
}

fn recorded(path: &Path) -> Vec<ProtoCreateSessionRequest> {
    let bytes = std::fs::read(path).unwrap();
    let mut rest = bytes.as_slice();
    let mut requests = Vec::new();
    while !rest.is_empty() {
        let length = u32::from_le_bytes(rest[..4].try_into().unwrap()) as usize;
        rest = &rest[4..];
        requests.push(ProtoCreateSessionRequest::decode(&rest[..length]).unwrap());
        rest = &rest[length..];
    }
    requests
}

fn assert_diagnostic(provider: &ClaudeSidecarProvider, offered: bool) {
    let diagnostics = provider.info().startup_diagnostics;
    let prompt_notes: Vec<_> = diagnostics
        .iter()
        .filter(|line| line.starts_with("Claude Code's system prompt unavailable:"))
        .collect();
    if offered {
        assert!(prompt_notes.is_empty());
    } else {
        assert_eq!(prompt_notes, vec![&format!(
            "Claude Code's system prompt unavailable: this sidecar does not offer system_prompt_preset, so Claude runs with the Agent SDK's minimal prompt. Build the sidecar for Verdandi {EXPECTED_VERDANDI_REVISION}."
        )]);
    }
}

#[test]
fn grpc_fresh_resume_and_adopt_preserve_the_prompt_choice() {
    for offered in [true, false] {
        let dir = Scratch(std::env::temp_dir().join(format!("eitri-prompt-{}", uuid::Uuid::new_v4())));
        agent::private_fs::create_private_dir_all(&dir.0, &dir.0).unwrap();
        let record = dir.0.join("requests.bin");
        let launcher = dir.0.join("sidecar");
        let quote = |path: &Path| format!("'{}'", path.display().to_string().replace('\'', "'\\''"));
        let script = format!(
            "#!/bin/sh\nexec {} --record-create {} {}\n",
            quote(Path::new(env!("CARGO_BIN_EXE_eitri-sidecar-standin"))),
            quote(&record),
            if offered { "" } else { "--without-system-prompt-preset" }
        );
        agent::private_fs::write_private(&launcher, script.as_bytes()).unwrap();
        std::fs::set_permissions(&launcher, std::fs::Permissions::from_mode(0o700)).unwrap();
        let source = ClaudeSidecarProvider::connect_to_program(&uuid::Uuid::new_v4().to_string(), &launcher).unwrap();
        assert!(source.capabilities().resume, "the peer must offer resume_session");
        assert_diagnostic(&source, offered);
        let fresh = source
            .create_session(CreateSessionRequest {
                cwd: dir.0.to_string_lossy().into_owned(),
                streaming: StreamingPreference::Partial,
                project: ProjectTrust::Untrusted,
            })
            .unwrap();
        let provider_id = opened(&source, &fresh);
        close_and_await_watch(&source, fresh);
        let resume = || ResumeSessionRequest {
            provider_session_id: provider_id.clone(),
            cwd: dir.0.to_string_lossy().into_owned(),
            streaming: StreamingPreference::Partial,
            project: ProjectTrust::Untrusted,
        };
        let session = source.resume_session(resume()).expect("resume must run, never skip");
        assert_eq!(opened(&source, &session), provider_id);
        let before_move = recorded(&record);
        assert_eq!(before_move.len(), 2);
        let (handoff, fds) = source.freeze_for_move().expect("exactly one live watch");
        let destination = ClaudeSidecarProvider::adopt(handoff, fds).unwrap();
        assert_diagnostic(&destination, offered);
        destination.commit().unwrap();
        source.forget_for_move();
        drop(source);
        assert_eq!(recorded(&record), before_move, "freeze/adopt sends no create request");
        close_and_await_watch(&destination, session);
        let session = destination.resume_session(resume()).unwrap();
        assert_eq!(opened(&destination, &session), provider_id);
        let requests = recorded(&record);
        assert_eq!(requests.len(), 3);
        for (i, request) in requests.iter().enumerate() {
            assert_eq!(request.system_prompt, None);
            assert_eq!(
                request.system_prompt_preset,
                offered.then(|| claude_runtime_protocol::v1::SystemPromptPreset {
                    append: Some(system_prompt::EITRI_APPEND.into()),
                })
            );
            assert_eq!(
                request.resume_provider_session_id.as_deref(),
                (i > 0).then_some(provider_id.as_str())
            );
        }
        close_and_await_watch(&destination, session);
        drop(destination);
    }
}
