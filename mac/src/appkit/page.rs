//! The agent panel's page in a WKWebView: the document, the view and its delegate, the `PageSurface` and
//! `PanelHost` the panel's logic drives, and the run-loop timer [`repeat`].
//!
//! The order a window follows: [`PanelView::new`] builds the view (no document yet), the window adds
//! [`PanelView::view`] beside the editor, `assembly::assemble` builds the panel with [`PanelView::page`] and
//! [`MacPanelHost`], then [`PanelView::connect`] routes the view's reports to that panel and loads the first
//! document, and `super::tick::PanelTicker` and `super::editor_tick::EditorTicker` drive it.
//!
//! **The page is never allowed to navigate.** It holds the `eitriAgent` bridge, which answers permission
//! cards, so any other document in this view would inherit it. The delegate allows only the loads
//! `load_document` itself asked for, hands a link click to the panel (which opens a web page or a mail
//! draft in the system's handler) and cancels it, and cancels every other navigation: a form, a script's
//! `location`, a reload, a frame, a new window. The document's own content security policy, which pins its
//! script by hash, is in the `<meta>` the panel writes first in `<head>`.

use std::cell::{Cell, RefCell};
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::LazyLock;
use std::time::Duration;

use block2::RcBlock;
use eitri_panel::agent_panel::{AgentPanelHandle, ViewId};
use eitri_panel::panel_document::PanelDocument;
use eitri_panel::panel_page::{PageSurface, PanelPage};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObject, ProtocolObject, Sel};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSColor, NSEvent, NSResponder, NSView, NSWindowOcclusionState};
use objc2_foundation::{
    NSArray, NSError, NSObjectProtocol, NSRect, NSRunLoop, NSRunLoopCommonModes, NSString, NSTimer, NSURL,
};
use objc2_web_kit::{
    WKNavigation, WKNavigationAction, WKNavigationActionPolicy, WKNavigationDelegate, WKNavigationType,
    WKScriptMessage, WKScriptMessageHandler, WKUserContentController, WKWebView, WKWebViewConfiguration,
    WKWebsiteDataStore,
};

/// The embedded, single-file `agent-ui/web` production build; `build.rs` builds it before this crate
/// compiles on macOS. Nothing is read from the app bundle at run time.
static AGENT_UI_HTML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../agent-ui/web/dist/index.html"));

/// The bundle with its content security policy, computed once on first use from the bundle embedded above,
/// so the policy's script hash always describes the script that is shipped.
pub static PANEL_DOCUMENT: LazyLock<PanelDocument> = LazyLock::new(|| PanelDocument::new(AGENT_UI_HTML));

/// The name of the page's message handler (`window.webkit.messageHandlers.eitriAgent`).
pub const MESSAGE_HANDLER: &str = "eitriAgent";

/// What the view reports, and to whom: the panel in the app, a recorder in a test.
pub trait PageEvents {
    /// A message the page posted to [`MESSAGE_HANDLER`].
    fn page_message(&self, raw: &str);
    /// A link in the page was clicked; the navigation itself is cancelled.
    fn link_clicked(&self, url: Option<&str>);
    /// The web content process ended.
    fn page_gone(&self, reason: &str);
    /// The view came on screen.
    fn page_shown(&self);
    /// How a navigation was decided, before the decision is acted on. Nothing in the app needs it.
    fn navigation_decided(&self, _kind: NavKind, _url: Option<&str>, _decision: &NavDecision) {}
}

impl PageEvents for AgentPanelHandle {
    fn page_message(&self, raw: &str) {
        AgentPanelHandle::page_message(self, ViewId::BASE, raw);
    }
    fn link_clicked(&self, url: Option<&str>) {
        AgentPanelHandle::link_clicked(self, ViewId::BASE, url);
    }
    fn page_gone(&self, reason: &str) {
        AgentPanelHandle::page_gone(self, ViewId::BASE, reason);
    }
    fn page_shown(&self) {
        AgentPanelHandle::page_shown(self, ViewId::BASE);
    }
}

/// What the view's callbacks share: where reports go, and the document loads still to be let through.
#[derive(Default)]
struct Bridge {
    events: RefCell<Option<Rc<dyn PageEvents>>>,
    awaited_loads: RefCell<Vec<String>>,
    /// The navigation of the newest `load_document`. Once it has finished or failed nothing newer is waiting,
    /// so whatever is still awaited belongs to a load WebKit dropped and must not let a script's `location`
    /// through.
    latest_load: RefCell<Option<Retained<WKNavigation>>>,
    /// Set for good by `PanelView::disconnect`: from then on every navigation is cancelled, a link click
    /// included, and nothing reaches the panel.
    disconnected: Cell<bool>,
    /// Told each time the newest load has ended, finished or failed.
    on_loaded: RefCell<Option<Rc<dyn Fn()>>>,
    #[cfg(any(test, debug_assertions, feature = "test-support"))]
    navigation_cancellations: Cell<u64>,
}

impl Bridge {
    /// `navigation` ended; when it is the newest load, no other load is awaited any more.
    fn load_ended(&self, navigation: Option<&WKNavigation>) {
        let latest = self.latest_load.borrow().clone();
        if let (Some(ended), Some(latest)) = (navigation, latest) {
            if std::ptr::eq(ended, &*latest) {
                self.newest_load_ended();
            }
        }
    }

    /// Nothing newer than the newest load can be waiting: whatever is still awaited was dropped by WebKit.
    fn newest_load_ended(&self) {
        self.awaited_loads.borrow_mut().clear();
        *self.latest_load.borrow_mut() = None;
        // Cloned out first: what it runs may load a document, which comes back here.
        let on_loaded = self.on_loaded.borrow().clone();
        if let Some(on_loaded) = on_loaded {
            on_loaded();
        }
    }

    /// Cloned out of the cell, so no borrow is held while the panel runs (it may load a document, which
    /// comes back here).
    fn events(&self) -> Option<Rc<dyn PageEvents>> {
        self.events.borrow().clone()
    }
}

/// Whether the web view may take the window's keys. The window sets it whenever the keys move; the view
/// asks it when AppKit, or the page's own `element.focus()`, wants to make it first responder.
pub struct KeyGate {
    editor_has_keys: Cell<bool>,
    on_refused: RefCell<Option<Rc<dyn Fn()>>>,
}

impl KeyGate {
    fn new() -> KeyGate {
        // The editor starts with the keys: the page focuses its composer as it loads, and that must not take
        // them.
        KeyGate {
            editor_has_keys: Cell::new(true),
            on_refused: RefCell::new(None),
        }
    }

    /// Set before the window moves the keys: to `false` before it makes the view first responder, to `true`
    /// before it gives them to the editor.
    pub fn set_editor_has_keys(&self, editor: bool) {
        self.editor_has_keys.set(editor);
    }

    pub fn editor_has_keys(&self) -> bool {
        self.editor_has_keys.get()
    }

    /// Called after a refusal, on a later turn of the run loop: AppKit has already asked the editor to give up
    /// the keys by then, so the window gives them back to it.
    pub fn on_refused(&self, f: impl Fn() + 'static) {
        *self.on_refused.borrow_mut() = Some(Rc::new(f));
    }
}

/// Whether the view may become first responder. A click on the panel moves the keys before AppKit routes it,
/// so it needs no exception here.
pub fn may_take_keys(editor_has_keys: bool) -> bool {
    !editor_has_keys
}

/// The page counts as on screen when the view and its ancestors are not hidden and it is in a window that is
/// itself on screen: not minimized, and not wholly covered or on another Space.
pub fn on_screen(hidden_or_ancestor_hidden: bool, window_on_screen: bool) -> bool {
    !hidden_or_ancestor_hidden && window_on_screen
}

/// Whether the document may be loaded: one that holds a script must start its `<head>` with the content
/// security policy, as the panel's themed document does. Without that policy nothing else in this view
/// restricts what the page's script may load or run, so such a document is refused rather than shown.
pub fn guarded_document(html: &str) -> bool {
    if !html.to_ascii_lowercase().contains("<script") {
        return true;
    }
    html.find("<head>").is_some_and(|at| {
        html[at + "<head>".len()..].starts_with("<meta http-equiv=\"Content-Security-Policy\" content=\"")
    })
}

/// The script that hands one envelope to the page. The envelope travels as a JSON string literal, so no
/// character in it can end the literal or run as code. The trailing `true` gives the script a value WebKit
/// can always hand back, whatever the page's function returns: one it cannot convert (a function, a DOM
/// node) would make the completion report an error for an envelope that was delivered.
pub fn dispatch_script(envelope_json: &str) -> String {
    let literal = serde_json::to_string(envelope_json).unwrap_or_else(|_| "\"\"".to_owned());
    format!("(window.__eitriDispatch && window.__eitriDispatch({literal}), true)")
}

/// What started a navigation, as WebKit reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavKind {
    LinkActivated,
    FormSubmitted,
    FormResubmitted,
    BackForward,
    Reload,
    Other,
    Unknown(isize),
}

impl NavKind {
    fn of(kind: WKNavigationType) -> NavKind {
        match kind {
            WKNavigationType::LinkActivated => NavKind::LinkActivated,
            WKNavigationType::FormSubmitted => NavKind::FormSubmitted,
            WKNavigationType::FormResubmitted => NavKind::FormResubmitted,
            WKNavigationType::BackForward => NavKind::BackForward,
            WKNavigationType::Reload => NavKind::Reload,
            WKNavigationType::Other => NavKind::Other,
            other => NavKind::Unknown(other.0),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NavDecision {
    /// The document `load_document` asked for.
    Allow,
    /// A click on a link: the panel decides whether to open it elsewhere; the view stays where it is.
    LinkClick(Option<String>),
    Cancel,
}

/// At most this many loads wait for their navigation. `load_document` is called once at start and once per
/// reload, so more than this means WebKit dropped the earlier ones without asking.
const MAX_AWAITED_LOADS: usize = 4;

/// Decides one navigation. `awaited` holds the base address of every `load_document` whose navigation has not
/// been seen yet; a main-frame load of one of them, started by the host (WebKit reports such a load as
/// `Other`), consumes it and is the only navigation allowed. A script assigning that same address while no
/// load waits is cancelled like any other.
pub fn decide_navigation(kind: NavKind, main_frame: bool, url: Option<&str>, awaited: &mut Vec<String>) -> NavDecision {
    match kind {
        NavKind::LinkActivated => NavDecision::LinkClick(url.map(str::to_owned)),
        NavKind::Other if main_frame => match url.and_then(|url| awaited.iter().position(|base| base == url)) {
            Some(at) => {
                awaited.remove(at);
                NavDecision::Allow
            }
            None => NavDecision::Cancel,
        },
        _ => NavDecision::Cancel,
    }
}

/// An sRGB colour's components for `[r, g, b]` bytes.
fn srgb_components([r, g, b]: [u8; 3]) -> [f64; 3] {
    [f64::from(r) / 255.0, f64::from(g) / 255.0, f64::from(b) / 255.0]
}

/// The web view's instance state.
pub struct ViewIvars {
    gate: Rc<KeyGate>,
    bridge: Rc<Bridge>,
    sink: Retained<KeySink>,
}

/// How many keys and text commands the sink has stopped.
#[derive(Default)]
pub struct SinkCounts {
    keys: Cell<u64>,
    commands: Cell<u64>,
}

define_class!(
    /// The web view's next responder while it is in a window: a key or text command the page left unhandled
    /// stops here. Past it, the window would take Tab and Shift+Tab for its own key-view loop (AppKit's first
    /// responder leaving the page while the panel still has the keys) and end any other key's walk with the
    /// alert sound, which an unhandled letter in the panel played. Everything else (the pointer, actions, the
    /// window's own commands) goes on to the window, its own next responder.
    #[unsafe(super(NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "EitriPanelKeySink"]
    #[ivars = SinkCounts]
    pub struct KeySink;

    impl KeySink {
        #[unsafe(method(keyDown:))]
        fn key_down(&self, _event: &NSEvent) {
            self.ivars().keys.set(self.ivars().keys.get() + 1);
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, _event: &NSEvent) {}

        #[unsafe(method(doCommandBySelector:))]
        fn do_command_by_selector(&self, _command: Sel) {
            self.ivars().commands.set(self.ivars().commands.get() + 1);
        }
    }
);

impl KeySink {
    pub fn new(mtm: MainThreadMarker) -> Retained<KeySink> {
        let this = KeySink::alloc(mtm).set_ivars(SinkCounts::default());
        // SAFETY: NSResponder's `init` on a freshly allocated object.
        unsafe { msg_send![super(this), init] }
    }

    /// The keys and the text commands stopped so far.
    pub fn stopped(&self) -> (u64, u64) {
        (self.ivars().keys.get(), self.ivars().commands.get())
    }
}

define_class!(
    /// A WKWebView that takes the window's keys only when it has been given them, leaves the pointer to the
    /// page, and stops what the page leaves unhandled before the window, never passing it to the editor's view.
    #[unsafe(super(WKWebView, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "EitriPanelWebView"]
    #[ivars = ViewIvars]
    pub struct GuardedWebView;

    impl GuardedWebView {
        /// The page's own `element.focus()` makes WebKit ask for first responder, at load and later; granted
        /// while the editor holds the keys, the editor's keys would land in the page.
        #[unsafe(method(becomeFirstResponder))]
        fn become_first_responder(&self) -> Bool {
            let gate = &self.ivars().gate;
            if !may_take_keys(gate.editor_has_keys.get()) {
                // AppKit has already asked the editor to resign, which would leave the window holding the
                // keys; the window gives them back once this call has returned.
                // In the run loop's common modes, so a refusal during a live resize or an open menu is answered
                // then too, not only once the tracking ends.
                // SAFETY: a plain delayed message to this object, which the run loop retains until it is sent.
                unsafe {
                    let modes = NSArray::from_slice(&[NSRunLoopCommonModes]);
                    let _: () = msg_send![
                        self,
                        performSelector: sel!(eitriKeysRefused),
                        withObject: None::<&AnyObject>,
                        afterDelay: 0.0f64,
                        inModes: &*modes
                    ];
                }
                return Bool::NO;
            }
            // SAFETY: the superclass's own implementation, with its own signature.
            unsafe { msg_send![super(self), becomeFirstResponder] }
        }

        #[unsafe(method(eitriKeysRefused))]
        fn keys_refused(&self) {
            let refused = self.ivars().gate.on_refused.borrow().clone();
            if let Some(refused) = refused {
                refused();
            }
        }

        /// WebKit sets the pointer itself as it moves over the page and does not handle `cursorUpdate:`;
        /// passed up the responder chain, the event reached the editor's view, which put its own cursor back
        /// over the page.
        #[unsafe(method(cursorUpdate:))]
        fn cursor_update(&self, _event: &NSEvent) {}

        /// WebKit sends a key the page left unhandled, and its text commands (`noop:` for a Cmd chord), up the
        /// view's responder chain. The editor's view, this view's superview, would take them: an unhandled
        /// letter typed in the panel reached nvim, and a Cmd chord the page ignored counted as handled and
        /// never reached the main menu. While the view is in a window, its next responder is the sink, and the
        /// sink's is the window. This is answered here, not set: AppKit puts the superview back as the next
        /// responder after the moves a setter would follow, and the editor's view was found there again with
        /// the panel holding the keys.
        #[unsafe(method_id(nextResponder))]
        fn next_responder(&self) -> Option<Retained<NSResponder>> {
            match self.window() {
                Some(window) => {
                    let sink = &self.ivars().sink;
                    // SAFETY: a plain setter on the main thread; the window outlives its views' use of it.
                    unsafe { sink.setNextResponder(Some(&window)) };
                    Some(Retained::into_super(sink.clone()))
                }
                // SAFETY: the superclass's own implementation, with its own signature.
                None => unsafe { msg_send![super(self), nextResponder] },
            }
        }

        #[unsafe(method(viewDidMoveToWindow))]
        fn view_did_move_to_window(&self) {
            // SAFETY: the superclass's own implementation, with its own signature.
            let _: () = unsafe { msg_send![super(self), viewDidMoveToWindow] };
            self.report_if_shown();
        }

        #[unsafe(method(viewDidUnhide))]
        fn view_did_unhide(&self) {
            // SAFETY: as above.
            let _: () = unsafe { msg_send![super(self), viewDidUnhide] };
            self.report_if_shown();
        }
    }
);

impl GuardedWebView {
    fn is_on_screen(&self) -> bool {
        let window_on_screen = self
            .window()
            .is_some_and(|w| w.isVisible() && w.occlusionState().contains(NSWindowOcclusionState::Visible));
        on_screen(self.isHiddenOrHasHiddenAncestor(), window_on_screen)
    }

    fn report_if_shown(&self) {
        if !self.is_on_screen() {
            return;
        }
        if let Some(events) = self.ivars().bridge.events() {
            events.page_shown();
        }
    }
}

define_class!(
    /// The page's message handler and the view's navigation delegate.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "EitriPanelDelegate"]
    #[ivars = Rc<Bridge>]
    struct PanelDelegate;

    unsafe impl NSObjectProtocol for PanelDelegate {}

    unsafe impl WKScriptMessageHandler for PanelDelegate {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn did_receive_script_message(&self, _controller: &WKUserContentController, message: &WKScriptMessage) {
            // SAFETY: plain getters of a message WebKit hands over for the length of this call.
            let (name, body, main_frame) =
                unsafe { (message.name(), message.body(), message.frameInfo().isMainFrame()) };
            if name.to_string() != MESSAGE_HANDLER || !main_frame {
                return;
            }
            let Some(body) = body.downcast_ref::<NSString>() else {
                crate::log_error!("[agent_panel] a page message that is not a string was dropped");
                return;
            };
            let raw = body.to_string();
            // Primary-selection copies and first-paint notifications both stop at the host. The panel's
            // parser knows neither kind and would log them whole, copied text included.
            if eitri_core::agent_bridge::is_primary_copy(&raw) || eitri_core::agent_bridge::is_first_paint(&raw) {
                return;
            }
            if let Some(events) = self.ivars().events() {
                events.page_message(&raw);
            }
        }
    }

    unsafe impl WKNavigationDelegate for PanelDelegate {
        #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
        fn decide_policy_for_navigation_action(
            &self,
            _web_view: &WKWebView,
            action: &WKNavigationAction,
            decision_handler: &block2::DynBlock<dyn Fn(WKNavigationActionPolicy)>,
        ) {
            // SAFETY: plain getters of an action WebKit hands over for the length of this call.
            let (kind, main_frame, url) = unsafe {
                (
                    NavKind::of(action.navigationType()),
                    action.targetFrame().is_some_and(|frame| frame.isMainFrame()),
                    action.request().URL().and_then(|url| url.absoluteString()).map(|s| s.to_string()),
                )
            };
            if self.ivars().disconnected.get() {
                self.answer_navigation(decision_handler, WKNavigationActionPolicy::Cancel);
                return;
            }
            let decision = {
                let mut awaited = self.ivars().awaited_loads.borrow_mut();
                decide_navigation(kind, main_frame, url.as_deref(), &mut awaited)
            };
            let events = self.ivars().events();
            if let Some(events) = &events {
                events.navigation_decided(kind, url.as_deref(), &decision);
            }
            match decision {
                NavDecision::Allow => self.answer_navigation(decision_handler, WKNavigationActionPolicy::Allow),
                NavDecision::Cancel => self.answer_navigation(decision_handler, WKNavigationActionPolicy::Cancel),
                NavDecision::LinkClick(url) => {
                    // Answered first: the panel may open the link in another app, and WebKit need not wait.
                    self.answer_navigation(decision_handler, WKNavigationActionPolicy::Cancel);
                    if let Some(events) = events {
                        events.link_clicked(url.as_deref());
                    }
                }
            }
        }

        #[unsafe(method(webView:didFinishNavigation:))]
        fn did_finish_navigation(&self, _web_view: &WKWebView, navigation: Option<&WKNavigation>) {
            self.ivars().load_ended(navigation);
        }

        #[unsafe(method(webView:didFailNavigation:withError:))]
        fn did_fail_navigation(&self, _web_view: &WKWebView, navigation: Option<&WKNavigation>, _error: &NSError) {
            self.ivars().load_ended(navigation);
        }

        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn did_fail_provisional_navigation(
            &self,
            _web_view: &WKWebView,
            navigation: Option<&WKNavigation>,
            _error: &NSError,
        ) {
            self.ivars().load_ended(navigation);
        }

        #[unsafe(method(webViewWebContentProcessDidTerminate:))]
        fn web_content_process_did_terminate(&self, _web_view: &WKWebView) {
            // No load survives its process: what was awaited would otherwise stay allowed until the next load
            // finishes.
            self.ivars().awaited_loads.borrow_mut().clear();
            *self.ivars().latest_load.borrow_mut() = None;
            if let Some(events) = self.ivars().events() {
                events.page_gone("web content process terminated");
            }
        }
    }
);

impl PanelDelegate {
    fn answer_navigation(
        &self,
        handler: &block2::DynBlock<dyn Fn(WKNavigationActionPolicy)>,
        policy: WKNavigationActionPolicy,
    ) {
        #[cfg(any(test, debug_assertions, feature = "test-support"))]
        if policy == WKNavigationActionPolicy::Cancel {
            self.ivars()
                .navigation_cancellations
                .set(self.ivars().navigation_cancellations.get() + 1);
        }
        handler.call((policy,));
    }
}

/// The view, and what the window and the panel need of it.
pub struct PanelView {
    view: Retained<GuardedWebView>,
    gate: Rc<KeyGate>,
    page: Rc<PanelPage>,
    bridge: Rc<Bridge>,
}

impl PanelView {
    /// Builds the web view at `frame`, with no document: a non-persistent data store (nothing the page keeps
    /// reaches the disk), the page's message handler and the navigation delegate.
    pub fn new(mtm: MainThreadMarker, frame: NSRect) -> PanelView {
        let bridge = Rc::new(Bridge::default());
        let gate = Rc::new(KeyGate::new());
        let delegate: Retained<PanelDelegate> = {
            let this = PanelDelegate::alloc(mtm).set_ivars(bridge.clone());
            // SAFETY: NSObject's `init` on a freshly allocated object.
            unsafe { msg_send![super(this), init] }
        };
        // SAFETY: WebKit's documented set-up calls, on the main thread, with objects made here.
        let view: Retained<GuardedWebView> = unsafe {
            let content = WKUserContentController::new(mtm);
            // The controller keeps the handler alive.
            content.addScriptMessageHandler_name(
                ProtocolObject::from_ref(&*delegate),
                &NSString::from_str(MESSAGE_HANDLER),
            );
            let config = WKWebViewConfiguration::new(mtm);
            config.setUserContentController(&content);
            config.setWebsiteDataStore(&WKWebsiteDataStore::nonPersistentDataStore(mtm));
            let this = GuardedWebView::alloc(mtm).set_ivars(ViewIvars {
                gate: gate.clone(),
                bridge: bridge.clone(),
                sink: KeySink::new(mtm),
            });
            let view: Retained<GuardedWebView> = msg_send![super(this), initWithFrame: frame, configuration: &*config];
            // Not retained by the view; `WkPage` keeps the delegate for as long as the page exists.
            view.setNavigationDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            // A force click on a link would otherwise open WebKit's own preview of it: a fetch of the address
            // outside the navigation delegate and the panel's choice of what a link may open.
            view.setAllowsLinkPreview(false);
            view
        };
        let surface = WkPage {
            view: view.clone(),
            bridge: bridge.clone(),
            _delegate: delegate,
        };
        let page = Rc::new(PanelPage::new(Rc::new(surface)));
        PanelView {
            view,
            gate,
            page,
            bridge,
        }
    }

    /// The view to add beside the editor.
    pub fn view(&self) -> &GuardedWebView {
        &self.view
    }

    /// Where the page's unhandled keys stop (the view's next responder while it is in a window).
    pub fn key_sink(&self) -> &KeySink {
        &self.view.ivars().sink
    }

    /// How many navigation actions the delegate has cancelled, including after disconnect.
    #[cfg(any(test, debug_assertions, feature = "test-support"))]
    pub fn navigation_cancellations(&self) -> u64 {
        self.bridge.navigation_cancellations.get()
    }

    /// Whether the newest document load still awaits its finish or failure callback.
    #[cfg(any(test, debug_assertions, feature = "test-support"))]
    pub fn document_loading(&self) -> bool {
        self.bridge.latest_load.borrow().is_some()
    }

    /// Asks the page to cancel a composition its composer has open (`window.__eitriImeCancel`, in the page's
    /// `imeCancel.ts`): WebKit composes in the web process, where the host cannot see it, and confirms the raw letters
    /// when the view gives up the keys. Sent before the keys move, so it reaches the page before the blur.
    pub fn cancel_composition(&self) {
        self.call_ime_cancel("window.__eitriImeCancel ? window.__eitriImeCancel() : 'no hook'");
    }

    /// The move [`cancel_composition`](Self::cancel_composition) was for did not happen: the composition goes on as
    /// the person's own.
    pub fn keep_composition(&self) {
        self.call_ime_cancel("window.__eitriImeCancel ? window.__eitriImeCancel(false) : 'no hook'");
    }

    fn call_ime_cancel(&self, script: &str) {
        let done = RcBlock::new(|result: *mut AnyObject, _error: *mut NSError| {
            // SAFETY: WebKit passes the script's value or null, alive for the length of this call.
            let answer = unsafe { result.as_ref() }
                .and_then(|value| value.downcast_ref::<NSString>())
                .map(|value| value.to_string());
            if answer.as_deref() == Some("armed") {
                crate::log_line!("[mac] the page's composition is cancelled as the keys leave it");
            }
        });
        // SAFETY: asynchronous by contract, as `send` is.
        unsafe {
            self.view
                .evaluateJavaScript_completionHandler(&NSString::from_str(script), Some(&done));
        }
    }

    /// Calls `f` each time the newest document load has ended (finished or failed).
    pub fn on_loaded(&self, f: impl Fn() + 'static) {
        *self.bridge.on_loaded.borrow_mut() = Some(Rc::new(f));
    }

    /// What the window sets when it moves the keys.
    pub fn gate(&self) -> &Rc<KeyGate> {
        &self.gate
    }

    /// The page the panel is built with.
    pub fn page(&self) -> Rc<PanelPage> {
        self.page.clone()
    }

    /// From here on the view's reports go to `events`.
    pub fn report_to(&self, events: Rc<dyn PageEvents>) {
        *self.bridge.events.borrow_mut() = Some(events);
    }

    /// Routes the view's reports to `panel` and loads the first document, which may only happen once the
    /// page's messages have somewhere to go.
    pub fn connect(&self, panel: &AgentPanelHandle) {
        self.report_to(Rc::new(panel.clone()));
        panel.load_document(ViewId::BASE);
    }

    /// Cuts the page off from the panel, for a window that is shutting down: called before the panel's own
    /// shutdown, so a late message or navigation reaches nothing. The page stops loading, its message handler
    /// is removed, nothing is awaited, and the navigation delegate stays to cancel every navigation from now
    /// on (without one, WebKit would allow them all). Idempotent.
    pub fn disconnect(&self) {
        self.bridge.disconnected.set(true);
        *self.bridge.events.borrow_mut() = None;
        self.bridge.awaited_loads.borrow_mut().clear();
        *self.bridge.latest_load.borrow_mut() = None;
        // SAFETY: WebKit's documented calls, on the main thread, on the view this value owns.
        unsafe {
            self.view.stopLoading();
            self.view
                .configuration()
                .userContentController()
                .removeScriptMessageHandlerForName(&NSString::from_str(MESSAGE_HANDLER));
        }
    }
}

/// The panel's page in a WKWebView.
struct WkPage {
    view: Retained<GuardedWebView>,
    bridge: Rc<Bridge>,
    /// The navigation delegate, which the view holds only weakly.
    _delegate: Retained<PanelDelegate>,
}

impl PageSurface for WkPage {
    fn send(&self, envelope_json: &str) {
        let done = RcBlock::new(|_result: *mut AnyObject, error: *mut NSError| {
            // SAFETY: WebKit passes a valid error or null, alive for the length of this call.
            if let Some(error) = unsafe { error.as_ref() } {
                // WebKit's description may quote the page's own text (a script error built from an envelope).
                crate::log_error!("[agent_panel] dispatch failed: {:?}", error.to_string());
            }
        });
        // SAFETY: asynchronous by contract: the script runs, and `done` is called, on a later turn of the run
        // loop, never before this returns.
        unsafe {
            self.view.evaluateJavaScript_completionHandler(
                &NSString::from_str(&dispatch_script(envelope_json)),
                Some(&done),
            );
        }
    }

    fn is_visible(&self) -> bool {
        self.view.is_on_screen()
    }

    fn load_document(&self, html: &str, base_uri: &str) {
        if !guarded_document(html) {
            crate::log_error!(
                "[agent_panel] refused to load a document whose script is not governed by a content security \
                 policy at the start of its <head>"
            );
            return;
        }
        let Some(base) = NSURL::URLWithString(&NSString::from_str(base_uri)) else {
            crate::log_error!("[agent_panel] the page's base address {base_uri:?} is not a URL; nothing loaded");
            return;
        };
        {
            let mut awaited = self.bridge.awaited_loads.borrow_mut();
            if awaited.len() >= MAX_AWAITED_LOADS {
                awaited.remove(0);
            }
            awaited.push(base_uri.to_owned());
        }
        // SAFETY: a document handed over as a string; the view copies it.
        let navigation = unsafe { self.view.loadHTMLString_baseURL(&NSString::from_str(html), Some(&base)) };
        *self.bridge.latest_load.borrow_mut() = navigation;
    }

    /// The colour behind the page, before it draws and past its edges. WebKit's default is derived from the
    /// page, white until the page has painted.
    fn set_background(&self, rgb: [u8; 3]) {
        let [r, g, b] = srgb_components(rgb);
        let colour = NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, 1.0);
        // SAFETY: a plain setter on the main thread.
        unsafe { self.view.setUnderPageBackgroundColor(Some(&colour)) };
    }
}

/// The process side of the panel: links open in the user's default handler. Absent from an acceptance build,
/// whose links are recorded and never opened, so such a build cannot be assembled with it.
#[cfg(not(feature = "acceptance"))]
pub use opening::MacPanelHost;

#[cfg(not(feature = "acceptance"))]
mod opening {
    use eitri_panel::panel_page::PanelHost;
    use objc2_app_kit::NSWorkspace;
    use objc2_foundation::{NSString, NSURL};

    pub struct MacPanelHost;

    impl PanelHost for MacPanelHost {
        fn open_external(&self, url: &str) {
            let Some(address) = NSURL::URLWithString(&NSString::from_str(url)) else {
                crate::log_error!("[agent_panel] opening {url}: not an address AppKit accepts");
                return;
            };
            if !NSWorkspace::sharedWorkspace().openURL(&address) {
                crate::log_error!("[agent_panel] opening {url}: the system found no handler or refused it");
            }
        }
    }
}

/// Calls `f` on the main run loop every `interval` until it returns `false`, also while a menu is open or
/// the window is being resized (the run loop's common modes): the panel keeps streaming then. The returned
/// timer can be invalidated to stop it sooner. Only on the main thread, which the marker proves: `f` holds
/// values that are not thread-safe, and it runs there.
pub fn repeat(_mtm: MainThreadMarker, interval: Duration, f: impl Fn() -> bool + 'static) -> Retained<NSTimer> {
    let block = RcBlock::new(move |timer: NonNull<NSTimer>| {
        if !f() {
            // SAFETY: the run loop passes the timer that is firing, alive for the length of this call.
            unsafe { timer.as_ref() }.invalidate();
        }
    });
    // SAFETY: a timer made unscheduled, then added to the main run loop in its common modes (an extern
    // static Foundation defines).
    unsafe {
        let timer = NSTimer::timerWithTimeInterval_repeats_block(interval.as_secs_f64(), true, &block);
        NSRunLoop::mainRunLoop().addTimer_forMode(&timer, NSRunLoopCommonModes);
        timer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decide(kind: NavKind, main_frame: bool, url: &str, awaited: &mut Vec<String>) -> NavDecision {
        decide_navigation(kind, main_frame, Some(url), awaited)
    }

    const BASE: &str = eitri_panel::panel_document::PANEL_BASE_URI;

    #[test]
    fn the_envelope_reaches_the_page_as_one_string_literal() {
        let envelope =
            "{\"kind\":\"notice\",\"text\":\"a \\\"quote\\\", a back\\\\slash, </script>, \u{2028}\n'single'\"}";
        let script = dispatch_script(envelope);
        let prefix = "(window.__eitriDispatch && window.__eitriDispatch(";
        let suffix = "), true)";
        assert!(script.starts_with(prefix) && script.ends_with(suffix), "{script}");
        let literal = &script[prefix.len()..script.len() - suffix.len()];
        // The literal is one JSON string (a JavaScript string literal too) that decodes to the envelope.
        let back: String = serde_json::from_str(literal).unwrap();
        assert_eq!(back, envelope);
        assert!(!literal[1..literal.len() - 1].contains('\n'));
    }

    #[test]
    fn only_the_documents_own_loads_are_allowed() {
        let mut awaited = vec![BASE.to_owned()];
        assert_eq!(decide(NavKind::Other, true, BASE, &mut awaited), NavDecision::Allow);
        assert!(awaited.is_empty());
        // The same address again, now that no load waits: a script's `location`.
        assert_eq!(decide(NavKind::Other, true, BASE, &mut awaited), NavDecision::Cancel);

        let mut awaited = vec![BASE.to_owned()];
        assert_eq!(
            decide(NavKind::Other, true, "https://example.com/", &mut awaited),
            NavDecision::Cancel
        );
        assert_eq!(decide(NavKind::Other, false, BASE, &mut awaited), NavDecision::Cancel);
        assert_eq!(
            awaited,
            vec![BASE.to_owned()],
            "a refused navigation spends no awaited load"
        );
        for kind in [
            NavKind::FormSubmitted,
            NavKind::FormResubmitted,
            NavKind::BackForward,
            NavKind::Reload,
            NavKind::Unknown(7),
        ] {
            assert_eq!(decide(kind, true, BASE, &mut awaited), NavDecision::Cancel, "{kind:?}");
        }
        assert_eq!(
            decide_navigation(NavKind::Other, true, None, &mut awaited),
            NavDecision::Cancel
        );
    }

    #[test]
    fn a_load_webkit_never_asked_about_leaves_no_allowance_once_the_newest_has_ended() {
        // Two loads were issued and WebKit asked about only the second.
        let bridge = Bridge::default();
        *bridge.awaited_loads.borrow_mut() = vec![BASE.to_owned(), BASE.to_owned()];
        let decide_now = || decide(NavKind::Other, true, BASE, &mut bridge.awaited_loads.borrow_mut());
        assert_eq!(decide_now(), NavDecision::Allow);
        // Without the clearing, a script's `location` to the same address would be let through.
        assert_eq!(
            bridge.awaited_loads.borrow().len(),
            1,
            "the dropped load's entry is left over"
        );
        bridge.newest_load_ended();
        assert_eq!(decide_now(), NavDecision::Cancel);
    }

    #[test]
    fn a_link_click_goes_to_the_panel_and_never_navigates() {
        let mut awaited = vec![BASE.to_owned()];
        for main_frame in [true, false] {
            assert_eq!(
                decide(
                    NavKind::LinkActivated,
                    main_frame,
                    "https://example.com/a",
                    &mut awaited
                ),
                NavDecision::LinkClick(Some("https://example.com/a".to_owned()))
            );
        }
        // Even one to the document's own address: the panel refuses it, and the view does not reload.
        assert_eq!(
            decide(NavKind::LinkActivated, true, BASE, &mut awaited),
            NavDecision::LinkClick(Some(BASE.to_owned()))
        );
        assert_eq!(awaited, vec![BASE.to_owned()]);
        assert_eq!(
            decide_navigation(NavKind::LinkActivated, true, None, &mut awaited),
            NavDecision::LinkClick(None)
        );
    }

    #[test]
    fn the_view_takes_the_keys_only_when_given_them() {
        assert!(!may_take_keys(true));
        assert!(may_take_keys(false));
        assert!(KeyGate::new().editor_has_keys(), "the editor starts with the keys");
    }

    #[test]
    fn the_page_is_on_screen_only_unhidden_in_a_window() {
        assert!(on_screen(false, true));
        assert!(!on_screen(true, true));
        assert!(!on_screen(false, false));
    }

    #[test]
    fn a_document_with_script_loads_only_with_its_policy_first_in_head() {
        let themed = PANEL_DOCUMENT.themed(&[]);
        assert!(guarded_document(&themed));
        assert!(!guarded_document(AGENT_UI_HTML), "the bundle without its policy");
        assert!(
            guarded_document(&eitri_panel::webview_crash_guard::crash_message_html("r")),
            "no script"
        );
        assert!(!guarded_document("<html><head><title>t</title><meta http-equiv=\"Content-Security-Policy\" content=\"x\"><SCRIPT>1</SCRIPT></head></html>"));
        assert!(
            !guarded_document("<html><body><script>1</script></body></html>"),
            "no <head> at all"
        );
    }

    #[test]
    fn background_bytes_become_srgb_components() {
        assert_eq!(srgb_components([0, 255, 51]), [0.0, 1.0, 0.2]);
    }

    #[test]
    fn the_embedded_document_pins_its_script_by_hash() {
        let csp = PANEL_DOCUMENT.csp();
        assert!(csp.contains("script-src 'sha256-"), "{csp}");
        assert!(
            !csp.contains("script-src 'none'"),
            "the extractor refused the bundle: {csp}"
        );
    }
}
