//! A private, unparented WebView for Mermaid. It has no bridge and shares no context with a panel page.
use eitri_panel::{
    agent_panel::AgentPanelHandle,
    diagrams::{
        DiagramDone, DiagramOutcome, DiagramQueue, DiagramRenderer, DiagramStarted, RendererLoad, MAX_SVG_BYTES,
    },
};
use gtk4::gio::prelude::*;
use gtk4::glib::variant::ToVariant;
use gtk4::{gio, glib};
use std::{cell::RefCell, collections::HashMap, path::PathBuf, rc::Rc};
use webkit6::{
    prelude::*, NetworkProxyMode, NetworkProxySettings, NetworkSession, UserContentManager, WebContext, WebView,
};

const RENDERER_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'unsafe-inline'; img-src 'none'; font-src 'none'; media-src 'none'; connect-src 'none'; frame-src 'none'; object-src 'none'; form-action 'none'; base-uri 'none'";
const ENTRY: &str = "eitri-diagram://renderer/diagram-renderer.html";
const RENDER_BODY: &str = "return await window.eitriRender(source, vars);";

fn asset_for_uri(uri: &str) -> Option<(&'static str, &'static str)> {
    match uri.strip_prefix("eitri-diagram://renderer/")? {
        "diagram-renderer.html" => Some(("diagram-renderer.html", "text/html")),
        "diagram-renderer.js" => Some(("diagram-renderer.js", "text/javascript")),
        "mermaid.js" => Some(("mermaid.js", "text/javascript")),
        _ => None,
    }
}
fn serve(request: &webkit6::URISchemeRequest, site: &std::path::Path) {
    let asset = request.uri().and_then(|uri| asset_for_uri(&uri));
    let Some((name, mime)) = asset else {
        request.finish_error(&mut glib::Error::new(gio::IOErrorEnum::NotFound, "not found"));
        return;
    };
    let request = request.clone();
    // Fixed names only; async open and stream reads never probe or read the disk on GTK's thread.
    gio::File::for_path(site.join(name)).read_async(
        glib::Priority::DEFAULT,
        None::<&gio::Cancellable>,
        move |result| match result {
            Ok(stream) => crate::plugin_page::respond(&request, &stream, -1, mime),
            Err(mut error) => request.finish_error(&mut error),
        },
    );
}
struct Call {
    attempt: u64,
    source: String,
    vars: String,
    done: DiagramDone,
    started: bool,
    start: Option<DiagramStarted>,
}
#[derive(Default)]
struct State {
    view: Option<WebView>,
    serial: u64,
    call: Option<Call>,
    loads: RendererLoad,
}
struct Renderer {
    site: PathBuf,
    state: Rc<RefCell<State>>,
}

fn finish(state: &Rc<RefCell<State>>, attempt: u64, outcome: DiagramOutcome) -> bool {
    let done = {
        let mut state = state.borrow_mut();
        if state.call.as_ref().is_none_or(|call| call.attempt != attempt) {
            return false;
        }
        state.call.take().map(|call| call.done)
    };
    if let Some(done) = done {
        done(outcome);
        true
    } else {
        false
    }
}
// Discard the old view and its readiness before a completion can install another load.
fn end_view(state: &Rc<RefCell<State>>, attempt: Option<u64>, deliberate: bool) -> Option<WebView> {
    let mut state = state.borrow_mut();
    if attempt.is_some_and(|attempt| state.call.as_ref().is_none_or(|call| call.attempt != attempt)) {
        return None;
    }
    let view = state.view.take();
    if deliberate {
        state.loads.cancelled();
    } else {
        state.loads.stopped();
    }
    view
}
fn stopped_outcome(state: &Rc<RefCell<State>>) -> DiagramOutcome {
    if state.borrow().call.as_ref().is_some_and(|call| call.started) {
        DiagramOutcome::error("the renderer stopped", true)
    } else {
        DiagramOutcome::startup()
    }
}
// Only this view's page can release its held source, after installing the API and importing Mermaid.
fn page_message(state: &Rc<RefCell<State>>, view: &WebView, message: &webkit6::javascriptcore::Value) {
    if state.borrow().view.as_ref() != Some(view) || !message.is_string() {
        return;
    }
    match message.to_str().as_str() {
        "ready" => {
            if !state.borrow().loads.ready() {
                return;
            }
            execute(state);
        }
        "failed" => {
            let attempt = state.borrow().call.as_ref().map(|call| call.attempt);
            if let Some(view) = end_view(state, attempt, false) {
                view.terminate_web_process();
            }
            if let Some(attempt) = attempt {
                finish(state, attempt, DiagramOutcome::startup());
            }
        }
        _ => {}
    }
}

fn execute(state: &Rc<RefCell<State>>) {
    let (view, attempt, arguments) = {
        let mut state = state.borrow_mut();
        if !state.loads.is_ready() {
            return;
        }
        let Some(view) = state.view.clone() else {
            return;
        };
        let Some(call) = state.call.as_mut() else {
            return;
        };
        if call.started {
            return;
        }
        let Some(start) = call.start.take() else {
            return;
        };
        if !start(std::time::Instant::now()) {
            return;
        }
        call.started = true;
        let arguments: HashMap<&str, glib::Variant> =
            HashMap::from([("source", call.source.to_variant()), ("vars", call.vars.to_variant())]);
        (view, call.attempt, arguments.to_variant())
    };
    let weak = Rc::downgrade(state);
    view.call_async_javascript_function(
        RENDER_BODY,
        Some(&arguments),
        None,
        None,
        None::<&gio::Cancellable>,
        move |result| {
            let Some(state) = weak.upgrade() else {
                return;
            };
            let outcome = match result {
                Ok(value) if value.is_string() => {
                    let text = value.to_str();
                    if text.len() > MAX_SVG_BYTES * 6 + 4096 {
                        DiagramOutcome::error("the drawing is too large", false)
                    } else {
                        serde_json::from_str(&text)
                            .unwrap_or_else(|_| DiagramOutcome::error("the renderer stopped", true))
                    }
                }
                _ => DiagramOutcome::error("the renderer stopped", true),
            };
            if outcome.transient {
                if let Some(view) = end_view(&state, Some(attempt), false) {
                    view.terminate_web_process();
                }
            }
            finish(&state, attempt, outcome);
        },
    );
}
impl Renderer {
    fn end(&self, deliberate: bool) {
        let outcome = stopped_outcome(&self.state);
        let view = end_view(&self.state, None, deliberate);
        let call = self.state.borrow_mut().call.take();
        if let Some(view) = view {
            view.terminate_web_process();
        }
        if let Some(call) = call {
            (call.done)(outcome);
        }
    }

    fn build(&self) -> WebView {
        let context = WebContext::new();
        if let Some(security) = context.security_manager() {
            security.register_uri_scheme_as_secure("eitri-diagram");
            security.register_uri_scheme_as_cors_enabled("eitri-diagram");
        }
        let site = self.site.clone();
        context.register_uri_scheme("eitri-diagram", move |request| serve(request, &site));
        let session = NetworkSession::new_ephemeral();
        let proxy = NetworkProxySettings::new(Some("http://127.0.0.1:9"), &[]);
        session.set_proxy_settings(NetworkProxyMode::Custom, Some(&proxy));
        session.connect_download_started(|_, download| download.cancel());
        let content = UserContentManager::new();
        content.register_script_message_handler("eitriDiagram", None);
        let view = WebView::builder()
            .web_context(&context)
            .network_session(&session)
            .user_content_manager(&content)
            .default_content_security_policy(RENDERER_CSP)
            .build();
        if let Some(settings) = WebViewExt::settings(&view) {
            settings.set_javascript_can_open_windows_automatically(false);
            settings.set_enable_developer_extras(false);
        }
        view.connect_create(|_, _| None);
        let initial = Rc::new(std::cell::Cell::new(true));
        view.connect_decide_policy(move |_, decision, kind| match kind {
            webkit6::PolicyDecisionType::NavigationAction => {
                let uri = decision
                    .downcast_ref::<webkit6::NavigationPolicyDecision>()
                    .and_then(|d| d.navigation_action())
                    .and_then(|a| a.request())
                    .and_then(|r| r.uri());
                if uri.as_deref() == Some(ENTRY) && initial.replace(false) {
                    return false;
                }
                decision.ignore();
                true
            }
            webkit6::PolicyDecisionType::NewWindowAction => {
                decision.ignore();
                true
            }
            _ => false,
        });
        let weak = Rc::downgrade(&self.state);
        let weak_view = view.downgrade();
        content.connect_script_message_received(Some("eitriDiagram"), move |_, message| {
            if let (Some(state), Some(view)) = (weak.upgrade(), weak_view.upgrade()) {
                page_message(&state, &view, message);
            }
        });
        let weak = Rc::downgrade(&self.state);
        view.connect_web_process_terminated(move |view, _| {
            let Some(state) = weak.upgrade() else {
                return;
            };
            if state.borrow().view.as_ref() != Some(view) {
                return;
            }
            let attempt = state.borrow().call.as_ref().map(|call| call.attempt);
            let outcome = stopped_outcome(&state);
            let _view = end_view(&state, attempt, false);
            if let Some(attempt) = attempt {
                finish(&state, attempt, outcome);
            }
        });
        let weak = Rc::downgrade(&self.state);
        view.connect_load_failed(move |view, _, _, _| {
            let Some(state) = weak.upgrade() else {
                return true;
            };
            if state.borrow().view.as_ref() != Some(view) {
                return true;
            }
            let attempt = state.borrow().call.as_ref().map(|call| call.attempt);
            let _view = end_view(&state, attempt, false);
            if let Some(attempt) = attempt {
                finish(&state, attempt, DiagramOutcome::startup());
            }
            true
        });
        view
    }
}
impl DiagramRenderer for Renderer {
    fn render(
        &self,
        source: &str,
        vars_json: &str,
        startup_deadline: std::time::Instant,
        started: DiagramStarted,
        done: DiagramDone,
    ) {
        if let Err(error) = eitri_core::agent_bridge::validate_diagram_source(source) {
            done(DiagramOutcome::error(error, false));
            return;
        }
        let needs_view = self.state.borrow().view.is_none();
        let loads = self.state.borrow().loads.clone();
        let new_view = if needs_view {
            let installed = loads.build_and_install(
                startup_deadline,
                || self.build(),
                |view| self.state.borrow_mut().view = Some(view.clone()),
            );
            match installed {
                Ok(view) => Some(view),
                Err(outcome) => {
                    done(outcome);
                    return;
                }
            }
        } else {
            None
        };
        {
            let mut state = self.state.borrow_mut();
            state.serial += 1;
            state.call = Some(Call {
                attempt: state.serial,
                source: source.into(),
                vars: vars_json.into(),
                done,
                started: false,
                start: Some(started),
            });
        }
        if let Some(view) = new_view {
            view.load_uri(ENTRY);
        } else {
            execute(&self.state);
        }
    }
    fn stop(&self) {
        self.end(false);
    }
    fn cancel(&self) {
        self.end(true);
    }
    fn drop_idle(&self) {
        self.cancel();
    }
}

#[derive(Default)]
struct Shared {
    resolving: bool,
    resolved: bool,
    waiting: Vec<AgentPanelHandle>,
    queue: Option<Rc<RefCell<DiagramQueue>>>,
}
thread_local! { static SHARED: RefCell<Shared> = RefCell::new(Shared::default()); }

fn resolve_assets() -> Option<PathBuf> {
    use eitri_core::plugin::search::{find_plugin, SearchEnv};
    let found = find_plugin("markdown", &SearchEnv::from_process()).ok()?;
    let site = found.dir.join("web/dist");
    for name in ["diagram-renderer.html", "diagram-renderer.js", "mermaid.js"] {
        if !site.join(name).is_file() {
            return None;
        }
    }
    std::fs::canonicalize(site).ok()
}
/// Resolve once, off the main loop. File removal after resolution fails that render through the scheme request.
pub(crate) fn install(panel: &AgentPanelHandle) {
    let start = SHARED.with(|shared| {
        let mut shared = shared.borrow_mut();
        if shared.resolved {
            let queue = shared.queue.clone();
            drop(shared);
            if let Some(queue) = queue {
                panel.set_diagram_renderer(queue);
            }
            return false;
        }
        shared.waiting.push(panel.clone());
        if shared.resolving {
            false
        } else {
            shared.resolving = true;
            true
        }
    });
    if !start {
        return;
    }
    glib::spawn_future_local(async {
        let site = gio::spawn_blocking(resolve_assets).await.ok().flatten();
        let queue = site.map(|site| {
            Rc::new(RefCell::new(DiagramQueue::new(Rc::new(Renderer {
                site,
                state: Rc::default(),
            }))))
        });
        let waiting = SHARED.with(|shared| {
            let mut shared = shared.borrow_mut();
            shared.resolved = true;
            shared.queue = queue.clone();
            std::mem::take(&mut shared.waiting)
        });
        if let Some(queue) = queue {
            for panel in waiting {
                panel.set_diagram_renderer(queue.clone());
            }
        }
    });
}

#[cfg(test)]
mod tests;
