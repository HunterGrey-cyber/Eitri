//! A window's project origin, and a remote project's window reaching its server.
//!
//! **At launch, before GTK.** The command line and the remote project records decide the origin
//! (`remote_project::decide_origin`): a root with a record opens as remote, `--ssh <host>` needs one, `--local` opens
//! a remote project's copy as a local project with no agent tabs. A remote origin needs `remote_dev.enabled`, read
//! from `init.lua` here only when the origin is remote. The origin is also the window's identity: [`control_dir`]
//! keeps a remote window's open socket apart from its copy's.
//!
//! **In the window.** [`prepare`] gives the panel its origin before any tab can start. After saved tabs are read,
//! [`start`] opens the connection on a worker (`remote_host::window::open_window`: the master, the host key, the
//! helper this build needs, installed with no question on a host the person consented to, and the connection to it).
//! The band says how far it is
//! ("connecting… / ready"); once it is open the panel gets it and its saved remote tabs come back, and resume and
//! network changes reach its master (`remote_resume`).

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use eitri_core::project_root::Launch;
use eitri_core::remote_host::window::{open_window, Opened};
use eitri_core::remote_project::{check_remote_dev, decide_origin, RemoteProject, RemoteProjects, WindowOrigin};
use futures_channel::mpsc::{unbounded, UnboundedSender};
use gtk4::glib;

use crate::agent_panel::AgentPanelHandle;

/// Set on a `--local` window's own process before anything starts, to the root of the remote project whose copy it
/// opens: window 2, its chat and page windows, `eitri page` and `eitri open` from its nvim and every launch from its
/// terminals inherit it, and open that root as the same copy unless `--ssh` says otherwise.
pub(crate) const LOCAL_COPY_ENV: &str = "EITRI_LOCAL_COPY";

/// Only a copy window passes copy state to its children, including when an explicit SSH launch uses the same root.
pub(crate) fn copy_root_for_children<'a>(origin: &WindowOrigin, root: &'a Path) -> Option<&'a Path> {
    origin.local_copy_host().map(|_| root)
}

fn local_copy_from_env() -> Option<PathBuf> {
    std::env::var_os(LOCAL_COPY_ENV)
        .filter(|root| !root.is_empty())
        .map(PathBuf::from)
}

fn store() -> RemoteProjects {
    RemoteProjects::new(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// The launch's origin. `Err` is the line to print before exiting: `--ssh` with no record of that root on that host,
/// a record that cannot be read, or a remote project while `remote_dev.enabled` is off. A root narrowed from a file
/// below a recorded remote project's root widens to that root ([`origin_in`]). A remote project's saved tabs are
/// readied here, before any window is built ([`carry_saved_tabs`]).
pub(crate) fn origin_for_launch(launch: &mut Launch) -> Result<WindowOrigin, String> {
    let origin = origin_in(&store(), launch, local_copy_from_env().as_deref())?;
    if origin.remote().is_some() {
        check_remote_dev(crate::host_command::remote_dev_enabled(), &origin)?;
    }
    carry_saved_tabs(&origin, &launch.root);
    Ok(origin)
}

/// [`origin_for_launch`] in `store`, before the switch: `local_copy` is [`LOCAL_COPY_ENV`]'s root. Opening a file names
/// the project by the nearest folder above it, and the folders above that up to the git top (`wider_roots`) are the
/// same project: the nearest of them that is a recorded remote project is the launch's project, its root the launch's
/// root, so the file reaches that project's window (or opens one) rather than a window of a folder inside it. A launch
/// of the root a `--local` window opened (from its terminal, say) opens the same copy.
fn origin_in(store: &RemoteProjects, launch: &mut Launch, local_copy: Option<&Path>) -> Result<WindowOrigin, String> {
    if launch.ssh.is_none() && !launch.wider_roots.is_empty() && store.find(&launch.root).ok().flatten().is_none() {
        let recorded = launch
            .wider_roots
            .iter()
            .position(|root| store.find(root).ok().flatten().is_some());
        if let Some(at) = recorded {
            let mut wider = launch.wider_roots.split_off(at);
            launch.root = wider.remove(0);
            launch.wider_roots = wider;
        }
    }
    let local = launch.local || (launch.ssh.is_none() && local_copy == Some(launch.root.as_path()));
    decide_origin(store, &launch.root, launch.ssh.as_deref(), local)
}

/// The origin of a window started for a project another way (window 2 of a pair, a chat or page window, `eitri open`):
/// the root's record decides, as for a launch that named only the root. Never a refusal, since the window is already
/// asked for: a record that cannot be read, or a remote project while `remote_dev.enabled` is off, opens as a copy,
/// whose agent tabs start nowhere.
pub(crate) fn origin_for_root(root: &Path) -> WindowOrigin {
    let local = local_copy_from_env().as_deref() == Some(root);
    let origin = origin_for_root_in(&store(), root, local, &crate::host_command::remote_dev_enabled);
    carry_saved_tabs(&origin, root);
    origin
}

/// [`origin_for_root`] in `store`: `local` when the window that started this one opened the root's copy.
fn origin_for_root_in(store: &RemoteProjects, root: &Path, local: bool, remote_dev: &dyn Fn() -> bool) -> WindowOrigin {
    match decide_origin(store, root, None, local) {
        Ok(WindowOrigin::Remote(project)) if !remote_dev() => {
            eprintln!(
                "eitri: {} is a project on {}; remote development is off",
                root.display(),
                project.host
            );
            WindowOrigin::LocalCopy { host: project.host }
        }
        Ok(origin) => origin,
        Err(why) => {
            eprintln!("eitri: {why}");
            WindowOrigin::LocalCopy {
                host: String::from("a server whose record cannot be read"),
            }
        }
    }
}

/// A remote project's saved tabs before its window is built: for each role a window of it can have, the remote tabs
/// of its server the project's shared file held are carried into the server's own directory, once
/// (`saved_tabs::carry_remote_tabs`). Disk work, at the launch, never on the GTK thread; one that fails is logged and
/// leaves those tabs where they were.
fn carry_saved_tabs(origin: &WindowOrigin, root: &Path) {
    let Some(project) = origin.remote() else {
        return;
    };
    let xdg = std::env::var_os("XDG_STATE_HOME");
    let home = std::env::var_os("HOME");
    let (Some(base), Some(keyed)) = (
        eitri_core::saved_tabs::state_dir(xdg.as_deref(), home.as_deref()),
        eitri_core::saved_tabs::state_dir_for(xdg.as_deref(), home.as_deref(), &origin.tab_origin()),
    ) else {
        return;
    };
    carry_saved_roles(&base, &keyed, root, &project.host_key_sha256);
}

fn carry_saved_roles(base: &Path, keyed: &Path, root: &Path, host_key: &str) {
    use eitri_core::window_link::pair::Role;
    for role in [Role::Main, Role::Second, Role::Unpaired] {
        if let Err(e) = eitri_core::saved_tabs::carry_remote_tabs(base, keyed, root, role, host_key) {
            eprintln!(
                "[tabs] the remote tabs saved before could not be carried to {}: {e}",
                keyed.display()
            );
        }
    }
}

/// The directory a window of `origin` holds its open socket in, and a launch looks for one: the shared control
/// directory for a local project and its copy, a subdirectory named by the server's key for a remote one, so a file
/// or a raise for one never reaches the other's window.
pub(crate) fn control_dir(origin: &WindowOrigin) -> PathBuf {
    let base =
        eitri_core::panel_control::control_dir(std::env::var_os("XDG_RUNTIME_DIR").as_deref(), &std::env::temp_dir());
    match origin.key() {
        Some(key) => base.join(key),
        None => base,
    }
}

/// What the worker tells the window.
enum Report {
    Band(String),
    Opened(Box<Opened>, Option<eitri_core::open_socket::OpenServer>),
    Failed(String),
    Replaced(String),
}

thread_local! {
    /// The resume and network listener of this window's connection, for as long as the window lives.
    static LISTENER: RefCell<Option<crate::remote_resume::Listener>> = const { RefCell::new(None) };
}

/// Opens a remote window's connection, on a worker each time: what [`start`] runs first, and again after a failed
/// first connection whenever the machine resumes or the network changes.
#[derive(Clone)]
struct Opener {
    /// The record as the launch read it, for when it cannot be read again.
    launched: RemoteProject,
    ssh_dir: PathBuf,
    exe: PathBuf,
    tell: UnboundedSender<Report>,
    opened: Arc<AtomicBool>,
    owns_socket: bool,
}

impl Opener {
    /// One attempt, blocking: the band lines and what it came to go to the window.
    fn attempt(&self) {
        let band = |line: String| {
            let _ = self.tell.unbounded_send(Report::Band(line));
        };
        let store = store();
        // The record as it is now: an earlier attempt may have moved it to a changed host key.
        let project = match retry_project(&store, &self.launched) {
            Ok(project) => project,
            Err(why) => {
                self.opened.store(true, Ordering::Release);
                let _ = self.tell.unbounded_send(Report::Replaced(why));
                return;
            }
        };
        let Some(result) = attempt_until_open(&self.opened, || {
            open_window(&project, &self.ssh_dir, &self.exe, &store, &band)
        }) else {
            return;
        };
        let report = match result {
            Ok(opened) => {
                let address = (self.owns_socket && self.launched.host_key_sha256 != opened.project.host_key_sha256)
                    .then(|| {
                        crate::open_requests::OpenRequests::claim_server(
                            &opened.project.root,
                            &WindowOrigin::Remote(opened.project.clone()),
                        )
                    })
                    .flatten();
                Report::Opened(Box::new(opened), address)
            }
            Err(why) => Report::Failed(why),
        };
        let _ = self.tell.unbounded_send(report);
    }

    fn on_a_worker(&self) -> std::io::Result<()> {
        let opener = self.clone();
        std::thread::Builder::new()
            .name("eitri-remote-open".into())
            .spawn(move || opener.attempt())
            .map(|_| ())
    }
}

fn retry_project(store: &RemoteProjects, launched: &RemoteProject) -> Result<RemoteProject, String> {
    let project = store
        .find(&launched.root)
        .ok()
        .flatten()
        .unwrap_or_else(|| launched.clone());
    if project.host != launched.host {
        return Err(format!(
            "{} now points at another server ({}); reopen this window",
            launched.root.display(),
            project.host
        ));
    }
    Ok(project)
}

/// Resume coalesces signals into sequential attempts; once one succeeds a queued signal has no work left.
fn attempt_until_open<T>(
    opened: &AtomicBool,
    attempt: impl FnOnce() -> Result<T, String>,
) -> Option<Result<T, String>> {
    if opened.load(Ordering::Acquire) {
        return None;
    }
    let result = attempt();
    if result.is_ok() {
        opened.store(true, Ordering::Release);
    }
    Some(result)
}

fn origin_changed(panel_key: &str, project: &RemoteProject) -> bool {
    panel_key != project.host_key_sha256
}

/// Gives the panel its origin before any tab can start. Opening the connection waits until saved files are read,
/// because that worker can migrate those files to a replacement key's directory.
pub(crate) fn prepare(panel: &AgentPanelHandle, origin: &WindowOrigin) -> bool {
    match origin {
        WindowOrigin::Local => {}
        WindowOrigin::LocalCopy { host } => panel.set_local_copy_host(Some(host.clone())),
        WindowOrigin::Remote(_) => {
            if let Err(why) = panel.set_project_origin(origin.tab_origin()) {
                eprintln!("[remote] {why}");
                return false;
            }
        }
    }
    true
}

/// Opens the prepared remote origin after saved-tab readers and writers are initialized. A first connection that
/// fails is tried again on the next resume or network change, one attempt at a time, until one opens.
pub(crate) fn start(
    panel: &AgentPanelHandle,
    origin: &WindowOrigin,
    opens: Option<std::rc::Rc<crate::open_requests::OpenRequests>>,
    pair: std::rc::Rc<crate::linked_window::spawn::Pair>,
) {
    match origin {
        WindowOrigin::Local | WindowOrigin::LocalCopy { .. } => {}
        WindowOrigin::Remote(project) => {
            let Some(ssh_dir) = eitri_core::remote_host::cli::ssh_dir(
                std::env::var_os("XDG_STATE_HOME").as_deref(),
                std::env::var_os("HOME").as_deref(),
            ) else {
                panel.show_notice(&format!("{}: no state directory for its ssh connection", project.host));
                return;
            };
            let exe = match std::env::current_exe() {
                Ok(exe) => exe,
                Err(e) => {
                    panel.show_notice(&format!(
                        "{}: this Eitri's executable cannot be found ({e})",
                        project.host
                    ));
                    return;
                }
            };
            let (tell, mut told) = unbounded();
            let host = project.host.clone();
            let opener = Opener {
                launched: project.clone(),
                ssh_dir,
                exe,
                tell,
                opened: Arc::new(AtomicBool::new(false)),
                owns_socket: opens.is_some(),
            };
            if let Err(e) = opener.on_a_worker() {
                panel.show_notice(&format!("{host}: the connection could not start ({e})"));
                return;
            }
            let panel = panel.clone();
            let mut panel_key = project.host_key_sha256.clone();
            glib::spawn_future_local(async move {
                while let Ok(report) = told.recv().await {
                    match report {
                        Report::Band(line) => {
                            println!("[remote] {line}");
                            panel.show_notice(&line);
                        }
                        Report::Replaced(why) => {
                            panel.show_notice(&why);
                            LISTENER.with(|slot| *slot.borrow_mut() = None);
                        }
                        Report::Failed(why) => {
                            eprintln!("[remote] {host}: {why}");
                            panel.show_notice(&format!("{host}: {why}"));
                            retry_later(&opener);
                        }
                        Report::Opened(opened, address) => {
                            if connected(&panel, *opened, &mut panel_key, &pair) {
                                if let (Some(opens), Some(address)) = (&opens, address) {
                                    opens.add_origin(address);
                                }
                            }
                        }
                    }
                }
            });
        }
    }
}

/// After a failed connection: the next resume or network change tries again, on a worker, one attempt at a time. The
/// listener stays until a connection opens and the master's own replaces it.
fn retry_later(opener: &Opener) {
    LISTENER.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            let opener = opener.clone();
            *slot = Some(crate::remote_resume::listen(crate::remote_resume::Resume::new(
                move |_| opener.attempt(),
            )));
        }
    });
}

/// The connection is open: a changed host key becomes the window's origin first (refused once a tab has started,
/// when the window has to be opened again), then the panel gets the connection and the master is followed through
/// resume and network changes.
fn connected(
    panel: &AgentPanelHandle,
    opened: Opened,
    panel_key: &mut String,
    pair: &crate::linked_window::spawn::Pair,
) -> bool {
    let Opened {
        connection,
        project,
        key_changed: _,
        master,
    } = opened;
    let origin = WindowOrigin::Remote(project.clone()).tab_origin();
    let dir = eitri_core::saved_tabs::state_dir_for(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
        &origin,
    );
    if !follow_origin(panel, &project, panel_key, pair, dir) {
        return false;
    }
    panel.set_remote_connection(connection);
    let listener = crate::remote_resume::listen(crate::remote_resume::Resume::for_masters(vec![master]));
    LISTENER.with(|slot| *slot.borrow_mut() = Some(listener));
    true
}

/// The panel and compositor writer follow the same accepted origin before the connection can restore tabs.
fn follow_origin(
    panel: &AgentPanelHandle,
    project: &RemoteProject,
    panel_key: &mut String,
    pair: &crate::linked_window::spawn::Pair,
    tabs_dir: Option<PathBuf>,
) -> bool {
    if let eitri_core::remote_project::TabOrigin::Remote { host, root, .. } = panel.project_origin() {
        if host != project.host || root != project.root {
            panel.show_notice("this project now points at another server; reopen this window");
            return false;
        }
    }
    if origin_changed(panel_key, project) {
        let origin = WindowOrigin::Remote(project.clone()).tab_origin();
        if let Err(why) = panel.set_project_origin(origin) {
            panel.show_notice(&format!(
                "{}: {why}; open this window again to use the server under its new key",
                project.host
            ));
            return false;
        }
        if let Some(dir) = tabs_dir {
            if let Err(why) = pair.rekey_group_tabs(dir, panel_key, &project.host_key_sha256) {
                panel.show_notice(&format!("{}: {why}", project.host));
                return false;
            }
        }
        *panel_key = project.host_key_sha256.clone();
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_refuse_a_roots_replacement_host_user_or_port_but_follow_its_new_key() {
        let store = store_with("/srv/project");
        let launched = record("/srv/project");
        for host in ["other-example-host", "user@example-host", "ssh://example-host:2222"] {
            let mut replacement = launched.clone();
            replacement.host = host.into();
            store.save(&replacement).unwrap();
            let why = retry_project(&store, &launched).unwrap_err();
            assert!(why.contains(host) && why.contains("reopen"), "{why}");
        }
        let mut replacement = launched.clone();
        replacement.host_key_sha256 = "SHA256:new".into();
        store.save(&replacement).unwrap();
        assert_eq!(retry_project(&store, &launched).unwrap(), replacement);
    }

    #[test]
    fn a_connected_window_never_rekeys_handoffs_to_another_host() {
        use crate::linked_window::spawn::Pair;
        let root = Path::new("/srv/project");
        let pair = Pair::full(Ok(None), root, false, false);
        let panel = AgentPanelHandle::for_host_test_without_pages();
        let launched = record("/srv/project");
        assert!(prepare(&panel, &WindowOrigin::Remote(launched.clone())));
        let mut key = launched.host_key_sha256.clone();
        for new_key in [launched.host_key_sha256.as_str(), "SHA256:other"] {
            let mut replacement = launched.clone();
            replacement.host = "other-example-host".into();
            replacement.host_key_sha256 = new_key.into();
            assert!(!follow_origin(&panel, &replacement, &mut key, &pair, None));
            assert_eq!(key, launched.host_key_sha256);
            assert_eq!(
                panel.project_origin(),
                WindowOrigin::Remote(launched.clone()).tab_origin()
            );
            assert!(panel
                .set_project_origin(WindowOrigin::Remote(replacement).tab_origin())
                .is_err());
        }
    }
    #[test]
    fn the_connection_worker_starts_after_both_saved_tab_readers() {
        let main = eitri_core::source_scan::read_required("shell/src/main.rs");
        let code: String = eitri_core::source_scan::code_only(&main.text)
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let prepare = code.find("remote_window::prepare(").unwrap();
        let panel_read = code.find("agent_panel_handle.set_tab_files(").unwrap();
        let group_read = code.find("group_tabs::GroupTabs::read(").unwrap();
        let start = code.find("remote_window::start(").unwrap();
        assert!(prepare < panel_read && prepare < group_read);
        assert!(
            panel_read < start && group_read < start,
            "key migration must not overtake either saved-tab reader"
        );
    }

    #[test]
    fn a_connected_origin_change_moves_the_hubs_writer_with_the_panels() {
        use crate::linked_window::{group_tabs::GroupTabs, spawn::Pair};
        use eitri_core::agent_bridge::SessionModeChoice;
        use eitri_core::saved_tabs::{self, SavedRemoteTab, SavedTabs};
        use eitri_core::window_link::names::WindowName;
        let state = agent::state_dirs::test_workspace_dir("remote-hub-rekey");
        let root = Path::new("/srv/project");
        let mut project = record("/srv/project");
        let old = state.join("old");
        let new = state.join("new");
        let mut group = GroupTabs::read(Some(&old), root, true);
        let saved = SavedTabs::default().with_remote(vec![SavedRemoteTab {
            helper: agent::HelperHandoff {
                host: "example-host".into(),
                host_key_sha256: project.host_key_sha256.clone(),
                daemon: "installation".into(),
                sidecar_id: "sc-1".into(),
                session_id: "session-1".into(),
                last_folded_sequence: 4,
                declared_client_tools: Default::default(),
            },
            provider_session_id: None,
            name: None,
            mode: SessionModeChoice::Auto,
            window: None,
        }]);
        group.reported(&WindowName::Chat(1), &saved.to_wire());
        assert!(group.wait_written());
        let pair = Pair::full(Ok(None), root, false, false);
        pair.keep_group_tabs(group);
        let panel = AgentPanelHandle::for_host_test_without_pages();
        assert!(prepare(&panel, &WindowOrigin::Remote(project.clone())));
        let mut panel_key = project.host_key_sha256.clone();
        project.host_key_sha256 = "SHA256:new".into();
        assert!(follow_origin(
            &panel,
            &project,
            &mut panel_key,
            &pair,
            Some(new.clone())
        ));
        pair.finish_group_tabs()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(60))
            .unwrap();
        assert_eq!(panel_key, "SHA256:new");
        let saved_tabs::Loaded::Saved(saved) = saved_tabs::load(&new, root) else {
            panic!("replacement file missing")
        };
        assert_eq!(saved.remote[0].helper.host_key_sha256, "SHA256:new");
        assert_eq!(saved.remote[0].helper.daemon, "installation");
    }

    fn launch(root: &str, wider: &[&str]) -> Launch {
        Launch {
            root: root.into(),
            wider_roots: wider.iter().map(PathBuf::from).collect(),
            open: Vec::new(),
            new_window: false,
            raise: false,
            notes: Vec::new(),
            ssh: None,
            local: false,
        }
    }

    fn record(root: &str) -> RemoteProject {
        RemoteProject {
            version: 1,
            host: "example-host".into(),
            host_key_sha256: "SHA256:example".into(),
            root: root.into(),
            helper_version: "0.3.0".into(),
            sync: None,
            created_at: 1,
            queued_stops: Vec::new(),
        }
    }

    fn store_with(root: &str) -> RemoteProjects {
        let state = agent::state_dirs::test_workspace_dir("remote-window-origin");
        let store = RemoteProjects::new(Some(state.as_os_str()), None);
        store.save(&record(root)).unwrap();
        store
    }

    /// A file below a remote project's root, opened from a terminal outside Eitri, names a folder inside the project;
    /// the project is the recorded root above it, so the launch is that root's, and the file goes to (or opens) the
    /// remote project's window, its socket looked for where that origin's windows keep theirs.
    #[test]
    fn a_file_inside_a_remote_project_opens_in_that_project() {
        let store = store_with("/srv/project");
        let mut narrowed = launch("/srv/project/src", &["/srv/project"]);
        let origin = origin_in(&store, &mut narrowed, None).unwrap();
        assert_eq!(origin, WindowOrigin::Remote(record("/srv/project")));
        assert_eq!(narrowed.root, PathBuf::from("/srv/project"));
        assert!(narrowed.wider_roots.is_empty());
        assert_ne!(control_dir(&origin), control_dir(&WindowOrigin::Local));

        let mut deeper = launch("/srv/project/a/b", &["/srv/project/a", "/srv/project", "/srv"]);
        assert!(origin_in(&store, &mut deeper, None).unwrap().remote().is_some());
        assert_eq!(deeper.root, PathBuf::from("/srv/project"));
        assert_eq!(deeper.wider_roots, [PathBuf::from("/srv")]);

        let mut elsewhere = launch("/srv/other/src", &["/srv/other"]);
        assert_eq!(origin_in(&store, &mut elsewhere, None).unwrap(), WindowOrigin::Local);
        assert_eq!(elsewhere.root, PathBuf::from("/srv/other/src"));

        let mut copy = launch("/srv/project/src", &["/srv/project"]);
        copy.local = true;
        let origin = origin_in(&store, &mut copy, None).unwrap();
        assert_eq!(origin.local_copy_host(), Some("example-host"));
        assert_eq!(copy.root, PathBuf::from("/srv/project"));
    }

    /// What a `--local` window starts (its terminal's launches, its window 2 and chat windows, `eitri page` from its
    /// nvim) opens the root as the same copy; `--ssh` still asks for the server, and another root is untouched.
    #[test]
    fn the_local_copy_reaches_what_the_window_starts() {
        let store = store_with("/srv/project");
        let copy_root = Some(Path::new("/srv/project"));
        let mut again = launch("/srv/project", &[]);
        assert_eq!(
            origin_in(&store, &mut again, copy_root).unwrap().local_copy_host(),
            Some("example-host")
        );
        let mut file = launch("/srv/project/src", &["/srv/project"]);
        assert_eq!(
            origin_in(&store, &mut file, copy_root).unwrap().local_copy_host(),
            Some("example-host")
        );
        let mut ssh = launch("/srv/project", &[]);
        ssh.ssh = Some("example-host".into());
        let remote = origin_in(&store, &mut ssh, copy_root).unwrap();
        assert!(remote.remote().is_some());
        assert_eq!(copy_root_for_children(&remote, Path::new("/srv/project")), None);
        assert_eq!(
            copy_root_for_children(&WindowOrigin::Local, Path::new("/srv/project")),
            None
        );
        let mut other = launch("/srv/other", &[]);
        assert_eq!(origin_in(&store, &mut other, copy_root).unwrap(), WindowOrigin::Local);

        let on = || true;
        assert_eq!(
            origin_for_root_in(&store, Path::new("/srv/project"), true, &on).local_copy_host(),
            Some("example-host")
        );
        assert!(origin_for_root_in(&store, Path::new("/srv/project"), false, &on)
            .remote()
            .is_some());
        let local_copy = WindowOrigin::LocalCopy {
            host: "example-host".into(),
        };
        assert_eq!(control_dir(&local_copy), control_dir(&WindowOrigin::Local));
    }

    /// A window 2 left waiting by a remote project's window 1 is found only by a window 1 of that origin: a window of
    /// the project's copy looks for its own in another directory.
    #[test]
    fn an_orphan_of_one_origin_is_not_found_from_the_other() {
        let remote = crate::linked_window::relink::orphan_dir_path(&WindowOrigin::Remote(record("/srv/project")));
        let local = crate::linked_window::relink::orphan_dir_path(&WindowOrigin::Local);
        let copy = crate::linked_window::relink::orphan_dir_path(&WindowOrigin::LocalCopy {
            host: "example-host".into(),
        });
        assert_ne!(remote, local);
        assert_eq!(copy, local);
        assert_eq!(remote.parent(), Some(local.as_path()));
    }
    #[test]
    fn a_retry_after_reconciliation_updates_the_panel_and_queued_wakes_do_not_open_twice() {
        let store = store_with("/srv/project");
        let original = store.find(Path::new("/srv/project")).unwrap().unwrap();
        let panel_key = original.host_key_sha256.clone();
        let opened = AtomicBool::new(false);
        let failed: Option<Result<RemoteProject, String>> = attempt_until_open(&opened, || {
            eitri_core::remote_project::reconcile_host_key(&store, &original, "SHA256:new").unwrap();
            Err("the new key still needs installation consent".into())
        });
        assert!(failed.unwrap().is_err());
        let project = attempt_until_open(&opened, || Ok(store.find(&original.root).unwrap().unwrap()))
            .unwrap()
            .unwrap();
        assert!(origin_changed(&panel_key, &project));
        assert!(!origin_changed(&project.host_key_sha256, &project));
        assert!(attempt_until_open::<()>(&opened, || panic!("a queued network signal must not open again")).is_none());
    }
    #[test]
    fn legacy_tabs_of_windows_not_restored_are_carried_before_the_offer() {
        use eitri_core::saved_tabs::{self, SavedRemoteTab, SavedTabs, TabFiles};
        use eitri_core::window_link::pair::Role;
        let state = agent::state_dirs::test_workspace_dir("remote-unrestored-carry");
        let base = state.join("tabs");
        let keyed = base.join("remote");
        let root = Path::new("/srv/project");
        let tab = SavedRemoteTab {
            helper: agent::HelperHandoff {
                daemon: "0.3.0-abcdef0".into(),
                host: "example-host".into(),
                host_key_sha256: "SHA256:example".into(),
                sidecar_id: "sc-2".into(),
                session_id: "s-2".into(),
                last_folded_sequence: 0,
                declared_client_tools: Default::default(),
            },
            provider_session_id: None,
            name: None,
            mode: eitri_core::agent_bridge::SessionModeChoice::Auto,
            window: None,
        };
        saved_tabs::save_as(
            &base,
            root,
            Role::Second,
            &SavedTabs::default().with_remote(vec![tab.clone()]),
        )
        .unwrap();
        carry_saved_roles(&base, &keyed, root, "SHA256:example");
        let files = TabFiles {
            role: Role::Main,
            also_offer: vec![Role::Second],
            offers_own: true,
        };
        let (offer, _, _) = files.startup(Some(&keyed), root);
        assert_eq!(offer.unwrap().remote, [tab]);
    }
}
