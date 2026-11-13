//! Protocol 3.26's plan mode on the wire, the host's side: what a plan decision and a `SetPlanning` send,
//! and what a `SetPlanning` reply means.
//!
//! **A reply is not the move.** The sidecar answers `SetPlanning` with the CLI's acknowledgement; the CLI's
//! own report of its mode (`CliModeReported`, the next `SessionReady`) is what confirms the move, and the
//! projection waits for that. What matters here is telling the replies apart: a refusal before the CLI was
//! asked (or the CLI's own refusal) means the move did not happen, while the sidecar's own deadline means
//! nothing either way, since the CLI may still move.

use crate::{ProviderError, ProviderErrorCode};
use claude_runtime_protocol::v1 as wire;
use std::time::Duration;

/// How long a `SetPlanning` may take before this side stops waiting. Longer than the sidecar's own 15 s
/// wait for the CLI's acknowledgement, so the sidecar's answer to a slow CLI (`DEADLINE_EXCEEDED`) arrives
/// rather than a timeout of this side's that says less.
pub(super) const PLANNING_RPC_TIMEOUT: Duration = Duration::from_secs(20);

/// The longest part of the CLI's own words a refused move carries, in characters.
const MAX_REFUSAL_CHARS: usize = 500;

/// An approval on the wire: the mode, and the digest of the plan the person read.
pub(super) fn approve(
    mode: crate::PlanExecutionMode,
    plan_sha256: String,
) -> wire::resolve_interaction_request::Result {
    wire::resolve_interaction_request::Result::Approve(wire::PlanApproval {
        mode: execution_mode(mode) as i32,
        plan_sha256,
    })
}

/// "Keep planning" on the wire, with the feedback exactly as written, empty included.
pub(super) fn keep_planning(feedback: String) -> wire::resolve_interaction_request::Result {
    wire::resolve_interaction_request::Result::KeepPlanning(wire::PlanKeepPlanning { feedback })
}

fn execution_mode(mode: crate::PlanExecutionMode) -> wire::PlanExecutionMode {
    match mode {
        crate::PlanExecutionMode::Manual => wire::PlanExecutionMode::Manual,
        crate::PlanExecutionMode::Auto => wire::PlanExecutionMode::Auto,
        crate::PlanExecutionMode::Bypass => wire::PlanExecutionMode::Bypass,
    }
}

/// The word the CLI uses for where a move lands: `plan` when entering, the creation mode when leaving. Reads
/// the requested mode without naming its auto variant, which only the wire guard's listed sites may.
pub(super) fn target_word(enter: bool, requested: crate::RequestedCliMode) -> &'static str {
    match (enter, requested) {
        (true, _) => "plan",
        (false, crate::RequestedCliMode::Default) => "default",
        (false, _) => "auto",
    }
}

/// What a `SetPlanning` that failed with `status` means. `target` is where the move was going: a refusal
/// saying the session is there already (`planning_already`, which a request sent again after a deadline
/// gets when the first one landed) is the move having happened, and reads as an acknowledgement of it.
pub(super) fn planning_failure(status: tonic::Status, target: &str) -> Result<String, ProviderError> {
    let Some(detail) = super::error_detail(&status) else {
        return Err(super::map_status(status));
    };
    let code = detail.code();
    let words = |text: &str| super::translate_322::bound(text, MAX_REFUSAL_CHARS);
    let refused = |message: String| {
        Err(ProviderError::Provider {
            code: ProviderErrorCode::PlanningRefused,
            message,
        })
    };
    match (code, detail.reason.as_str()) {
        (_, "planning_already") => Ok(target.to_string()),
        (_, "planning_not_offered") => refused("plan mode is not offered on this session".to_string()),
        (_, "planning_interaction_pending") => {
            refused("plan mode changes once nothing waits for an answer".to_string())
        }
        (wire::ErrorCode::TurnAlreadyActive, _) | (_, "planning_turn_running") => {
            refused("plan mode changes when the turn has ended".to_string())
        }
        (_, "planning_cli_refused") => refused(format!("Claude Code refused: {}", words(&detail.message))),
        (_, reason) if reason.starts_with("planning_") => refused(words(&detail.message)),
        _ => Err(super::map_status(status)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message as _;

    fn status(code: wire::ErrorCode, reason: &str, message: &str) -> tonic::Status {
        tonic::Status::with_details(
            tonic::Code::FailedPrecondition,
            message,
            wire::ErrorDetail {
                code: code as i32,
                message: message.to_string(),
                reason: reason.to_string(),
            }
            .encode_to_vec()
            .into(),
        )
    }

    fn code_of(result: Result<String, ProviderError>) -> Option<ProviderErrorCode> {
        match result {
            Err(ProviderError::Provider { code, .. }) => Some(code),
            _ => None,
        }
    }

    /// Every refusal before the CLI was asked, and the CLI's own, is `PlanningRefused`: benign, and the
    /// move did not happen. `planning_already` is the target reached. The sidecar's deadline is not a
    /// refusal: it stays `DeadlineExceeded`, which undoes nothing.
    #[test]
    fn set_planning_failures_are_told_apart() {
        let invalid = wire::ErrorCode::InvalidConfiguration;
        for reason in [
            "planning_not_offered",
            "planning_interaction_pending",
            "planning_cli_refused",
            "planning_new_rule",
        ] {
            let mapped = planning_failure(status(invalid, reason, "no"), "plan");
            assert_eq!(code_of(mapped), Some(ProviderErrorCode::PlanningRefused), "{reason}");
        }
        let running = planning_failure(
            status(wire::ErrorCode::TurnAlreadyActive, "planning_turn_running", "busy"),
            "plan",
        );
        assert_eq!(code_of(running), Some(ProviderErrorCode::PlanningRefused));
        assert!(ProviderErrorCode::PlanningRefused.is_benign());
        match planning_failure(status(invalid, "planning_cli_refused", "not now"), "plan") {
            Err(ProviderError::Provider { message, .. }) => assert_eq!(message, "Claude Code refused: not now"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            planning_failure(status(invalid, "planning_already", "there"), "auto").unwrap(),
            "auto"
        );
        let deadline = planning_failure(status(wire::ErrorCode::DeadlineExceeded, "", "slow"), "plan");
        assert_eq!(code_of(deadline), Some(ProviderErrorCode::DeadlineExceeded));
        let closed = planning_failure(status(wire::ErrorCode::SessionNotFound, "", "gone"), "plan");
        assert_eq!(code_of(closed), Some(ProviderErrorCode::SessionNotFound));
        assert!(matches!(
            planning_failure(tonic::Status::unavailable("down"), "plan"),
            Err(ProviderError::Transport(_))
        ));
        assert!(PLANNING_RPC_TIMEOUT > Duration::from_secs(15));
    }

    #[test]
    fn the_target_of_a_move_is_plan_or_the_creation_mode() {
        assert_eq!(target_word(true, crate::RequestedCliMode::Auto), "plan");
        assert_eq!(target_word(false, crate::RequestedCliMode::Default), "default");
        assert_eq!(target_word(false, crate::RequestedCliMode::Auto), "auto");
    }
}
