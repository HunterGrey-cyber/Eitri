//! Every web module's `WebView` sits at (0,0) of a one-child host of its own (2026-09-29).
//!
//! **What went wrong.** On the owner's machine (GNOME Wayland, fcitx5 through `GTK_IM_MODULE=fcitx`,
//! candidates drawn by kimpanel) the candidate window for the agent panel's composer opened far right
//! of the caret. A sandbox pass measured it the same day (fcitx5-gtk's own in-process panel, not
//! kimpanel): the rectangle sent to fcitx5 was off by exactly the `WebView`'s position in the module
//! grid times the output scale -- 761 logical px in the default split, at scale 1 and 1.5 and at 96 and
//! 144 dpi, on the empty tab and a live one -- and by nothing whenever the panel sat at x=0 (a zoom).
//!
//! **Why: WebKitGTK counts that position twice.** WebKitGTK 2.52.6 adds the `WebView`'s allocation,
//! its position in its **parent**, to the caret rectangle before handing it to the input method:
//! `InputMethodFilter::platformTransformCursorRectToViewCoordinates`
//! (`Source/WebKit/UIProcess/API/gtk/InputMethodFilterGtk.cpp:34-41`), called from
//! `InputMethodFilter::notifyCursorRect` (`Source/WebKit/UIProcess/API/glib/InputMethodFilter.cpp:216-235`),
//! which ends in `gtk_im_context_set_cursor_location`
//! (`Source/WebKit/UIProcess/API/gtk/WebKitInputMethodContextImplGtk.cpp:195-199`). Under GTK4 that
//! context's client widget is the `WebView` itself (`WebKitInputMethodContextImplGtk.cpp:238-241`,
//! reached from `webkitWebViewBaseRealize`, `WebKitWebViewBase.cpp:683`), and GTK4's contract is a
//! location relative to the client widget, so the input method translates from the `WebView` to the
//! window a second time (fcitx5-gtk 5.1.7, `gtk4/fcitximcontext.cpp`, `_set_cursor_location_internal`).
//! WebKit `main` was unchanged on 2026-09-29. Where a `WebView` fills its parent from (0,0) the double
//! count adds nothing, which is why most applications never see it; `module_grid` used to be the
//! `WebView`'s parent, so it added the module's own x and y.
//!
//! **The workaround.** [`WebHost`] has exactly one child, the `WebView`, and allocates it at its own
//! (0,0) with no transform (`GtkBinLayout`: `gtk_bin_layout_allocate`, `gtkbinlayout.c:81-96`, GTK
//! 4.22.5). [`WebHost::new`] also clears the child's margins and sets its alignment to `Fill`, the two
//! things `gtk_widget_allocate` adds to the position a parent gives a child and
//! `gtk_widget_get_allocation` (what WebKit reads) reports back (`gtkwidget.c`, 4.22.5). The grid
//! allocates the host where the module goes, and WebKit then adds (0,0). No rule in Eitri's
//! stylesheet names the host's node, [`CSS_NAME`] (`theme::gtk_css`'s tests hold that), and CSS margin,
//! border or padding on either widget would not move the child anyway: `gtk_widget_get_allocation`
//! subtracts the child's own CSS boxes again, and the host's are outside its children's coordinates.
//!
//! **It stays correct if WebKit fixes this upstream.** A WebKit that returns the rectangle untranslated
//! under `USE(GTK4)` -- the fix to report to bugs.webkit.org -- adds nothing, and (0,0) plus nothing is
//! still right. The host would then cost one widget per web module, and nothing else.
//!
//! **What it does not cover: a rectangle left stale by a layout change.** WebKit re-sends the caret
//! only when the caret itself moves 10px or more (`InputMethodFilter.cpp:229`), and fcitx5-gtk
//! translates only when it is handed a new rectangle or focus arrives. A zoom, a divider drag or a
//! window resize that moves the panel but not its caret therefore leaves fcitx5 holding the rectangle
//! translated at the panel's old position until the next caret move or focus-in, so the first
//! composition after such a change can still open where the panel used to be. The sandbox measured
//! it (a `prefix z` zoom, then typing: no new rectangle sent at all). Eitri cannot reach WebKit's
//! private input method context to make it send again.
//!
//! **Focus.** The host is not focusable, and GTK's default grab refuses a widget that is not
//! (`gtk_widget_grab_focus_self`, `gtkwidget.c`, 4.22.5) -- the sandbox's first wrapped run, a plain
//! `GtkBox`, left the keys in the editor that way. So module focus does not go through the host:
//! `ModuleGrid::add` takes each module's focus target, the `WebView` here, and
//! `ModuleGrid::focus_target` hands it back. A grab that reaches the host anyway -- code handed only
//! the grid's hosts -- lands on the `WebView` too, because the host forwards it to its one child as
//! `GtkPopoverBin` does (review round 1, 2026-09-29). What else is keyed on the host holds with the
//! host an ancestor of the `WebView`: `pane_focus` walks up from the focus widget; the capture-phase
//! key and scroll controllers (`main.rs`'s `install_module_nav`, `panel_super`, `wheel_zoom`) run on
//! every ancestor of the target, top down, before anything on the target itself
//! (`gtk_propagate_event_internal`, `gtkmain.c`, 4.22.5), WebKit's own second delivery of a key the page
//! did not handle included (`pane_switch::LetThrough`); and HINT labels the host's rectangle, which is
//! the `WebView`'s.

use std::cell::RefCell;

use gtk4::glib;
use gtk4::prelude::*;
use gtk4::subclass::prelude::*;

/// The host's CSS node name. No rule in `theme::gtk_css` may name it (its tests hold that).
pub(crate) const CSS_NAME: &str = "webhost";

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct WebHost {
        /// The one child, set once by [`super::WebHost::new`] and unparented in `dispose`.
        pub(super) content: RefCell<Option<gtk4::Widget>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for WebHost {
        const NAME: &'static str = "EitriWebHost";
        type Type = super::WebHost;
        type ParentType = gtk4::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name(super::CSS_NAME);
            // Measures the child and allocates it the host's whole size at (0,0), no transform.
            klass.set_layout_manager_type::<gtk4::BinLayout>();
        }
    }

    impl ObjectImpl for WebHost {
        fn dispose(&self) {
            // Taken out of the `RefCell` first: an unparent unrealizes, and nothing that runs then
            // should find it borrowed (`ModuleGrid`'s own `dispose` does the same).
            let content = self.content.borrow_mut().take();
            if let Some(content) = content {
                content.unparent();
            }
        }
    }

    impl WidgetImpl for WebHost {
        /// A grab on the host lands on its one child, as `GtkPopoverBin`'s does
        /// (`widget_class->grab_focus = gtk_widget_grab_focus_child`, `gtkpopoverbin.c:319`, GTK
        /// 4.22.5): the host itself is never focusable, so GTK's default would refuse it.
        fn grab_focus(&self) -> bool {
            // Cloned out first: the grab runs focus handlers, and none should find it borrowed.
            let content = self.content.borrow().clone();
            content.is_some_and(|content| content.grab_focus())
        }
    }
}

glib::wrapper! {
    // `pub`, not `pub(crate)`: `ObjectSubclass::Type` must be at least as visible as the subclass.
    // The module itself is private to the crate.
    pub struct WebHost(ObjectSubclass<imp::WebHost>)
        @extends gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl WebHost {
    /// `content` -- a module's `WebView`, or the notice standing in for it where WebKit cannot start
    /// (`webkit_sandbox`) -- at (0,0) of a new host of its own: the child's margins cleared, its
    /// alignment `Fill`. This is `content`'s first and only parent, as the grid is the host's
    /// (`module_grid`'s module doc: nothing is ever reparented). Panics if `content` already has a
    /// parent: a second parent is a wiring mistake that would reparent, and unrealize, the `WebView`.
    pub(crate) fn new(content: &impl IsA<gtk4::Widget>) -> Self {
        let content: gtk4::Widget = content.clone().upcast();
        assert!(
            content.parent().is_none(),
            "a web module's content gets exactly one parent, its WebHost"
        );
        let host: Self = glib::Object::new();
        content.set_margin_start(0);
        content.set_margin_end(0);
        content.set_margin_top(0);
        content.set_margin_bottom(0);
        content.set_halign(gtk4::Align::Fill);
        content.set_valign(gtk4::Align::Fill);
        content.set_parent(&host);
        *host.imp().content.borrow_mut() = Some(content);
        host
    }

    /// The one child [`WebHost::new`] was given: the module's focus target (`ModuleGrid::add`).
    pub(crate) fn content(&self) -> Option<gtk4::Widget> {
        self.imp().content.borrow().clone()
    }
}
