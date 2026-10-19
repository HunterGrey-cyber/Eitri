//! A plugin's page in WebKitGTK: a web context of its own serving the plugin's scheme, a content security
//! policy floor the page cannot loosen, an ephemeral network session whose proxy lets through only the
//! origins the manifest declares, a navigation lock, and the bridge script. What a page may do is decided in
//! `eitri_core::plugin`; this file builds the view and carries text both ways. Its `PagePort` is the only
//! place a script is run in the page.

use std::path::{Path, PathBuf};
use std::rc::Rc;

use eitri_core::plugin::host::PagePort;
use eitri_core::plugin::manifest::Origin;
use eitri_core::plugin::page_policy::{self, Navigation, SiteRefusal};
use eitri_core::plugin::protocol::BRIDGE_JS;
use gtk4::gio;
use gtk4::gio::prelude::*;
use gtk4::glib;
use gtk4::prelude::*;
use webkit6::prelude::*;
use webkit6::{
    NetworkProxyMode, NetworkProxySettings, NetworkSession, URISchemeRequest, UserContentInjectedFrames,
    UserContentManager, UserScript, UserScriptInjectionTime, WebContext, WebView,
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
    /// A new document started loading: the host must hear `host.hello` again before it sends events.
    pub(crate) on_loading: Rc<dyn Fn()>,
    /// A clicked `http`, `https` or `mailto` link.
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

/// Builds the view and starts loading the plugin's entry.
pub(crate) fn build(spec: &PageSpec, hooks: PageHooks) -> WebView {
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
    // The downloads are the page's own `blob:` files, plus a same-origin `<a download>` or a site file the view
    // cannot show (WebKit turns those into downloads after the navigation lock has let them through). Whichever
    // starts one, only site files or the page's own data can be saved, and each goes under a safe name in the
    // download directory, never over an existing file.
    {
        let downloads = spec.downloads.clone();
        session.connect_download_started(move |_, download| {
            download.set_allow_overwrite(false);
            let downloads = downloads.clone();
            download.connect_decide_destination(move |download, suggested| {
                let target = page_policy::download_destination(&downloads, suggested);
                download.set_destination(&target.to_string_lossy());
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

    // The navigation lock: the page stays on its own origin; a clicked web or mail link is handed out, anything
    // else is refused, and so is every new window.
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
                        if action.navigation_type() == webkit6::NavigationType::LinkClicked {
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
        let on_loading = hooks.on_loading.clone();
        webview.connect_load_changed(move |_, event| {
            if event == webkit6::LoadEvent::Started {
                on_loading();
            }
        });
    }

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
}
