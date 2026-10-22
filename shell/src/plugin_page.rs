//! A plugin's page in WebKitGTK: a web context of its own serving the plugin's scheme, a content security
//! policy floor the page cannot loosen, an ephemeral network session whose proxy lets through only the
//! origins the manifest declares (and `https`, for a page that shows remote images), a navigation lock, and
//! the bridge script. What a page may do is decided in `eitri_core::plugin`; this file builds the view and
//! carries text both ways. Its `PagePort` is the only
//! place a script is run in the page, and `apply_theme` the only place the window's theme reaches the view.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use eitri_core::plugin::doc_route::{self, DocSlot};
use eitri_core::plugin::host::PagePort;
use eitri_core::plugin::manifest::Origin;
use eitri_core::plugin::page_policy::{self, Navigation, SiteRefusal};
use eitri_core::plugin::protocol::BRIDGE_JS;
use eitri_core::plugin::theme::PageTheme;
use eitri_core::plugin::trace;
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
    /// Some only for a plugin that declares `editor.follow`: the document's project directory its pages' images
    /// come from, set by the host.
    pub(crate) doc_slot: Option<Rc<RefCell<DocSlot>>>,
    /// The plugin shows remote `https` images: `img-src` gains `https:` and the session's proxy lets them out.
    pub(crate) remote_images: bool,
    /// A probe build with the plugin trace on: the page's console messages go to stdout, where a GUI pass reads
    /// them.
    pub(crate) console_to_stdout: bool,
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

/// What a request of the plugin's scheme asks for.
#[derive(Debug, PartialEq)]
enum Route {
    /// The keyboard kit, answered from memory.
    Kit,
    /// A file of the followed document's project, in the slot's current generation.
    Doc { dir: PathBuf, rel: String },
    /// A document-route path that names no current file: another generation, a path that is not clean, or no
    /// project known yet.
    NoDoc,
    /// A file of the plugin's own site.
    Site,
}

/// Which kind of path `uri` asks for: the kit, a document file of the slot's current generation, a document path
/// that names no current file (another generation, or no project yet), or a site file. Without a slot (a plugin
/// that does not follow a document) a document-route path is a site path like any other.
fn route(id: &str, uri: &str, doc_slot: Option<&DocSlot>) -> Route {
    if page_policy::kit_request(id, uri) {
        return Route::Kit;
    }
    let Some(slot) = doc_slot else {
        return Route::Site;
    };
    if !page_policy::decoded_path(id, uri).is_some_and(|p| p.starts_with(doc_route::DOC_PREFIX)) {
        return Route::Site;
    }
    match (doc_route::doc_request(id, uri), &slot.dir) {
        (Some((generation, rel)), Some(dir)) if generation == slot.generation => Route::Doc { dir: dir.clone(), rel },
        _ => Route::NoDoc,
    }
}

/// Serves one request of the plugin's scheme. Checking the path touches the disk (`canonicalize`, `metadata`),
/// which can stall on a slow or hung filesystem, so it runs on a worker thread; the request is answered back on
/// the main context, and the file itself is read without blocking this thread.
fn serve(request: &URISchemeRequest, site_root: &Path, id: &str, entry: &str, doc_slot: Option<&Rc<RefCell<DocSlot>>>) {
    let uri = request.uri().map(|u| u.to_string()).unwrap_or_default();
    if trace::enabled() {
        trace::line(id, "serve", &page_policy::decoded_path(id, &uri).unwrap_or_default());
    }
    // The kit is part of the host, not of the plugin's site: it is answered from memory, ahead of the site
    // lookup, so a site file of the same name can neither replace it nor be asked for through it. The
    // document route comes next, so a site file can never stand in for a document's image either.
    let route = match doc_slot.map(|slot| slot.try_borrow()) {
        None => route(id, &uri, None),
        Some(Ok(slot)) => route(id, &uri, Some(&slot)),
        // The slot is being written while this request arrives: no project is known for this request, so a
        // document path is a 404 rather than a panic inside the scheme handler.
        Some(Err(_)) => route(id, &uri, Some(&DocSlot::default())),
    };
    match route {
        Route::Kit => respond_static(request, KIT_JS, "text/javascript"),
        Route::NoDoc => not_found(request),
        Route::Doc { dir, rel } => {
            let request = request.clone();
            glib::spawn_future_local(async move {
                let checked = gio::spawn_blocking(move || doc_route::doc_file(&dir, &rel)).await;
                match checked {
                    Ok(Ok(file)) => read_file(&request, file),
                    // Every refusal reads as a missing file, so a page learns nothing about what lies outside
                    // its document's project, and a stale image never shows the "not built" page.
                    _ => not_found(&request),
                }
            });
        }
        Route::Site => {
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
    }
}

fn answer(
    request: &URISchemeRequest,
    checked: Result<page_policy::SiteFile, SiteRefusal>,
    id: &str,
    entry: &str,
    uri: &str,
) {
    match checked {
        Ok(file) => read_file(request, file),
        Err(SiteRefusal::NotFound) if page_policy::is_entry(id, entry, uri) => {
            respond_static(request, page_policy::NOT_BUILT_HTML, "text/html")
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

/// Answers with a checked file, read without blocking this thread.
fn read_file(request: &URISchemeRequest, file: page_policy::SiteFile) {
    let request = request.clone();
    let (mime, len) = (file.mime, i64::try_from(file.len).unwrap_or(-1));
    gio::File::for_path(&file.path).read_async(glib::Priority::DEFAULT, None::<&gio::Cancellable>, move |opened| {
        match opened {
            Ok(stream) => respond(&request, &stream, len, mime),
            Err(mut e) => request.finish_error(&mut e),
        }
    });
}

/// Answers with text the host holds in memory.
fn respond_static(request: &URISchemeRequest, text: &'static str, mime: &str) {
    let bytes = glib::Bytes::from_static(text.as_bytes());
    let stream = gio::MemoryInputStream::from_bytes(&bytes);
    respond(request, &stream, bytes.len() as i64, mime);
}

/// Every answer of the scheme goes out with its type and `X-Content-Type-Options: nosniff`. The origin serves
/// the plugin's site and, for a page that follows a document, image files of that document's project, which
/// anyone who wrote to the project chose the bytes of: the type the host decided is final, so an image can
/// never be taken for a script or a style sheet, even by a tag that the page's own policy would let load.
fn respond(request: &URISchemeRequest, stream: &impl IsA<gio::InputStream>, len: i64, mime: &str) {
    let response = webkit6::URISchemeResponse::new(stream, len);
    response.set_status(200, None);
    response.set_content_type(mime);
    // Headers given to the response replace the ones WebKit would derive, so the type goes in them too.
    let headers = webkit6::soup::MessageHeaders::new(webkit6::soup::MessageHeadersType::Response);
    headers.append("Content-Type", mime);
    headers.append("X-Content-Type-Options", "nosniff");
    response.set_http_headers(headers);
    request.finish_with_response(&response);
}

fn not_found(request: &URISchemeRequest) {
    let mut error = glib::Error::new(gio::IOErrorEnum::NotFound, SiteRefusal::NotFound.message());
    request.finish_error(&mut error);
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
        let doc_slot = spec.doc_slot.clone();
        context.register_uri_scheme(page_policy::SCHEME, move |request| {
            serve(request, &site_root, &id, &entry, doc_slot.as_ref())
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
    // a closed port, except the declared origins' hosts and, for a page that shows remote images, `https`.
    let session = NetworkSession::new_ephemeral();
    let plan = page_policy::proxy_plan(&spec.origins, spec.remote_images);
    if plan.backstop {
        let ignore: Vec<&str> = plan.ignore_hosts.iter().map(String::as_str).collect();
        let mut proxy = NetworkProxySettings::new(Some(page_policy::PROXY_BACKSTOP), &ignore);
        if plan.https_direct {
            proxy.add_proxy_for_scheme("https", "direct://");
        }
        session.set_proxy_settings(NetworkProxyMode::Custom, Some(&proxy));
    } else {
        session.set_proxy_settings(NetworkProxyMode::NoProxy, None);
    }
    println!("[plugin {}] proxy: {}", spec.id, plan.summary());
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

    let csp = page_policy::csp_floor(&spec.origins, spec.remote_images);
    println!("[plugin {}] csp: {}", spec.id, csp);
    let webview = WebView::builder()
        .web_context(&context)
        .user_content_manager(&content)
        .network_session(&session)
        .default_content_security_policy(csp)
        .build();
    if let Some(settings) = WebViewExt::settings(&webview) {
        settings.set_javascript_can_open_windows_automatically(false);
        settings.set_enable_developer_extras(false);
        if spec.console_to_stdout {
            settings.set_enable_write_console_messages_to_stdout(true);
        }
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
                            trace::line(&id, "outside", &url);
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

    #[test]
    fn the_kit_the_document_route_and_the_site_are_told_apart() {
        // Named inside the test for the same reason as above.
        use super::{route, Route};
        use eitri_core::plugin::doc_route::DocSlot;
        use std::path::PathBuf;

        let at = |path: &str| format!("eitri-plugin://demo/{path}");
        let slot = DocSlot {
            generation: 2,
            dir: Some(PathBuf::from("/p")),
        };
        let with = |path: &str| route("demo", &at(path), Some(&slot));
        assert_eq!(with(".eitri/keys.js"), Route::Kit);
        assert_eq!(
            with(".eitri/doc/2/a.png"),
            Route::Doc {
                dir: PathBuf::from("/p"),
                rel: "a.png".into()
            }
        );
        assert_eq!(
            with("%2Eeitri/doc/2/img/a%20b.png?v=1"),
            Route::Doc {
                dir: PathBuf::from("/p"),
                rel: "img/a b.png".into()
            }
        );
        assert_eq!(with(".eitri/doc/1/a.png"), Route::NoDoc);
        assert_eq!(with(".eitri/doc/3/a.png"), Route::NoDoc);
        assert_eq!(with(".eitri/doc/2/../a.png"), Route::NoDoc);
        assert_eq!(with(".eitri/doc/2/"), Route::NoDoc);
        assert_eq!(with("index.html"), Route::Site);
        assert_eq!(with("eitri/doc/2/a.png"), Route::Site);
        assert_eq!(
            route("demo", "eitri-plugin://other/.eitri/doc/2/a.png", Some(&slot)),
            Route::Site,
            "another origin is the site lookup's to refuse"
        );
        let empty = DocSlot {
            generation: 2,
            dir: None,
        };
        assert_eq!(route("demo", &at(".eitri/doc/2/a.png"), Some(&empty)), Route::NoDoc);
        assert_eq!(route("demo", &at(".eitri/doc/2/a.png"), None), Route::Site);
        assert_eq!(route("demo", &at(".eitri/keys.js"), None), Route::Kit);
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
