//! `eitri open --nvim <addr> [DIR]`: a person's running terminal nvim, moved into a window of its own and given back
//! to its terminal however the window ends.
//!
//! Before GTK runs, on the one thread there is and on workers it waits for: the scrub of what an nvim's job hands down,
//! the streams moved off that nvim's pipes, the checks that refuse before anything moves (exit 64, one line to the
//! starter), the window's feed and pane-switch sockets bound so the glue's parts have servers listening when they
//! install, the waiting room and the glue (`eitri_core::takeover::sequence::prepare`), and the terminal's UI moved
//! into the room. Then the window starts with its editor attached to that nvim ([`TakeoverStart`]).
//!
//! While the window runs, [`Held`] is what workers share: the links to the nvim and the room, the editor's hang-up
//! handle, and the one detach. The link worker reads the glue's notices and the room's events, and for anything that
//! hands the nvim back it does the whole detach itself, off the GTK thread and before the window asks anything: the
//! glue's teardown (at most [`DETACH_TEARDOWN`]), the editor's hang-up, then `let_go` to the room (at most
//! [`DETACH_ROOM_WAIT`]). The window's own closes reach the same detach through the editor's `Detach`.
//!
//! Nothing here sends a signal to, or ends, the person's nvim: every way out is the glue's teardown, a hang-up of the
//! editor's channel, and the room's return of the terminal's UI.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use eitri_core::companion::TEARDOWN_LUA;
use eitri_core::editor_rpc::{EditorRpc, Glue, GlueSlot};
use eitri_core::nvim_rpc::{LinkEvent, NvimLink, Pending, RpcError};
use eitri_core::project_root::OpenArgs;
use eitri_core::takeover::glue::{CommandsPart, GlueNotice, TakeoverParts, TmuxSwap};
use eitri_core::takeover::room::{LetGo, RoomEvent};
use eitri_core::takeover::sequence::{self, Checks, Moved, Prepared, Refusal, StepError};
use eitri_core::wm::close::HandBackWhy;
use eitri_core::wm::{CloseAsks, EditorEvent, HostEvent, HostSink, WinId, DETACH_ROOM_WAIT, DETACH_TEARDOWN};
use gtk4::glib;
use rmpv::Value;

use crate::page_window::StarterStderr;

/// After the project's name in the window's title and on its top bar: the window that gives an nvim back when closed.
pub(crate) const TITLE_SUFFIX: &str = " (from terminal)";
/// The band when the terminal the nvim came from is closed, or the move never landed in the room.
pub(crate) const TERMINAL_CLOSED: &str = "the terminal this nvim came from is closed";
/// The band when the room is gone, or found the nvim no longer listening.
pub(crate) const WAY_BACK_GONE: &str = "the terminal's way back is gone";
/// The channel the fork's attach records in the person's nvim; read over the window's own link.
pub(crate) const FORK_CHAN_LUA: &str =
    "local a = rawget(_G, '__neovide_attach'); return type(a) == 'table' and a.chan or nil";

/// How long a close's questions to nvim wait, all of them together; no answer in time is nvim waiting for a key. Inside
/// the controller's own wait for the answer ([`eitri_core::wm::close::CLOSE_FACTS_WITHIN`]), so the facts only the
/// window knows (the drafts, the proposals, a terminal that is gone) reach a close even when nvim answers nothing.
const FACTS_WITHIN: Duration = Duration::from_millis(150);
/// How long, after the link to nvim closed, the window looks for nvim's process to have gone before it takes the
/// close for a lost link: nvim's own exit closes the link first.
const EXIT_GRACE: Duration = Duration::from_secs(1);
/// How long the process waits, once the window has gone, for a detach still under way: its own bounds and a margin.
const DETACH_DONE_WITHIN: Duration = Duration::from_secs(3);
/// How long the fork's attach takes to record its channel at most, after the editor's session is up.
const FORK_CHAN_WITHIN: Duration = Duration::from_secs(2);
/// The variable the IDE's proposal view sets on each of its windows, naming the view.
const PROPOSAL_MARK: &str = "eitri_ide_view";

/// The code the process ends with once the window has closed; 0 until something decided another. The first stays.
static EXIT: AtomicI32 = AtomicI32::new(0);

fn exit_with(code: i32) {
    let _ = EXIT.compare_exchange(0, code, Ordering::SeqCst, Ordering::SeqCst);
}

/// The one line a refusal or a failure is told with.
fn step_text(error: &StepError) -> String {
    match error {
        StepError::Refused(refusal) => match refusal {
            Refusal::NotASocket => "the address is not an nvim socket".into(),
            Refusal::OtherUid => "the nvim at that address belongs to another user".into(),
            Refusal::Off => "takeover is off (editor.takeover in init.lua)".into(),
            Refusal::TooOld(version) => format!("Eitri's takeover needs nvim 0.12 or later (this is {version})"),
            Refusal::NotOneTui => "nvim must show exactly one UI, the terminal it was started in".into(),
            Refusal::AlreadyTaken => "this nvim is open in an Eitri window".into(),
            Refusal::NoDisplay => "no display to open a window on".into(),
            Refusal::Blocking => "nvim is waiting for a key (or busy)".into(),
        },
        StepError::RoomStart(why) => format!("the waiting room did not start: {why}"),
        StepError::Install(why) => why.clone(),
        StepError::MoveError(why) => format!("the terminal's UI did not move: {why}"),
        StepError::MoveUnanswered => "nvim did not answer the move; its terminal gets the UI back when it does".into(),
        StepError::TuiStayed => "the terminal's UI did not leave".into(),
        StepError::OtherUi => "another UI attached to nvim while it moved".into(),
        StepError::WayBackGone => WAY_BACK_GONE.into(),
    }
}

/// Says `error` where the starter reads and in the log, and gives the exit code.
fn fail(starter: &StarterStderr, error: &StepError) -> i32 {
    say_failed(starter, &step_text(error));
    error.exit_code()
}

fn say_failed(starter: &StarterStderr, text: &str) {
    let line = format!("eitri open: {text}");
    starter.say(&line);
    eprintln!("{line}");
}

/// The window's sockets, bound before the glue goes in so each part's server listens when the part connects.
pub(crate) struct Feeds {
    pub(crate) theme: Option<crate::theme::feed::ThemeFeed>,
    pub(crate) context: Option<eitri_core::editor_context::feed::EditorContextFeed>,
    pub(crate) keys: Option<eitri_core::nvim_keys::feed::NvimKeysFeed>,
    pub(crate) pane_switch: Option<crate::pane_switch::PaneSwitchChannel>,
}

impl Feeds {
    fn bind() -> Feeds {
        Feeds {
            theme: crate::theme::feed::ThemeFeed::new(),
            context: eitri_core::editor_context::feed::EditorContextFeed::new(),
            keys: eitri_core::nvim_keys::feed::NvimKeysFeed::new(),
            pane_switch: crate::pane_switch::open(),
        }
    }

    /// The glue's parts, each pointed at its socket. `:Eitri` completes the words of `keys`, the keymap `init.lua`
    /// gives, with its group of keys under `keymap.nvim_prefix`, as a window's own nvim gets them.
    fn parts(&self, keys: &WindowKeys) -> TakeoverParts {
        let pane_switch = self.pane_switch.as_ref();
        let sockets = eitri_core::companion::Sockets {
            editor_context: self.context.as_ref().map(|feed| feed.socket_path().to_path_buf()),
            theme: self.theme.as_ref().map(|feed| feed.socket_path().to_path_buf()),
            keys: self.keys.as_ref().map(|feed| feed.socket_path().to_path_buf()),
            pane_switch: pane_switch.map(|channel| channel.socket_path().to_path_buf()),
            reload_always: true,
        };
        let (words, group) = keys.commands();
        let tmux_swap = pane_switch.and_then(|channel| {
            let env = channel.child_env();
            let get = |name: &str| env.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone());
            Some(TmuxSwap {
                tmux: get("TMUX")?,
                tmux_pane: get("TMUX_PANE")?,
                pane_switch_socket: channel.socket_path().to_path_buf(),
                shim_dir: channel.dir().join("bin"),
            })
        });
        TakeoverParts {
            sockets,
            commands: CommandsPart {
                socket: pane_switch
                    .map(|channel| channel.socket_path().to_path_buf())
                    .unwrap_or_default(),
                words,
                group,
            },
            tmux_swap,
        }
    }
}

/// The keys `init.lua` gives the window, as far as `:Eitri` in the person's nvim needs them: read before the glue goes
/// in, from the configuration alone (no plugin code runs), so a page key a plugin sets at its load is not among them.
pub(crate) struct WindowKeys {
    keymap: Rc<eitri_core::keymap::Keymap>,
    nvim_prefix: Option<String>,
    pages: Vec<(eitri_core::layout::ModuleId, Option<String>)>,
}

impl WindowKeys {
    /// The stock keymap, with no group under a prefix.
    fn stock() -> WindowKeys {
        WindowKeys {
            keymap: Rc::new(eitri_core::keymap::Keymap::defaults()),
            nvim_prefix: None,
            pages: Vec::new(),
        }
    }

    /// The words `:Eitri` completes, and the group of keys under `keymap.nvim_prefix` when it is set.
    fn commands(
        &self,
    ) -> (
        eitri_core::nvim_verbs::CommandWords,
        Option<(String, Vec<eitri_core::nvim_verbs::leader::LeaderKey>)>,
    ) {
        let (module_keys, _) = eitri_core::layout::ModuleKeys::build_settled(&self.pages, &self.keymap);
        let actions: Vec<String> = self
            .keymap
            .bindings()
            .iter()
            .map(|binding| binding.action.name())
            .collect();
        let keyed: Vec<eitri_core::layout::ModuleId> = module_keys.entries().iter().map(|(_, id)| id.clone()).collect();
        let panels: Vec<eitri_core::layout::ModuleId> = self.pages.iter().map(|(id, _)| id.clone()).collect();
        let words = crate::nvim_verbs::completion_words(&actions, &keyed, &panels);
        let group = self.nvim_prefix.clone().map(|lhs| {
            let panel_ids: Vec<String> = panels
                .iter()
                .filter_map(|id| id.as_str().strip_prefix("lua:").map(str::to_string))
                .collect();
            (
                lhs,
                eitri_core::nvim_verbs::leader::leader_group(&self.keymap, &module_keys, &panel_ids),
            )
        });
        (words, group)
    }
}

/// What `build_ui` takes for a window over a person's nvim.
pub(crate) struct TakeoverStart {
    pub(crate) feeds: Feeds,
    pub(crate) held: Arc<Held>,
    /// The move's `ui(true)` came from the room; without it the band shows from the start.
    pub(crate) landed: bool,
    pub(crate) starter: Rc<StarterStderr>,
}

/// Runs `eitri open` with its arguments parsed: the process's exit code.
pub(crate) fn run(args: OpenArgs) -> i32 {
    use eitri_core::companion::env::{inherited_editor_env, scrub, INHERITED_FROM_EDITOR};
    let mut names = INHERITED_FROM_EDITOR.to_vec();
    for name in inherited_editor_env(&|name| std::env::var_os(name)) {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    scrub(&names, &|name| {
        // SAFETY: only this thread exists: GTK has not started, and nothing above spawned a thread or a child.
        unsafe { std::env::remove_var(name) }
    });
    let starter = Rc::new(leave_editor_pipes(&args.nvim));
    crate::window_log::start_early();
    println!("eitri open: {}", args.nvim);

    // A directory named on the command line is resolved now; otherwise nvim's own working directory, once read.
    let named_root = match args.dir.as_deref().map(resolve_root).transpose() {
        Ok(root) => root,
        Err(message) => {
            say_failed(&starter, &message);
            return 64;
        }
    };
    let display_ok = gtk4::init().is_ok();
    // Without a display nothing of `init.lua` is run here: the refusal names the display.
    let (takeover_on, keys) = match display_ok
        .then(takeover_setting)
        .unwrap_or_else(|| Ok((true, WindowKeys::stock())))
    {
        Ok(read) => read,
        Err(message) => {
            say_failed(&starter, &message);
            return 64;
        }
    };
    let checks = Checks {
        addr: args.nvim.clone(),
        takeover_on,
        display_ok,
    };
    let origin = match on_worker(move || sequence::check(&checks)) {
        Ok(origin) => origin,
        Err(error) => return fail(&starter, &error),
    };
    let origin_process = origin
        .rpc()
        .peer_pid()
        .and_then(|pid| eitri_core::split::proc_start_time(pid).map(|start| (pid, start)));

    let feeds = Feeds::bind();
    let parts = feeds.parts(&keys);
    let runtime = runtime_dir();
    eitri_core::takeover::room::sweep_stale(&runtime);
    let prepared = match on_worker(move || {
        sequence::prepare(origin, &runtime, &|origin| {
            eitri_core::takeover::glue::takeover_install(origin, &parts)
        })
    }) {
        Ok(prepared) => prepared,
        Err(error) => return fail(&starter, &error),
    };
    let held = Arc::new(Held::new(prepared, args.nvim.clone(), origin_process));
    let moving = held.clone();
    let landed = match on_worker(move || sequence::move_tui(&moving.prepared)) {
        Ok(Moved::Landed) => true,
        Ok(Moved::NotLanded) => false,
        Err(error) => return fail(&starter, &error),
    };
    let root = match named_root.map_or_else(|| resolve_root(&held.prepared.cwd), Ok) {
        Ok(root) => root,
        // The room takes the terminal's UI back once this process's link to it closes.
        Err(message) => {
            say_failed(&starter, &message);
            return 1;
        }
    };
    if !landed {
        held.set_band(TERMINAL_CLOSED);
    }
    run_window(
        root,
        TakeoverStart {
            feeds,
            held: held.clone(),
            landed,
            starter,
        },
    );
    // The window has gone; a detach it started finishes before the process does, or the room never hears `let_go`.
    held.wait_detached(DETACH_DONE_WITHIN);
    EXIT.load(Ordering::SeqCst)
}

/// Where the waiting room's private directory goes: the user's runtime directory, as every socket of Eitri's.
fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .unwrap_or_else(std::env::temp_dir)
}

/// The window: `build_ui`'s full window, its editor attached to the person's nvim.
fn run_window(root: PathBuf, start: TakeoverStart) {
    use gtk4::prelude::*;
    crate::window_log::start_main(&root);
    let backend_kind = match eitri_core::agent_backend::BackendKind::from_env(false) {
        Ok(kind) => kind,
        Err(err) => {
            say_failed(&start.starter, &err.message);
            exit_with(1);
            return;
        }
    };
    let panel_notice = match crate::webkit_sandbox::decision() {
        crate::webkit_sandbox::Decision::Unavailable { reason } => {
            let notice = crate::webkit_sandbox::unavailable_notice(reason);
            eprint!("{notice}");
            Some(notice)
        }
        crate::webkit_sandbox::Decision::Available | crate::webkit_sandbox::Decision::DisabledByUser => None,
    };
    if !crate::process_memory::ui_process_jsc_without_jit() {
        eprintln!("[memory] JavaScriptCore refused useJIT=false; the UI process keeps its JIT");
    }
    println!("eitri: project root {}", root.display());
    let root_note = crate::agent_panel::root_note_for(&root);
    if let Some(note) = &root_note {
        println!("[agent] project root note: {note}");
    }
    let app = crate::build_application(crate::launch::AppKind::of(&crate::linked_window::WindowStart::Full));
    let pieces = RefCell::new(Some(start));
    app.connect_activate(move |app| {
        let Some(start) = pieces.borrow_mut().take() else {
            return;
        };
        let state = eitri_core::layout::persist::state_dir(
            std::env::var_os("XDG_STATE_HOME").as_deref(),
            std::env::var_os("HOME").as_deref(),
        );
        // Window 1 of its project when no other window holds the project; otherwise a window of its own.
        let taken = state
            .as_deref()
            .ok_or_else(|| std::io::Error::other("no state directory"))
            .and_then(|dir| eitri_core::window_link::pair::PairLock::try_take(dir, &root));
        let pair = crate::linked_window::spawn::Pair::full(taken, &root, false, false);
        crate::build_ui(
            app,
            false,
            &root,
            backend_kind,
            panel_notice.as_deref(),
            root_note.clone(),
            pair,
            None,
            Vec::new(),
            None,
            Some(start),
        );
    });
    if app.run_with_args::<&str>(&[]) != glib::ExitCode::SUCCESS {
        exit_with(1);
    }
}

/// `editor.takeover` from `init.lua`, and the window's keys for `:Eitri`, read as a companion reads its config: no
/// plugin code runs and no plugin state is written. A configuration the window could not start with leaves `:Eitri`
/// the stock keymap's words; the window itself says what is wrong with it.
fn takeover_setting() -> Result<(bool, WindowKeys), String> {
    let config_dir = crate::launch::config_dir();
    let engine = crate::lua::LuaEngine::without_plugins(config_dir.clone())
        .map_err(|e| format!("init.lua could not be read: {e}"))?;
    engine.load_init_file(&config_dir.join("init.lua"));
    let value = engine
        .config
        .borrow()
        .get(eitri_core::lua::config::EDITOR_TAKEOVER_KEY)
        .map(str::to_owned);
    let on = eitri_core::lua::config::parse_editor_takeover(value.as_deref())?;
    let keys = match crate::window_config::load(&engine) {
        Ok(config) => WindowKeys {
            keymap: config.keymap.clone(),
            nvim_prefix: config.nvim_prefix.clone(),
            pages: engine
                .recorded_plugin_keys()
                .into_iter()
                .map(|(id, key)| (eitri_core::layout::ModuleId::lua(&id), Some(key.to_string())))
                .collect(),
        },
        Err(_) => WindowKeys::stock(),
    };
    Ok((on, keys))
}

/// The project root `dir` names, checked as every start checks it.
fn resolve_root(dir: &Path) -> Result<PathBuf, String> {
    eitri_core::project_root::resolve_args(&[std::ffi::OsString::from("--"), dir.as_os_str().to_owned()])
}

/// Runs `step` on a thread of its own and waits for it: the takeover's steps block, and none belongs on the thread
/// GTK runs on.
fn on_worker<T: Send + 'static>(step: impl FnOnce() -> Result<T, StepError> + Send + 'static) -> Result<T, StepError> {
    std::thread::Builder::new()
        .name("takeover-step".into())
        .spawn(step)
        .map_err(|e| StepError::Install(e.to_string()))?
        .join()
        .unwrap_or_else(|_| Err(StepError::Install("a takeover step panicked".into())))
}

/// Moves the streams off the nvim's job pipes into the takeover's log, keeping the starter's stderr for a refusal.
fn leave_editor_pipes(addr: &str) -> StarterStderr {
    use std::io::Write;
    let starter = StarterStderr::keep();
    let dir = eitri_core::companion::stdio::log_dir(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    );
    let name = format!("takeover-{}.log", eitri_core::state_paths::project_key(Path::new(addr)));
    eitri_core::companion::stdio::leave_pipes_to(dir.as_deref(), &name, &|log| {
        // Not `eprintln!`: the nvim that started the window may already be gone, and a failed write must not panic.
        let _ = match log {
            Some(path) => writeln!(
                std::io::stderr(),
                "eitri open: running; its output goes to {}",
                path.display()
            ),
            None => writeln!(std::io::stderr(), "eitri open: running; its output is discarded"),
        };
    });
    starter
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Detach {
    NotStarted,
    Running,
    Done,
}

/// What the window and its workers share about the person's nvim.
pub(crate) struct Held {
    prepared: Prepared,
    /// The nvim's address, as the person's nvim gave it and as the close names it.
    addr: String,
    /// The nvim's process, as the kernel named the peer of the link, and its start time.
    origin_process: Option<(u32, u64)>,
    /// The editor's hang-up, kept for the detach once the editor's session is up.
    editor_link: EditorLink<neovide_editor::HangUp>,
    detach: Mutex<Detach>,
    detach_done: Condvar,
    /// The band's text once the terminal or its way back is gone.
    band: Mutex<Option<&'static str>>,
    /// The window's scratch directory, where its drafts live.
    scratch: Mutex<Option<PathBuf>>,
}

impl Held {
    fn new(prepared: Prepared, addr: String, origin_process: Option<(u32, u64)>) -> Held {
        Held {
            prepared,
            addr,
            origin_process,
            editor_link: EditorLink::new(),
            detach: Mutex::new(Detach::NotStarted),
            detach_done: Condvar::new(),
            band: Mutex::new(None),
            scratch: Mutex::new(None),
        }
    }

    fn origin(&self) -> &NvimLink {
        self.prepared.origin.rpc()
    }

    /// The glue's channel: the window's own link to the person's nvim.
    pub(crate) fn link_chan(&self) -> u64 {
        self.prepared.glue.link_chan
    }

    /// The person's nvim, at the address it was taken over from.
    pub(crate) fn addr(&self) -> &str {
        &self.addr
    }

    /// The nvim's process as the window's clipboard admits it.
    pub(crate) fn attached_nvim(&self) -> Option<eitri_core::host_clipboard::AttachedNvim> {
        self.origin_process
            .map(|(pid, start_time)| eitri_core::host_clipboard::AttachedNvim { pid, start_time })
    }

    pub(crate) fn set_scratch(&self, dir: Option<PathBuf>) {
        *lock(&self.scratch) = dir;
    }

    /// Records the band's text; `false` when one was up already.
    fn set_band(&self, text: &'static str) -> bool {
        let mut band = lock(&self.band);
        if band.is_some() {
            return false;
        }
        *band = Some(text);
        true
    }

    /// Takes the band down when it says `text`; `false` when it says something else or nothing.
    fn clear_band(&self, text: &'static str) -> bool {
        let mut band = lock(&self.band);
        if *band != Some(text) {
            return false;
        }
        *band = None;
        true
    }

    fn no_way_back(&self) -> Option<String> {
        lock(&self.band).is_some().then(|| self.addr.clone())
    }

    fn origin_alive(&self) -> bool {
        self.origin_process.is_some_and(|(pid, start)| {
            eitri_core::split::proc_start_time(pid) == Some(start) && !process_is_zombie(pid)
        })
    }

    fn detach_done(&self) {
        *lock(&self.detach) = Detach::Done;
        self.detach_done.notify_all();
    }

    /// Whether this caller is the one to run the detach: the first claim is, and from then on the process waits for
    /// the detach to end before it exits.
    fn claim_detach(&self) -> bool {
        let mut state = lock(&self.detach);
        if *state != Detach::NotStarted {
            return false;
        }
        *state = Detach::Running;
        true
    }

    /// The whole detach, once: the glue's teardown, the editor's hang-up, then the room takes the terminal's UI back
    /// (or, with the terminal gone, is told to quit). A second caller waits for the first one's end.
    fn detach(self: &Arc<Self>) {
        if self.claim_detach() {
            self.run_detach(None);
        } else {
            self.wait_detached(DETACH_DONE_WITHIN);
        }
    }

    /// The detach a claim gave this caller. `open_views`: the proposal views the window's IDE host holds, when the
    /// caller could read them.
    fn run_detach(self: &Arc<Self>, open_views: Option<usize>) {
        // What ends with the window goes to the log on every detach: a close that could not ask (a signal, a hand-back,
        // or one that came after a declined question) has only the log to name it.
        let asks = self.close_asks(open_views);
        if asks.drafts > 0 || asks.proposals > 0 {
            println!(
                "[takeover] ending with the window: {}",
                eitri_core::wm::feedback::unsaved_in_editor_prompt(asks.drafts, asks.proposals)
                    .trim_end_matches(", close anyway? (y/n)")
            );
        }
        let no_way_back = self.no_way_back();
        if let Some(addr) = &no_way_back {
            println!(
                "[takeover] {}",
                eitri_core::wm::feedback::no_way_back_prompt(addr).trim_end_matches(" Close? (y/n)")
            );
        }
        let started = Instant::now();
        // nvim runs the teardown when it can; one waiting for a key misses this bound, and its glue goes when the
        // link closes.
        let torn = self
            .origin()
            .exec_lua(TEARDOWN_LUA, vec![self.link_chan().into()])
            .wait(DETACH_TEARDOWN);
        println!(
            "[takeover] the glue's teardown {} after {:?}",
            match torn {
                Some(Ok(_)) => "answered",
                Some(Err(_)) => "failed",
                None => "did not answer",
            },
            started.elapsed()
        );
        match self.editor_link.let_go() {
            Some(hang_up) => hang_up.hang_up(),
            None => println!("[takeover] the editor is not attached yet: it lets go as soon as it is"),
        }
        let held = self.clone();
        let answer = if no_way_back.is_some() {
            println!("[takeover] the terminal is gone: the room is told to quit");
            bounded(DETACH_ROOM_WAIT, move || {
                held.prepared.room.quit().map(|()| LetGo::Returned)
            })
        } else {
            bounded(DETACH_ROOM_WAIT, move || held.prepared.room.let_go())
        };
        match answer {
            Some(Ok(LetGo::Returned)) => println!("[takeover] the terminal has nvim back"),
            // The room quits once this process's link to it closes, unless a UI reaches it before then.
            Some(Ok(LetGo::NoUi)) => {
                println!("[takeover] {TERMINAL_CLOSED}: nvim --remote-ui --server {}", self.addr)
            }
            Some(Err(error)) => println!("[takeover] the room did not take nvim back: {}", step_text(&error)),
            None => println!("[takeover] the room did not answer within {DETACH_ROOM_WAIT:?}"),
        }
        self.detach_done();
    }

    /// Waits, at most `within`, for a detach under way to end.
    fn wait_detached(&self, within: Duration) {
        let state = lock(&self.detach);
        let _ = self
            .detach_done
            .wait_timeout_while(state, within, |state| *state == Detach::Running);
    }

    /// What a close asks about: whether nvim waits for a key, and the drafts and proposals that end with the window.
    /// nvim says which drafts have changes and which proposal views it shows; what it does not answer in time (a nvim
    /// waiting for a key answers only its mode) is read from the window's own side instead: every draft file still
    /// open in the editor, and `open_views`, the proposal views the window's IDE host holds.
    fn close_asks(&self, open_views: Option<usize>) -> CloseAsks {
        let deadline = Instant::now() + FACTS_WITHIN;
        let link = self.origin();
        let mode = link
            .call("nvim_get_mode", vec![])
            .wait(FACTS_WITHIN)
            .and_then(Result::ok);
        let left = || deadline.saturating_duration_since(Instant::now());
        let blocking = mode.as_ref().is_none_or(|mode| {
            field(mode, "blocking").and_then(Value::as_bool) != Some(false)
                || field(mode, "mode")
                    .and_then(Value::as_str)
                    .is_none_or(|mode| mode.starts_with('r') || mode == "!")
        });
        let scratch = lock(&self.scratch).clone();
        let mut asks = CloseAsks {
            blocking,
            no_way_back: self.no_way_back(),
            drafts: scratch.as_deref().map_or(0, open_drafts),
            proposals: open_views.unwrap_or(0),
        };
        // A waiting nvim would queue every question: only one that answered is asked more.
        if blocking {
            return asks;
        }
        if let Some(scratch) = scratch {
            let buffers = link.call("nvim_list_bufs", vec![]).wait(left()).and_then(Result::ok);
            if let Some(buffers) = buffers.as_ref().and_then(Value::as_array) {
                let asked: Vec<(Pending, Pending)> = buffers
                    .iter()
                    .map(|buf| {
                        let options = Value::Map(vec![("buf".into(), buf.clone())]);
                        (
                            link.call("nvim_buf_get_name", vec![buf.clone()]),
                            link.call("nvim_get_option_value", vec!["modified".into(), options]),
                        )
                    })
                    .collect();
                let answers: Vec<_> = asked
                    .into_iter()
                    .map(|(name, modified)| (name.wait(left()), modified.wait(left())))
                    .collect();
                // Every buffer answered: nvim's count, of the drafts with changes, stands for the files'.
                if answers
                    .iter()
                    .all(|(name, modified)| name.is_some() && modified.is_some())
                {
                    asks.drafts = answers
                        .iter()
                        .filter(|(name, modified)| {
                            let name = name.as_ref().and_then(|name| name.as_ref().ok());
                            let modified = modified.as_ref().and_then(|modified| modified.as_ref().ok());
                            name.and_then(Value::as_str).is_some_and(|name| {
                                eitri_core::scratch::holds(&scratch, name) && name.ends_with("-draft.md")
                            }) && modified.and_then(Value::as_bool) == Some(true)
                        })
                        .count();
                }
            }
        }
        let windows = link.call("nvim_list_wins", vec![]).wait(left()).and_then(Result::ok);
        if let Some(windows) = windows.as_ref().and_then(Value::as_array) {
            let marks: Vec<Pending> = windows
                .iter()
                .map(|win| link.call("nvim_win_get_var", vec![win.clone(), PROPOSAL_MARK.into()]))
                .collect();
            let marks: Vec<_> = marks.into_iter().map(|mark| mark.wait(left())).collect();
            // A window that did not answer may be a proposal's: then the host's count stands.
            if marks.iter().all(Option::is_some) {
                let mut views: Vec<Value> = Vec::new();
                for view in marks.into_iter().flatten().filter_map(Result::ok) {
                    if !views.contains(&view) {
                        views.push(view);
                    }
                }
                asks.proposals = views.len();
            }
        }
        asks
    }
}

/// The drafts the window has open in the editor, read from its scratch directory: a draft's file whose end marker nvim
/// has not written yet. Whether one has changes only nvim can say, so each counts as having them.
fn open_drafts(scratch: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(scratch) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.strip_suffix("-draft.md")
                .is_some_and(|id| !scratch.join(format!("{id}-draft.done")).exists())
        })
        .count()
}

/// Runs `step` on a thread of its own, waiting at most `within` for its answer; a late answer is dropped with the
/// thread that made it.
fn bounded<T: Send + 'static>(within: Duration, step: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("takeover-bounded".into())
        .spawn(move || {
            let _ = tx.send(step());
        })
        .ok()?;
    rx.recv_timeout(within).ok()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn field<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
    value
        .as_map()?
        .iter()
        .find(|(key, _)| key.as_str() == Some(name))
        .map(|(_, value)| value)
}

/// Whether `pid` has exited and waits for its parent to reap it: gone, for this window.
fn process_is_zombie(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return true;
    };
    stat.rfind(')')
        .and_then(|end| stat[end + 1..].split_whitespace().next().map(str::to_owned))
        .is_some_and(|state| state == "Z" || state == "X")
}

/// The editor's hang-up as the detach and the editor's session reach it, in either order: the detach hangs up a
/// session that is up, and a session that comes up after the detach is hung up at once.
struct EditorLink<H>(Mutex<LinkSlot<H>>);

enum LinkSlot<H> {
    Waiting,
    Up(H),
    LetGo,
}

impl<H> EditorLink<H> {
    fn new() -> Self {
        EditorLink(Mutex::new(LinkSlot::Waiting))
    }

    /// The editor's session is up with `handle`: kept for the detach, or, once the detach has let go, given back
    /// as `Err` to hang up now.
    fn ready(&self, handle: Option<H>) -> Result<(), Option<H>> {
        let mut slot = lock(&self.0);
        if matches!(*slot, LinkSlot::LetGo) {
            return Err(handle);
        }
        if let Some(handle) = handle {
            *slot = LinkSlot::Up(handle);
        }
        Ok(())
    }

    /// The detach lets go: the handle to hang up now when the session is up. From here on a session that comes up
    /// is hung up at once.
    fn let_go(&self) -> Option<H> {
        match std::mem::replace(&mut *lock(&self.0), LinkSlot::LetGo) {
            LinkSlot::Up(handle) => Some(handle),
            LinkSlot::Waiting | LinkSlot::LetGo => None,
        }
    }
}

/// What a worker tells the window.
enum Notice {
    Detached,
    CloseFacts { generation: u32, asks: CloseAsks },
    HandBack(HandBackWhy),
    Band(&'static str),
    BandCleared,
    AttachFailed(String),
}

/// The window's side of a takeover, on the GTK thread: the workers' notices reach the window's controller and band
/// through it.
pub(crate) struct WindowSide {
    held: Arc<Held>,
    notices: futures_channel::mpsc::UnboundedSender<Notice>,
    band: gtk4::Label,
    attached_once: Cell<bool>,
}

impl WindowSide {
    /// The window's side, its band hidden until the terminal or its way back is gone, and the link worker started.
    pub(crate) fn start(
        held: Arc<Held>,
        sink: Weak<dyn HostSink>,
        landed: bool,
        starter: Rc<StarterStderr>,
    ) -> Rc<WindowSide> {
        use gtk4::prelude::*;
        let band = gtk4::Label::new(None);
        band.add_css_class("takeover-band");
        band.set_halign(gtk4::Align::Fill);
        band.set_valign(gtk4::Align::End);
        band.set_wrap(true);
        band.set_visible(false);
        let (notices, mut taken) = futures_channel::mpsc::unbounded::<Notice>();
        let side = Rc::new(WindowSide {
            held: held.clone(),
            notices,
            band,
            attached_once: Cell::new(false),
        });
        if !landed {
            side.show_band(TERMINAL_CLOSED);
        }
        let weak = Rc::downgrade(&side);
        glib::spawn_future_local(async move {
            while let Ok(notice) = taken.recv().await {
                let Some(side) = weak.upgrade() else { return };
                let post = |event: EditorEvent| {
                    if let Some(sink) = sink.upgrade() {
                        sink.post(HostEvent::Editor(event));
                    }
                };
                let win = WinId::MAIN;
                match notice {
                    Notice::Detached => post(EditorEvent::Detached { win }),
                    Notice::CloseFacts { generation, asks } => post(EditorEvent::CloseFacts { win, generation, asks }),
                    Notice::HandBack(why) => post(EditorEvent::HandedBack { win, why }),
                    Notice::Band(text) => side.show_band(text),
                    Notice::BandCleared => side.hide_band(),
                    Notice::AttachFailed(line) => {
                        say_failed(&starter, &line);
                        exit_with(1);
                        post(EditorEvent::HandedBack {
                            win,
                            why: HandBackWhy::AttachFailed,
                        });
                    }
                }
            }
        });
        link_worker(held, side.notices.clone());
        side
    }

    /// What the window's workers share: the window's own parts ask it for the nvim's address and channel.
    pub(crate) fn held(&self) -> Arc<Held> {
        self.held.clone()
    }

    /// The band's widget, laid over the window's content by `build_ui`.
    pub(crate) fn band(&self) -> &gtk4::Label {
        &self.band
    }

    fn show_band(&self, text: &str) {
        use gtk4::prelude::*;
        println!("[takeover] band: {text}");
        self.band.set_text(text);
        self.band.set_visible(true);
    }

    fn hide_band(&self) {
        use gtk4::prelude::*;
        println!("[takeover] band cleared: the terminal's UI is in the room");
        self.band.set_visible(false);
    }

    /// The editor's session is up on the person's nvim: its hang-up handle is kept for the detach, and the attach is
    /// checked on a worker (one UI, the editor's own), after which the room is told it holds the terminal's UI.
    pub(crate) fn session_ready(&self, pane: &neovide_editor::NeovideEditorPane) {
        if self.attached_once.replace(true) {
            return;
        }
        // nvim went back before the editor's session was up (Enter in the room, another UI, a lost link): the
        // editor lets go at once. Its channel's end takes out what the fork put in, as nothing of Eitri's is left
        // there to do it.
        if let Err(late) = self.held.editor_link.ready(pane.hang_up_handle()) {
            println!("[takeover] the editor attached after nvim was handed back: it lets go at once");
            if let Some(hang_up) = late {
                hang_up.hang_up();
            }
            return;
        }
        println!("[takeover] the window's editor shows {}", self.held.addr);
        let held = self.held.clone();
        let notices = self.notices.clone();
        spawn("takeover-attach", move || {
            let chan = held
                .origin()
                .exec_lua(FORK_CHAN_LUA, vec![])
                .wait(FORK_CHAN_WITHIN)
                .and_then(Result::ok)
                .and_then(|value| value.as_u64());
            let Some(chan) = chan else {
                let _ = notices.unbounded_send(Notice::AttachFailed("the editor's attach recorded no channel".into()));
                return;
            };
            match sequence::after_attach(&held.prepared, chan) {
                Ok(()) => println!("[takeover] attached: nvim's one UI is the editor's channel {chan}"),
                Err(StepError::WayBackGone) => {
                    println!("[takeover] attached, and the room does not answer");
                    if held.set_band(WAY_BACK_GONE) {
                        let _ = notices.unbounded_send(Notice::Band(WAY_BACK_GONE));
                    }
                }
                Err(error) => {
                    // The step tore the glue down, let go of the editor's channel and told the room itself.
                    held.detach_done();
                    let _ = notices.unbounded_send(Notice::AttachFailed(step_text(&error)));
                }
            }
        });
    }

    /// The editor could not attach to the person's nvim after the terminal's UI moved into the room: nvim goes back to
    /// its terminal (the whole detach, on a worker), and the window ends with exit 1. A session that was up ends
    /// through [`WindowSide::session_ended`] instead.
    pub(crate) fn start_failed(&self, why: &str) {
        if self.attached_once.get() {
            return;
        }
        let first = why.lines().find(|line| !line.trim().is_empty()).unwrap_or(why).trim();
        let line = format!("the window's editor could not attach to nvim: {first}");
        println!("[takeover] {line}");
        let held = self.held.clone();
        let notices = self.notices.clone();
        spawn("takeover-detach", move || {
            held.detach();
            let _ = notices.unbounded_send(Notice::AttachFailed(line));
        });
    }

    /// The editor's session ended; whether this window answers the end itself. A `:connect` typed in the editor
    /// hands the nvim it left back to its terminal; a `:restart` ends that nvim as `:qa` does, and the log names the
    /// new server; Eitri's own hang-up is answered by its detach.
    pub(crate) fn session_ended(&self, ended: Option<&neovide_editor::PaneEnded>) -> bool {
        use neovide_editor::{Left, PaneEnded};
        match ended {
            Some(PaneEnded::SessionLeft(Left::Connect)) => {
                println!("[takeover] `:connect` moved the editor to another nvim");
                let held = self.held.clone();
                let notices = self.notices.clone();
                spawn("takeover-detach", move || {
                    held.detach();
                    let _ = notices.unbounded_send(Notice::HandBack(HandBackWhy::Connected));
                });
                true
            }
            Some(PaneEnded::SessionLeft(Left::Restart { addr })) => {
                match addr {
                    Some(addr) => println!("[takeover] nvim restarted at {addr}: nvim --remote-ui --server {addr}"),
                    None => println!("[takeover] nvim restarted with no address to reach it at"),
                }
                false
            }
            Some(PaneEnded::Detached) => true,
            Some(PaneEnded::Exited) | None => false,
        }
    }

    /// The close's `Detach`: the whole detach on a worker, answered with `Detached`. Claimed here, on the GTK thread,
    /// so a window that quits at once still waits for it before the process ends. `open_views`: the proposal views the
    /// window's IDE host holds.
    pub(crate) fn detach(&self, open_views: usize) {
        let held = self.held.clone();
        let notices = self.notices.clone();
        let claimed = held.claim_detach();
        spawn("takeover-detach", move || {
            if claimed {
                held.run_detach(Some(open_views));
            } else {
                held.wait_detached(DETACH_DONE_WITHIN);
            }
            let _ = notices.unbounded_send(Notice::Detached);
        });
    }

    /// The close's ask: answered on a worker with the same generation. `open_views`: the proposal views the window's
    /// IDE host holds, which a nvim waiting for a key cannot name.
    pub(crate) fn ask_close_facts(&self, generation: u32, open_views: usize) {
        let held = self.held.clone();
        let notices = self.notices.clone();
        spawn("takeover-close-facts", move || {
            let asks = held.close_asks(Some(open_views));
            let _ = notices.unbounded_send(Notice::CloseFacts { generation, asks });
        });
    }
}

fn spawn(name: &str, work: impl FnOnce() + Send + 'static) {
    if let Err(error) = std::thread::Builder::new().name(name.into()).spawn(work) {
        eprintln!("[takeover] {name} could not start: {error}");
    }
}

/// The glue's notice, as the hand-back it is.
fn hand_back_for(notice: &GlueNotice) -> HandBackWhy {
    match notice {
        GlueNotice::UiEntered => HandBackWhy::OtherUi,
        GlueNotice::Replaced => HandBackWhy::Replaced,
    }
}

/// Reads the glue's notices on the link to the person's nvim and the room's events. A hand-back is detached here,
/// before the window hears of it, so the terminal never waits on a question or a stuck GTK thread.
fn link_worker(held: Arc<Held>, notices: futures_channel::mpsc::UnboundedSender<Notice>) {
    let link_events = held.prepared.origin.events();
    let room_events = held.prepared.events();
    let hand_back = {
        let held = held.clone();
        let notices = notices.clone();
        move |why: HandBackWhy| {
            println!("[takeover] handing nvim back: {why:?}");
            held.detach();
            let _ = notices.unbounded_send(Notice::HandBack(why));
        }
    };
    let link_hand_back = hand_back.clone();
    let link_held = held.clone();
    spawn("takeover-link", move || {
        for event in link_events {
            match event {
                LinkEvent::Notification { method, .. } => {
                    let notice = match method.as_str() {
                        "eitri_ui_entered" => GlueNotice::UiEntered,
                        "eitri_replaced" => GlueNotice::Replaced,
                        _ => continue,
                    };
                    link_hand_back(hand_back_for(&notice));
                    return;
                }
                LinkEvent::Closed => break,
            }
        }
        // A detach of the window's own closes the link too.
        if *lock(&link_held.detach) != Detach::NotStarted {
            return;
        }
        // nvim's own exit closes the link first; the editor's end reports that exit. Only a link lost while nvim
        // lives is a hand-back.
        let until = Instant::now() + EXIT_GRACE;
        while Instant::now() < until {
            if !link_held.origin_alive() {
                println!("[takeover] the link to nvim closed with nvim's exit");
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        link_hand_back(HandBackWhy::LinkLost);
    });
    spawn("takeover-room", move || {
        for event in room_events.iter() {
            match event {
                RoomEvent::TakeBack => {
                    hand_back(HandBackWhy::TakeBack);
                    return;
                }
                RoomEvent::Ui(false) => {
                    if held.set_band(TERMINAL_CLOSED) {
                        let _ = notices.unbounded_send(Notice::Band(TERMINAL_CLOSED));
                    }
                }
                // The terminal's UI reached the room late (a move that did not land in time, a terminal that was
                // stopped): the way back is there again, and a close gives nvim back to it.
                RoomEvent::Ui(true) => {
                    if held.clear_band(TERMINAL_CLOSED) {
                        let _ = notices.unbounded_send(Notice::BandCleared);
                    }
                }
                RoomEvent::OriginUnreachable => {
                    if held.set_band(WAY_BACK_GONE) {
                        let _ = notices.unbounded_send(Notice::Band(WAY_BACK_GONE));
                    }
                }
            }
        }
        // The room's link closed: unless this window let it go, the way back went with it.
        if *lock(&held.detach) == Detach::NotStarted && held.set_band(WAY_BACK_GONE) {
            let _ = notices.unbounded_send(Notice::Band(WAY_BACK_GONE));
        }
    });
}

/// The window's handle on the person's nvim: every call goes through the pane, whose chunk check ([`chunk_check`])
/// refuses what the table refuses and gives a registered chunk the glue's channel. It says it reaches a taken-over
/// nvim, and which glue it holds there.
pub(crate) struct HeldEditorRpc {
    pub(crate) inner: Rc<dyn EditorRpc>,
    pub(crate) link_chan: u64,
}

impl EditorRpc for HeldEditorRpc {
    fn exec_lua(&self, code: &'static str, args: Vec<Value>) -> Pending {
        // editor-rpc-scan: forwards a caller's constant
        self.inner.exec_lua(code, args)
    }
    fn target(&self) -> Option<u64> {
        self.inner.target()
    }
    fn attached(&self) -> bool {
        true
    }
    fn glue(&self) -> Option<Glue> {
        Some(Glue {
            slot: GlueSlot::Agent,
            chan: self.link_chan,
        })
    }
    fn on_screen(&self) -> bool {
        self.inner.on_screen()
    }
}

/// The transport [`chunk_rule`] runs the takeover's rule over: it keeps the arguments the rule would send.
struct Kept(RefCell<Option<Vec<Value>>>);

impl EditorRpc for Kept {
    fn exec_lua(&self, _code: &'static str, args: Vec<Value>) -> Pending {
        *self.0.borrow_mut() = Some(args);
        let (answer, pending) = Pending::pair();
        answer.send(Ok(Value::Nil));
        pending
    }
    fn target(&self) -> Option<u64> {
        Some(0)
    }
}

/// The takeover's own rule (`TakeoverRpc`), as the arguments it would send: a chunk the table refuses or does not know
/// is a refusal and is never sent; a registered chunk gets the glue's channel appended.
fn chunk_rule(link_chan: u64, code: &'static str, args: Vec<Value>) -> Result<Vec<Value>, String> {
    let kept = Rc::new(Kept(RefCell::new(None)));
    let rule = eitri_core::editor_rpc::TakeoverRpc::new(kept.clone(), link_chan);
    // editor-rpc-scan: forwards a caller's constant
    let pending = rule.exec_lua(code, args);
    let sent = kept.0.borrow_mut().take();
    sent.ok_or_else(|| match pending.wait(Duration::ZERO) {
        Some(Err(RpcError::Unavailable(why))) => why,
        Some(Err(error)) => error.to_string(),
        _ => eitri_core::editor_rpc::ChunkRefused.to_string(),
    })
}

/// The attached pane's check: [`chunk_rule`] for the glue's channel, before anything is written to nvim.
pub(crate) fn chunk_check(link_chan: u64) -> neovide_editor::ChunkCheck {
    neovide_editor::ChunkCheck::new(move |code, args| {
        chunk_rule(link_chan, code, args).map_err(|reason| neovide_editor::ChunkRefused { reason })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_panes_rule_refuses_what_the_table_refuses_and_gives_a_registered_chunk_the_glue_channel() {
        assert!(chunk_rule(42, eitri_core::companion::INSTALL_LUA, vec![]).is_err());
        assert!(
            chunk_rule(42, "return 1", vec![]).is_err(),
            "a chunk the table does not know"
        );
        assert_eq!(
            chunk_rule(42, TEARDOWN_LUA, vec![7.into()]).unwrap(),
            vec![Value::from(7)],
            "a one-shot chunk goes as it is"
        );
        assert_eq!(
            chunk_rule(
                42,
                eitri_core::editor_tools::tools::lsp::LSP_TARGETS_LUA,
                vec![1.into()]
            )
            .unwrap(),
            vec![Value::from(1), Value::Map(vec![("takeover_chan".into(), 42.into())])]
        );
    }

    #[test]
    fn refusals_say_their_exact_copy() {
        assert_eq!(
            step_text(&StepError::Refused(Refusal::TooOld("0.11.2".into()))),
            "Eitri's takeover needs nvim 0.12 or later (this is 0.11.2)"
        );
        assert_eq!(
            step_text(&StepError::Refused(Refusal::Off)),
            "takeover is off (editor.takeover in init.lua)"
        );
        assert_eq!(
            step_text(&StepError::Refused(Refusal::Blocking)),
            "nvim is waiting for a key (or busy)"
        );
        assert_eq!(step_text(&StepError::TuiStayed), "the terminal's UI did not leave");
    }

    #[test]
    fn the_fork_channel_chunk_is_classified_and_never_reaches_the_editor() {
        assert_eq!(
            eitri_core::editor_rpc::check_chunk(FORK_CHAN_LUA),
            Ok(eitri_core::editor_rpc::ChunkSide::Refused)
        );
        assert!(chunk_rule(42, FORK_CHAN_LUA, vec![]).is_err());
    }

    #[test]
    fn a_draft_still_open_in_the_editor_counts_until_nvim_marks_its_end() {
        let dir = std::env::temp_dir().join(format!("takeover-drafts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(open_drafts(&dir), 0, "no draft");
        std::fs::write(dir.join("3-draft.md"), "text").unwrap();
        std::fs::write(dir.join("4-view.md"), "a view").unwrap();
        std::fs::write(dir.join("5-draft.md"), "text").unwrap();
        assert_eq!(open_drafts(&dir), 2, "two drafts open, a view is no draft");
        std::fs::write(dir.join("5-draft.done"), "written").unwrap();
        assert_eq!(open_drafts(&dir), 1, "a draft nvim has ended is not open");
        assert_eq!(open_drafts(&dir.join("gone")), 0, "no directory");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_up_after_the_detach_is_hung_up_at_once() {
        // The usual order: the session comes up, the detach hangs it up.
        let link = EditorLink::new();
        assert_eq!(link.ready(Some(1)), Ok(()));
        assert_eq!(link.let_go(), Some(1));
        assert_eq!(link.ready(Some(2)), Err(Some(2)), "a session after the let-go is kept");

        // The detach before the session: nothing to hang up then, and the session's handle comes back to hang up.
        let link = EditorLink::<u32>::new();
        assert_eq!(link.let_go(), None);
        assert_eq!(link.ready(Some(3)), Err(Some(3)));
        assert_eq!(link.let_go(), None, "a second let-go gave a handle to hang up again");
    }

    #[test]
    fn the_close_facts_answer_inside_the_controllers_wait() {
        assert!(
            FACTS_WITHIN + Duration::from_millis(40) <= eitri_core::wm::close::CLOSE_FACTS_WITHIN,
            "{FACTS_WITHIN:?} leaves no room inside {:?}",
            eitri_core::wm::close::CLOSE_FACTS_WITHIN
        );
    }

    fn no_feeds() -> Feeds {
        Feeds {
            theme: None,
            context: None,
            keys: None,
            pane_switch: None,
        }
    }

    #[test]
    fn eitri_in_the_persons_nvim_gets_the_windows_prefix_group() {
        let stock = no_feeds().parts(&WindowKeys::stock());
        assert!(stock.commands.group.is_none(), "a group with no keymap.nvim_prefix");

        let keys = WindowKeys {
            nvim_prefix: Some("<leader>W".into()),
            ..WindowKeys::stock()
        };
        let parts = no_feeds().parts(&keys);
        let (lhs, group) = parts.commands.group.expect("no group under keymap.nvim_prefix");
        assert_eq!(lhs, "<leader>W");
        assert!(!group.is_empty(), "the group has no keys");
    }

    #[test]
    fn a_glue_notice_is_the_hand_back_it_names() {
        assert_eq!(hand_back_for(&GlueNotice::UiEntered), HandBackWhy::OtherUi);
        assert_eq!(hand_back_for(&GlueNotice::Replaced), HandBackWhy::Replaced);
    }
}
