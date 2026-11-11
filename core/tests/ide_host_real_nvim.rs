#![cfg(target_os = "linux")]
//! `IdeHost` against a real `nvim`, over an `NvimLink` as the terminal `claude`'s views would run: events made by hand,
//! the person's keys through `NvimLink::input`, and what stands read back from nvim on a channel of its own.
//!
//! Every case is `#[ignore]`d: it needs `nvim` on `PATH`, spends no tokens and opens no window.
//!
//! Run: `cargo test -p eitri-core --test ide_host_real_nvim -- --ignored --test-threads=1`

#[path = "support/embed_client.rs"]
mod embed_client;

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eitri_core::editor_rpc::EditorRpc;
use eitri_core::editor_tools::stamp::EDITOR_BUDGET;
use eitri_core::ide_server::host::IdeHost;
use eitri_core::ide_server::lines::{line_values, split_lines};
use eitri_core::ide_server::{Command, ConnId, DiffRequest, Event, RpcId, ToolAnswer};
use eitri_core::nvim_rpc::{NvimLink, Pending};
use embed_client::Embed;
use rmpv::Value;

const STEP: Duration = Duration::from_millis(5);
const PATIENCE: Duration = Duration::from_secs(5);

/// How many windows of views stand, in every tabpage.
const VIEW_WINDOWS: &str = "
local count = 0
for _, win in ipairs(vim.api.nvim_list_wins()) do
  if pcall(vim.api.nvim_win_get_var, win, 'eitri_ide_view') then count = count + 1 end
end
return count
";

/// How many buffers of views stand, listed or not.
const VIEW_BUFFERS: &str = "
local count = 0
for _, buf in ipairs(vim.api.nvim_list_bufs()) do
  if vim.api.nvim_buf_get_name(buf):find('^eitri%-') then count = count + 1 end
end
return count
";

/// Puts the person in a view's proposal window, as a click would.
const ENTER_VIEW: &str = "
for _, win in ipairs(vim.api.nvim_list_wins()) do
  if pcall(vim.api.nvim_win_get_var, win, 'eitri_ide_view') then
    vim.api.nvim_set_current_win(win)
    return true
  end
end
return false
";

/// Whether a connection's first chunk ran: the prelude writes its scope down once a chunk has passed its stamp.
const SCOPE_RAN: &str = "
local state = rawget(_G, '__eitri_agent')
return state ~= nil and state.seq[(...)] ~= nil
";

/// Keeps nvim busy while the test queues behind it.
const HOLD_NVIM: &str = "local ready, fifo = ...; vim.fn.writefile({'ready'}, ready); vim.fn.readfile(fifo)";

/// The same editor under whatever number the test gives it, as a restart would change it.
struct Retargeted<'a> {
    link: &'a NvimLink,
    shift: Cell<u64>,
}

impl EditorRpc for Retargeted<'_> {
    fn exec_lua(&self, code: &'static str, args: Vec<Value>) -> Pending {
        EditorRpc::exec_lua(self.link, code, args)
    }

    fn target(&self) -> Option<u64> {
        EditorRpc::target(self.link).map(|target| target + self.shift.get())
    }
}

struct Session {
    host: IdeHost,
    link: NvimLink,
    nvim: Embed,
    dir: PathBuf,
    out: Vec<Command>,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn start(case: &str) -> Session {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("ide-host-{}-{case}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    let socket = agent::socket_path::in_dir(&dir, "n.sock").expect("a socket path");
    let nvim = Embed::start(&dir, &["--listen", socket.to_str().expect("text")], &[]);
    let link = connect(&socket);
    Session {
        host: IdeHost::new(dir.clone()),
        link,
        nvim,
        dir,
        out: Vec::new(),
    }
}

/// nvim binds the socket a moment before it listens, so the connect is retried until it answers.
fn connect(socket: &Path) -> NvimLink {
    let deadline = Instant::now() + PATIENCE;
    loop {
        match NvimLink::connect(socket, Duration::from_secs(1)) {
            Ok((link, _events)) => return link,
            Err(why) => {
                assert!(Instant::now() < deadline, "could not connect: {why}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

fn request(number: i64) -> RpcId {
    RpcId::Num(number)
}

/// What was answered. A release is no answer: it goes to the worker, and never reaches the client.
fn answered(commands: Vec<Command>) -> Vec<(ConnId, RpcId, ToolAnswer)> {
    commands
        .into_iter()
        .filter_map(|command| match command {
            Command::Answer { conn, id, answer } => Some((conn, id, answer)),
            Command::Released { .. } => None,
            other => panic!("not an answer: {other:?}"),
        })
        .collect()
}

fn is_error(answer: &ToolAnswer, sentence: &str) -> bool {
    matches!(answer, ToolAnswer::Error(why) if why == sentence)
}

impl Session {
    fn open_diff(&self, conn: ConnId, id: i64, tab: &str) -> Event {
        let (file_lines, _) = split_lines(b"one\ntwo\n");
        let (proposal_lines, shape) = split_lines(b"one\n2\n");
        Event::OpenDiff {
            conn,
            id: request(id),
            diff: Box::new(DiffRequest {
                tab_name: tab.to_owned(),
                real_path: self.dir.join("x.txt"),
                rel: "x.txt".to_owned(),
                file_text: b"one\ntwo\n".to_vec(),
                file_lines: line_values(file_lines),
                proposal_lines: line_values(proposal_lines),
                shape,
            }),
        }
    }

    fn event(&mut self, event: Event) {
        self.host.on_event(event, &self.link, Instant::now(), &mut self.out);
    }

    fn tick(&mut self) {
        self.host.tick(&self.link, Instant::now(), &mut self.out);
    }

    /// Ticks until `done` holds, as the window's timer would.
    fn until(&mut self, what: &str, mut done: impl FnMut(&mut Session) -> bool) {
        let deadline = Instant::now() + PATIENCE;
        loop {
            self.tick();
            if done(self) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(STEP);
        }
    }

    fn windows(&mut self) -> i64 {
        self.nvim.lua(VIEW_WINDOWS, vec![]).as_i64().expect("a count")
    }

    fn buffers(&mut self) -> i64 {
        self.nvim.lua(VIEW_BUFFERS, vec![]).as_i64().expect("a count")
    }

    /// `conn`'s view, standing and known to be `Open`.
    fn open(&mut self, conn: ConnId, id: i64, tab: &str) {
        let before = self.windows();
        let event = self.open_diff(conn, id, tab);
        self.event(event);
        self.until("the view", |session| {
            session.host.next_tick() == Some(Duration::from_millis(100))
        });
        assert_eq!(self.windows(), before + 2);
        assert!(self.out.is_empty(), "{:?}", self.out);
    }

    fn scope_ran(&mut self, conn: ConnId) -> bool {
        let scope = Value::from(format!("ide-{conn}"));
        self.nvim.lua(SCOPE_RAN, vec![scope]).as_bool().expect("a yes or no")
    }

    /// The view's own keys, in its proposal window.
    fn keys(&mut self, keys: &str) {
        assert_eq!(self.nvim.lua(ENTER_VIEW, vec![]), Value::from(true), "a view stands");
        let _pending = self.link.input(keys);
    }

    /// Nothing stands, and the host has nothing left to do.
    fn nothing_left(&mut self) {
        self.until("the windows to go", |session| session.windows() == 0);
        self.until("the host to go idle", |session| session.host.next_tick().is_none());
        assert_eq!(self.buffers(), 0);
    }

    /// Samples request turns, with a same-link barrier before every poll of the host's answers.
    fn settled(&mut self) -> Vec<(ConnId, RpcId, ToolAnswer)> {
        let mut clock = Instant::now();
        for _ in 0..20 {
            self.link
                .exec_lua("return true", vec![])
                .wait(PATIENCE)
                .expect("the editor barrier")
                .unwrap();
            clock += Duration::from_millis(100);
            self.host.tick(&self.link, clock, &mut self.out);
        }
        answered(std::mem::take(&mut self.out))
    }
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn cancelled_closes_the_view_and_answers_nothing() {
    let mut session = start("cancelled");
    session.open(1, 1, "tab");
    session.event(Event::Cancelled {
        conn: 1,
        id: request(1),
    });
    session.nothing_left();
    assert_eq!(session.settled(), vec![]);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn a_closing_connection_closes_its_view_and_answers_nothing() {
    let mut session = start("gone");
    session.open(1, 1, "tab");
    session.event(Event::ClientGone { conn: 1 });
    session.nothing_left();
    assert_eq!(session.settled(), vec![]);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn the_windows_end_closes_views_and_answers_nothing() {
    let mut session = start("end");
    session.open(1, 1, "tab");
    let opening = session.open_diff(2, 2, "tab");
    session.nvim.command("tabnew");
    session.event(opening);
    session.host.shutdown(&session.link);
    session.nothing_left();
    assert_eq!(session.settled(), vec![]);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn a_changed_target_answers_an_error() {
    let mut session = start("target");
    session.open(1, 1, "tab");
    let editor = Retargeted {
        link: &session.link,
        shift: Cell::new(1),
    };
    let mut out = Vec::new();
    session.host.tick(&editor, Instant::now(), &mut out);
    let answers = answered(out);
    assert_eq!(answers.len(), 1, "{answers:?}");
    assert_eq!((answers[0].0, answers[0].1.clone()), (1, request(1)));
    assert!(is_error(&answers[0].2, "the editor restarted"), "{answers:?}");
    assert_eq!(session.host.next_tick(), None, "polling stops");
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn close_tab_with_no_view_answers_success() {
    let mut session = start("no-view");
    session.event(Event::CloseTab {
        conn: 1,
        id: request(1),
        tab_name: "tab".to_owned(),
    });
    assert_eq!(session.settled(), vec![(1, request(1), ToolAnswer::Empty)]);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn close_tab_closes_only_that_connections_view_of_that_name() {
    let mut session = start("own-name");
    session.open(2, 1, "tab");
    session.event(Event::CloseTab {
        conn: 1,
        id: request(2),
        tab_name: "tab".to_owned(),
    });
    assert_eq!(session.settled(), vec![(1, request(2), ToolAnswer::Empty)]);
    assert_eq!(session.windows(), 2, "another connection's view stands");
    session.event(Event::CloseTab {
        conn: 2,
        id: request(3),
        tab_name: "tab".to_owned(),
    });
    session.nothing_left();
    let answers = session.settled();
    assert!(is_error(&answers[0].2, "the view was closed"), "{answers:?}");
    assert_eq!(answers[1], (2, request(3), ToolAnswer::Empty));
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn close_all_closes_all_of_a_connections() {
    let mut session = start("close-all");
    session.open(1, 1, "person closed");
    session.keys(":q<CR>");
    session.until("the person's close", |session| {
        session.windows() == 0 && session.host.next_tick().is_none()
    });
    session.open(1, 2, "standing");
    session.nvim.command("tabnew");
    session.open(2, 3, "another connection");

    session.event(Event::CloseAll {
        conn: 1,
        id: request(4),
    });
    session.until("the first connection's view to go", |session| session.windows() == 2);
    let answers = session.settled();
    assert_eq!(answers.len(), 3, "{answers:?}");
    for (conn, id, answer) in &answers[..2] {
        assert_eq!(*conn, 1);
        assert!(*id == request(1) || *id == request(2), "{id:?}");
        assert!(is_error(answer, "the view was closed"), "{answer:?}");
    }
    assert_eq!(answers[2], (1, request(4), ToolAnswer::Empty));
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn a_request_behind_a_view_the_person_closed_answers_an_error_on_close_tab() {
    let mut session = start("behind");
    session.open(1, 1, "tab");
    session.keys("\\q");
    session.until("the person's close", |session| {
        session.windows() == 0 && session.host.next_tick().is_none()
    });
    assert_eq!(session.settled(), vec![], "the terminal prompt decides");
    session.event(Event::CloseTab {
        conn: 1,
        id: request(2),
        tab_name: "tab".to_owned(),
    });
    let answers = session.settled();
    assert_eq!(answers.len(), 2, "{answers:?}");
    assert_eq!(answers[0].1, request(1));
    assert!(is_error(&answers[0].2, "the view was closed"), "{answers:?}");
    assert_eq!(answers[1], (1, request(2), ToolAnswer::Empty));
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn a_second_open_diff_answers_an_error() {
    let mut session = start("second");
    session.open(1, 1, "tab");
    let second = session.open_diff(1, 2, "other");
    session.event(second);
    let answers = session.settled();
    assert_eq!(answers.len(), 1, "{answers:?}");
    assert_eq!(answers[0].1, request(2));
    assert!(matches!(answers[0].2, ToolAnswer::Error(_)));
    assert_eq!(session.windows(), 2, "the first view stands");
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn a_second_connections_open_diff_beside_a_view_answers_an_error() {
    let mut session = start("beside");
    session.open(1, 1, "tab");
    let second = session.open_diff(2, 2, "tab");
    session.event(second);
    session.until("the editor's refusal", |session| !session.out.is_empty());
    let answers = session.settled();
    assert_eq!(answers.len(), 1, "{answers:?}");
    assert_eq!((answers[0].0, answers[0].1.clone()), (2, request(2)));
    assert!(matches!(answers[0].2, ToolAnswer::Error(_)));
    assert_eq!(session.windows(), 2, "a diff's tabpage takes no other");
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn reject_answers_diff_rejected() {
    let mut session = start("reject");
    session.open(1, 1, "tab");
    session.keys("\\d");
    session.until("the answer", |session| !session.out.is_empty());
    session.nothing_left();
    assert_eq!(session.settled(), vec![(1, request(1), ToolAnswer::DiffRejected)]);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn accept_goes_on_as_the_edited_lines() {
    let mut session = start("accept");
    session.open(1, 1, "tab");
    session.keys("\\a");
    session.until("the answer", |session| !session.out.is_empty());
    session.nothing_left();
    let [Command::Saved { conn, id, lines, shape }] = session.out.as_slice() else {
        panic!("one accept: {:?}", session.out);
    };
    assert_eq!((*conn, id), (1, &request(1)));
    assert_eq!(
        eitri_core::ide_server::lines::join_values(lines, *shape),
        Some(b"one\n2\n".to_vec())
    );
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn a_late_close_still_closes() {
    let mut session = start("late");
    let fifo = session.dir.join("release");
    let name = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: the path is NUL-terminated and remains live for the call.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let ready = session.dir.join("held");
    let _held = session.link.exec_lua(
        HOLD_NVIM,
        vec![ready.to_str().unwrap().into(), fifo.to_str().unwrap().into()],
    );
    let guard = Instant::now() + PATIENCE;
    while !ready.exists() {
        assert!(Instant::now() < guard, "the editor never entered its hold");
        std::thread::yield_now();
    }
    let open = session.open_diff(1, 1, "tab");
    session.event(open);
    // The host gives up on the open while nvim is still held: the open and then the close run after that.
    let past = Instant::now() + EDITOR_BUDGET;
    session.host.tick(&session.link, past, &mut session.out);
    let answers = answered(std::mem::take(&mut session.out));
    assert_eq!(answers.len(), 1, "{answers:?}");
    assert!(matches!(answers[0].2, ToolAnswer::Error(_)));
    std::fs::write(&fifo, b"").unwrap();
    session.until("the open to run", |session| session.scope_ran(1));
    session.nothing_left();
    assert_eq!(session.settled(), vec![]);
}

#[test]
#[ignore = "needs a real nvim on PATH"]
fn a_close_sent_right_after_an_open_leaves_nothing_standing() {
    let mut session = start("right-after");
    let open = session.open_diff(1, 1, "tab");
    session.event(open);
    session.event(Event::CloseTab {
        conn: 1,
        id: request(2),
        tab_name: "tab".to_owned(),
    });
    session.until("the open to run", |session| session.scope_ran(1));
    session.nothing_left();
    let answers = session.settled();
    assert_eq!(answers.len(), 2, "{answers:?}");
    assert!(is_error(&answers[0].2, "the view was closed"), "{answers:?}");
    assert_eq!(answers[1], (1, request(2), ToolAnswer::Empty));
}
