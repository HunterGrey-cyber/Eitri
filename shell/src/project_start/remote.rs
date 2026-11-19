//! The GTK carrier for the remote project chooser. Its worker owns the connection and questions.

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};

use eitri_core::remote_host::{
    cli::{self, Context, SshReach, Terminal},
    cli_sync::SyncSettings,
    first_connect::{self, FolderCompletion, FolderRequest, WindowLaunch},
    host_list,
};
use eitri_core::remote_project::{RemoteProject, RemoteProjects};
use eitri_core::theme::ThemeTokens;
use futures_channel::mpsc::{unbounded, UnboundedSender};
use gtk4::{gio, glib, prelude::*};
use serde_json::{json, Value};
use webkit6::prelude::*;

use super::{How, FAILED_WITHIN};

const SCHEME: &str = "eitri-remote-project";
const ENTRY: &str = "eitri-remote-project://chooser/open-on-host";

pub(crate) fn first_request(store: &RemoteProjects, args: &[OsString]) -> Result<Option<(String, PathBuf)>, String> {
    let Some((host, root)) = eitri_core::project_root::ssh_request(args)? else {
        return Ok(None);
    };
    let record = store
        .find(&root)
        .map_err(|e| format!("cannot read the remote project record of {} ({e})", root.display()))?;
    Ok(record.is_none().then_some((host, root)))
}

pub(crate) fn store() -> RemoteProjects {
    RemoteProjects::new(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

enum Page {
    Hosts {
        hosts: Vec<String>,
    },
    Question {
        id: u32,
        text: String,
    },
    Folder {
        id: u32,
    },
    Suggestions {
        id: u32,
        prefix: String,
        folders: Vec<String>,
        error: Option<String>,
    },
    Line {
        text: String,
    },
    Error {
        text: String,
    },
}

impl Page {
    fn json(self) -> Value {
        match self {
            Self::Hosts { hosts } => json!({"kind": "hosts", "hosts": hosts}),
            Self::Question { id, text } => json!({"kind": "question", "id": id, "text": text}),
            Self::Folder { id } => json!({"kind": "folder", "id": id}),
            Self::Suggestions {
                id,
                prefix,
                folders,
                error,
            } => {
                json!({"kind": "suggestions", "id": id, "prefix": prefix, "folders": folders, "error": error})
            }
            Self::Line { text } => json!({"kind": "line", "text": text}),
            Self::Error { text } => json!({"kind": "error", "text": text}),
        }
    }
}

enum Action {
    Ready,
    Host { host: String },
    Answer { id: u32, yes: bool },
    Folder { id: u32, path: String },
    Complete { id: u32, prefix: String },
    Cancel,
}

impl Action {
    fn parse(text: &str) -> Option<Self> {
        let payload: Value = serde_json::from_str(text).ok()?;
        let id = || u32::try_from(payload["id"].as_u64()?).ok();
        let string = |key| payload[key].as_str().map(str::to_owned);
        Some(match payload["kind"].as_str()? {
            "ready" => Self::Ready,
            "host" => Self::Host { host: string("host")? },
            "answer" => Self::Answer {
                id: id()?,
                yes: payload["yes"].as_bool()?,
            },
            "folder" => Self::Folder {
                id: id()?,
                path: string("path")?,
            },
            "complete" => Self::Complete {
                id: id()?,
                prefix: string("prefix")?,
            },
            "cancel" => Self::Cancel,
            _ => return None,
        })
    }
}

enum Display {
    Page(Page),
    Token(mpsc::Sender<Option<String>>),
    Opened,
}

struct PageTerminal {
    events: UnboundedSender<Display>,
    actions: mpsc::Receiver<Action>,
    closed: Arc<AtomicBool>,
    question: u32,
    flags: Vec<OsString>,
}

impl PageTerminal {
    fn send(&self, page: Page) {
        let _ = self.events.unbounded_send(Display::Page(page));
    }

    fn next(&mut self) -> u32 {
        self.question += 1;
        self.question
    }

    fn action(&self) -> Result<Action, String> {
        if self.cancelled() {
            return Err("first connection cancelled".into());
        }
        match self.actions.recv() {
            Ok(Action::Cancel) | Err(_) => Err("first connection cancelled".into()),
            Ok(action) => Ok(action),
        }
    }
}

impl Terminal for PageTerminal {
    fn say(&mut self, line: &str) {
        self.send(Page::Line { text: line.into() });
    }

    fn ask(&mut self, text: &str) -> bool {
        let id = self.next();
        self.send(Page::Question { id, text: text.into() });
        while let Ok(action) = self.action() {
            if let Action::Answer { id: reply, yes } = action {
                if reply == id {
                    return yes;
                }
            }
        }
        false
    }

    fn folder(&mut self, completion: Option<FolderCompletion>) -> Result<FolderRequest, String> {
        // The single folder prompt keeps its ID even if completing it first needs a consent question.
        let id = 0;
        if let Some(FolderCompletion { prefix, result }) = completion {
            let (folders, error) = match result {
                Ok(folders) => (folders, None),
                Err(error) => (Vec::new(), Some(error)),
            };
            self.send(Page::Suggestions {
                id,
                prefix,
                folders,
                error,
            });
        } else {
            self.send(Page::Folder { id });
        }
        loop {
            match self.action()? {
                Action::Folder { id: reply, path } if reply == id => return Ok(FolderRequest::Open(path.into())),
                Action::Complete { id: reply, prefix } if reply == id => {
                    return Ok(FolderRequest::Complete(prefix));
                }
                _ => {}
            }
        }
    }

    fn watch_first_sync(&mut self) -> mpsc::Receiver<()> {
        mpsc::channel().1
    }

    fn recorded(&mut self, project: &RemoteProject) {
        self.say(&format!("Recorded {}:{}", project.host, project.root.display()));
    }

    fn cancelled(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn tree_ready(&mut self, project: &RemoteProject, git_pct: u8) -> Result<Option<WindowLaunch>, String> {
        if git_pct < 100 {
            self.say(&format!("history arriving: {git_pct} %"));
        }
        let (send, recv) = mpsc::channel();
        self.events
            .unbounded_send(Display::Token(send))
            .map_err(|_| "chooser closed")?;
        let token = recv.recv_timeout(FAILED_WITHIN).map_err(|_| "chooser closed")?;
        if self.cancelled() {
            return Err("first connection cancelled".into());
        }
        let exe = super::exe_to_start(std::env::current_exe(), &|path| path.exists());
        let how = How::Remote {
            host: project.host.clone(),
            path: project.root.clone(),
        };
        let started = super::start(&exe, &self.flags, how, token.as_deref())
            .map_err(|e| format!("could not start the project window: {e}"))?;
        Ok(Some(WindowLaunch {
            pid: started.pid,
            ended: started.ended,
        }))
    }
}

fn run_worker(term: &mut PageTerminal, settings: SyncSettings, seed: Option<(String, PathBuf)>) -> Result<(), String> {
    // Readiness means the page has installed its receiver.
    loop {
        if matches!(term.action()?, Action::Ready) {
            break;
        }
    }
    let store = store();
    let home = std::env::var_os("HOME");
    let state = std::env::var_os("XDG_STATE_HOME");
    let (host, root) = match seed {
        Some((host, root)) => (host, Some(root)),
        None => {
            let hosts = host_list::hosts(home.as_deref().map(Path::new), &store, |line| term.say(&line))?;
            term.send(Page::Hosts { hosts });
            loop {
                if let Action::Host { host } = term.action()? {
                    break (host, None);
                }
            }
        }
    };
    let ssh_dir =
        cli::ssh_dir(state.as_deref(), home.as_deref()).ok_or("no state directory (set HOME or XDG_STATE_HOME)")?;
    let known_hosts = home
        .map(|parent| PathBuf::from(parent).join(".ssh/known_hosts"))
        .unwrap_or_default();
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut reach = SshReach::new(ssh_dir, exe, known_hosts).with_sync_settings(settings);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    first_connect::run(
        &mut Context {
            store: &store,
            reach: &mut reach,
            term,
            remote_dev_enabled: &|| true,
            now,
        },
        host,
        root,
    )
}

pub(crate) fn run(seed: (String, PathBuf), flags: Vec<OsString>, settings: SyncSettings) -> glib::ExitCode {
    if !settings.enabled {
        eprintln!("eitri: {}", eitri_core::remote_project::remote_dev_off("eitri --ssh"));
        return glib::ExitCode::FAILURE;
    }
    if !crate::webkit_sandbox::decision().allows_webviews() {
        eprintln!("eitri: the SSH project chooser needs WebKit; use eitri host open for first connect");
        return glib::ExitCode::FAILURE;
    }
    let app = crate::build_application(crate::AppKind::Main);
    let launch = std::cell::RefCell::new(Some((seed, flags, settings)));
    app.connect_activate(move |app| {
        if let Some((seed, flags, settings)) = launch.borrow_mut().take() {
            let window = gtk4::ApplicationWindow::new(app);
            show(
                Some(app),
                window.upcast_ref(),
                flags,
                settings,
                Some(seed),
                &ThemeTokens::fallback(),
                || {},
            );
            crate::launch_env::window_up();
        }
    });
    app.run_with_args::<&str>(&[])
}

pub(crate) fn available(settings: &SyncSettings) -> bool {
    settings.enabled && crate::webkit_sandbox::decision().allows_webviews()
}

pub(crate) fn open(
    parent: &gtk4::Window,
    flags: Vec<OsString>,
    settings: SyncSettings,
    theme: &ThemeTokens,
    opened: impl FnOnce() + 'static,
) {
    let window = gtk4::Window::builder()
        .transient_for(parent)
        .modal(true)
        .destroy_with_parent(true)
        .build();
    let app = parent.application();
    show(app.as_ref(), &window, flags, settings, None, theme, opened);
}

fn show(
    app: Option<&gtk4::Application>,
    window: &gtk4::Window,
    flags: Vec<OsString>,
    settings: SyncSettings,
    seed: Option<(String, PathBuf)>,
    theme: &ThemeTokens,
    opened: impl FnOnce() + 'static,
) {
    window.set_title(Some("Open on a host…"));
    window.set_default_size(720, 520);
    let content = webkit6::UserContentManager::new();
    let session = webkit6::NetworkSession::new_ephemeral();
    session.set_proxy_settings(
        webkit6::NetworkProxyMode::Custom,
        Some(&webkit6::NetworkProxySettings::new(
            Some(crate::agent_panel::PANEL_NETWORK_PROXY_URI),
            &[],
        )),
    );
    session.connect_download_started(|_, download| download.cancel());
    let document = crate::agent_panel::panel_document();
    let html = glib::Bytes::from_owned(document.themed(&theme.css_vars()).into_bytes());
    let context = webkit6::WebContext::new();
    context.register_uri_scheme(SCHEME, move |request| {
        if request.uri().as_deref() == Some(ENTRY) {
            let stream = gio::MemoryInputStream::from_bytes(&html);
            request.finish(&stream, html.len() as i64, Some("text/html; charset=utf-8"));
        } else {
            request.finish_error(&mut glib::Error::new(gio::IOErrorEnum::NotFound, "not found"));
        }
    });
    let view = webkit6::WebView::builder()
        .web_context(&context)
        .network_session(&session)
        .user_content_manager(&content)
        .default_content_security_policy(document.csp())
        .build();
    view.connect_create(|_, _| None);
    let initial = Cell::new(true);
    view.connect_decide_policy(move |_, decision, kind| {
        use webkit6::PolicyDecisionType::{NavigationAction, NewWindowAction};
        match kind {
            NavigationAction => {
                let uri = decision
                    .downcast_ref::<webkit6::NavigationPolicyDecision>()
                    .and_then(|nav| nav.navigation_action())
                    .and_then(|action| action.request())
                    .and_then(|request| request.uri());
                if uri.as_deref() == Some(ENTRY) && initial.replace(false) {
                    return false;
                }
            }
            NewWindowAction => {}
            _ => return false,
        }
        decision.ignore();
        true
    });
    let (events, mut display) = unbounded();
    let (actions, receive) = mpsc::channel();
    let closed = Arc::new(AtomicBool::new(false));
    let term = PageTerminal {
        events,
        actions: receive,
        closed: closed.clone(),
        question: 0,
        flags,
    };
    let spawned = std::thread::Builder::new()
        .name("remote-first-connect".into())
        .spawn(move || {
            let mut term = term;
            match run_worker(&mut term, settings, seed) {
                Ok(()) => {
                    let _ = term.events.unbounded_send(Display::Opened);
                }
                Err(text) if !term.cancelled() => term.send(Page::Error { text }),
                Err(_) => {}
            }
        });
    if let Err(error) = spawned {
        window.set_child(Some(&gtk4::Label::new(Some(&format!(
            "Could not start the SSH worker ({error})"
        )))));
        window.present();
        return;
    }
    content.register_script_message_handler("eitriRemoteProject", None);
    let close = window.downgrade();
    let from_page = actions.clone();
    content.connect_script_message_received(Some("eitriRemoteProject"), move |_, value| {
        if !value.is_string() {
            return;
        }
        let text = value.to_str();
        if text.len() > 32 * 1024 {
            return;
        }
        match Action::parse(&text) {
            Some(Action::Cancel) => {
                if let Some(window) = close.upgrade() {
                    window.close();
                }
            }
            Some(action) => {
                let _ = from_page.send(action);
            }
            None => {}
        }
    });
    let cancel = move || {
        closed.store(true, Ordering::Release);
        let _ = actions.send(Action::Cancel);
    };
    window.connect_close_request({
        let cancel = cancel.clone();
        move |_| {
            cancel();
            glib::Propagation::Proceed
        }
    });
    window.connect_destroy({
        let cancel = cancel.clone();
        move |_| cancel()
    });
    let parent = window.downgrade();
    view.connect_load_failed(move |_, _, _, _| {
        cancel();
        if let Some(window) = parent.upgrade() {
            window.set_child(Some(&gtk4::Label::new(Some("Could not load the SSH project chooser."))));
        }
        true
    });
    let close = window.downgrade();
    view.connect_web_process_terminated(move |_, _| {
        if let Some(window) = close.upgrade() {
            window.close();
        }
    });
    let hold = app.map(|app| app.hold());
    let window_ref = window.downgrade();
    let view_ref = view.downgrade();
    glib::spawn_future_local(async move {
        let _hold = hold;
        while let Ok(event) = display.recv().await {
            let (Some(window), Some(view)) = (window_ref.upgrade(), view_ref.upgrade()) else {
                continue;
            };
            match event {
                Display::Page(page) => send(&view, page),
                Display::Token(reply) => {
                    let token = WidgetExt::display(&window)
                        .app_launch_context()
                        .startup_notify_id(None::<&gio::AppInfo>, &[])
                        .map(|token| token.to_string());
                    let _ = reply.send(token);
                }
                Display::Opened => {
                    window.close();
                    opened();
                    break;
                }
            }
        }
    });
    window.set_child(Some(&view));
    view.load_uri(ENTRY);
    window.present();
    view.grab_focus();
}

fn send(view: &webkit6::WebView, page: Page) {
    let json = page.json().to_string();
    let args = HashMap::from([("payload", json.to_variant())]).to_variant();
    view.call_async_javascript_function(
        "window.eitriRemoteProject(JSON.parse(payload));",
        Some(&args),
        None,
        None,
        None::<&gio::Cancellable>,
        |_| {},
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_flag_on_a_never_opened_root_starts_the_flow() {
        let scratch = agent::state_dirs::test_workspace_dir("ssh-chooser-launch");
        std::fs::create_dir_all(&scratch).unwrap();
        let store = RemoteProjects::new(Some(scratch.as_os_str()), None);
        let args = ["--clean", "--ssh", "example-host", "--raise", "--", "/srv/project"].map(OsString::from);
        let dispatch = |run: &mut dyn FnMut((String, PathBuf), Vec<OsString>) -> glib::ExitCode| {
            crate::dispatch_first_ssh(
                &crate::linked_window::WindowStart::Full,
                &args,
                || RemoteProjects::new(Some(scratch.as_os_str()), None),
                run,
            )
        };
        assert_eq!(
            dispatch(&mut |seed, flags| {
                assert_eq!(seed, ("example-host".into(), "/srv/project".into()));
                assert_eq!(flags, ["--clean", "--raise"]);
                glib::ExitCode::SUCCESS
            }),
            Some(glib::ExitCode::SUCCESS)
        );
        let project = serde_json::from_value(serde_json::json!({
            "version": 1, "host": "example-host", "host_key_sha256": "SHA256:example",
            "root": "/srv/project", "helper_version": "0.4.0", "created_at": 0
        }))
        .unwrap();
        store.save(&project).unwrap();
        assert_eq!(
            dispatch(&mut |_, _| panic!("a recorded project bypasses first connect")),
            None
        );
        let exe = Path::new("/bin/eitri");
        let child = super::super::command(
            exe,
            &[],
            How::Remote {
                host: project.host,
                path: project.root,
            },
            None,
        );
        assert_eq!(
            child.get_args().collect::<Vec<_>>(),
            ["--ssh", "example-host", "--", "/srv/project"]
        );
        std::fs::remove_dir_all(scratch).unwrap();
    }
}
