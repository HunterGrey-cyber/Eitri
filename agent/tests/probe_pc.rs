//! Probe PC: a Claude chat moved from one process to another, mid-turn at a waiting card and idle, with the real
//! sidecar and the real `claude`, exactly as two windows move one: the source's conversation frozen and its provider
//! frozen through the face every provider moves by, the records carried as JSON and the descriptors over a unix
//! socket, the destination taking the provider up by the kind the record names, rebuilding the conversation by
//! folding, and committing.
//!
//! Real sidecar, real `claude`, real tokens, so `#[ignore]`d. Run it under the test profile, which pins both the
//! account and the CLI binary the sidecar starts:
//! `cargo test -p agent --test probe_pc -- --ignored --nocapture --test-threads=1`.
//!
//! The source (A) is a process of its own, this test binary started again as `move_source_helper`; the destination
//! (B) is the test itself. Each clause prints its verdict before the test asserts on all of them. The clauses that
//! need a window's tab set, its turn review or the editor are not run here (their own tests drive them with
//! stand-ins); the record names each of them as not run.
#![cfg(target_os = "linux")]

use agent::chat_mover::{self, MovedKind};
use agent::lease::SessionLease;
use agent::setting_sources::ProjectTrust;
use agent::{
    AgentConversation, AgentDomainEvent, AgentProvider, ChatMoveCore, ClaudeSidecarProvider, ConversationHandoff,
    MovedProvider, PermissionDecision, ProviderCapabilities, ProviderInfo, UiDelivery,
};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Set, to the project directory, in the environment of the source process this file starts; nothing else sets it.
const HELPER_VAR: &str = "PC_SOURCE_PROJECT";
/// The test the source process runs.
const HELPER_TEST: &str = "move_source_helper";

/// From the turn's start to the second card: one file written, then the model asks for its next tool.
const SECOND_CARD_BUDGET: Duration = Duration::from_secs(300);
/// From the answers in the destination to the turn's end: the command runs, the second file is written, a reply.
const TURN_END_BUDGET: Duration = Duration::from_secs(300);
/// How long any one wait on something local (a commit, a process's exit, a file) may take before it is called hung.
const HANG: Duration = Duration::from_secs(60);
/// The most descriptors a move of a Claude chat brings: the sidecar's three and the session lease.
const MOVE_FDS: usize = 4;
/// How many turns the synthetic chat the freeze and the seed are measured on holds.
const MEASURED_TURNS: usize = 200;

const PROMPT: &str = "Do exactly these three steps, one tool call each, in this order, and nothing else. \
                      1. Use the Write tool to create the file a.txt in the current directory containing the word one. \
                      2. Use the Bash tool to run exactly: sleep 2 \
                      3. Use the Write tool to create the file b.txt in the current directory containing the word two. \
                      Then reply with the single word done.";

// ---------------------------------------------------------------------------------------------
// Clocks, descriptors and frames between the two processes.
// ---------------------------------------------------------------------------------------------

/// `CLOCK_MONOTONIC` in nanoseconds: one clock both processes of the probe read.
fn now_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `clock_gettime` writes into the struct it is given.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// Room for one control message carrying up to `MOVE_FDS` descriptors, aligned for `cmsghdr`.
type ControlBuffer = [u64; 16];

fn send_fds(socket: RawFd, fds: &[RawFd]) -> std::io::Result<()> {
    let mut byte = [b'F'];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: byte.len(),
    };
    let mut control: ControlBuffer = [0; 16];
    let bytes = std::mem::size_of_val(fds) as u32;
    // SAFETY: `CMSG_SPACE` only computes a size.
    let space = unsafe { libc::CMSG_SPACE(bytes) } as usize;
    assert!(
        space <= std::mem::size_of_val(&control),
        "the descriptors fit the buffer"
    );
    // SAFETY: an all-zero `msghdr` is a valid empty message; every pointer set below outlives the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: `msg_control` points at `space` writable, aligned bytes, so the first header exists and its data has
    // room for every descriptor.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(bytes) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(cmsg).cast::<RawFd>(), fds.len());
    }
    // SAFETY: `msg` is fully initialised above.
    if unsafe { libc::sendmsg(socket, &msg, libc::MSG_NOSIGNAL) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn recv_fds(socket: RawFd) -> std::io::Result<Vec<OwnedFd>> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: byte.len(),
    };
    let mut control: ControlBuffer = [0; 16];
    // SAFETY: as in `send_fds`.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&control) as _;
    // SAFETY: `msg` points at buffers that live for the call; what arrives is close-on-exec.
    let received = unsafe { libc::recvmsg(socket, &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if received < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if received == 0 {
        return Err(std::io::Error::other(
            "the socket closed before the descriptors arrived",
        ));
    }
    let mut fds = Vec::new();
    // SAFETY: `recvmsg` filled `msg`, so its control headers are walkable; the data of an `SCM_RIGHTS` header is
    // descriptors this process now owns.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() || (*cmsg).cmsg_level != libc::SOL_SOCKET || (*cmsg).cmsg_type != libc::SCM_RIGHTS {
            return Err(std::io::Error::other("the message carried no descriptor"));
        }
        let data = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
        let count = (data / std::mem::size_of::<RawFd>()).min(MOVE_FDS);
        for at in 0..count {
            let fd = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>().add(at));
            fds.push(OwnedFd::from_raw_fd(fd));
        }
    }
    Ok(fds)
}

fn send_frame(stream: &mut UnixStream, bytes: &[u8]) -> std::io::Result<()> {
    stream.write_all(&(bytes.len() as u64).to_le_bytes())?;
    stream.write_all(bytes)
}

fn recv_frame(stream: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 8];
    stream.read_exact(&mut len)?;
    let len = u64::from_le_bytes(len) as usize;
    if len > 256 << 20 {
        return Err(std::io::Error::other("a frame past the probe's cap"));
    }
    let mut bytes = vec![0u8; len];
    stream.read_exact(&mut bytes)?;
    Ok(bytes)
}

// ---------------------------------------------------------------------------------------------
// The source process.
// ---------------------------------------------------------------------------------------------

/// The source process: starts a session, runs the turn until a second card waits after the first file was written,
/// freezes the chat there and sends it to the destination on its stdin (a unix socket), lets it go once the
/// destination has committed, and exits. Run with nothing in `PC_SOURCE_PROJECT`, it does nothing, so a plain
/// `--ignored` run passes it.
#[test]
#[ignore = "the source half of probe PC; started by the probe below, never on its own"]
fn move_source_helper() {
    let Some(project) = std::env::var_os(HELPER_VAR) else {
        return;
    };
    // SAFETY: fd 0 is the socket the probe connected this process's stdin to, and nothing else here uses it.
    let mut control = unsafe { UnixStream::from_raw_fd(0) };
    if let Err(why) = source_half(Path::new(&project), &mut control) {
        let _ = send_frame(&mut control, json!({ "error": why }).to_string().as_bytes());
    }
    std::process::exit(0);
}

/// The conversation frozen for a move, asked again while its ingest folds a batch or writes after one, as a window
/// asks again on its next tick: the freeze itself never waits for the ingest.
fn freeze_settled(conversation: &mut AgentConversation) -> Result<(ChatMoveCore, ConversationHandoff), String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match conversation.freeze_for_move(|c| {
            let _ = c.take_ui_delivery();
            Ok(())
        }) {
            Err(agent::HandoffRefused::Folding) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(2))
            }
            other => return other.map_err(|e| e.to_string()),
        }
    }
}

fn pending_ids(conversation: &AgentConversation) -> Vec<String> {
    conversation.projection().pending_permissions.keys().cloned().collect()
}

fn source_half(project: &Path, control: &mut UnixStream) -> Result<(), String> {
    let provider = ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string())
        .map_err(|e| format!("the sidecar does not start: {e}"))?;
    let mut conversation = AgentConversation::create(Arc::new(provider), project, ProjectTrust::Untrusted)
        .map_err(|e| format!("the session does not open: {e}"))?;
    conversation
        .send_turn(PROMPT)
        .map_err(|e| format!("the turn does not start: {e}"))?;
    let started = Instant::now();
    let mut allowed = BTreeSet::new();
    let waiting = loop {
        if started.elapsed() > SECOND_CARD_BUDGET {
            return Err("no second card came within the budget".into());
        }
        let _ = conversation.take_ui_delivery();
        let pending = pending_ids(&conversation);
        let wrote = project.join("a.txt").exists();
        if let Some(card) = pending.iter().find(|id| !allowed.contains(*id)) {
            if wrote {
                break card.clone();
            }
            conversation
                .respond_permission(card, PermissionDecision::Allow)
                .map_err(|e| format!("the first card was not answered: {e}"))?;
            allowed.insert(card.clone());
        }
        if conversation.projection().active_turn_id.is_none() && !allowed.is_empty() {
            return Err("the turn ended before a second card".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    let frozen_at = now_ns();
    let (core, handoff) =
        freeze_settled(&mut conversation).map_err(|e| format!("the conversation does not freeze: {e}"))?;
    let projection = serde_json::to_value(&*conversation.projection()).map_err(|e| e.to_string())?;
    let mover = conversation.mover().ok_or("the sidecar's provider does not move")?;
    let frozen = mover.freeze_for_chat_move()?;
    let lease = conversation
        .lease_descriptor()
        .ok_or("the session holds no lease")?
        .map_err(|e| format!("the lease's descriptor: {e}"))?;
    let seed_bytes = serde_json::to_vec(&core).map_err(|e| e.to_string())?.len();
    let record = json!({
        "core": core,
        "handoff": handoff,
        "kind": frozen.kind.wire(),
        "provider": frozen.record,
        "projection": projection,
        "card": waiting,
        "frozen_at_ns": frozen_at,
        "sent_at_ns": now_ns(),
        "seed_bytes": seed_bytes,
    });
    let copies = frozen.copies().map_err(|e| e.to_string())?;
    let raw: Vec<RawFd> = copies
        .iter()
        .map(AsRawFd::as_raw_fd)
        .chain(Some(lease.as_raw_fd()))
        .collect();
    send_fds(control.as_raw_fd(), &raw).map_err(|e| format!("SCM_RIGHTS: {e}"))?;
    send_frame(control, record.to_string().as_bytes()).map_err(|e| e.to_string())?;
    drop(copies);
    drop(lease);
    let mut committed = [0u8; 1];
    control
        .read_exact(&mut committed)
        .map_err(|e| format!("the destination never said it committed: {e}"))?;
    // The destination holds the chat: this process lets it go without ending anything, and exits normally.
    mover.forget_chat_move();
    drop(frozen);
    drop(conversation);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// The destination: the probe.
// ---------------------------------------------------------------------------------------------

struct Helper(Child);

impl Drop for Helper {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

#[derive(Default)]
struct Verdict {
    lines: Vec<String>,
    failed: Vec<String>,
}

impl Verdict {
    fn check(&mut self, clause: &str, passed: bool, detail: impl Into<String>) {
        let detail = detail.into();
        self.lines
            .push(format!("{} {clause}: {detail}", if passed { "PASS" } else { "FAIL" }));
        if !passed {
            self.failed.push(clause.to_owned());
        }
    }
    fn measured(&mut self, what: &str, value: String) {
        self.lines.push(format!("MEASURED {what}: {value}"));
    }
    fn not_run(&mut self, clause: &str, why: &str) {
        self.lines.push(format!("NOT RUN {clause}: {why}"));
    }
}

/// The fields of `ours` that differ from `theirs`, by name.
fn differing(theirs: &Value, ours: &Value) -> Vec<String> {
    let (Some(theirs), Some(ours)) = (theirs.as_object(), ours.as_object()) else {
        return vec!["<not objects>".into()];
    };
    let names: BTreeSet<&String> = theirs.keys().chain(ours.keys()).collect();
    names
        .into_iter()
        .filter(|name| theirs.get(*name) != ours.get(*name))
        .cloned()
        .collect()
}

fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 checks for the process and sends nothing.
    pid > 1 && unsafe { libc::kill(pid as libc::pid_t, 0) } == 0
}

/// Drains `conversation`, answering every card it holds with allow, until its turn ends or `budget` runs out;
/// whether the turn ended, and when the first event was delivered (`now_ns`).
fn run_to_turn_end(conversation: &mut AgentConversation, budget: Duration) -> (bool, Option<u64>) {
    let started = Instant::now();
    let mut answered = BTreeSet::new();
    let mut first = None;
    loop {
        if let UiDelivery::Events(events) = conversation.take_ui_delivery() {
            if !events.is_empty() && first.is_none() {
                first = Some(now_ns());
            }
            if events
                .iter()
                .any(|e| matches!(e, AgentDomainEvent::TurnCompleted { .. }))
            {
                return (true, first);
            }
        }
        for id in pending_ids(conversation) {
            if answered.insert(id.clone()) {
                let _ = conversation.respond_permission(&id, PermissionDecision::Allow);
            }
        }
        if started.elapsed() > budget {
            return (false, first);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// The idle half: `conversation` frozen idle and taken up again in this process by its kind, as another window would.
/// The commit is timed; the chat is driven on by a short turn.
fn idle_move(conversation: AgentConversation, project: &Path, verdict: &mut Verdict) -> Option<AgentConversation> {
    let mut conversation = conversation;
    let frozen_at = Instant::now();
    let (core, handoff) = match freeze_settled(&mut conversation) {
        Ok(taken) => taken,
        Err(e) => {
            verdict.check("an idle chat freezes", false, e);
            return None;
        }
    };
    let conversation_frozen = frozen_at.elapsed();
    // The ingest writes a turn's history right after it folds the turn's end, so a move just after it usually finds
    // it written already; the record says which this run found.
    verdict.measured(
        "the idle move's `history_dirty` at the freeze (the turn ended just before)",
        handoff.history_dirty.to_string(),
    );
    let mover = conversation.mover().expect("a sidecar chat moves");
    let provider_at = Instant::now();
    let frozen = match mover.freeze_for_chat_move() {
        Ok(frozen) => frozen,
        Err(why) => {
            verdict.check("an idle chat's sidecar is handed over", false, why);
            return None;
        }
    };
    let lease = conversation.lease_descriptor().and_then(Result::ok);
    let session = handoff.provider_session_id.clone().unwrap_or_default();
    let conversation_id = handoff.conversation_id.clone();
    let cwd = handoff.canonical_cwd.to_string_lossy().into_owned();
    let provider_frozen = provider_at.elapsed();
    let record: Value = serde_json::from_str(&frozen.record.to_string()).unwrap();
    let kind = MovedKind::from_wire(frozen.kind.wire()).unwrap();
    let adopt_at = Instant::now();
    let adopted = chat_mover::adopt(kind, &record, frozen.copies().unwrap()).expect("the idle chat is taken up");
    let lease = lease.map(|fd| SessionLease::from_moved(fd, &handoff.provider_name, &cwd, &session).unwrap());
    let mut taken = AgentConversation::adopt_moved(
        core,
        handoff,
        MovedProvider {
            agent: adopted.agent,
            external: None,
            lease,
        },
    )
    .expect("the idle conversation is rebuilt");
    let adopted_in = adopt_at.elapsed();
    let commit_at = Instant::now();
    let (done, answered) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        let taken = &taken;
        scope.spawn(move || {
            let _ = done.send(taken.mover().unwrap().commit_chat_move().is_ok());
        });
        let committed = answered.recv_timeout(HANG);
        verdict.check(
            "an idle chat whose last event is S commits with no new event from the sidecar",
            committed == Ok(true),
            format!("{committed:?} in {:?}", commit_at.elapsed()),
        );
    });
    verdict.measured(
        "the idle move, step by step (conversation frozen / sidecar handed over / taken up / committed)",
        format!(
            "{:.1} / {:.1} / {:.1} / {:.1} ms",
            conversation_frozen.as_secs_f64() * 1e3,
            provider_frozen.as_secs_f64() * 1e3,
            adopted_in.as_secs_f64() * 1e3,
            commit_at.elapsed().as_secs_f64() * 1e3
        ),
    );
    let _ = taken.commit_moved();
    mover.forget_chat_move();
    drop(frozen);
    drop(conversation);
    let wrote = (0..(HANG.as_millis() / 100)).any(|_| {
        let written = agent::history::store::load(&conversation_id, &session).is_ok_and(|h| h.item_count() > 0);
        if !written {
            std::thread::sleep(Duration::from_millis(100));
        }
        written
    });
    verdict.check(
        "the moved turn's history is on disk once the destination holds the chat",
        wrote,
        format!("history of {session} under {}", project.display()),
    );
    Some(taken)
}

#[test]
#[ignore = "spends real Claude tokens; run under with --ignored"]
fn a_claude_chat_moves_between_two_processes_mid_turn_at_a_card_and_idle() {
    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("probe_pc-{}", uuid::Uuid::new_v4()));
    let project = scratch.join("project");
    std::fs::create_dir_all(&project).expect("the scratch project");
    let project = project.canonicalize().expect("the project resolves");
    let mut verdict = Verdict::default();

    // ---- the mid-turn move, from the source process.
    let (mut ours, theirs) = UnixStream::pair().expect("a socket pair");
    let exe = std::env::current_exe().expect("the test binary");
    let child = Command::new(exe)
        .args([HELPER_TEST, "--exact", "--ignored", "--nocapture", "--test-threads=1"])
        .env(HELPER_VAR, &project)
        .stdin(Stdio::from(OwnedFd::from(theirs)))
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("the source process starts");
    let mut source = Helper(child);
    ours.set_read_timeout(Some(SECOND_CARD_BUDGET + HANG)).unwrap();
    let fds = recv_fds(ours.as_raw_fd());
    let record: Value = recv_frame(&mut ours)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null);
    if let Some(error) = record.get("error") {
        panic!("the source could not move its chat: {error}");
    }
    let fds = fds.expect("the descriptors arrive");
    let received_at = now_ns();
    verdict.check(
        "the move brings the sidecar's three descriptors and the lease",
        fds.len() == MOVE_FDS,
        format!("{} descriptors", fds.len()),
    );
    let frozen_at = record["frozen_at_ns"].as_u64().unwrap_or_default();
    verdict.measured(
        "the source's freeze (stop folding, last pump, records, sidecar parked)",
        format!(
            "{:.1} ms",
            (record["sent_at_ns"]
                .as_u64()
                .unwrap_or_default()
                .saturating_sub(frozen_at)) as f64
                / 1e6
        ),
    );
    verdict.measured(
        "the mid-turn chat's seed and running turn",
        format!("{} bytes", record["seed_bytes"]),
    );
    let core: ChatMoveCore = serde_json::from_value(record["core"].clone()).expect("the conversation's core");
    let handoff: ConversationHandoff =
        serde_json::from_value(record["handoff"].clone()).expect("the conversation's handoff");
    let kind = MovedKind::from_wire(record["kind"].as_str().unwrap_or_default()).expect("a kind this build moves");
    let session = handoff.provider_session_id.clone().expect("the session's id");
    let cwd = handoff.canonical_cwd.to_string_lossy().into_owned();
    let mut fds = fds.into_iter();
    let provider_fds: Vec<OwnedFd> = fds.by_ref().take(3).collect();
    let lease = SessionLease::from_moved(fds.next().expect("the lease"), &handoff.provider_name, &cwd, &session)
        .expect("the lease is this session's");
    let lease_path = lease.path().to_path_buf();
    let adopted = chat_mover::adopt(kind, &record["provider"], provider_fds).expect("the sidecar is taken over");
    let mut b = AgentConversation::adopt_moved(
        core,
        handoff,
        MovedProvider {
            agent: adopted.agent,
            external: None,
            lease: Some(lease),
        },
    )
    .expect("the conversation is rebuilt by folding");
    let ours_projection = serde_json::to_value(&*b.projection()).unwrap();
    let theirs_projection = &record["projection"];
    let differs = differing(theirs_projection, &ours_projection);
    verdict.check(
        "B shows the same projection as A, field by field through Serialize",
        differs.is_empty(),
        if differs.is_empty() {
            "every field equal".to_string()
        } else {
            format!("differs in {differs:?}")
        },
    );
    verdict.check(
        "B's last_revision equals A's, and every running-turn record keeps its seq",
        ours_projection["last_revision"] == theirs_projection["last_revision"]
            && ours_projection["transcript"] == theirs_projection["transcript"]
            && ours_projection["tool_calls"] == theirs_projection["tool_calls"],
        format!(
            "last_revision {} / {}",
            ours_projection["last_revision"], theirs_projection["last_revision"]
        ),
    );
    verdict.check(
        "after adoption the projection's status is Running",
        ours_projection["status"]["kind"] == "running",
        format!("{}", ours_projection["status"]),
    );
    let card = record["card"].as_str().unwrap_or_default().to_owned();
    verdict.check(
        "the card that waited in A waits in B under the same id",
        b.projection().pending_permissions.contains_key(&card),
        card.clone(),
    );

    b.mover()
        .expect("the adopted provider moves too")
        .commit_chat_move()
        .expect("the commit starts the watch");
    let committed_at = now_ns();
    let _ = b.commit_moved();
    verdict.measured(
        "frozen from the source's freeze to the destination's commit",
        format!("{:.1} ms", committed_at.saturating_sub(frozen_at) as f64 / 1e6),
    );
    verdict.measured(
        "on the link and adopting (receive to commit)",
        format!("{:.1} ms", committed_at.saturating_sub(received_at) as f64 / 1e6),
    );
    let owner = std::fs::read_to_string(&lease_path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| v["owner_pid"].as_u64());
    verdict.check(
        "the lease names the destination after the commit",
        owner == Some(u64::from(std::process::id())),
        format!("owner_pid {owner:?}, this process {}", std::process::id()),
    );
    ours.write_all(b"C").expect("the source is told");
    let exited = (0..(HANG.as_millis() / 50)).any(|_| {
        let gone = matches!(source.0.try_wait(), Ok(Some(_)));
        if !gone {
            std::thread::sleep(Duration::from_millis(50));
        }
        gone
    });
    verdict.check("A exits once B holds the chat", exited, "");
    let sidecar_pid = record["provider"]["child"]["pid"].as_u64().unwrap_or_default() as u32;
    verdict.check(
        "the sidecar outlives A",
        pid_alive(sidecar_pid),
        format!("pid {sidecar_pid}"),
    );
    let socket = PathBuf::from(record["provider"]["socket_path"].as_str().unwrap_or_default());
    verdict.check(
        "after A's normal drop the sidecar's socket is still there and answers a new connection",
        socket.exists() && UnixStream::connect(&socket).is_ok(),
        socket.display().to_string(),
    );
    let (ended, first_event) = run_to_turn_end(&mut b, TURN_END_BUDGET);
    if let Some(first) = first_event {
        verdict.measured(
            "frozen from the source's freeze to the first event delivered after the commit",
            format!("{:.1} ms", first.saturating_sub(frozen_at) as f64 / 1e6),
        );
    }
    verdict.check("B answers the card and the turn completes in B", ended, "");
    verdict.check(
        "the turn's edits from before and after the move are both made",
        project.join("a.txt").exists() && project.join("b.txt").exists(),
        "a.txt and b.txt",
    );

    // ---- the idle move, right after the turn's end.
    if let Some(mut c) = idle_move(b, &project, &mut verdict) {
        c.send_turn("Reply with the single word ok.")
            .expect("the moved chat takes a turn");
        let (ended, _) = run_to_turn_end(&mut c, TURN_END_BUDGET);
        verdict.check("the idle-moved chat runs its next turn", ended, "");
        c.shutdown();
    }

    // ---- a resumed session whose resume was rejected, moved idle.
    resumed_unavailable(&project, &mut verdict);

    // ---- the freeze and the seed measured on a long chat.
    measure_long_chat(&project, &mut verdict);

    for (clause, why) in [
        (
            "B's turn review of the moved turn equals the same turn run without a move",
            "needs the window's turn review; `core` tab-set tests hand it over with stand-ins",
        ),
        (
            "a card approved in A in Auto just before the move is not asked again in B",
            "needs the window's gate and tab set (`human_allowed`); `core`'s tab-set move tests carry it",
        ),
        (
            "A started under `script -c` and its pty hung up: the sidecar survives",
            "the stand-in sidecar test `a_moved_sidecar_survives_the_spawners_terminal_hangup_and_sigint` runs it",
        ),
        ("a turn of 5,000 events moves", "not run against the real CLI: its cost"),
        (
            "B's first tick and delivery keep the moved approvals and candidates; a bypass answer is made once, by A",
            "needs the window's tab set; `core`'s tab-set move tests carry it",
        ),
        (
            "a move between two text messages keeps two; a move refused while a steer is open",
            "the stand-in sidecar tests run both; the real CLI's timing cannot be steered here",
        ),
        (
            "no end snapshot in A and A's running file gone after move_done",
            "needs the window's turn review and presence; the panel's and tab set's tests run it",
        ),
        (
            "a hunk reverted in nvim during the freeze reaches B's draft",
            "needs the editor; the pair's review test runs it",
        ),
    ] {
        verdict.not_run(clause, why);
    }

    drop(source);
    let _ = std::fs::remove_dir_all(&scratch);
    eprintln!("probe_pc: ---- record ----");
    for line in &verdict.lines {
        eprintln!("probe_pc: {line}");
    }
    let overall = if verdict.failed.is_empty() { "PASS" } else { "FAIL" };
    eprintln!("probe_pc: PC verdict: {overall}");
    assert!(verdict.failed.is_empty(), "probe PC failed: {:?}", verdict.failed);
}

/// A resume of a session the CLI does not have: its rejected verdict arrives after its opening, and the chat, moved
/// idle, shows it unavailable in the destination.
fn resumed_unavailable(project: &Path, verdict: &mut Verdict) {
    let provider = match ClaudeSidecarProvider::connect(&uuid::Uuid::new_v4().to_string()) {
        Ok(provider) => provider,
        Err(e) => return verdict.not_run("a rejected resume moved idle", &format!("no second sidecar: {e}")),
    };
    let missing = uuid::Uuid::new_v4().to_string();
    let mut conversation =
        match AgentConversation::resume(Arc::new(provider), project, &missing, ProjectTrust::Untrusted) {
            Ok(conversation) => conversation,
            Err(e) => {
                return verdict.not_run(
                    "a rejected resume moved idle",
                    &format!("the resume was refused before it opened: {e}"),
                )
            }
        };
    let started = Instant::now();
    while !matches!(
        conversation.projection().status,
        agent::ProjectionStatus::Unavailable { .. } | agent::ProjectionStatus::Closed { .. }
    ) {
        let _ = conversation.take_ui_delivery();
        if started.elapsed() > HANG {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let before = serde_json::to_value(&conversation.projection().status).unwrap();
    let frozen = conversation.freeze_for_move(|c| {
        let _ = c.take_ui_delivery();
        Ok(())
    });
    match frozen {
        Ok((core, handoff)) => {
            let rebuilt = agent::AgentSessionProjection::adopt(&core).map(|p| serde_json::to_value(&p.status).unwrap());
            verdict.check(
                "a resumed session whose rejected ResumeOutcome came after its SessionOpened shows Unavailable once rebuilt",
                rebuilt.as_ref().is_ok_and(|status| status["kind"] == "unavailable" && *status == before),
                format!("source {before}, rebuilt {rebuilt:?}"),
            );
            drop(handoff);
            conversation.unfreeze();
        }
        Err(e) => verdict.not_run(
            "a rejected resume moved idle",
            &format!("the source {before} refuses to freeze: {e}"),
        ),
    }
    conversation.shutdown();
}

/// A provider for the synthetic long chat: it hands the conversation what it is given, and nothing else.
#[derive(Default)]
struct Scripted {
    queued: Mutex<Vec<AgentDomainEvent>>,
}

impl AgentProvider for Scripted {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::default()
    }
    fn info(&self) -> ProviderInfo {
        ProviderInfo::default()
    }
    fn create_session(&self, _: agent::CreateSessionRequest) -> Result<String, agent::ProviderError> {
        Ok("long-session".into())
    }
    fn resume_session(&self, _: agent::ResumeSessionRequest) -> Result<String, agent::ProviderError> {
        Err(agent::ProviderError::UnsupportedCapability("resume"))
    }
    fn send_turn(&self, _: agent::SendTurnRequest) -> Result<String, agent::ProviderError> {
        Ok("turn".into())
    }
    fn interrupt_turn(&self, _: agent::InterruptTurnRequest) -> Result<(), agent::ProviderError> {
        Ok(())
    }
    fn resolve_permission(&self, _: agent::ResolvePermissionRequest) -> Result<(), agent::ProviderError> {
        Ok(())
    }
    fn close_session(&self, _: agent::CloseSessionRequest) -> Result<(), agent::ProviderError> {
        Ok(())
    }
    fn pump(&self) -> Vec<AgentDomainEvent> {
        std::mem::take(&mut *self.queued.lock().unwrap())
    }
}

/// The freeze's time and the seed's size for a chat of `MEASURED_TURNS` turns, folded in this process from scripted
/// events (no CLI is asked for 200 turns): what the freeze costs is the conversation's, whatever made its events.
fn measure_long_chat(project: &Path, verdict: &mut Verdict) {
    let provider = Arc::new(Scripted::default());
    let mut conversation =
        AgentConversation::create(provider.clone(), project, ProjectTrust::Untrusted).expect("a scripted session");
    let mut events = vec![AgentDomainEvent::SessionOpened {
        session_id: "long-session".into(),
        provider_session_id: uuid::Uuid::new_v4().to_string(),
        model: "m".into(),
        cwd: project.to_string_lossy().into_owned(),
    }];
    for turn in 0..MEASURED_TURNS {
        let id = format!("t{turn}");
        events.push(AgentDomainEvent::UserPromptSubmitted {
            text: format!("question {turn}: what does line {turn} of the file do?"),
        });
        events.push(AgentDomainEvent::TurnStarted { turn_id: id.clone() });
        events.push(AgentDomainEvent::ContentDelta {
            turn_id: id.clone(),
            kind: agent::ContentKind::Text,
            text: "It reads the configuration, checks each entry against the schema and returns the first error. "
                .repeat(4),
        });
        events.push(AgentDomainEvent::TurnCompleted {
            turn_id: id,
            outcome: agent::TurnOutcome::Completed,
            result_text: String::new(),
            stop_reason: None,
            usage: None,
            detail: Default::default(),
        });
    }
    let expected = events.len() as u64;
    provider.queued.lock().unwrap().extend(events);
    let started = Instant::now();
    while conversation.projection().last_revision < expected {
        let _ = conversation.take_ui_delivery();
        if started.elapsed() > HANG {
            return verdict.not_run("the long chat's freeze", "its events were not folded in time");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = conversation.take_ui_delivery();
    // Timed per attempt: an attempt refused while the ingest writes after its last batch changes nothing, and only
    // the one that froze is the freeze's cost.
    let deadline = Instant::now() + Duration::from_secs(5);
    let (frozen, elapsed) = loop {
        let at = Instant::now();
        let attempt = conversation.freeze_for_move(|c| {
            let _ = c.take_ui_delivery();
            Ok(())
        });
        let elapsed = at.elapsed();
        match attempt {
            Err(agent::HandoffRefused::Folding) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(2))
            }
            other => break (other.map_err(|e| e.to_string()), elapsed),
        }
    };
    match frozen {
        Ok((core, _handoff)) => {
            let bytes = serde_json::to_vec(&core).map(|b| b.len()).unwrap_or_default();
            verdict.measured(
                &format!("the freeze of a {MEASURED_TURNS}-turn chat (scripted events, folded here)"),
                format!("{:.2} ms", elapsed.as_secs_f64() * 1e3),
            );
            verdict.measured(
                &format!("the seed of a {MEASURED_TURNS}-turn chat as JSON"),
                format!("{bytes} bytes"),
            );
        }
        Err(e) => verdict.not_run("the long chat's freeze", &e),
    }
}
