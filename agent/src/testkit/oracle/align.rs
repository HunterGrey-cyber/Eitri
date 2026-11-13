//! Pairs the CLI form with one of Eitri's records.
//!
//! 1. The CLI form is projected onto what the record can hold ([`normalise::project`]).
//! 2. Turns are paired in order by their first entry (a prompt, or for a turn the CLI started, its
//!    notification): a longest common subsequence over those keys. Each paired turn, with the unpaired
//!    turns that follow it on either side, is one segment.
//! 3. Within a segment, entries are paired by a longest common subsequence over (kind, key): the key is the
//!    tool-use id for a tool call, the normalised text for user, assistant and thinking text. Tool results
//!    and question sets are paired by their tool-use id wherever they sit, since parallel calls finish in
//!    any order and a record may keep a question set where its call was asked rather than where it was
//!    answered; a paired question set differs in the parts of its key that differ. An entry
//!    left on one side whose twin is left on the other side elsewhere is paired as out of order; an entry
//!    left on each side at the same place, of one kind, is paired as differing in text.
//! 4. A turn that was not paired by its first entry is placed by its entries: in the record's turn that
//!    holds its first entry the record can attribute (any entry for E; a tool call or a message taken into
//!    a running turn for A; any entry for B). Each paired entry's turn is then checked against its CLI
//!    turn's place, at the resolution the record has ([`super::Attribution`]): a turn the CLI started with
//!    no tool call is checked by order only in A, and so is the first turn whose calls sit in a turn of A
//!    opened by a prompt that made no call (A took that turn's id from those calls); B's turns only by
//!    order while its tool calls carry one turn id per line.
//! 5. CLI entries before the first pair are the record's truncation when the record says it left items out
//!    (as many as it counted, when it counted them), or when, for A, the next older item could not have
//!    fitted its caps.

use super::normalise::{self, canonical_json, result_text, text, user_text};
use super::{Attribution, EitriForm, Entry, Omission, Origin, Source, Turn, Via};
use crate::history::transcript_jsonl::value_chars;
use crate::history::{strip_composed_block, HISTORY_MAX_CHARS, HISTORY_MAX_ITEMS};
use std::collections::{BTreeMap, HashMap};

/// The CLI form against one record, turn by turn.
#[derive(Clone, Debug)]
pub struct Alignment {
    pub source: Source,
    /// The CLI's turns, projected onto what the record can hold. [`At`] indexes into these.
    pub cli: Vec<Turn>,
    /// The record's turns. [`At`] indexes into these.
    pub eitri: Vec<Turn>,
    /// CLI entries left out because the record cannot hold their kind, by kind.
    pub projected_out: BTreeMap<&'static str, usize>,
    /// In CLI order, with the record's own turns where they fall.
    pub turns: Vec<TurnPair>,
    /// How many CLI entries were marked [`Mark::Truncated`].
    pub truncated: usize,
}

/// An entry's place: the turn's position in [`Alignment::cli`] or [`Alignment::eitri`] and the entry's
/// position in that turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct At {
    pub turn: usize,
    pub entry: usize,
}

/// A CLI turn and the record's turn it sits in, or a turn only one side has.
#[derive(Clone, Debug)]
pub struct TurnPair {
    pub cli: Option<usize>,
    pub eitri: Option<usize>,
    pub how: TurnMatch,
    pub entries: Vec<EntryPair>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnMatch {
    /// Paired by the first entry.
    Same,
    /// Placed by its entries in a record turn no other CLI turn sits in.
    ByEntries,
    /// Placed by its entries in a record turn another CLI turn sits in, where the record cannot separate
    /// the two: compared by order only.
    OrderOnly,
    /// Placed by its entries in a record turn another CLI turn sits in, where the record should have kept
    /// it apart.
    Merged,
    /// Every entry is the record's truncation.
    Truncated,
    CliOnly,
    EitriOnly,
}

#[derive(Clone, Debug)]
pub struct EntryPair {
    pub cli: Option<At>,
    pub eitri: Option<At>,
    pub mark: Mark,
    /// For a subagent both sides hold: its turns aligned the same way.
    pub nested: Option<Box<Alignment>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mark {
    Same,
    /// Equal after normalisation, with what normalisation did.
    Normalised(Vec<Note>),
    CliOnly,
    EitriOnly,
    /// Paired, with the fields that differ.
    Differs(Vec<Field>),
    /// Left out by the record's caps, which it recorded.
    Truncated,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Field {
    Text,
    /// One side has the words opening a turn, the other inside a running one.
    Via,
    /// The message carried other images: another number, another type or another digest.
    Images,
    Name,
    Input,
    IsError,
    Result,
    /// A question set holds another number of questions.
    Questions,
    /// A question set has answers to other questions.
    Answered,
    /// A question set or a plan ended another way.
    Outcome,
    /// A plan has another text (another digest).
    PlanText,
    /// The entry sits in another turn than its CLI turn's place in the record.
    Turn,
    /// The entry sits elsewhere in the order.
    Order,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Note {
    /// Eitri's composed editor block was stripped.
    ComposedBlock,
    /// Line endings or trailing spaces differed.
    Whitespace,
    /// The result's blocks were flattened to text.
    ResultFlattened,
    /// Eitri saw this call's input change between the gate and the CLI.
    ChangedInput,
}

/// Aligns the CLI form with one record.
pub fn align(cli: &[Turn], record: &EitriForm) -> Alignment {
    let (projected, removed, pieces) = normalise::project_counted(cli, record.source);
    let eitri = join_record_replies(&record.turns, &projected);
    let mut alignment = align_turns(projected, eitri, record, &pieces);
    alignment.projected_out = removed;
    alignment
}

/// The record's turns with adjacent replies joined where the CLI holds the joined text.
///
/// A record keeps two messages of one turn as two rows where the CLI's joined blocks are one entry, but a
/// record that cannot tell turns apart (A) also holds the replies of two CLI turns side by side, and those
/// stay apart. So a run of adjacent replies is joined when the CLI holds their joined text and does not
/// hold each of them.
fn join_record_replies(turns: &[Turn], cli: &[Turn]) -> Vec<Turn> {
    let cli_replies: std::collections::HashSet<String> = cli
        .iter()
        .flat_map(|t| t.entries.iter())
        .filter_map(|e| match e {
            Entry::Assistant { text: t } => Some(text(t)),
            _ => None,
        })
        .collect();
    turns
        .iter()
        .map(|turn| {
            let mut joined = Turn {
                index: turn.index,
                origin: turn.origin.clone(),
                entries: Vec::with_capacity(turn.entries.len()),
                positions: Vec::with_capacity(turn.positions.len()),
            };
            let mut i = 0;
            while i < turn.entries.len() {
                let run_end = turn.entries[i..]
                    .iter()
                    .position(|e| !matches!(e, Entry::Assistant { .. }))
                    .map_or(turn.entries.len(), |n| i + n);
                let piece = |k: usize| match &turn.entries[k] {
                    Entry::Assistant { text } => text.as_str(),
                    _ => "",
                };
                // The longest run from `i` whose joined text the CLI holds, unless it holds every piece.
                let mut take = 1;
                for end in (i + 2..=run_end).rev() {
                    let whole: String = (i..end).map(piece).collect();
                    let each_held = (i..end).all(|k| cli_replies.contains(&text(piece(k))));
                    if cli_replies.contains(&text(&whole)) && !each_held {
                        take = end - i;
                        break;
                    }
                }
                let entry = if take > 1 {
                    Entry::Assistant {
                        text: (i..i + take).map(piece).collect(),
                    }
                } else {
                    turn.entries[i].clone()
                };
                joined.entries.push(entry);
                joined.positions.push(turn.positions[i]);
                i += take;
            }
            joined
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Key {
    User(String),
    Assistant(String),
    Thinking(String),
    Tool(String),
    Result(String),
    Subagent(String),
    Compaction(String),
    Interrupted,
    Ended(String),
    Question(String),
    Plan(String),
}

fn key(entry: &Entry) -> Key {
    match entry {
        Entry::User { text, .. } => Key::User(user_text(text)),
        Entry::Assistant { text: t } => Key::Assistant(text(t)),
        Entry::Thinking { text: t } => Key::Thinking(text(t)),
        Entry::ToolUse { id, .. } => Key::Tool(id.clone()),
        Entry::ToolResult { id, .. } => Key::Result(id.clone()),
        Entry::Subagent { parent_tool_use_id, .. } => Key::Subagent(parent_tool_use_id.clone()),
        Entry::Compaction { trigger } => Key::Compaction(trigger.clone()),
        Entry::Interrupted => Key::Interrupted,
        Entry::Ended { reason } => Key::Ended(format!("{reason:?}")),
        Entry::Question { id, .. } => Key::Question(id.clone()),
        Entry::Plan { id, .. } => Key::Plan(id.clone()),
    }
}

/// Pairs of indices of a longest common subsequence of `a` and `b`, in order.
fn lcs<K: PartialEq>(a: &[K], b: &[K]) -> Vec<(usize, usize)> {
    let mut start = 0;
    while start < a.len() && start < b.len() && a[start] == b[start] {
        start += 1;
    }
    let (mut end_a, mut end_b) = (a.len(), b.len());
    while end_a > start && end_b > start && a[end_a - 1] == b[end_b - 1] {
        end_a -= 1;
        end_b -= 1;
    }
    let (ma, mb) = (&a[start..end_a], &b[start..end_b]);
    let width = mb.len() + 1;
    let mut table = vec![0u32; (ma.len() + 1) * width];
    for i in (0..ma.len()).rev() {
        for j in (0..mb.len()).rev() {
            table[i * width + j] = if ma[i] == mb[j] {
                table[(i + 1) * width + j + 1] + 1
            } else {
                table[(i + 1) * width + j].max(table[i * width + j + 1])
            };
        }
    }
    let mut pairs: Vec<(usize, usize)> = (0..start).map(|i| (i, i)).collect();
    let (mut i, mut j) = (0, 0);
    while i < ma.len() && j < mb.len() {
        if ma[i] == mb[j] {
            pairs.push((start + i, start + j));
            i += 1;
            j += 1;
        } else if table[(i + 1) * width + j] >= table[i * width + j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    pairs.extend((0..a.len() - end_a).map(|k| (end_a + k, end_b + k)));
    pairs
}

/// How an op came to pair its two entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Paired {
    /// By the subsequence within a segment.
    Lcs,
    /// A tool result, a question set or a plan, by its tool-use id.
    ById,
    Order,
    Text,
}

#[derive(Clone, Debug)]
struct Op {
    c: Option<At>,
    e: Option<At>,
    paired: Option<Paired>,
    segment: usize,
}

fn entry_at(turns: &[Turn], at: At) -> &Entry {
    &turns[at.turn].entries[at.entry]
}

/// Whether an entry belongs to a call and pairs by the call's tool-use id rather than by its place: a
/// tool result, or the question set or plan a call settled.
fn is_result(entry: &Entry) -> bool {
    matches!(
        entry,
        Entry::ToolResult { .. } | Entry::Question { .. } | Entry::Plan { .. }
    )
}

/// Whether the record can be held to the turn of the entry at `index` in its turn.
fn attributable(attribution: Attribution, entry: &Entry, index: usize) -> bool {
    match attribution {
        Attribution::Full => true,
        Attribution::ToolCallsAndMidTurnMessages => match entry {
            Entry::ToolUse { .. } => true,
            // A message taken into a running turn; the words that open a turn are where it starts.
            Entry::User { via, .. } => index > 0 && *via != Via::Prompt,
            _ => false,
        },
        Attribution::None => false,
    }
}

/// `pieces` says, per CLI turn and entry, how many of the CLI's blocks a joined entry holds (one where
/// it is not given).
fn align_turns(cli: Vec<Turn>, eitri: Vec<Turn>, record: &EitriForm, pieces: &[Vec<usize>]) -> Alignment {
    let attribution = record.attribution;

    // Turns, paired by their first entry.
    let turn_key = |turn: &Turn| turn.entries.first().map(key);
    let cli_keys: Vec<Option<Key>> = cli.iter().map(turn_key).collect();
    let eitri_keys: Vec<Option<Key>> = eitri.iter().map(turn_key).collect();
    let anchors: Vec<(usize, usize)> = {
        // `None` (an empty turn) pairs with nothing.
        let wrap = |keys: &[Option<Key>], side: u8| -> Vec<(Option<Key>, u8, usize)> {
            keys.iter()
                .enumerate()
                .map(|(i, k)| {
                    (
                        k.clone(),
                        if k.is_some() { 0 } else { side },
                        if k.is_some() { 0 } else { i },
                    )
                })
                .collect()
        };
        lcs(&wrap(&cli_keys, 1), &wrap(&eitri_keys, 2))
    };
    let mut cli_anchor: HashMap<usize, usize> = HashMap::new();
    for &(c, e) in &anchors {
        cli_anchor.insert(c, e);
    }

    // Segments: an anchored pair and the unpaired turns after it (or before the first).
    let mut bounds: Vec<(usize, usize)> = vec![(0, 0)];
    bounds.extend(anchors.iter().copied());
    bounds.push((cli.len(), eitri.len()));
    let mut segments: Vec<(std::ops::Range<usize>, std::ops::Range<usize>)> = Vec::new();
    for window in bounds.windows(2) {
        let ((c0, e0), (c1, e1)) = (window[0], window[1]);
        if c0 < c1 || e0 < e1 {
            segments.push((c0..c1, e0..e1));
        }
    }

    // Tool results, question sets and plans pair by id across the whole form.
    let mut eitri_results: HashMap<Key, Vec<At>> = HashMap::new();
    for (t, turn) in eitri.iter().enumerate() {
        for (i, entry) in turn.entries.iter().enumerate() {
            if is_result(entry) {
                eitri_results
                    .entry(key(entry))
                    .or_default()
                    .push(At { turn: t, entry: i });
            }
        }
    }
    let mut result_partner: HashMap<At, At> = HashMap::new();
    let mut eitri_result_taken: HashMap<At, bool> = HashMap::new();
    for (t, turn) in cli.iter().enumerate() {
        for (i, entry) in turn.entries.iter().enumerate() {
            if is_result(entry) {
                if let Some(list) = eitri_results.get_mut(&key(entry)) {
                    if !list.is_empty() {
                        let partner = list.remove(0);
                        result_partner.insert(At { turn: t, entry: i }, partner);
                        eitri_result_taken.insert(partner, true);
                    }
                }
            }
        }
    }

    // Entries, paired within each segment.
    let mut ops: Vec<Op> = Vec::new();
    for (segment, (c_range, e_range)) in segments.iter().enumerate() {
        let flat = |turns: &[Turn], range: &std::ops::Range<usize>| -> Vec<At> {
            range
                .clone()
                .flat_map(|t| {
                    turns[t]
                        .entries
                        .iter()
                        .enumerate()
                        .filter(|(_, e)| !is_result(e))
                        .map(move |(i, _)| At { turn: t, entry: i })
                })
                .collect()
        };
        let c_flat = flat(&cli, c_range);
        let e_flat = flat(&eitri, e_range);
        let c_keys: Vec<Key> = c_flat.iter().map(|&at| key(entry_at(&cli, at))).collect();
        let e_keys: Vec<Key> = e_flat.iter().map(|&at| key(entry_at(&eitri, at))).collect();
        let pairs = lcs(&c_keys, &e_keys);
        let mut seg_ops: Vec<Op> = Vec::new();
        let (mut ci, mut ei) = (0, 0);
        let push_one = |ops: &mut Vec<Op>, c: Option<At>, e: Option<At>, paired: Option<Paired>| {
            ops.push(Op { c, e, paired, segment });
        };
        for &(pc, pe) in pairs.iter().chain(std::iter::once(&(c_flat.len(), e_flat.len()))) {
            while ci < pc {
                push_one(&mut seg_ops, Some(c_flat[ci]), None, None);
                ci += 1;
            }
            while ei < pe {
                push_one(&mut seg_ops, None, Some(e_flat[ei]), None);
                ei += 1;
            }
            if pc < c_flat.len() {
                push_one(&mut seg_ops, Some(c_flat[pc]), Some(e_flat[pe]), Some(Paired::Lcs));
                ci += 1;
                ei += 1;
            }
        }
        // Results go right after the entry they follow on their side.
        let mut seen_c_turns: Vec<usize> = Vec::new();
        let mut seen_e_turns: Vec<usize> = Vec::new();
        let mut placed_c: Vec<At> = Vec::new();
        let mut placed_e: Vec<At> = Vec::new();
        let results_after = |turns: &[Turn], at: Option<At>, turn: usize| -> Vec<At> {
            let start = at.map(|a| a.entry + 1).unwrap_or(0);
            turns[turn].entries[start..]
                .iter()
                .enumerate()
                .take_while(|(_, e)| is_result(e))
                .map(|(i, _)| At { turn, entry: start + i })
                .collect()
        };
        for op in seg_ops {
            if let Some(c) = op.c {
                if !seen_c_turns.contains(&c.turn) {
                    seen_c_turns.push(c.turn);
                    for r in results_after(&cli, None, c.turn) {
                        placed_c.push(r);
                        let partner = result_partner.get(&r).copied();
                        push_one(&mut ops, Some(r), partner, partner.map(|_| Paired::ById));
                    }
                }
            }
            if let Some(e) = op.e {
                if !seen_e_turns.contains(&e.turn) {
                    seen_e_turns.push(e.turn);
                    for r in results_after(&eitri, None, e.turn) {
                        if !eitri_result_taken.contains_key(&r) {
                            placed_e.push(r);
                            push_one(&mut ops, None, Some(r), None);
                        }
                    }
                }
            }
            let (c, e) = (op.c, op.e);
            ops.push(op);
            if let Some(c) = c {
                for r in results_after(&cli, Some(c), c.turn) {
                    placed_c.push(r);
                    let partner = result_partner.get(&r).copied();
                    push_one(&mut ops, Some(r), partner, partner.map(|_| Paired::ById));
                }
            }
            if let Some(e) = e {
                for r in results_after(&eitri, Some(e), e.turn) {
                    if !eitri_result_taken.contains_key(&r) {
                        placed_e.push(r);
                        push_one(&mut ops, None, Some(r), None);
                    }
                }
            }
        }
        // A turn holding nothing but results.
        for t in c_range.clone() {
            for (i, entry) in cli[t].entries.iter().enumerate() {
                let at = At { turn: t, entry: i };
                if is_result(entry) && !placed_c.contains(&at) {
                    let partner = result_partner.get(&at).copied();
                    push_one(&mut ops, Some(at), partner, partner.map(|_| Paired::ById));
                }
            }
        }
        for t in e_range.clone() {
            for (i, entry) in eitri[t].entries.iter().enumerate() {
                let at = At { turn: t, entry: i };
                if is_result(entry) && !placed_e.contains(&at) && !eitri_result_taken.contains_key(&at) {
                    push_one(&mut ops, None, Some(at), None);
                }
            }
        }
    }

    // One side's leftover whose twin is the other side's leftover elsewhere: out of order.
    let mut eitri_left: HashMap<Key, Vec<usize>> = HashMap::new();
    for (index, op) in ops.iter().enumerate() {
        if let (None, Some(e)) = (op.c, op.e) {
            eitri_left.entry(key(entry_at(&eitri, e))).or_default().push(index);
        }
    }
    let mut removed: Vec<bool> = vec![false; ops.len()];
    for index in 0..ops.len() {
        if let (Some(c), None) = (ops[index].c, ops[index].e) {
            if let Some(list) = eitri_left.get_mut(&key(entry_at(&cli, c))) {
                if let Some(other) = list.first().copied() {
                    list.remove(0);
                    ops[index].e = ops[other].e;
                    ops[index].paired = Some(Paired::Order);
                    removed[other] = true;
                }
            }
        }
    }
    let mut ops: Vec<Op> = ops
        .into_iter()
        .zip(removed)
        .filter_map(|(op, gone)| (!gone).then_some(op))
        .collect();

    // Leftovers side by side, of one kind with text: the same entry with different words.
    let mut index = 0;
    while index + 1 < ops.len() {
        let (a, b) = (&ops[index], &ops[index + 1]);
        let pair = match (a.c, a.e, b.c, b.e) {
            (Some(c), None, None, Some(e)) | (None, Some(e), Some(c), None) if a.segment == b.segment => {
                let (ce, ee) = (entry_at(&cli, c), entry_at(&eitri, e));
                let same_kind = ce.kind() == ee.kind()
                    && matches!(
                        ce,
                        Entry::User { .. } | Entry::Assistant { .. } | Entry::Thinking { .. }
                    );
                same_kind.then_some((c, e))
            }
            _ => None,
        };
        match pair {
            Some((c, e)) => {
                ops[index] = Op {
                    c: Some(c),
                    e: Some(e),
                    paired: Some(Paired::Text),
                    segment: ops[index].segment,
                };
                ops.remove(index + 1);
            }
            None => index += 1,
        }
    }

    // Where each CLI turn sits in the record.
    let mut place: HashMap<usize, usize> = HashMap::new();
    for (c, _) in cli.iter().enumerate() {
        if let Some(&e) = cli_anchor.get(&c) {
            place.insert(c, e);
            continue;
        }
        let matched = ops
            .iter()
            .filter(|op| op.paired == Some(Paired::Lcs) && op.c.is_some_and(|at| at.turn == c) && op.e.is_some());
        let mut first_any = None;
        let mut first_attributable = None;
        for op in matched {
            let (cat, eat) = (op.c.expect("filtered"), op.e.expect("filtered"));
            first_any.get_or_insert(eat.turn);
            if attributable(attribution, entry_at(&cli, cat), cat.entry) {
                first_attributable = Some(eat.turn);
                break;
            }
        }
        if let Some(e) = first_attributable.or(first_any) {
            place.insert(c, e);
        }
    }
    let mut sharing: HashMap<usize, usize> = HashMap::new();
    for e in place.values() {
        *sharing.entry(*e).or_default() += 1;
    }
    let order_only = |c: usize| -> bool {
        match attribution {
            Attribution::Full => false,
            Attribution::None => true,
            Attribution::ToolCallsAndMidTurnMessages => {
                let turn = &cli[c];
                turn.origin != Origin::Human
                    && !turn
                        .entries
                        .iter()
                        .enumerate()
                        .any(|(i, e)| attributable(attribution, e, i))
            }
        }
    };
    // A record turn a prompt opened takes its turn id from its first tool call. When the CLI turn of that
    // prompt made no call, the record cannot tell it from the turn the CLI started next, whose calls then
    // carry the id: the first CLI turn whose calls sit in such a record turn is compared by order only.
    let mut id_from_later_turn: Vec<usize> = Vec::new();
    if attribution == Attribution::ToolCallsAndMidTurnMessages {
        for (e, turn) in eitri.iter().enumerate() {
            if !matches!(turn.entries.first(), Some(Entry::User { via: Via::Prompt, .. })) {
                continue;
            }
            let first_caller = ops
                .iter()
                .filter_map(|op| match (op.c, op.e) {
                    (Some(c), Some(r)) if r.turn == e && matches!(entry_at(&cli, c), Entry::ToolUse { .. }) => {
                        Some(c.turn)
                    }
                    _ => None,
                })
                .min();
            if let Some(c) = first_caller {
                if cli_anchor.get(&c) != Some(&e) {
                    id_from_later_turn.push(c);
                }
            }
        }
    }
    let mut how: Vec<TurnMatch> = (0..cli.len())
        .map(|c| {
            if cli_anchor.contains_key(&c) {
                TurnMatch::Same
            } else if let Some(e) = place.get(&c) {
                if sharing[e] > 1 {
                    if order_only(c) || id_from_later_turn.contains(&c) {
                        TurnMatch::OrderOnly
                    } else {
                        TurnMatch::Merged
                    }
                } else {
                    TurnMatch::ByEntries
                }
            } else {
                TurnMatch::CliOnly
            }
        })
        .collect();

    // Marks.
    let mut marks: Vec<Mark> = Vec::with_capacity(ops.len());
    let mut nested: Vec<Option<Box<Alignment>>> = Vec::with_capacity(ops.len());
    for op in &ops {
        let mut sub = None;
        let mark = match (op.c, op.e) {
            (Some(_), None) => Mark::CliOnly,
            (None, Some(_)) => Mark::EitriOnly,
            (Some(c), Some(e)) => {
                let (ce, ee) = (entry_at(&cli, c), entry_at(&eitri, e));
                let (mut fields, notes) = compare(ce, ee, record);
                match op.paired {
                    Some(Paired::Order) => fields.push(Field::Order),
                    Some(Paired::Text) => fields.push(Field::Text),
                    _ => {}
                }
                let checked = !matches!(how[c.turn], TurnMatch::OrderOnly) && attributable(attribution, ce, c.entry);
                if checked && place.get(&c.turn).is_some_and(|&p| p != e.turn) {
                    fields.push(Field::Turn);
                }
                if let (Entry::Subagent { turns: ct, .. }, Entry::Subagent { turns: et, .. }) = (ce, ee) {
                    let inner = EitriForm {
                        source: record.source,
                        turns: Vec::new(),
                        attribution,
                        omission: Omission::None,
                        changed_input: record.changed_input.clone(),
                    };
                    sub = Some(Box::new(align_turns(ct.clone(), et.clone(), &inner, &[])));
                }
                fields.sort();
                fields.dedup();
                if !fields.is_empty() {
                    Mark::Differs(fields)
                } else if !notes.is_empty() {
                    Mark::Normalised(notes)
                } else {
                    Mark::Same
                }
            }
            (None, None) => unreachable!("every op holds an entry"),
        };
        marks.push(mark);
        nested.push(sub);
    }

    // The record's truncation: the CLI entries before the first pair, when the record accounts for them.
    let first_pair = ops
        .iter()
        .position(|op| op.c.is_some() && op.e.is_some())
        .unwrap_or(ops.len());
    let prefix_clean = ops[..first_pair].iter().all(|op| op.e.is_none());
    let mut truncated = 0;
    let cut = if prefix_clean {
        truncated_prefix(&record.omission, &cli, pieces, &ops[..first_pair])
    } else {
        0
    };
    if cut > 0 {
        for mark in marks[..cut].iter_mut() {
            *mark = Mark::Truncated;
            truncated += 1;
        }
        for (c, turn_how) in how.iter_mut().enumerate() {
            let all_cut = ops[..cut]
                .iter()
                .filter(|op| op.c.is_some_and(|at| at.turn == c))
                .count()
                == cli[c].entries.len();
            if *turn_how == TurnMatch::CliOnly && all_cut {
                *turn_how = TurnMatch::Truncated;
            }
        }
    }

    // Turn pairs, in op order.
    let mut turns: Vec<TurnPair> = Vec::new();
    let mut cli_pair: HashMap<usize, usize> = HashMap::new();
    let mut eitri_pair: HashMap<usize, usize> = HashMap::new();
    let placed_by: HashMap<usize, usize> = {
        let mut by = HashMap::new();
        for c in 0..cli.len() {
            if let Some(&e) = place.get(&c) {
                by.entry(e).or_insert(c);
            }
        }
        by
    };
    for ((op, mark), sub) in ops.iter().zip(marks).zip(nested) {
        let holder = match (op.c, op.e) {
            (Some(c), _) => Some(c.turn),
            (None, Some(e)) => placed_by.get(&e.turn).copied(),
            (None, None) => None,
        };
        let slot = match holder {
            Some(c) => *cli_pair.entry(c).or_insert_with(|| {
                turns.push(TurnPair {
                    cli: Some(c),
                    eitri: place.get(&c).copied(),
                    how: how[c].clone(),
                    entries: Vec::new(),
                });
                turns.len() - 1
            }),
            None => {
                let e = op.e.expect("an op without a CLI side has a record side").turn;
                *eitri_pair.entry(e).or_insert_with(|| {
                    turns.push(TurnPair {
                        cli: None,
                        eitri: Some(e),
                        how: TurnMatch::EitriOnly,
                        entries: Vec::new(),
                    });
                    turns.len() - 1
                })
            }
        };
        turns[slot].entries.push(EntryPair {
            cli: op.c,
            eitri: op.e,
            mark,
            nested: sub,
        });
    }
    for (e, turn) in eitri.iter().enumerate() {
        if turn.entries.is_empty() && !placed_by.contains_key(&e) {
            turns.push(TurnPair {
                cli: None,
                eitri: Some(e),
                how: TurnMatch::EitriOnly,
                entries: Vec::new(),
            });
        }
    }

    Alignment {
        source: record.source,
        cli,
        eitri,
        projected_out: BTreeMap::new(),
        turns,
        truncated,
    }
}

/// The fields of two paired entries that differ, and what normalisation did to make the rest equal.
fn compare(cli: &Entry, eitri: &Entry, record: &EitriForm) -> (Vec<Field>, Vec<Note>) {
    let mut fields = Vec::new();
    let mut notes = Vec::new();
    let text_notes = |a: &str, b: &str, user: bool, notes: &mut Vec<Note>| {
        if a == b {
            return;
        }
        if user && (strip_composed_block(a) != a || strip_composed_block(b) != b) {
            notes.push(Note::ComposedBlock);
        }
        let (sa, sb) = if user {
            (strip_composed_block(a), strip_composed_block(b))
        } else {
            (a, b)
        };
        if sa != sb {
            notes.push(Note::Whitespace);
        }
    };
    match (cli, eitri) {
        (
            Entry::User {
                text: a,
                via: va,
                images: ia,
            },
            Entry::User {
                text: b,
                via: vb,
                images: ib,
            },
        ) => {
            if user_text(a) != user_text(b) {
                fields.push(Field::Text);
            } else {
                text_notes(a, b, true, &mut notes);
            }
            if ia != ib {
                fields.push(Field::Images);
            }
            if (*va == Via::Prompt) != (*vb == Via::Prompt) {
                fields.push(Field::Via);
            }
        }
        (Entry::Assistant { text: a }, Entry::Assistant { text: b })
        | (Entry::Thinking { text: a }, Entry::Thinking { text: b }) => {
            if text(a) != text(b) {
                fields.push(Field::Text);
            } else {
                text_notes(a, b, false, &mut notes);
            }
        }
        (
            Entry::ToolUse {
                id,
                name: na,
                input: ia,
            },
            Entry::ToolUse {
                name: nb, input: ib, ..
            },
        ) => {
            if na != nb {
                fields.push(Field::Name);
            }
            if canonical_json(ia) != canonical_json(ib) {
                fields.push(Field::Input);
            }
            if record.changed_input.contains_key(id) {
                notes.push(Note::ChangedInput);
            }
        }
        (
            Entry::ToolResult {
                is_error: ea,
                content: ca,
                ..
            },
            Entry::ToolResult {
                is_error: eb,
                content: cb,
                ..
            },
        ) => {
            if ea != eb {
                fields.push(Field::IsError);
            }
            if text(&result_text(ca)) != text(&result_text(cb)) {
                fields.push(Field::Result);
            } else if canonical_json(ca) != canonical_json(cb) {
                notes.push(Note::ResultFlattened);
            }
        }
        (Entry::Question { key: ka, .. }, Entry::Question { key: kb, .. }) => {
            if ka.count != kb.count {
                fields.push(Field::Questions);
            }
            if ka.answered != kb.answered {
                fields.push(Field::Answered);
            }
            if ka.end != kb.end {
                fields.push(Field::Outcome);
            }
        }
        (Entry::Plan { key: ka, .. }, Entry::Plan { key: kb, .. }) => {
            if ka.plan_sha256 != kb.plan_sha256 {
                fields.push(Field::PlanText);
            }
            if ka.end != kb.end {
                fields.push(Field::Outcome);
            }
        }
        (a, b) if a.kind() != b.kind() => fields.push(Field::Text),
        _ => {}
    }
    (fields, notes)
}

/// How many of the CLI-only ops `prefix` (everything before the first pair) the record accounts for as
/// its own truncation, counted from the oldest.
///
/// A record that counted what it left out accounts for that many items by the caps' measure (a person's
/// prompt, each reply block, each tool call with its result, question set and plan), and an older entry of no counted kind
/// between them; what lies past the count is a loss of its own. A record that could not count accounts
/// for the whole prefix, and so does the stored history when the newest item left out could not have
/// fitted its caps.
fn truncated_prefix(omission: &Omission, cli: &[Turn], pieces: &[Vec<usize>], prefix: &[Op]) -> usize {
    match omission {
        Omission::None => 0,
        Omission::Recorded(None) => prefix.len(),
        Omission::Recorded(Some(n)) => {
            let mut counted = 0;
            let mut cut_calls: Vec<&str> = Vec::new();
            let mut cut = 0;
            for (k, op) in prefix.iter().enumerate() {
                let Some(at) = op.c else { break };
                let entry = entry_at(cli, at);
                let blocks = pieces.get(at.turn).and_then(|t| t.get(at.entry)).copied().unwrap_or(1);
                let fits = match entry {
                    Entry::User { via: Via::Prompt, .. } if cli[at.turn].origin == Origin::Human => {
                        counted += 1;
                        counted <= *n
                    }
                    Entry::Assistant { .. } => {
                        counted += blocks;
                        counted <= *n
                    }
                    Entry::ToolUse { id, .. } => {
                        counted += 1;
                        cut_calls.push(id);
                        counted <= *n
                    }
                    // A result, a question set and a plan go with their call, which the count already holds.
                    Entry::ToolResult { id, .. } | Entry::Question { id, .. } | Entry::Plan { id, .. } => {
                        cut_calls.contains(&id.as_str())
                    }
                    _ => counted < *n,
                };
                if !fits {
                    break;
                }
                cut = k + 1;
            }
            cut
        }
        Omission::Bounds { items, chars } => {
            // The newest item left out must be one the caps' walk could not take: the item cap was
            // reached, or it would have broken the character cap.
            let newest = prefix.iter().rev().find_map(|op| {
                let at = op.c?;
                let entry = entry_at(cli, at);
                item_chars(entry, &cli[at.turn], at.entry)
            });
            match newest {
                Some(cost) if *items >= HISTORY_MAX_ITEMS || chars + cost > HISTORY_MAX_CHARS => prefix.len(),
                _ => 0,
            }
        }
    }
}

/// An entry's size by the caps' measure, if it is one of the items they count.
fn item_chars(entry: &Entry, turn: &Turn, index: usize) -> Option<usize> {
    match entry {
        Entry::User { text, .. } | Entry::Assistant { text } => Some(text.chars().count()),
        Entry::ToolUse { id, name, input } => {
            let result = turn.entries[index..].iter().find_map(|e| match e {
                Entry::ToolResult { id: rid, content, .. } if rid == id => Some(value_chars(content)),
                _ => None,
            });
            Some(name.chars().count() + value_chars(input) + result.unwrap_or(0))
        }
        _ => None,
    }
}

impl Alignment {
    /// The CLI entry an [`At`] names.
    pub fn cli_entry(&self, at: At) -> &Entry {
        entry_at(&self.cli, at)
    }

    /// The record's entry an [`At`] names.
    pub fn eitri_entry(&self, at: At) -> &Entry {
        entry_at(&self.eitri, at)
    }

    /// Every entry pair, in order, with nested subagent pairs left inside their entry.
    pub fn entry_pairs(&self) -> impl Iterator<Item = &EntryPair> {
        self.turns.iter().flat_map(|t| t.entries.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lcs_keeps_order_and_finds_the_longest_run() {
        let a = ["a", "b", "c", "d", "e"];
        let b = ["a", "c", "x", "d", "e"];
        assert_eq!(lcs(&a, &b), vec![(0, 0), (2, 1), (3, 3), (4, 4)]);
        assert_eq!(lcs::<&str>(&[], &["a"]), vec![]);
        assert_eq!(lcs(&["a"], &["a"]), vec![(0, 0)]);
    }
}
