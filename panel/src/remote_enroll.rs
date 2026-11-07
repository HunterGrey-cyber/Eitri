//! The window's questions about remote control, asked in its base page: may a phone that presented this window's
//! enrollment code be added, may a device's grant change (`eitri remote grant`), and may `serve` listen for phones on
//! a named address. The grant question is answered with the phone question's page kind, `remote_enroll_answer`,
//! under its own nonce: both say yes or no to what a device may do, and both meet the same gate.
//!
//! Each is a y/n of the page's own kind (drawn after paint, a `y` counted only once the question has been on screen
//! and no other key came just before it), and each answer is window-level and gated like `confirm_bypass`: from the
//! base page only, and a `y` only while the window's answer gate agrees the page is not covered. A refused `y` leaves
//! the question waiting under its nonce. Nothing is decided here: the answer goes to whoever asked (the `rv-`
//! socket's errand), which mints the device's token or records the address only after a `y`.
//!
//! One question of each kind waits at a time; another arriving while one waits is turned down. One left unanswered for
//! [`QUESTION_WITHIN`] is answered no (whoever asked is told `the desk did not answer in time`) at the first of: the
//! remote socket's pump at or after that time ([`expire_questions`]), the next question of its kind, or the page's
//! late answer. Each question tells the page how long it waits, and the page takes it down then on its own.
//!
//! The device list is the window's too, asked for from the base page (`remote_devices_open`, `remote_start_enroll`,
//! `remote_revoke_ask`): each is kept as a [`DeskRequest`] for the `rv-` socket's next pump, which reads the store
//! and the audit log, makes a code or reads the device's name on a thread of its own. Only while remote control is on
//! in the window ([`switch_on`], when its socket is with the panel); before that each is refused. A revoke from the
//! list is the fourth question, "Revoke <name>? (y/n)" (`remote_revoke_ask`), answered by `remote_revoke` under its
//! nonce and gated as the others are; one waits at a time, and a newer one replaces it, since only the desk asks it.
//!
//! A question may be asked before the base page's document is ready (an address asked about as the window opens) or
//! outlive the document it was shown in (a reload): every question still waiting in time is shown again, with the time
//! it has left, to each new base document on its `ready` ([`questions_for_new_document`]).

use super::*;
use eitri_core::remote_devices::Grant;
use eitri_core::remote_pairing::{IfaceKind, PairMode, ENROLL_CODE_TTL};
use std::time::{Duration, Instant};

/// How long a question waits for the desk: as long as an enrollment code lives.
pub(super) const QUESTION_WITHIN: Duration = ENROLL_CODE_TTL;

/// What an extra page is told when it answers one of these questions.
pub(super) const REMOTE_QUESTIONS_ARE_THE_BASE_PAGES: &str = "protocol: remote control is answered in the base view";

/// What a second phone is told while the desk is asked about the first.
pub(super) const ANOTHER_DEVICE_WAITS: &str = "the desk is already asked about another device";

/// What a second listen request is told while the desk is asked about one.
const ANOTHER_ADDRESS_WAITS: &str = "the desk is already asked about an address";

/// What the phone is told when the desk said no.
pub(super) const THE_DESK_SAID_NO: &str = "the desk said no";

/// What the phone is told when nobody answered in time.
pub(super) const THE_DESK_DID_NOT_ANSWER: &str = "the desk did not answer in time";

/// What a page is told when it answers a question that is no longer the one waiting.
const NO_LONGER_CURRENT: &str = "that question is no longer current";

/// What a page is told when it chooses a grant the question did not offer.
pub(super) const SSH_ONLY_WATCHES: &str = "a phone enrolled through SSH may only watch for now";

/// What a second grant request is told while the desk is asked about one.
const ANOTHER_GRANT_WAITS: &str = "the desk is already asked about a grant";

/// What a page is told when its yes names a grant other than the one asked about.
const NOT_THE_GRANT_ASKED: &str = "that is not the grant the desk was asked about";

/// The longest device name, host or peer the question shows; a longer one is refused before the desk is asked.
pub(super) const REMOTE_FIELD_MAX_BYTES: usize = 256;

/// What an extra page is told when it asks for the device list or a code.
pub(super) const REMOTE_IS_THE_BASE_PAGES: &str = "protocol: remote control's device list is in the base view";

/// What the base page is told when it asks for the device list while remote control is off in the window.
pub(super) const REMOTE_OFF: &str = "remote control is off in this window (remote.enabled)";

/// What the base page is told when it asks for more than the socket has taken yet.
pub(super) const DESK_BUSY: &str = "the window is still reading the device list: try again";

/// What a page is told when its revoke names another device than the question asked about.
pub(super) const NOT_THE_DEVICE_ASKED: &str = "that is not the device the desk was asked about";

/// The device list's requests kept for the socket's next pump; one more is refused ([`DESK_BUSY`]).
pub(super) const DESK_REQUESTS_MAX: usize = 8;

/// What the base page asked of the device list, carried out by the `rv-` socket off the tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DeskRequest {
    /// Read the device store and each device's last audit records (`remote_devices`).
    List,
    /// Make an enrollment code by this recipe, the window's default with `None`.
    StartEnroll(Option<PairMode>),
    /// Read the device's name and ask the base page whether to revoke it.
    RevokeAsk(String),
}

/// A revoke from the device list as the desk is asked about it: the device's id and its name in the store.
#[derive(Debug, Clone)]
pub(super) struct RevokeAsk {
    pub(super) device: String,
    pub(super) name: String,
}

/// A phone's enrollment as the desk is asked about it: the name it claims, the `Host` it reached `serve` through,
/// the address it came from, and how the code it presented said it connects.
#[derive(Debug, Clone)]
pub(super) struct EnrollAsk {
    pub(super) name: String,
    pub(super) host: String,
    pub(super) peer: String,
    /// The recipe of the code the phone presented, which the device is stored under.
    pub(super) mode: PairMode,
    /// Where that code sends phones, as the desk is told it ([`eitri_core::remote_pairing::Endpoint::describe`]).
    pub(super) target: String,
}

/// A named listen address as the desk is asked about it.
#[derive(Debug, Clone)]
pub(super) struct ListenAsk {
    pub(super) addr: String,
    pub(super) iface: String,
    pub(super) kind: IfaceKind,
}

/// A change to an enrolled device's grant as the desk is asked about it: the device's id and name in the store, its
/// grant now and the one asked for.
#[derive(Debug, Clone)]
pub(super) struct GrantAsk {
    pub(super) device: String,
    pub(super) name: String,
    pub(super) from: Grant,
    pub(super) to: Grant,
}

/// The desk's answer about a phone, or about a grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EnrollReply {
    /// `y`, with the grant chosen.
    Yes(Grant),
    /// No, and what the phone is told.
    No(&'static str),
}

/// Where an answer goes, called once.
type Reply<T> = Box<dyn FnOnce(T)>;

struct Asking<A, R> {
    nonce: u64,
    asked: Instant,
    ask: A,
    reply: Reply<R>,
}

/// The questions waiting in this window.
#[derive(Default)]
pub(super) struct RemoteDesk {
    next_nonce: u64,
    enroll: Option<Asking<EnrollAsk, EnrollReply>>,
    grant: Option<Asking<GrantAsk, EnrollReply>>,
    listen: Option<Asking<ListenAsk, bool>>,
    /// Called only on a yes: a no decides nothing, so the page takes its question down without a word.
    revoke: Option<Asking<RevokeAsk, ()>>,
    /// Remote control is on in this window: its `rv-` socket is with the panel.
    on: bool,
    /// What the base page asked of the device list, for the socket's next pump.
    requests: std::collections::VecDeque<DeskRequest>,
    /// The last phone list and sandbox, replayed to each document on `ready`.
    connected: Vec<ConnectedShown>,
    sandbox: Option<String>,
}

impl RemoteDesk {
    fn nonce(&mut self) -> u64 {
        self.next_nonce += 1;
        self.next_nonce
    }

    /// The nonce of the phone question waiting, if any.
    #[cfg(test)]
    pub(super) fn enroll_nonce(&self) -> Option<u64> {
        self.enroll.as_ref().map(|a| a.nonce)
    }

    /// The nonce of the grant question waiting, if any.
    #[cfg(test)]
    pub(super) fn grant_nonce(&self) -> Option<u64> {
        self.grant.as_ref().map(|a| a.nonce)
    }

    /// The nonce of the revoke question waiting, if any.
    #[cfg(test)]
    pub(super) fn revoke_nonce(&self) -> Option<u64> {
        self.revoke.as_ref().map(|a| a.nonce)
    }
}

/// The grants the desk may give a phone that connects this way: a phone using the SSH recipe only watches.
pub(super) fn grants_for(mode: PairMode) -> &'static [Grant] {
    match mode {
        PairMode::Ssh => &[Grant::Watch],
        PairMode::Direct => &[Grant::Watch, Grant::WatchSteer],
    }
}

pub(super) fn grant_word(grant: Grant) -> &'static str {
    match grant {
        Grant::Watch => "watch",
        Grant::WatchSteer => "watch+steer",
    }
}

/// The phone question `nonce` about `ask`, waiting `left` more.
fn enroll_payload(nonce: u64, ask: &EnrollAsk, left: Duration) -> String {
    let grants: Vec<&str> = grants_for(ask.mode).iter().copied().map(grant_word).collect();
    serde_json::json!({
        "kind": "remote_enroll",
        "expiresInMs": left.as_millis() as u64,
        "nonce": nonce,
        "name": ask.name,
        "host": ask.host,
        "peer": ask.peer,
        "recipe": ask.mode.as_str(),
        "target": ask.target,
        "grants": grants,
        "grant": grant_word(Grant::Watch),
    })
    .to_string()
}

/// The grant question `nonce` about `ask`, waiting `left` more.
fn grant_payload(nonce: u64, ask: &GrantAsk, left: Duration) -> String {
    serde_json::json!({
        "kind": "remote_grant",
        "expiresInMs": left.as_millis() as u64,
        "nonce": nonce,
        "device": ask.device,
        "name": ask.name,
        "from": grant_word(ask.from),
        "to": grant_word(ask.to),
    })
    .to_string()
}

/// The revoke question `nonce` about `ask`, waiting `left` more.
fn revoke_payload(nonce: u64, ask: &RevokeAsk, left: Duration) -> String {
    serde_json::json!({
        "kind": "remote_revoke_ask",
        "expiresInMs": left.as_millis() as u64,
        "nonce": nonce,
        "device": ask.device,
        "name": ask.name,
    })
    .to_string()
}

/// The address question `nonce` about `ask`, waiting `left` more.
fn listen_payload(nonce: u64, ask: &ListenAsk, left: Duration) -> String {
    serde_json::json!({
        "kind": "remote_listen",
        "expiresInMs": left.as_millis() as u64,
        "nonce": nonce,
        "addr": ask.addr,
        "iface": ask.iface,
        "link": ask.kind.as_str(),
        "plain": !ask.kind.is_tunnel(),
    })
    .to_string()
}

/// The questions waiting in time at `now`, for a base document that has just said `ready`: each under its own nonce,
/// with the time it has left. A question past its time is not shown; it is answered no as before.
pub(super) fn questions_for_new_document(state: &AgentPanelState, now: Instant) -> Vec<String> {
    let desk = &state.remote_desk;
    let left = |asked: Instant| {
        QUESTION_WITHIN
            .checked_sub(now.saturating_duration_since(asked))
            .filter(|d| !d.is_zero())
    };
    let mut shown = Vec::new();
    if let Some((a, left)) = desk.enroll.as_ref().and_then(|a| Some((a, left(a.asked)?))) {
        shown.push(enroll_payload(a.nonce, &a.ask, left));
    }
    if let Some((a, left)) = desk.grant.as_ref().and_then(|a| Some((a, left(a.asked)?))) {
        shown.push(grant_payload(a.nonce, &a.ask, left));
    }
    if let Some((a, left)) = desk.listen.as_ref().and_then(|a| Some((a, left(a.asked)?))) {
        shown.push(listen_payload(a.nonce, &a.ask, left));
    }
    if let Some((a, left)) = desk.revoke.as_ref().and_then(|a| Some((a, left(a.asked)?))) {
        shown.push(revoke_payload(a.nonce, &a.ask, left));
    }
    shown
}

/// Asks the base page whether the phone `ask` describes may be added; `reply` gets the answer once. Turned down,
/// with what the phone is told, while another phone's question waits in time.
pub(super) fn ask_enroll(
    state: &mut AgentPanelState,
    ask: EnrollAsk,
    now: Instant,
    reply: Reply<EnrollReply>,
    out: &mut Outbox,
) -> Result<(), &'static str> {
    let desk = &mut state.remote_desk;
    if let Some(waiting) = &desk.enroll {
        if now.duration_since(waiting.asked) < QUESTION_WITHIN {
            return Err(ANOTHER_DEVICE_WAITS);
        }
    }
    if let Some(stale) = desk.enroll.take() {
        (stale.reply)(EnrollReply::No(THE_DESK_DID_NOT_ANSWER));
    }
    let nonce = desk.nonce();
    out.send(ViewId::BASE, enroll_payload(nonce, &ask, QUESTION_WITHIN));
    desk.enroll = Some(Asking {
        nonce,
        asked: now,
        ask,
        reply,
    });
    Ok(())
}

/// Asks the base page whether the device `ask` names may change its grant; `reply` gets the answer once, `Yes` with the
/// grant asked for. Turned down while another grant question waits in time.
pub(super) fn ask_grant(
    state: &mut AgentPanelState,
    ask: GrantAsk,
    now: Instant,
    reply: Reply<EnrollReply>,
    out: &mut Outbox,
) -> Result<(), &'static str> {
    let desk = &mut state.remote_desk;
    if let Some(waiting) = &desk.grant {
        if now.duration_since(waiting.asked) < QUESTION_WITHIN {
            return Err(ANOTHER_GRANT_WAITS);
        }
    }
    if let Some(stale) = desk.grant.take() {
        (stale.reply)(EnrollReply::No(THE_DESK_DID_NOT_ANSWER));
    }
    let nonce = desk.nonce();
    out.send(ViewId::BASE, grant_payload(nonce, &ask, QUESTION_WITHIN));
    desk.grant = Some(Asking {
        nonce,
        asked: now,
        ask,
        reply,
    });
    Ok(())
}

/// Asks the base page whether `serve` may listen on the address `ask` names; `reply` gets the answer once. Turned
/// down while another address's question waits in time.
pub(super) fn ask_listen(
    state: &mut AgentPanelState,
    ask: ListenAsk,
    now: Instant,
    reply: Reply<bool>,
    out: &mut Outbox,
) -> Result<(), &'static str> {
    let desk = &mut state.remote_desk;
    if let Some(waiting) = &desk.listen {
        if now.duration_since(waiting.asked) < QUESTION_WITHIN {
            return Err(ANOTHER_ADDRESS_WAITS);
        }
    }
    if let Some(stale) = desk.listen.take() {
        (stale.reply)(false);
    }
    let nonce = desk.nonce();
    out.send(ViewId::BASE, listen_payload(nonce, &ask, QUESTION_WITHIN));
    desk.listen = Some(Asking {
        nonce,
        asked: now,
        ask,
        reply,
    });
    Ok(())
}

/// Asks the base page whether the device `ask` names may be revoked; `reply` is called once, on a yes. A revoke
/// question waiting is replaced: only the desk asks one, and the newest is the one the desk means.
pub(super) fn ask_revoke(
    state: &mut AgentPanelState,
    ask: RevokeAsk,
    now: Instant,
    reply: Reply<()>,
    out: &mut Outbox,
) {
    let desk = &mut state.remote_desk;
    let nonce = desk.nonce();
    out.send(ViewId::BASE, revoke_payload(nonce, &ask, QUESTION_WITHIN));
    desk.revoke = Some(Asking {
        nonce,
        asked: now,
        ask,
        reply,
    });
}

/// The page's yes to the revoke question `nonce` about `device`, already admitted (the base page, past the window's
/// gate). Only the question waiting, in time and about that device is answered; otherwise nothing changes and the
/// page is told why. A question past its time is taken out.
pub(super) fn answer_revoke(state: &mut AgentPanelState, nonce: u64, device: &str, now: Instant) -> Result<(), String> {
    let desk = &mut state.remote_desk;
    let Some(waiting) = desk.revoke.as_ref().filter(|a| a.nonce == nonce) else {
        return Err(NO_LONGER_CURRENT.into());
    };
    if now.duration_since(waiting.asked) >= QUESTION_WITHIN {
        desk.revoke = None;
        return Err(NO_LONGER_CURRENT.into());
    }
    if waiting.ask.device != device {
        return Err(NOT_THE_DEVICE_ASKED.into());
    }
    let Some(asking) = desk.revoke.take() else {
        return Err(NO_LONGER_CURRENT.into());
    };
    (asking.reply)(());
    Ok(())
}

/// Keeps what the base page asked of the device list for the socket's next pump. Refused while remote control is off
/// in the window, and past [`DESK_REQUESTS_MAX`] waiting.
pub(super) fn want(state: &mut AgentPanelState, request: DeskRequest) -> Result<(), &'static str> {
    let desk = &mut state.remote_desk;
    if !desk.on {
        return Err(REMOTE_OFF);
    }
    if desk.requests.len() >= DESK_REQUESTS_MAX {
        return Err(DESK_BUSY);
    }
    desk.requests.push_back(request);
    Ok(())
}

/// What the base page asked of the device list since the last pump, oldest first.
pub(super) fn take_requests(state: &mut AgentPanelState) -> Vec<DeskRequest> {
    state.remote_desk.requests.drain(..).collect()
}

/// Remote control is on in this window (its socket is with the panel): the base page is told once, so the device
/// list's key is offered from now on. An extra chat view is never told: the list is the base page's, and a view that
/// cannot open it does not offer its key.
pub(super) fn switch_on(state: &mut AgentPanelState, out: &mut Outbox) {
    if state.remote_desk.on {
        return;
    }
    state.remote_desk.on = true;
    if state.pages.contains_key(&ViewId::BASE) {
        out.send(ViewId::BASE, remote_on_payload());
    }
}

fn remote_on_payload() -> String {
    serde_json::json!({ "kind": "remote_on" }).to_string()
}

/// What a document that has just said `ready` in `view` is told about remote control being on: only the base page's.
pub(super) fn remote_on_for_new_document(state: &AgentPanelState, view: ViewId) -> Option<String> {
    (state.remote_desk.on && view.is_base()).then(remote_on_payload)
}

/// One device of the list as the base page is shown it: the store's entry, where its notifications go (the host
/// only: the rest of an endpoint can be a credential) and its newest audit records, each `{ time, event, writer,
/// fields }` with every field as `[name, text]`.
#[derive(Debug, Clone)]
pub(super) struct DeviceRow {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) grant: Grant,
    pub(super) recipe: &'static str,
    pub(super) enrolled_at: Option<u64>,
    pub(super) last_seen: Option<u64>,
    pub(super) last_host: Option<String>,
    pub(super) push_host: Option<String>,
    pub(super) records: Vec<serde_json::Value>,
}

/// Shows the device list in the base page; a device is `connected` when the last phone list names it.
pub(super) fn show_devices(state: &AgentPanelState, rows: &[DeviceRow], out: &mut Outbox) {
    let devices: Vec<serde_json::Value> = rows
        .iter()
        .map(|d| {
            serde_json::json!({
                "id": d.id,
                "name": d.name,
                "grant": grant_word(d.grant),
                "recipe": d.recipe,
                "enrolled_at": d.enrolled_at,
                "last_seen": d.last_seen,
                "last_host": d.last_host,
                "connected": state.remote_desk.connected.iter().any(|c| c.device == d.id),
                "push_host": d.push_host,
                "records": d.records,
            })
        })
        .collect();
    out.send(
        ViewId::BASE,
        serde_json::json!({ "kind": "remote_devices", "devices": devices }).to_string(),
    );
}

/// The page's answer to the phone question or the grant question `nonce`, already admitted (the base page; a `yes`
/// past the window's gate). The question must be the one waiting and in time, and a `yes` must choose a grant it
/// offered (for a grant question, the grant it asked about); otherwise nothing changes and the page is told why. A
/// question past its time is answered no.
pub(super) fn answer_enroll(
    state: &mut AgentPanelState,
    nonce: u64,
    yes: bool,
    grant: Grant,
    now: Instant,
) -> Result<(), String> {
    if state.remote_desk.grant.as_ref().is_some_and(|a| a.nonce == nonce) {
        return answer_grant(state, nonce, yes, grant, now);
    }
    let desk = &mut state.remote_desk;
    let Some(waiting) = desk.enroll.as_ref().filter(|a| a.nonce == nonce) else {
        return Err(NO_LONGER_CURRENT.into());
    };
    if now.duration_since(waiting.asked) >= QUESTION_WITHIN {
        if let Some(stale) = desk.enroll.take() {
            (stale.reply)(EnrollReply::No(THE_DESK_DID_NOT_ANSWER));
        }
        return Err(NO_LONGER_CURRENT.into());
    }
    if yes && !grants_for(waiting.ask.mode).contains(&grant) {
        return Err(SSH_ONLY_WATCHES.into());
    }
    let Some(asking) = desk.enroll.take() else {
        return Err(NO_LONGER_CURRENT.into());
    };
    (asking.reply)(if yes {
        EnrollReply::Yes(grant)
    } else {
        EnrollReply::No(THE_DESK_SAID_NO)
    });
    Ok(())
}

/// The page's answer to the grant question `nonce`, reached through [`answer_enroll`].
fn answer_grant(state: &mut AgentPanelState, nonce: u64, yes: bool, grant: Grant, now: Instant) -> Result<(), String> {
    let desk = &mut state.remote_desk;
    let Some(waiting) = desk.grant.as_ref().filter(|a| a.nonce == nonce) else {
        return Err(NO_LONGER_CURRENT.into());
    };
    if now.duration_since(waiting.asked) >= QUESTION_WITHIN {
        if let Some(stale) = desk.grant.take() {
            (stale.reply)(EnrollReply::No(THE_DESK_DID_NOT_ANSWER));
        }
        return Err(NO_LONGER_CURRENT.into());
    }
    if yes && grant != waiting.ask.to {
        return Err(NOT_THE_GRANT_ASKED.into());
    }
    let Some(asking) = desk.grant.take() else {
        return Err(NO_LONGER_CURRENT.into());
    };
    let to = asking.ask.to;
    (asking.reply)(if yes {
        EnrollReply::Yes(to)
    } else {
        EnrollReply::No(THE_DESK_SAID_NO)
    });
    Ok(())
}

/// The page's answer to the listen question `nonce`, admitted as [`answer_enroll`]'s is.
pub(super) fn answer_listen(state: &mut AgentPanelState, nonce: u64, yes: bool, now: Instant) -> Result<(), String> {
    let desk = &mut state.remote_desk;
    let Some(waiting) = desk.listen.as_ref().filter(|a| a.nonce == nonce) else {
        return Err(NO_LONGER_CURRENT.into());
    };
    let in_time = now.duration_since(waiting.asked) < QUESTION_WITHIN;
    let Some(asking) = desk.listen.take() else {
        return Err(NO_LONGER_CURRENT.into());
    };
    if !in_time {
        (asking.reply)(false);
        return Err(NO_LONGER_CURRENT.into());
    }
    (asking.reply)(yes);
    Ok(())
}

/// Answers no to every question that has waited [`QUESTION_WITHIN`] by `now`, telling whoever asked, and tells the base
/// page to take each down (`remote_question_closed`).
pub(super) fn expire_questions(state: &mut AgentPanelState, now: Instant, out: &mut Outbox) {
    let desk = &mut state.remote_desk;
    let late = |asked: Instant| now.saturating_duration_since(asked) >= QUESTION_WITHIN;
    let mut closed = Vec::new();
    if desk.enroll.as_ref().is_some_and(|a| late(a.asked)) {
        let stale = desk.enroll.take().expect("checked above");
        closed.push(stale.nonce);
        (stale.reply)(EnrollReply::No(THE_DESK_DID_NOT_ANSWER));
    }
    if desk.grant.as_ref().is_some_and(|a| late(a.asked)) {
        let stale = desk.grant.take().expect("checked above");
        closed.push(stale.nonce);
        (stale.reply)(EnrollReply::No(THE_DESK_DID_NOT_ANSWER));
    }
    if desk.listen.as_ref().is_some_and(|a| late(a.asked)) {
        let stale = desk.listen.take().expect("checked above");
        closed.push(stale.nonce);
        (stale.reply)(false);
    }
    if desk.revoke.as_ref().is_some_and(|a| late(a.asked)) {
        let stale = desk.revoke.take().expect("checked above");
        closed.push(stale.nonce);
    }
    for nonce in closed {
        out.send(
            ViewId::BASE,
            serde_json::json!({ "kind": "remote_question_closed", "nonce": nonce }).to_string(),
        );
    }
}

impl RemoteDesk {
    /// Whether any question waits.
    pub(super) fn any_waiting(&self) -> bool {
        self.enroll.is_some() || self.grant.is_some() || self.listen.is_some() || self.revoke.is_some()
    }
}

/// What the base page shows while a code waits to be scanned: the code (for typing instead), the URI and the QR
/// code's modules, one string of `0` and `1` per row.
pub(super) struct PairingShown {
    pub(super) code: eitri_core::remote_rv::Secret,
    pub(super) uri: String,
    pub(super) modules: Vec<String>,
}

/// The QR code of `uri`, medium error correction, as rows of `0` (light) and `1` (dark); `None` if it does not fit
/// a QR code.
pub(super) fn qr_rows(uri: &str) -> Option<Vec<String>> {
    let code = qrcode::QrCode::with_error_correction_level(uri.as_bytes(), qrcode::EcLevel::M).ok()?;
    let width = code.width();
    let colors = code.to_colors();
    Some(
        colors
            .chunks(width)
            .map(|row| {
                row.iter()
                    .map(|c| if *c == qrcode::Color::Dark { '1' } else { '0' })
                    .collect()
            })
            .collect(),
    )
}

/// Tells the base page that the device `device` (`name`) is in the store with no desk's yes recorded: something
/// other than a window's enrollment wrote it. Nothing is refused for it; the page says how to take it out.
pub(super) fn show_flagged(device: &str, name: &str, out: &mut Outbox) {
    out.send(
        ViewId::BASE,
        serde_json::json!({
            "kind": "remote_device_flagged",
            "device": device,
            "name": name,
        })
        .to_string(),
    );
}

/// One phone connected now, as the base page is told: the store's name and grant for the device `serve` named, and
/// the `Host` and time of the connection.
#[derive(Clone)]
pub(super) struct ConnectedShown {
    pub(super) device: String,
    pub(super) name: String,
    pub(super) grant: eitri_core::remote_devices::Grant,
    pub(super) host: String,
    pub(super) since: u64,
}

impl ConnectedShown {
    /// A connected device whose store entry could not be read: shown by its id, with the narrowest grant.
    pub(super) fn unnamed(c: &eitri_core::remote_rv::Connected) -> ConnectedShown {
        ConnectedShown {
            device: c.device.clone(),
            name: c.device.clone(),
            grant: eitri_core::remote_devices::Grant::Watch,
            host: c.host.clone(),
            since: c.since,
        }
    }
}

/// Tells every chat page who is connected now: the whole list, which replaces the last one. An empty list takes the
/// phone mark off the status band. The list is kept for the documents that load after this call
/// ([`connected_for_new_document`]).
pub(super) fn show_connected(state: &mut AgentPanelState, connected: &[ConnectedShown], out: &mut Outbox) {
    state.remote_desk.connected = connected.to_vec();
    send_connected(state, out);
}

pub(super) fn show_sandbox(state: &mut AgentPanelState, sandbox: String, out: &mut Outbox) {
    if state.remote_desk.sandbox.as_ref() != Some(&sandbox) {
        state.remote_desk.sandbox = Some(sandbox);
        send_connected(state, out);
    }
}

fn connected_payload(state: &AgentPanelState) -> String {
    let devices: Vec<serde_json::Value> = state
        .remote_desk
        .connected
        .iter()
        .map(|c| {
            serde_json::json!({
                "device": c.device,
                "name": c.name,
                "grant": grant_word(c.grant),
                "host": c.host,
                "since": c.since,
            })
        })
        .collect();
    serde_json::json!({ "kind": "remote_connected", "devices": devices, "sandbox": state.remote_desk.sandbox })
        .to_string()
}

fn send_connected(state: &AgentPanelState, out: &mut Outbox) {
    let payload = connected_payload(state);
    let views: Vec<ViewId> = state.pages.keys().copied().collect();
    for view in views {
        out.send(view, payload.clone());
    }
}

/// What a document that has just said `ready` is owed about the phones: the last list, while any phone is
/// connected (a document starts with none).
pub(super) fn connected_for_new_document(state: &AgentPanelState) -> Option<String> {
    (!state.remote_desk.connected.is_empty()).then(|| connected_payload(state))
}

/// Shows the code waiting to be scanned in the base page.
pub(super) fn show_pairing(shown: &PairingShown, out: &mut Outbox) {
    out.send(
        ViewId::BASE,
        serde_json::json!({
            "kind": "remote_pairing",
            "code": shown.code.expose(),
            "uri": shown.uri,
            "modules": shown.modules,
            "expiresInMs": QUESTION_WITHIN.as_millis() as u64,
        })
        .to_string(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_panel::tests::*;

    fn answer(handle: &AgentPanelHandle, from: ViewId, request: &str, nonce: u64, yes: bool, grant: &str) {
        handle.page_message(
            from,
            &format!(
                r#"{{"type":"remote_enroll_answer","request_id":"{request}","nonce":{nonce},"yes":{yes},"grant":"{grant}"}}"#
            ),
        );
    }

    fn ask(handle: &AgentPanelHandle, mode: PairMode) -> (u64, Rc<RefCell<Vec<EnrollReply>>>) {
        let replies = Rc::new(RefCell::new(Vec::new()));
        let sink = replies.clone();
        handle
            .run(|state, out| {
                ask_enroll(
                    state,
                    EnrollAsk {
                        name: "Pixel".into(),
                        host: "127.0.0.1:7341".into(),
                        peer: "127.0.0.1:50122".into(),
                        mode,
                        target: "directly to 127.0.0.1:7341 (http)".into(),
                    },
                    Instant::now(),
                    Box::new(move |r| sink.borrow_mut().push(r)),
                    out,
                )
            })
            .expect("nothing else waits");
        let nonce = handle.state.borrow().remote_desk.enroll_nonce().unwrap();
        (nonce, replies)
    }

    #[test]
    fn enroll_answer_and_listen_answer_are_gated() {
        let (handle, base, second) = handle_with_two_pages();
        let covered = Rc::new(Cell::new(true));
        let asked = recording_gate(&handle, &[1], covered.clone());

        let (nonce, replies) = ask(&handle, PairMode::Direct);
        let shown = base
            .sent
            .borrow()
            .iter()
            .map(|e| json_of(e))
            .find(|v| v["kind"] == "remote_enroll")
            .expect("the base page is asked");
        assert_eq!(shown["name"], "Pixel");
        assert_eq!(shown["grants"], serde_json::json!(["watch", "watch+steer"]));

        // Another page's answer never reaches the window or the question.
        answer(&handle, view(2), "x2", nonce, true, "watch");
        assert!(asked.borrow().is_empty());
        assert_eq!(
            result_for(&second.sent.borrow(), "x2"),
            Some(Err(REMOTE_QUESTIONS_ARE_THE_BASE_PAGES.into()))
        );

        // A covered base page's yes is refused by the window, and the question keeps its nonce.
        answer(&handle, ViewId::BASE, "y1", nonce, true, "watch+steer");
        assert_eq!(*asked.borrow(), vec![ViewId::BASE]);
        assert!(matches!(result_for(&base.sent.borrow(), "y1"), Some(Err(_))));
        assert!(replies.borrow().is_empty());
        assert_eq!(handle.state.borrow().remote_desk.enroll_nonce(), Some(nonce));

        // Uncovered, the same yes is taken, with the grant chosen.
        covered.set(false);
        answer(&handle, ViewId::BASE, "y2", nonce, true, "watch+steer");
        assert_eq!(result_for(&base.sent.borrow(), "y2"), Some(Ok(())));
        assert_eq!(*replies.borrow(), vec![EnrollReply::Yes(Grant::WatchSteer)]);
        assert_eq!(handle.state.borrow().remote_desk.enroll_nonce(), None);

        // A no does not ask the window.
        let (nonce, replies) = ask(&handle, PairMode::Direct);
        let before = asked.borrow().len();
        covered.set(true);
        answer(&handle, ViewId::BASE, "n1", nonce, false, "watch");
        assert_eq!(asked.borrow().len(), before);
        assert_eq!(*replies.borrow(), vec![EnrollReply::No(THE_DESK_SAID_NO)]);

        // The listen question meets the same gate.
        covered.set(true);
        let said = Rc::new(RefCell::new(Vec::new()));
        let sink = said.clone();
        handle
            .run(|state, out| {
                ask_listen(
                    state,
                    ListenAsk {
                        addr: "198.51.100.20".into(),
                        iface: "eth0".into(),
                        kind: IfaceKind::Plain,
                    },
                    Instant::now(),
                    Box::new(move |yes| sink.borrow_mut().push(yes)),
                    out,
                )
            })
            .unwrap();
        let listen = base
            .sent
            .borrow()
            .iter()
            .map(|e| json_of(e))
            .rfind(|v| v["kind"] == "remote_listen")
            .unwrap();
        assert_eq!(listen["plain"], true);
        let nonce = listen["nonce"].as_u64().unwrap();
        let listen_answer = |request: &str, yes: bool| {
            handle.page_message(
                ViewId::BASE,
                &format!(r#"{{"type":"remote_listen_answer","request_id":"{request}","nonce":{nonce},"yes":{yes}}}"#),
            );
        };
        let before = asked.borrow().len();
        listen_answer("l1", true);
        assert_eq!(asked.borrow().len(), before + 1, "a yes asks the window");
        assert!(said.borrow().is_empty(), "and covered, it is refused");
        covered.set(false);
        listen_answer("l2", true);
        assert_eq!(*said.borrow(), vec![true]);
    }

    #[test]
    fn a_waiting_question_is_shown_again_to_a_new_base_document() {
        let (handle, base, second) = handle_with_two_pages();
        let asked_at = Instant::now();
        handle
            .run(|state, out| {
                ask_listen(
                    state,
                    ListenAsk {
                        addr: "198.51.100.20".into(),
                        iface: "eth0".into(),
                        kind: IfaceKind::Plain,
                    },
                    asked_at,
                    Box::new(|_| {}),
                    out,
                )
            })
            .unwrap();
        let (enroll_nonce, _replies) = ask(&handle, PairMode::Direct);
        let listens = |sent: &[String]| -> Vec<serde_json::Value> {
            sent.iter()
                .map(|e| json_of(e))
                .filter(|v| v["kind"] == "remote_listen" || v["kind"] == "remote_enroll")
                .collect()
        };
        let first = listens(&base.sent.borrow());
        assert_eq!(first.len(), 2);
        // The document that was there missed them (it was not loaded yet, or it is reloaded): the new one is told.
        base.sent.borrow_mut().clear();
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r-again"}"#);
        let again = listens(&base.sent.borrow());
        let nonces: Vec<u64> = again.iter().map(|v| v["nonce"].as_u64().unwrap()).collect();
        assert_eq!(nonces.len(), 2, "{again:?}");
        assert_eq!(first[0]["kind"], "remote_listen");
        assert_eq!(nonces, [enroll_nonce, first[0]["nonce"].as_u64().unwrap()]);
        assert!(again
            .iter()
            .all(|v| v["expiresInMs"].as_u64().unwrap() <= QUESTION_WITHIN.as_millis() as u64));
        // Only the base page answers them, so only it is shown them.
        second.sent.borrow_mut().clear();
        handle.page_message(view(2), r#"{"type":"ready","request_id":"r-second"}"#);
        assert!(listens(&second.sent.borrow()).is_empty());
        // Past its time a question is not shown again.
        let late = Instant::now() + QUESTION_WITHIN;
        assert!(questions_for_new_document(&handle.state.borrow(), late).is_empty());
    }

    #[test]
    fn plain_interface_warns_and_defaults_no() {
        let (handle, base, _second) = handle_with_two_pages();
        let said = Rc::new(RefCell::new(Vec::new()));
        for (addr, kind) in [
            ("198.51.100.20", IfaceKind::Plain),
            ("203.0.113.2", IfaceKind::WireGuard),
        ] {
            let sink = said.clone();
            handle
                .run(|state, out| {
                    ask_listen(
                        state,
                        ListenAsk {
                            addr: addr.into(),
                            iface: "if0".into(),
                            kind,
                        },
                        Instant::now(),
                        Box::new(move |yes| sink.borrow_mut().push(yes)),
                        out,
                    )
                })
                .unwrap();
            let asked = base
                .sent
                .borrow()
                .iter()
                .map(|e| json_of(e))
                .rfind(|v| v["kind"] == "remote_listen")
                .unwrap();
            assert_eq!(asked["addr"], addr);
            assert_eq!(asked["link"], kind.as_str());
            assert_eq!(asked["plain"], kind == IfaceKind::Plain, "{addr}");
            // Nothing answered is a no: a question left past its time is answered no by the next one.
            let nonce = asked["nonce"].as_u64().unwrap();
            let late = Instant::now() + QUESTION_WITHIN;
            assert_eq!(
                answer_listen(&mut handle.state.borrow_mut(), nonce, true, late),
                Err(NO_LONGER_CURRENT.into())
            );
        }
        assert_eq!(*said.borrow(), vec![false, false]);
    }

    #[test]
    fn an_ssh_phone_may_only_watch() {
        let (handle, base, _second) = handle_with_two_pages();
        let (nonce, replies) = ask(&handle, PairMode::Ssh);
        let shown = base
            .sent
            .borrow()
            .iter()
            .map(|e| json_of(e))
            .find(|v| v["kind"] == "remote_enroll")
            .unwrap();
        assert_eq!(shown["grants"], serde_json::json!(["watch"]));
        answer(&handle, ViewId::BASE, "s1", nonce, true, "watch+steer");
        assert_eq!(
            result_for(&base.sent.borrow(), "s1"),
            Some(Err(SSH_ONLY_WATCHES.into()))
        );
        assert!(replies.borrow().is_empty(), "the question still waits");
        answer(&handle, ViewId::BASE, "s2", nonce, true, "watch");
        assert_eq!(*replies.borrow(), vec![EnrollReply::Yes(Grant::Watch)]);
    }

    #[test]
    fn one_phone_question_at_a_time_and_a_stale_one_is_no() {
        let (handle, _base, _second) = handle_with_two_pages();
        let (first, replies) = ask(&handle, PairMode::Direct);
        let err = handle.run(|state, out| {
            ask_enroll(
                state,
                EnrollAsk {
                    name: "Other".into(),
                    host: "h".into(),
                    peer: "p".into(),
                    mode: PairMode::Direct,
                    target: "directly to h:7341 (http)".into(),
                },
                Instant::now(),
                Box::new(|_| {}),
                out,
            )
        });
        assert_eq!(err, Err(ANOTHER_DEVICE_WAITS));
        let late = Instant::now() + QUESTION_WITHIN;
        assert_eq!(
            answer_enroll(&mut handle.state.borrow_mut(), first, true, Grant::Watch, late),
            Err(NO_LONGER_CURRENT.into())
        );
        assert_eq!(*replies.borrow(), vec![EnrollReply::No(THE_DESK_DID_NOT_ANSWER)]);
        assert_eq!(
            answer_enroll(
                &mut handle.state.borrow_mut(),
                first + 7,
                true,
                Grant::Watch,
                Instant::now()
            ),
            Err(NO_LONGER_CURRENT.into())
        );
    }

    #[test]
    fn an_unanswered_question_is_answered_no_at_its_limit() {
        let (handle, base, _second) = handle_with_two_pages();
        let asked_at = Instant::now();
        let (_, enroll) = ask(&handle, PairMode::Direct);
        let granted = Rc::new(RefCell::new(Vec::new()));
        let listened = Rc::new(RefCell::new(Vec::new()));
        let (g, l) = (granted.clone(), listened.clone());
        handle
            .run(|state, out| {
                ask_grant(
                    state,
                    GrantAsk {
                        device: "d1".into(),
                        name: "Pixel".into(),
                        from: Grant::Watch,
                        to: Grant::WatchSteer,
                    },
                    asked_at,
                    Box::new(move |r| g.borrow_mut().push(r)),
                    out,
                )?;
                ask_listen(
                    state,
                    ListenAsk {
                        addr: "203.0.113.2".into(),
                        iface: "wg0".into(),
                        kind: IfaceKind::WireGuard,
                    },
                    asked_at,
                    Box::new(move |yes| l.borrow_mut().push(yes)),
                    out,
                )
            })
            .expect("one of each kind may wait");
        let limits: Vec<serde_json::Value> = base
            .sent
            .borrow()
            .iter()
            .map(|e| json_of(e))
            .filter(|v| {
                matches!(
                    v["kind"].as_str(),
                    Some("remote_enroll" | "remote_grant" | "remote_listen")
                )
            })
            .map(|v| v["expiresInMs"].clone())
            .collect();
        let within = serde_json::json!(QUESTION_WITHIN.as_millis() as u64);
        assert_eq!(
            limits,
            vec![within.clone(), within.clone(), within],
            "each question says how long it waits"
        );

        // Before the limit nothing is answered.
        handle.run(|state, out| expire_questions(state, asked_at + QUESTION_WITHIN / 2, out));
        assert!(enroll.borrow().is_empty() && granted.borrow().is_empty() && listened.borrow().is_empty());

        // At it, every question is answered no, whoever asked is told, and the page is told to take it down.
        let shown_before = base.sent.borrow().len();
        let late = Instant::now() + QUESTION_WITHIN;
        handle.run(|state, out| expire_questions(state, late, out));
        assert_eq!(*enroll.borrow(), vec![EnrollReply::No(THE_DESK_DID_NOT_ANSWER)]);
        assert_eq!(*granted.borrow(), vec![EnrollReply::No(THE_DESK_DID_NOT_ANSWER)]);
        assert_eq!(*listened.borrow(), vec![false]);
        let closed: Vec<serde_json::Value> = base.sent.borrow()[shown_before..]
            .iter()
            .map(|e| json_of(e))
            .filter(|v| v["kind"] == "remote_question_closed")
            .collect();
        assert_eq!(closed.len(), 3, "{closed:?}");
        let desk = &handle.state.borrow().remote_desk;
        assert_eq!((desk.enroll_nonce(), desk.grant_nonce()), (None, None));
        assert!(desk.listen.is_none());
    }

    #[test]
    fn the_qr_code_is_square_rows_of_modules() {
        let rows = qr_rows("eitri-pair:v=1&c=BwcHBwcHBwcHBwcHBwcHBw&i=b4e1c09a7d3f5e21&m=ssh&h=desk&p=7341").unwrap();
        assert!(rows.len() >= 21 && (rows.len() - 17) % 4 == 0, "a QR code's width");
        assert!(rows
            .iter()
            .all(|r| r.len() == rows.len() && r.chars().all(|c| c == '0' || c == '1')));
        // The finder pattern's top row.
        assert!(rows[0].starts_with("1111111"));
    }

    fn revoke(handle: &AgentPanelHandle, from: ViewId, request: &str, device: &str, nonce: u64) {
        handle.page_message(
            from,
            &format!(r#"{{"type":"remote_revoke","request_id":"{request}","device":"{device}","nonce":{nonce}}}"#),
        );
    }

    #[test]
    fn revoke_needs_the_windows_painted_yes() {
        let (handle, base, second) = handle_with_two_pages();
        let covered = Rc::new(Cell::new(true));
        let asked = recording_gate(&handle, &[1], covered.clone());
        let revoked = Rc::new(Cell::new(0));
        let sink = revoked.clone();
        handle.run(|state, out| {
            ask_revoke(
                state,
                RevokeAsk {
                    device: "d1".into(),
                    name: "Pixel".into(),
                },
                Instant::now(),
                Box::new(move |()| sink.set(sink.get() + 1)),
                out,
            )
        });
        let shown = base
            .sent
            .borrow()
            .iter()
            .map(|e| json_of(e))
            .find(|v| v["kind"] == "remote_revoke_ask")
            .expect("the base page is asked");
        assert_eq!(
            (shown["device"].as_str(), shown["name"].as_str()),
            (Some("d1"), Some("Pixel"))
        );
        assert!(shown["expiresInMs"].as_u64().unwrap() > 0);
        let nonce = shown["nonce"].as_u64().unwrap();

        // Another page's yes never reaches the window or the question.
        revoke(&handle, view(2), "v2", "d1", nonce);
        assert!(asked.borrow().is_empty());
        assert_eq!(
            result_for(&second.sent.borrow(), "v2"),
            Some(Err(REMOTE_QUESTIONS_ARE_THE_BASE_PAGES.into()))
        );

        // A covered base page's yes is refused by the window, and the question keeps its nonce.
        revoke(&handle, ViewId::BASE, "v1", "d1", nonce);
        assert_eq!(*asked.borrow(), vec![ViewId::BASE]);
        assert!(matches!(result_for(&base.sent.borrow(), "v1"), Some(Err(_))));
        assert_eq!(revoked.get(), 0);
        assert_eq!(handle.state.borrow().remote_desk.revoke_nonce(), Some(nonce));

        // Uncovered: a yes about another device, or under another nonce, answers nothing.
        covered.set(false);
        revoke(&handle, ViewId::BASE, "v3", "d2", nonce);
        assert_eq!(
            result_for(&base.sent.borrow(), "v3"),
            Some(Err(NOT_THE_DEVICE_ASKED.into()))
        );
        revoke(&handle, ViewId::BASE, "v4", "d1", nonce + 5);
        assert_eq!(
            result_for(&base.sent.borrow(), "v4"),
            Some(Err(NO_LONGER_CURRENT.into()))
        );
        assert_eq!(revoked.get(), 0);

        // The yes to the question shown is taken, once.
        revoke(&handle, ViewId::BASE, "v5", "d1", nonce);
        assert_eq!(result_for(&base.sent.borrow(), "v5"), Some(Ok(())));
        assert_eq!(revoked.get(), 1);
        revoke(&handle, ViewId::BASE, "v6", "d1", nonce);
        assert_eq!(
            result_for(&base.sent.borrow(), "v6"),
            Some(Err(NO_LONGER_CURRENT.into()))
        );
        assert_eq!(revoked.get(), 1);

        // Past its time the question is taken down and answers nothing.
        handle.run(|state, out| {
            ask_revoke(
                state,
                RevokeAsk {
                    device: "d1".into(),
                    name: "Pixel".into(),
                },
                Instant::now(),
                Box::new(|()| panic!("a late yes revokes nothing")),
                out,
            )
        });
        let late = Instant::now() + QUESTION_WITHIN;
        let nonce = handle.state.borrow().remote_desk.revoke_nonce().unwrap();
        assert_eq!(
            answer_revoke(&mut handle.state.borrow_mut(), nonce, "d1", late),
            Err(NO_LONGER_CURRENT.into())
        );
        assert_eq!(handle.state.borrow().remote_desk.revoke_nonce(), None);
    }

    #[test]
    fn the_device_list_is_asked_for_only_while_remote_control_is_on() {
        let (handle, base, second) = handle_with_two_pages();
        let open = |from: ViewId, request: &str| {
            handle.page_message(
                from,
                &format!(r#"{{"type":"remote_devices_open","request_id":"{request}"}}"#),
            );
        };
        open(ViewId::BASE, "o1");
        assert_eq!(result_for(&base.sent.borrow(), "o1"), Some(Err(REMOTE_OFF.into())));
        assert!(base.sent.borrow().iter().all(|e| json_of(e)["kind"] != "remote_on"));
        assert!(remote_on_for_new_document(&handle.state.borrow(), ViewId::BASE).is_none());
        assert!(take_requests(&mut handle.state.borrow_mut()).is_empty());

        handle.run(|state, out| switch_on(state, out));
        for (page, told) in [(&base, 1), (&second, 0)] {
            assert_eq!(
                page.sent
                    .borrow()
                    .iter()
                    .filter(|e| json_of(e)["kind"] == "remote_on")
                    .count(),
                told,
                "the base page is told once, an extra view never"
            );
        }
        handle.run(|state, out| switch_on(state, out));
        assert_eq!(
            base.sent
                .borrow()
                .iter()
                .filter(|e| json_of(e)["kind"] == "remote_on")
                .count(),
            1
        );
        assert!(remote_on_for_new_document(&handle.state.borrow(), ViewId::BASE).is_some());
        assert!(
            remote_on_for_new_document(&handle.state.borrow(), view(2)).is_none(),
            "an extra view's new document does not offer the device list's key"
        );

        open(view(2), "o2");
        assert_eq!(
            result_for(&second.sent.borrow(), "o2"),
            Some(Err(REMOTE_IS_THE_BASE_PAGES.into()))
        );
        open(ViewId::BASE, "o3");
        assert_eq!(result_for(&base.sent.borrow(), "o3"), Some(Ok(())));
        assert_eq!(take_requests(&mut handle.state.borrow_mut()), vec![DeskRequest::List]);
        for n in 0..=DESK_REQUESTS_MAX {
            open(ViewId::BASE, &format!("f{n}"));
        }
        assert_eq!(
            result_for(&base.sent.borrow(), &format!("f{DESK_REQUESTS_MAX}")),
            Some(Err(DESK_BUSY.into()))
        );
        assert_eq!(take_requests(&mut handle.state.borrow_mut()).len(), DESK_REQUESTS_MAX);
    }
}
