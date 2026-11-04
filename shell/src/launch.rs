use super::*;

/// Which kind of process this is, and so which application id it registers: a full window or window 2 is the main
/// application, a chat window is the panel's, a page window has its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppKind {
    Main,
    Chat,
    Page,
}

impl AppKind {
    pub(crate) fn of(start: &linked_window::WindowStart) -> AppKind {
        match start {
            linked_window::WindowStart::Module {
                name: WindowName::Chat(_),
                ..
            } => AppKind::Chat,
            linked_window::WindowStart::Module {
                name: WindowName::Page(_),
                ..
            }
            | linked_window::WindowStart::Beside { .. } => AppKind::Page,
            _ => AppKind::Main,
        }
    }

    pub(crate) fn id(self) -> &'static str {
        match self {
            AppKind::Main => APP_ID,
            AppKind::Chat => companion::PANEL_APP_ID,
            AppKind::Page => PAGE_APP_ID,
        }
    }
}

/// The module a chat or page window shows alone: a chat window shows its chat in its own base view, whatever its
/// number, and a page window its plugin's page.
pub(crate) fn module_of_window(name: &WindowName) -> Option<ModuleId> {
    match name {
        WindowName::Second => None,
        WindowName::Chat(_) => Some(ModuleId::agent()),
        WindowName::Page(_) => name.module(),
    }
}

/// How long the top bar's app name shows the prefix indicator's block when a layout verb is refused
/// (the refusal flashes the top bar, the same way the prefix indicator shows).
pub(crate) const REFUSAL_FLASH: std::time::Duration = std::time::Duration::from_millis(200);

/// Resolve the user's configuration independently of the project's working directory.
pub(crate) fn config_dir() -> PathBuf {
    eitri_core::config_dir::resolve(&|k| std::env::var_os(k)).unwrap_or_else(|err| {
        eprintln!("{err}");
        std::process::exit(1);
    })
}

/// The guard plugin tasks run under: `eitri-run-guard` from beside this executable, which loads neither GTK nor
/// WebKitGTK, or this executable's own `run-guard` when the helper is not there (a build of the `shell` binary
/// alone) or is another build's.
pub(crate) fn task_guard_command() -> eitri_core::plugin::tasks::GuardCommand {
    let helper = std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join("eitri-run-guard")))
        .filter(|helper| helper.is_file())
        .filter(|helper| helper_is_this_build(helper));
    match helper {
        Some(program) => eitri_core::plugin::tasks::GuardCommand {
            program,
            args: Vec::new(),
            env: Vec::new(),
        },
        None => eitri_core::plugin::tasks::GuardCommand {
            program: "/proc/self/exe".into(),
            args: vec![eitri_core::run_guard::SUBCOMMAND.into()],
            env: Vec::new(),
        },
    }
}

/// Whether `helper` is the guard this executable was built with. A helper left beside it by an earlier build (`cargo
/// run -p shell` builds only this binary) would run this window's tasks under another build's guard. The helper
/// answers in about a millisecond; one that does not within the bound is passed over.
fn helper_is_this_build(helper: &Path) -> bool {
    use std::io::Read;
    let spawned = std::process::Command::new(helper)
        .arg(eitri_core::run_guard::FINGERPRINT_FLAG)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn();
    let Ok(mut child) = spawned else {
        return false;
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
    let mut answer = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut answer);
    }
    let same = answer.trim() == eitri_core::run_guard::fingerprint();
    if !same {
        eprintln!(
            "[eitri] {} is another build's task guard; plugin tasks run under this executable's own",
            helper.display()
        );
    }
    same
}

/// The hub's compositor record is kept up to date only on a normal start: `--clean` neither reads it nor writes
/// the default set over it, so the next normal start still finds the saved windows.
pub(crate) fn cm_record_kept(
    start: Option<eitri_core::window_link::cm_file::CmFile>,
    want_clean: bool,
) -> Option<eitri_core::window_link::cm_file::CmFile> {
    start.filter(|_| !want_clean)
}

/// A window started by window 1 takes its start's token and flags. With no link to window 1 it has nothing to be.
pub(crate) fn linked_pair(
    pair: std::io::Result<Rc<linked_window::spawn::Pair>>,
    token: Option<String>,
    restore: bool,
    backend_kind: eitri_core::agent_backend::BackendKind,
    want_clean: bool,
) -> Rc<linked_window::spawn::Pair> {
    match pair {
        Ok(pair) => {
            pair.set_activation(token);
            pair.set_restored(restore);
            pair.set_launch_flags(
                backend_kind == eitri_core::agent_backend::BackendKind::Legacy,
                want_clean,
            );
            pair
        }
        Err(err) => {
            eprintln!("eitri: the link to window 1 could not be started: {err}");
            std::process::exit(2);
        }
    }
}

/// Whether the first-start screen and its onboarding may open in this window: a full window's only, never one another
/// window started, one beside a person's nvim or one whose pair another window holds.
pub(crate) fn onboarding_full_window(pair: &linked_window::spawn::Pair) -> bool {
    !pair.is_second() && !pair.is_unpaired() && !pair.is_beside()
}

/// What this window's start knows of itself before `init.lua` runs.
pub(crate) fn early_facts(pair: &linked_window::spawn::Pair, shows_chat: bool) -> eitri_core::wm::start::EarlyFacts {
    let env = |name: &str| std::env::var_os(name);
    let module_orphans = pair.relinked_module_windows();
    eitri_core::wm::start::EarlyFacts {
        // Window 1 started it, or it is a page window beside nvim: its mode is not in question either way.
        started: pair.is_second() || pair.is_beside(),
        shows_chat,
        mac: cfg!(target_os = "macos"),
        wayland: eitri_core::compositor::mode::wayland_session(&env),
        wm: eitri_core::compositor::detect(&env),
        window2_orphan: pair.relink_holders().is_some() && !module_orphans,
        module_orphans,
        unpaired: pair.is_unpaired(),
        alone: pair.is_alone(),
        choice: None,
    }
}
