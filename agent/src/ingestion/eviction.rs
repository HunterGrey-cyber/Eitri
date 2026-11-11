use super::*;
use crate::history::archive::{ArchiveId, ArchiveWriter, ArchivedRecord, ArchivedTurn};
use crate::projection::{turns::Turns, QuestionState, TaskStatus, ToolCallRecord};
use std::collections::BTreeMap;
use std::path::Path;

fn ready(state: &mut IngestState, budget: usize) -> bool {
    if state.reload_held && Turns::of(&state.projection).count() as usize >= budget.saturating_mul(2) {
        state.reload_held = false;
        state.reload_generation += 1;
    }
    let p = &state.projection;
    p.is_settled()
        && state.running_turn.is_none()
        && p.opening_prompt.is_none()
        && p.open_client_calls.is_empty()
        && p.questions
            .iter()
            .all(|q| matches!(q.state, QuestionState::Settled { .. }))
        && !state.reload_held
}

fn running_background(call: &ToolCallRecord, roster: &BTreeMap<&str, bool>) -> bool {
    // A roster outcome is authoritative even when a synthetic launcher never receives a result.
    roster
        .get(call.tool_use_id.as_str())
        .copied()
        .unwrap_or(call.background && call.result.is_none())
        || call.children.as_ref().is_some_and(|children| {
            children
                .tool_calls
                .iter()
                .any(|child| running_background(child, roster))
        })
}

pub(super) fn flush_eviction(
    state: &Mutex<IngestState>,
    session_id: &Mutex<Option<String>>,
    context: &AdoptionContext,
    budget: usize,
) -> bool {
    {
        let mut st = state.lock().unwrap();
        if !st.eviction_due || st.eviction_retry || !ready(&mut st, budget) {
            return false;
        }
    }
    // Resolving the directory can touch the filesystem, so it belongs outside the state lock too.
    flush_in(
        state,
        session_id,
        context,
        budget,
        crate::state_dirs::history_dir(),
        || {},
    )
}

pub(super) fn flush_in(
    state: &Mutex<IngestState>,
    session_id: &Mutex<Option<String>>,
    context: &AdoptionContext,
    budget: usize,
    directory: std::io::Result<std::path::PathBuf>,
    after_write: impl FnOnce(),
) -> bool {
    let (revision, count, cut, through_seq, absolute, lifted, expired, records, id, mut writer) = {
        let mut st = state.lock().unwrap();
        if !st.eviction_due || st.eviction_retry || !ready(&mut st, budget) {
            return false;
        }
        let p = &st.projection;
        let turns = Turns::of(p);
        let count = turns.count() as usize;
        if count <= budget + (budget / 10).max(1) {
            st.eviction_due = false;
            return false;
        }
        let Some(sid) = read_session_id(session_id) else {
            return false;
        };
        let cut = count - budget;
        let through_seq = turns.starts[cut];
        let mut roster = BTreeMap::new();
        for row in &p.background_tasks.rows {
            if let Some(id) = row.task.tool_use_id.as_deref() {
                *roster.entry(id).or_insert(false) |= row.status == TaskStatus::Running;
            }
        }
        let lifted: Vec<_> = p
            .tool_calls
            .iter()
            .filter(|call| call.seq < through_seq && running_background(call, &roster))
            .map(|call| call.seq)
            .collect();
        // Finished trees lifted by earlier cuts leave no archive row, but still carry edit attribution.
        let expired: Vec<_> = p
            .tool_calls
            .iter()
            .filter(|call| {
                call.seq < through_seq
                    && p.evicted.as_ref().is_some_and(|e| e.lifted.contains(&call.seq))
                    && !lifted.contains(&call.seq)
            })
            .cloned()
            .collect();
        let mut records: Vec<_> = (0..cut)
            .map(|i| ArchivedTurn {
                turn: turns.absolute(i),
                records: Vec::new(),
            })
            .collect();
        // Earlier lifted calls are already absent from their archived turns. Once finished they simply expire.
        macro_rules! copy {
            ($field:ident, $variant:ident) => {
                for r in &p.$field {
                    if r.seq >= turns.starts[0] && r.seq < through_seq && !lifted.contains(&r.seq) {
                        let i = turns.turn_of(r.seq);
                        records[i].records.push(ArchivedRecord::$variant(r.clone()));
                    }
                }
            };
        }
        copy!(user_prompts, UserPrompt);
        copy!(transcript, Transcript);
        copy!(thinking, Thinking);
        copy!(tool_calls, ToolCall);
        copy!(turn_starts, TurnStart);
        copy!(turn_endings, TurnEnding);
        copy!(notices, Notice);
        copy!(questions, Question);
        let revision = p.last_revision;
        let absolute = turns.absolute(cut);
        if st.archive_id.is_none() {
            st.archive_id = ArchiveId::new(&context.conversation_id, &sid).ok();
        }
        st.eviction_retry = true;
        (
            revision,
            count,
            cut,
            through_seq,
            absolute,
            lifted,
            expired,
            records,
            st.archive_id.clone(),
            st.archive_writer.take(),
        )
    };
    let written = (|| -> std::io::Result<()> {
        let dir: &Path = directory.as_ref().map_err(|e| std::io::Error::other(e.to_string()))?;
        let id = id
            .as_ref()
            .ok_or_else(|| std::io::Error::other("archive identity unavailable"))?;
        if writer.is_none() {
            writer = Some(ArchiveWriter::open(dir, id)?);
        }
        writer.as_mut().unwrap().append(&records)
    })();
    after_write();
    let mut st = state.lock().unwrap();
    // A reset owns a new archive generation; never put the old writer back into it.
    if st.archive_id == id {
        st.archive_writer = writer;
    }
    if st.projection.last_revision != revision || !ready(&mut st, budget) {
        return false;
    }
    let lost = written.err();
    if lost.is_some() {
        if count < budget.saturating_mul(2) {
            return false;
        }
        st.stats.lost_history_turns += cut as u64;
    }
    st.fold(AgentDomainEvent::HistoryEvicted {
        through_seq,
        turns: absolute,
        lifted,
    });
    let mut removed = crate::history::HistoryParts::default();
    removed.tool_calls.extend(expired);
    for record in records.into_iter().flat_map(|turn| turn.records) {
        match record {
            ArchivedRecord::UserPrompt(r) => removed.user_prompts.push(r),
            ArchivedRecord::Transcript(r) => removed.transcript.push(r),
            ArchivedRecord::ToolCall(r) => removed.tool_calls.push(r),
            ArchivedRecord::TurnStart(r) => removed.turn_starts.push(r),
            ArchivedRecord::Notice(r) => removed.notices.push(r),
            ArchivedRecord::Question(r) => removed.questions.push(r),
            ArchivedRecord::Thinking(_) | ArchivedRecord::TurnEnding(_) => {}
        }
    }
    let calls = removed.tool_calls.clone();
    st.history_tail.extend(removed);
    let IngestState {
        history_tail,
        projection,
        ..
    } = &mut *st;
    history_tail.trim(crate::history::store::HistoryView::of(projection));
    st.queue_evicted_calls(calls);
    st.eviction_due = false;
    drop(st);
    if let Some(error) = lost {
        eprintln!("agent: archive write failed; {cut} earlier turns lost: {error}");
    }
    true
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod record_tests;
