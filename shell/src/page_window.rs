//! `eitri page <id> [--nvim <addr>] [DIR]`: one plugin's page in a window of its own beside the person's own nvim.
//!
//! The window is `build_ui`'s module-window branch under the shell role `WindowRole::Beside`: no window 1, no pair,
//! no editor of its own. Its editor is the person's nvim, reached through its own [`CompanionLink`], which installs the
//! page's slot there; the page's follow snippet joins that slot. What its keys and follow commands do is the window
//! controller's (`ModuleWindowKind::Beside`); what the controller asks of the windows around it ([`BesideOp`]) is
//! carried out here, every start and socket call on a worker thread.
//!
//! Everything before GTK happens on the one thread there is, in this order: the arguments (reading `$NVIM`), the scrub
//! of what an nvim `:terminal` hands down, the project, the plugin's check, the control socket's claim (one window per
//! nvim and plugin; a second start raises the first), and leaving the starting nvim's pipes.

use std::cell::RefCell;
use std::ffi::OsString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU8, Ordering};

use gtk4::glib;

use eitri_core::companion::attach::{Detach, LinkState};
use eitri_core::companion::link::CompanionLink;
use eitri_core::companion::{InstallKind, InstallRefusal, Sockets};
use eitri_core::panel_control::{self, Claim, ControlKey, ControlServer, Forwarded, Reply, Request};
use eitri_core::plugin::search::SearchEnv;
use eitri_core::project_root::PageArgs;
use eitri_core::window_link::names::WindowName;
use eitri_core::wm::BesideOp;

/// The exit of a start refused before anything was shown: its arguments, the whitelist, an install the editor
/// refused.
const REFUSED: u8 = 64;
/// The exit of a page that cannot run: no such plugin, an attach that failed, a follow snippet the editor refused.
const FAILED: u8 = 1;

/// Logged when the link to the editor ends after it was attached.
pub(crate) const WENT_AWAY: &str = "[page] the editor went away; closing";
/// Logged when the editor refuses the page's follow snippet: the slot is gone or another channel's.
pub(crate) const FOLLOW_REFUSED: &str = "[page] the editor refused the page's follow snippet; closing";
/// Logged when another install took the page's slot over (a takeover of the nvim).
const MOVED: &str = "[page] this nvim moved into an Eitri window; closing";
/// What the page install answers in Eitri's own nvim, which opens its pages itself.
const EITRI_WINDOW_TEXT: &str = "this window opens pages itself: press the prefix and the page's key";
/// What the page install answers in an nvim an Eitri window has taken over.
const TAKEOVER_TEXT: &str = "this nvim is open in an Eitri window: its pages open there";

/// The code the process ends with once the window has closed; 0 until a close decided another. The first decided
/// code stays: a link that ends while a refused close is under way does not turn it into a clean one.
static EXIT: AtomicU8 = AtomicU8::new(0);

/// The process ends with `code` when the window closes.
fn exit_with(code: u8) {
    let _ = EXIT.compare_exchange(0, code, Ordering::SeqCst, Ordering::SeqCst);
}

fn named(message: &str) -> String {
    if message.starts_with("eitri page:") {
        message.to_owned()
    } else {
        format!("eitri page: {message}")
    }
}

fn exit_now(code: u8, message: &str) -> glib::ExitCode {
    eprintln!("{}", named(message));
    glib::ExitCode::from(code)
}

/// `eitri page`'s command line, after `page`: `$NVIM` read before anything else can scrub it, as the default address.
pub(crate) fn run_args(args: &[OsString]) -> glib::ExitCode {
    let nvim_env = std::env::var_os("NVIM").filter(|value| !value.is_empty());
    match eitri_core::project_root::parse_page_args(args) {
        Ok(mut page) => {
            if page.nvim.is_none() {
                page.nvim = nvim_env.map(PathBuf::from);
            }
            run(page)
        }
        Err(message) => exit_now(REFUSED, &message),
    }
}

/// Runs `eitri page` with its arguments parsed and `--nvim` defaulted.
pub(crate) fn run(args: PageArgs) -> glib::ExitCode {
    use eitri_core::companion::env::{inherited_editor_env, scrub, INHERITED_FROM_EDITOR};
    use gtk4::prelude::*;
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
    let search = SearchEnv::from_process();
    if args.list {
        print!("{}", list_text(&search));
        return glib::ExitCode::SUCCESS;
    }
    let Some(addr) = args.nvim.clone() else {
        return exit_now(REFUSED, "needs --nvim <address> when $NVIM is not set");
    };
    // SAFETY: `geteuid` takes no arguments and cannot fail.
    if let Err(message) = panel_control::validate_nvim_addr(&addr, unsafe { libc::geteuid() }) {
        return exit_now(REFUSED, &message);
    }
    let rest: Vec<OsString> = args
        .dir
        .clone()
        .map(|dir| vec![OsString::from("--"), dir.into_os_string()])
        .unwrap_or_default();
    let project_root = match eitri_core::project_root::resolve_args(&rest) {
        Ok(root) => root,
        Err(message) => return exit_now(REFUSED, &message),
    };
    if let Err((code, message)) = check_plugin(&args.id, &search) {
        return exit_now(code, &message);
    }
    // One window per nvim and plugin: a second start hands the first a raise and ends.
    let control_dir = panel_control::control_dir(std::env::var_os("XDG_RUNTIME_DIR").as_deref(), &std::env::temp_dir());
    let key = ControlKey::Page {
        addr: &addr,
        id: &args.id,
    };
    let server = match panel_control::claim_for(&control_dir, &key, Some(&Request::Raise)) {
        Ok(Claim::Ours(server)) => server,
        Ok(Claim::Forwarded(Reply::Ok)) => {
            println!("eitri page: raised the running page window for {}", args.id);
            return glib::ExitCode::SUCCESS;
        }
        Ok(Claim::Forwarded(Reply::Refused(why))) => {
            return exit_now(FAILED, &format!("the running page window refused: {why}"))
        }
        Ok(Claim::Unresponsive { pid }) => {
            return exit_now(
                FAILED,
                &format!("a page window for this nvim is running but not answering (pid {pid})"),
            )
        }
        Err(message) => return exit_now(FAILED, &message),
    };
    let starter = leave_editor_pipes(page_log_name(&control_dir, &key).as_deref());
    println!("eitri page: {} for {}", args.id, project_root.display());
    let backend_kind = match eitri_core::agent_backend::BackendKind::from_env(false) {
        Ok(kind) => kind,
        Err(err) => {
            starter.say(&named(&err.message));
            return exit_now(FAILED, &err.message);
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
    let root_note = crate::agent_panel::root_note_for(&project_root);
    // Stats, so here rather than on the GTK thread.
    let exe = crate::project_start::exe_to_start(std::env::current_exe(), &|path| path.exists());
    let start = crate::linked_window::WindowStart::Beside {
        name: WindowName::Page(args.id.clone()),
        nvim: addr,
        root: project_root,
    };
    let app = crate::build_application(crate::AppKind::of(&start));
    let pieces = RefCell::new(Some((start, server, starter, control_dir)));
    app.connect_activate(move |app| {
        // `activate` comes once for a window that opts out of single-instance behaviour.
        let Some((start, server, starter, control_dir)) = pieces.borrow_mut().take() else {
            return;
        };
        let crate::linked_window::WindowStart::Beside { name, nvim, root } = start else {
            return;
        };
        let WindowName::Page(id) = name.clone() else {
            return;
        };
        // The theme part's socket is the one thing the page slot is given; it is bound before the link exists, so
        // the first install carries it.
        let theme_feed = crate::theme::feed::ThemeFeed::new();
        let sockets = Sockets {
            theme: theme_feed.as_ref().map(|feed| feed.socket_path().to_path_buf()),
            ..Sockets::default()
        };
        let link = Rc::new(CompanionLink::new(
            Some(nvim.clone()),
            sockets,
            InstallKind::Page { id },
        ));
        // Nothing is built or shown until the page's slot is installed: a refusal or a failed attach ends the start
        // with no window. Held meanwhile, as the application has no window to keep it running.
        let hold = app.hold();
        let (app, panel_notice, root_note, exe) = (app.clone(), panel_notice.clone(), root_note.clone(), exe.clone());
        await_install(link.clone(), move |installed| {
            let _hold = hold;
            if let Err((code, line, to_starter)) = installed {
                link.shutdown();
                server.cleanup();
                say_closing(&starter, code, &line, to_starter);
                app.quit();
                return;
            }
            let pair = crate::linked_window::spawn::Pair::beside(name, &root, link);
            crate::build_ui(
                &app,
                false,
                &root,
                backend_kind,
                panel_notice.as_deref(),
                root_note,
                pair,
                None,
                Vec::new(),
                Some(BesideStart {
                    theme_feed,
                    server: Rc::new(server),
                    nvim,
                    starter: Rc::new(starter),
                    control_dir,
                    exe,
                }),
            );
        });
    });
    let code = app.run_with_args::<&str>(&[]);
    match EXIT.load(Ordering::SeqCst) {
        0 => code,
        decided => glib::ExitCode::from(decided),
    }
}

/// The plugin `id` on the search path, with a manifest a page window can honour: missing is exit 1, a capability
/// outside the whitelist (or a `[window]` table) exit 64.
fn check_plugin(id: &str, search: &SearchEnv) -> Result<(), (u8, String)> {
    let found = eitri_core::plugin::search::find_plugin(id, search)
        .map_err(|_| (FAILED, format!("no plugin \"{id}\" on the plugin search path")))?;
    let text = std::fs::read_to_string(found.dir.join(eitri_core::plugin::search::MANIFEST))
        .map_err(|e| (FAILED, format!("{id}: its manifest cannot be read: {e}")))?;
    let manifest = eitri_core::plugin::manifest::parse_manifest(&text).map_err(|e| (FAILED, format!("{id}: {e}")))?;
    eitri_core::plugin::beside::check(&manifest).map_err(|why| (REFUSED, why))
}

/// What `eitri page --list` prints: each id on the search path a page window can open, one per line.
fn list_text(search: &SearchEnv) -> String {
    eitri_core::plugin::beside::list(search)
        .into_iter()
        .map(|id| format!("{id}\n"))
        .collect()
}

/// `page-<16 hex>.log`, the hex the page's control socket's own.
fn page_log_name(control_dir: &Path, key: &ControlKey<'_>) -> Option<String> {
    let sock = panel_control::socket_path_for(control_dir, key).ok()?;
    let hex = sock
        .file_name()?
        .to_str()?
        .strip_prefix("g-")?
        .strip_suffix(".sock")?
        .to_owned();
    Some(eitri_core::companion::stdio::page_log_file_name(&hex))
}

/// Moves the standard streams off the pipes of the nvim that started the window, into the page's log, saying once on
/// them where its output goes; a terminal or a file is left alone. The starter's stderr is kept for a refusal the
/// window finds once it is up.
fn leave_editor_pipes(log_name: Option<&str>) -> StarterStderr {
    use std::io::Write;
    let starter = StarterStderr::keep();
    let dir = eitri_core::companion::stdio::log_dir(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    );
    let Some(name) = log_name else {
        return starter;
    };
    eitri_core::companion::stdio::leave_pipes_to(dir.as_deref(), name, &|log| {
        // Not `eprintln!`: the nvim that started the window may already be gone, and a failed write must not panic.
        let _ = match log {
            Some(path) => writeln!(
                std::io::stderr(),
                "eitri page: running; its output goes to {}",
                path.display()
            ),
            None => writeln!(std::io::stderr(), "eitri page: running; its output is discarded"),
        };
    });
    starter
}

/// The stderr pipe of the nvim that started the window, kept after the streams left it so a refusal found once the
/// window is up still reaches the line that nvim shows. Close-on-exec, so no child of the window holds it; let go
/// once the page is attached, when that nvim's job sees its stream end.
pub(crate) struct StarterStderr(RefCell<Option<OwnedFd>>);

impl StarterStderr {
    fn keep() -> StarterStderr {
        if !eitri_core::companion::stdio::is_pipe(2) {
            return StarterStderr(RefCell::new(None));
        }
        // SAFETY: `fcntl(F_DUPFD_CLOEXEC)` on an open descriptor returns a new one this process owns, or -1.
        let fd = unsafe { libc::fcntl(2, libc::F_DUPFD_CLOEXEC, 3) };
        // SAFETY: a non-negative `fd` was just made by the call above and nothing else owns it.
        StarterStderr(RefCell::new((fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) })))
    }

    /// Writes `line` where the starter reads, if it still can; a starter gone is no failure.
    pub(crate) fn say(&self, line: &str) {
        if let Some(fd) = self.0.borrow().as_ref() {
            let text = format!("{line}\n");
            // SAFETY: `write` from a buffer that lives for the call, to a descriptor this struct owns.
            let _ = unsafe { libc::write(fd.as_raw_fd(), text.as_ptr().cast(), text.len()) };
        }
    }

    /// The starter hears nothing more from this window.
    pub(crate) fn release(&self) {
        self.0.borrow_mut().take();
    }
}

/// What `build_ui` takes for a beside window, made before GTK or at its activation.
pub(crate) struct BesideStart {
    pub(crate) theme_feed: Option<crate::theme::feed::ThemeFeed>,
    pub(crate) server: Rc<ControlServer>,
    pub(crate) nvim: PathBuf,
    pub(crate) starter: Rc<StarterStderr>,
    pub(crate) control_dir: PathBuf,
    /// What a window this one starts runs: this executable's path, which after an update names the new build.
    pub(crate) exe: PathBuf,
}

/// What the window does at a state of its link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LinkVerdict {
    /// Still connecting or installing.
    Wait,
    /// The page's slot is installed: the plugin's editor session can start.
    Attached,
    /// The window closes and the process ends with `code`, saying `line`: to the starter as well when `to_starter`
    /// (a refusal the person must read), else to the log only.
    Close { code: u8, line: String, to_starter: bool },
}

/// The link's `state`, `attached_before` once it has ever attached: a page window lives exactly as long as its link.
pub(crate) fn link_verdict(state: &LinkState, attached_before: bool) -> LinkVerdict {
    let close = |code, line: &str, to_starter| LinkVerdict::Close {
        code,
        line: line.to_owned(),
        to_starter,
    };
    match state {
        LinkState::Attached { .. } => LinkVerdict::Attached,
        LinkState::NoEditor | LinkState::Connecting { .. } | LinkState::Attaching { .. } if !attached_before => {
            LinkVerdict::Wait
        }
        LinkState::Refused { why, .. } => {
            let text = match why {
                InstallRefusal::EitriWindow => EITRI_WINDOW_TEXT,
                InstallRefusal::Takeover => TAKEOVER_TEXT,
            };
            close(REFUSED, &named(text), true)
        }
        LinkState::Failed { why, .. } if !attached_before => {
            close(FAILED, &named(&format!("could not attach: {why}")), true)
        }
        LinkState::Detached { why: Detach::Replaced } => close(0, MOVED, false),
        LinkState::NoEditor
        | LinkState::Connecting { .. }
        | LinkState::Attaching { .. }
        | LinkState::Failed { .. }
        | LinkState::Detached {
            why: Detach::EditorWentAway,
        } => close(0, WENT_AWAY, false),
    }
}

/// Polls `link` on the main loop, as the companion's own link is polled, calling `on_state` with each new state and
/// the pid of the process holding the editor's socket; stops once the link is shut.
pub(crate) fn watch_link(link: &Rc<CompanionLink>, on_state: impl Fn(&LinkState, Option<u32>) + 'static) {
    {
        let (state, _, peer) = link.snapshot();
        on_state(&state, peer);
    }
    let weak = Rc::downgrade(link);
    glib::timeout_add_local(CompanionLink::POLL_INTERVAL, move || {
        let Some(link) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        let poll = link.poll(std::time::Instant::now());
        for line in &poll.logs {
            println!("{line}");
        }
        if let Some((state, _, peer)) = poll.changed {
            on_state(&state, peer);
        }
        if poll.shut {
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });
}

/// Polls `link` on the main loop until the page's slot is installed (`Ok`) or the start ends without a window (`Err`
/// with the exit code, the line and whether the starter hears it), then calls `then` once and stops.
fn await_install(link: Rc<CompanionLink>, then: impl FnOnce(Result<(), (u8, String, bool)>) + 'static) {
    let then = RefCell::new(Some(then));
    let decide = move |state: &LinkState| -> bool {
        let installed = match link_verdict(state, false) {
            LinkVerdict::Wait => return false,
            LinkVerdict::Attached => Ok(()),
            LinkVerdict::Close { code, line, to_starter } => Err((code, line, to_starter)),
        };
        if let Some(then) = then.borrow_mut().take() {
            then(installed);
        }
        true
    };
    if decide(&link.snapshot().0) {
        return;
    }
    glib::timeout_add_local(CompanionLink::POLL_INTERVAL, move || {
        let poll = link.poll(std::time::Instant::now());
        for line in &poll.logs {
            println!("{line}");
        }
        let decided = poll.changed.is_some_and(|(state, _, _)| decide(&state));
        if decided || poll.shut {
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });
}

/// Says `line` for a close that ends the process with `code`: to the starter as well when `to_starter`.
fn say_closing(starter: &StarterStderr, code: u8, line: &str, to_starter: bool) {
    if to_starter {
        starter.say(line);
        eprintln!("{line}");
    } else {
        println!("{line}");
    }
    exit_with(code);
}

/// Closes the window for `line`, ending the process with `code`; the starter hears `line` too when `to_starter`.
pub(crate) fn close_for(
    driver: &eitri_core::wm::Driver<eitri_core::wm::Controller>,
    starter: &StarterStderr,
    code: u8,
    line: &str,
    to_starter: bool,
) {
    say_closing(starter, code, line, to_starter);
    driver.close_requested(eitri_core::wm::WinId::MAIN, eitri_core::wm::CloseBy::User);
}

/// The follow snippet was refused by the editor: the window closes with exit 1, and never tries again.
pub(crate) fn follow_refused(driver: &eitri_core::wm::Driver<eitri_core::wm::Controller>, starter: &StarterStderr) {
    close_for(driver, starter, FAILED, FOLLOW_REFUSED, false);
}

/// What [`wire`] joins up, from `build_ui`.
pub(crate) struct Wiring {
    pub(crate) app: gtk4::Application,
    pub(crate) window: gtk4::ApplicationWindow,
    pub(crate) driver: Rc<eitri_core::wm::Driver<eitri_core::wm::Controller>>,
    pub(crate) wm_host: Rc<crate::gtk_host::GtkHost>,
    pub(crate) plugin_panels: Rc<crate::plugin_panels::PluginPanels>,
    pub(crate) link: Rc<CompanionLink>,
    pub(crate) start: BesideStart,
    pub(crate) project: PathBuf,
    pub(crate) pair_raise: crate::linked_window::focus::PairRaise,
    pub(crate) wm: eitri_core::compositor::WmChoice,
}

/// A beside window's own services: the link polled for its whole life (its attach starts the plugin's editor session,
/// its end closes the window), the control socket's raises, and the carrier of the controller's beside ops. At the
/// application's end the link lets go of the editor without waiting for it, and the control socket is removed.
pub(crate) fn wire(wiring: Wiring) {
    use gtk4::prelude::*;
    let Wiring {
        app,
        window,
        driver,
        wm_host,
        plugin_panels,
        link,
        start,
        project,
        pair_raise,
        wm,
    } = wiring;
    let BesideStart {
        server,
        nvim,
        starter,
        control_dir,
        exe,
        ..
    } = start;
    let runner =
        crate::companion::wm_runner::WmRunner::new(eitri_core::compositor::resolve(wm, &|name| std::env::var_os(name)));
    let raise_self: Rc<dyn Fn()> = {
        let window = window.downgrade();
        Rc::new(move || {
            if let Some(window) = window.upgrade() {
                crate::gtk_host::PairRaise::raise(&pair_raise, window.upcast_ref(), None);
            }
        })
    };
    {
        let link = Rc::downgrade(&link);
        let runner = runner.clone();
        wm_host.set_beside(Rc::new(Carrier {
            ctx: Rc::new(move || {
                let link = link.upgrade();
                BesideCtx {
                    nvim: nvim.clone(),
                    project: project.clone(),
                    in_tmux: link.as_ref().and_then(|link| link.in_tmux()).unwrap_or(false),
                    nvim_pid: link.as_ref().and_then(|link| link.nvim_pid()),
                    control_dir: control_dir.clone(),
                    exe: exe.clone(),
                    exe_args: Vec::new(),
                }
            }),
            raise_editor: Rc::new(move |pid| runner.raise_editor(pid)),
            raise_self: raise_self.clone(),
        }));
    }
    {
        let attached = std::cell::Cell::new(false);
        let closing = std::cell::Cell::new(false);
        let driver = Rc::downgrade(&driver);
        let starter = starter.clone();
        let editor = Rc::downgrade(&link);
        let watched = link.clone();
        // The link attached before the window was built; its state is read once `build_ui` has returned, so the
        // attach finds the window's follow feed, scratch prefix and editor handle in place.
        let on_state = move |state: &LinkState, peer: Option<u32>| match link_verdict(state, attached.get()) {
            LinkVerdict::Wait => {}
            LinkVerdict::Attached => {
                if !attached.replace(true) {
                    println!("[page] attached to the editor (process {})", peer.map_or(0, |pid| pid));
                    // The starting nvim has read all it will: its job sees the stream end now.
                    starter.release();
                    runner.set_partner_from(peer);
                    // The page is this window's own, held by no window 1: it answers its follow lines and commands
                    // here from the attach on.
                    for module in plugin_panels.modules() {
                        plugin_panels.activate_holding(&module);
                    }
                    if let Some(link) = editor.upgrade() {
                        plugin_panels.set_editor(link);
                    }
                    plugin_panels.editor_session();
                }
            }
            LinkVerdict::Close { code, line, to_starter } => {
                if closing.replace(true) {
                    return;
                }
                let (driver, starter) = (driver.clone(), starter.clone());
                // Not from inside the poll: the close goes through the controller, which may be deciding now.
                glib::idle_add_local_once(move || {
                    if let Some(driver) = driver.upgrade() {
                        close_for(&driver, &starter, code, &line, to_starter);
                    }
                });
            }
        };
        glib::idle_add_local_once(move || watch_link(&watched, on_state));
    }
    {
        let server = server.clone();
        let window = window.downgrade();
        glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
            if window.upgrade().is_none() {
                return glib::ControlFlow::Break;
            }
            // A page claim answers only a raise; an attach was refused on the socket's thread.
            if server.requests().try_iter().count() > 0 {
                println!("[page] raised by another start");
                raise_self();
            }
            glib::ControlFlow::Continue
        });
    }
    app.connect_shutdown(move |_| {
        // Queues the slot's teardown and closes the connection once it is written; never waits for nvim, whose
        // liveness timer removes the slot if the write never goes out.
        link.shutdown();
        server.cleanup();
    });
}

/// What a beside op needs to be carried out, read when the op comes.
#[derive(Debug, Clone)]
pub(crate) struct BesideCtx {
    /// The address of the nvim the window follows.
    pub nvim: PathBuf,
    pub project: PathBuf,
    /// The install's answer: that nvim runs inside tmux.
    pub in_tmux: bool,
    /// The pid of the process holding that nvim's socket, while attached.
    pub nvim_pid: Option<u32>,
    /// Where the control sockets are, the agent window's among them.
    pub control_dir: PathBuf,
    /// What runs Eitri for a window this one starts: this executable, with `exe_args` before its own arguments (none,
    /// but in a test).
    pub exe: PathBuf,
    pub exe_args: Vec<OsString>,
}

/// What [`carry_out`] leaves for the main loop: raises go through the window's own adapter, which never blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Carried {
    Done,
    RaiseEditor(u32),
    RaiseSelf,
}

/// Carries out `op`. `Agent` and `Page` connect and start processes, so they run on a worker; the raises are only
/// decided here. An `Err` is the reason, which the controller shows.
pub(crate) fn carry_out(op: &BesideOp, ctx: &BesideCtx) -> Result<Carried, String> {
    let nvim = ctx.nvim.clone().into_os_string();
    match op {
        BesideOp::Agent => {
            match panel_control::forward(&ctx.control_dir, &ControlKey::Project(&ctx.project), &Request::Raise)? {
                // A running agent window keeps whatever nvim it follows: it is asked to come forward, nothing else.
                Forwarded::Answered(Reply::Ok) => Ok(Carried::Done),
                Forwarded::Answered(Reply::Refused(why)) => Err(format!("the agent window refused: {why}")),
                Forwarded::NoneListening => start(ctx, "the agent window", vec!["panel".into(), "--nvim".into(), nvim]),
            }
        }
        BesideOp::Page(id) => start(
            ctx,
            "the page window",
            vec!["page".into(), id.into(), "--nvim".into(), nvim],
        ),
        BesideOp::RaiseEditor if ctx.in_tmux => Err(eitri_core::wm::feedback::BESIDE_TMUX_TEXT.to_owned()),
        BesideOp::RaiseEditor => ctx
            .nvim_pid
            .map(Carried::RaiseEditor)
            .ok_or_else(|| "the editor is not attached yet".to_owned()),
        BesideOp::RaiseSelf => Ok(Carried::RaiseSelf),
    }
}

/// Starts Eitri with `args` for the window's project, reaped by its own thread; a child that ends badly at once is
/// said, one that stays (or raised a running window and ended cleanly) is done.
fn start(ctx: &BesideCtx, what: &str, args: Vec<OsString>) -> Result<Carried, String> {
    let mut flags = ctx.exe_args.clone();
    flags.extend(args);
    let started = crate::project_start::start(
        &ctx.exe,
        &flags,
        crate::project_start::How::Plain(ctx.project.clone()),
        None,
    )
    .map_err(|e| format!("{what} could not be started: {e}"))?;
    match started.failed_within(crate::project_start::FAILED_WITHIN) {
        None => Ok(Carried::Done),
        Some(Some(code)) => Err(format!("{what} ended at once (exit {code})")),
        Some(None) => Err(format!("{what} ended at once")),
    }
}

/// The window's carrier of beside ops: the context read when an op comes, and the window's raises.
pub(crate) struct Carrier {
    pub(crate) ctx: Rc<dyn Fn() -> BesideCtx>,
    pub(crate) raise_editor: Rc<dyn Fn(u32)>,
    pub(crate) raise_self: Rc<dyn Fn()>,
}

impl crate::gtk_host::BesidePort for Carrier {
    fn carry_out(&self, op: &BesideOp, later: Box<dyn FnOnce(String)>) -> Result<(), String> {
        let ctx = (self.ctx)();
        match op {
            BesideOp::Agent | BesideOp::Page(_) => {
                let (send, answer) = std::sync::mpsc::channel();
                let op = op.clone();
                std::thread::Builder::new()
                    .name("beside-op".into())
                    .spawn(move || {
                        let _ = send.send(carry_out(&op, &ctx));
                    })
                    .map_err(|e| format!("could not start a worker: {e}"))?;
                let later = RefCell::new(Some(later));
                glib::timeout_add_local(std::time::Duration::from_millis(50), move || match answer.try_recv() {
                    Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                    Ok(Ok(_)) => glib::ControlFlow::Break,
                    Ok(Err(why)) => {
                        if let Some(later) = later.borrow_mut().take() {
                            later(why);
                        }
                        glib::ControlFlow::Break
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
                });
                Ok(())
            }
            BesideOp::RaiseEditor | BesideOp::RaiseSelf => match carry_out(op, &ctx)? {
                Carried::RaiseEditor(pid) => {
                    println!("[page] raise the editor's window");
                    (self.raise_editor)(pid);
                    Ok(())
                }
                Carried::RaiseSelf => {
                    println!("[page] raise this window");
                    (self.raise_self)();
                    Ok(())
                }
                Carried::Done => Ok(()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::companion::attach::{Detach, LinkState};
    use eitri_core::companion::InstallRefusal;
    use eitri_core::wm::BesideOp;
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    const PATIENCE: Duration = Duration::from_secs(30);

    /// A private scratch directory of the test's own, removed when dropped.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Scratch {
            use std::os::unix::fs::PermissionsExt;
            let dir = std::env::temp_dir().join(format!("eitri-page-window-{label}-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            Scratch(dir)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }

        /// A shell script the test runs as `/bin/sh <script>`: nothing the test writes is ever exec'd itself.
        fn stub(&self, body: &str) -> OsString {
            let path = self.path("stub.sh");
            std::fs::write(&path, body).unwrap();
            path.into_os_string()
        }

        fn wait_for(&self, name: &str) -> String {
            let until = std::time::Instant::now() + PATIENCE;
            while !self.path(name).exists() {
                assert!(std::time::Instant::now() < until, "{name} never appeared");
                std::thread::sleep(Duration::from_millis(10));
            }
            std::fs::read_to_string(self.path(name)).unwrap()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn ctx(scratch: &Scratch, stub: OsString) -> BesideCtx {
        BesideCtx {
            nvim: PathBuf::from("/run/user/1/nvim.7.0"),
            project: PathBuf::from("/work/中文 项目"),
            in_tmux: false,
            nvim_pid: Some(4242),
            control_dir: scratch.path("eitri"),
            exe: PathBuf::from("/bin/sh"),
            exe_args: vec![stub],
        }
    }

    /// `<prefix> a` raises the running agent window over its control socket and starts nothing; with none listening it
    /// starts one for this page's nvim and project, and never sends an attach to a panel that runs.
    #[test]
    fn agent_op_sends_raise_when_a_panel_answers_and_starts_one_when_none_listens() {
        use eitri_core::panel_control::{claim, Claim, Request};
        let scratch = Scratch::new("agent");
        let stub = scratch.stub(&format!(
            "printf '%s\\n' \"$@\" > '{0}.part'\nmv '{0}.part' '{0}'\n",
            scratch.path("args").display()
        ));
        let ctx = ctx(&scratch, stub);
        let server = match claim(&ctx.control_dir, &ctx.project, None).unwrap() {
            Claim::Ours(server) => server,
            other => panic!("the scratch control directory was not free: {other:?}"),
        };
        assert_eq!(carry_out(&BesideOp::Agent, &ctx), Ok(Carried::Done));
        let received = server
            .requests()
            .recv_timeout(PATIENCE)
            .expect("the panel heard a request");
        assert_eq!(received.request, Request::Raise, "a raise, never an attach");
        assert!(!scratch.path("args").exists(), "nothing started while a panel answers");
        server.cleanup();

        assert_eq!(carry_out(&BesideOp::Agent, &ctx), Ok(Carried::Done));
        let args: Vec<String> = scratch.wait_for("args").lines().map(str::to_owned).collect();
        assert_eq!(
            args,
            ["panel", "--nvim", "/run/user/1/nvim.7.0", "--", "/work/中文 项目"]
        );
    }

    /// Another page's key starts `eitri page` for the same nvim and project; a running one raises itself there.
    #[test]
    fn a_page_op_starts_eitri_page_for_the_same_nvim() {
        let scratch = Scratch::new("page");
        let stub = scratch.stub(&format!(
            "printf '%s\\n' \"$@\" > '{0}.part'\nmv '{0}.part' '{0}'\n",
            scratch.path("args").display()
        ));
        let ctx = ctx(&scratch, stub);
        assert_eq!(carry_out(&BesideOp::Page("sketch".into()), &ctx), Ok(Carried::Done));
        let args: Vec<String> = scratch.wait_for("args").lines().map(str::to_owned).collect();
        assert_eq!(
            args,
            [
                "page",
                "sketch",
                "--nvim",
                "/run/user/1/nvim.7.0",
                "--",
                "/work/中文 项目"
            ]
        );
        let failing = scratch.stub("exit 3\n");
        let ctx = BesideCtx {
            exe_args: vec![failing],
            ..ctx
        };
        assert!(
            carry_out(&BesideOp::Page("sketch".into()), &ctx).is_err(),
            "a child that ends at once is said"
        );
    }

    /// Inside tmux the editor's window cannot be found from nvim's pid: nothing is raised, and the refusal is the
    /// text the controller toasts on the first press and only logs after.
    #[test]
    fn raise_editor_inside_tmux_raises_nothing_and_toasts_once() {
        use std::cell::{Cell, RefCell};
        use std::rc::Rc;
        let scratch = Scratch::new("tmux");
        let in_tmux = BesideCtx {
            in_tmux: true,
            ..ctx(&scratch, OsString::new())
        };
        for _ in 0..2 {
            assert_eq!(
                carry_out(&BesideOp::RaiseEditor, &in_tmux),
                Err(eitri_core::wm::feedback::BESIDE_TMUX_TEXT.to_string())
            );
        }
        let raised = Rc::new(Cell::new(0));
        let shown = Rc::new(Cell::new(0));
        let tmux = Rc::new(Cell::new(true));
        let carrier = Carrier {
            ctx: {
                let tmux = tmux.clone();
                let base = RefCell::new(ctx(&scratch, OsString::new()));
                Rc::new(move || BesideCtx {
                    in_tmux: tmux.get(),
                    ..base.borrow().clone()
                })
            },
            raise_editor: {
                let raised = raised.clone();
                Rc::new(move |pid| {
                    assert_eq!(pid, 4242);
                    raised.set(raised.get() + 1)
                })
            },
            raise_self: {
                let shown = shown.clone();
                Rc::new(move || shown.set(shown.get() + 1))
            },
        };
        use crate::gtk_host::BesidePort;
        for _ in 0..2 {
            assert_eq!(
                carrier.carry_out(&BesideOp::RaiseEditor, Box::new(|_| panic!("no later answer"))),
                Err(eitri_core::wm::feedback::BESIDE_TMUX_TEXT.to_string())
            );
        }
        assert_eq!(raised.get(), 0, "nothing raised from inside tmux");
        tmux.set(false);
        assert_eq!(
            carrier.carry_out(&BesideOp::RaiseEditor, Box::new(|_| panic!("no later answer"))),
            Ok(())
        );
        assert_eq!(
            raised.get(),
            1,
            "outside tmux the editor's window comes forward by nvim's pid"
        );
        assert_eq!(
            carrier.carry_out(&BesideOp::RaiseSelf, Box::new(|_| panic!("no later answer"))),
            Ok(())
        );
        assert_eq!(shown.get(), 1);
    }

    /// The install's refusal ends `eitri page` with 64 and the text the person reads; a failed attach with 1; the
    /// editor's going away, or a takeover replacing the slot, with 0 and the log line.
    #[test]
    fn a_refused_install_exits_64_with_the_eitri_window_and_takeover_texts() {
        let addr = PathBuf::from("/run/user/1/nvim.7.0");
        let refused = |why| LinkState::Refused {
            addr: addr.clone(),
            why,
        };
        assert_eq!(
            link_verdict(&refused(InstallRefusal::EitriWindow), false),
            LinkVerdict::Close {
                code: 64,
                line: "eitri page: this window opens pages itself: press the prefix and the page's key".into(),
                to_starter: true,
            }
        );
        assert_eq!(
            link_verdict(&refused(InstallRefusal::Takeover), false),
            LinkVerdict::Close {
                code: 64,
                line: "eitri page: this nvim is open in an Eitri window: its pages open there".into(),
                to_starter: true,
            }
        );
        let failed = LinkState::Failed {
            addr: addr.clone(),
            why: "connection refused".into(),
        };
        assert_eq!(
            link_verdict(&failed, false),
            LinkVerdict::Close {
                code: 1,
                line: "eitri page: could not attach: connection refused".into(),
                to_starter: true,
            }
        );
        let went = LinkVerdict::Close {
            code: 0,
            line: "[page] the editor went away; closing".into(),
            to_starter: false,
        };
        assert_eq!(link_verdict(&failed, true), went);
        assert_eq!(
            link_verdict(
                &LinkState::Detached {
                    why: Detach::EditorWentAway
                },
                true
            ),
            went
        );
        assert_eq!(link_verdict(&LinkState::NoEditor, true), went);
        assert_eq!(
            link_verdict(&LinkState::Detached { why: Detach::Replaced }, true),
            LinkVerdict::Close {
                code: 0,
                line: "[page] this nvim moved into an Eitri window; closing".into(),
                to_starter: false,
            }
        );
        let attached = LinkState::Attached {
            addr: addr.clone(),
            channel: 4,
            nvim_pid: 77,
            in_tmux: false,
        };
        assert_eq!(link_verdict(&attached, false), LinkVerdict::Attached);
        for waiting in [
            LinkState::NoEditor,
            LinkState::Connecting { addr: addr.clone() },
            LinkState::Attaching {
                addr: addr.clone(),
                waiting_for_key: true,
            },
        ] {
            assert_eq!(link_verdict(&waiting, false), LinkVerdict::Wait, "{waiting:?}");
        }
        assert_eq!(
            FOLLOW_REFUSED,
            "[page] the editor refused the page's follow snippet; closing"
        );
    }

    /// `--list` prints the ids on the search path a page window can open, one per line, and nothing else.
    #[test]
    fn list_prints_the_qualifying_ids() {
        let scratch = Scratch::new("list");
        let plugins = scratch.path("data/eitri/plugins");
        for (id, caps) in [
            ("notes", "[\"net\"]"),
            ("tasker", "[\"tasks\"]"),
            ("viewer", "[\"state\"]"),
        ] {
            let dir = plugins.join(id);
            std::fs::create_dir_all(dir.join("site")).unwrap();
            std::fs::write(
                dir.join("eitri-plugin.toml"),
                format!(
                    "id = \"{id}\"\ntitle = \"T\"\nprotocol = 1\nsite = \"site\"\nentry = \"index.html\"\ncapabilities = {caps}\n"
                ),
            )
            .unwrap();
            std::fs::write(dir.join("site/index.html"), "<p>x</p>\n").unwrap();
        }
        let env = eitri_core::plugin::search::SearchEnv {
            xdg_data_home: Some(scratch.path("data").into_os_string()),
            xdg_data_dirs: Some(scratch.path("none").into_os_string()),
            ..Default::default()
        };
        assert_eq!(list_text(&env), "notes\nviewer\n");
    }

    /// The page's log is `page-<hex>.log` in the companion log directory, the hex its control socket's; started on
    /// pipes, the window says where its output goes on them and moves both streams there.
    #[test]
    fn stdio_leaves_nvims_pipes_for_page_hex_log() {
        let scratch = Scratch::new("stdio");
        let addr = scratch.path("nvim.sock");
        std::fs::write(&addr, b"").unwrap();
        let control = scratch.path("eitri");
        let key = eitri_core::panel_control::ControlKey::Page {
            addr: &addr,
            id: "notes",
        };
        let sock = eitri_core::panel_control::socket_path_for(&control, &key).unwrap();
        let hex = sock
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("g-"))
            .and_then(|name| name.strip_suffix(".sock"))
            .unwrap()
            .to_owned();
        assert_eq!(
            page_log_name(&control, &key).as_deref(),
            Some(format!("page-{hex}.log").as_str())
        );

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "page_window::tests::a_helper_that_leaves_its_pipes",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(HELPER_LOG, format!("page-{hex}.log"))
            .env("XDG_STATE_HOME", scratch.path("state"))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .unwrap();
        let log = scratch.path("state").join(format!("eitri/companion/page-{hex}.log"));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("eitri page: running; its output goes to {}", log.display())),
            "{stderr}"
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("after the move"));
        let written = std::fs::read_to_string(&log).unwrap();
        assert!(written.contains("after the move"), "{written}");
    }

    const HELPER_LOG: &str = "EITRI_TEST_PAGE_LOG";

    /// Not a test of its own: [`stdio_leaves_nvims_pipes_for_page_hex_log`] runs this binary with only this test, on
    /// pipes, as a page window started from nvim would be.
    #[test]
    fn a_helper_that_leaves_its_pipes() {
        let Some(name) = std::env::var_os(HELPER_LOG) else {
            return;
        };
        let _starter = leave_editor_pipes(name.to_str());
        println!("after the move");
    }

    /// The log's name carries the control key's 16 hex digits, wherever the control directory is.
    #[test]
    fn the_page_key_hex_is_the_sockets() {
        let scratch = Scratch::new("hex");
        let addr = scratch.path("n");
        std::fs::write(&addr, b"").unwrap();
        let key = eitri_core::panel_control::ControlKey::Page {
            addr: &addr,
            id: "notes",
        };
        let name = page_log_name(Path::new("/run/user/1/eitri"), &key).unwrap();
        assert!(name.starts_with("page-") && name.ends_with(".log") && name.len() == "page-.log".len() + 16);
    }
}
