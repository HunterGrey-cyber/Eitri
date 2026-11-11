use super::*;
use crate::history::archive::{ArchiveId, ArchiveWriter, ArchivedRecord, ArchivedTurn};
use crate::projection::{turns::Turns, QuestionState, TaskStatus, ToolCallRecord};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

fn ready(state: &mut IngestState, budget: usize) -> (bool, Option<String>) {
    let mut warning = None;
    let ceiling = Turns::of(&state.projection).count() as usize >= budget.saturating_mul(2);
    let p = &state.projection;
    let blockers = [
        (p.opening_prompt.is_some(), "opening prompt"),
        (!p.open_client_calls.is_empty(), "open client calls"),
        (
            p.questions
                .iter()
                .any(|q| !matches!(q.state, QuestionState::Settled { .. })),
            "waiting/answering question",
        ),
    ];
    if ceiling && !state.eviction_ceiling_logged && blockers.iter().any(|(blocked, _)| *blocked) {
        let names: Vec<_> = blockers
            .iter()
            .filter(|(blocked, _)| *blocked)
            .map(|(_, name)| *name)
            .collect();
        warning = Some(format!(
            "agent: history at twice its turn budget; eviction deferred by {}",
            names.join(", ")
        ));
        state.eviction_ceiling_logged = true;
    } else if !ceiling {
        state.eviction_ceiling_logged = false;
    }
    let p = &state.projection;
    let eligible = p.is_settled()
        && state.running_turn.is_none()
        && p.opening_prompt.is_none()
        && p.open_client_calls.is_empty()
        && p.questions
            .iter()
            .all(|q| matches!(q.state, QuestionState::Settled { .. }))
        && (!state.reload_held || ceiling);
    (eligible, warning)
}

fn running_background(
    call: &ToolCallRecord,
    roster: &BTreeMap<&str, bool>,
    background_turns: &BTreeSet<String>,
) -> bool {
    // A roster outcome is authoritative even when a synthetic launcher never receives a result.
    (background_turns.contains(&call.turn_id) && call.result.is_none() && call.denied.is_none())
        || roster
            .get(call.tool_use_id.as_str())
            .copied()
            .unwrap_or(call.background && call.result.is_none())
        || call.children.as_ref().is_some_and(|children| {
            children
                .tool_calls
                .iter()
                .any(|child| running_background(child, roster, background_turns))
        })
}

fn collect_call_turns<'a>(calls: &'a [ToolCallRecord], turns: &mut BTreeSet<&'a str>) {
    for call in calls {
        turns.insert(&call.turn_id);
        if let Some(children) = &call.children {
            collect_call_turns(&children.tool_calls, turns);
        }
    }
}

pub(super) fn flush_eviction(
    state: &Mutex<IngestState>,
    session_id: &Mutex<Option<String>>,
    context: &AdoptionContext,
    budget: usize,
) -> bool {
    let (eligible, warning) = {
        let mut st = state.lock().unwrap();
        if !st.eviction_due || st.eviction_retry {
            return false;
        }
        ready(&mut st, budget)
    };
    if let Some(warning) = warning {
        eprintln!("{warning}");
    }
    if !eligible {
        return false;
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
    let mut warning = None;
    let copied = (|| {
        let mut st = state.lock().unwrap();
        if !st.eviction_due || st.eviction_retry {
            return None;
        }
        let (eligible, message) = ready(&mut st, budget);
        warning = message;
        if !eligible {
            return None;
        }
        let p = &st.projection;
        let turns = Turns::of(p);
        let count = turns.count() as usize;
        if count <= budget + (budget / 10).max(1) {
            st.eviction_due = false;
            return None;
        }
        let sid = read_session_id(session_id)?;
        let cut = count - budget;
        let through_seq = turns.starts[cut];
        let mut roster = BTreeMap::new();
        for row in &p.background_tasks.rows {
            if let Some(id) = row.task.tool_use_id.as_deref() {
                *roster.entry(id).or_insert(false) |= row.status == TaskStatus::Running;
            }
        }
        let empty_turns = BTreeSet::new();
        let background_turns = if context.provider_name == "claude" {
            &empty_turns
        } else {
            &st.background_turns
        };
        let lifted: Vec<_> = p
            .tool_calls
            .iter()
            .filter(|call| call.seq < through_seq && running_background(call, &roster, background_turns))
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
        Some((
            revision,
            count,
            cut,
            through_seq,
            absolute,
            lifted,
            expired,
            records,
            st.archive_id.clone(),
            st.lease.is_some(),
            st.archive_writer.take(),
        ))
    })();
    if let Some(warning) = warning {
        eprintln!("{warning}");
    }
    let Some((revision, count, cut, through_seq, absolute, lifted, expired, records, id, holds_lease, mut writer)) =
        copied
    else {
        return false;
    };
    let written = (|| -> std::io::Result<()> {
        let dir: &Path = directory.as_ref().map_err(|e| std::io::Error::other(e.to_string()))?;
        let id = id
            .as_ref()
            .ok_or_else(|| std::io::Error::other("archive identity unavailable"))?;
        if writer.is_none() {
            writer = Some(ArchiveWriter::open(dir, id, holds_lease)?);
        }
        writer.as_mut().unwrap().append(&records)
    })();
    let poisoned = writer.as_ref().is_some_and(ArchiveWriter::is_poisoned);
    let recovered = writer.as_ref().and_then(ArchiveWriter::recovered_rollback);
    after_write();
    #[cfg(feature = "test-support")]
    write_gate::hold(&context.conversation_id);
    let mut st = state.lock().unwrap();
    // A reset owns a new archive generation; never put the old writer back into it.
    if st.archive_id == id {
        st.archive_writer = writer;
        // Successful append completes any pending rollback before writing the new chunk.
        if written.is_ok()
            && !poisoned
            && recovered
                .zip(st.archive_readable_limit)
                .is_some_and(|(from, limit)| from <= limit)
        {
            st.archive_readable_limit = None;
        }
    }
    if st.projection.last_revision != revision {
        return false;
    }
    let (eligible, warning) = ready(&mut st, budget);
    if !eligible {
        drop(st);
        if let Some(warning) = warning {
            eprintln!("{warning}");
        }
        return false;
    }
    let lost = written.err();
    if lost.is_some() {
        if count < budget.saturating_mul(2) {
            drop(st);
            if let Some(warning) = warning {
                eprintln!("{warning}");
            }
            return false;
        }
        st.stats.lost_history_turns += cut as u64;
        if poisoned {
            let limit = absolute - cut as u64;
            st.archive_readable_limit = Some(st.archive_readable_limit.map_or(limit, |old| old.min(limit)));
        }
    }
    if st.reload_held {
        st.reload_held = false;
        st.reload_generation += 1;
    }
    st.fold(AgentDomainEvent::HistoryEvicted {
        through_seq,
        turns: absolute,
        lifted,
    });
    {
        let IngestState {
            projection,
            background_turns,
            ..
        } = &mut *st;
        // Only calls still held by the projection can need the count fallback after a cut.
        let mut retained_turns = BTreeSet::new();
        collect_call_turns(&projection.tool_calls, &mut retained_turns);
        background_turns.retain(|turn| retained_turns.contains(turn.as_str()));
    }
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
    st.eviction_ceiling_logged = false;
    drop(st);
    if let Some(warning) = warning {
        eprintln!("{warning}");
    }
    if let Some(error) = lost {
        eprintln!("agent: archive write failed; {cut} earlier turns lost: {error}");
    }
    true
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod record_tests;

#[cfg(feature = "test-support")]
pub(super) mod write_gate {
    use std::sync::{mpsc, Mutex, OnceLock};
    type Gate = (mpsc::SyncSender<()>, mpsc::Receiver<()>);
    static GATES: OnceLock<Mutex<std::collections::HashMap<String, Gate>>> = OnceLock::new();

    pub fn arm(conversation: &str) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (entered, wait) = mpsc::sync_channel(1);
        let (release, gate) = mpsc::channel();
        GATES
            .get_or_init(Mutex::default)
            .lock()
            .unwrap()
            .insert(conversation.into(), (entered, gate));
        (wait, release)
    }

    pub(super) fn hold(conversation: &str) {
        let gate = GATES.get_or_init(Mutex::default).lock().unwrap().remove(conversation);
        if let Some((entered, release)) = gate {
            let _ = entered.send(());
            let _ = release.recv_timeout(std::time::Duration::from_secs(30));
        }
    }
}
