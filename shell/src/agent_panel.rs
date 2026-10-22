//! The agent panel's WebKitGTK half: the view and its content-security and network policy, the page and
//! process implementations the panel's logic drives (`eitri_panel::panel_page`), the pump's GLib timer, the
//! application hold that outlives a closing window and the reload action. The panel's logic -- tabs, sessions,
//! the trust gate, turn review, the page protocol -- is `eitri_panel::agent_panel`.

use eitri_core::agent_backend::BackendKind;
use eitri_panel::agent_panel::{PanelInit, ShutdownWatch, CLOSE_WATCH_POLL};
use eitri_panel::panel_document::PanelDocument;
use eitri_panel::panel_page::{PageSurface, PanelHost, PanelPage};
use eitri_panel::panel_wake::{NextTick, TickSchedule};
use gtk4::prelude::*;
use gtk4::Application;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;
use webkit6::prelude::*;
use webkit6::{NetworkProxyMode, NetworkProxySettings, NetworkSession, UserContentManager, WebView};

// What the rest of `shell` reaches through `crate::agent_panel::`.
pub(crate) use eitri_panel::agent_panel::{AgentPanelHandle, HintInbound, ViewId};

/// The embedded, single-file `agent-ui/web` production build -- `shell/build.rs`
/// guarantees this file exists and is current by the time `shell` itself compiles.
const AGENT_UI_HTML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../agent-ui/web/dist/index.html"));

/// The bundle with its content security policy, computed once on first use.
static PANEL_DOCUMENT: std::sync::LazyLock<PanelDocument> =
    std::sync::LazyLock::new(|| PanelDocument::new(AGENT_UI_HTML));

/// Keeps the process alive until `watch` is done, WITHOUT anything waiting for it.
///
/// **The mechanism, because the previous two versions of this code got its premise wrong.** A
/// window close that has teardown left to do has two bad options if it insists on a wait: return
/// from `connect_close_request` immediately and let the process exit with `claude`, the sidecar and
/// `node` still running, or block the GTK main thread -- which keeps the window mapped and
/// unresponsive for as long as the wait lasts, because `window.connect_close_request` runs ON that
/// thread and nothing is destroyed until it returns. The first round of this branch moved the WORK
/// to a worker and kept a 3s wait on the main thread; the second raised that wait to 15s while
/// declaring the main loop no longer waited. It did. That is a 15-second frozen window, not a
/// bound.
///
/// There is a third option and it is the one GLib is built for: `g_application_hold`
/// (`gio::prelude::ApplicationExtManual::hold`) raises the application's use count, so
/// `Application::run` keeps iterating the main loop after the last window is gone instead of
/// returning. **This function's caller** returns at once, the main loop keeps running, and this
/// function's `CLOSE_WATCH_POLL` tick drops the guard once every watched worker reported, died or
/// passed its backstop. Releasing it is what lets `run` return and the process exit -- once nothing
/// else holds the application, that is. One hold covers every entry, a teardown that a finished
/// connect starts included, so the use count cannot touch zero between the two. Nothing anywhere
/// blocks.
///
/// Verified rather than declared, in two places, because this doc is the third attempt at it:
/// `a_hold_keeps_the_application_running_after_its_last_window` (this module, `#[ignore]`d --
/// `Application::run` wants the default main context and must not race the rest of the suite for
/// it) drives a real `gio::Application` with no window at all and shows `run` returning only after
/// the guard drops; `ShutdownWatch`'s own tests pin what each tick decides.
pub(crate) fn hold_until_done(app: &Application, watch: ShutdownWatch) {
    if watch.is_empty() {
        return;
    }
    // Taken BEFORE this function returns, i.e. before `connect_close_request` returns and GTK
    // destroys the last window -- which is the moment the application would otherwise release
    // itself and `run` would return.
    let mut hold = Some(app.hold());
    let mut watch = watch;
    gtk4::glib::timeout_add_local(CLOSE_WATCH_POLL, move || {
        if watch.step(std::time::Instant::now()) {
            return gtk4::glib::ControlFlow::Continue;
        }
        // Dropping the guard IS the release, and it has to happen here rather than be left to the
        // closure's captures falling when GLib frees the removed source: the release is the thing
        // that lets `Application::run` return, so leaving it to the source's own teardown would
        // make process exit depend on GLib freeing a closure whose last act was to ask for exit.
        hold.take();
        gtk4::glib::ControlFlow::Break
    });
}

/// The panel's page in WebKitGTK.
struct WebKitPage(WebView);

impl PageSurface for WebKitPage {
    fn send(&self, envelope_json: &str) {
        // The grid probe judges what an idle page was sent from this line: a page that is not live can still show
        // an empty document while data reaches it, so the page's own inbound line cannot vouch for it. The page's
        // pointer is the identity the probe logs for each host.
        #[cfg(feature = "probe")]
        eprintln!(
            "[agent_panel] page {:#x}: sent {}",
            gtk4::glib::object::ObjectType::as_ptr(&self.0) as usize,
            serde_json::from_str::<serde_json::Value>(envelope_json)
                .ok()
                .and_then(|v| v.get("kind").and_then(|k| k.as_str().map(str::to_owned)))
                .unwrap_or_else(|| "unknown".to_owned())
        );
        let script = format!(
            "window.__eitriDispatch && window.__eitriDispatch({});",
            serde_json::to_string(envelope_json).unwrap_or_default()
        );
        self.0
            .evaluate_javascript(&script, None, None, None::<&gtk4::gio::Cancellable>, |result| {
                if let Err(e) = result {
                    eprintln!("[agent_panel] dispatch failed: {e}");
                }
            });
    }

    fn is_visible(&self) -> bool {
        self.0.is_mapped()
    }

    fn load_document(&self, html: &str, base_uri: &str) {
        self.0.load_html(html, Some(base_uri));
    }

    /// The view's own background, which shows before the web process has painted anything. WebKit
    /// defaults it to opaque white.
    fn set_background(&self, [r, g, b]: [u8; 3]) {
        self.0.set_background_color(&gtk4::gdk::RGBA::new(
            f32::from(r) / 255.0,
            f32::from(g) / 255.0,
            f32::from(b) / 255.0,
            1.0,
        ));
    }
}

/// The process side: links go to the desktop's handler through GTK.
struct GtkPanelHost;

impl PanelHost for GtkPanelHost {
    fn open_external(&self, url: &str) {
        let shown = url.to_owned();
        gtk4::UriLauncher::new(url).launch(None::<&gtk4::Window>, None::<&gtk4::gio::Cancellable>, move |result| {
            if let Err(error) = result {
                eprintln!("[agent_panel] opening {shown}: {error}");
            }
        });
    }
}

/// `backend_kind` is `main()`'s own choice: `BackendKind::from_env`
/// is called there, before any window exists, so `--legacy`/`EITRI_AGENT_BACKEND=legacy` on a
/// release build can exit 1 naming the reason before touching GTK at all. This function no longer
/// makes that choice itself -- it only prints it.
///
/// `unavailable`: `Some(notice)` when WebKit's sandbox cannot start in this process
/// (`webkit_sandbox::Decision::Unavailable`). Then no `WebView` -- nor anything else of WebKit's --
/// is built: the panel's place is `webkit_sandbox::notice_widget(notice)`, and the handle carries the
/// same state with no page (`AgentPanelHandle::with_pages`'s own doc), no pump, no supervisor client.
/// Otherwise the handle is built first and the base view's page built for it by [`build_chat_page`].
pub(crate) fn build_agent_panel(
    project_dir: PathBuf,
    editor_context: eitri_core::editor_context::ContextSource,
    scratch: Option<eitri_core::scratch::ScratchDir>,
    backend_kind: BackendKind,
    unavailable: Option<&str>,
    related: Option<&WebView>,
) -> (gtk4::Widget, AgentPanelHandle) {
    let init = PanelInit {
        project_dir,
        editor_context,
        scratch,
        backend_kind,
        document: &PANEL_DOCUMENT,
    };
    if let Some(notice) = unavailable {
        let handle = AgentPanelHandle::new(init, false, Rc::new(GtkPanelHost));
        return (crate::webkit_sandbox::notice_widget(notice), handle);
    }
    let handle = AgentPanelHandle::new(init, true, Rc::new(GtkPanelHost));
    let (webview, _page) = build_chat_page(&handle, ViewId::BASE, related);

    // Started once, at construction, rather than when a session starts: it is also what collects a
    // finished background connect. Previously it was started inside the handler that constructs the
    // backend, which meant a second start attempt after a failure installed a SECOND timer on the
    // same state -- every later event would then be dispatched twice. One timer serves every page.
    start_pump_timer(handle.clone());

    (webview.upcast(), handle)
}

/// The one way a chat page is built: its own `UserContentManager`, the CSP floor, the ephemeral
/// network session with the proxy backstop (with `related`, the related page's process and session,
/// which WebKit shares, so no session of its own is set), the navigation and form-submission lock, the link handler, and the `eitriAgent` handler,
/// which carries `view` -- the only place a message's page is decided. Attaches the page to `handle` as
/// `view` and loads its document. The page's `PanelPage` comes back with it, so a page that goes idle
/// and is reused under the same view is attached again as the same page.
///
/// Every chat page of the window -- the base view's and each extra view's -- comes from here, so a page
/// cannot be built without the policy or with a handler that answers for another view: the view is
/// moved into the handler's closure when the page is built and is never read from page content.
pub(crate) fn build_chat_page(
    handle: &AgentPanelHandle,
    view: ViewId,
    related: Option<&WebView>,
) -> (WebView, Rc<PanelPage>) {
    // Its own content manager: a manager shared between two pages would deliver one page's messages to
    // the other page's handler too, and the handler is what says which view a message came from.
    let content_manager = UserContentManager::new();
    // A CSP floor the page cannot loosen, plus a network backstop behind it -- see
    // `panel_content_security_policy` and `PANEL_NETWORK_PROXY_URI`'s doc comments.
    let builder = WebView::builder()
        .user_content_manager(&content_manager)
        .default_content_security_policy(panel_content_security_policy());
    let webview = match related {
        // The first page gets the ephemeral session pointed at the unreachable proxy.
        None => {
            let network_session = NetworkSession::new_ephemeral();
            network_session.set_proxy_settings(
                NetworkProxyMode::Custom,
                Some(&NetworkProxySettings::new(Some(PANEL_NETWORK_PROXY_URI), &[])),
            );
            builder.network_session(&network_session).build()
        }
        // `related-view` is construct-only: it can be given here and nowhere later. The page then shares
        // the related page's web process and its network session, so the proxy backstop with it. WebKit
        // ignores a session given together with a related view (and logs a critical for it), so none is.
        Some(related) => builder.related_view(related).build(),
    };
    webview.set_hexpand(true);
    webview.set_vexpand(true);
    let page = Rc::new(PanelPage::new(Rc::new(WebKitPage(webview.clone()))));
    handle.attach_page(view, page.clone());

    // Security: never let this WebView itself navigate away from its one embedded document.
    // Even sanitized markdown can legitimately contain an external link (a click), and a script
    // that got past sanitization anyway (defense-in-depth) might try to redirect the page --
    // either way, navigating this WebView to a remote origin would hand that origin the same
    // UserContentManager and therefore the same `eitriAgent` bridge this panel uses to relay
    // permission decisions. Hand link clicks to the panel (`link_clicked`, which opens a web page or
    // mail draft in the system's handler) and keep this WebView on its embedded document for the
    // panel's whole lifetime.
    {
        let handle = handle.clone();
        webview.connect_decide_policy(move |_webview, decision, decision_type| {
            if decision_type == webkit6::PolicyDecisionType::NavigationAction {
                if let Some(nav_decision) = decision.downcast_ref::<webkit6::NavigationPolicyDecision>() {
                    if let Some(action) = nav_decision.navigation_action() {
                        match action.navigation_type() {
                            webkit6::NavigationType::LinkClicked => {
                                let uri = action.request().and_then(|r| r.uri());
                                handle.link_clicked(view, uri.as_deref());
                                nav_decision.ignore();
                                return true;
                            }
                            webkit6::NavigationType::FormSubmitted | webkit6::NavigationType::FormResubmitted => {
                                // DOMPurify's default config deliberately keeps <form action="...">
                                // intact -- a sanitized-but-attacker-styled form submit button is a
                                // real, click-required (not zero-click) way to navigate this panel to
                                // a remote origin that would inherit the same bridge, and the link-click
                                // guard above does not cover it. There is no legitimate reason this panel's own
                                // embedded document would ever submit a form anywhere.
                                nav_decision.ignore();
                                return true;
                            }
                            _ => {}
                        }
                    }
                }
            }
            false
        });
    }

    content_manager.register_script_message_handler("eitriAgent", None);
    {
        let handle = handle.clone();
        // Weak: the page owns this handler, so a strong reference here would keep the page alive forever.
        let page_widget = webview.downgrade();
        // `view` is moved in here, once: whatever the page posts, it is this page's view's message.
        content_manager.connect_script_message_received(Some("eitriAgent"), move |_manager, js_value| {
            let raw = js_value.to_str();
            // A copy's text for the primary selection: only a widget reaches its display's selection. One of
            // this kind that is not well formed is dropped here too, since the panel would log its text.
            if eitri_core::agent_bridge::is_primary_copy(&raw) {
                if let Some(text) = eitri_core::agent_bridge::primary_copy(&raw) {
                    if let Some(widget) = page_widget.upgrade() {
                        widget.primary_clipboard().set_text(&text);
                    }
                }
                return;
            }
            handle.page_message(view, &raw);
        });
    }

    handle.load_document(view);

    // Back on screen: whatever finished in the tab the page shows while it was away has been seen
    // (the tray's `agent •`).
    {
        let handle = handle.clone();
        webview.connect_map(move |_| handle.page_shown(view));
    }

    // Recover this WebView's own crash automatically (guarded) instead of leaving the chat
    // blank until the user finds `\u{21bb}`/`prefix r` by hand. See `AgentPanelHandle::page_gone`'s
    // own doc. An end the host asked for itself is no crash.
    {
        let handle = handle.clone();
        webview.connect_web_process_terminated(move |_webview, reason| {
            if reason == webkit6::WebProcessTerminationReason::TerminatedByApi {
                return;
            }
            handle.page_gone(view, &format!("{reason:?}"));
        });
    }

    (webview, page)
}

/// Runs the panel's tick when it says: a watch on its wake's descriptor, and one one-shot timer armed for
/// the earliest moment anything asked for (`eitri_panel::panel_wake`). An idle panel is not woken at all.
struct PumpTimer {
    handle: AgentPanelHandle,
    schedule: RefCell<TickSchedule>,
    timer: RefCell<Option<gtk4::glib::SourceId>>,
    watch: RefCell<Option<gtk4::glib::SourceId>>,
    /// Each timer and the watch hold the pump through this, so it lives as long as something can run it.
    me: std::rc::Weak<PumpTimer>,
}

impl PumpTimer {
    /// The wake's descriptor is readable: take it and arm the timer if the tick is due sooner.
    fn on_wake(&self) -> gtk4::glib::ControlFlow {
        if let Some(due) = self.handle.take_wake(Instant::now()) {
            let armed = self.schedule.borrow_mut().on_wake(due);
            if let Some(at) = armed {
                self.arm(at);
            }
        }
        gtk4::glib::ControlFlow::Continue
    }

    /// The timer fired: run the tick and arm what it asks for next.
    fn on_timer(&self) {
        *self.timer.borrow_mut() = None;
        self.schedule.borrow_mut().fired();
        let next = self.handle.run_tick(Instant::now());
        if next == NextTick::Stop {
            if let Some(watch) = self.watch.borrow_mut().take() {
                watch.remove();
            }
            return;
        }
        let armed = self.schedule.borrow_mut().after_tick(next);
        if let Some(at) = armed {
            self.arm(at);
        }
    }

    /// Replaces the armed timer with one for `at`.
    fn arm(&self, at: Instant) {
        if let Some(old) = self.timer.borrow_mut().take() {
            old.remove();
        }
        let Some(pump) = self.me.upgrade() else { return };
        let id = gtk4::glib::timeout_add_local_once(timer_wait(at, Instant::now()), move || pump.on_timer());
        *self.timer.borrow_mut() = Some(id);
    }
}

/// How long a GLib timer waits to fire at `at`: rounded UP to a whole millisecond, since GLib counts in
/// milliseconds and drops the rest, and a timer that fires before a deadline costs a tick that finds nothing
/// due and has to be armed again.
fn timer_wait(at: Instant, now: Instant) -> std::time::Duration {
    let nanos = at.saturating_duration_since(now).as_nanos();
    std::time::Duration::from_millis(u64::try_from(nanos.div_ceil(1_000_000)).unwrap_or(u64::MAX))
}

fn start_pump_timer(handle: AgentPanelHandle) {
    let Some(fd) = handle.wake_fd() else {
        // No wake (no page, or the socket pair could not be made): the tick runs on its steady cadence.
        gtk4::glib::timeout_add_local(AgentPanelHandle::TICK, move || {
            if handle.tick() {
                gtk4::glib::ControlFlow::Continue
            } else {
                gtk4::glib::ControlFlow::Break
            }
        });
        return;
    };
    let pump = std::rc::Rc::new_cyclic(|me| PumpTimer {
        handle,
        schedule: RefCell::new(TickSchedule::new()),
        timer: RefCell::new(None),
        watch: RefCell::new(None),
        me: me.clone(),
    });
    let watched = {
        let pump = pump.clone();
        neovide_editor::fd_watch::add_local(
            fd,
            gtk4::glib::IOCondition::IN | gtk4::glib::IOCondition::ERR | gtk4::glib::IOCondition::HUP,
            gtk4::glib::ffi::G_PRIORITY_DEFAULT,
            move |_| pump.on_wake(),
        )
    };
    match watched {
        Ok(watch) => *pump.watch.borrow_mut() = Some(watch),
        Err(neovide_editor::fd_watch::NotOwner) => {
            // Not on the main context's own thread, where nothing could watch the descriptor: the tick
            // runs on its steady cadence, as it always did.
            let handle = pump.handle.clone();
            gtk4::glib::timeout_add_local(AgentPanelHandle::TICK, move || {
                if handle.tick() {
                    gtk4::glib::ControlFlow::Continue
                } else {
                    gtk4::glib::ControlFlow::Break
                }
            });
            return;
        }
    }
    // The first tick at once: whatever the window set up before the loop runs is sent by it.
    pump.schedule.borrow_mut().on_wake(Instant::now());
    pump.arm(Instant::now());
}

/// Raises the window when `eitri-supervisor` asks: `on_activate` runs for each request. A watch on the
/// supervisor's socket once the panel has one, a look every 200 ms only while one is still being started and
/// connected, and nothing at all when there is none -- a window with no supervisor used to look five times a
/// second for its whole life.
pub(crate) fn watch_supervisor(handle: AgentPanelHandle, on_activate: Rc<dyn Fn()>) {
    use eitri_panel::agent_panel::SupervisorLink;
    match handle.supervisor_link() {
        SupervisorLink::Absent => {}
        SupervisorLink::Connecting => {
            gtk4::glib::timeout_add_local_once(SUPERVISOR_CONNECT_LOOK, move || watch_supervisor(handle, on_activate));
        }
        SupervisorLink::Socket(fd) => {
            let watched = {
                let handle = handle.clone();
                let on_activate = on_activate.clone();
                neovide_editor::fd_watch::add_local(
                    fd,
                    gtk4::glib::IOCondition::IN | gtk4::glib::IOCondition::ERR | gtk4::glib::IOCondition::HUP,
                    gtk4::glib::ffi::G_PRIORITY_DEFAULT,
                    move |_| {
                        #[cfg(test)]
                        SUPERVISOR_LOOKS.with(|looks| looks.set(looks.get() + 1));
                        if handle.poll_activate() {
                            on_activate();
                        }
                        // A connection that ended reads as readable for good: let go of it.
                        match handle.supervisor_link() {
                            SupervisorLink::Socket(_) => gtk4::glib::ControlFlow::Continue,
                            SupervisorLink::Connecting | SupervisorLink::Absent => gtk4::glib::ControlFlow::Break,
                        }
                    },
                )
            };
            if watched.is_err() {
                // Not on the main context's own thread, where nothing could watch the socket: look on the
                // timer, as the window always did.
                gtk4::glib::timeout_add_local(SUPERVISOR_CONNECT_LOOK, move || {
                    if handle.poll_activate() {
                        on_activate();
                    }
                    gtk4::glib::ControlFlow::Continue
                });
            }
        }
    }
}

/// How often a window looks at a supervisor connection still being made, and at one it cannot watch.
const SUPERVISOR_CONNECT_LOOK: std::time::Duration = std::time::Duration::from_millis(200);

#[cfg(test)]
thread_local! {
    /// How often the supervisor watch ran, for the test that it is not woken while nothing arrives.
    static SUPERVISOR_LOOKS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// The panel's Content-Security-Policy: a floor the page cannot loosen, applied two ways -- as the
/// `WebView`'s own `default-content-security-policy` (`build_chat_page`) and as the first element
/// of `<head>` (`PanelDocument::themed`), so it is in force before the single-file build's own inlined
/// `<script>`/`<style>` run. Model output rendered in the panel is untrusted; this is defence in
/// depth behind the markdown sanitizer, for whatever gets past it. Both copies carry the same
/// string: the browser enforces both, so a script allowed by only one would still be blocked.
///
/// Scripts are pinned by the SHA-256 of the bundle's own inline script(s), with no `'unsafe-inline'`
/// for scripts, so an injected inline handler, `javascript:` URL or `<script>` does not run -- and
/// script in this page can answer permission cards, so that matters. Styles keep `'unsafe-inline'`:
/// the theme block `PanelDocument::themed` inserts and the bundle's own `<style>` need it, and a style
/// cannot act on the host. The hash is computed from the embedded `AGENT_UI_HTML` at first use
/// (`PanelDocument::new`, `eitri_panel::panel_csp`), not carried in from the build script: `include_str!` follows the built
/// file while the build script reruns only when its inputs change, so the two could describe
/// different bundles, and the panel would silently never start. A document the extractor refuses
/// gets `script-src 'none'` and a stderr line, never inline script by keyword.
///
/// Checked what the panel legitimately loads before picking each other directive: `agent-ui/web/src`
/// has no `fetch`/`XMLHttpRequest`/`WebSocket`/`eval`/`new Function`, and `index.css` has no
/// `@font-face`/`url(` -- every font is a locally-installed one named by `font-family`, resolved by
/// WebKit's own font matching, never loaded as a resource. So `connect-src 'none'` and `media-src
/// 'none'` cost the panel nothing today. `img-src data:`/`font-src data:` stay open only for a
/// future inline `data:` use, never a remote load. `frame-src`/`object-src 'none'` and
/// `form-action`/`base-uri 'none'` match `connect_decide_policy`'s existing navigation guard, which
/// already refuses any `FormSubmitted`/`LinkClicked` navigation of this WebView itself.
///
/// `evaluate_javascript` from Rust (`PageSurface::send`) bypasses CSP by design -- it is this
/// panel's own push channel, not page-originated content -- so the dispatch path this panel relies
/// on is unaffected.
fn panel_content_security_policy() -> &'static str {
    PANEL_DOCUMENT.csp()
}

/// A backstop for the CSP: an ephemeral
/// `NetworkSession` (per `WebKitWebsiteDataManager:is-ephemeral`, all data -- cookies, cache, storage
/// -- is held in memory for the session's lifetime and never written to disk; it is non-persistent,
/// not absent) whose proxy is the "discard" port on loopback, so any request that gets past the CSP
/// above and the sanitizer dies at a closed port. **This is a backstop, not the boundary** --
/// the CSP is.
const PANEL_NETWORK_PROXY_URI: &str = "http://127.0.0.1:9";

/// Installs `app.reload-agent-panel`, which the top bar's `↻` and `prefix r`
/// fire, for the panel `handle` owns.
///
/// The affordance deliberately lives OUTSIDE the WebView. A reload button drawn by the panel's own
/// page would be drawn by the very thing that has stopped responding -- missing exactly when it is
/// needed -- and until now a wedged panel could only be recovered by closing the window, which also
/// takes the editor, the nvim child and the agent session with it.
///
/// It binds no accelerator: `Ctrl+Shift+R` was removed with every other `Ctrl+Shift` chord.
///
/// **Not verified, and stated here so nobody reads a constraint into the call site:** `main.rs`
/// calls this before building the top bar, whose `⟳` button is a `GtkActionable` pointed at this
/// action. An earlier version of this comment claimed the order was load-bearing -- that a button
/// pointed at an action that does not exist yet renders permanently insensitive. That claim was
/// never tested here, and upstream GTK4's own `gtk/gtkactionhelper.c` appears to contradict it:
/// `GtkActionHelper` implements `GtkActionObserver` (`action_added`, `action_removed`,
/// `action_enabled_changed`) and calls `gtk_widget_set_sensitive` when an action appears. That is
/// read from the GNOME repository's `main`, not from the GTK actually installed here, and it is not
/// the same thing as having watched a late-registered action light the button up. So the ordering
/// is most likely free, on documentary evidence rather than observation. It is kept as it is because it is the conservative one, not
/// because anything in this repository has demonstrated that the other order breaks. Whether the
/// button is in fact sensitive on screen still needs an interactive check.
///
/// **Known limitation while `shell` can still run two windows in one process:** the action is
/// process-global (`app.add_action`) and this handle is per-window, so a second `activate` running
/// `build_ui` again silently REPLACES the first window's action -- after which window 1's `⟳` and
/// Ctrl+Shift+R reload window 2's panel. `ApplicationFlags::NON_UNIQUE`
/// makes that unreachable by giving each launch its own process, so this only matters if a second
/// window is ever hosted in one process again.
///
/// `keys_view` names the chat view the reload is for when it runs: the one with the keys, else the base
/// view's, so `prefix r` in an extra view reloads that view's page and never another's.
pub(crate) fn install_reload_action(app: &Application, handle: &AgentPanelHandle, keys_view: Rc<dyn Fn() -> ViewId>) {
    let action = gtk4::gio::SimpleAction::new("reload-agent-panel", None);
    // Cloned because the caller still needs its own handle afterwards -- `connect_close_request`
    // takes it by move.
    let handle = handle.clone();
    action.connect_activate(move |_, _| handle.reload_document_by_hand(keys_view()));
    app.add_action(&action);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_panel_puts_envelopes_in_the_page_only_through_the_funnel() {
        // `PanelPage::send` and `AgentPanelHandle::dispatch` hand over what the cadence holds
        // before they send; `set_theme`'s own send is the other direct one and carries no stream
        // state. Any further direct send would let an envelope overtake a held one -- a snapshot
        // ahead of the events before it draws them twice. The one script that reaches the page is
        // the WebKit surface's own.
        let panel = panel_code();
        assert_eq!(
            panel.matches(".evaluate_javascript(").count(),
            0,
            "the panel's logic sends to the page outside the funnel"
        );
        let flat: String = panel.split_whitespace().collect();
        assert!(
            flat.contains("page.flush();page.send_direct("),
            "AgentPanelHandle::dispatch hands over what is held first"
        );
        assert_eq!(
            panel.matches(".send_direct(").count(),
            2,
            "a direct send past the pacer other than `set_theme` and `dispatch`"
        );
        assert!(
            !panel.contains("pacer_for("),
            "the pacer is the page's own, never looked up by the page's view"
        );
        let shell = webkit_half();
        assert_eq!(
            shell.matches(".evaluate_javascript(").count(),
            1,
            "the WebKit half sends to the page outside `PageSurface::send`"
        );
        let surface = impl_blocks(&shell, "implPageSurfaceforWebKitPage");
        assert_eq!(surface.len(), 1);
        assert_eq!(surface[0].matches(".evaluate_javascript(").count(), 1);
        assert_eq!(shell.matches(".send_direct(").count(), 0);
    }

    #[test]
    fn only_the_pumps_stream_payload_is_ever_held() {
        assert_eq!(
            panel_code().matches(".send_stream(").count(),
            1,
            "`send_stream` is the pump's alone; everything else is never delayed"
        );
        assert_eq!(webkit_half().matches(".send_stream(").count(), 0);
    }

    /// The code (comments and literals blanked, no test module) of the panel's logic: the one file under
    /// `shell/src` and `panel/src` that defines `handle_inbound_message`.
    fn panel_code() -> String {
        use eitri_core::source_scan as scan;
        let sources = scan::rust_sources(&["shell/src", "panel/src"]);
        let code = scan::code_only(&scan::file_with(&sources, "fn handle_inbound_message(").text);
        scan::without(&code, &scan::modules(&code, "tests"))
    }

    /// The code (no test module) of the WebKit half: the one file under `shell/src` that defines `WebKitPage`.
    fn webkit_half() -> String {
        use eitri_core::source_scan as scan;
        let sources = scan::rust_sources(&["shell/src"]);
        let code = scan::code_only(&scan::file_with(&sources, "struct WebKitPage").text);
        scan::without(&code, &scan::modules(&code, "tests"))
    }

    /// The braced body of every `impl` whose header, with the whitespace removed, is `header_flat`.
    fn impl_blocks<'a>(code: &'a str, header_flat: &str) -> Vec<&'a str> {
        use eitri_core::source_scan as scan;
        code.match_indices("impl")
            .filter(|(at, _)| {
                !code[..*at]
                    .chars()
                    .next_back()
                    .is_some_and(|c| c.is_alphanumeric() || c == '_')
            })
            .filter_map(|(at, _)| {
                let open = at + code[at..].find('{')?;
                let flat: String = code[at..open].split_whitespace().collect();
                (flat == header_flat).then(|| scan::braced(code, open))
            })
            .collect()
    }

    /// The one place a document is handed to the WebKit view hands it the base it was given: the panel's
    /// loads all name `PANEL_BASE_URI` (`eitri_panel::agent_panel`'s own test), so a `load_html` that
    /// substituted another base would undo them.
    #[test]
    fn the_webkit_page_hands_load_html_the_base_it_is_given() {
        use eitri_core::source_scan as scan;
        let code = webkit_half();
        assert_eq!(
            code.matches(".load_html(").count(),
            1,
            "a document load around PageSurface"
        );
        let surface = impl_blocks(&code, "implPageSurfaceforWebKitPage");
        assert_eq!(surface.len(), 1);
        let load_document = scan::functions(surface[0], "load_document");
        assert_eq!(load_document.len(), 1);
        let flat: String = load_document[0].split_whitespace().collect();
        assert!(flat.contains(".load_html(html,Some(base_uri))"), "{flat}");
    }

    /// The premise the whole close path now rests on: a `gio` application hold keeps
    /// `Application::run` iterating the main loop with no window on screen, and dropping the guard
    /// is what lets `run` return.
    ///
    /// **`#[ignore]`d, and the reason is not flakiness.** `g_application_run` iterates the DEFAULT
    /// main context; two of these in two test threads would fight over it, and `cargo test` runs
    /// this crate's tests in parallel. Run it alone:
    /// `cargo test -p shell a_hold_keeps_the_application_running -- --ignored --exact ...`.
    ///
    /// It drives a plain `gio::Application` rather than a `gtk4::Application` deliberately -- the
    /// latter needs a display, which is what stops `shell` from testing any of this -- and
    /// `gtk4::Application` IS a `gio::Application`, which is where `hold` is defined and what
    /// `hold_until_done` calls it through.
    ///
    /// The first half is a control: without the hold, `run` returns as soon as `activate` does. Its
    /// point is that the second half's 300ms is the hold's doing and not the loop's own latency.
    #[test]
    #[ignore = "runs a real gio main loop on the default main context; must not race the rest of the suite"]
    fn a_hold_keeps_the_application_running_after_its_last_window() {
        use gtk4::gio;

        let control = gio::Application::new(None, gio::ApplicationFlags::empty());
        control.connect_activate(|_| {});
        let started = std::time::Instant::now();
        control.run_with_args(&["shell-hold-control"]);
        let without_a_hold = started.elapsed();
        assert!(
            without_a_hold < std::time::Duration::from_millis(100),
            "an application with nothing holding it must not stay in its main loop at all; it took {without_a_hold:?}"
        );

        let held = gio::Application::new(None, gio::ApplicationFlags::empty());
        held.connect_activate(|app| {
            let mut hold = Some(app.hold());
            // The shape `hold_until_done` uses: a main-loop source, and a release from
            // inside it once its work is done.
            gtk4::glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
                if started_long_enough_ago() {
                    hold.take();
                    return gtk4::glib::ControlFlow::Break;
                }
                gtk4::glib::ControlFlow::Continue
            });
        });
        let started = std::time::Instant::now();
        RELEASE_AT.with(|cell| cell.set(Some(started + std::time::Duration::from_millis(300))));
        held.run_with_args(&["shell-hold"]);
        let with_a_hold = started.elapsed();
        assert!(
            with_a_hold >= std::time::Duration::from_millis(290),
            "the hold did not keep the loop running: run() returned after {with_a_hold:?}"
        );
        assert!(
            with_a_hold < std::time::Duration::from_secs(5),
            "releasing the hold did not let run() return: it took {with_a_hold:?}"
        );
    }

    fn started_long_enough_ago() -> bool {
        RELEASE_AT.with(|cell| cell.get().is_some_and(|at| std::time::Instant::now() >= at))
    }

    /// The document this panel loads -- at startup, and again on every `reload_document()` -- has to
    /// actually carry the frontend and the global Rust dispatches into. A stale or truncated
    /// `agent-ui/web/dist/index.html` (build.rs's mtime freshness check going wrong, a half-written
    /// build output) would produce a WebView that loads without error and can never receive a single
    /// envelope -- which presents exactly as the wedged panel `reload_document` exists to recover
    /// from, so the recovery would silently reproduce the fault.
    ///
    /// What this does NOT prove: that the reload itself works. Re-injection is a real `load_html`
    /// call on a real `WebView`. `WebViewExt::reload()` does NOT work for a substitute-data load
    /// like this one.
    #[test]
    fn the_embedded_panel_document_carries_the_frontend_and_its_dispatch_entry_point() {
        assert!(
            AGENT_UI_HTML.contains("__eitriDispatch"),
            "the embedded document never installs the global Rust pushes into -- every envelope would be dropped"
        );
        assert!(
            AGENT_UI_HTML.contains("eitriAgent"),
            "the embedded document never posts through the script-message handler -- no command could reach Rust"
        );
    }

    /// The first frame of a freshly loaded document must already be in nvim's colours: the theme
    /// is parsed before the script that renders anything, and nothing else in the document moves.
    #[test]
    fn the_panel_document_carries_its_theme_before_its_script() {
        let tokens = eitri_core::theme::ThemeTokens::fallback();
        let html = PANEL_DOCUMENT.themed(&tokens.css_vars());
        let csp_at = html
            .find("<meta http-equiv=\"Content-Security-Policy\"")
            .expect("the CSP meta is inserted");
        let style_at = html
            .find("<style id=\"nv-theme\">")
            .expect("the theme block is inserted");
        let script_at = html.find("<script").expect("the single-file build inlines its script");
        assert!(
            csp_at < style_at,
            "the CSP must be in force before the theme style, or it would not cover it"
        );
        assert!(
            style_at < script_at,
            "the theme must be parsed before the script that renders"
        );
        assert_eq!(html.matches("<style id=\"nv-theme\">").count(), 1);
        assert!(html.contains(&format!("--nv-bg:{};", tokens.bg.hex())));
        let end = style_at + html[style_at..].find("</style>").unwrap() + "</style>".len();
        // The CSP meta and the theme style are inserted back to back at the same point `<head>`
        // used to sit -- removing both must reconstruct the untouched embedded document.
        assert_eq!(format!("{}{}", &html[..csp_at], &html[end..]), AGENT_UI_HTML);
    }

    /// The CSP meta is the very first thing inside `<head>` -- nothing (not even the theme style)
    /// may load before it -- and its `content` is exactly the policy the WebView builder applies, so
    /// the two copies cannot drift apart (see `panel_content_security_policy`).
    #[test]
    fn the_panel_documents_csp_meta_is_first_in_head_and_matches_the_policy_constant() {
        let tokens = eitri_core::theme::ThemeTokens::fallback();
        let html = PANEL_DOCUMENT.themed(&tokens.css_vars());
        let head_at = html.find("<head>").expect("the document has a <head>");
        let after_head = head_at + "<head>".len();
        assert!(
            html[after_head..].starts_with("<meta http-equiv=\"Content-Security-Policy\""),
            "the CSP meta must be the very first thing after <head>, before even the theme style"
        );
        let expected_meta = format!(
            "<meta http-equiv=\"Content-Security-Policy\" content=\"{}\">",
            panel_content_security_policy()
        );
        assert!(html.contains(&expected_meta), "{html}");
        assert_eq!(html.matches("Content-Security-Policy").count(), 1);
    }

    /// Pins the policy string itself against silent drift, and each directive's presence against
    /// the reasoning in `panel_content_security_policy`'s doc comment: `default-src 'none'` closes
    /// everything not named, scripts are only the bundle's own (by hash), and nothing here reopens
    /// network access for the panel (`connect-src`, `frame-src`, `object-src`, `media-src` all
    /// `'none'`; images/fonts limited to `data:`).
    #[test]
    fn the_panel_csp_closes_every_directive_it_does_not_explicitly_reopen() {
        let policy = panel_content_security_policy();
        let hashes = eitri_panel::panel_csp::inline_script_hashes(AGENT_UI_HTML)
            .expect("the embedded bundle's scripts can be pinned")
            .join(" ");
        assert_eq!(
            policy,
            format!(
                "default-src 'none'; script-src {hashes}; style-src 'unsafe-inline'; img-src data:; \
                 font-src data:; media-src 'none'; connect-src 'none'; frame-src 'none'; object-src 'none'; \
                 form-action 'none'; base-uri 'none'"
            )
        );
        assert!(policy.starts_with("default-src 'none';"));
        for directive in [
            "connect-src 'none'",
            "frame-src 'none'",
            "object-src 'none'",
            "media-src 'none'",
        ] {
            assert!(policy.contains(directive), "{directive} missing from {policy}");
        }
        // No directive allows an https:/http: remote load -- every source list is 'none', a hash, an
        // inline keyword for styles, or `data:`. Base64 has no `*`, so a hash cannot trip the last one.
        assert!(!policy.contains("https:"));
        assert!(!policy.contains("http:"));
        assert!(!policy.contains('*'));
    }

    /// The policy's `script-src` names exactly the bundle's inline script, by a hash computed here a
    /// second time without the extractor: the first `<script` start tag's body, hashed with `sha2`
    /// rather than GLib. If a rebuilt bundle and the pinned hash ever disagree, this fails instead of
    /// the panel silently never starting.
    #[test]
    fn the_panel_csp_pins_the_embedded_bundles_script_by_hash() {
        use sha2::Digest;
        let tag_at = AGENT_UI_HTML.find("<script").expect("the bundle inlines a script");
        let body_at = tag_at + AGENT_UI_HTML[tag_at..].find('>').expect("its start tag ends") + 1;
        let body_end = body_at + AGENT_UI_HTML[body_at..].find("</script").expect("it is closed");
        let expected = format!(
            "'sha256-{}'",
            gtk4::glib::base64_encode(&sha2::Sha256::digest(&AGENT_UI_HTML.as_bytes()[body_at..body_end]))
        );
        let hashes = eitri_panel::panel_csp::inline_script_hashes(AGENT_UI_HTML);
        assert!(
            matches!(&hashes, Ok(list) if !list.is_empty()),
            "the embedded bundle's scripts cannot be pinned: {hashes:?}"
        );
        let policy = panel_content_security_policy();
        let script_src = policy
            .split(';')
            .map(str::trim)
            .find_map(|directive| directive.strip_prefix("script-src "))
            .expect("the policy has a script-src directive");
        let sources: Vec<&str> = script_src.split_whitespace().collect();
        assert_eq!(sources.first().copied(), Some(expected.as_str()), "{policy}");
        assert_eq!(Ok(sources.iter().map(ToString::to_string).collect::<Vec<_>>()), hashes);
    }

    /// Scripts are allowed only by hash: no keyword or scheme in `script-src` that would let an
    /// injected inline handler, `javascript:` URL or `<script>` run.
    #[test]
    fn the_panel_csp_script_src_allows_nothing_but_hashes() {
        let policy = panel_content_security_policy();
        assert!(policy.starts_with("default-src 'none';"), "{policy}");
        let script_src = policy
            .split(';')
            .map(str::trim)
            .find_map(|directive| directive.strip_prefix("script-src "))
            .expect("the policy has a script-src directive");
        let sources: Vec<&str> = script_src.split_whitespace().collect();
        assert!(!sources.is_empty(), "{policy}");
        for source in sources {
            for forbidden in [
                "'unsafe-inline'",
                "'unsafe-eval'",
                "'unsafe-hashes'",
                "'strict-dynamic'",
            ] {
                assert_ne!(source, forbidden, "{policy}");
            }
            let digest = source
                .strip_prefix("'sha256-")
                .and_then(|rest| rest.strip_suffix('\''))
                .unwrap_or_else(|| panic!("script-src allows something other than a hash: {source} in {policy}"));
            // A SHA-256 digest is 32 bytes: 44 base64 characters, the last one padding.
            assert!(
                digest.len() == 44
                    && digest.ends_with('=')
                    && digest
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=')),
                "not a SHA-256 digest: {source} in {policy}"
            );
        }
    }

    /// The `WebView` itself must be built with the same policy as a floor the page cannot loosen --
    /// source-scanned the same way
    /// `the_panel_document_is_loaded_in_a_secure_context_that_never_resolves` pins `load_html`,
    /// since constructing a real `WebView` needs a running WebKitGTK display this crate's tests do
    /// not have.
    #[test]
    fn the_webview_builder_applies_the_content_security_policy() {
        let source = include_str!("agent_panel.rs");
        let fn_at = source
            .find("fn build_chat_page(")
            .expect("every chat page is built by build_chat_page");
        let builder_at = source[fn_at..]
            .find("WebView::builder()")
            .map(|i| fn_at + i)
            .expect("the panel's WebView is built through WebView::builder()");
        let build_at = source[builder_at..]
            .find(".build()")
            .map(|i| builder_at + i)
            .expect("the builder chain ends in .build()");
        let chain = &source[builder_at..build_at];
        assert!(
            chain.contains(".default_content_security_policy(panel_content_security_policy())"),
            "the WebView builder chain never applies the panel's CSP: {chain}"
        );
    }

    /// The panel's WebView uses an ephemeral `NetworkSession` with a closed loopback proxy,
    /// so requests that pass the CSP and sanitizer still cannot leave.
    /// Check the builder source because constructing a WebView requires a display.
    /// The scan ends at the first page's `.build()`; pages built with a related view inherit
    /// that page's session.
    #[test]
    fn the_webview_builder_applies_the_network_proxy_backstop() {
        assert_eq!(PANEL_NETWORK_PROXY_URI, "http://127.0.0.1:9");
        let source = include_str!("agent_panel.rs");
        // Scoped to `build_chat_page`'s own body, ending at the WebView `.build()` call -- never
        // the whole file. `source` (the whole file, via `include_str!`) includes this very test's own
        // string literals, so searching all of `source` for e.g. "NetworkSession::new_ephemeral()"
        // would always find a match in the assertion below regardless of what the production code
        // does: a mutation that deleted the real call, or swapped in a persistent `NetworkSession`,
        // would still leave this test green. `fn_at` keeps the needles below restricted to code that
        // actually ran before the WebView was built.
        let fn_at = source
            .find("fn build_chat_page(")
            .expect("every chat page is built by build_chat_page");
        let builder_at = source[fn_at..]
            .find("WebView::builder()")
            .map(|i| fn_at + i)
            .expect("the panel's WebView is built through WebView::builder()");
        let build_at = source[builder_at..]
            .find(".build()")
            .map(|i| builder_at + i)
            .expect("the builder chain ends in .build()");
        let scope = &source[fn_at..build_at];
        let chain = &source[builder_at..build_at];
        assert!(
            chain.contains(".network_session(&network_session)"),
            "the WebView builder chain never installs the ephemeral network session: {chain}"
        );
        assert!(
            scope.contains("NetworkSession::new_ephemeral()"),
            "the backstop session must be ephemeral: no cookies, cache or storage"
        );
        assert!(
            scope.contains("NetworkProxyMode::Custom")
                && scope.contains("NetworkProxySettings::new(Some(PANEL_NETWORK_PROXY_URI)"),
            "the ephemeral session must be pointed at the unreachable proxy, not left on WebKit's default"
        );
    }

    /// The files under `shell/src` whose own pages are not chat pages: the grid probe's test pages, the Lua
    /// panels and the plugin pages (each with its own scheme, policy and bridge, `plugin_page.rs`).
    /// Everything else that builds a page builds a chat page.
    const NOT_CHAT_PAGES: &[&str] = &[
        "shell/src/grid_probe/",
        "shell/src/lua/panel.rs",
        "shell/src/plugin_page.rs",
    ];

    /// What builds a page, or hands it a message bridge.
    const PAGE_MAKERS: &[&str] = &[
        "WebView::builder()",
        "WebView::new(",
        "UserContentManager::new()",
        "register_script_message_handler",
    ];

    /// Every chat page comes from `build_chat_page`: in the code of `shell/src` (comments, literals and
    /// test modules blanked), outside the files that build pages that are not chat pages, a `WebView` is
    /// built, a content manager made and a message handler registered once each, and that once is inside
    /// `build_chat_page`'s body. A second constructor would be a page without the policy, or with a
    /// handler that answers for some other view.
    #[test]
    fn every_chat_page_is_built_by_the_one_constructor() {
        use eitri_core::source_scan as scan;
        let sources = scan::rust_sources(&["shell/src"]);
        for exempt in NOT_CHAT_PAGES {
            assert!(
                sources.iter().any(|source| source.path.starts_with(exempt)),
                "{exempt} is not under shell/src any more: the list names nothing"
            );
        }
        scan::require_anchors(&sources, &["fn build_chat_page("]);
        let mut found: Vec<(String, &str, usize)> = Vec::new();
        let mut in_constructor = std::collections::BTreeMap::new();
        for source in &sources {
            if NOT_CHAT_PAGES.iter().any(|exempt| source.path.starts_with(exempt)) {
                continue;
            }
            let code = scan::code_only(&source.text);
            let code = scan::without(&code, &scan::modules(&code, "tests"));
            for maker in PAGE_MAKERS {
                let count = code.matches(maker).count();
                if count > 0 {
                    found.push((source.path.clone(), maker, count));
                }
            }
            for body in scan::functions(&code, "build_chat_page") {
                for maker in PAGE_MAKERS {
                    *in_constructor.entry(*maker).or_insert(0) += body.matches(maker).count();
                }
            }
        }
        let expected: Vec<(String, &str, usize)> = [
            "WebView::builder()",
            "UserContentManager::new()",
            "register_script_message_handler",
        ]
        .into_iter()
        .map(|maker| ("shell/src/agent_panel.rs".to_string(), maker, 1))
        .collect();
        let mut found_sorted = found.clone();
        found_sorted.sort();
        let mut expected_sorted = expected;
        expected_sorted.sort();
        assert_eq!(found_sorted, expected_sorted, "a page is built outside build_chat_page");
        for maker in [
            "WebView::builder()",
            "UserContentManager::new()",
            "register_script_message_handler",
        ] {
            assert_eq!(
                in_constructor.get(maker),
                Some(&1),
                "{maker} is not inside build_chat_page"
            );
        }
    }

    /// The `eitriAgent` handler is the only place a message's page is decided, and it decides it from
    /// the view the page was built for: `build_chat_page` takes `view: ViewId`, its message closure
    /// hands that `view` to `page_message`, and no message is ever said to come from a fixed view.
    #[test]
    fn the_message_handler_carries_the_pages_own_view() {
        use eitri_core::source_scan as scan;
        let raw = include_str!("agent_panel.rs");
        let production = raw.split("#[cfg(test)]\nmod tests {").next().unwrap_or_default();
        assert_eq!(
            production
                .matches("register_script_message_handler(\"eitriAgent\", None)")
                .count(),
            1,
            "the page's bridge is registered once"
        );
        assert_eq!(
            production
                .matches("connect_script_message_received(Some(\"eitriAgent\")")
                .count(),
            1,
            "the page's bridge is heard once"
        );
        let code = scan::code_only(raw);
        let code = scan::without(&code, &scan::modules(&code, "tests"));
        let fn_at = code.find("fn build_chat_page(").expect("build_chat_page is defined");
        let open = fn_at + code[fn_at..].find('{').expect("its body");
        let signature: String = code[fn_at..open].split_whitespace().collect();
        assert!(signature.contains("view:ViewId"), "{signature}");
        let bodies = scan::functions(&code, "build_chat_page");
        assert_eq!(bodies.len(), 1);
        let body: String = bodies[0].split_whitespace().collect();
        let heard_at = body
            .find(".connect_script_message_received(")
            .expect("the page's messages are heard in build_chat_page");
        let closure = &body[heard_at..];
        let closure = &closure[..closure.find("});").expect("the closure ends")];
        assert!(
            closure.contains("move|"),
            "the view is moved into the handler: {closure}"
        );
        assert!(closure.contains(".page_message(view,"), "{closure}");
        assert_eq!(body.matches(".page_message(").count(), 1, "{body}");
        for call in ["page_message(", "link_clicked(", "page_shown(", "page_gone("] {
            for (at, _) in code.match_indices(call) {
                let rest: String = code[at + call.len()..].split_whitespace().collect();
                assert!(
                    !rest.starts_with("ViewId::"),
                    "a page's {call}..) names a fixed view, not its own"
                );
            }
        }
        assert_eq!(code.matches("fn build_chat_page(").count(), 1);
    }

    thread_local! {
        /// When the ignored hold test's source should release. A thread-local rather than a capture
        /// so the closure above stays the same shape as the real one.
        static RELEASE_AT: std::cell::Cell<Option<std::time::Instant>> = const { std::cell::Cell::new(None) };
    }

    /// GLib's own SHA-256 and base64 are an independent oracle for the hash the policy pins.
    #[test]
    fn the_hash_matches_glibs() {
        let mut big = Vec::new();
        for i in 0..(1 << 20) {
            big.push((i % 251) as u8);
        }
        for data in [&b""[..], b"abc", b"window.__eitriDispatch(1)", &big] {
            let mut checksum = gtk4::glib::Checksum::new(gtk4::glib::ChecksumType::Sha256).unwrap();
            checksum.update(data);
            let glib = gtk4::glib::base64_encode(&checksum.digest()).to_string();
            assert_eq!(eitri_panel::panel_csp::sha256_base64(data), glib);
        }
    }

    /// A page for the pump tests: visible, and it keeps what it is sent.
    #[derive(Default)]
    struct CountingPage {
        sent: std::cell::RefCell<Vec<String>>,
    }

    impl eitri_panel::panel_page::PageSurface for CountingPage {
        fn send(&self, envelope_json: &str) {
            self.sent.borrow_mut().push(envelope_json.to_owned());
        }
        fn is_visible(&self) -> bool {
            true
        }
        fn load_document(&self, _html: &str, _base_uri: &str) {}
        fn set_background(&self, _rgb: [u8; 3]) {}
    }

    /// Runs glib's default main context for `ms`.
    fn iterate_for(ms: u64) {
        let context = gtk4::glib::MainContext::default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
        while std::time::Instant::now() < deadline {
            while context.iteration(false) {}
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// An idle panel (a ready page, one empty tab, nothing happening) on the real pump and glib's loop:
    /// how many ticks run in one second.
    #[test]
    fn an_idle_panel_is_not_ticked() {
        let _turn = crate::layout_state::DEFAULT_CONTEXT_TURN
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let context = gtk4::glib::MainContext::default();
        let _owner = context.acquire().expect("glib's default main context");
        let page = std::rc::Rc::new(CountingPage::default());
        let handle = AgentPanelHandle::for_host_test(page.clone());
        handle.page_message(ViewId::BASE, r#"{"type":"ready","request_id":"r1"}"#);
        start_pump_timer(handle.clone());
        iterate_for(300);
        let before = handle.ticks_run();
        iterate_for(1000);
        let ticks = handle.ticks_run() - before;
        eprintln!("idle panel: {ticks} ticks in 1 s");
        assert!(ticks <= 1, "an idle panel ticked {ticks} times in a second");

        // A call into the panel is still answered by a tick, at once, and then it is quiet again.
        let sent = page.sent.borrow().len();
        let before = handle.ticks_run();
        handle.new_tab();
        iterate_for(100);
        assert!(handle.ticks_run() > before, "the call woke the tick");
        assert!(page.sent.borrow().len() > sent, "and what it changed reached the page");
        let before = handle.ticks_run();
        iterate_for(500);
        assert!(handle.ticks_run() - before <= 1, "quiet again");

        // The close stops it for good.
        let _watch = handle.shutdown();
        iterate_for(100);
        let before = handle.ticks_run();
        handle.wake();
        iterate_for(200);
        assert_eq!(handle.ticks_run(), before, "nothing runs after the close");
    }

    /// The supervisor raise waits on its socket: a connected window is not woken while nothing arrives
    /// (it looked five times a second), a request still raises it at once, and a supervisor that goes away
    /// lets go of the watch. A window with no supervisor arms nothing.
    #[test]
    fn the_supervisor_raise_waits_on_its_socket() {
        let _turn = crate::layout_state::DEFAULT_CONTEXT_TURN
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let context = gtk4::glib::MainContext::default();
        let _owner = context.acquire().expect("glib's default main context");
        let handle = AgentPanelHandle::for_host_test(std::rc::Rc::new(CountingPage::default()));
        assert_eq!(
            handle.supervisor_link(),
            eitri_panel::agent_panel::SupervisorLink::Absent
        );
        watch_supervisor(handle.clone(), std::rc::Rc::new(|| panic!("no supervisor, no raise")));

        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        handle.connect_test_supervisor(ours);
        let raised = std::rc::Rc::new(std::cell::Cell::new(0));
        {
            let raised = raised.clone();
            watch_supervisor(handle.clone(), std::rc::Rc::new(move || raised.set(raised.get() + 1)));
        }
        SUPERVISOR_LOOKS.with(|looks| looks.set(0));
        iterate_for(1000);
        let looks = SUPERVISOR_LOOKS.with(|looks| looks.get());
        eprintln!("supervisor raise: {looks} looks in 1 s with nothing sent");
        assert_eq!(looks, 0);

        use std::io::Write;
        (&theirs).write_all(b"{\"type\":\"activate\"}\n").unwrap();
        iterate_for(100);
        assert_eq!(raised.get(), 1, "the request raised the window");

        drop(theirs);
        iterate_for(100);
        let after_close = SUPERVISOR_LOOKS.with(|looks| looks.get());
        iterate_for(300);
        assert_eq!(
            SUPERVISOR_LOOKS.with(|looks| looks.get()),
            after_close,
            "a closed connection is let go, not looked at forever"
        );
        assert_eq!(
            handle.supervisor_link(),
            eitri_panel::agent_panel::SupervisorLink::Absent
        );
    }

    #[test]
    fn a_timer_never_fires_before_its_deadline() {
        let now = Instant::now();
        let ms = std::time::Duration::from_millis;
        assert_eq!(timer_wait(now + std::time::Duration::from_micros(33_400), now), ms(34));
        assert_eq!(timer_wait(now + ms(33), now), ms(33), "a whole millisecond stays");
        assert_eq!(timer_wait(now + std::time::Duration::from_nanos(1), now), ms(1));
        assert_eq!(timer_wait(now, now + ms(5)), ms(0), "a deadline passed fires at once");
    }
}
