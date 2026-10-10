//! The panel's page in a real WKWebView, off screen: no window and no application are made, the view
//! only runs its page while this test turns the main run loop.
//!
//! The embedded bundle loads under its content security policy and says `ready`; a script the page did
//! not ship is blocked (and runs in the same bundle under a permissive policy, while a bundle with no policy
//! at all is not loaded); an envelope reaches `window.__eitriDispatch` as the very string sent; every kind of
//! navigation leaves the document where it is (a link click reaches the panel instead); and a repeating
//! timer stops when its callback says so.
//!
//! WebKit wants the main thread, which only a test without the standard harness runs on, so this file is
//! its own `main`.

#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
fn main() {
    macos::run();
}

#[cfg(target_os = "macos")]
mod macos {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use block2::RcBlock;
    use eitri_core::theme::ThemeTokens;
    use eitri_mac::appkit::page::{repeat, NavDecision, NavKind, PageEvents, PanelView, PANEL_DOCUMENT};
    use eitri_panel::panel_document::PANEL_BASE_URI;
    use eitri_panel::webview_crash_guard;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObject, Sel};
    use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
    use objc2_app_kit::{
        NSBackingStoreType, NSEvent, NSEventModifierFlags, NSEventType, NSResponder, NSWindow, NSWindowStyleMask,
    };
    use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSError, NSPoint, NSRect, NSRunLoop, NSSize, NSString};
    use objc2_web_kit::WKWebView;

    const TRIALS: usize = 5;

    #[derive(Default)]
    struct Recorder {
        messages: RefCell<Vec<String>>,
        links: RefCell<Vec<Option<String>>>,
        gone: RefCell<Vec<String>>,
        decisions: RefCell<Vec<(NavKind, Option<String>, NavDecision)>>,
    }

    impl PageEvents for Recorder {
        fn page_message(&self, raw: &str) {
            self.messages.borrow_mut().push(raw.to_owned());
        }
        fn link_clicked(&self, url: Option<&str>) {
            self.links.borrow_mut().push(url.map(str::to_owned));
        }
        fn page_gone(&self, reason: &str) {
            self.gone.borrow_mut().push(reason.to_owned());
        }
        fn page_shown(&self) {}
        fn navigation_decided(&self, kind: NavKind, url: Option<&str>, decision: &NavDecision) {
            self.decisions
                .borrow_mut()
                .push((kind, url.map(str::to_owned), decision.clone()));
        }
    }

    impl Recorder {
        fn readies(&self) -> usize {
            self.messages
                .borrow()
                .iter()
                .filter(|m| serde_json::from_str::<serde_json::Value>(m).is_ok_and(|v| v["type"] == "ready"))
                .count()
        }
    }

    fn spin(for_: Duration) {
        let end = Instant::now() + for_;
        while Instant::now() < end {
            spin_once();
        }
    }

    fn spin_once() {
        // SAFETY: turns this thread's own run loop once, for at most 20 ms.
        unsafe {
            let until = NSDate::dateWithTimeIntervalSinceNow(0.02);
            NSRunLoop::currentRunLoop().runMode_beforeDate(NSDefaultRunLoopMode, &until);
        }
    }

    fn spin_until(limit: Duration, done: impl Fn() -> bool) -> bool {
        let end = Instant::now() + limit;
        while Instant::now() < end {
            if done() {
                return true;
            }
            spin_once();
        }
        done()
    }

    /// The script's value as text, or `ERROR: ...`.
    fn eval(view: &WKWebView, script: &str) -> String {
        let out: Rc<RefCell<Option<String>>> = Rc::default();
        let sink = out.clone();
        let done = RcBlock::new(move |result: *mut AnyObject, error: *mut NSError| {
            // SAFETY: WebKit passes valid objects or null, alive for the length of this call.
            let text = unsafe {
                if let Some(error) = error.as_ref() {
                    format!("ERROR: {error}")
                } else if let Some(result) = result.as_ref() {
                    match result.downcast_ref::<NSString>() {
                        Some(s) => s.to_string(),
                        None => {
                            let d: Retained<NSString> = msg_send![result, description];
                            d.to_string()
                        }
                    }
                } else {
                    "nil".to_owned()
                }
            };
            *sink.borrow_mut() = Some(text);
        });
        // SAFETY: as the page does; the completion runs on a later turn of the run loop.
        unsafe { view.evaluateJavaScript_completionHandler(&NSString::from_str(script), Some(&done)) };
        if !spin_until(Duration::from_secs(10), || out.borrow().is_some()) {
            return "TIMEOUT".to_owned();
        }
        let text = out.borrow_mut().take().unwrap();
        text
    }

    struct Checks(Vec<(String, bool, String)>);

    impl Checks {
        fn check(&mut self, name: &str, ok: bool, detail: String) {
            println!("{} {name}: {detail}", if ok { "ok  " } else { "FAIL" });
            self.0.push((name.to_owned(), ok, detail));
        }
    }

    pub fn run() {
        let mtm = MainThreadMarker::new().expect("a test without the harness runs on the main thread");
        let mut checks = Checks(Vec::new());

        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(440.0, 700.0));
        let panel = PanelView::new(mtm, frame);
        let recorder = Rc::new(Recorder::default());
        panel.report_to(recorder.clone());
        let page = panel.page();
        let view: &WKWebView = panel.view();
        let document = PANEL_DOCUMENT.themed(&ThemeTokens::fallback().css_vars());
        page.load_document(&document, PANEL_BASE_URI);

        let ready = spin_until(Duration::from_secs(30), || recorder.readies() > 0);
        checks.check(
            "the embedded bundle runs and says ready",
            ready,
            format!(
                "{} page message(s); gone {:?}",
                recorder.messages.borrow().len(),
                recorder.gone.borrow()
            ),
        );
        if !ready {
            finish(checks);
        }

        let href = eval(view, "location.href");
        checks.check("the document is at the panel's address", href == PANEL_BASE_URI, href);
        let meta = eval(
            view,
            "(function(){var m=document.head.firstElementChild;return m&&m.httpEquiv==='Content-Security-Policy'?m.content:'(not the CSP meta)'})()",
        );
        let want = PANEL_DOCUMENT.csp();
        checks.check(
            "the policy is the first element of <head> and pins the script by hash",
            meta == want && want.contains("script-src 'sha256-"),
            meta,
        );
        let typeof_dispatch = eval(view, "typeof window.__eitriDispatch");
        checks.check(
            "the page installed its dispatch",
            typeof_dispatch == "function",
            typeof_dispatch,
        );

        let mut blocked = 0;
        for n in 0..TRIALS {
            eval(
                view,
                &format!(
                    "(function(){{var s=document.createElement('script');s.textContent='window.__foreign{n}=1';document.head.appendChild(s);return 'x'}})()"
                ),
            );
            spin(Duration::from_millis(100));
            if eval(view, &format!("String(window.__foreign{n}||null)")) == "null" {
                blocked += 1;
            }
        }
        checks.check(
            "a foreign inline script is blocked",
            blocked == TRIALS,
            format!("{blocked}/{TRIALS}"),
        );

        // The dispatch path: the page's own function, wrapped to keep what it was given.
        eval(
            view,
            "(function(){var o=window.__eitriDispatch;window.__got=[];window.__eitriDispatch=function(e){window.__got.push(e);return o(e)};return 'x'})()",
        );
        let envelope = r#"{"kind":"probe","text":"a \"quote\", a back\\slash, </script>, ' and a line\nbreak"}"#;
        let mut delivered = 0;
        for _ in 0..TRIALS {
            page.send_direct(envelope);
            spin(Duration::from_millis(50));
        }
        let got = eval(view, "JSON.stringify(window.__got)");
        if let Ok(list) = serde_json::from_str::<Vec<String>>(&got) {
            delivered = list.iter().filter(|e| *e == envelope).count();
        }
        checks.check(
            "an envelope reaches __eitriDispatch as the string sent",
            delivered == TRIALS,
            format!("{delivered}/{TRIALS}"),
        );
        // What WebKit makes of a script whose value is `undefined`, the reason the dispatch script ends in
        // `true`: recorded, not judged.
        println!(
            "note an undefined script value completes as: {}",
            eval(view, "undefined")
        );

        // Every kind of navigation: the document stays, and no second `ready` (no reload) arrives.
        // The first load was the only navigation allowed so far.
        let allowed = |d: &[(NavKind, Option<String>, NavDecision)]| {
            d.iter()
                .filter(|(_, _, decision)| *decision == NavDecision::Allow)
                .count()
        };
        checks.check(
            "the document's own load was the one navigation allowed",
            allowed(&recorder.decisions.borrow()) == 1,
            format!("{:?}", recorder.decisions.borrow()),
        );
        // `asked`: the delegate must have refused it itself; otherwise WebKit or the policy may stop it first
        // (a form `form-action 'none'` forbids, a pop-up without a user gesture, a frame `frame-src 'none'`
        // forbids), and only no allowance counts. The policy's refusals are counted for the record.
        eval(
            view,
            "(function(){window.__violations=[];document.addEventListener('securitypolicyviolation',function(e){window.__violations.push(e.violatedDirective)});return 'x'})()",
        );
        let violations = || {
            eval(view, "String(window.__violations.length)")
                .parse::<usize>()
                .unwrap_or(usize::MAX)
        };
        let attempts: [(&str, &str, Option<&str>, bool); 9] = [
            (
                "link click",
                "(function(){var a=document.createElement('a');a.href='https://eitri-test.invalid/link';document.body.appendChild(a);a.click();a.remove();return 'x'})()",
                Some("https://eitri-test.invalid/link"),
                true,
            ),
            (
                "form post submit",
                "(function(){var f=document.createElement('form');f.method='post';f.action='https://eitri-test.invalid/submit';document.body.appendChild(f);f.submit();f.remove();return 'x'})()",
                None,
                false,
            ),
            (
                "location.href",
                "(function(){location.href='https://eitri-test.invalid/script';return 'x'})()",
                None,
                true,
            ),
            (
                "location to its own address",
                "(function(){location.href='https://eitri.invalid/';return 'x'})()",
                None,
                true,
            ),
            ("location.reload", "(function(){location.reload();return 'x'})()", None, true),
            (
                "window.open",
                "(function(){return String(window.open('https://eitri-test.invalid/popup'))})()",
                None,
                false,
            ),
            (
                "iframe",
                "(function(){var f=document.createElement('iframe');f.src='https://eitri-test.invalid/frame';document.body.appendChild(f);setTimeout(function(){f.remove()},300);return 'x'})()",
                None,
                false,
            ),
            (
                "tel: link click",
                "(function(){var a=document.createElement('a');a.href='tel:+15550100';document.body.appendChild(a);a.click();a.remove();return 'x'})()",
                Some("tel:+15550100"),
                true,
            ),
            ("tel: location", "(function(){location.href='tel:+15550100';return 'x'})()", None, true),
        ];
        for (name, script, link, asked) in attempts {
            let mut refused = 0;
            let mut by_the_delegate = 0;
            let violations_before = violations();
            for _ in 0..TRIALS {
                let readies = recorder.readies();
                let links = recorder.links.borrow().len();
                let decided = recorder.decisions.borrow().len();
                eval(view, script);
                spin(Duration::from_millis(400));
                let stayed = eval(view, "location.href") == PANEL_BASE_URI
                    && eval(view, "typeof window.__eitriDispatch") == "function"
                    && recorder.readies() == readies;
                let new_links: Vec<Option<String>> = recorder.links.borrow()[links..].to_vec();
                let links_ok = match link {
                    Some(url) => new_links == vec![Some(url.to_owned())],
                    None => new_links.is_empty(),
                };
                let decisions = recorder.decisions.borrow()[decided..].to_vec();
                let none_allowed = allowed(&decisions) == 0;
                if !decisions.is_empty() {
                    by_the_delegate += 1;
                }
                if stayed && links_ok && none_allowed && (!asked || !decisions.is_empty()) {
                    refused += 1;
                }
            }
            checks.check(
                &format!("navigation refused: {name}"),
                refused == TRIALS,
                format!(
                    "{refused}/{TRIALS}, decided by the delegate {by_the_delegate}/{TRIALS}, policy violations {}",
                    violations().wrapping_sub(violations_before)
                ),
            );
        }
        checks.check(
            "the content process never ended",
            recorder.gone.borrow().is_empty(),
            format!("{:?}", recorder.gone.borrow()),
        );

        // A force click on a link must not open WebKit's own preview of it.
        // SAFETY: a plain getter on the main thread.
        let previews = unsafe { view.allowsLinkPreview() };
        checks.check(
            "link previews are off",
            !previews,
            format!("allowsLinkPreview {previews}"),
        );

        // Later loads, as a reload by hand and the crash guard make them: each is allowed and the page comes
        // back (a new `ready`), at the same address.
        let allowed_now = || allowed(&recorder.decisions.borrow());
        let mut reloaded = 0;
        for _ in 0..TRIALS {
            let (readies, allows) = (recorder.readies(), allowed_now());
            page.load_document(&document, PANEL_BASE_URI);
            let back = spin_until(Duration::from_secs(15), || recorder.readies() > readies);
            if back
                && allowed_now() == allows + 1
                && eval(view, "location.href") == PANEL_BASE_URI
                && eval(view, "typeof window.__eitriDispatch") == "function"
            {
                reloaded += 1;
            }
        }
        checks.check(
            "a reload of the document is allowed and the page comes back",
            reloaded == TRIALS,
            format!("{reloaded}/{TRIALS}"),
        );

        // Two loads in a row, as two quick reloads: whatever WebKit dropped leaves no allowance behind for a
        // script's `location` to the same address.
        // Two quick loads are both asked about (seen: 2 allowed), so no entry is left over by them. A load
        // stopped before WebKit asks about it is: the first load here is stopped at once, the second goes on.
        let (readies, allows) = (recorder.readies(), allowed_now());
        page.load_document(&document, PANEL_BASE_URI);
        // SAFETY: a plain call on the main thread.
        unsafe { view.stopLoading() };
        page.load_document(&document, PANEL_BASE_URI);
        spin_until(Duration::from_secs(15), || recorder.readies() > readies);
        spin(Duration::from_millis(800));
        let allowed_by_two = allowed_now() - allows;
        // Recorded, not judged: WebKit has been seen to ask about both (2 allowed), in which case no entry is
        // left over here; the clearing is shown by the unit test of the bridge.
        println!("note a load stopped at once, then another: {allowed_by_two} allowed");
        let (readies, decided) = (recorder.readies(), recorder.decisions.borrow().len());
        eval(
            view,
            "(function(){location.href='https://eitri.invalid/';return 'x'})()",
        );
        spin(Duration::from_millis(500));
        let decisions = recorder.decisions.borrow()[decided..].to_vec();
        checks.check(
            "after two quick loads a script's location to the panel's address is still refused",
            !decisions.is_empty() && allowed(&decisions) == 0 && recorder.readies() == readies,
            format!("{decisions:?}"),
        );

        // The page shown after repeated crashes: allowed, and drawn.
        let allows = allowed_now();
        page.load_document(
            &webview_crash_guard::crash_message_html("Reload it by hand."),
            PANEL_BASE_URI,
        );
        let drawn = spin_until(Duration::from_secs(15), || {
            eval(view, "document.body ? document.body.innerText : ''").contains("kept crashing")
        });
        checks.check(
            "the give-up page is allowed and drawn",
            drawn && allowed_now() == allows + 1,
            format!("drawn {drawn}, allowed {}", allowed_now() - allows),
        );
        // A reload by hand from there brings the panel back.
        let readies = recorder.readies();
        page.load_document(&document, PANEL_BASE_URI);
        let back = spin_until(Duration::from_secs(15), || recorder.readies() > readies);
        checks.check(
            "a reload from the give-up page brings the panel back",
            back,
            String::new(),
        );

        // The content process ends: the panel hears of it, and the load its crash guard makes brings the page
        // back. The process is found through WebKit's own (private) record of its pid, read here in the test
        // only, and ended by that pid.
        // SAFETY: a selector checked to exist first, answering a pid.
        let pid: i32 = unsafe {
            let known: bool = msg_send![view, respondsToSelector: sel!(_webProcessIdentifier)];
            if known {
                msg_send![view, _webProcessIdentifier]
            } else {
                0
            }
        };
        if pid > 0 {
            // SAFETY: a signal to the pid just read, WebKit's content process for this view.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            let gone = spin_until(Duration::from_secs(15), || !recorder.gone.borrow().is_empty());
            let readies = recorder.readies();
            page.load_document(&document, PANEL_BASE_URI);
            let back = spin_until(Duration::from_secs(20), || recorder.readies() > readies);
            checks.check(
                "a killed content process is reported, and the next load brings the page back",
                gone && back,
                format!("pid {pid}, gone {:?}, back {back}", recorder.gone.borrow()),
            );
        } else {
            println!("note the content process's pid is not readable here; its crash is left to the on-screen run");
        }

        // A document whose script is not governed by a policy at the start of its <head> is not loaded at all.
        let bare = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../agent-ui/web/dist/index.html"))
            .expect("the bundle the build script made");
        let refused = PanelView::new(mtm, frame);
        let refused_recorder = Rc::new(Recorder::default());
        refused.report_to(refused_recorder.clone());
        refused.page().load_document(&bare, PANEL_BASE_URI);
        spin(Duration::from_secs(2));
        checks.check(
            "the bundle without its policy is not loaded",
            refused_recorder.readies() == 0 && refused_recorder.decisions.borrow().is_empty(),
            format!(
                "{} message(s), {:?}",
                refused_recorder.messages.borrow().len(),
                refused_recorder.decisions.borrow()
            ),
        );

        // The control: the same bundle under a policy that allows inline script, frames and forms, where the
        // same foreign script must run, so the blocking above is the pinned policy's doing; and where forms and
        // frames are not stopped by the policy, so the delegate's own refusal of them is seen.
        const PERMISSIVE: &str = "script-src 'unsafe-inline'; style-src 'unsafe-inline'; frame-src *; form-action *";
        let permissive = bare.replacen(
            "<head>",
            &format!("<head><meta http-equiv=\"Content-Security-Policy\" content=\"{PERMISSIVE}\">"),
            1,
        );
        let control = PanelView::new(mtm, frame);
        let control_recorder = Rc::new(Recorder::default());
        control.report_to(control_recorder.clone());
        control.page().load_document(&permissive, PANEL_BASE_URI);
        let control_ready = spin_until(Duration::from_secs(30), || control_recorder.readies() > 0);
        let control_view: &WKWebView = control.view();
        let meta = eval(
            control_view,
            "(function(){var m=document.querySelector('meta[http-equiv=\"Content-Security-Policy\"]');return m?m.content:'(no policy)'})()",
        );
        let mut ran = 0;
        for n in 0..TRIALS {
            eval(
                control_view,
                &format!(
                    "(function(){{var s=document.createElement('script');s.textContent='window.__foreign{n}=1';document.head.appendChild(s);return 'x'}})()"
                ),
            );
            spin(Duration::from_millis(100));
            if eval(control_view, &format!("String(window.__foreign{n}||null)")) == "1" {
                ran += 1;
            }
        }
        checks.check(
            "control under a permissive policy: the same foreign script runs",
            control_ready && meta == PERMISSIVE && ran == TRIALS,
            format!("ready {control_ready}, policy {meta:?}, ran {ran}/{TRIALS}"),
        );

        // With the policy out of the way, the delegate itself refuses a form and a frame.
        let delegate_only = [
            (
                "form post submit",
                "(function(){var f=document.createElement('form');f.method='post';f.action='https://eitri-test.invalid/submit';document.body.appendChild(f);f.submit();f.remove();return 'x'})()",
                NavKind::FormSubmitted,
            ),
            (
                "iframe",
                "(function(){var f=document.createElement('iframe');f.src='https://eitri-test.invalid/frame';document.body.appendChild(f);setTimeout(function(){f.remove()},300);return 'x'})()",
                NavKind::Other,
            ),
        ];
        for (name, script, kind) in delegate_only {
            let mut cancelled = 0;
            for _ in 0..TRIALS {
                let decided = control_recorder.decisions.borrow().len();
                let readies = control_recorder.readies();
                eval(control_view, script);
                spin(Duration::from_millis(400));
                let decisions = control_recorder.decisions.borrow()[decided..].to_vec();
                let stayed =
                    eval(control_view, "location.href") == PANEL_BASE_URI && control_recorder.readies() == readies;
                if stayed
                    && !decisions.is_empty()
                    && decisions
                        .iter()
                        .all(|(k, _, d)| *k == kind && *d == NavDecision::Cancel)
                {
                    cancelled += 1;
                }
            }
            checks.check(
                &format!("the delegate itself cancels: {name}"),
                cancelled == TRIALS,
                format!("{cancelled}/{TRIALS}"),
            );
        }

        // Only a string message reaches the panel.
        let before = control_recorder.messages.borrow().len();
        for body in ["{type:'ready',request_id:'x'}", "1", "null", "['a']"] {
            eval(
                control_view,
                &format!("(function(){{window.webkit.messageHandlers.eitriAgent.postMessage({body});return 'x'}})()"),
            );
        }
        spin(Duration::from_millis(300));
        let not_strings = control_recorder.messages.borrow().len() - before;
        eval(
            control_view,
            "(function(){window.webkit.messageHandlers.eitriAgent.postMessage('a string');return 'x'})()",
        );
        spin(Duration::from_millis(300));
        let after: Vec<String> = control_recorder.messages.borrow()[before..].to_vec();
        checks.check(
            "only a string message reaches the panel",
            not_strings == 0 && after == ["a string"],
            format!("{not_strings} non-string(s) arrived; then {after:?}"),
        );

        // Disconnected for shutdown: nothing more reaches the panel, and the view has no delegate.
        control.disconnect();
        let before = control_recorder.messages.borrow().len();
        // With no handler left the page may have no `messageHandlers` at all.
        let handler = eval(
            control_view,
            "(function(){try{window.webkit.messageHandlers.eitriAgent.postMessage('late');return 'posted'}catch(e){return 'no bridge'}})()",
        );
        // The delegate stays and cancels everything: a navigation that needs no network (it would succeed at
        // once if allowed) and a link click both leave the page where it is.
        eval(control_view, "(function(){location.href='about:blank';return 'x'})()");
        spin(Duration::from_millis(400));
        eval(
            control_view,
            "(function(){var a=document.createElement('a');a.href='about:blank#link';document.body.appendChild(a);a.click();return 'x'})()",
        );
        spin(Duration::from_millis(400));
        let href = eval(control_view, "location.href");
        // SAFETY: a plain getter on the main thread.
        let delegate_kept = unsafe { control_view.navigationDelegate() }.is_some();
        checks.check(
            "after disconnect the page has no bridge and every navigation is cancelled",
            handler == "no bridge"
                && delegate_kept
                && href == PANEL_BASE_URI
                && control_recorder.messages.borrow().len() == before
                && control_recorder.links.borrow().is_empty(),
            format!("handler {handler}, delegate kept {delegate_kept}, href {href}"),
        );

        // What the page leaves unhandled stops at the sink: keys and text commands go no further, the pointer
        // goes on. With no window the view keeps AppKit's own next responder; in one, the sink, then the window.
        let sink = panel.key_sink();
        // SAFETY: plain getters on the main thread.
        let alone = unsafe { panel.view().nextResponder() }.map(|r| r.class().name().to_string_lossy().into_owned());
        checks.check(
            "with no window the web view's next responder is not the sink",
            alone.as_deref() != Some("EitriPanelKeySink"),
            format!("{alone:?}"),
        );
        let tail = Tail::new(mtm);
        // SAFETY: plain setters and AppKit's own event constructors, on the main thread.
        let reached = unsafe {
            sink.setNextResponder(Some(&tail));
            let key = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
                NSEventType::KeyDown, NSPoint::new(0.0, 0.0), NSEventModifierFlags::empty(), 0.0, 0, None,
                &NSString::from_str("n"), &NSString::from_str("n"), false, 45,
            ).expect("a key event");
            let up = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
                NSEventType::KeyUp, NSPoint::new(0.0, 0.0), NSEventModifierFlags::empty(), 0.0, 0, None,
                &NSString::from_str("n"), &NSString::from_str("n"), false, 45,
            ).expect("a key event");
            let tab = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
                NSEventType::KeyDown, NSPoint::new(0.0, 0.0), NSEventModifierFlags::empty(), 0.0, 0, None,
                &NSString::from_str("\t"), &NSString::from_str("\t"), false, 48,
            ).expect("a key event");
            let click = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
                NSEventType::LeftMouseDown, NSPoint::new(0.0, 0.0), NSEventModifierFlags::empty(), 0.0, 0, None, 0, 1, 1.0,
            ).expect("a mouse event");
            sink.keyDown(&key);
            sink.keyUp(&up);
            sink.keyDown(&tab);
            let _: () = msg_send![sink, doCommandBySelector: sel!(insertTab:)];
            let _: () = msg_send![sink, doCommandBySelector: sel!(noop:)];
            sink.mouseDown(&click);
            tail.ivars().borrow().clone()
        };
        checks.check(
            "the sink stops keys and text commands, and passes the pointer on",
            reached == ["mouseDown:"] && sink.stopped() == (2, 2),
            format!("reached the next responder: {reached:?}; stopped {:?}", sink.stopped()),
        );
        let window = offscreen_window(mtm);
        window.contentView().expect("a content view").addSubview(panel.view());
        // SAFETY: plain getters on the main thread.
        let (next, after_is_window) = unsafe {
            let next = panel.view().nextResponder();
            let after = next.as_ref().and_then(|r| r.nextResponder());
            (
                next.map(|r| r.class().name().to_string_lossy().into_owned()),
                after.is_some_and(|r| std::ptr::eq(Retained::as_ptr(&r).cast::<NSWindow>(), Retained::as_ptr(&window))),
            )
        };
        checks.check(
            "in a window the web view's next responder is the sink, and the sink's the window",
            next.as_deref() == Some("EitriPanelKeySink") && after_is_window,
            format!("{next:?}, then the window: {after_is_window}"),
        );
        panel.view().removeFromSuperview();

        // The timer stops itself.
        let calls = Rc::new(Cell::new(0u32));
        let counted = calls.clone();
        let _timer = repeat(mtm, Duration::from_millis(10), move || {
            counted.set(counted.get() + 1);
            counted.get() < 3
        });
        spin(Duration::from_millis(300));
        checks.check(
            "a repeating timer stops when it says so",
            calls.get() == 3,
            format!("{} call(s)", calls.get()),
        );

        finish(checks);
    }

    define_class!(
        /// Records the selectors that reach it.
        #[unsafe(super(NSResponder, NSObject))]
        #[thread_kind = MainThreadOnly]
        #[ivars = RefCell<Vec<String>>]
        struct Tail;

        impl Tail {
            #[unsafe(method(keyDown:))]
            fn key_down(&self, _event: &NSEvent) {
                self.ivars().borrow_mut().push("keyDown:".into());
            }
            #[unsafe(method(keyUp:))]
            fn key_up(&self, _event: &NSEvent) {
                self.ivars().borrow_mut().push("keyUp:".into());
            }
            #[unsafe(method(mouseDown:))]
            fn mouse_down(&self, _event: &NSEvent) {
                self.ivars().borrow_mut().push("mouseDown:".into());
            }
            #[unsafe(method(doCommandBySelector:))]
            fn do_command(&self, command: Sel) {
                self.ivars().borrow_mut().push(command.name().to_string_lossy().into_owned());
            }
            #[unsafe(method(noResponderFor:))]
            fn no_responder(&self, command: Sel) {
                self.ivars().borrow_mut().push(format!("noResponderFor:{}", command.name().to_string_lossy()));
            }
        }
    );

    impl Tail {
        fn new(mtm: MainThreadMarker) -> Retained<Tail> {
            let this = Tail::alloc(mtm).set_ivars(RefCell::new(Vec::new()));
            // SAFETY: NSResponder's `init` on a freshly allocated object.
            unsafe { msg_send![super(this), init] }
        }
    }

    /// An off-screen window, never ordered in.
    fn offscreen_window(mtm: MainThreadMarker) -> Retained<NSWindow> {
        // SAFETY: AppKit's designated initialiser on the main thread; released by its owner, not on close.
        unsafe {
            let window = NSWindow::initWithContentRect_styleMask_backing_defer(
                NSWindow::alloc(mtm),
                NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(800.0, 600.0)),
                NSWindowStyleMask::Borderless,
                NSBackingStoreType::Buffered,
                true,
            );
            window.setReleasedWhenClosed(false);
            window
        }
    }

    fn finish(checks: Checks) -> ! {
        let failed: Vec<&String> = checks
            .0
            .iter()
            .filter(|(_, ok, _)| !ok)
            .map(|(name, _, _)| name)
            .collect();
        println!("{} check(s), {} failed", checks.0.len(), failed.len());
        std::process::exit(if failed.is_empty() { 0 } else { 1 });
    }
}
