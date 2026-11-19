//! A diagnostic socket for the acceptance driver, compiled only with the `acceptance` feature.
//!
//! The driver checks what a person would see through the app's own state rather than through pixels: which
//! pane has the keys and whether AppKit's first responder agrees, the window and menu state, scripts run in
//! the page (its focus, its composer, a foreign script's fate), how each navigation was decided, and which
//! links the panel asked to open. Links are recorded and never opened in such a build.
//!
//! The socket is `a.sock` in a fresh 0700 directory under the temporary directory, named on stderr at start
//! (`[acceptance] control socket <path>`). Each connection is answered only for a peer running as this user,
//! one JSON request line and one JSON reply line: `{"cmd":"state"}`, `{"cmd":"js","script":"..."}` (the
//! script's value as text), `{"cmd":"focus","pane":"editor"|"panel"}`, `{"cmd":"activate"}` (bring the app
//! forward, for a frontmost check that found another app in front), `{"cmd":"menu","title":"Quit"}` (what a
//! click on that main-menu item does), `{"cmd":"resize","w":pt,"h":pt}` (the content size, as a drag of the window's
//! edge would leave it), `{"cmd":"zoom"}` (the prefix's zoom: the panel hidden, or shown again), `{"cmd":"beep"}`
//! (the system alert sound, once).

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use eitri_core::keymap::prefix::PrefixCommand;
use eitri_core::keymap::Action;
use eitri_panel::agent_panel::{AgentPanelHandle, ViewId};
use eitri_panel::panel_page::PanelHost;
use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_foundation::{NSError, NSString};
use serde_json::{json, Value};

use super::menu::edit_menu_present;
use super::page::{NavDecision, NavKind, PageEvents};
use super::window::{Host, Responder};
use crate::keys::Pane;

/// A string only an acceptance build carries: `bundle.sh` and the acceptance driver look for it in a binary to tell
/// such a build from a default one, and `tests/acceptance_marker.rs` pins it in all three places.
pub const BUILD_MARKER: &str = "eitri-acceptance-build-marker-1";

/// Kept in the binary whether or not anything reads it (`#[used]` survives the linker's dead stripping).
#[used]
static BUILD_MARKER_BYTES: [u8; 31] = *b"eitri-acceptance-build-marker-1";

/// The largest request line read.
const MAX_REQUEST: u64 = 1 << 20;
/// The socket's file name in its directory; the path is built by `agent::socket_path::in_dir`.
const SOCKET: &str = "a.sock";
/// How long a request waits for the main thread (a script for the page).
const ANSWER_WITHIN: Duration = Duration::from_secs(10);

thread_local! {
    static HOST: RefCell<Weak<Host>> = const { RefCell::new(Weak::new()) };
    static LINKS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    static NAVIGATIONS: RefCell<Vec<Value>> = const { RefCell::new(Vec::new()) };
}

/// The panel's process side in an acceptance build: a link is recorded, never opened.
pub struct RecordingHost;

impl PanelHost for RecordingHost {
    fn open_external(&self, url: &str) {
        crate::log_error!("[acceptance] open_external {url} (recorded, not opened)");
        LINKS.with(|links| links.borrow_mut().push(url.to_owned()));
    }
}

/// The panel's host to assemble with in an acceptance build.
pub fn panel_host() -> Rc<dyn PanelHost> {
    Rc::new(RecordingHost)
}

/// The page's reports go to the panel, and each navigation decision is recorded on the way.
struct RecordingEvents(AgentPanelHandle);

impl PageEvents for RecordingEvents {
    fn page_message(&self, raw: &str) {
        self.0.page_message(ViewId::BASE, raw);
    }
    fn link_clicked(&self, url: Option<&str>) {
        self.0.link_clicked(ViewId::BASE, url);
    }
    fn page_gone(&self, reason: &str) {
        self.0.page_gone(ViewId::BASE, reason);
    }
    fn page_shown(&self) {
        self.0.page_shown(ViewId::BASE);
    }
    fn navigation_decided(&self, kind: NavKind, url: Option<&str>, decision: &NavDecision) {
        let decision = match decision {
            NavDecision::Allow => "allow",
            NavDecision::LinkClick(_) => "link click, cancelled",
            NavDecision::Cancel => "cancel",
        };
        NAVIGATIONS.with(|n| {
            n.borrow_mut()
                .push(json!({"kind": format!("{kind:?}"), "url": url, "decision": decision}))
        });
    }
}

pub(super) fn recording_events(panel: AgentPanelHandle) -> Rc<dyn PageEvents> {
    Rc::new(RecordingEvents(panel))
}

/// Opens the socket and serves it on a thread of its own. A failure is reported and leaves the app running.
pub(super) fn start(host: Weak<Host>) {
    HOST.with(|h| *h.borrow_mut() = host);
    match bind() {
        Ok((path, listener)) => {
            crate::log_error!("[acceptance] control socket {}", path.display());
            let spawned = std::thread::Builder::new()
                .name("eitri-acceptance".into())
                .spawn(move || serve(listener));
            if let Err(e) = spawned {
                crate::log_error!("[acceptance] no control thread: {e}");
            }
        }
        Err(e) => crate::log_error!("[acceptance] no control socket: {e}"),
    }
}

fn bind() -> std::io::Result<(PathBuf, UnixListener)> {
    let dir = eitri_core::instance_dir::create_instance_dir(&std::env::temp_dir(), "nv-ac-", SOCKET, "eitri-mac")?;
    let path = agent::socket_path::in_dir(&dir, SOCKET)?;
    let listener = UnixListener::bind(&path)?;
    Ok((path, listener))
}

fn serve(listener: UnixListener) {
    // SAFETY: `geteuid` takes nothing and cannot fail.
    let euid = unsafe { libc::geteuid() };
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        match eitri_core::panel_control::peer_uid(&stream) {
            Ok(uid) if uid == euid => {}
            other => {
                crate::log_error!("[acceptance] a connection from another user was refused ({other:?})");
                continue;
            }
        }
        if let Err(e) = answer(stream) {
            crate::log_error!("[acceptance] request failed: {e}");
        }
    }
}

fn answer(stream: UnixStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut line = String::new();
    BufReader::new((&stream).take(MAX_REQUEST)).read_line(&mut line)?;
    let request: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
    let (reply_to, replies) = mpsc::channel::<Value>();
    dispatch2::DispatchQueue::main().exec_async(move || on_main(request, reply_to));
    let reply = replies
        .recv_timeout(ANSWER_WITHIN)
        .unwrap_or_else(|_| json!({"error": "no answer from the main thread"}));
    let mut stream = stream;
    writeln!(stream, "{reply}")
}

fn on_main(request: Value, reply_to: mpsc::Sender<Value>) {
    let Some(host) = HOST.with(|h| h.borrow().upgrade()) else {
        let _ = reply_to.send(json!({"error": "no window"}));
        return;
    };
    match request["cmd"].as_str() {
        Some("state") => {
            let _ = reply_to.send(state(&host));
        }
        Some("js") => {
            let script = request["script"].as_str().unwrap_or_default();
            evaluate(&host, script, reply_to);
        }
        Some("activate") => {
            // The call measured to bring an app forward from the background on this macOS (the plain
            // `activate` was not), for the driver's frontmost check to retry after another app took the front.
            let app = objc2_app_kit::NSApplication::sharedApplication(host.mtm());
            #[allow(deprecated)]
            app.activateIgnoringOtherApps(true);
            if let Some(window) = host.ns_window() {
                window.makeKeyAndOrderFront(None);
            }
            let _ = reply_to.send(state(&host));
        }
        Some("menu") => {
            // What a click on a main-menu item does: the item's menu performs its action. Finds the first item
            // whose title starts with `title`, in the main menu's submenus.
            let title = request["title"].as_str().unwrap_or_default().to_owned();
            let _ = reply_to.send(perform_menu_item(&host, &title));
        }
        Some("resize") => {
            // The window's content to `w` x `h` points, as a person's drag of its edge would leave it.
            let Some((w, h)) = resize_request(&request) else {
                let _ = reply_to.send(json!({"error": format!(
                    "resize needs w and h in points, each finite, above 0 and at most {MAX_RESIZE_PT}"
                )}));
                return;
            };
            if let Some(window) = host.ns_window() {
                window.setContentSize(objc2_foundation::NSSize::new(w, h));
            }
            let _ = reply_to.send(state(&host));
        }
        Some("zoom") => {
            // The prefix's zoom (`prefix z`): hides the panel, or shows it again, through the assembly's own path.
            if let Some(assembly) = host.assembly() {
                assembly.run_prefix(PrefixCommand::Run(Action::Zoom), host.pane());
            }
            let _ = reply_to.send(state(&host));
        }
        Some("editor_ic") => {
            let done = host.probe_editor_input_context(request["op"].as_str().unwrap_or_default());
            let _ = reply_to.send(json!({"done": done}));
        }
        Some("beep") => {
            // The alert sound AppKit plays at the end of an unhandled key's walk: the driver's check that its
            // recorder hears this process at all, before it counts silence as a pass.
            objc2_app_kit::NSBeep();
            let _ = reply_to.send(json!({"beeped": true}));
        }
        Some("focus") => {
            let pane = match request["pane"].as_str() {
                Some("editor") => Pane::Editor,
                Some("panel") => Pane::Panel,
                _ => {
                    let _ = reply_to.send(json!({"error": "pane is editor or panel"}));
                    return;
                }
            };
            host.focus_pane(pane);
            let _ = reply_to.send(state(&host));
        }
        _ => {
            let _ = reply_to.send(json!({"error": "unknown request", "request": request}));
        }
    }
}

/// The largest content size `resize` asks AppKit for, in points.
const MAX_RESIZE_PT: f64 = 10_000.0;

/// `resize`'s `w` and `h`, or `None` for a size AppKit must not be handed (not a number, infinite, zero or below,
/// or past `MAX_RESIZE_PT`): an exception from `setContentSize:` would abort the app inside a dispatch block.
/// Sizes under the window's minimum are allowed on purpose: what the window makes of them is under test.
fn resize_request(request: &Value) -> Option<(f64, f64)> {
    let fits = |v: f64| v.is_finite() && v > 0.0 && v <= MAX_RESIZE_PT;
    let (w, h) = (request["w"].as_f64()?, request["h"].as_f64()?);
    (fits(w) && fits(h)).then_some((w, h))
}

fn editor_input_context(host: &Host) -> Value {
    let (source, current) = host.editor_input_context();
    let (loaded, settled, log) = host.input_method_state();
    json!({"source": source, "current": current, "page_loaded": loaded, "settled": settled, "log": log})
}

fn state(host: &Host) -> Value {
    let responder = host.responder();
    let pane = host.pane();
    let web = host.panel_view().view();
    let window = host.ns_window();
    // In two parts, merged: one `json!` with every field is past the macro's recursion limit.
    let mut state = json!({
        "window_number": window.as_ref().map(|w| w.windowNumber()),
        // The title bar as the window frame's height minus its content layout rect's (points), never assumed.
        "title_bar": window.as_ref().map(|w| w.frame().size.height - w.contentLayoutRect().size.height),
        "backing_scale": window.as_ref().map(|w| w.backingScaleFactor()),
        "editor_screen": window.as_ref().and_then(|w| w.contentView()).map(|v| screen_rect(&v, host.mtm())),
        "web_screen": screen_rect(web, host.mtm()),
        "pane": format!("{pane:?}"),
        "first_responder": format!("{responder:?}"),
        "agree": super::window::responder_agrees(pane, responder),
        "panel_gate_editor_has_keys": host.panel_view().gate().editor_has_keys(),
        "window_key": host.is_key(),
        "full_screen": host.is_full_screen(),
        "panel_shown": !web.isHidden(),
        // The web view's next responder: the sink, never the editor's view, or a key the page leaves goes there;
        // and the sink's: the window.
        // SAFETY: plain getters on the main thread.
        "web_next_responder": unsafe { web.nextResponder() }.map(|r| r.class().name().to_string_lossy().into_owned()),
        "web_next_next_responder": unsafe { web.nextResponder().and_then(|r| r.nextResponder()) }
            .map(|r| r.class().name().to_string_lossy().into_owned()),
        "sink_stopped": host.panel_view().key_sink().stopped(),
        "hidden_by_user": host.hidden_by_user(),
        "frames": host.frames(),
        "edit_menu": edit_menu_present(host.mtm()),
        "subtitle": host.subtitle(),
        "editor_marked_text": host.editor_marked_text(),
        "editor_input_context": editor_input_context(host),
        "nvim_listen": host.assembly().map(|a| a.editor.nvim_listen().display().to_string()),
        "links": LINKS.with(|l| l.borrow().clone()),
        "navigations": NAVIGATIONS.with(|n| n.borrow().clone()),
    });
    let rest = json!({
        "pid": std::process::id(),
        "editor_link": host.assembly().and_then(|a| a.editor_link()).map(|(state, text)| json!({"state": state, "text": text})),
        "attach": host.life.attach_phase(),
        "background": background(host),
        "edit_menu_insertions": host.life.menu_insertions(),
        "new_window_items_removed": host.life.new_window_items_removed(),
        "quit_items_retargeted": host.life.quit_items_retargeted(),
        "extra_windows": host.extra_windows(),
        "reconciles": host.reconciles().into_iter().map(|(why, n)| json!({"why": why, "count": n})).collect::<Vec<_>>(),
        "quit_log": host.life.quit_log(),
        "quit_asking": host.life.quit_asking(),
        "finished_via": host.life.finished_via(),
        "editor_ended": host.life.editor_ended(),
        "editor_ends": host.life.editor_ends(),
        "restarts": host.life.restarts(),
        "running": host.assembly().map(|a| a.panel.running_count()),
        "queued": host.assembly().map(|a| a.panel.queued_count()),
        "responder_is_none": responder == Responder::None,
    });
    if let (Some(state), Value::Object(rest)) = (state.as_object_mut(), rest) {
        state.extend(rest);
    }
    state
}

fn perform_menu_item(host: &Host, title: &str) -> Value {
    let Some(main) = objc2_app_kit::NSApplication::sharedApplication(host.mtm()).mainMenu() else {
        return json!({"found": false, "why": "no main menu"});
    };
    for top in main.itemArray().iter() {
        let Some(menu) = top.submenu() else { continue };
        for (index, item) in menu.itemArray().iter().enumerate() {
            if !title.is_empty() && item.title().to_string().starts_with(title) {
                menu.performActionForItemAtIndex(index as isize);
                return json!({"found": true, "menu": top.title().to_string(), "item": item.title().to_string()});
            }
        }
    }
    json!({"found": false, "why": format!("no item titled {title:?}")})
}

/// Where `view` is on screen, in the window server's global coordinates (points, origin at the top left of the
/// first screen), the ones synthetic mouse events use. `null` while it is in no window.
fn screen_rect(view: &objc2_app_kit::NSView, mtm: objc2::MainThreadMarker) -> Value {
    let Some(window) = view.window() else {
        return Value::Null;
    };
    let rect = window.convertRectToScreen(view.convertRect_toView(view.bounds(), None));
    let Some(first) = objc2_app_kit::NSScreen::screens(mtm).firstObject() else {
        return Value::Null;
    };
    let height = first.frame().size.height;
    json!({
        "x": rect.origin.x,
        "y": height - (rect.origin.y + rect.size.height),
        "w": rect.size.width,
        "h": rect.size.height,
        "hidden": view.isHiddenOrHasHiddenAncestor(),
    })
}

/// The colour behind the page, as AppKit describes it.
fn background(host: &Host) -> String {
    // SAFETY: plain getters on the main thread.
    unsafe {
        let colour = host.panel_view().view().underPageBackgroundColor();
        let description: Retained<NSString> = msg_send![&*colour, description];
        description.to_string()
    }
}

fn evaluate(host: &Host, script: &str, reply_to: mpsc::Sender<Value>) {
    let done = RcBlock::new(move |result: *mut AnyObject, error: *mut NSError| {
        // SAFETY: WebKit passes valid objects or null, alive for the length of this call.
        let reply = unsafe {
            if let Some(error) = error.as_ref() {
                json!({"error": error.to_string()})
            } else if let Some(result) = result.as_ref() {
                let text = match result.downcast_ref::<NSString>() {
                    Some(s) => s.to_string(),
                    None => {
                        let description: Retained<NSString> = msg_send![result, description];
                        description.to_string()
                    }
                };
                json!({"value": text})
            } else {
                json!({"value": null})
            }
        };
        let _ = reply_to.send(reply);
    });
    // SAFETY: asynchronous by contract; the completion runs on a later turn of the main run loop.
    unsafe {
        host.panel_view()
            .view()
            .evaluateJavaScript_completionHandler(&NSString::from_str(script), Some(&done));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resize_takes_only_sizes_appkit_can_be_given() {
        assert_eq!(resize_request(&json!({"w": 1200.0, "h": 711})), Some((1200.0, 711.0)));
        // Under the window's minimum: allowed, the window's answer is what is checked.
        assert_eq!(resize_request(&json!({"w": 150, "h": 200})), Some((150.0, 200.0)));
        assert_eq!(
            resize_request(&json!({"w": 10000, "h": 10000})),
            Some((10000.0, 10000.0))
        );
        for bad in [
            json!({"w": 0, "h": 500}),
            json!({"w": -1, "h": 500}),
            json!({"w": 800, "h": 10001}),
            json!({"w": 1e300, "h": 500}),
            json!({"w": "800", "h": 500}),
            json!({"w": 800}),
            json!({}),
        ] {
            assert_eq!(resize_request(&bad), None, "{bad}");
        }
    }
}
