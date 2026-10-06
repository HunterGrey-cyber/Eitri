//! The agent panel's WebKitGTK half: the view and its content-security and network policy, the page and
//! process implementations the panel's logic drives (`eitri_panel::panel_page`), the pump's GLib timer, the
//! application hold that outlives a closing window and the reload action. The panel's logic -- tabs, sessions,
//! the trust gate, turn review, the page protocol -- is `eitri_panel::agent_panel`.

use eitri_core::agent_backend::BackendKind;
use eitri_panel::agent_panel::{PanelInit, ShutdownWatch, CLOSE_WATCH_POLL};
use eitri_panel::panel_document::PanelDocument;
use eitri_panel::panel_page::{PageSurface, PanelHost, PanelPage};
use gtk4::prelude::*;
use gtk4::Application;
use std::path::PathBuf;
use std::rc::Rc;
use webkit6::prelude::*;
use webkit6::{NetworkProxyMode, NetworkProxySettings, NetworkSession, UserContentManager, WebView};

// What the rest of `shell` reaches through `crate::agent_panel::`.
pub(crate) use eitri_panel::agent_panel::{AgentPanelHandle, HintInbound, ReviewConfig};

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
/// same state with no page (`AgentPanelHandle::page`'s own doc), no pump, no supervisor client.
pub(crate) fn build_agent_panel(
    project_dir: PathBuf,
    editor_context: eitri_core::editor_context::ContextSource,
    scratch: Option<eitri_core::scratch::ScratchDir>,
    backend_kind: BackendKind,
    unavailable: Option<&str>,
) -> (gtk4::Widget, AgentPanelHandle) {
    let init = PanelInit {
        project_dir,
        editor_context,
        scratch,
        backend_kind,
        document: &PANEL_DOCUMENT,
    };
    if let Some(notice) = unavailable {
        let handle = AgentPanelHandle::new(init, None, Rc::new(GtkPanelHost));
        return (crate::webkit_sandbox::notice_widget(notice), handle);
    }
    let content_manager = UserContentManager::new();
    // A CSP floor the page cannot loosen, plus a network backstop behind it -- see
    // `panel_content_security_policy` and `PANEL_NETWORK_PROXY_URI`'s doc comments.
    let network_session = NetworkSession::new_ephemeral();
    network_session.set_proxy_settings(
        NetworkProxyMode::Custom,
        Some(&NetworkProxySettings::new(Some(PANEL_NETWORK_PROXY_URI), &[])),
    );
    let webview = WebView::builder()
        .user_content_manager(&content_manager)
        .network_session(&network_session)
        .default_content_security_policy(panel_content_security_policy())
        .build();
    webview.set_hexpand(true);
    webview.set_vexpand(true);
    let page = Rc::new(PanelPage::new(Rc::new(WebKitPage(webview.clone()))));
    let handle = AgentPanelHandle::new(init, Some(page), Rc::new(GtkPanelHost));

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
                                handle.link_clicked(uri.as_deref());
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
        content_manager.connect_script_message_received(Some("eitriAgent"), move |_manager, js_value| {
            let raw = js_value.to_str();
            handle.page_message(&raw);
        });
    }

    handle.load_first_document();

    // Back on screen: whatever finished in the active tab while the chat was away has been seen
    // (the tray's `agent •`).
    {
        let handle = handle.clone();
        webview.connect_map(move |_| handle.page_shown());
    }

    // Started once, at construction, rather than when a session starts: it is also what collects a
    // finished background connect. Previously it was started inside the handler that constructs the
    // backend, which meant a second start attempt after a failure installed a SECOND timer on the
    // same state -- every later event would then be dispatched twice.
    start_pump_timer(handle.clone());

    // Recover this WebView's own crash automatically (guarded) instead of leaving the chat
    // blank until the user finds `\u{21bb}`/`prefix r` by hand. See `AgentPanelHandle::page_gone`'s
    // own doc. An end the host asked for itself is no crash.
    {
        let handle = handle.clone();
        webview.connect_web_process_terminated(move |_webview, reason| {
            if reason == webkit6::WebProcessTerminationReason::TerminatedByApi {
                return;
            }
            handle.page_gone(&format!("{reason:?}"));
        });
    }

    (webview.upcast(), handle)
}

fn start_pump_timer(handle: AgentPanelHandle) {
    gtk4::glib::timeout_add_local(AgentPanelHandle::TICK, move || {
        if handle.tick() {
            gtk4::glib::ControlFlow::Continue
        } else {
            gtk4::glib::ControlFlow::Break
        }
    });
}

/// The panel's Content-Security-Policy: a floor the page cannot loosen, applied two ways -- as the
/// `WebView`'s own `default-content-security-policy` (`build_agent_panel`) and as the first element
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
/// button is in fact sensitive on screen is a GUI-pass item -- see
/// `shell/MANUAL_VERIFICATION.md`'s 2026-09-15 section.
///
/// **Known limitation while `shell` can still run two windows in one process:** the action is
/// process-global (`app.add_action`) and this handle is per-window, so a second `activate` running
/// `build_ui` again silently REPLACES the first window's action -- after which window 1's `⟳` and
/// Ctrl+Shift+R reload window 2's panel. `ApplicationFlags::NON_UNIQUE`
/// makes that unreachable by giving each launch its own process, so this only matters if a second
/// window is ever hosted in one process again.
pub(crate) fn install_reload_action(app: &Application, handle: &AgentPanelHandle) {
    let action = gtk4::gio::SimpleAction::new("reload-agent-panel", None);
    // Cloned because the caller still needs its own handle afterwards -- `connect_close_request`
    // takes it by move.
    let handle = handle.clone();
    action.connect_activate(move |_, _| handle.reload_document_by_hand());
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
    /// call on a real `WebView`, and it is verified in a sandbox -- notably `WebViewExt::reload()`
    /// does NOT work for a substitute-data load like this one, per
    /// `shell/MANUAL_VERIFICATION.md`'s 2026-09-11 reload check.
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
        assert_eq!(Ok(sources.iter().map(|s| s.to_string()).collect::<Vec<_>>()), hashes);
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
        let builder_at = source
            .find("WebView::builder()")
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

    /// Finding 3's backstop (`PANEL_NETWORK_PROXY_URI`'s own doc comment): the panel's `WebView` is
    /// built with an ephemeral `NetworkSession` whose proxy is the closed loopback "discard" port, so
    /// a request that gets past the CSP and the sanitizer still cannot leave. Source-scanned for the
    /// same reason the CSP test above is.
    #[test]
    fn the_webview_builder_applies_the_network_proxy_backstop() {
        assert_eq!(PANEL_NETWORK_PROXY_URI, "http://127.0.0.1:9");
        let source = include_str!("agent_panel.rs");
        // Scoped to `build_agent_panel`'s own body, ending at the WebView `.build()` call -- never
        // the whole file. `source` (the whole file, via `include_str!`) includes this very test's own
        // string literals, so searching all of `source` for e.g. "NetworkSession::new_ephemeral()"
        // would always find a match in the assertion below regardless of what the production code
        // does: a mutation that deleted the real call, or swapped in a persistent `NetworkSession`,
        // would still leave this test green. `fn_at` keeps the needles below restricted to code that
        // actually ran before the WebView was built.
        let fn_at = source
            .find("fn build_agent_panel(")
            .expect("the panel is built by build_agent_panel");
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
}
