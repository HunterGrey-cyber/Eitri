#![cfg(target_os = "linux")]
//! The terminal `claude`'s side of the lock file, replayed against the real listener, the real host and a real `nvim`.
//!
//! A client speaks the CLI's side frame for frame, in the order read from the pinned binary; the test thread plays the
//! window's main context between the client's steps (drain the listener, hand the host its events, tick it, poll the
//! editor-context feed); the person's keys go into `nvim` itself. Nothing here ran against a real `claude`.
//!
//! Every case is `#[ignore]`d: it needs `nvim` on `PATH`, spends no tokens and opens no window.
//!
//! Run: `cargo test -p eitri-core --test ide_server_replay -- --ignored --test-threads=1`

#[path = "support/embed_client.rs"]
mod embed_client;
#[path = "support/ide_client.rs"]
mod ide_client;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use eitri_core::editor_context::feed::{EditorContextFeed, EditorContextReader};
use eitri_core::editor_context::{ContextSource, EditorContext};
use eitri_core::editor_feeds::{EditorFeeds, PushSource};
use eitri_core::ide_server::handshake::getrandom_fill;
use eitri_core::ide_server::host::IdeHost;
use eitri_core::ide_server::server::IdeServer;
use eitri_core::ide_server::{Command, ContextUpdate, Event, Limits, ServerConfig, Started};
use eitri_core::nvim_rpc::NvimLink;
use embed_client::Embed;
use ide_client::IdeClient;
use rmpv::Value;
use serde_json::{json, Value as Json};

/// How long the test waits for something that must happen. Never an expected time.
const PATIENCE: Duration = Duration::from_secs(20);
/// How long a test listens for something that must not happen.
const QUIET: Duration = Duration::from_millis(300);
const STEP: Duration = Duration::from_millis(5);

/// How many windows of proposal views stand, in every tabpage.
const VIEW_WINDOWS: &str = "
local count = 0
for _, win in ipairs(vim.api.nvim_list_wins()) do
  if pcall(vim.api.nvim_win_get_var, win, 'eitri_ide_view') then count = count + 1 end
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

/// The file the editor shows and `getDiagnostics` is asked about: a non-BMP character on its only line, and one
/// diagnostic on the `b` after it.
const DIAGNOSED: &str = "
vim.diagnostic.set(vim.api.nvim_create_namespace('replay'), 0, {
  { lnum = 0, col = 5, end_lnum = 0, end_col = 6, severity = vim.diagnostic.severity.ERROR, message = 'on the b' }
})
";

const OUTSIDE_NOTE: &str = "Eitri: that file is outside the project, or its name cannot be sent; nothing was sent";

/// The window's side of one replay: a listener, the host, the feeds and a real editor, all on this thread.
struct Replay {
    server: Option<IdeServer>,
    host: IdeHost,
    feeds: EditorFeeds,
    context: ContextSource,
    facts: PushSource,
    /// Whatever the server sent that nobody was waiting for.
    notes: Vec<Json>,
    port: u16,
    token: String,
    root: PathBuf,
    dir: PathBuf,
    link: NvimLink,
    nvim: Embed,
    feed: EditorContextFeed,
}

impl Drop for Replay {
    fn drop(&mut self) {
        if let Some(server) = self.server.take() {
            self.host.shutdown(&self.link);
            server.stop(Duration::from_millis(500));
        }
        self.feed.cleanup();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Replay {
    fn start(case: &str) -> Replay {
        agent::state_dirs::redirect_state_to_a_test_root();
        // Under the system temp dir and short: the socket inside it is capped at 103 bytes, which a target
        // directory's absolute path can exceed on its own.
        let dir = std::env::temp_dir().join(format!("irp-{}-{case}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("project")).expect("scratch project");
        std::fs::create_dir_all(dir.join("home")).expect("scratch home");
        // Resolved, so the paths the listener reports are the ones the test wrote.
        let dir = dir.canonicalize().expect("scratch dir");
        let (root, home) = (dir.join("project"), dir.join("home"));

        let mut feed = EditorContextFeed::new().expect("the feed binds");
        let mut feeds = EditorFeeds::new();
        let reader = EditorContextReader::new(feed.take_listener().expect("the listener"));
        let (context, _reset) = feeds.add_context(reader, None, Instant::now());
        let facts = feeds.push_source().expect("a context feed");

        let socket = agent::socket_path::in_dir(&dir, "n.sock").expect("a socket path");
        let mut args: Vec<String> = vec!["--listen".to_owned(), socket.to_str().expect("text").to_owned()];
        args.extend(feed.nvim_args());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let env = feed.child_env();
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let mut nvim = Embed::start(&dir, &args, &env);
        // Before any view maps its keys.
        nvim.command("let maplocalleader = ','");
        let link = connect(&socket);

        let server = IdeServer::start_with(
            ServerConfig {
                root: root.clone(),
                home: Some(home.clone()),
            },
            Limits::default(),
            getrandom_fill,
        )
        .expect("a loopback port can be had");
        let Some(Started::Attached { port }) = server.first_answer(PATIENCE) else {
            panic!("the listener did not attach");
        };
        let lock = home.join(".claude").join("ide").join(format!("{port}.lock"));
        let lock: Json = serde_json::from_slice(&std::fs::read(&lock).expect("the lock")).expect("a JSON lock");
        let token = lock["authToken"].as_str().expect("a token").to_owned();

        Replay {
            server: Some(server),
            host: IdeHost::new(root.clone()),
            feeds,
            context,
            facts,
            notes: Vec::new(),
            port,
            token,
            root,
            dir,
            link,
            nvim,
            feed,
        }
    }

    /// One turn of the window's main context: what the listener has, the host's timer, the feeds.
    fn pump(&mut self) {
        let now = Instant::now();
        let mut out = Vec::new();
        let events = self.server.as_ref().expect("a listener").drain();
        for event in events {
            if !matches!(event, Event::Started(_)) {
                self.host.on_event(event, &self.link, now, &mut out);
            }
        }
        if self.host.next_tick().is_some() {
            self.host.tick(&self.link, now, &mut out);
        }
        if self.feeds.poll(now).context {
            let facts = (self.facts)().unwrap_or_default();
            out.push(Command::Context(ContextUpdate {
                context: (self.context)(),
                facts,
            }));
            self.host.check_target(&self.link, &mut out);
        }
        if !out.is_empty() {
            self.server.as_ref().expect("a listener").send(out);
        }
    }

    fn pump_until(&mut self, what: &str, mut done: impl FnMut(&mut Replay) -> bool) {
        let deadline = Instant::now() + PATIENCE;
        loop {
            self.pump();
            if done(self) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(STEP);
        }
    }

    /// The next message from the server, the main context running meanwhile.
    fn message(&mut self, client: &mut IdeClient) -> Json {
        let deadline = Instant::now() + PATIENCE;
        loop {
            self.pump();
            if client.waiting_within(STEP) {
                return client.read_message();
            }
            assert!(Instant::now() < deadline, "no message came");
        }
    }

    /// The answers to these ids, in the order they came. A notification on the way is kept in `notes`.
    fn answers(&mut self, client: &mut IdeClient, ids: &[Json]) -> Vec<Json> {
        let mut found = Vec::new();
        while found.len() < ids.len() {
            let message = self.message(client);
            if message.get("id").is_some_and(|id| ids.contains(id)) {
                found.push(message);
            } else if message.get("id").is_none() {
                self.notes.push(message);
            } else {
                panic!("an answer nobody asked for: {message}");
            }
        }
        found
    }

    fn answer(&mut self, client: &mut IdeClient, id: i64) -> Json {
        self.answers(client, &[json!(id)]).remove(0)
    }

    /// The next notification of this method that `want` accepts; others are dropped.
    fn push(&mut self, client: &mut IdeClient, method: &str, mut want: impl FnMut(&Json) -> bool) -> Json {
        let deadline = Instant::now() + PATIENCE;
        loop {
            while !self.notes.is_empty() {
                let note = self.notes.remove(0);
                if note["method"] == method && want(&note["params"]) {
                    return note["params"].clone();
                }
            }
            let message = self.message(client);
            assert!(message.get("id").is_none(), "an answer nobody asked for: {message}");
            self.notes.push(message);
            assert!(Instant::now() < deadline, "no {method} wanted came");
        }
    }

    /// Everything the server sends for `within`, notes first.
    fn listen(&mut self, client: &mut IdeClient, within: Duration) -> Vec<Json> {
        let until = Instant::now() + within;
        let mut heard = std::mem::take(&mut self.notes);
        while Instant::now() < until {
            self.pump();
            if client.waiting_within(STEP) {
                heard.push(client.read_message());
            }
        }
        heard
    }

    fn calls(&mut self, client: &mut IdeClient, id: i64, tool: &str, arguments: Json) -> Json {
        client.call_tool(id, tool, arguments);
        self.answer(client, id)
    }

    /// The upgrade, `server/discover` and `initialize`, as the CLI's first frames.
    fn attach(&mut self) -> IdeClient {
        let mut client = IdeClient::attach_like_cli(self.port, &self.token);
        client.send_json(
            &json!({"jsonrpc": "2.0", "id": "server-discover-probe-1", "method": "server/discover", "params": {}}),
        );
        let discover = self.answers(&mut client, &[json!("server-discover-probe-1")]).remove(0);
        assert_eq!(discover["error"]["code"], -32601, "{discover}");
        client.request(
            1,
            "initialize",
            json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "claude-code", "version": "2.1.311"},
            }),
        );
        let initialize = self.answer(&mut client, 1);
        assert_eq!(initialize["result"]["serverInfo"]["name"], "Eitri", "{initialize}");
        assert_eq!(initialize["result"]["protocolVersion"], "2025-11-25", "{initialize}");
        client
    }

    /// `notifications/initialized`, then `ide_connected`. The first push waits for the first of them.
    fn ready(&mut self, client: &mut IdeClient) {
        client.notify("notifications/initialized", json!({}));
        client.notify("ide_connected", json!({"pid": std::process::id()}));
    }

    fn connect(&mut self) -> IdeClient {
        let mut client = self.attach();
        self.ready(&mut client);
        client
    }

    fn write(&self, relative: &str, text: &str) -> PathBuf {
        let path = self.root.join(relative);
        std::fs::write(&path, text).expect("a file");
        path
    }

    /// A file outside the project, in the same scratch directory.
    fn write_outside(&self, name: &str, text: &str) -> PathBuf {
        let path = self.dir.join(name);
        std::fs::write(&path, text).expect("a file");
        path
    }

    fn edit(&mut self, path: &Path) {
        self.nvim.lua(
            "vim.cmd.edit(vim.fn.fnameescape((...)))",
            vec![Value::from(path.to_str().expect("text"))],
        );
    }

    /// The main context has seen the feed report this file.
    fn context_is(&mut self, path: &Path) {
        let want = path.to_str().expect("text").to_owned();
        self.pump_until("the feed's report of the file", |replay| {
            (replay.context)().is_some_and(|context: EditorContext| context.file == want)
        });
    }

    fn view_windows(&mut self) -> i64 {
        self.nvim.lua(VIEW_WINDOWS, vec![]).as_i64().expect("a count")
    }

    /// The person presses `keys` in the proposal window, once the pair stands.
    fn press(&mut self, keys: &str) {
        self.pump_until("the pair", |replay| replay.view_windows() == 2);
        assert_eq!(self.nvim.lua(ENTER_VIEW, vec![]), Value::from(true), "a view stands");
        self.nvim.input(keys);
    }

    fn open_diff(&mut self, client: &mut IdeClient, id: i64, path: &Path, contents: &str, tab: &str) {
        let path = path.to_str().expect("text");
        client.call_tool(
            id,
            "openDiff",
            json!({"old_file_path": path, "new_file_path": path, "new_file_contents": contents, "tab_name": tab}),
        );
    }

    fn mention_command_defined(&mut self) -> bool {
        self.nvim.eval("exists(':EitriMention')").as_i64() == Some(2)
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

fn texts(answer: &Json) -> Vec<String> {
    answer["result"]["content"]
        .as_array()
        .unwrap_or_else(|| panic!("no content in {answer}"))
        .iter()
        .map(|item| {
            assert_eq!(item["type"], "text", "{answer}");
            item["text"].as_str().expect("a text").to_owned()
        })
        .collect()
}

fn is_push(message: &Json, method: &str) -> bool {
    message["method"] == method
}

fn file_url(path: &Path) -> String {
    format!("file://{}", path.display())
}

/// A `selection_changed` for the file with this selection and nothing selected otherwise.
fn cursor_form(path: &Path, line: u32) -> Json {
    json!({
        "selection": {"start": {"line": line, "character": 0}, "end": {"line": line, "character": 0}},
        "text": "",
        "filePath": path.to_str().expect("text"),
    })
}

#[test]
#[ignore = "needs nvim on PATH; headless, no account or display"]
fn a_turn_goes_frame_for_frame() {
    let mut replay = Replay::start("turn");
    let diagnosed = replay.write("diagnosed.txt", "a\u{1F600}bc\n");
    let edited = replay.write("x.txt", "one\ntwo\n");
    replay.edit(&diagnosed);
    replay.nvim.lua(DIAGNOSED, vec![]);
    replay.context_is(&diagnosed);

    let mut client = replay.attach();
    replay.ready(&mut client);
    // The first push comes once, on the first of the two notifications, and the second adds none.
    let first = replay.push(&mut client, "selection_changed", |_| true);
    assert_eq!(first, cursor_form(&diagnosed, 0));

    let closed_all = replay.calls(&mut client, 2, "closeAllDiffTabs", json!({}));
    assert_eq!(texts(&closed_all), [""]);

    let uri = file_url(&diagnosed);
    let baseline = replay.calls(&mut client, 3, "getDiagnostics", json!({"uri": uri}));
    let baseline: Json = serde_json::from_str(&texts(&baseline)[0]).expect("JSON text");
    assert_eq!(baseline[0]["uri"], uri.as_str(), "{baseline}");
    assert_eq!(
        baseline[0]["diagnostics"][0]["range"],
        json!({"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 4}}),
        "{baseline}"
    );

    replay.open_diff(&mut client, 4, &edited, "one\n2\n", "tab one");
    replay.press(",a");
    let saved = replay.answer(&mut client, 4);
    assert_eq!(texts(&saved), ["FILE_SAVED", "one\n2\n"]);

    for id in [5, 6] {
        let closed = replay.calls(&mut client, id, "close_tab", json!({"tab_name": "tab one"}));
        assert_eq!(texts(&closed), [""], "close_tab {id}");
    }

    let all = replay.calls(&mut client, 7, "getDiagnostics", json!({}));
    let all: Json = serde_json::from_str(&texts(&all)[0]).expect("JSON text");
    assert_eq!(all[0]["uri"], baseline[0]["uri"], "{all}");

    let again = replay.listen(&mut client, QUIET);
    assert!(
        !again.iter().any(|message| is_push(message, "selection_changed")),
        "the first push came once: {again:?}"
    );
}

#[test]
#[ignore = "needs nvim on PATH; headless, no account or display"]
fn a_rejected_turn() {
    let mut replay = Replay::start("rejected");
    let edited = replay.write("x.txt", "one\ntwo\n");
    let mut client = replay.connect();

    let closed_all = replay.calls(&mut client, 2, "closeAllDiffTabs", json!({}));
    assert_eq!(texts(&closed_all), [""]);
    replay.open_diff(&mut client, 3, &edited, "one\n2\n", "tab one");
    replay.press(",d");
    let rejected = replay.answer(&mut client, 3);
    assert_eq!(texts(&rejected), ["DIFF_REJECTED"]);
    for id in [4, 5] {
        let closed = replay.calls(&mut client, id, "close_tab", json!({"tab_name": "tab one"}));
        assert_eq!(texts(&closed), [""], "close_tab {id}");
    }
    assert_eq!(replay.view_windows(), 0);
}

#[test]
#[ignore = "needs nvim on PATH; headless, no account or display"]
fn a_closed_view_leaves_the_prompt_to_the_terminal() {
    let mut replay = Replay::start("closed");
    let edited = replay.write("x.txt", "one\ntwo\n");
    let mut client = replay.connect();

    replay.open_diff(&mut client, 2, &edited, "one\n2\n", "tab one");
    replay.press(",q");
    replay.pump_until("the view to go", |replay| replay.view_windows() == 0);
    let heard = replay.listen(&mut client, QUIET);
    assert!(
        heard.iter().all(|message| message.get("id").is_none()),
        "the person closing a view answers nothing: {heard:?}"
    );

    client.call_tool(3, "close_tab", json!({"tab_name": "tab one"}));
    let answers = replay.answers(&mut client, &[json!(2), json!(3)]);
    for answer in &answers {
        if answer["id"] == 3 {
            assert_eq!(texts(answer), [""]);
        } else {
            assert!(
                answer["error"]["message"].is_string(),
                "the open request is an error: {answer}"
            );
            assert!(answer.get("result").is_none(), "{answer}");
        }
    }
}

#[test]
#[ignore = "needs nvim on PATH; headless, no account or display"]
fn the_view_is_closed_before_file_saved_goes() {
    let mut replay = Replay::start("next-edit");
    let edited = replay.write("x.txt", "one\ntwo\n");
    let mut client = replay.connect();

    replay.open_diff(&mut client, 2, &edited, "one\n2\n", "tab one");
    replay.press(",a");
    let saved = replay.answer(&mut client, 2);
    assert_eq!(texts(&saved)[0], "FILE_SAVED");

    // The next edit's request comes on the heels of the answer, with no `close_tab` between.
    replay.open_diff(&mut client, 3, &edited, "one\n3\n", "tab two");
    replay.press(",d");
    let rejected = replay.answer(&mut client, 3);
    assert_eq!(
        texts(&rejected),
        ["DIFF_REJECTED"],
        "the next edit was not refused: {rejected}"
    );
}

/// Opens `text` in the editor and connects once the main context has its report.
fn connected_on(replay: &mut Replay, name: &str, text: &str) -> (PathBuf, IdeClient) {
    let path = replay.write(name, text);
    replay.edit(&path);
    replay.context_is(&path);
    let mut client = replay.connect();
    let first = replay.push(&mut client, "selection_changed", |_| true);
    assert_eq!(first, cursor_form(&path, 0));
    (path, client)
}

#[test]
#[ignore = "needs nvim on PATH; headless, no account or display"]
fn a_one_line_v_selection_pushes_its_columns() {
    let mut replay = Replay::start("v-columns");
    let (path, mut client) = connected_on(&mut replay, "a.txt", "bravo\n");
    replay.nvim.input("lvll");
    let pushed = replay.push(&mut client, "selection_changed", |params| params["text"] == "rav");
    assert_eq!(
        pushed,
        json!({
            "selection": {"start": {"line": 0, "character": 1}, "end": {"line": 0, "character": 4}},
            "text": "rav",
            "filePath": path.to_str().unwrap(),
        })
    );
}

#[test]
#[ignore = "needs nvim on PATH; headless, no account or display"]
fn a_v_selection_ending_in_a_non_bmp_character() {
    let mut replay = Replay::start("non-bmp");
    let (path, mut client) = connected_on(&mut replay, "a.txt", "a\u{1F642}b\n");
    replay.nvim.input("lv");
    let pushed = replay.push(&mut client, "selection_changed", |params| params["text"] == "\u{1F642}");
    assert_eq!(
        pushed,
        json!({
            "selection": {"start": {"line": 0, "character": 1}, "end": {"line": 0, "character": 3}},
            "text": "\u{1F642}",
            "filePath": path.to_str().unwrap(),
        })
    );
}

#[test]
#[ignore = "needs nvim on PATH; headless, no account or display"]
fn a_linewise_selection_ending_on_an_empty_line_ends_at_the_next_line() {
    let mut replay = Replay::start("linewise");
    let (path, mut client) = connected_on(&mut replay, "a.txt", "abc\n\nghi\n");
    replay.nvim.input("Vj");
    let pushed = replay.push(&mut client, "selection_changed", |params| {
        params["text"].as_str().is_some_and(|text| text.starts_with("abc"))
    });
    assert_eq!(
        pushed["selection"]["start"],
        json!({"line": 0, "character": 0}),
        "{pushed}"
    );
    assert_eq!(
        pushed["selection"]["end"],
        json!({"line": 2, "character": 0}),
        "{pushed}"
    );
    assert_eq!(pushed["filePath"], path.to_str().unwrap());
}

#[test]
#[ignore = "needs nvim on PATH; headless, no account or display"]
fn the_first_push_comes_on_connect() {
    let mut replay = Replay::start("first-push");
    let path = replay.write("a.txt", "one\ntwo\nthree\n");
    replay.edit(&path);
    replay.nvim.input("jj");
    replay.pump_until("the cursor's report", |replay| {
        (replay.context)().is_some_and(|context: EditorContext| context.file == path.to_str().unwrap())
            && (replay.facts)().is_some_and(|facts| facts.cursor_line == Some(3))
    });
    let mut client = replay.attach();
    let early = replay.listen(&mut client, QUIET);
    assert!(
        early.is_empty(),
        "nothing is pushed before the client says it is ready: {early:?}"
    );
    client.notify("notifications/initialized", json!({}));
    let first = replay.push(&mut client, "selection_changed", |_| true);
    assert_eq!(first, cursor_form(&path, 2));
    // `ide_connected` comes after the push, and a second readiness signal does not push again.
    client.notify("ide_connected", json!({"pid": std::process::id()}));
    let more = replay.listen(&mut client, QUIET);
    assert!(more.is_empty(), "one push, once: {more:?}");
}

#[test]
#[ignore = "needs nvim on PATH; headless, no account or display"]
fn outside_the_project_the_push_is_the_empty_form() {
    let mut replay = Replay::start("outside");
    let outside = replay.write_outside("outside.txt", "one\ntwo\n");
    replay.edit(&outside);
    replay.context_is(&outside);
    let mut client = replay.connect();
    let first = replay.push(&mut client, "selection_changed", |_| true);
    assert_eq!(
        first,
        json!({
            "selection": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}},
            "text": "",
        }),
        "no path and no text of a file outside"
    );
}

#[test]
#[ignore = "needs nvim on PATH; headless, no account or display"]
fn leaving_visual_mode_pushes_the_empty_form_with_the_file() {
    let mut replay = Replay::start("leave-visual");
    let (path, mut client) = connected_on(&mut replay, "a.txt", "bravo\n");
    replay.nvim.input("lvll");
    replay.push(&mut client, "selection_changed", |params| params["text"] == "rav");
    replay.nvim.input("<Esc>");
    let left = replay.push(&mut client, "selection_changed", |params| params["text"] == "");
    assert_eq!(left, cursor_form(&path, 0));
}

#[test]
#[ignore = "needs nvim on PATH; headless, no account or display"]
fn a_mention_reaches_the_client() {
    let mut replay = Replay::start("mention");
    let (path, mut client) = connected_on(&mut replay, "a.txt", "1\n2\n3\n4\n5\n");
    replay.pump_until("the command", |replay| replay.mention_command_defined());
    replay.nvim.command("2,4EitriMention");
    let mention = replay.push(&mut client, "at_mentioned", |_| true);
    assert_eq!(
        mention,
        json!({"filePath": path.to_str().unwrap(), "lineStart": 1, "lineEnd": 3})
    );
}

#[test]
#[ignore = "needs nvim on PATH; headless, no account or display"]
fn a_mention_followed_by_a_context_update_both_arrive_and_the_context_is_unchanged() {
    let mut replay = Replay::start("mention-update");
    let (path, mut client) = connected_on(&mut replay, "a.txt", "1\n2\n3\n4\n5\n");
    replay.pump_until("the command", |replay| replay.mention_command_defined());
    // Leaving Visual mode for the command raises a context update at once.
    replay.nvim.input("jVj:EitriMention<CR>");
    let mention = replay.push(&mut client, "at_mentioned", |_| true);
    assert_eq!(
        mention,
        json!({"filePath": path.to_str().unwrap(), "lineStart": 1, "lineEnd": 2})
    );
    // The update that follows is the cursor on the file again: the mention did not take the context away.
    let after = replay.push(&mut client, "selection_changed", |params| params["text"] == "");
    assert_eq!(after["filePath"], path.to_str().unwrap(), "{after}");
}

#[test]
#[ignore = "needs nvim on PATH; headless, no account or display"]
fn a_mention_outside_sends_nothing() {
    let mut replay = Replay::start("mention-outside");
    let outside = replay.write_outside("outside.txt", "one\ntwo\n");
    replay.edit(&outside);
    replay.context_is(&outside);
    let mut client = replay.connect();
    replay.pump_until("the command", |replay| replay.mention_command_defined());
    replay.nvim.command("EitriMention");
    let heard = replay.listen(&mut client, QUIET);
    assert!(
        !heard.iter().any(|message| is_push(message, "at_mentioned")),
        "nothing was sent: {heard:?}"
    );
    replay.pump_until("the note in the message area", |replay| {
        replay
            .nvim
            .eval("execute('messages')")
            .as_str()
            .is_some_and(|messages| messages.contains(OUTSIDE_NOTE))
    });
}

#[test]
#[ignore = "needs nvim on PATH; headless, no account or display"]
fn no_client_no_eitri_mention() {
    let mut replay = Replay::start("no-command");
    replay.pump();
    assert!(!replay.mention_command_defined(), "undefined before the first client");
    let mut client = replay.connect();
    replay.pump_until("the command", |replay| replay.mention_command_defined());
    client.close(1000);
    drop(client);
    replay.pump_until("the command to go", |replay| !replay.mention_command_defined());
}
