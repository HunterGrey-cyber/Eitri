//! Runs a window's editor calls: one call per tab in flight, the others queued, a call waiting on a person's card
//! outside the queue altogether. Nothing here waits. Every chunk's answer is a `Pending` read from [`Broker::tick`],
//! and that tick is the only place a run is stepped.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::rc::Rc;
use std::time::Instant;

use agent::setting_sources::ProjectTrust;
use agent::PermissionMode;
use rmpv::Value;

use super::cards::{CardAnswer, CardDecision, CardLine, CardOrigin, CardProposal, EditorCardWire};
use super::memory::Memory;
use super::stamp::{wall_ms, Sequencer, STAMP_MARGIN};
use super::step::{
    encoded_len, AgentCall, BridgeCallId, EditorFailure, EditorReach, EditorSight, Step, StepEvent, Tool, ToolCx,
    ToolError, ToolResult, ToolRun, ANSWER_MAX_ENCODED,
};
use super::tools::baseline::{BaselineRun, TurnBaseline};
use super::tools::{diagnostics, tool_named};
use super::way::EditorWay;
use super::{words, CARD_LIMIT};
use crate::editor_context::EditorContext;
use crate::nvim_rpc::{Pending, RpcError};
use crate::tabs::TabId;
use crate::turn_review::ProposalWriter;

/// The most calls one tab keeps waiting for its turn.
const WAITING_MAX: usize = 16;

/// The most calls one tab keeps on a card. With the queue and the one in flight, a tab stays well under the
/// sidecar's 64 open calls on a session, and a model cannot raise cards without limit.
const PARKED_MAX: usize = 16;

/// What a card shows as the tool of a run the host started.
const HOST_TOOL: &str = "eitri";

/// What the panel knows of a tab, read on every tick.
#[derive(Debug, Clone)]
pub struct TabFacts {
    pub mode: PermissionMode,
    pub trust: ProjectTrust,
    /// The conversation's id, which `/clear` replaces.
    pub session: String,
    /// The id the agent's calls are answered under (Verdandi's session on a Claude tab), which only a new start
    /// replaces. A line belongs to this: `/clear` keeps the process, and an agent running in the background with it.
    pub process: String,
    /// The tab's `last_revision`, a raised card's `seq`.
    pub revision: u64,
    /// What this session was given: `declared_client_tools` on a sidecar tab, the list the bridge was given on an
    /// external one.
    pub declared: Rc<BTreeSet<String>>,
}

pub trait BrokerEnv {
    /// `None`: no way to any editor.
    fn way(&self) -> Option<&dyn EditorWay>;
    /// `None`: the tab is gone or has no session.
    fn facts(&self, tab: TabId) -> Option<TabFacts>;
    fn root(&self) -> &Path;
    fn home(&self) -> Option<&Path>;
    fn context(&self) -> Option<EditorContext>;
    /// Where a proposal keeps its bytes. `None`: this window can keep none.
    fn proposal_writer(&self) -> Option<ProposalWriter> {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    Sidecar {
        call_id: String,
    },
    Bridge {
        id: BridgeCallId,
    },
    /// A run the host started itself: nothing is answered.
    Host,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrokerOut {
    Answer {
        tab: TabId,
        caller: Caller,
        result: ToolResult,
    },
    CardsChanged {
        tab: TabId,
    },
}

/// Only this file can make one, so only [`Broker::answer_card`] can make a [`CardAnswer`]: a tool holding an
/// answer holds a person's answer.
pub(super) struct AnswerSeal(());

/// One open call. Which line of its tab holds it says what it waits on.
struct OpenCall {
    id: String,
    caller: Caller,
    tool_use_id: Option<String>,
    tool: &'static str,
    run: Box<dyn ToolRun>,
}

fn agent_call<'a>(caller: &'a Caller, tool_use_id: Option<&'a str>) -> AgentCall<'a> {
    match caller {
        Caller::Sidecar { .. } => AgentCall::Claude(tool_use_id),
        Caller::Bridge { id } => AgentCall::Bridge {
            call_id: tool_use_id,
            request: id,
        },
        Caller::Host => AgentCall::Host,
    }
}

/// A call waiting for its turn, and what its run hears when the turn comes.
struct Queued {
    call: OpenCall,
    event: StepEvent,
}

/// The tab's one call on the editor.
struct Flying {
    call: OpenCall,
    waits_on: FlightWait,
}

enum FlightWait {
    Editor { pending: Pending, give_up: Instant },
    Poll { due: Instant },
}

/// A call on a person's card. It takes no turn until the card is answered.
struct Parked {
    call: OpenCall,
    id: String,
    seq: u64,
    title: String,
    body: Vec<CardLine>,
    binding: Option<[u8; 32]>,
    proposal: Option<CardProposal>,
    raised: Instant,
}

impl Parked {
    fn is_past_its_limit(&self, now: Instant) -> bool {
        now >= self.raised + CARD_LIMIT
    }

    fn wire(&self) -> EditorCardWire {
        EditorCardWire {
            id: self.id.clone(),
            seq: self.seq,
            tool: self.call.tool.to_owned(),
            tool_use_id: self.call.tool_use_id.clone(),
            title: self.title.clone(),
            body: self.body.clone(),
            proposal: self.proposal.clone(),
        }
    }
}

/// What one tab holds.
struct Line {
    /// `None` on a line a host run made, until the next tick reads the tab's process.
    process: Option<String>,
    sequencer: Sequencer,
    queue: VecDeque<Queued>,
    flight: Option<Flying>,
    parked: Vec<Parked>,
}

impl Line {
    fn holds_nothing(&self) -> bool {
        self.queue.is_empty() && self.flight.is_none() && self.parked.is_empty()
    }

    /// Takes every call `which` names, wherever it is, and says whether a card went with one.
    fn take_where(&mut self, which: impl Fn(&OpenCall) -> bool) -> (Vec<OpenCall>, bool) {
        let mut taken = Vec::new();
        if self.flight.as_ref().is_some_and(|flying| which(&flying.call)) {
            taken.extend(self.flight.take().map(|flying| flying.call));
        }
        for queued in std::mem::take(&mut self.queue) {
            if which(&queued.call) {
                taken.push(queued.call);
            } else {
                self.queue.push_back(queued);
            }
        }
        let cards_before = self.parked.len();
        for parked in std::mem::take(&mut self.parked) {
            if which(&parked.call) {
                taken.push(parked.call);
            } else {
                self.parked.push(parked);
            }
        }
        (taken, self.parked.len() != cards_before)
    }
}

/// The ids and scopes of one broker. The random value keeps them apart from the other window's: a card's id in a
/// page can only ever meet the broker that raised it.
struct Ids {
    random: String,
    calls: u64,
    cards: u64,
    lines: u64,
}

impl Ids {
    fn new() -> Ids {
        Ids {
            random: format!("{:016x}", uuid::Uuid::new_v4().as_u128() as u64),
            calls: 0,
            cards: 0,
            lines: 0,
        }
    }

    fn call(&mut self) -> String {
        self.calls += 1;
        format!("{}-{}", self.random, self.calls)
    }

    /// New for each raised card, from its own counter: a key on a stale card can only meet "no such card", and a
    /// call that asks twice has two ids.
    fn card(&mut self) -> String {
        self.cards += 1;
        format!("ec-{}-{}", self.random, self.cards)
    }

    /// A line's stamps count in a scope of its own. nvim keeps a scope's last number after the line that used it has
    /// gone, so a new line counting from 1 in the tab's old scope would have its first calls refused as older.
    fn line(&mut self, tab: TabId, process: Option<String>) -> Line {
        self.lines += 1;
        Line {
            process,
            sequencer: Sequencer::new(format!("{}-{}-{}", self.random, tab.0, self.lines)),
            queue: VecDeque::new(),
            flight: None,
            parked: Vec::new(),
        }
    }
}

pub struct Broker {
    lines: BTreeMap<TabId, Line>,
    ids: Ids,
    memory: Memory,
}

impl Default for Broker {
    fn default() -> Broker {
        Broker::new()
    }
}

fn refusal(text: impl Into<String>) -> ToolResult {
    ToolResult {
        text: text.into(),
        is_error: true,
    }
}

fn not_available(tool: &str) -> ToolResult {
    refusal(format!("{tool} is not available in this session"))
}

/// Every answer leaves through here, so none is over the cap, whoever wrote it. A host run answers nobody.
fn answer(out: &mut Vec<BrokerOut>, tab: TabId, caller: Caller, result: ToolResult) {
    if caller == Caller::Host {
        return;
    }
    let result = if encoded_len(&result.text) > ANSWER_MAX_ENCODED {
        refusal(words::TOO_LARGE)
    } else {
        result
    };
    out.push(BrokerOut::Answer { tab, caller, result });
}

/// What a run hears of a chunk's reply. `{ expired = true }` is a chunk that did nothing.
fn heard(reply: Result<Value, RpcError>) -> StepEvent {
    StepEvent::Answer(match reply {
        Ok(value) if is_expired(&value) => Err(EditorFailure::NotAnswered),
        Ok(value) => Ok(value),
        Err(RpcError::Unavailable(why)) => Err(EditorFailure::Unavailable(why)),
        Err(other) => Err(EditorFailure::Refused(other.to_string())),
    })
}

fn is_expired(value: &Value) -> bool {
    value.as_map().is_some_and(|map| {
        map.iter()
            .any(|(key, value)| key.as_str() == Some("expired") && value.as_bool() == Some(true))
    })
}

fn tool_cx<'a>(
    tab: TabId,
    facts: &'a TabFacts,
    env: &'a dyn BrokerEnv,
    context: Option<&'a EditorContext>,
    call: &'a str,
    agent_call: AgentCall<'a>,
    memory: &'a Memory,
) -> ToolCx<'a> {
    ToolCx {
        mode: facts.mode,
        trust: facts.trust,
        root: env.root(),
        home: env.home(),
        reach: env.way().map_or(EditorReach::Own, |way| way.reach()),
        sight: env.way().map_or(EditorSight::OnScreen, |way| way.sight()),
        context,
        tab: tab.0,
        session: &facts.session,
        call,
        agent_call,
        memory,
    }
}

/// One tab's share of a tick.
struct Round<'a> {
    tab: TabId,
    facts: &'a TabFacts,
    env: &'a dyn BrokerEnv,
    memory: &'a Memory,
    ids: &'a mut Ids,
    now: Instant,
    out: &'a mut Vec<BrokerOut>,
}

impl Round<'_> {
    fn run(&mut self, line: &mut Line) {
        // First: whatever this tick hears may raise a card, and a card that has ended holds no place.
        self.end_cards_past_their_limit(line);
        self.hear_flight(line);
        while line.flight.is_none() {
            let Some(Queued { call, event }) = line.queue.pop_front() else {
                break;
            };
            self.advance(line, call, event);
        }
    }

    /// A chunk's reply, a chunk past its budget, or a poll that came due. A chunk given up on is dropped here; it
    /// may still run in the editor, where its stamp makes it do nothing.
    fn hear_flight(&mut self, line: &mut Line) {
        let Some(flying) = line.flight.take() else {
            return;
        };
        let event = match &flying.waits_on {
            FlightWait::Editor { pending, give_up } => match pending.try_take() {
                Some(reply) => Some(heard(reply)),
                None if self.now >= *give_up => Some(StepEvent::Answer(Err(EditorFailure::NotAnswered))),
                None => None,
            },
            FlightWait::Poll { due } => (self.now >= *due).then_some(StepEvent::Poll),
        };
        match event {
            Some(event) => self.advance(line, flying.call, event),
            None => line.flight = Some(flying),
        }
    }

    /// The call ends with the tool not asked. The agent's side may have given up long before; this limit is what
    /// ends a card nobody answers.
    fn end_cards_past_their_limit(&mut self, line: &mut Line) {
        let (late, waiting): (Vec<Parked>, Vec<Parked>) = std::mem::take(&mut line.parked)
            .into_iter()
            .partition(|parked| parked.is_past_its_limit(self.now));
        line.parked = waiting;
        if late.is_empty() {
            return;
        }
        for parked in late {
            answer(self.out, self.tab, parked.call.caller, refusal(words::NOT_ANSWERED));
        }
        self.out.push(BrokerOut::CardsChanged { tab: self.tab });
    }

    /// The tab's proposal `id` as its card carries it. Taken when the card is raised: a card's wire is built on every
    /// snapshot, with no tab's memory at hand.
    fn card_proposal(&self, id: &str) -> Option<CardProposal> {
        let proposal = self.memory.proposal(self.tab.0, id)?;
        let files = proposal
            .files
            .iter()
            .map(|file| file.rel.to_string_lossy().into_owned());
        Some(CardProposal {
            id: proposal.id.clone(),
            files: files.collect(),
        })
    }

    /// Steps `call` from `event` until it waits on something or ends. A step that cannot leave the broker comes
    /// straight back as the run's next event.
    fn advance(&mut self, line: &mut Line, mut call: OpenCall, mut event: StepEvent) {
        let context = self.env.context();
        loop {
            let cx = tool_cx(
                self.tab,
                self.facts,
                self.env,
                context.as_ref(),
                &call.id,
                agent_call(&call.caller, call.tool_use_id.as_deref()),
                self.memory,
            );
            match call.run.step(event, &cx) {
                Step::Done(result) => return answer(self.out, self.tab, call.caller, result),
                Step::PollAfter(wait) => {
                    let due = self.now + wait;
                    line.flight = Some(Flying {
                        call,
                        waits_on: FlightWait::Poll { due },
                    });
                    return;
                }
                Step::Ask(request) => {
                    if line.parked.len() >= PARKED_MAX {
                        return answer(self.out, self.tab, call.caller, refusal(words::TOO_MANY));
                    }
                    let proposal = request.proposal.as_deref().and_then(|id| self.card_proposal(id));
                    line.parked.push(Parked {
                        call,
                        id: self.ids.card(),
                        seq: self.facts.revision,
                        title: request.title,
                        body: request.body,
                        binding: request.binding,
                        proposal,
                        raised: self.now,
                    });
                    self.out.push(BrokerOut::CardsChanged { tab: self.tab });
                    return;
                }
                Step::Call {
                    call: editor_call,
                    budget,
                } => {
                    // The chunk stops counting first, so a chunk that ran is answered before the broker gives
                    // up, and the agent is never told nothing was shown while a window opens. The give-up counts
                    // from the tick's `now` and the stamp from the wall clock as it reads it here, so what the
                    // tick has spent since `now` comes off the stamp too: a stall cannot push it past the give-up.
                    let spent = Instant::now().saturating_duration_since(self.now);
                    let chunk_budget = budget.saturating_sub(STAMP_MARGIN).saturating_sub(spent);
                    // Its stamp would already be past: the chunk could only answer that it did nothing.
                    if chunk_budget.is_zero() {
                        event = StepEvent::Answer(Err(EditorFailure::NotAnswered));
                        continue;
                    }
                    let Some(way) = self.env.way() else {
                        event = StepEvent::Answer(Err(EditorFailure::Unavailable(words::NO_EDITOR.to_owned())));
                        continue;
                    };
                    let stamp = line.sequencer.stamp(chunk_budget);
                    let pending = way.call(&self.facts.session, &editor_call, &stamp);
                    let give_up = self.now + budget;
                    line.flight = Some(Flying {
                        call,
                        waits_on: FlightWait::Editor { pending, give_up },
                    });
                    return;
                }
            }
        }
    }
}

impl Broker {
    pub fn new() -> Broker {
        Broker {
            lines: BTreeMap::new(),
            ids: Ids::new(),
            memory: Memory::default(),
        }
    }

    /// What the tabs keep between calls: a proposal a person asks to see is read from here.
    pub fn memory(&self) -> &Memory {
        &self.memory
    }

    #[allow(clippy::too_many_arguments)]
    pub fn submit(
        &mut self,
        tab: TabId,
        caller: Caller,
        tool_use_id: Option<String>,
        tool: &str,
        arguments: &str,
        env: &dyn BrokerEnv,
        now: Instant,
    ) -> Vec<BrokerOut> {
        let mut out = Vec::new();
        // Off means not declared, and `tool_named` finds every row of the catalogue while the bridge passes any
        // name it is sent. So this comes before the lookup, whoever the caller says it is.
        let declared = env.facts(tab).is_some_and(|facts| facts.declared.contains(tool));
        if !declared {
            answer(&mut out, tab, caller, not_available(tool));
            return out;
        }
        let Some(found) = tool_named(tool) else {
            answer(&mut out, tab, caller, refusal(format!("unknown tool: {tool}")));
            return out;
        };
        self.submit_tool(tab, caller, tool_use_id, found, arguments, env, now)
    }

    /// `submit` after the lookup.
    #[allow(clippy::too_many_arguments)]
    fn submit_tool(
        &mut self,
        tab: TabId,
        caller: Caller,
        tool_use_id: Option<String>,
        tool: &'static dyn Tool,
        arguments: &str,
        env: &dyn BrokerEnv,
        _now: Instant,
    ) -> Vec<BrokerOut> {
        let mut out = Vec::new();
        let name = tool.row().name;
        let Some(facts) = env.facts(tab) else {
            answer(&mut out, tab, caller, not_available(name));
            return out;
        };
        if !self.line_is_of_process(tab, &facts) {
            self.drop_line(tab, &mut out);
        }
        let args = match serde_json::from_str::<serde_json::Value>(arguments) {
            Ok(args) if args.is_object() => args,
            _ => {
                answer(&mut out, tab, caller, refusal("the arguments must be an object"));
                return out;
            }
        };
        let id = self.ids.call();
        let context = env.context();
        self.memory.set_writer(env.proposal_writer());
        let began = tool.begin(
            &args,
            &tool_cx(
                tab,
                &facts,
                env,
                context.as_ref(),
                &id,
                agent_call(&caller, tool_use_id.as_deref()),
                &self.memory,
            ),
        );
        let run = match began {
            Ok(run) => run,
            Err(ToolError(why)) => {
                answer(&mut out, tab, caller, refusal(why));
                return out;
            }
        };
        let ids = &mut self.ids;
        let line = self
            .lines
            .entry(tab)
            .or_insert_with(|| ids.line(tab, Some(facts.process.clone())));
        if line.queue.len() >= WAITING_MAX || line.parked.len() >= PARKED_MAX {
            answer(&mut out, tab, caller, refusal(words::TOO_MANY));
            return out;
        }
        line.queue.push_back(Queued {
            call: OpenCall {
                id,
                caller,
                tool_use_id,
                tool: name,
                run,
            },
            event: StepEvent::Start,
        });
        out
    }

    /// A run made by the host, queued as a call is. A tab with no line yet gets one, which takes its session at the
    /// next `tick`. It counts against no limit of its own: the host makes few, and it answers nobody.
    pub fn submit_run(&mut self, tab: TabId, run: Box<dyn ToolRun>, _now: Instant) {
        let id = self.ids.call();
        let ids = &mut self.ids;
        let line = self.lines.entry(tab).or_insert_with(|| ids.line(tab, None));
        line.queue.push_back(Queued {
            call: OpenCall {
                id,
                caller: Caller::Host,
                tool_use_id: None,
                tool: HOST_TOOL,
                run,
            },
            event: StepEvent::Start,
        });
    }

    /// A turn of the tab began. When the tab has `diagnostics`, the editor is asked once for every diagnostic it holds,
    /// as the baseline `diagnostics {since: "turn"}` subtracts; the run is queued ahead of whatever the turn calls, and
    /// answers nobody. A tab without the tool takes none. What the tab's memory held of an earlier turn is replaced
    /// at once, so a late answer of that turn finds it gone.
    pub fn turn_began(&mut self, tab: TabId, turn: &str, env: &dyn BrokerEnv, now: Instant) -> Vec<BrokerOut> {
        let mut out = Vec::new();
        let Some(facts) = env.facts(tab) else {
            return out;
        };
        if !facts.declared.contains(diagnostics::ROW.name) {
            return out;
        }
        // A new session's first turn: the line of the old one goes first, with its memory.
        if !self.line_is_of_process(tab, &facts) {
            self.drop_line(tab, &mut out);
        }
        self.memory.with(tab.0, |memory| {
            memory.baseline = Some(TurnBaseline::Taking { turn: turn.to_owned() });
        });
        self.submit_run(tab, Box::new(BaselineRun::new(turn)), now);
        out
    }

    /// The agent gave the call up (`ClientToolCallSettled`, `notifications/cancelled`): its card goes, nothing is
    /// answered, and a chunk out stays harmless by its stamp.
    pub fn cancel(&mut self, tab: TabId, caller: &Caller) -> Vec<BrokerOut> {
        if *caller == Caller::Host {
            return Vec::new();
        }
        self.take_calls(tab, |call| call.caller == *caller, None)
    }

    /// The person's answer; the call goes to the back of the queue. An unknown id, a card of another tab and a card
    /// past its limit are all `Err`: the last has ended, whether or not a tick has said so yet.
    pub fn answer_card(
        &mut self,
        tab: TabId,
        card: &str,
        decision: CardDecision,
        origin: CardOrigin,
        now: Instant,
    ) -> Result<Vec<BrokerOut>, String> {
        let no_such_card = || format!("no such card: {card}");
        let line = self.lines.get_mut(&tab).ok_or_else(no_such_card)?;
        let at = line
            .parked
            .iter()
            .position(|parked| parked.id == card && !parked.is_past_its_limit(now))
            .ok_or_else(no_such_card)?;
        let parked = line.parked.remove(at);
        let answered = CardAnswer::sealed(
            AnswerSeal(()),
            decision,
            origin,
            parked.id,
            parked.call.id.clone(),
            wall_ms(),
            parked.binding,
        );
        line.queue.push_back(Queued {
            call: parked.call,
            event: StepEvent::Card(answered),
        });
        Ok(vec![BrokerOut::CardsChanged { tab }])
    }

    pub fn tick(&mut self, env: &dyn BrokerEnv, now: Instant) -> Vec<BrokerOut> {
        let mut out = Vec::new();
        for tab in self.lines.keys().copied().collect::<Vec<_>>() {
            let facts = env.facts(tab);
            let Some(facts) = facts.filter(|facts| self.line_is_of_process(tab, facts)) else {
                self.drop_line(tab, &mut out);
                continue;
            };
            let Some(line) = self.lines.get_mut(&tab) else {
                continue;
            };
            let mut round = Round {
                tab,
                facts: &facts,
                env,
                memory: &self.memory,
                ids: &mut self.ids,
                now,
                out: &mut out,
            };
            round.run(line);
        }
        out
    }

    /// Every open call of the tab answers `why`, every card goes: a tab leaving this window.
    pub fn withdraw_tab(&mut self, tab: TabId, why: &str) -> Vec<BrokerOut> {
        let out = self.take_calls(tab, |_| true, Some(why));
        self.lines.remove(&tab);
        self.memory.forget(tab.0);
        out
    }

    /// The tab's turn ended interrupted: every bridge call of the tab goes, queued, in flight or on a card. Codex
    /// sends no `notifications/cancelled` and never completes the item, so nothing else would end one, and it
    /// ignores the reply. Sidecar calls end by `cancel`, on their `ClientToolCallSettled`.
    pub fn turn_interrupted(&mut self, tab: TabId) -> Vec<BrokerOut> {
        self.take_calls(
            tab,
            |call| matches!(call.caller, Caller::Bridge { .. }),
            Some(words::INTERRUPTED),
        )
    }

    pub fn cards(&self, tab: TabId) -> Vec<EditorCardWire> {
        self.lines
            .get(&tab)
            .map_or_else(Vec::new, |line| line.parked.iter().map(Parked::wire).collect())
    }

    pub fn is_idle(&self) -> bool {
        self.lines.values().all(Line::holds_nothing)
    }

    /// Whether something here ends with no wake of its own: a chunk's reply raises none, and a queued call needs the
    /// next tick. A card needs neither: its answer comes as a page message, and [`Broker::next_card_limit`] names when
    /// it ends unanswered.
    pub fn needs_polling(&self) -> bool {
        self.lines
            .values()
            .any(|line| !line.queue.is_empty() || line.flight.is_some())
    }

    /// The soonest moment a waiting card ends unanswered.
    pub fn next_card_limit(&self) -> Option<Instant> {
        self.lines
            .values()
            .flat_map(|line| &line.parked)
            .map(|parked| parked.raised + CARD_LIMIT)
            .min()
    }

    /// Whether the tab's line, if it has one, belongs to `facts.process`. A line with no process yet takes this one:
    /// that is not a change of session.
    fn line_is_of_process(&mut self, tab: TabId, facts: &TabFacts) -> bool {
        let Some(line) = self.lines.get_mut(&tab) else {
            return true;
        };
        line.process.get_or_insert_with(|| facts.process.clone()) == &facts.process
    }

    /// The tab's session is gone: its calls are answered nowhere, and the page loses any card it still shows.
    fn drop_line(&mut self, tab: TabId, out: &mut Vec<BrokerOut>) {
        if let Some(line) = self.lines.remove(&tab) {
            self.memory.forget(tab.0);
            if !line.parked.is_empty() {
                out.push(BrokerOut::CardsChanged { tab });
            }
        }
    }

    /// Takes the calls `which` names, each answering `why` if there is one.
    fn take_calls(&mut self, tab: TabId, which: impl Fn(&OpenCall) -> bool, why: Option<&str>) -> Vec<BrokerOut> {
        let mut out = Vec::new();
        let Some(line) = self.lines.get_mut(&tab) else {
            return out;
        };
        let (taken, cards_went) = line.take_where(which);
        if let Some(why) = why {
            for call in taken {
                answer(&mut out, tab, call.caller, refusal(why));
            }
        }
        if cards_went {
            out.push(BrokerOut::CardsChanged { tab });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor_tools::cards::CardRequest;
    use crate::editor_tools::catalog::ToolRow;
    use crate::editor_tools::stamp::{Stamp, EDITOR_BUDGET};
    use crate::editor_tools::step::ANSWER_MAX_ENCODED;
    use crate::editor_tools::testing;
    use crate::editor_tools::way::EditorCall;
    use crate::nvim_rpc::Answer;
    use crate::turn_review::ProposedFile;
    use serde_json::json;
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::thread;
    use std::time::Duration;

    const TAB: TabId = TabId(1);
    const OTHER_TAB: TabId = TabId(2);
    const REVISION: u64 = 40;

    static SCRIPTED_ROW: ToolRow = ToolRow {
        name: "scripted",
        description: "",
        schema: "{}",
        always_load: true,
        default_on: true,
        in_untrusted: true,
    };

    /// Does what its `steps` argument says, one entry a step, and answers everything it heard.
    struct Scripted;
    static SCRIPTED: Scripted = Scripted;

    impl Tool for Scripted {
        fn row(&self) -> &'static ToolRow {
            &SCRIPTED_ROW
        }

        fn begin(&self, args: &serde_json::Value, cx: &ToolCx<'_>) -> Result<Box<dyn ToolRun>, ToolError> {
            if args.get("refuse").is_some() {
                return Err(ToolError(format!("begin refused for {:?}", cx.agent_call)));
            }
            let steps: Vec<String> = args["steps"]
                .as_array()
                .expect("a script")
                .iter()
                .map(|step| step.as_str().expect("a word").to_owned())
                .collect();
            Ok(Box::new(ScriptRun {
                steps: steps.into(),
                heard: Vec::new(),
            }))
        }
    }

    struct ScriptRun {
        steps: VecDeque<String>,
        heard: Vec<String>,
    }

    impl ScriptRun {
        fn of(steps: &[&str]) -> Box<ScriptRun> {
            Box::new(ScriptRun {
                steps: steps.iter().map(|step| (*step).to_owned()).collect(),
                heard: Vec::new(),
            })
        }
    }

    fn done(text: String) -> Step {
        Step::Done(ToolResult { text, is_error: false })
    }

    /// Whose chunk this is, from the caller the broker built the context with.
    fn who(cx: &ToolCx<'_>) -> String {
        match cx.agent_call {
            AgentCall::Claude(Some(id)) => id.to_owned(),
            AgentCall::Bridge { call_id: Some(id), .. } => id.to_owned(),
            AgentCall::Host => "host".to_owned(),
            other => format!("{other:?}"),
        }
    }

    impl ToolRun for ScriptRun {
        fn step(&mut self, event: StepEvent, cx: &ToolCx<'_>) -> Step {
            self.heard.push(match event {
                StepEvent::Start => "start".to_owned(),
                StepEvent::Answer(Ok(value)) => format!("value={value}"),
                StepEvent::Answer(Err(failure)) => format!("{failure:?}"),
                StepEvent::Poll => "poll".to_owned(),
                StepEvent::Card(answer) => format!(
                    "card allowed={} origin={:?} ms={} bind={} call={} id={}",
                    answer.allowed(),
                    answer.origin(),
                    answer.answered_ms(),
                    answer.binding().map_or(0, |binding| binding[0]),
                    answer.call_id(),
                    answer.card_id(),
                ),
            });
            let Some(word) = self.steps.pop_front() else {
                return done(self.heard.join(" "));
            };
            let (verb, number) = word.split_once(' ').unwrap_or((word.as_str(), ""));
            let number: u64 = number.parse().unwrap_or(0);
            match verb {
                "call" => Step::Call {
                    call: EditorCall::EditorState { root: who(cx).into() },
                    budget: if word == "call" {
                        EDITOR_BUDGET
                    } else {
                        Duration::from_millis(number)
                    },
                },
                "ask" => {
                    let mut request = CardRequest::new("Allow this?", vec![CardLine::Plain("what".to_owned())]);
                    if number > 0 {
                        request.binding = Some([number as u8; 32]);
                    }
                    Step::Ask(request)
                }
                "ask-proposal" => {
                    let file = ProposedFile {
                        rel: "src/a.rs".into(),
                        before: "blob".to_owned(),
                        after: "blob".to_owned(),
                        mode: 0o644,
                        hunks: String::new(),
                    };
                    let id = cx.memory.keep_proposal(cx.tab, "a change".to_owned(), vec![file]);
                    Step::Ask(CardRequest {
                        proposal: Some(id),
                        ..CardRequest::new("Apply this?", Vec::new())
                    })
                }
                "ask-gone-proposal" => Step::Ask(CardRequest {
                    proposal: Some("p-9".to_owned()),
                    ..CardRequest::new("Apply this?", Vec::new())
                }),
                "poll" => Step::PollAfter(Duration::from_millis(number)),
                "remember" => {
                    cx.memory.with(cx.tab, |_| ());
                    Step::PollAfter(Duration::from_secs(3600))
                }
                "who" => done(format!("{:?} session={}", cx.agent_call, cx.session)),
                "over" => done("x".repeat(ANSWER_MAX_ENCODED)),
                "fits" => done("x".repeat(ANSWER_MAX_ENCODED - 2)),
                other => panic!("no such step: {other}"),
            }
        }
    }

    struct Sent {
        who: String,
        session: String,
        stamp: Stamp,
        answer: Option<Answer>,
    }

    /// Holds the answer of every chunk, for the test to give.
    #[derive(Default)]
    struct FakeWay {
        sent: RefCell<Vec<Sent>>,
    }

    impl EditorWay for FakeWay {
        fn call(&self, session: &str, call: &EditorCall, stamp: &Stamp) -> Pending {
            let who = match call {
                EditorCall::EditorState { root } => root.display().to_string(),
                EditorCall::Baseline { .. } => "baseline".to_owned(),
                _ => panic!("the script sends two kinds of call"),
            };
            let (answer, pending) = Pending::pair();
            self.sent.borrow_mut().push(Sent {
                who,
                session: session.to_owned(),
                stamp: stamp.clone(),
                answer: Some(answer),
            });
            pending
        }

        fn reach(&self) -> EditorReach {
            EditorReach::Own
        }
    }

    struct FakeEnv {
        way: Option<FakeWay>,
        tabs: RefCell<BTreeMap<TabId, TabFacts>>,
        root: PathBuf,
    }

    fn facts(session: &str) -> TabFacts {
        TabFacts {
            mode: PermissionMode::Manual,
            trust: ProjectTrust::Trusted,
            session: session.to_owned(),
            process: format!("process of {session}"),
            revision: REVISION,
            declared: Rc::new(BTreeSet::from([
                "declared_nowhere".to_owned(),
                "editor_state".to_owned(),
            ])),
        }
    }

    impl FakeEnv {
        fn new() -> FakeEnv {
            FakeEnv {
                way: Some(FakeWay::default()),
                tabs: RefCell::new(BTreeMap::from([(TAB, facts("s1")), (OTHER_TAB, facts("s2"))])),
                root: PathBuf::from("/p"),
            }
        }

        fn without_a_way() -> FakeEnv {
            FakeEnv {
                way: None,
                ..FakeEnv::new()
            }
        }

        fn sent(&self) -> std::cell::RefMut<'_, Vec<Sent>> {
            self.way.as_ref().expect("a way").sent.borrow_mut()
        }

        fn sent_by(&self) -> Vec<String> {
            self.sent().iter().map(|sent| sent.who.clone()).collect()
        }

        fn stamps(&self) -> Vec<Stamp> {
            self.sent().iter().map(|sent| sent.stamp.clone()).collect()
        }

        /// Answers the `index`th chunk ever sent.
        fn reply(&self, index: usize, reply: Result<Value, RpcError>) {
            self.sent()[index].answer.take().expect("not answered yet").send(reply);
        }

        fn ok(&self, index: usize) {
            self.reply(index, Ok(Value::from("ok")));
        }
    }

    impl BrokerEnv for FakeEnv {
        fn way(&self) -> Option<&dyn EditorWay> {
            self.way.as_ref().map(|way| way as &dyn EditorWay)
        }

        fn facts(&self, tab: TabId) -> Option<TabFacts> {
            self.tabs.borrow().get(&tab).cloned()
        }

        fn root(&self) -> &Path {
            &self.root
        }

        fn home(&self) -> Option<&Path> {
            None
        }

        fn context(&self) -> Option<EditorContext> {
            None
        }
    }

    fn sidecar(name: &str) -> Caller {
        Caller::Sidecar {
            call_id: name.to_owned(),
        }
    }

    fn bridge(call: u64) -> Caller {
        Caller::Bridge {
            id: BridgeCallId {
                tab: TAB.0,
                connection: 1,
                call,
            },
        }
    }

    /// Submits the test tool; the call's `tool_use_id` is the caller's own name, which is how the way tells
    /// chunks apart.
    fn submit(
        broker: &mut Broker,
        env: &FakeEnv,
        tab: TabId,
        caller: Caller,
        steps: &[&str],
        now: Instant,
    ) -> Vec<BrokerOut> {
        let who = match &caller {
            Caller::Sidecar { call_id } => call_id.clone(),
            Caller::Bridge { id } => format!("b{}", id.call),
            Caller::Host => "host".to_owned(),
        };
        let arguments = json!({ "steps": steps }).to_string();
        broker.submit_tool(tab, caller, Some(who), &SCRIPTED, &arguments, env, now)
    }

    fn answers(out: &[BrokerOut]) -> Vec<(Caller, ToolResult)> {
        out.iter()
            .filter_map(|out| match out {
                BrokerOut::Answer { caller, result, .. } => Some((caller.clone(), result.clone())),
                BrokerOut::CardsChanged { .. } => None,
            })
            .collect()
    }

    fn only_answer(out: &[BrokerOut]) -> ToolResult {
        let mut found = answers(out);
        assert_eq!(found.len(), 1, "{out:?}");
        found.remove(0).1
    }

    fn changed(out: &[BrokerOut], tab: TabId) -> bool {
        out.contains(&BrokerOut::CardsChanged { tab })
    }

    fn refused(text: &str) -> ToolResult {
        ToolResult {
            text: text.to_owned(),
            is_error: true,
        }
    }

    fn after(from: Instant, ms: u64) -> Instant {
        from + Duration::from_millis(ms)
    }

    /// The value `key=` holds in a run's answer.
    fn heard<'a>(text: &'a str, key: &str) -> &'a str {
        let at = text.find(key).unwrap_or_else(|| panic!("{key} in {text}")) + key.len();
        text[at..].split(' ').next().unwrap()
    }

    #[test]
    fn one_call_is_in_flight_per_tab_and_the_next_waits() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("a"), &["call"], t0);
        submit(&mut broker, &env, TAB, sidecar("b"), &["call"], t0);
        assert!(broker.tick(&env, t0).is_empty());
        assert!(broker.tick(&env, after(t0, 10)).is_empty());
        assert_eq!(env.sent_by(), ["a"], "the second waits for the first");

        env.ok(0);
        let out = broker.tick(&env, after(t0, 20));
        assert_eq!(answers(&out)[0].0, sidecar("a"));
        assert_eq!(only_answer(&out).text, "start value=\"ok\"");
        assert_eq!(env.sent_by(), ["a", "b"], "the same tick admits the next");
    }

    #[test]
    fn two_tabs_run_at_once() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("a"), &["call"], t0);
        submit(&mut broker, &env, OTHER_TAB, sidecar("b"), &["call"], t0);
        broker.tick(&env, t0);
        assert_eq!(env.sent_by(), ["a", "b"]);
    }

    #[test]
    fn calls_that_end_at_once_all_end_in_one_tick() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("a"), &[], t0);
        submit(&mut broker, &env, TAB, sidecar("b"), &[], t0);
        assert_eq!(answers(&broker.tick(&env, t0)).len(), 2);
        assert!(broker.is_idle());
    }

    #[test]
    fn the_seventeenth_waiting_call_is_refused() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("flying"), &["call"], t0);
        broker.tick(&env, t0);
        for n in 0..16 {
            let out = submit(&mut broker, &env, TAB, sidecar(&format!("q{n}")), &[], t0);
            assert!(out.is_empty(), "{n}: {out:?}");
        }
        let out = submit(&mut broker, &env, TAB, sidecar("late"), &[], t0);
        assert_eq!(answers(&out), [(sidecar("late"), refused(words::TOO_MANY))]);
    }

    #[test]
    fn the_seventeenth_parked_call_is_refused() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        for n in 0..16 {
            submit(&mut broker, &env, TAB, sidecar(&format!("p{n}")), &["ask"], t0);
            broker.tick(&env, t0);
        }
        assert_eq!(broker.cards(TAB).len(), 16);
        let out = submit(&mut broker, &env, TAB, sidecar("late"), &["ask"], t0);
        assert_eq!(only_answer(&out), refused(words::TOO_MANY));
    }

    #[test]
    fn a_card_past_the_parked_limit_ends_the_call_that_asked() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        for n in 0..15 {
            submit(&mut broker, &env, TAB, sidecar(&format!("p{n}")), &["ask"], t0);
            broker.tick(&env, t0);
        }
        submit(&mut broker, &env, TAB, sidecar("takes the last place"), &["ask"], t0);
        submit(&mut broker, &env, TAB, sidecar("finds none"), &["ask"], t0);
        let out = broker.tick(&env, t0);
        assert_eq!(
            broker.cards(TAB).len(),
            16,
            "no more than 16 cards, whatever was queued"
        );
        assert_eq!(answers(&out), [(sidecar("finds none"), refused(words::TOO_MANY))]);
    }

    #[test]
    fn a_card_past_its_limit_makes_room_for_what_the_same_tick_hears() {
        for (steps, the_editor_answers) in [(["poll 100", "ask"], false), (["call", "ask"], true)] {
            let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
            for n in 0..15 {
                submit(&mut broker, &env, TAB, sidecar(&format!("p{n}")), &["ask"], t0);
                broker.tick(&env, t0);
            }
            let near = t0 + CARD_LIMIT - Duration::from_millis(50);
            submit(&mut broker, &env, TAB, sidecar("fills the cards"), &["ask"], near);
            submit(&mut broker, &env, TAB, sidecar("asks after"), &steps, near);
            broker.tick(&env, near);
            assert_eq!(broker.cards(TAB).len(), 16, "{steps:?}");

            if the_editor_answers {
                env.ok(0);
            }
            let out = broker.tick(&env, near + Duration::from_millis(100));
            let ended = answers(&out);
            assert_eq!(ended.len(), 15, "{steps:?}: only the old cards end: {ended:?}");
            assert!(ended.iter().all(|(_, result)| *result == refused(words::NOT_ANSWERED)));
            assert_eq!(broker.cards(TAB).len(), 2, "{steps:?}");
        }
    }

    #[test]
    fn a_call_past_its_budget_answers_and_a_late_answer_is_dropped() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("a"), &["call"], t0);
        broker.tick(&env, t0);
        let budget = EDITOR_BUDGET.as_millis() as u64;
        assert!(broker.tick(&env, after(t0, budget - 1)).is_empty());

        let out = broker.tick(&env, after(t0, budget));
        assert_eq!(only_answer(&out).text, "start NotAnswered");

        env.ok(0);
        assert!(broker.tick(&env, after(t0, budget + 10)).is_empty());
        assert!(broker.is_idle());
    }

    #[test]
    fn an_expired_answer_is_a_failure_not_a_value() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("a"), &["call"], t0);
        broker.tick(&env, t0);
        env.reply(0, Ok(Value::Map(vec![(Value::from("expired"), Value::from(true))])));
        assert_eq!(only_answer(&broker.tick(&env, t0)).text, "start NotAnswered");

        submit(&mut broker, &env, TAB, sidecar("b"), &["call"], t0);
        broker.tick(&env, t0);
        env.reply(1, Ok(Value::Map(vec![(Value::from("expired"), Value::from(false))])));
        let out = broker.tick(&env, t0);
        assert!(only_answer(&out).text.starts_with("start value="), "{out:?}");
    }

    #[test]
    fn a_failed_call_is_told_apart_by_what_failed() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("a"), &["call"], t0);
        broker.tick(&env, t0);
        env.reply(0, Err(RpcError::Unavailable("nothing there".to_owned())));
        assert_eq!(
            only_answer(&broker.tick(&env, t0)).text,
            "start Unavailable(\"nothing there\")"
        );

        submit(&mut broker, &env, TAB, sidecar("b"), &["call"], t0);
        broker.tick(&env, t0);
        env.reply(1, Err(RpcError::Nvim("E5108".to_owned())));
        assert_eq!(only_answer(&broker.tick(&env, t0)).text, "start Refused(\"E5108\")");

        submit(&mut broker, &env, TAB, sidecar("c"), &["call"], t0);
        broker.tick(&env, t0);
        env.sent()[2].answer = None;
        assert_eq!(
            only_answer(&broker.tick(&env, t0)).text,
            "start Refused(\"the connection to nvim closed\")"
        );
    }

    #[test]
    fn a_stamp_counts_up_per_tab() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("a"), &["call", "call"], t0);
        submit(&mut broker, &env, OTHER_TAB, sidecar("b"), &["call"], t0);
        broker.tick(&env, t0);
        env.ok(0);
        broker.tick(&env, t0);
        let stamps = env.stamps();
        assert_eq!(stamps.iter().map(|stamp| stamp.seq).collect::<Vec<_>>(), [1, 1, 2]);
        assert_eq!(stamps[0].scope, stamps[2].scope, "one tab, one count");
        assert_ne!(stamps[0].scope, stamps[1].scope);
        assert!(
            stamps[0].clone().checked(0).is_ok(),
            "window 1 takes this scope's shape"
        );
    }

    #[test]
    fn the_stamp_gives_up_before_the_broker_does() {
        let before = wall_ms();
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("a"), &["call"], t0);
        broker.tick(&env, t0);
        let wall_after = wall_ms();

        let stamped = (EDITOR_BUDGET - STAMP_MARGIN).as_millis() as u64;
        let deadline = env.stamps()[0].deadline_ms;
        // Two whole-millisecond roundings sit between `t0` and the deadline.
        assert!(
            deadline + 2 >= before + stamped && deadline <= wall_after + stamped,
            "{deadline}"
        );
        assert!(
            broker.tick(&env, t0 + EDITOR_BUDGET - STAMP_MARGIN).is_empty(),
            "the broker still waits when the chunk has stopped running"
        );
        assert_eq!(answers(&broker.tick(&env, t0 + EDITOR_BUDGET)).len(), 1);
    }

    #[test]
    fn a_budget_at_or_under_the_margin_answers_at_once_with_no_chunk_sent() {
        let margin = STAMP_MARGIN.as_millis() as u64;
        for budget in [0, 1, margin] {
            let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
            submit(&mut broker, &env, TAB, sidecar("a"), &[&format!("call {budget}")], t0);
            let out = broker.tick(&env, t0);
            assert_eq!(only_answer(&out).text, "start NotAnswered", "{budget}");
            assert!(env.sent_by().is_empty(), "{budget}");
        }
    }

    #[test]
    fn a_budget_just_over_the_margin_sends_its_chunk() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        let budget = STAMP_MARGIN.as_millis() as u64 + 1;
        submit(&mut broker, &env, TAB, sidecar("a"), &[&format!("call {budget}")], t0);
        let before = wall_ms();
        // A `now` still ahead of the clock: nothing of this one millisecond has gone by the time it is stamped.
        broker.tick(&env, after(t0, 60_000));
        assert_eq!(env.sent_by(), ["a"]);
        assert!(env.stamps()[0].deadline_ms > before);
        assert!(env.stamps()[0].deadline_ms <= wall_ms() + 1);
    }

    #[test]
    fn a_tick_that_runs_late_stamps_from_the_moment_the_broker_counts_from() {
        let late = Duration::from_secs(1);
        let (mut broker, env, now) = (Broker::new(), FakeEnv::new(), Instant::now() - late);
        submit(&mut broker, &env, TAB, sidecar("a"), &["call"], now);
        broker.tick(&env, now);
        let stamped = (EDITOR_BUDGET - STAMP_MARGIN - late).as_millis() as u64;
        assert!(
            env.stamps()[0].deadline_ms <= wall_ms() + stamped,
            "a chunk stamped late still has to stop before the broker gives up at `now` plus the budget"
        );
    }

    #[test]
    fn a_tick_later_than_the_budget_sends_no_chunk() {
        let (mut broker, env, now) = (Broker::new(), FakeEnv::new(), Instant::now() - EDITOR_BUDGET);
        submit(&mut broker, &env, TAB, sidecar("a"), &["call"], now);
        assert_eq!(only_answer(&broker.tick(&env, now)).text, "start NotAnswered");
        assert!(env.sent_by().is_empty());
    }

    #[test]
    fn a_poll_keeps_the_slot_and_comes_back_when_due() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("a"), &["poll 100"], t0);
        submit(&mut broker, &env, TAB, sidecar("b"), &["call"], t0);
        assert!(broker.tick(&env, t0).is_empty());
        assert!(broker.tick(&env, after(t0, 99)).is_empty());
        assert!(env.sent_by().is_empty(), "the poll holds the tab's flight");
        assert_eq!(only_answer(&broker.tick(&env, after(t0, 100))).text, "start poll");
        assert_eq!(env.sent_by(), ["b"]);
    }

    #[test]
    fn a_call_on_a_card_is_not_in_flight() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("a"), &["ask"], t0);
        submit(&mut broker, &env, TAB, sidecar("b"), &["call"], t0);
        let out = broker.tick(&env, t0);
        assert!(changed(&out, TAB), "{out:?}");
        assert_eq!(broker.cards(TAB).len(), 1);
        assert_eq!(env.sent_by(), ["b"]);
        assert!(!broker.is_idle());
    }

    #[test]
    fn an_answered_card_queues_behind_what_waits() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("a"), &["ask", "call"], t0);
        submit(&mut broker, &env, TAB, sidecar("b"), &["call"], t0);
        submit(&mut broker, &env, TAB, sidecar("c"), &["call"], t0);
        broker.tick(&env, t0);
        let id = broker.cards(TAB)[0].id.clone();
        let out = broker
            .answer_card(TAB, &id, CardDecision::Allow, CardOrigin::Key, t0)
            .unwrap();
        assert_eq!(out, [BrokerOut::CardsChanged { tab: TAB }]);
        assert!(broker.cards(TAB).is_empty());

        env.ok(0);
        broker.tick(&env, t0);
        env.ok(1);
        broker.tick(&env, t0);
        assert_eq!(env.sent_by(), ["b", "c", "a"]);
    }

    #[test]
    fn a_card_wire_names_its_call() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("a"), &["ask"], t0);
        broker.tick(&env, t0);
        let card = broker.cards(TAB).remove(0);
        assert_eq!(card.tool, "scripted");
        assert_eq!(card.tool_use_id.as_deref(), Some("a"));
        assert_eq!(card.title, "Allow this?");
        assert_eq!(card.body, [CardLine::Plain("what".to_owned())]);
        assert!(broker.cards(OTHER_TAB).is_empty());
    }

    #[test]
    fn a_card_of_a_proposal_names_its_files_on_the_wire() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("a"), &["ask-proposal"], t0);
        broker.tick(&env, t0);
        let card = broker.cards(TAB).remove(0);
        assert_eq!(
            card.proposal,
            Some(CardProposal {
                id: "p-1".to_owned(),
                files: vec!["src/a.rs".to_owned()],
            })
        );
    }

    #[test]
    fn a_card_names_no_proposal_the_tab_does_not_hold() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("a"), &["ask-gone-proposal"], t0);
        submit(&mut broker, &env, TAB, sidecar("b"), &["ask"], t0);
        broker.tick(&env, t0);
        broker.tick(&env, t0);
        assert!(broker.cards(TAB).iter().all(|card| card.proposal.is_none()));
    }

    #[test]
    fn a_card_takes_the_tabs_revision_as_its_seq() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("a"), &["ask"], t0);
        env.tabs.borrow_mut().get_mut(&TAB).unwrap().revision = 77;
        broker.tick(&env, t0);
        assert_eq!(broker.cards(TAB)[0].seq, 77);
    }

    #[test]
    fn a_card_past_ten_minutes_answers_and_goes() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("a"), &["ask", "call"], t0);
        broker.tick(&env, t0);
        assert!(broker.tick(&env, t0 + CARD_LIMIT - Duration::from_millis(1)).is_empty());
        assert_eq!(broker.cards(TAB).len(), 1);

        let out = broker.tick(&env, t0 + CARD_LIMIT);
        assert_eq!(only_answer(&out), refused(words::NOT_ANSWERED), "the tool is not asked");
        assert!(changed(&out, TAB));
        assert!(broker.cards(TAB).is_empty());
        assert!(env.sent_by().is_empty());
        assert!(broker.is_idle());
    }

    #[test]
    fn a_card_past_its_limit_takes_no_answer() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("a"), &["ask", "call"], t0);
        broker.tick(&env, t0);
        let id = broker.cards(TAB)[0].id.clone();
        assert!(broker
            .answer_card(TAB, &id, CardDecision::Allow, CardOrigin::Key, t0 + CARD_LIMIT)
            .is_err());
        let out = broker.tick(&env, t0 + CARD_LIMIT);
        assert_eq!(only_answer(&out), refused(words::NOT_ANSWERED));
        assert!(env.sent_by().is_empty());
    }

    #[test]
    fn a_cancel_takes_the_card_and_answers_nothing() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("a"), &["ask", "call"], t0);
        broker.tick(&env, t0);
        let id = broker.cards(TAB)[0].id.clone();

        let out = broker.cancel(TAB, &sidecar("a"));
        assert_eq!(out, [BrokerOut::CardsChanged { tab: TAB }]);
        assert!(broker.cards(TAB).is_empty());
        assert!(broker.is_idle());
        assert!(broker
            .answer_card(TAB, &id, CardDecision::Allow, CardOrigin::Key, t0)
            .is_err());
        assert!(broker.tick(&env, t0).is_empty());
        assert!(env.sent_by().is_empty());
    }

    #[test]
    fn a_cancel_takes_a_call_wherever_it_is() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("carded"), &["ask", "call"], t0);
        submit(&mut broker, &env, TAB, sidecar("flying"), &["call"], t0);
        broker.tick(&env, t0);
        submit(&mut broker, &env, TAB, sidecar("queued"), &["call"], t0);

        assert!(broker.cancel(TAB, &sidecar("queued")).is_empty());
        let id = broker.cards(TAB)[0].id.clone();
        broker
            .answer_card(TAB, &id, CardDecision::Allow, CardOrigin::Key, t0)
            .unwrap();
        assert!(
            broker.cancel(TAB, &sidecar("carded")).is_empty(),
            "an answered card is already gone"
        );
        assert!(broker.cancel(TAB, &sidecar("flying")).is_empty());
        assert!(broker.is_idle());

        env.ok(0);
        assert!(broker.tick(&env, t0).is_empty());
        assert_eq!(env.sent_by(), ["flying"], "neither of the others ran");
    }

    #[test]
    fn a_cancel_from_nobody_takes_no_host_run() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        broker.submit_run(TAB, ScriptRun::of(&["call"]), t0);
        assert!(broker.cancel(TAB, &Caller::Host).is_empty());
        broker.tick(&env, t0);
        assert_eq!(env.sent_by(), ["host"]);
    }

    #[test]
    fn withdrawing_a_tab_answers_each_call_once() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("carded"), &["ask"], t0);
        submit(&mut broker, &env, TAB, sidecar("flying"), &["call"], t0);
        broker.tick(&env, t0);
        submit(&mut broker, &env, TAB, sidecar("queued"), &["call"], t0);

        let out = broker.withdraw_tab(TAB, words::MOVED);
        let mut who: Vec<Caller> = answers(&out).into_iter().map(|(caller, _)| caller).collect();
        who.sort_by_key(|caller| format!("{caller:?}"));
        assert_eq!(who, [sidecar("carded"), sidecar("flying"), sidecar("queued")]);
        assert!(answers(&out).iter().all(|(_, result)| *result == refused(words::MOVED)));
        assert!(changed(&out, TAB));
        assert!(broker.cards(TAB).is_empty());
        assert!(broker.is_idle());

        env.ok(0);
        assert!(broker.tick(&env, t0).is_empty());
        assert!(broker.withdraw_tab(TAB, words::MOVED).is_empty());
    }

    #[test]
    fn a_tab_gone_leaves_nothing_and_answers_nothing() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("carded"), &["ask"], t0);
        submit(&mut broker, &env, TAB, sidecar("flying"), &["call"], t0);
        broker.tick(&env, t0);
        submit(&mut broker, &env, TAB, sidecar("queued"), &["call"], t0);
        env.tabs.borrow_mut().remove(&TAB);

        let out = broker.tick(&env, t0);
        assert!(answers(&out).is_empty(), "{out:?}");
        assert!(broker.cards(TAB).is_empty());
        assert!(broker.is_idle());
        env.ok(0);
        assert!(broker.tick(&env, t0).is_empty());
    }

    #[test]
    fn a_new_session_in_a_tab_starts_clean() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("carded"), &["ask"], t0);
        submit(&mut broker, &env, TAB, sidecar("a"), &["call", "call", "call"], t0);
        broker.tick(&env, t0);
        env.ok(0);
        broker.tick(&env, t0);
        env.tabs.borrow_mut().insert(TAB, facts("s9"));

        let out = broker.tick(&env, t0);
        assert!(answers(&out).is_empty(), "{out:?}");
        assert!(changed(&out, TAB), "the page still shows the old session's card");
        assert!(broker.cards(TAB).is_empty());

        submit(&mut broker, &env, TAB, sidecar("next"), &["call"], t0);
        broker.tick(&env, t0);
        let sent = env.sent();
        let last = sent.last().unwrap();
        assert_eq!((last.who.as_str(), last.session.as_str()), ("next", "s9"));
    }

    #[test]
    fn a_new_conversation_in_the_same_process_keeps_the_line() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("carded"), &["ask"], t0);
        submit(&mut broker, &env, TAB, sidecar("a"), &["call", "call", "call"], t0);
        broker.tick(&env, t0);
        env.ok(0);
        broker.tick(&env, t0);
        env.tabs.borrow_mut().get_mut(&TAB).unwrap().session = "s9".to_owned();

        let out = broker.tick(&env, t0);
        assert!(answers(&out).is_empty(), "{out:?}");
        assert!(!changed(&out, TAB), "the agent is still waiting on its card");
        assert_eq!(broker.cards(TAB).len(), 1);

        env.ok(1);
        broker.tick(&env, t0);
        let sent = env.sent();
        let last = sent.last().unwrap();
        assert_eq!((last.who.as_str(), last.session.as_str()), ("a", "s9"));
    }

    #[test]
    fn a_new_line_in_a_tab_is_not_older_than_what_ran_before_it() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("a"), &["call", "call"], t0);
        broker.tick(&env, t0);
        env.ok(0);
        broker.tick(&env, t0);

        env.tabs.borrow_mut().insert(TAB, facts("s9"));
        broker.tick(&env, t0);
        submit(&mut broker, &env, TAB, sidecar("after a new session"), &["call"], t0);
        broker.tick(&env, t0);

        broker.withdraw_tab(TAB, words::MOVED);
        submit(&mut broker, &env, TAB, sidecar("after coming back"), &["call"], t0);
        broker.tick(&env, t0);

        // The chunk's own rule: a number at or below one already run in its scope does nothing.
        let mut last_run: BTreeMap<String, u64> = BTreeMap::new();
        for stamp in env.stamps() {
            let last = last_run.insert(stamp.scope.clone(), stamp.seq).unwrap_or(0);
            assert!(stamp.seq > last, "{stamp:?} would be refused as older");
        }
    }

    #[test]
    fn an_interrupted_turn_takes_every_bridge_call_and_leaves_sidecar_calls() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, bridge(2), &["ask"], t0);
        submit(&mut broker, &env, TAB, sidecar("carded"), &["ask"], t0);
        submit(&mut broker, &env, TAB, bridge(1), &["call"], t0);
        broker.tick(&env, t0);
        submit(&mut broker, &env, TAB, bridge(3), &["call"], t0);
        submit(&mut broker, &env, TAB, sidecar("queued"), &["call"], t0);
        submit(&mut broker, &env, OTHER_TAB, bridge(4), &["ask"], t0);
        broker.tick(&env, t0);

        let out = broker.turn_interrupted(TAB);
        let mut who: Vec<Caller> = answers(&out).into_iter().map(|(caller, _)| caller).collect();
        who.sort_by_key(|caller| format!("{caller:?}"));
        assert_eq!(who, [bridge(1), bridge(2), bridge(3)]);
        assert!(answers(&out)
            .iter()
            .all(|(_, result)| *result == refused(words::INTERRUPTED)));
        assert!(changed(&out, TAB));
        assert_eq!(broker.cards(TAB).len(), 1, "the sidecar's card stays");
        assert_eq!(broker.cards(OTHER_TAB).len(), 1, "another tab's turn did not end");

        broker.tick(&env, t0);
        assert_eq!(
            env.sent_by(),
            ["b1", "queued"],
            "the sidecar's call goes on, the bridge's do not"
        );
    }

    #[test]
    fn a_call_in_flight_dropped_by_an_interrupt_ignores_its_late_answer() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, bridge(1), &["call", "call"], t0);
        broker.tick(&env, t0);
        assert_eq!(answers(&broker.turn_interrupted(TAB)).len(), 1);

        env.ok(0);
        assert!(
            broker.tick(&env, t0).is_empty(),
            "no second answer, and the run does not go on"
        );
        assert_eq!(env.sent_by(), ["b1"]);
    }

    #[test]
    fn a_card_id_is_new_for_each_raised_card_and_never_a_call_id() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("a"), &["ask", "ask"], t0);
        broker.tick(&env, t0);
        let first = broker.cards(TAB)[0].id.clone();
        broker
            .answer_card(TAB, &first, CardDecision::Allow, CardOrigin::Key, t0)
            .unwrap();
        broker.tick(&env, t0);
        let second = broker.cards(TAB)[0].id.clone();
        assert_ne!(first, second, "the same call asking again raises a new card");
        assert!(broker
            .answer_card(TAB, &first, CardDecision::Allow, CardOrigin::Key, t0)
            .is_err());
        broker
            .answer_card(TAB, &second, CardDecision::Allow, CardOrigin::Key, t0)
            .unwrap();
        let out = broker.tick(&env, t0);

        let text = only_answer(&out).text;
        let call = heard(&text, "call=").to_owned();
        assert!(text.ends_with(&format!("id={second}")), "{text}");
        let parts: Vec<&str> = first.split('-').collect();
        assert_eq!(parts.len(), 3, "{first}");
        assert_eq!(parts[0], "ec");
        assert_eq!(parts[1].len(), 16);
        assert!(parts[1]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));
        assert!(!call.starts_with("ec-"), "{call}");
        assert!(call.starts_with(parts[1]), "{call}");
    }

    #[test]
    fn two_brokers_never_share_a_card_id() {
        let (mut one, mut two, env, t0) = (Broker::new(), Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut one, &env, TAB, sidecar("a"), &["ask"], t0);
        submit(&mut two, &env, TAB, sidecar("a"), &["ask"], t0);
        one.tick(&env, t0);
        two.tick(&env, t0);
        let (id_one, id_two) = (one.cards(TAB)[0].id.clone(), two.cards(TAB)[0].id.clone());
        assert_ne!(id_one, id_two);
        assert!(two
            .answer_card(TAB, &id_one, CardDecision::Allow, CardOrigin::Key, t0)
            .is_err());
    }

    #[test]
    fn a_card_is_answered_only_once() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("a"), &["ask"], t0);
        broker.tick(&env, t0);
        let id = broker.cards(TAB)[0].id.clone();
        broker
            .answer_card(TAB, &id, CardDecision::Deny, CardOrigin::Key, t0)
            .unwrap();
        assert!(broker
            .answer_card(TAB, &id, CardDecision::Allow, CardOrigin::Key, t0)
            .is_err());
        let text = only_answer(&broker.tick(&env, t0)).text;
        assert_eq!(heard(&text, "allowed="), "false");
    }

    #[test]
    fn a_card_is_answered_only_in_its_own_tab() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("a"), &["ask"], t0);
        submit(&mut broker, &env, OTHER_TAB, sidecar("b"), &[], t0);
        broker.tick(&env, t0);
        let id = broker.cards(TAB)[0].id.clone();
        assert!(broker
            .answer_card(OTHER_TAB, &id, CardDecision::Allow, CardOrigin::Key, t0)
            .is_err());
        assert!(broker
            .answer_card(TAB, "ec-nothing-1", CardDecision::Allow, CardOrigin::Key, t0)
            .is_err());
        assert_eq!(broker.cards(TAB).len(), 1);
    }

    #[test]
    fn the_answer_says_how_the_person_answered() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("a"), &["ask"], t0);
        broker.tick(&env, t0);
        let id = broker.cards(TAB)[0].id.clone();
        broker
            .answer_card(TAB, &id, CardDecision::Allow, CardOrigin::Click, t0)
            .unwrap();
        let text = only_answer(&broker.tick(&env, t0)).text;
        assert_eq!(heard(&text, "allowed="), "true");
        assert_eq!(heard(&text, "origin="), "Click");
    }

    #[test]
    fn the_answer_carries_the_parked_cards_binding() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("a"), &["ask 7"], t0);
        submit(&mut broker, &env, TAB, sidecar("b"), &["ask 9"], t0);
        broker.tick(&env, t0);
        let (a, b) = (broker.cards(TAB)[0].id.clone(), broker.cards(TAB)[1].id.clone());
        let wire = serde_json::to_string(&broker.cards(TAB)).unwrap();
        assert!(!wire.contains("binding"), "{wire}");

        broker
            .answer_card(TAB, &b, CardDecision::Allow, CardOrigin::Key, t0)
            .unwrap();
        broker
            .answer_card(TAB, &a, CardDecision::Allow, CardOrigin::Key, t0)
            .unwrap();
        let out = broker.tick(&env, t0);
        let found = answers(&out);
        assert_eq!(found.len(), 2);
        for (caller, result) in found {
            let bound = if caller == sidecar("a") { "7" } else { "9" };
            assert_eq!(heard(&result.text, "bind="), bound, "{caller:?}");
        }
    }

    #[test]
    fn answered_ms_is_when_the_person_answered_not_when_the_run_resumed() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("a"), &["ask"], t0);
        submit(&mut broker, &env, TAB, sidecar("flying"), &["call"], t0);
        broker.tick(&env, t0);
        let id = broker.cards(TAB)[0].id.clone();

        let before = wall_ms();
        broker
            .answer_card(TAB, &id, CardDecision::Allow, CardOrigin::Key, t0)
            .unwrap();
        let answered = wall_ms();
        thread::sleep(Duration::from_millis(20));
        broker.tick(&env, t0);
        env.ok(0);
        let out = broker.tick(&env, t0);

        let found = answers(&out);
        let (_, result) = found.iter().find(|(caller, _)| *caller == sidecar("a")).unwrap();
        let ms: u64 = heard(&result.text, "ms=").parse().unwrap();
        assert!(ms >= before && ms <= answered, "{before} {ms} {answered}");
    }

    #[test]
    fn an_approved_card_runs_though_nobody_reads_the_answer() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, bridge(1), &["ask", "call"], t0);
        broker.tick(&env, t0);
        let id = broker.cards(TAB)[0].id.clone();
        broker
            .answer_card(TAB, &id, CardDecision::Allow, CardOrigin::Key, t0)
            .unwrap();

        // An agent that gave up without saying so: its answer goes out and nothing takes it.
        broker.tick(&env, t0);
        assert_eq!(env.sent_by(), ["b1"]);
        env.ok(0);
        drop(broker.tick(&env, t0));
        assert!(broker.is_idle());
    }

    #[test]
    fn an_oversized_answer_becomes_an_error_sentence() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("a"), &["over"], t0);
        assert_eq!(only_answer(&broker.tick(&env, t0)), refused(words::TOO_LARGE));

        submit(&mut broker, &env, TAB, sidecar("b"), &["fits"], t0);
        let fits = only_answer(&broker.tick(&env, t0));
        assert_eq!((fits.text.len(), fits.is_error), (ANSWER_MAX_ENCODED - 2, false));
    }

    #[test]
    fn a_refusal_that_echoes_its_input_still_fits() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        let name = "x".repeat(ANSWER_MAX_ENCODED);
        let out = broker.submit(TAB, sidecar("a"), None, &name, "{}", &env, t0);
        assert_eq!(only_answer(&out), refused(words::TOO_LARGE));
    }

    #[test]
    fn a_tool_not_declared_for_the_session_is_refused_before_begin() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        for name in ["open_file", "nvim_nope"] {
            let out = broker.submit(TAB, sidecar("a"), None, name, "not json", &env, t0);
            assert_eq!(
                only_answer(&out),
                refused(&format!("{name} is not available in this session")),
                "{name}: not the argument error, not the lookup's, not the tool's own"
            );
        }
        assert!(broker.is_idle());
    }

    #[test]
    fn a_tab_with_no_session_has_no_tool() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        let out = broker.submit(TabId(9), sidecar("a"), None, "editor_state", "{}", &env, t0);
        assert_eq!(
            only_answer(&out),
            refused("editor_state is not available in this session")
        );
    }

    #[test]
    fn a_declared_name_no_tool_has_answers_an_error() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        let out = broker.submit(TAB, sidecar("a"), None, "declared_nowhere", "{}", &env, t0);
        assert_eq!(only_answer(&out), refused("unknown tool: declared_nowhere"));
    }

    #[test]
    fn arguments_that_are_not_an_object_answer_an_error() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        for arguments in ["", "[]", "3", "null", "{"] {
            let out = broker.submit_tool(TAB, sidecar("a"), None, &SCRIPTED, arguments, &env, t0);
            assert_eq!(
                only_answer(&out),
                refused("the arguments must be an object"),
                "{arguments:?}"
            );
        }
        assert!(broker.is_idle());
    }

    #[test]
    fn a_refused_begin_answers_at_once() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        let out = broker.submit_tool(
            TAB,
            bridge(1),
            Some("c1".to_owned()),
            &SCRIPTED,
            r#"{"refuse":true}"#,
            &env,
            t0,
        );
        let text = only_answer(&out).text;
        assert!(text.starts_with("begin refused for Bridge"), "{text}");
        assert!(text.contains("c1"), "begin sees the agent's call: {text}");
        assert!(broker.is_idle());
    }

    #[test]
    fn a_run_is_told_which_call_it_is() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("toolu_1"), &["who"], t0);
        let text = only_answer(&broker.tick(&env, t0)).text;
        assert_eq!(text, "Claude(Some(\"toolu_1\")) session=s1");

        let bridged = broker.submit_tool(TAB, bridge(5), None, &SCRIPTED, r#"{"steps":["who"]}"#, &env, t0);
        assert!(bridged.is_empty());
        let text = only_answer(&broker.tick(&env, t0)).text;
        assert!(text.starts_with("Bridge { call_id: None"), "{text}");
    }

    #[test]
    fn a_submit_for_a_new_session_drops_the_old_line_first() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("old"), &["ask"], t0);
        broker.tick(&env, t0);
        env.tabs.borrow_mut().insert(TAB, facts("s9"));

        let out = submit(&mut broker, &env, TAB, sidecar("new"), &["call"], t0);
        assert!(answers(&out).is_empty(), "{out:?}");
        assert!(changed(&out, TAB));
        assert!(broker.cards(TAB).is_empty());
        broker.tick(&env, t0);
        assert_eq!(env.sent_by(), ["new"]);
    }

    #[test]
    fn a_host_run_keeps_a_tab_with_no_line_and_takes_its_session_at_the_next_tick() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        broker.submit_run(TAB, ScriptRun::of(&["call"]), t0);
        assert!(!broker.is_idle());

        assert!(
            broker.tick(&env, t0).is_empty(),
            "no session yet is not a change of session"
        );
        assert_eq!(env.sent_by(), ["host"]);
        assert_eq!(env.sent()[0].session, "s1");

        env.ok(0);
        assert!(broker.tick(&env, t0).is_empty(), "a host run answers nobody");
        assert!(broker.is_idle());
    }

    #[test]
    fn a_host_run_on_a_tab_gone_is_dropped() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        broker.submit_run(TabId(9), ScriptRun::of(&["call"]), t0);
        assert!(broker.tick(&env, t0).is_empty());
        assert!(broker.is_idle());
        assert!(env.sent_by().is_empty());
    }

    #[test]
    fn forgetting_a_tab_clears_its_memory() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        let remember = |broker: &mut Broker, session: &str| {
            env.tabs.borrow_mut().insert(TAB, facts(session));
            submit(broker, &env, TAB, sidecar("a"), &["remember"], t0);
            broker.tick(&env, t0);
            assert!(broker.memory.holds(TAB.0));
        };

        remember(&mut broker, "s1");
        broker.withdraw_tab(TAB, words::MOVED);
        assert!(!broker.memory.holds(TAB.0), "a withdrawn tab");

        remember(&mut broker, "s1");
        env.tabs.borrow_mut().remove(&TAB);
        broker.tick(&env, t0);
        assert!(!broker.memory.holds(TAB.0), "a tab gone");

        remember(&mut broker, "s1");
        env.tabs.borrow_mut().insert(TAB, facts("s9"));
        broker.tick(&env, t0);
        assert!(!broker.memory.holds(TAB.0), "a new session");
    }

    #[test]
    fn no_step_runs_with_no_way_to_an_editor() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::without_a_way(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("a"), &["call"], t0);
        let out = broker.tick(&env, t0);
        assert_eq!(
            only_answer(&out).text,
            format!("start Unavailable({:?})", words::NO_EDITOR)
        );
        assert!(broker.is_idle());
    }

    #[test]
    fn nothing_held_is_idle() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        assert!(broker.is_idle());
        submit(&mut broker, &env, TAB, sidecar("a"), &["call"], t0);
        assert!(!broker.is_idle(), "queued");
        broker.tick(&env, t0);
        assert!(!broker.is_idle(), "in flight");
        env.ok(0);
        broker.tick(&env, t0);
        assert!(broker.is_idle(), "an empty line holds nothing");
    }

    /// A waiting card can stay for minutes, and nothing about it needs looking at until an answer comes or its limit.
    #[test]
    fn a_waiting_card_asks_for_no_polling_and_names_its_limit() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        assert!(!broker.needs_polling());
        assert_eq!(broker.next_card_limit(), None);
        submit(&mut broker, &env, TAB, sidecar("a"), &["ask"], t0);
        assert!(broker.needs_polling(), "queued");
        broker.tick(&env, t0);
        assert_eq!(broker.cards(TAB).len(), 1);
        assert!(!broker.is_idle(), "the call is still open");
        assert!(!broker.needs_polling(), "on a card");
        assert_eq!(broker.next_card_limit(), Some(t0 + CARD_LIMIT));

        let later = t0 + Duration::from_secs(1);
        submit(&mut broker, &env, TAB, sidecar("b"), &["ask"], later);
        broker.tick(&env, later);
        assert_eq!(broker.cards(TAB).len(), 2);
        assert_eq!(
            broker.next_card_limit(),
            Some(t0 + CARD_LIMIT),
            "the soonest of the two"
        );

        let id = broker.cards(TAB)[0].id.clone();
        broker
            .answer_card(TAB, &id, CardDecision::Allow, CardOrigin::Key, later)
            .unwrap();
        assert!(broker.needs_polling(), "an answered call goes back in the queue");
        assert_eq!(broker.next_card_limit(), Some(later + CARD_LIMIT));
    }

    fn driven_cx<'a>(memory: &'a Memory) -> ToolCx<'a> {
        ToolCx {
            mode: PermissionMode::Manual,
            trust: ProjectTrust::Trusted,
            root: Path::new("/p"),
            home: None,
            reach: EditorReach::Own,
            sight: EditorSight::OnScreen,
            context: None,
            tab: TAB.0,
            session: "s1",
            call: "the-call",
            agent_call: AgentCall::Host,
            memory,
        }
    }

    #[test]
    fn drive_with_cards_answers_a_card() {
        let memory = Memory::default();
        let cx = driven_cx(&memory);
        let mut asked = Vec::new();
        let steps = testing::drive_with_cards(
            ScriptRun::of(&["ask 9", "call"]),
            &cx,
            |_| Ok(Value::from("seen")),
            |request| {
                asked.push(request.title.clone());
                (CardDecision::Allow, CardOrigin::Key)
            },
        );
        assert_eq!(asked, ["Allow this?"]);
        assert_eq!(steps.len(), 3, "{steps:?}");
        let Step::Done(result) = &steps[2] else {
            panic!("{steps:?}");
        };
        assert_eq!(heard(&result.text, "allowed="), "true");
        assert_eq!(heard(&result.text, "bind="), "9");
        assert_eq!(heard(&result.text, "call="), "the-call");
        assert!(result.text.ends_with("value=\"seen\""), "{}", result.text);
    }

    #[test]
    fn drive_keeps_the_brokers_rule_for_a_spent_budget() {
        let memory = Memory::default();
        let cx = driven_cx(&memory);
        let budget = STAMP_MARGIN.as_millis();
        let word = format!("call {budget}");
        let steps = testing::drive(ScriptRun::of(&[&word, "poll 5"]), &cx, |_| {
            panic!("a spent budget reaches no editor")
        });
        let Step::Done(result) = steps.last().unwrap() else {
            panic!("{steps:?}");
        };
        assert_eq!(result.text, "start NotAnswered poll");
    }

    /// A window whose tabs were given `diagnostics`.
    fn env_with_diagnostics() -> FakeEnv {
        let env = FakeEnv::new();
        for tab in [TAB, OTHER_TAB] {
            let mut tab_facts = facts("s1");
            tab_facts.declared = Rc::new(BTreeSet::from(["editor_state".to_owned(), "diagnostics".to_owned()]));
            env.tabs.borrow_mut().insert(tab, tab_facts);
        }
        env
    }

    /// What the editor says to the baseline's chunk: one error.
    fn one_diagnostic() -> Value {
        let text = |name: &str, value: &str| (Value::from(name), Value::from(value));
        let number = |name: &str, value: u32| (Value::from(name), Value::from(value));
        Value::Map(vec![
            (
                Value::from("entries"),
                Value::Array(vec![Value::Map(vec![
                    text("path", "/p/a.rs"),
                    number("lnum", 0),
                    number("col", 0),
                    number("end_lnum", 0),
                    number("end_col", 1),
                    text("severity", "error"),
                    text("message", "boom"),
                ])]),
            ),
            (Value::from("modified"), Value::Array(vec![])),
            number("left_out", 0),
        ])
    }

    fn baseline_of(broker: &Broker, tab: TabId) -> Option<TurnBaseline> {
        broker.memory.with(tab.0, |memory| memory.baseline.clone())
    }

    #[test]
    fn a_turn_start_takes_a_baseline_ahead_of_its_calls() {
        let (mut broker, env, t0) = (Broker::new(), env_with_diagnostics(), Instant::now());
        assert!(broker.turn_began(TAB, "t1", &env, t0).is_empty());
        submit(&mut broker, &env, TAB, sidecar("a"), &["call"], t0);
        assert_eq!(
            baseline_of(&broker, TAB),
            Some(TurnBaseline::Taking { turn: "t1".to_owned() })
        );
        broker.tick(&env, t0);
        assert_eq!(env.sent_by(), ["baseline"], "the turn's own call waits for it");

        env.reply(0, Ok(one_diagnostic()));
        broker.tick(&env, after(t0, 10));
        assert_eq!(env.sent_by(), ["baseline", "a"]);
        let Some(TurnBaseline::Taken { turn, counts, cut }) = baseline_of(&broker, TAB) else {
            panic!("the baseline is in");
        };
        assert_eq!((turn.as_str(), counts.len(), cut), ("t1", 1, false));
        assert_eq!(env.sent()[0].session, "s1");
    }

    #[test]
    fn the_tool_off_takes_no_baseline() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        assert!(broker.turn_began(TAB, "t1", &env, t0).is_empty());
        assert!(broker.is_idle());
        assert!(!broker.memory.holds(TAB.0));
        broker.tick(&env, t0);
        assert!(env.sent_by().is_empty());
        // A tab the window does not know has none either.
        let env = env_with_diagnostics();
        broker.turn_began(TabId(9), "t1", &env, t0);
        assert!(broker.is_idle());
    }

    #[test]
    fn a_late_baseline_of_an_older_turn_is_dropped() {
        let (mut broker, env, t0) = (Broker::new(), env_with_diagnostics(), Instant::now());
        broker.turn_began(TAB, "t1", &env, t0);
        broker.tick(&env, t0);
        assert_eq!(env.sent_by(), ["baseline"]);
        broker.turn_began(TAB, "t2", &env, after(t0, 5));

        env.reply(0, Ok(one_diagnostic()));
        broker.tick(&env, after(t0, 10));
        assert_eq!(
            baseline_of(&broker, TAB),
            Some(TurnBaseline::Taking { turn: "t2".to_owned() }),
            "the first turn's answer did not become the second's baseline"
        );
        assert_eq!(env.sent_by(), ["baseline", "baseline"]);

        env.reply(1, Ok(Value::Map(vec![(Value::from("expired"), Value::from(true))])));
        broker.tick(&env, after(t0, 20));
        assert_eq!(
            baseline_of(&broker, TAB),
            Some(TurnBaseline::Failed { turn: "t2".to_owned() })
        );
    }

    #[test]
    fn a_baseline_run_answers_nobody() {
        let (mut broker, env, t0) = (Broker::new(), env_with_diagnostics(), Instant::now());
        broker.turn_began(TAB, "t1", &env, t0);
        assert!(broker.tick(&env, t0).is_empty());
        env.reply(0, Ok(one_diagnostic()));
        assert!(broker.tick(&env, after(t0, 10)).is_empty());
        assert!(broker.is_idle());
        // Neither does one the editor never answers.
        broker.turn_began(TAB, "t2", &env, after(t0, 20));
        broker.tick(&env, after(t0, 20));
        let out = broker.tick(&env, after(t0, 20) + EDITOR_BUDGET + Duration::from_millis(1));
        assert!(out.is_empty(), "{out:?}");
        assert_eq!(
            baseline_of(&broker, TAB),
            Some(TurnBaseline::Failed { turn: "t2".to_owned() })
        );
    }

    fn keep_a_proposal(broker: &Broker) -> String {
        broker.memory.keep_proposal(TAB.0, "a change".to_owned(), Vec::new())
    }

    #[test]
    fn a_session_change_drops_proposals() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("a"), &[], t0);
        broker.tick(&env, t0);
        let id = keep_a_proposal(&broker);
        assert!(broker.memory.proposal(TAB.0, &id).is_some());

        env.tabs.borrow_mut().insert(TAB, facts("another session"));
        submit(&mut broker, &env, TAB, sidecar("b"), &[], after(t0, 5));
        assert!(
            broker.memory.proposal(TAB.0, &id).is_none(),
            "a proposal is for the session it was computed for"
        );
    }

    #[test]
    fn a_tab_leaving_drops_proposals() {
        let (mut broker, env, t0) = (Broker::new(), FakeEnv::new(), Instant::now());
        submit(&mut broker, &env, TAB, sidecar("a"), &[], t0);
        let id = keep_a_proposal(&broker);
        broker.withdraw_tab(TAB, words::MOVED);
        assert!(broker.memory.proposal(TAB.0, &id).is_none());
    }

    #[test]
    fn a_call_hands_the_window_its_writer_before_the_tool_begins() {
        struct WithAWriter(FakeEnv);
        impl BrokerEnv for WithAWriter {
            fn way(&self) -> Option<&dyn EditorWay> {
                self.0.way()
            }
            fn facts(&self, tab: TabId) -> Option<TabFacts> {
                self.0.facts(tab)
            }
            fn root(&self) -> &Path {
                self.0.root()
            }
            fn home(&self) -> Option<&Path> {
                self.0.home()
            }
            fn context(&self) -> Option<EditorContext> {
                None
            }
            fn proposal_writer(&self) -> Option<ProposalWriter> {
                Some(ProposalWriter::new(None, Path::new("/p")))
            }
        }
        let (mut broker, t0) = (Broker::new(), Instant::now());
        let with_one = WithAWriter(FakeEnv::new());
        assert!(broker.memory.writer().is_none());
        broker.submit_tool(TAB, sidecar("a"), None, &SCRIPTED, r#"{"steps":[]}"#, &with_one, t0);
        assert!(broker.memory.writer().is_some());

        // A default says none, and the writer of an earlier call does not stay for it.
        let without = FakeEnv::new();
        submit(&mut broker, &without, TAB, sidecar("b"), &[], t0);
        assert!(broker.memory.writer().is_none());
    }

    #[test]
    fn a_new_sessions_first_turn_replaces_the_old_ones_line() {
        let (mut broker, env, t0) = (Broker::new(), env_with_diagnostics(), Instant::now());
        broker.turn_began(TAB, "t1", &env, t0);
        broker.tick(&env, t0);
        let mut next = facts("s2");
        next.declared = Rc::new(BTreeSet::from(["diagnostics".to_owned()]));
        env.tabs.borrow_mut().insert(TAB, next);
        broker.turn_began(TAB, "t1", &env, after(t0, 5));
        broker.tick(&env, after(t0, 5));
        assert_eq!(env.sent_by(), ["baseline", "baseline"]);
        assert_eq!(env.sent()[1].session, "s2");
        assert_eq!(
            baseline_of(&broker, TAB),
            Some(TurnBaseline::Taking { turn: "t1".to_owned() })
        );
    }
}
