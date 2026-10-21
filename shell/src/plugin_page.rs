//! A plugin's page in WebKitGTK: a web context of its own serving the plugin's scheme, a content security
//! policy floor the page cannot loosen, an ephemeral network session whose proxy lets through only the
//! origins the manifest declares, a navigation lock, and the bridge script. What a page may do is decided in
//! `eitri_core::plugin`; this file builds the view and carries text both ways. Its `PagePort` is the only
//! place a script is run in the page, and `apply_theme` the only place the window's theme reaches the view.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use eitri_core::plugin::host::PagePort;
use eitri_core::plugin::manifest::Origin;
use eitri_core::plugin::page_policy::{self, Navigation, SiteRefusal};
use eitri_core::plugin::protocol::BRIDGE_JS;
use eitri_core::plugin::theme::PageTheme;
use gtk4::gio;
use gtk4::gio::prelude::*;
use gtk4::glib;
use gtk4::prelude::*;
use webkit6::prelude::*;
use webkit6::{
    NetworkProxyMode, NetworkProxySettings, NetworkSession, URISchemeRequest, UserContentInjectedFrames,
    UserContentManager, UserScript, UserScriptInjectionTime, UserStyleLevel, UserStyleSheet, WebContext, WebView,
};

/// The name the page posts to: `window.webkit.messageHandlers.eitriPlugin`.
pub(crate) const HANDLER: &str = "eitriPlugin";

/// The keyboard kit `build.rs` bundles from `plugin-kit/keys`, answered at `page_policy::KIT_PATH` in every
/// plugin's origin so a page loads it with an ordinary same-origin script tag under the CSP floor.
const KIT_JS: &str = include_str!("../../plugin-kit/keys/dist/keys.js");

pub(crate) struct PageSpec {
    pub(crate) id: String,
    /// Canonical when the site is built; a missing site shows the static "not built" page.
    pub(crate) site_root: PathBuf,
    pub(crate) entry: String,
    pub(crate) origins: Vec<Origin>,
    /// Where a file the page saves (its own `blob:`) goes.
    pub(crate) downloads: PathBuf,
}

pub(crate) struct PageHooks {
    /// A message the page posted while on its own origin.
    pub(crate) on_message: Rc<dyn Fn(&str)>,
    /// A document was committed (a load, a reload, a back/forward-cache restore): a new document starts, which
    /// must say `host.hello` again, and the host sends it the current theme, which a restored document would
    /// otherwise never hear. Only a commit counts: a load that never commits leaves the running document as it was.
    pub(crate) on_committed: Rc<dyn Fn()>,
    /// An `http`, `https` or `mailto` link the user clicked.
    pub(crate) open_outside: Rc<dyn Fn(&str)>,
}

/// Serves one request of the plugin's scheme. Checking the path touches the disk (`canonicalize`, `metadata`),
/// which can stall on a slow or hung filesystem, so it runs on a worker thread; the request is answered back on
/// the main context, and the file itself is read without blocking this thread.
fn serve(request: &URISchemeRequest, site_root: &Path, id: &str, entry: &str) {
    let uri = request.uri().map(|u| u.to_string()).unwrap_or_default();
    // The kit is part of the host, not of the plugin's site: it is answered from memory, ahead of the site
    // lookup, so a site file of the same name can neither replace it nor be asked for through it.
    if page_policy::kit_request(id, &uri) {
        let bytes = glib::Bytes::from_static(KIT_JS.as_bytes());
        let stream = gio::MemoryInputStream::from_bytes(&bytes);
        request.finish(&stream, bytes.len() as i64, Some("text/javascript"));
        return;
    }
    let request = request.clone();
    let (site_root, id, entry) = (site_root.to_path_buf(), id.to_owned(), entry.to_owned());
    glib::spawn_future_local(async move {
        let checked = {
            let (site_root, id, entry, uri) = (site_root, id.clone(), entry.clone(), uri.clone());
            gio::spawn_blocking(move || page_policy::site_file(&site_root, &id, &entry, &uri)).await
        };
        // A worker that panicked is answered like a missing file rather than leaving the request hanging.
        let checked = checked.unwrap_or(Err(SiteRefusal::NotFound));
        answer(&request, checked, &id, &entry, &uri);
    });
}

fn answer(
    request: &URISchemeRequest,
    checked: Result<page_policy::SiteFile, SiteRefusal>,
    id: &str,
    entry: &str,
    uri: &str,
) {
    match checked {
        Ok(file) => {
            let request = request.clone();
            let (mime, len) = (file.mime, i64::try_from(file.len).unwrap_or(-1));
            gio::File::for_path(&file.path).read_async(
                glib::Priority::DEFAULT,
                None::<&gio::Cancellable>,
                move |opened| match opened {
                    Ok(stream) => request.finish(&stream, len, Some(mime)),
                    Err(mut e) => request.finish_error(&mut e),
                },
            );
        }
        Err(SiteRefusal::NotFound) if page_policy::is_entry(id, entry, uri) => {
            let bytes = glib::Bytes::from_static(page_policy::NOT_BUILT_HTML.as_bytes());
            let stream = gio::MemoryInputStream::from_bytes(&bytes);
            request.finish(&stream, bytes.len() as i64, Some("text/html"));
        }
        Err(refusal) => {
            let kind = match refusal {
                SiteRefusal::NotFound => gio::IOErrorEnum::NotFound,
                _ => gio::IOErrorEnum::PermissionDenied,
            };
            let mut error = glib::Error::new(kind, refusal.message());
            request.finish_error(&mut error);
        }
    }
}

/// The window's theme on the view: its background, which shows before the web process paints and while a
/// reload is under way (WebKit's default is opaque white), and a user style sheet holding the theme's
/// variables on the root, which every document starts with. The sheet is replaced whole, so a variable the
/// new theme lacks does not linger from the old one.
fn apply_theme(webview: &WebView, theme: &PageTheme) {
    let [r, g, b] = theme.background();
    webview.set_background_color(&gtk4::gdk::RGBA::new(
        f32::from(r) / 255.0,
        f32::from(g) / 255.0,
        f32::from(b) / 255.0,
        1.0,
    ));
    if let Some(content) = webview.user_content_manager() {
        content.remove_all_style_sheets();
        content.add_style_sheet(&UserStyleSheet::new(
            &theme.user_style_sheet(),
            UserContentInjectedFrames::TopFrame,
            UserStyleLevel::User,
            &[],
            &[],
        ));
    }
}

/// Builds the view and starts loading the plugin's entry, already wearing `theme`.
pub(crate) fn build(spec: &PageSpec, hooks: PageHooks, theme: &PageTheme) -> WebView {
    let context = WebContext::new();
    if let Some(security) = context.security_manager() {
        security.register_uri_scheme_as_secure(page_policy::SCHEME);
        security.register_uri_scheme_as_cors_enabled(page_policy::SCHEME);
    }
    {
        let (site_root, id, entry) = (spec.site_root.clone(), spec.id.clone(), spec.entry.clone());
        context.register_uri_scheme(page_policy::SCHEME, move |request| {
            serve(request, &site_root, &id, &entry)
        });
    }

    let content = UserContentManager::new();
    content.add_script(&UserScript::new(
        BRIDGE_JS,
        UserContentInjectedFrames::TopFrame,
        UserScriptInjectionTime::Start,
        &[],
        &[],
    ));
    content.register_script_message_handler(HANDLER, None);

    // The CSP is the boundary; the proxy is a backstop behind it: anything that gets past the policy dies at
    // a closed port, except the declared origins' hosts.
    let session = NetworkSession::new_ephemeral();
    let ignore = page_policy::proxy_ignore_hosts(&spec.origins);
    let ignore: Vec<&str> = ignore.iter().map(String::as_str).collect();
    session.set_proxy_settings(
        NetworkProxyMode::Custom,
        Some(&NetworkProxySettings::new(Some(page_policy::PROXY_BACKSTOP), &ignore)),
    );
    // The downloads are the page's own `blob:` files, plus a same-origin `<a download>` (WebKit turns those into
    // downloads after the navigation lock has let them through). Whichever starts one, only site files or the
    // page's own data can be saved, and each goes under a safe name in the download directory, never over an
    // existing file. Choosing the name stats the directory, which can hang on a network mount, so it runs on a
    // worker thread: the handler claims the decision at once and WebKit holds the download until it is set.
    {
        let downloads = spec.downloads.clone();
        let id = spec.id.clone();
        session.connect_download_started(move |_, download| {
            download.set_allow_overwrite(false);
            let (downloads, id) = (downloads.clone(), id.clone());
            download.connect_decide_destination(move |download, suggested| {
                let (download, dir, suggested, id) =
                    (download.clone(), downloads.clone(), suggested.to_owned(), id.clone());
                glib::spawn_future_local(async move {
                    let target = {
                        let suggested = suggested.clone();
                        gio::spawn_blocking(move || page_policy::download_destination(&dir, &suggested)).await
                    };
                    match target {
                        Ok(Some(target)) => download.set_destination(&target.to_string_lossy()),
                        Ok(None) => {
                            eprintln!(
                                "[plugin {id}] not saving {}: every name for it is taken",
                                suggested.escape_debug()
                            );
                            download.cancel();
                        }
                        // A worker that panicked leaves no destination; the download is not left waiting.
                        Err(_) => {
                            eprintln!("[plugin {id}] not saving {}: no destination", suggested.escape_debug());
                            download.cancel();
                        }
                    }
                });
                true
            });
        });
    }

    let webview = WebView::builder()
        .web_context(&context)
        .user_content_manager(&content)
        .network_session(&session)
        .default_content_security_policy(page_policy::csp_floor(&spec.origins))
        .build();
    if let Some(settings) = WebViewExt::settings(&webview) {
        settings.set_javascript_can_open_windows_automatically(false);
        settings.set_enable_developer_extras(false);
    }
    webview.set_hexpand(true);
    webview.set_vexpand(true);

    // A message counts only while the view shows the plugin's own origin. A weak reference: the content
    // manager belongs to the view, and a strong one here would keep the view alive forever.
    {
        let weak = webview.downgrade();
        let id = spec.id.clone();
        let on_message = hooks.on_message.clone();
        content.connect_script_message_received(Some(HANDLER), move |_, value| {
            let Some(view) = weak.upgrade() else { return };
            let here = view.uri().map(|u| u.to_string()).unwrap_or_default();
            if page_policy::navigation(&id, &here) != Navigation::Stay || !value.is_string() {
                eprintln!("[plugin {id}] ignored a message from {here}");
                return;
            }
            on_message(&value.to_str());
        });
    }

    // The navigation lock: the page stays on its own origin; a web or mail link the user clicked (or pressed
    // Enter on) is handed out, a script's `click()` on one opens nothing, anything else is refused, and so is
    // every new window.
    {
        let id = spec.id.clone();
        let open_outside = hooks.open_outside.clone();
        webview.connect_decide_policy(move |_, decision, kind| match kind {
            webkit6::PolicyDecisionType::NavigationAction => {
                let action = decision
                    .downcast_ref::<webkit6::NavigationPolicyDecision>()
                    .and_then(|d| d.navigation_action());
                let Some(action) = action else {
                    decision.ignore();
                    return true;
                };
                let uri = action
                    .request()
                    .and_then(|r| r.uri())
                    .map(|u| u.to_string())
                    .unwrap_or_default();
                match page_policy::navigation(&id, &uri) {
                    Navigation::Stay => false,
                    Navigation::Outside(url) => {
                        let link_clicked = action.navigation_type() == webkit6::NavigationType::LinkClicked;
                        // WebKit runs every script the host sends the page (`evaluate_javascript`) as a user
                        // gesture, and the page's promise continuations inherit it, so `is_user_gesture` alone
                        // passes a script's `click()` made after any host reply. A mouse button is reported only
                        // for a trusted click (the user's press, or Enter on a focused link, which WebKit turns
                        // into one), which no script can make.
                        let by_user = action.is_user_gesture() && action.mouse_button() != 0;
                        if page_policy::opens_outside(link_clicked, by_user) {
                            open_outside(&url);
                        }
                        decision.ignore();
                        true
                    }
                    Navigation::Refuse => {
                        decision.ignore();
                        true
                    }
                }
            }
            webkit6::PolicyDecisionType::NewWindowAction => {
                decision.ignore();
                true
            }
            _ => false,
        });
        webview.connect_create(|_, _| None);
    }

    {
        let on_committed = hooks.on_committed.clone();
        webview.connect_load_changed(move |_, event| {
            if event == webkit6::LoadEvent::Committed {
                on_committed();
            }
        });
    }

    // Before the first load, so the first document's first frame is already themed.
    apply_theme(&webview, theme);
    webview.load_uri(&page_policy::entry_uri(&spec.id, &spec.entry));
    webview
}

/// The page, for its host: the one place a script is run in it.
pub(crate) struct WebKitPluginPage(pub(crate) glib::WeakRef<WebView>);

impl PagePort for WebKitPluginPage {
    fn send(&self, message: &str) {
        let Some(webview) = self.0.upgrade() else { return };
        let script = format!(
            "window.__eitriPluginDispatch && window.__eitriPluginDispatch({});",
            serde_json::to_string(message).unwrap_or_default()
        );
        webview.evaluate_javascript(&script, None, None, None::<&gio::Cancellable>, |result| {
            if let Err(e) = result {
                eprintln!("[plugin] a message to the page was lost: {e}");
            }
        });
    }

    fn set_theme(&self, theme: &PageTheme) {
        let Some(webview) = self.0.upgrade() else { return };
        apply_theme(&webview, theme);
    }
}

/// A clicked link, given the same check as a reply link in the chat before the desktop opens it.
pub(crate) fn open_outside(url: &str) {
    let Some(url) = eitri_panel::agent_panel::clicked_link_url(url) else {
        eprintln!("[plugin] not opening {}", url.escape_debug());
        return;
    };
    let shown = url.to_owned();
    gtk4::UriLauncher::new(url).launch(None::<&gtk4::Window>, None::<&gio::Cancellable>, move |result| {
        if let Err(e) = result {
            eprintln!("[plugin] opening {shown}: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_embedded_kit_is_the_built_bundle() {
        // Named inside the test, not imported at module level: the `harness = false` target that includes this
        // file compiles the module without its tests, and an outer import would be unused there.
        use super::KIT_JS;
        assert!(!KIT_JS.is_empty());
        assert!(
            KIT_JS.starts_with("\"use strict\";\n(() => {"),
            "the bundle build.mjs emits is one IIFE, not a module"
        );
        assert!(
            KIT_JS.contains("eitriKeys"),
            "the bundle installs the page-facing global"
        );
        // The bundle is also copied on its own (a plugin vendors it for use outside Eitri), so the notice of the
        // code it adapts must be inside it, not only in the release's licence collection.
        assert!(KIT_JS.contains("Copyright (c) 2010 Phil Crosby, Ilya Sukhar."));
        assert!(KIT_JS.contains("The above copyright notice and this permission notice shall be included in"));
    }

    /// The `(` at `open` to its matching `)`, as a byte range of `code`.
    fn paren_span(code: &str, open: usize) -> std::ops::Range<usize> {
        let mut depth = 0usize;
        for (at, c) in code[open..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return open..open + at + 1;
                    }
                }
                _ => {}
            }
        }
        panic!("unbalanced parentheses from byte {open}");
    }

    /// Choosing a download's name stats the download directory, which can hang on a network mount, so the
    /// destination handler only ever asks for it on a worker thread.
    #[test]
    fn the_download_destination_is_chosen_off_the_gtk_thread() {
        use eitri_core::source_scan as scan;
        let code = scan::code_only(&scan::read_required("shell/src/plugin_page.rs").text);
        let handlers: Vec<&str> = code
            .match_indices("connect_decide_destination(")
            .map(|(at, _)| {
                let open = at + code[at..].find('{').expect("the handler has a body");
                scan::braced(&code, open)
            })
            .collect();
        assert_eq!(handlers.len(), 1, "one destination handler");
        let body = handlers[0];
        let workers: Vec<std::ops::Range<usize>> = body
            .match_indices("gio::spawn_blocking(")
            .map(|(at, m)| paren_span(body, at + m.len() - 1))
            .collect();
        let calls: Vec<usize> = body.match_indices("download_destination(").map(|(at, _)| at).collect();
        assert!(!calls.is_empty(), "the handler chooses the destination");
        for at in calls {
            assert!(
                workers.iter().any(|w| w.contains(&at)),
                "download_destination is called outside gio::spawn_blocking: {}",
                &body[at.saturating_sub(80)..(at + 40).min(body.len())]
            );
        }
    }
}
