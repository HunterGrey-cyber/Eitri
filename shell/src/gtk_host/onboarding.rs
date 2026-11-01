//! The first-start screen, drawn: one panel in the window's overlay that shows what the controller's view
//! ([`OnboardingView`]) says. The state machine, the wording and the ops are the core's and `onboarding_host`'s; this
//! file only draws plain text.
//!
//! **Text.** Everything on the screen is a tmux path, an owner of a mapping, a compositor's answer or the screen's own
//! wording, so every label is set as text, never as markup, in one function ([`set_row_text`]). A sentence or a line of
//! the file wraps, so none of it is cut; a label that names something short may ellipsize. No label asks for a width of
//! its own.
//!
//! **Keys.** The window's prefix layer already reads every key at capture and hands it to the controller, which
//! answers it for the screen; there is no key layer here. The screen's focus target is the panel itself, a plain box that
//! can take the keys and has no input-method context, so nothing composes on it and a key goes through unchanged.
//! Every preset, every key the footer names and every key a step's own text offers is also a button; a click is posted
//! as the same key by the host (`on_button`), so the state machine has one input. The buttons never take the focus
//! themselves, and the footer line says only what no button does.
//!
//! **Size.** The panel is an overlay child, which no part of the window's own minimum size measures. It is at most 90%
//! of the overlay across (and at most [`MAX_WIDTH`]) and 80% down, fitted again by a frame tick while it is up, and
//! what does not fit scrolls: the scroller propagates no natural width, so a long line never widens the window. It is
//! drawn opaque (a CSS background, not transparency), so the editor's subsurface check holds under it.
//!
//! **Scrolling.** The keys are the controller's, so the scroller never hears them: the screen's scroll keys come here
//! as [`OnboardingScreen::scroll`]. Until one of them is used on a page, every view that replaces the text brings the
//! presets into view once it is laid out: all of them (with their question) when they fit the visible part, else the
//! chosen one. A row that fills in above the presets after the page came up (`claude` not found, a late answer)
//! pushes them down, and this is what puts them back; once the person scrolls, the text stays where they left it.
//! A new page of the screen starts at the top.
//!
//! known limit: GTK layout, so no headless test holds the placement or the refit; the conformance tests hold that the
//! screen asks for no width of its own and that a short window scrolls to the chosen preset and by the keys.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use eitri_core::onboarding::{OnboardingView, Scroll, ViewButton, ViewItem, ViewKey, ViewRow};
use gtk4::glib;
use gtk4::prelude::*;

/// The share of the window's width the panel may take, in percent.
const MAX_WIDTH_PERCENT: i32 = 90;
/// The share of the window's height the panel may take, in percent.
const MAX_HEIGHT_PERCENT: i32 = 80;
/// The widest the panel gets on a large window: a line of this width is still readable.
const MAX_WIDTH: i32 = 860;
/// What the panel's own padding and border take from its width.
const FRAME_WIDTH: i32 = 48;
/// What the panel's padding, border and the gaps between its parts take from its height.
const FRAME_HEIGHT: i32 = 48;
/// The most characters a label asks for before it wraps or ellipsizes: no label asks for the panel's width.
const LABEL_CHARS: i32 = 40;
/// The gap between the blocks of the screen's text, in pixels.
const CONTENT_SPACING: i32 = 6;
/// How far `j` or `k` moves the text, in pixels: about three lines of it.
const LINE_SCROLL: f64 = 48.0;
/// The part of the visible text a page key moves, so a line stays to read on from.
const PAGE_SCROLL: f64 = 0.9;

type ButtonFn = Rc<dyn Fn(ViewKey)>;
type ButtonSlot = Rc<RefCell<Option<ButtonFn>>>;

/// The one place a label's text is set: as text, never as markup.
fn set_row_text(label: &gtk4::Label, text: &str) {
    label.set_text(text);
}

/// A label that shows `text` as written, takes no part in input and asks for no width of its own: it wraps
/// (`wrap`), or ellipsizes on one line.
fn text_label(class: &str, text: &str, wrap: bool) -> gtk4::Label {
    let label = gtk4::Label::new(None);
    label.set_use_markup(false);
    if wrap {
        label.set_wrap(true);
        label.set_wrap_mode(gtk4::pango::WrapMode::WordChar);
    } else {
        label.set_single_line_mode(true);
        label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    }
    label.set_width_chars(0);
    label.set_max_width_chars(LABEL_CHARS);
    label.set_xalign(0.0);
    label.set_can_focus(false);
    label.set_selectable(false);
    // One class per word: GTK takes a string with a space in it as the name of a single class.
    for class in class.split_whitespace() {
        label.add_css_class(class);
    }
    set_row_text(&label, text);
    label
}

/// The panel's widgets (each a handle to the one widget), shared with the tick that fits them again.
#[derive(Clone)]
struct Parts {
    frame: gtk4::Box,
    title: gtk4::Label,
    step: gtk4::Label,
    scroller: gtk4::ScrolledWindow,
    content: gtk4::Box,
    help: gtk4::Label,
    footer: gtk4::Label,
    buttons: gtk4::Box,
    /// The presets to bring into view, until a frame has laid them out and scrolled to them.
    reveal: Rc<RefCell<Option<Reveal>>>,
}

/// What the screen keeps in view: the chosen preset's row, and the block of the question and all the presets.
#[derive(Clone)]
struct Reveal {
    row: gtk4::Widget,
    block: gtk4::Widget,
}

/// What tells one page of the screen from another: its title, its counter and its question.
type Page = (String, String, Option<String>);

pub(crate) struct OnboardingScreen {
    parts: Parts,
    on_button: ButtonSlot,
    /// The page last shown, and the number of its chosen preset.
    shown: RefCell<Option<(Page, Option<u32>)>>,
    /// Whether a scroll key moved the text since the page came up: from then on the text stays where it is.
    scrolled_by_hand: Cell<bool>,
    /// The size of the overlay the panel was last fitted to.
    fitted: Rc<Cell<(i32, i32)>>,
    /// The tick that fits the panel again when the overlay's size changes, while the panel is up.
    tick: RefCell<Option<gtk4::TickCallbackId>>,
}

impl OnboardingScreen {
    /// The panel, hidden until [`OnboardingScreen::show`] or [`OnboardingScreen::grab_keys`].
    pub(crate) fn new() -> Rc<OnboardingScreen> {
        let frame = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        frame.add_css_class("onboarding-screen");
        frame.set_halign(gtk4::Align::Center);
        frame.set_valign(gtk4::Align::Center);
        // The keys' target: a box takes the focus and no text widget's input-method context comes with it.
        frame.set_can_focus(true);
        frame.set_focusable(true);
        frame.set_visible(false);
        let head = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
        let title = text_label("onboarding-title", "", false);
        title.set_hexpand(true);
        let step = text_label("onboarding-step", "", false);
        head.append(&title);
        head.append(&step);
        let content = gtk4::Box::new(gtk4::Orientation::Vertical, CONTENT_SPACING);
        let scroller = gtk4::ScrolledWindow::new();
        scroller.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        // No natural width of its own: a long line scrolls or wraps and never widens the window.
        scroller.set_propagate_natural_width(false);
        scroller.set_min_content_width(0);
        scroller.set_propagate_natural_height(true);
        scroller.set_can_focus(false);
        scroller.set_child(Some(&content));
        let help = text_label("onboarding-help", "", true);
        let footer = text_label("onboarding-footer", "", true);
        let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        frame.append(&head);
        frame.append(&scroller);
        frame.append(&help);
        frame.append(&footer);
        frame.append(&buttons);
        Rc::new(OnboardingScreen {
            parts: Parts {
                frame,
                title,
                step,
                scroller,
                content,
                help,
                footer,
                buttons,
                reveal: Rc::new(RefCell::new(None)),
            },
            on_button: Rc::new(RefCell::new(None)),
            shown: RefCell::new(None),
            scrolled_by_hand: Cell::new(false),
            fitted: Rc::new(Cell::new((0, 0))),
            tick: RefCell::new(None),
        })
    }

    /// The panel's widget, for the window's overlay.
    pub(crate) fn widget(&self) -> &gtk4::Widget {
        self.parts.frame.upcast_ref()
    }

    /// What a click on a shortcut's button does: it is told the shortcut's key.
    pub(crate) fn on_button(&self, f: impl Fn(ViewKey) + 'static) {
        *self.on_button.borrow_mut() = Some(Rc::new(f));
    }

    /// Whether the keys are on the panel or in something inside it.
    pub(crate) fn has_keys(&self) -> bool {
        let Some(root) = self.parts.frame.root() else {
            return false;
        };
        let Some(focus) = RootExt::focus(&root) else {
            return false;
        };
        focus == *self.parts.frame.upcast_ref::<gtk4::Widget>() || focus.is_ancestor(&self.parts.frame)
    }

    /// Shows `view`, replacing what the panel held, or takes the panel down for `None`.
    pub(crate) fn show(&self, view: Option<&OnboardingView>) {
        let Some(view) = view else {
            self.hide();
            return;
        };
        let parts = &self.parts;
        set_row_text(&parts.title, &view.title);
        set_row_text(&parts.step, &view.step);
        while let Some(child) = parts.content.first_child() {
            parts.content.remove(&child);
        }
        if !view.rows.is_empty() {
            parts.content.append(&rows_grid(&view.rows));
        }
        // The question and the presets under it are one block, so they can be kept in view together.
        let block = gtk4::Box::new(gtk4::Orientation::Vertical, CONTENT_SPACING);
        if let Some(question) = &view.question {
            block.append(&text_label("onboarding-question", question, true));
        }
        let mut chosen = None;
        if !view.items.is_empty() {
            let (list, row) = items_grid(&view.items, &self.on_button);
            block.append(&list);
            chosen = row;
        }
        if block.first_child().is_some() {
            parts.content.append(&block);
        }
        for line in &view.body {
            parts.content.append(&text_label("onboarding-body", line, true));
        }
        for line in &view.status {
            parts.content.append(&text_label("onboarding-status", line, true));
        }
        set_row_text(&parts.help, view.help.as_deref().unwrap_or(""));
        parts.help.set_visible(view.help.is_some());
        // The rest of the footer is the buttons under it.
        let hint = view.footer_hint();
        set_row_text(&parts.footer, &hint);
        parts.footer.set_visible(!hint.is_empty());
        self.set_buttons(&view.buttons);
        parts.frame.set_visible(true);
        parts.fit(&self.fitted);
        self.place(view, chosen, block.upcast());
        if self.tick.borrow().is_none() {
            let (parts, fitted) = (self.parts.clone(), self.fitted.clone());
            let id = self.parts.frame.add_tick_callback(move |_, _| {
                if parts.parent_size() != fitted.get() {
                    parts.fit(&fitted);
                }
                parts.reveal_chosen();
                glib::ControlFlow::Continue
            });
            *self.tick.borrow_mut() = Some(id);
        }
    }

    /// Decides where the text stands after [`OnboardingScreen::show`]: a new page starts at the top, and the presets
    /// come into view when the page or the choice changed, or while no scroll key has moved the text on this page (a
    /// view that only filled in a row above them leaves the text where the person scrolled it).
    fn place(&self, view: &OnboardingView, chosen: Option<(u32, gtk4::Widget)>, block: gtk4::Widget) {
        let page: Page = (view.title.clone(), view.step.clone(), view.question.clone());
        let number = chosen.as_ref().map(|(number, _)| *number);
        let last = self.shown.borrow_mut().replace((page.clone(), number));
        let new_page = last.as_ref().is_none_or(|(shown, _)| *shown != page);
        if new_page {
            self.parts.scroller.vadjustment().set_value(0.0);
            self.scrolled_by_hand.set(false);
        }
        let moved = new_page || last.is_some_and(|(_, shown)| shown != number);
        // The widgets were just replaced, so a reveal still waiting names one that is gone.
        let waiting = self.parts.reveal.borrow().is_some();
        *self.parts.reveal.borrow_mut() = if moved || waiting || !self.scrolled_by_hand.get() {
            chosen.map(|(_, row)| Reveal { row, block })
        } else {
            None
        };
    }

    /// Moves the text by one of the screen's scroll keys; the adjustment keeps it within the text.
    pub(crate) fn scroll(&self, by: Scroll) {
        self.scrolled_by_hand.set(true);
        // A reveal still waiting for the new widgets' layout must not pull the text back from where the key put it.
        *self.parts.reveal.borrow_mut() = None;
        let adjustment = self.parts.scroller.vadjustment();
        let page = adjustment.page_size() * PAGE_SCROLL;
        let step = match by {
            Scroll::LineUp => -LINE_SCROLL,
            Scroll::LineDown => LINE_SCROLL,
            Scroll::PageUp => -page,
            Scroll::PageDown => page,
        };
        adjustment.set_value(adjustment.value() + step);
    }

    fn hide(&self) {
        self.parts.frame.set_visible(false);
        *self.shown.borrow_mut() = None;
        *self.parts.reveal.borrow_mut() = None;
        if let Some(id) = self.tick.borrow_mut().take() {
            id.remove();
        }
    }

    /// Makes the panel visible, then gives it the keys. The controller's `Focus` op runs before the `Chrome` op that
    /// fills the panel, so the panel shows itself here, and is refused only when the toolkit does not take the grab.
    pub(crate) fn grab_keys(&self) -> bool {
        self.parts.frame.set_visible(true);
        self.parts.frame.grab_focus()
    }

    fn set_buttons(&self, buttons: &[ViewButton]) {
        let parts = &self.parts;
        while let Some(child) = parts.buttons.first_child() {
            parts.buttons.remove(&child);
        }
        parts.buttons.set_visible(!buttons.is_empty());
        for button in buttons {
            let widget = gtk4::Button::new();
            widget.set_child(Some(&text_label("onboarding-button-label", &button.label, false)));
            widget.add_css_class("onboarding-button");
            // A click never moves the keys off the panel: the keys are the controller's.
            widget.set_focus_on_click(false);
            widget.set_can_focus(false);
            click_posts(&widget, button.key, &self.on_button);
            parts.buttons.append(&widget);
        }
    }
}

impl Parts {
    /// Scrolls the presets into view once they have been laid out: the whole block when it fits the visible part of
    /// the text, else the chosen row. A tick runs before the frame's layout, so a row made since the last frame has no
    /// height yet and waits for the next one.
    fn reveal_chosen(&self) {
        let Some(Reveal { row, block }) = self.reveal.borrow().clone() else {
            return;
        };
        if row.height() == 0 {
            return;
        }
        *self.reveal.borrow_mut() = None;
        let adjustment = self.scroller.vadjustment();
        let whole = block.compute_bounds(&self.content);
        let target = match whole {
            Some(bounds) if f64::from(bounds.height()) <= adjustment.page_size() => Some(bounds),
            _ => row.compute_bounds(&self.content),
        };
        let Some(bounds) = target else {
            return;
        };
        let top = f64::from(bounds.y());
        let bottom = top + f64::from(bounds.height());
        if top < adjustment.value() {
            adjustment.set_value(top);
        } else if bottom > adjustment.value() + adjustment.page_size() {
            adjustment.set_value(bottom - adjustment.page_size());
        }
    }

    /// The size of the overlay the panel sits in, `(0, 0)` while it has none.
    fn parent_size(&self) -> (i32, i32) {
        self.frame
            .parent()
            .map_or((0, 0), |parent| (parent.width(), parent.height()))
    }

    /// Sizes the panel to the window it is in, and notes in `fitted` the size it fitted to. A window not laid out
    /// yet (no width) leaves the panel asking for nothing.
    fn fit(&self, fitted: &Cell<(i32, i32)>) {
        let (width, height) = self.parent_size();
        fitted.set((width, height));
        if width <= 0 || height <= 0 {
            self.frame.set_size_request(-1, -1);
            return;
        }
        let across = (width * MAX_WIDTH_PERCENT / 100).min(MAX_WIDTH) - FRAME_WIDTH;
        self.frame.set_size_request(across.max(1), -1);
        let height_of = |widget: &gtk4::Widget| {
            if widget.is_visible() {
                widget.measure(gtk4::Orientation::Vertical, across.max(1)).1
            } else {
                0
            }
        };
        let others = height_of(self.title.upcast_ref())
            + height_of(self.help.upcast_ref())
            + height_of(self.footer.upcast_ref())
            + height_of(self.buttons.upcast_ref());
        let room = (height * MAX_HEIGHT_PERCENT / 100 - others - FRAME_HEIGHT).max(1);
        self.scroller.set_max_content_height(room);
    }
}

/// The found rows: what each is, what was found and a note beside it.
fn rows_grid(rows: &[ViewRow]) -> gtk4::Grid {
    let grid = gtk4::Grid::new();
    grid.set_column_spacing(12);
    grid.set_row_spacing(2);
    for (line, row) in rows.iter().enumerate() {
        let line = i32::try_from(line).unwrap_or(i32::MAX);
        grid.attach(&text_label("onboarding-row-label", &row.label, false), 0, line, 1, 1);
        let text = text_label("onboarding-row-text", &row.text, true);
        text.set_hexpand(true);
        grid.attach(&text, 1, line, 1, 1);
        if !row.note.is_empty() {
            grid.attach(&text_label("onboarding-row-note", &row.note, true), 2, line, 1, 1);
        }
    }
    grid
}

/// A click on `widget` posts `key` through the screen's one button callback.
fn click_posts(widget: &gtk4::Button, key: ViewKey, callback: &ButtonSlot) {
    let callback = callback.clone();
    widget.connect_clicked(move |_| {
        let callback = callback.clone();
        // After the click's own emission: the answer rebuilds the widgets that were clicked.
        glib::idle_add_local_once(move || {
            let call = callback.borrow().clone();
            if let Some(call) = call {
                call(key);
            }
        });
    });
}

/// The presets, one button each (a click chooses it, as its number does): a marker on the chosen one, its number and
/// name, what it does and its tag. The parts of a row line up with the other rows' through one size group per column.
/// Also gives the chosen preset's number and its row.
fn items_grid(items: &[ViewItem], on_button: &ButtonSlot) -> (gtk4::Box, Option<(u32, gtk4::Widget)>) {
    let list = gtk4::Box::new(gtk4::Orientation::Vertical, 2);
    let columns: Vec<gtk4::SizeGroup> = (0..5)
        .map(|_| gtk4::SizeGroup::new(gtk4::SizeGroupMode::Horizontal))
        .collect();
    let mut chosen = None;
    for item in items {
        let class = if item.selected {
            "onboarding-item selected"
        } else {
            "onboarding-item"
        };
        let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 10);
        let marker = text_label(class, if item.selected { ">" } else { "" }, false);
        marker.set_width_chars(1);
        let number = text_label(class, &format!("{}", item.number), false);
        let name = text_label(class, &item.name, true);
        let summary = text_label("onboarding-item-summary", &item.summary, true);
        summary.set_hexpand(true);
        let tag = text_label("onboarding-item-tag", item.tag.as_deref().unwrap_or(""), false);
        let parts: [&gtk4::Label; 5] = [&marker, &number, &name, &summary, &tag];
        for (column, part) in columns.iter().zip(parts) {
            column.add_widget(part);
            row.append(part);
        }
        let button = gtk4::Button::new();
        button.set_child(Some(&row));
        button.add_css_class("onboarding-item-button");
        // A click never moves the keys off the panel: the keys are the controller's.
        button.set_focus_on_click(false);
        button.set_can_focus(false);
        if let Some(key) = char::from_digit(item.number, 10) {
            click_posts(&button, ViewKey::Char(key), on_button);
        }
        list.append(&button);
        if item.selected {
            chosen = Some((item.number, button.upcast()));
        }
    }
    (list, chosen)
}

#[cfg(test)]
mod tests {
    /// A tmux path, a mapping's owner or a compositor's answer fills these labels: `<b>x</b>` shows as those
    /// characters. Read off this file's code (comments, literals and this test module blanked): no markup setter,
    /// markup never switched on, and every label's text set in exactly one function, `set_row_text`.
    #[test]
    fn onboarding_rows_are_set_as_text_never_markup() {
        use eitri_core::source_scan as scan;
        let source = scan::read_required("shell/src/gtk_host/onboarding.rs");
        let code = scan::code_only(&source.text);
        let code = scan::without(&code, &scan::modules(&code, "tests"));
        let flat: String = code.split_whitespace().collect();
        for forbidden in [
            "set_markup(",
            "set_use_markup(true)",
            "set_label(",
            "with_mnemonic(",
            "with_label(",
            "set_text_with_mnemonic(",
            "set_markup_with_mnemonic(",
            "Label::with_markup(",
            "set_tooltip_markup(",
        ] {
            assert!(!flat.contains(forbidden), "onboarding.rs calls {forbidden}");
        }
        assert!(
            flat.contains("set_use_markup(false)"),
            "markup is switched off on every label"
        );
        assert_eq!(flat.matches(".set_text(").count(), 1, "the text is set in one place");
        let setter = scan::functions(&code, "set_row_text");
        assert_eq!(setter.len(), 1);
        assert!(setter[0].contains(".set_text("), "{}", setter[0]);
        // A label made with text would set it past the one function.
        assert_eq!(flat.matches("Label::new(").count(), 1);
        assert!(flat.contains("Label::new(None)"), "labels are made empty");
    }

    /// Nothing composes on the screen: no text widget, no input-method context, no key controller of its own. The
    /// keys are the controller's, read at the window's capture.
    #[test]
    fn an_input_method_never_composes_on_the_screen() {
        use eitri_core::source_scan as scan;
        let source = scan::read_required("shell/src/gtk_host/onboarding.rs");
        let code = scan::code_only(&source.text);
        let code = scan::without(&code, &scan::modules(&code, "tests"));
        for forbidden in [
            "set_im_context",
            "IMContext",
            "IMMulticontext",
            "EventControllerKey",
            "gtk4::Entry",
            "gtk4::Text",
            "TextView",
            "SearchEntry",
            "PasswordEntry",
        ] {
            assert!(!code.contains(forbidden), "onboarding.rs uses {forbidden}");
        }
    }
}
