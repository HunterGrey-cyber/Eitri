//! Shared segmentation of the live projection; records before the first opener belong to its turn.

use super::AgentSessionProjection;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turns {
    pub base: u64,
    pub starts: Vec<u64>,
}

impl Turns {
    pub fn of(projection: &AgentSessionProjection) -> Self {
        let mut starts: Vec<_> = projection
            .user_prompts
            .iter()
            .map(|r| r.seq)
            .chain(projection.turn_starts.iter().map(|r| r.seq))
            .collect();
        starts.sort_unstable();
        starts.dedup();
        let boundary = projection.evicted.as_ref().map_or(0, |e| e.through_seq);
        match starts.first_mut() {
            Some(first) => *first = boundary,
            None => starts.push(boundary),
        }
        Self {
            base: projection.evicted.as_ref().map_or(0, |e| e.turns),
            starts,
        }
    }

    pub fn count(&self) -> u64 {
        self.starts.len() as u64
    }

    /// Sequence bounds for relative turns `from..to`, with an unbounded last turn.
    pub fn seqs(&self, from: usize, to: usize) -> (u64, u64) {
        (self.starts[from], self.starts.get(to).copied().unwrap_or(u64::MAX))
    }

    pub fn turn_of(&self, seq: u64) -> usize {
        self.starts.partition_point(|start| *start <= seq).saturating_sub(1)
    }

    pub fn absolute(&self, relative: usize) -> u64 {
        self.base + relative as u64
    }

    pub fn relative(&self, absolute: u64) -> Option<usize> {
        absolute
            .checked_sub(self.base)
            .filter(|i| *i < self.count())
            .map(|i| i as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projection::*;
    use serde_json::{json, Value};

    #[test]
    fn history_evicted_shared_fixture_prunes_every_collection_and_keeps_children() {
        let fixture: Value =
            serde_json::from_str(include_str!("../../../agent-ui/web/src/fixtures/history-evicted.json")).unwrap();
        let mut p = AgentSessionProjection::default();
        for event in fixture["events"].as_array().unwrap() {
            p.apply(&serde_json::from_value::<AgentDomainEvent>(event.clone()).unwrap());
        }
        macro_rules! check { ($($field:ident),*) => { $(
            assert_eq!(json!(p.$field.iter().map(|r| r.seq).collect::<Vec<_>>()), fixture["surviving"][stringify!($field)]);
        )* }; }
        check!(
            user_prompts,
            transcript,
            thinking,
            tool_calls,
            turn_starts,
            turn_endings,
            notices,
            questions
        );
        assert_eq!(serde_json::to_value(&p.evicted).unwrap(), fixture["evicted"]);
        assert_eq!(p.call_index.covers, 0);
        assert!(p.tool_calls[0].background);
        assert_eq!(
            p.tool_calls[0].children.as_ref().unwrap().tool_calls[0].tool_use_id,
            "nested"
        );
        let seed = p.export_settled();
        let moved = AgentSessionProjection::adopt(&crate::move_seed::ChatMoveCore {
            seed,
            lifecycle: Vec::new(),
            running_turn: Vec::new(),
            folded_through: 0,
            call_marks: Default::default(),
        })
        .unwrap();
        assert_eq!(moved.evicted, p.evicted);
        p.apply(&AgentDomainEvent::ConversationReset {
            new_conversation_id: "new".into(),
            trigger: None,
        });
        assert_eq!(p.evicted, None);
    }

    #[test]
    fn turns_share_sorted_deduplicated_boundaries_and_absolute_indices() {
        let mut p = AgentSessionProjection::default();
        assert_eq!(Turns::of(&p).starts, vec![0]);
        p.user_prompts = vec![
            UserPromptRecord {
                seq: 20,
                text: "second".into(),
                delivered: false,
                attachments: Vec::new(),
            },
            UserPromptRecord {
                seq: 10,
                text: "first".into(),
                delivered: false,
                attachments: Vec::new(),
            },
        ];
        p.turn_starts = vec![TurnStartRecord {
            seq: 20,
            turn_id: "cli".into(),
            origin: TurnOrigin::Cli,
            cause: None,
        }];
        let t = Turns::of(&p);
        assert_eq!(t.starts, vec![0, 20]);
        assert_eq!(t.seqs(0, 1), (0, 20));
        assert_eq!(t.seqs(1, 2), (20, u64::MAX));
        assert_eq!(t.turn_of(19), 0);
        p.evicted = Some(Evicted {
            through_seq: 10,
            turns: 42,
            lifted: vec![2],
        });
        let t = Turns::of(&p);
        assert_eq!(t.starts, vec![10, 20]);
        assert_eq!(t.base, 42);
        assert_eq!(t.absolute(1), 43);
        assert_eq!(t.relative(43), Some(1));
        assert_eq!(t.relative(41), None);
        assert_eq!(t.relative(44), None);
    }

    #[test]
    fn an_evicted_seed_validates_its_boundary_and_cannot_seed_over_another_boundary() {
        let mut p = AgentSessionProjection::default();
        p.last_revision = 10;
        p.evicted = Some(Evicted {
            through_seq: 4,
            turns: 1,
            lifted: vec![2],
        });
        let mut seed = p.export_settled();
        assert!(AgentSessionProjection::default().seed_fits(&seed).is_ok());
        seed.evicted.as_mut().unwrap().through_seq = 10;
        assert!(AgentSessionProjection::default().seed_fits(&seed).is_err());
        seed.evicted.as_mut().unwrap().through_seq = 1;
        assert!(AgentSessionProjection::default().seed_fits(&seed).is_err());
        assert!(p.seed_fits(&p.export_settled()).is_err());
    }
}
