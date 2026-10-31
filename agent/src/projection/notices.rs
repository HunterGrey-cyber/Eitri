use super::{AgentDomainEvent, AgentSessionProjection, NoticeLevel, NoticeRecord, SessionNoticeKind};
use serde_json::{json, Value};

pub(super) fn records_row(event: &AgentDomainEvent) -> bool {
    let AgentDomainEvent::SessionNotice {
        kind, level, scalars, ..
    } = event
    else {
        return false;
    };
    if kind.is_transient() {
        return false;
    }
    match kind {
        SessionNoticeKind::Informational => *level != NoticeLevel::Info,
        SessionNoticeKind::PeerHold => matches!(
            scalars.get("hold_state").and_then(Value::as_str),
            Some("held" | "dropped")
        ),
        SessionNoticeKind::RateLimit => !matches!(
            scalars.get("rate_status").and_then(Value::as_str),
            Some("allowed" | "allowed_warning" | "rejected")
        ),
        _ => true,
    }
}

pub(super) fn fold(state: &mut AgentSessionProjection, event: &AgentDomainEvent) {
    #[cfg(test)]
    super::count_hook("notices::fold");
    let AgentDomainEvent::SessionNotice {
        kind,
        level,
        content,
        scalars,
        task_id,
        tool_use_id,
        task_status,
    } = event
    else {
        return;
    };
    let record = NoticeRecord {
        seq: state.last_revision,
        kind: *kind,
        level: *level,
        content: content.clone(),
        scalars: scalars.clone(),
        task_id: task_id.clone(),
        tool_use_id: tool_use_id.clone(),
        task_status: task_status.clone(),
    };
    match kind {
        SessionNoticeKind::ApiRetry => state.api_retry = Some(record.clone()),
        SessionNoticeKind::Compacting => state.compacting = true,
        SessionNoticeKind::Compaction => state.compacting = false,
        SessionNoticeKind::Notification => state.notification = Some(record.clone()),
        SessionNoticeKind::ModelFallback => {
            if let Some(model) = scalars.get("to_model").and_then(Value::as_str) {
                state.fallback_model = Some(model.to_owned());
            }
        }
        SessionNoticeKind::RateLimit => {
            state.rate_limit_warning =
                (scalars.get("rate_status").and_then(Value::as_str) == Some("allowed_warning")).then(|| record.clone());
        }
        _ => {}
    }
    if records_row(event) {
        state.assistant_message_open = false;
        state.thinking_open = false;
        state.notices.push(record);
    }
}

fn record_json(record: &NoticeRecord) -> Value {
    json!({"seq": record.seq, "kind": record.kind, "level": record.level, "content": record.content,
        "scalars": record.scalars, "taskId": record.task_id, "toolUseId": record.tool_use_id, "taskStatus": record.task_status})
}

pub(super) fn snapshot_json(state: &AgentSessionProjection, snapshot: &mut Value) {
    #[cfg(test)]
    super::count_hook("notices::snapshot_json");
    for (key, record) in [
        ("apiRetry", &state.api_retry),
        ("rateLimitWarning", &state.rate_limit_warning),
        ("notification", &state.notification),
    ] {
        if let Some(record) = record {
            snapshot[key] = record_json(record);
        }
    }
    if state.compacting {
        snapshot["compacting"] = json!(true);
    }
    if let Some(model) = &state.fallback_model {
        snapshot["fallbackModel"] = json!(model);
    }
    if let Some(effort) = &state.effort {
        snapshot["effort"] = json!(effort);
    }
}

pub(super) fn on_turn_end(state: &mut AgentSessionProjection, _event: &AgentDomainEvent) {
    #[cfg(test)]
    super::count_hook("notices::on_turn_end");
    state.api_retry = None;
}

pub(super) fn on_content(state: &mut AgentSessionProjection, _event: &AgentDomainEvent) {
    #[cfg(test)]
    super::count_hook("notices::on_content");
    state.api_retry = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_notice_has_the_expected_destination() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../../agent-ui/web/src/fixtures/session-notices.json")).unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let events: Vec<AgentDomainEvent> = serde_json::from_value(case["domain_events"].clone()).unwrap();
            let mut state = AgentSessionProjection::default();
            for event in &events {
                state.apply(event);
            }
            let name = case["name"].as_str().unwrap();
            let rows = match name {
                "compaction"
                | "fallback"
                | "refusal_without_content"
                | "informational_notice"
                | "informational_suggestion"
                | "informational_warning"
                | "peer_held"
                | "peer_dropped"
                | "rate_reset" => 1,
                _ => 0,
            };
            assert_eq!(state.notices.len(), rows, "{name}");
            assert_eq!(state.api_retry.is_some(), name == "retry", "{name}");
            assert_eq!(state.compacting, name == "compacting", "{name}");
            assert_eq!(
                state.rate_limit_warning.is_some(),
                matches!(name, "rate_allowed_warning" | "quiet_preserves_stream"),
                "{name}"
            );
            assert_eq!(
                state.fallback_model.as_deref(),
                (name == "fallback").then_some("small"),
                "{name}"
            );
            assert_eq!(state.notification.is_some(), name == "notification", "{name}");
            if name == "compaction" {
                assert_eq!(state.notices[0].scalars["pre_tokens"], 182000);
                assert_eq!(state.notices[0].scalars["trigger"], "auto");
            }
            if name == "refusal_without_content" {
                assert!(state.notices[0].content.is_none());
            }
        }
    }
}
