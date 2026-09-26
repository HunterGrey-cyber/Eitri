//! Real, `#[ignore]`d tests against a real, freshly-spawned `claude-sidecar` process -- but no real
//! Claude API cost: every test here only exercises unary RPC round trips whose *shape* the sidecar
//! itself can confirm without ever issuing a query (a session_id/turn_id coming back, a handshake,
//! or a well-formed provider error). Real end-to-end conversation content is Task 9's job.
//!
//! "No query", not "no `claude` process": the sidecar shells out to `claude --version` once at
//! startup for its own CLI-compatibility check (`runtimeServiceImpl.ts::getActualClaudeCodeVersion`),
//! so the binary does run. A version probe sends no messages and costs no tokens. `claude` must
//! therefore be installed for these to pass -- the cost claim is about billing, not about
//! dependencies.

use agent::{AgentProvider, ClaudeSidecarProvider, CreateSessionRequest, PermissionMode};

#[test]
#[ignore]
fn create_session_returns_a_real_session_id() {
    let provider = ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string()).unwrap();
    let session_id = provider
        .create_session(CreateSessionRequest {
            cwd: std::env::temp_dir().to_string_lossy().to_string(),
            permission_mode: PermissionMode::Auto,
            streaming: agent::StreamingPreference::Partial,
        })
        .unwrap();
    assert!(!session_id.is_empty());
    provider
        .close_session(agent::CloseSessionRequest { session_id })
        .unwrap();
}

#[test]
#[ignore]
fn resume_is_now_advertised_and_an_empty_id_is_rejected_before_the_wire() {
    // This test used to assert resume was honestly UNSUPPORTED. It is supported as of the Verdandi
    // protocol change that added resume_provider_session_id to CreateSession; what it pins now is
    // the replacement honesty property -- the capability is real, and a request that cannot be
    // honored fails as a typed error rather than silently starting a fresh session.
    let provider = ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string()).unwrap();
    assert!(
        provider.capabilities().resume,
        "the sidecar must advertise resume_session"
    );
    assert!(
        !provider.capabilities().fork,
        "fork is on the wire but this client does not drive it yet"
    );

    let result = provider.resume_session(agent::ResumeSessionRequest {
        provider_session_id: "   ".into(),
        cwd: "/tmp".into(),
        permission_mode: PermissionMode::Bypass,
        streaming: agent::StreamingPreference::Partial,
    });
    match result {
        Err(agent::ProviderError::Provider { code, .. }) => {
            assert_eq!(code, agent::ProviderErrorCode::InvalidConfiguration);
        }
        other => panic!("an empty resume id must be a typed error, never a fresh session: {other:?}"),
    }
}

#[test]
#[ignore]
fn close_session_on_an_unknown_id_surfaces_a_typed_provider_error() {
    let provider = ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string()).unwrap();
    let result = provider.close_session(agent::CloseSessionRequest {
        session_id: "does-not-exist".into(),
    });
    // The typed code, not just "some provider error": the sidecar encodes a real SESSION_NOT_FOUND
    // in the grpc-status-details-bin trailer, and this crate now carries it across the boundary so a
    // caller can distinguish a dead session from a recoverable ordering complaint without parsing
    // English. Asserting only `Provider(_)` would have passed even while the code was discarded.
    match result {
        Err(agent::ProviderError::Provider { code, message }) => {
            assert_eq!(
                code,
                agent::ProviderErrorCode::SessionNotFound,
                "message was: {message}"
            );
            assert!(
                !code.is_benign(),
                "an unknown session is not something to continue past"
            );
        }
        other => panic!("expected a typed provider error, got: {other:?}"),
    }
}

/// The one check that can actually catch handshake drift.
///
/// `real_handshake_today()` in `agent/src/providers/claude_sidecar/mod.rs` is a hand-written literal
/// whose doc comment claims it is "the exact shape today's real sidecar returns". Nothing verified
/// that. On 2026-09-15 a cross-repository review found it still saying `protocol_major: 1` with
/// seven capabilities, while `CLIENT_PROTOCOL_MAJOR` was 2 and the pinned sidecar advertised nine --
/// it described a peer `connect()` would have REFUSED outright, and every unit test around it stayed
/// green throughout, because they all read that same literal.
///
/// So this asserts against the live process instead. It is the cheapest real test in the crate:
/// `connect()` performs the handshake and stops -- no session is created, no turn is sent, nothing
/// reaches the model, nothing is billed. (The sidecar does run `claude --version` at startup for its
/// own compatibility check; that is a version probe, not a query.)
///
/// Run it after bumping `EXPECTED_VERDANDI_REVISION`, and update the fixture from what it reports.
#[test]
#[ignore]
fn the_live_handshake_still_matches_the_fixture_in_mod_rs() {
    let provider = ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string()).unwrap();
    let info = provider.info();

    // The value `connect()` itself refuses on, so reaching this line already proves it -- asserted
    // anyway, because that number is what the fixture most needs to be right about.
    assert_eq!(
        info.protocol_major, 3,
        "CLIENT_PROTOCOL_MAJOR and the fixture must both say this"
    );

    let expected: Vec<String> = [
        "handshake",
        "create_session",
        "send_turn",
        "watch_session_events",
        "interrupt_turn",
        "resolve_permission",
        "close_session",
        "resume_session",
        "fork_session",
        "setting_sources",
        "tool_policy",
        "session_model",
        "session_effort",
        "system_prompt",
        "output_format",
        "structured_output",
        "turn_usage",
        "account_identity",
        "init_fingerprint",
        "tool_allow_list",
        "set_permission_mode",
        "text_delta_message_id",
        "executable_host_cli",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    // Order and length included for the 133dc03 list, not just set membership: the fixture is
    // transcribed from the sidecar's own literal, and this is what keeps the transcription honest.
    //
    // Two permitted differences, each transcribed from Verdandi's own handshake handler at its one
    // permitted position, stripped out before comparing what remains against `expected` verbatim:
    // `egress_restricted`, right after `tool_allow_list` (before `set_permission_mode`) when this
    // sidecar's own configuration restricts egress; and a trailing `executable_sdk_bundled` on a
    // build running from a Verdandi CHECKOUT, which a packaged single-file artifact cannot serve at
    // all (no node_modules for the SDK's own CLI to resolve through). This test spawns whichever the
    // host has, so it accepts either, neither, or both -- but only in their one permitted slot.
    // Anything else means the real list moved and the fixture owes an update.
    let mut actual = info.advertised_capabilities.clone();
    if let Some(pos) = actual.iter().position(|c| c == "egress_restricted") {
        assert_eq!(
            pos.checked_sub(1).and_then(|i| actual.get(i)).map(String::as_str),
            Some("tool_allow_list"),
            "egress_restricted moved; it must sit right after tool_allow_list, got the list at \
             this position: {actual:?}"
        );
        actual.remove(pos);
    }
    if actual.last().map(String::as_str) == Some("executable_sdk_bundled") {
        actual.pop();
    }
    assert_eq!(
        actual, expected,
        "the real sidecar's capability list has moved; update real_handshake_today() in \
         agent/src/providers/claude_sidecar/mod.rs to match"
    );

    assert_eq!(
        info.advertised_permission_modes,
        vec![
            "interactive".to_string(),
            "verdandi_rules".to_string(),
            "bypass".to_string()
        ],
        "the real sidecar's permission modes have moved; update real_handshake_today()"
    );
    assert_eq!(
        info.sidecar_version, "0.1.0",
        "SIDECAR_VERSION moved; update real_handshake_today()"
    );
    assert_eq!(
        info.event_buffer_policy, "bounded-1000",
        "the sidecar builds this from its RESOLVED ring capacity, so a non-default \
         VERDANDI_CLAUDE_SIDECAR_RING_CAPACITY in the environment fails this legitimately"
    );
}
