//! `shell choose-project [--clean] [--legacy]`: a desktop launch that named no path.
//!
//! A process of its own, because `main()` settles one project root for a process and everything after that assumes
//! the root. This one has no project, no nvim, no WebKit, no lock and no socket. It reads `init.lua` for one key, shows
//! the chooser when the key asks for it, and hands over to a child that is an ordinary launch of what was picked.
//!
//! A child and not an `exec`: the activation token names this window's surface, which must outlive the new window's
//! first map, the crash relay's pipes would have no reader in a new image, and a thread stuck on a dead mount can hold
//! an `exec`. So the window stays up, saying what it is opening, until the first of the ends in [`Ends`].

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use eitri_core::lua::config::{parse_desktop_start, DesktopStart, DESKTOP_START_KEY};
use eitri_core::lua::kernel::{refuse_panels, Kernel};
use eitri_core::onboarding::Lang;
use eitri_core::project_chooser::{start_failed_line, Chooser, Effect, Ev, Fact, LineEv, Mode, Row};
use eitri_core::recent_projects::{self, Sources};
use eitri_core::theme::start::{StartReport, StartTheme};
use eitri_core::theme::store::Loaded;
use gtk4::glib;
use gtk4::prelude::*;

use crate::gtk_host::project_chooser::{ChooserReport, ProjectChooserView};
use crate::project_host::{ProjectWorkers, WorkerAnswer};
use crate::project_start::{self, How};

pub(crate) const SUBCOMMAND: &str = "choose-project";

/// The exit of a command line this subcommand does not take (sysexits' `EX_USAGE`), as the launcher's own.
const USAGE: u8 = 64;
/// How long the window waits for a new window that never takes the keys: a compositor may give it no focus at all.
const HAND_OVER_WITHIN: Duration = Duration::from_secs(10);
const POLL_EVERY: Duration = Duration::from_millis(10);
const WIDTH: i32 = 720;
const HEIGHT: i32 = 520;
/// The launcher's words for a directory that cannot be resolved.
const NO_DIRECTORY: &str = "eitri: no such directory";

/// Whether a command line, with the program's own name left off, is this subcommand's.
pub(crate) fn named(args: &[OsString]) -> bool {
    args.first().is_some_and(|arg| arg == SUBCOMMAND)
}

/// What a launch does once `init.lua` has had its say.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Next {
    Show,
    Raise(PathBuf),
    Cwd,
}

/// `rows` are in `recent_projects::read`'s order, which already holds `$HOME` and `/` back behind every other root.
/// A row whose folder could not be told to be there is passed over: a dead mount must not hold a start.
pub(crate) fn next(start: DesktopStart, rows: &[Row]) -> Next {
    match start {
        DesktopStart::Choose => Next::Show,
        DesktopStart::Cwd => Next::Cwd,
        DesktopStart::Last => {
            recent_projects::newest_present(rows).map_or(Next::Show, |row| Next::Raise(row.path.clone()))
        }
    }
}

/// What ends the window once a start is under way. A child that exited 0 raised a window or handed its files over;
/// any other exit brings the chooser back with the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ends {
    /// The new window took the keys.
    LostActivation,
    ChildExited(Exit),
    /// The new window has not taken the keys in time, and it runs on without this one.
    Deadline,
}

/// How a started child ended: its exit code, `None` when a signal ended it or it never started, and whether its folder
/// was gone by then, which is then the reason said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Exit {
    code: Option<i32>,
    gone: bool,
}

/// The end of a start, from all that has been seen of it. How the child went comes first, because a failure is the one
/// end the person has to hear about.
fn ends(exit: Option<Exit>, lost_activation: bool, past_deadline: bool) -> Option<Ends> {
    if let Some(exit) = exit {
        return Some(Ends::ChildExited(exit));
    }
    if lost_activation {
        return Some(Ends::LostActivation);
    }
    past_deadline.then_some(Ends::Deadline)
}

/// Puts `end` on the chooser, and says whether the window stays for it.
fn settle(chooser: &mut Chooser, end: Ends, target: &Path) -> bool {
    match end {
        Ends::ChildExited(Exit { code: Some(0), .. }) | Ends::LostActivation | Ends::Deadline => false,
        Ends::ChildExited(Exit { code, gone }) => {
            println!("{}", start_failed_line(code));
            chooser.start_failed(target.to_path_buf(), gone);
            true
        }
    }
}

/// The flags to hand on: they mean nothing here and are only the child's. Anything else is not this subcommand.
fn flags(args: &[OsString]) -> Option<Vec<OsString>> {
    args.iter()
        .all(|arg| arg == "--clean" || arg == "--legacy")
        .then(|| args.to_vec())
}

/// How the launcher's own directory is started. It is not looked for in the records or the chooser: a directory that
/// is gone is said and ends the launch, as the launcher says it from a terminal.
fn cwd_how(current: io::Result<PathBuf>) -> Result<How, &'static str> {
    current.map(How::Plain).map_err(|_| NO_DIRECTORY)
}

/// The window's reading of the key. What `init.lua` got wrong is left for the window that opens to say; a value that is
/// not one is the default, with a line for whoever reads stderr.
fn desktop_start(config_dir: &Path) -> DesktopStart {
    let Ok(kernel) = Kernel::new(config_dir.to_path_buf(), refuse_panels) else {
        return DesktopStart::Choose;
    };
    kernel.load_init_file(&config_dir.join("init.lua"));
    let value = kernel.config.borrow().get(DESKTOP_START_KEY).map(str::to_owned);
    parse_desktop_start(value.as_deref()).unwrap_or_else(|why| {
        eprintln!("eitri: {why}; the default is used");
        DesktopStart::Choose
    })
}

/// Persisted roots are canonical, so a home named through a symlink is resolved before it is compared with them.
fn canonical_home(home: Option<&Path>) -> Option<PathBuf> {
    home.map(|home| home.canonicalize().unwrap_or_else(|_| home.to_path_buf()))
}

fn project_name(path: &Path) -> String {
    Row::new(path.to_path_buf(), None, String::new(), Fact::Unknown).name
}

fn exe_to_start() -> PathBuf {
    project_start::exe_to_start(std::env::current_exe(), &|path| path.exists())
}

/// The newest project's remembered colours, else the built-in ones: a theme is per project, and this has no project.
fn start_theme(sources: &Sources) -> StartTheme {
    let dir = eitri_core::theme::store::state_dir(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    );
    match recent_projects::candidates(sources).first() {
        Some(newest) => StartTheme::read(dir.as_deref(), &newest.root, false),
        None => StartTheme::from_loaded(Loaded::Missing),
    }
}

/// What the window is made from, all of it read before GTK is up.
struct Shown {
    flags: Vec<OsString>,
    sources: Sources,
    home: Option<PathBuf>,
    theme: StartTheme,
    /// A start made before the window, which failed: its folder, and whether that folder was gone.
    failed: Option<(PathBuf, bool)>,
}

/// The subcommand. Everything up to [`show`] runs before GTK, so it may wait on the disk and on a child.
pub(crate) fn run(args: &[OsString]) -> glib::ExitCode {
    let Some(flags) = flags(args) else {
        eprintln!("usage: shell {SUBCOMMAND} [--clean] [--legacy]");
        return glib::ExitCode::from(USAGE);
    };
    // Before GTK, which takes the token out of the environment; a window it opens asks its own display for one.
    let token = crate::open_requests::token_from_env();
    let start = eitri_core::config_dir::resolve(&|name| std::env::var_os(name))
        .map_or(DesktopStart::Choose, |dir| desktop_start(&dir));
    let control =
        eitri_core::panel_control::control_dir(std::env::var_os("XDG_RUNTIME_DIR").as_deref(), &std::env::temp_dir());
    let sources = Sources::from_env(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
        control,
    );
    let rows = match start {
        DesktopStart::Last => recent_projects::read(&sources),
        DesktopStart::Choose | DesktopStart::Cwd => Vec::new(),
    };
    let how = match next(start, &rows) {
        Next::Show => None,
        Next::Raise(path) => Some(How::Raise(path)),
        Next::Cwd => match cwd_how(std::env::current_dir()) {
            Ok(how) => Some(how),
            Err(line) => {
                eprintln!("{line}");
                return glib::ExitCode::FAILURE;
            }
        },
    };
    let mut failed = None;
    if let Some(how) = how {
        let target = match &how {
            How::Raise(path) | How::Plain(path) => path.clone(),
        };
        // No window is up to say a failure, so the child's first seconds are waited for here: a desktop launch must
        // not end in nothing, and the chooser is what is left to offer.
        let start = project_start::start(&exe_to_start(), &flags, how, token.as_deref());
        let Some(code) = project_start::failure(start, project_start::FAILED_WITHIN) else {
            return glib::ExitCode::SUCCESS;
        };
        println!("{}", start_failed_line(code));
        let gone = project_start::not_there(&target);
        failed = Some((target, gone));
    }
    let home = canonical_home(sources.home.as_deref());
    let theme = start_theme(&sources);
    show(Shown {
        flags,
        sources,
        home,
        theme,
        failed,
    })
}

fn show(shown: Shown) -> glib::ExitCode {
    let app = crate::build_application(crate::AppKind::Main);
    let shown = RefCell::new(Some(shown));
    let kept = RefCell::new(None);
    app.connect_activate(move |app| {
        // `activate` comes once for a process that opts out of single-instance behaviour.
        let Some(shown) = shown.borrow_mut().take() else { return };
        *kept.borrow_mut() = Some(LaunchWindow::open(app, shown));
    });
    app.run_with_args::<&str>(&[])
}

/// What the chooser hears of the view. A window this size has no module to go back to, so a click beside the panel and
/// a focus change end nothing: `Esc` and the window's own close are the ways out.
fn view_event(report: ChooserReport) -> Option<Ev> {
    match report {
        ChooserReport::Line(LineEv::KeysLost) | ChooserReport::Dismiss => None,
        ChooserReport::Line(ev) => Some(Ev::Line(ev)),
        ChooserReport::Pick(index) => Some(Ev::Pick(index)),
        ChooserReport::Folder(path) => Some(Ev::Folder(path)),
    }
}

/// What the chooser hears of a worker. This window watches its own start, for longer than the workers do.
fn worker_event(answer: WorkerAnswer) -> Option<Ev> {
    match answer {
        WorkerAnswer::Rows { serial, rows } => Some(Ev::Rows { serial, rows }),
        WorkerAnswer::Path { serial, answer } => Some(Ev::PathAnswer { serial, answer }),
        WorkerAnswer::Folder(path) => Some(Ev::Folder(path)),
        WorkerAnswer::Started { .. } => None,
    }
}

fn activation_token(window: &gtk4::ApplicationWindow) -> Option<String> {
    WidgetExt::display(window)
        .app_launch_context()
        .startup_notify_id(None::<&gtk4::gio::AppInfo>, &[])
        .map(|token| token.to_string())
        .filter(|token| !token.is_empty())
}

/// Starts the child off the GTK thread, since the start stats and blocks on the spawn, and sends its exit: the code, or
/// `None` when it died by a signal or never started, and on a failure whether the folder is gone, which is asked here
/// because it stats. A reaper that went away with no message says nothing about the child, so nothing is sent and the
/// other ends decide.
fn start_child(flags: Vec<OsString>, target: PathBuf, token: Option<String>, exit: mpsc::Sender<Exit>) {
    let failed = exit.clone();
    let spawned = std::thread::Builder::new().name("project-open".into()).spawn(move || {
        let code = match project_start::start(&exe_to_start(), &flags, How::Raise(target.clone()), token.as_deref()) {
            Ok(started) => match started.ended.recv() {
                Ok(code) => code,
                Err(_) => return,
            },
            Err(_) => None,
        };
        let gone = code != Some(0) && project_start::not_there(&target);
        let _ = exit.send(Exit { code, gone });
    });
    if spawned.is_err() {
        let _ = failed.send(Exit {
            code: None,
            gone: false,
        });
    }
}

/// A start under way.
struct Watch {
    target: PathBuf,
    exit: mpsc::Receiver<Exit>,
    lost_activation: Cell<bool>,
    deadline: Instant,
}

/// The window, with the chooser as all it holds, and the small carrier of the chooser's effects.
struct LaunchWindow {
    me: Weak<LaunchWindow>,
    window: gtk4::ApplicationWindow,
    view: ProjectChooserView,
    workers: ProjectWorkers,
    chooser: RefCell<Chooser>,
    flags: Vec<OsString>,
    watch: RefCell<Option<Watch>>,
}

impl LaunchWindow {
    fn open(app: &gtk4::Application, shown: Shown) -> Rc<LaunchWindow> {
        let Shown {
            flags,
            sources,
            home,
            theme,
            failed,
        } = shown;
        crate::xft_dpi::ensure_xft_dpi();
        gtk4::Window::set_default_icon_name(crate::APP_ID);
        if let Some(report) = theme.report() {
            match report {
                StartReport::Out(line) => println!("{line}"),
                StartReport::Err(line) => eprintln!("{line}"),
            }
        }
        crate::theme::gtk_css::ThemeCss::install(&theme.tokens, theme.prefers_dark());

        let lang = Lang::from_env(&|name| std::env::var_os(name));
        let mut chooser = Chooser::new(Mode::Launch, home, lang, 1);
        if let Some((target, gone)) = failed {
            chooser.start_failed(target, gone);
        }
        let window = gtk4::ApplicationWindow::new(app);
        window.set_default_size(WIDTH, HEIGHT);
        window.set_title(Some(&chooser.view().title));

        let this = Rc::new_cyclic(|me: &Weak<LaunchWindow>| {
            let to_view = me.clone();
            let view = ProjectChooserView::new(Rc::new(move |report: ChooserReport| {
                if let (Some(this), Some(ev)) = (to_view.upgrade(), view_event(report)) {
                    this.feed(ev);
                }
            }));
            let to_workers = me.clone();
            let workers = ProjectWorkers::new(
                sources,
                Rc::new(move |answer: WorkerAnswer| {
                    if let (Some(this), Some(ev)) = (to_workers.upgrade(), worker_event(answer)) {
                        this.feed(ev);
                    }
                }),
            );
            LaunchWindow {
                me: me.clone(),
                window: window.clone(),
                view,
                workers,
                chooser: RefCell::new(chooser),
                flags,
                watch: RefCell::new(None),
            }
        });
        window.set_child(Some(this.view.widget()));
        window.connect_close_request(|_| {
            println!("[project] closed");
            glib::Propagation::Proceed
        });
        let me = this.me.clone();
        window.connect_is_active_notify(move |window| {
            // The change and not the state: a window that was not active when the start began (a file dialog was
            // still closing) has lost nothing yet.
            if window.is_active() {
                return;
            }
            if let Some(this) = me.upgrade() {
                if let Some(watch) = this.watch.borrow().as_ref() {
                    watch.lost_activation.set(true);
                }
            }
        });
        println!("[project] chooser open");
        let effects = this.chooser.borrow_mut().shown();
        this.carry(effects);
        window.present();
        this.view.focus();
        this
    }

    fn feed(&self, ev: Ev) {
        let (effects, changed) = {
            let mut chooser = self.chooser.borrow_mut();
            let before = matches!(ev, Ev::Rows { .. }).then(|| chooser.view());
            let effects = chooser.event(ev);
            let changed = before
                .is_some_and(|before| before != chooser.view())
                .then(|| chooser.projects());
            (effects, changed)
        };
        if let Some(count) = changed {
            println!("[project] rows {count}");
        }
        self.carry(effects);
    }

    fn carry(&self, effects: Vec<Effect>) {
        for effect in effects {
            match effect {
                Effect::List { serial } => self.workers.list(serial),
                Effect::Path { serial, text, base } => self.workers.path(serial, text, base),
                Effect::FolderDialog => self.workers.folder_dialog(self.window.upcast_ref()),
                // Completion already changed the model's line; drawing it carries it to the entry.
                Effect::SetLine(_) => {}
                Effect::Open(target) => self.start(target),
                Effect::Close => self.window.close(),
            }
        }
        self.draw();
    }

    /// No borrow of the chooser is held across this: the view's own changes report back through [`Self::feed`].
    fn draw(&self) {
        let view = self.chooser.borrow().view();
        self.view.show(Some(&view));
    }

    fn start(&self, target: PathBuf) {
        println!("[project] open");
        let name = project_name(&target);
        self.chooser.borrow_mut().opening(&name);
        // The token names the input that picked, so it is taken here, before the start leaves this thread.
        let token = activation_token(&self.window);
        let (exit, watched) = mpsc::channel();
        start_child(self.flags.clone(), target.clone(), token, exit);
        *self.watch.borrow_mut() = Some(Watch {
            target,
            exit: watched,
            lost_activation: Cell::new(false),
            deadline: Instant::now() + HAND_OVER_WITHIN,
        });
        let me = self.me.clone();
        glib::timeout_add_local(POLL_EVERY, move || {
            let Some(this) = me.upgrade() else {
                return glib::ControlFlow::Break;
            };
            match this.end_so_far() {
                Some(end) => {
                    this.finish(end);
                    glib::ControlFlow::Break
                }
                None => glib::ControlFlow::Continue,
            }
        });
    }

    fn end_so_far(&self) -> Option<Ends> {
        let watch = self.watch.borrow();
        let watch = watch.as_ref()?;
        ends(
            watch.exit.try_recv().ok(),
            watch.lost_activation.get(),
            Instant::now() >= watch.deadline,
        )
    }

    fn finish(&self, end: Ends) {
        let Some(watch) = self.watch.take() else { return };
        if settle(&mut self.chooser.borrow_mut(), end, &watch.target) {
            self.draw();
            self.view.focus();
        } else {
            self.window.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eitri_core::plugin::test_support::Scratch;
    use eitri_core::project_chooser::PathAnswer;
    use eitri_core::recent_projects::Probe;
    use eitri_core::source_scan::{code_only, functions, read_required};
    use eitri_core::state_paths::project_key;
    use std::fs::{self, File};
    use std::os::unix::ffi::OsStrExt;
    use std::process::{Command, Stdio};
    use std::sync::atomic::AtomicBool;
    use std::time::SystemTime;

    /// Set, the helper below runs `run` in the process it finds itself in; unset, it does nothing.
    const CWD_HELPER: &str = "EITRI_TEST_CHOOSE_PROJECT_CWD";
    /// Long enough for a loaded machine; a read that passes never waits it out.
    const PATIENCE: Duration = Duration::from_secs(30);

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn row(path: &str, fact: Fact) -> Row {
        Row::new(path.into(), None, "1 m".into(), fact)
    }

    /// Answers from the disk, and no window holds anything.
    struct OnDisk;

    impl Probe for OnDisk {
        fn exists(&self, path: &Path) -> bool {
            path.is_dir()
        }

        fn window_holds(&self, _root: &Path) -> bool {
            false
        }
    }

    fn stamp(sources: &Sources, root: &Path, seconds: u64) {
        let dir = sources.layout.as_ref().unwrap();
        let path = dir.join(format!("{}.pair.lock", project_key(root)));
        agent::private_fs::write_private(&path, root.as_os_str().as_bytes()).unwrap();
        let used = SystemTime::UNIX_EPOCH + Duration::from_secs(seconds);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(used)
            .unwrap();
    }

    #[test]
    fn grammar_takes_clean_and_legacy_and_refuses_the_rest() {
        for good in [
            &[][..],
            &["--clean"][..],
            &["--legacy"][..],
            &["--clean", "--legacy"][..],
            &["--legacy", "--clean"][..],
        ] {
            assert_eq!(flags(&os(good)), Some(os(good)), "{good:?}");
        }
        for bad in [
            &["--new-window"][..],
            &["--raise"][..],
            &["--version"][..],
            &["--"][..],
            &["/work/app"][..],
            &["--clean", "/work/app"][..],
            &["--clean", "--", "--legacy"][..],
        ] {
            assert_eq!(flags(&os(bad)), None, "{bad:?}");
            assert_eq!(run(&os(bad)), glib::ExitCode::from(64), "{bad:?}");
        }
    }

    #[test]
    fn only_the_first_word_names_the_subcommand() {
        assert!(named(&os(&["choose-project", "--version"])));
        assert!(!named(&os(&["--clean", "choose-project"])));
        assert!(!named(&os(&["--", "choose-project"])));
        assert!(!named(&[]));
    }

    #[test]
    fn last_takes_the_newest_present_and_counts_home_only_when_nothing_else_is_left() {
        let scratch = Scratch::new("choose-project");
        let (home, older, gone) = (scratch.0.join("home"), scratch.0.join("older"), scratch.0.join("gone"));
        fs::create_dir(&home).unwrap();
        fs::create_dir(&older).unwrap();
        let state = scratch.0.join("state");
        let sources = Sources::from_env(
            Some(state.as_os_str()),
            Some(home.as_os_str()),
            scratch.0.join("control"),
        );
        fs::create_dir_all(sources.layout.as_ref().unwrap()).unwrap();
        stamp(&sources, &gone, 40);
        stamp(&sources, &home, 30);
        stamp(&sources, &older, 20);
        static BUSY: AtomicBool = AtomicBool::new(false);
        let read = || recent_projects::read_with(&sources, Box::new(OnDisk), PATIENCE, &BUSY);

        assert_eq!(
            next(DesktopStart::Last, &read()),
            Next::Raise(older.clone()),
            "home is newer, and it still waits behind a project"
        );
        fs::remove_dir(&older).unwrap();
        assert_eq!(next(DesktopStart::Last, &read()), Next::Raise(home));
    }

    #[test]
    fn last_with_nothing_present_shows_the_chooser() {
        assert_eq!(next(DesktopStart::Last, &[]), Next::Show);
        let rows = [row("/gone", Fact::Gone), row("/on/a/dead/mount", Fact::Unknown)];
        assert_eq!(next(DesktopStart::Last, &rows), Next::Show);
        assert_eq!(
            next(
                DesktopStart::Last,
                &[row("/gone", Fact::Gone), row("/open", Fact::Open)]
            ),
            Next::Raise("/open".into()),
            "a project with a window is a project, and the raise finds it"
        );
    }

    #[test]
    fn choose_shows_the_chooser_whatever_the_rows_hold() {
        let rows = [row("/work/app", Fact::Present)];
        assert_eq!(next(DesktopStart::Choose, &rows), Next::Show);
    }

    #[test]
    fn cwd_starts_with_no_raise() {
        let rows = [row("/work/app", Fact::Present)];
        assert_eq!(next(DesktopStart::Cwd, &rows), Next::Cwd);
        assert!(matches!(
            cwd_how(Ok("/work/here".into())),
            Ok(How::Plain(path)) if path == Path::new("/work/here")
        ));
    }

    /// Only for the test below: it has a working directory of its own to lose.
    #[test]
    fn a_helper_that_runs_with_no_working_directory() {
        if std::env::var_os(CWD_HELPER).is_some() {
            std::process::exit(i32::from(run(&[]) == glib::ExitCode::FAILURE));
        }
    }

    #[test]
    fn cwd_with_a_deleted_cwd_prints_no_such_directory_and_exits_1() {
        let scratch = Scratch::new("choose-project");
        let (config, gone) = (scratch.0.join("config"), scratch.0.join("gone"));
        fs::create_dir(&config).unwrap();
        fs::create_dir(&gone).unwrap();
        fs::write(
            config.join("init.lua"),
            "eitri.config.set(\"window.desktop_start\", \"cwd\")\n",
        )
        .unwrap();
        let output = Command::new("/bin/sh")
            .args(["-c", "cd \"$1\" && rmdir \"$1\" && shift && exec \"$@\"", "sh"])
            .arg(&gone)
            .arg(std::env::current_exe().unwrap())
            .args([
                "choose_project::tests::a_helper_that_runs_with_no_working_directory",
                "--exact",
                "--nocapture",
            ])
            .env(CWD_HELPER, "1")
            .env("EITRI_CONFIG_DIR", &config)
            .env("XDG_STATE_HOME", scratch.0.join("state"))
            // This path never reaches GTK; should that break, the window must find no display to open on.
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DISPLAY")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{stderr}");
        assert!(stderr.contains("eitri: no such directory"), "{stderr}");
    }

    #[test]
    fn the_kernel_loads_an_init_with_a_panel_and_no_gtk() {
        let scratch = Scratch::new("choose-project");
        fs::write(
            scratch.0.join("init.lua"),
            "eitri.panel.register({ id = \"mine\" })\n\
             eitri.plugin.load(\"mine\")\n\
             eitri.config.set(\"window.desktop_start\", \"last\")\n",
        )
        .unwrap();
        assert_eq!(
            desktop_start(&scratch.0),
            DesktopStart::Last,
            "a line after the panel still runs"
        );
    }

    #[test]
    fn a_bad_value_and_a_failed_init_are_the_default() {
        let scratch = Scratch::new("choose-project");
        let init = scratch.0.join("init.lua");
        fs::write(&init, "eitri.config.set(\"window.desktop_start\", \"later\")\n").unwrap();
        assert_eq!(desktop_start(&scratch.0), DesktopStart::Choose);
        fs::write(&init, "this is not lua\n").unwrap();
        assert_eq!(desktop_start(&scratch.0), DesktopStart::Choose);
        fs::remove_file(&init).unwrap();
        assert_eq!(desktop_start(&scratch.0), DesktopStart::Choose);
    }

    fn exited(code: Option<i32>) -> Ends {
        Ends::ChildExited(Exit { code, gone: false })
    }

    #[test]
    fn the_small_window_ends_on_each_of_the_four() {
        let exit = |code| {
            Some(Exit {
                code: Some(code),
                gone: false,
            })
        };
        assert_eq!(ends(None, false, false), None, "a start with nothing to show goes on");
        assert_eq!(ends(None, true, false), Some(Ends::LostActivation));
        assert_eq!(ends(exit(0), false, false), Some(exited(Some(0))));
        assert_eq!(ends(exit(3), false, false), Some(exited(Some(3))));
        assert_eq!(ends(None, false, true), Some(Ends::Deadline));
        assert_eq!(
            ends(exit(3), true, true),
            Some(exited(Some(3))),
            "a failure is heard, whatever else came with it"
        );
        assert_eq!(ends(None, true, true), Some(Ends::LostActivation));

        let mut chooser = opening();
        let app = Path::new("/work/app");
        for end in [Ends::LostActivation, exited(Some(0)), Ends::Deadline] {
            assert!(!settle(&mut chooser, end, app), "{end:?} ends the window");
        }
        assert!(settle(&mut chooser, exited(Some(3)), app), "and this one does not");
    }

    fn opening() -> Chooser {
        let mut chooser = Chooser::new(Mode::Launch, Some("/home/me".into()), Lang::En, 1);
        let effects = chooser.shown();
        let [Effect::List { serial }] = effects[..] else {
            panic!("{effects:?}");
        };
        let rows = vec![row("/work/app", Fact::Present)];
        assert_eq!(chooser.event(Ev::Rows { serial, rows }), vec![]);
        assert_eq!(
            chooser.event(Ev::Line(LineEv::Enter)),
            vec![Effect::Open("/work/app".into())]
        );
        chooser.opening("app");
        chooser
    }

    #[test]
    fn child_failure_brings_the_chooser_back_with_the_message() {
        for (exit, said) in [
            (
                Exit {
                    code: Some(1),
                    gone: false,
                },
                "could not open app: see its window log",
            ),
            (
                Exit {
                    code: None,
                    gone: false,
                },
                "could not open app: see its window log",
            ),
            (
                Exit {
                    code: Some(1),
                    gone: true,
                },
                "/work/app is not there",
            ),
        ] {
            let mut chooser = opening();
            assert!(chooser.view().opening);
            assert!(chooser.view().message.is_some_and(|text| text.contains("Opening app")));
            assert_eq!(chooser.event(Ev::Line(LineEv::Enter)), vec![], "a start is under way");

            assert!(settle(&mut chooser, Ends::ChildExited(exit), Path::new("/work/app")));

            let view = chooser.view();
            assert!(!view.opening, "the keys are back");
            assert_eq!(view.message.as_deref(), Some(said), "{exit:?}");
            assert_eq!(view.rows[0].name, "app", "the list is still there");
            assert_eq!(
                chooser.event(Ev::Line(LineEv::Enter)),
                vec![Effect::Open("/work/app".into())],
                "and it can be tried again"
            );
        }
    }

    #[test]
    fn a_child_that_left_no_code_failed() {
        let exit = Exit {
            code: None,
            gone: false,
        };
        assert_eq!(ends(Some(exit), false, false), Some(Ends::ChildExited(exit)));
    }

    #[test]
    fn a_click_beside_the_panel_and_a_focus_change_end_nothing() {
        assert_eq!(view_event(ChooserReport::Dismiss), None);
        assert_eq!(view_event(ChooserReport::Line(LineEv::KeysLost)), None);
        assert_eq!(
            view_event(ChooserReport::Line(LineEv::Escape)),
            Some(Ev::Line(LineEv::Escape)),
            "Esc is the way out"
        );
        assert_eq!(view_event(ChooserReport::Pick(2)), Some(Ev::Pick(2)));
        assert_eq!(view_event(ChooserReport::Folder(None)), Some(Ev::Folder(None)));
    }

    #[test]
    fn a_workers_answers_reach_the_chooser_and_its_starts_do_not() {
        assert_eq!(
            worker_event(WorkerAnswer::Path {
                serial: 3,
                answer: PathAnswer::Missing
            }),
            Some(Ev::PathAnswer {
                serial: 3,
                answer: PathAnswer::Missing
            })
        );
        assert_eq!(worker_event(WorkerAnswer::Started { failed: None }), None);
    }

    #[test]
    fn a_home_named_through_a_link_is_the_one_the_records_hold() {
        let scratch = Scratch::new("choose-project");
        let real = scratch.0.join("real");
        fs::create_dir(&real).unwrap();
        let link = scratch.0.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(canonical_home(Some(&link)), Some(real));
        let missing = scratch.0.join("missing");
        assert_eq!(canonical_home(Some(&missing)), Some(missing));
        assert_eq!(canonical_home(None), None);
    }

    /// The relay has to be up, for a `println!` in `init.lua` that fails; the task guard is what a process that runs no
    /// plugin task has no use for, and finding it spawns a helper. `--version` is read before all of them, and it has
    /// to leave this command line alone, or that flag would pass where every other is refused.
    #[test]
    fn the_subcommand_sits_after_the_relay_and_before_the_guard() {
        let code = code_only(&read_required("shell/src/main.rs").text);
        let [body] = functions(&code, "main")[..] else {
            panic!("main.rs has one main");
        };
        let at = |needle: &str| body.find(needle).unwrap_or_else(|| panic!("{needle} in main"));
        let order = [
            at("companion::env::drop_inherited_editor_env()"),
            at("&& !choose_project::named(&early_args)"),
            at("window_log::start_early()"),
            at("choose_project::run(&early_args[1..])"),
            at("tasks::install_guard("),
            at("project_root::resolve_launch()"),
        ];
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
    }
}
