//! Native text-size actions and editor echo wiring around the shared window model.
//!
//! The model itself -- S, R, the pending write and its burst, the pages' factors, and why it has
//! no clock -- is `core/src/wm/text_size.rs`; this file only wires it to GTK, nvim and WebKit.

use eitri_core::layout::ModuleId;
use eitri_core::wm::{Driver, EditorEvent, HostEvent, TextSizeEvent, WinId};
use gtk4::glib;
use gtk4::prelude::*;
use neovide_editor::NeovideEditorPane;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use webkit6::prelude::*;

#[allow(unused_imports)]
pub(crate) use eitri_core::wm::text_size::{route_focused_module, TextSizeTarget, TextStep, TEXT_SIZE_ACTIONS};

/// Returns the live panel font size for the colorscheme listener's `panel_tokens` closure
/// in `main.rs`. A colorscheme change must rebuild the other tokens without resetting zoom,
/// so the closure reads the size from the zoom controls rather than a captured startup base.
pub(crate) fn live_panel_font_size_px(panel_px: &Cell<f32>) -> f32 {
    panel_px.get()
}

/// Reports inputs to the live window model: the three `app.text-*` accelerators (registered
/// exactly like `window_mode::WindowModes` registers F11) and following `g:neovide_scale_factor`
/// back from nvim. The controller's prefix routing decides editor-only and panel-only steps
/// directly rather than going through an app action, since those two are one-pane operations
/// the prefix has already resolved a target for (via `route_focused_module`). The driver owns
/// that model; this glue only keeps the actions, reporting hooks and each page's weak view.
pub(crate) struct TextSizeController {
    driver: Rc<Driver>,
    /// Each page's view, by module, given its zoom level whenever the state moves. Weak: the grid
    /// owns the view, and a killed page's view may go.
    pages: RefCell<BTreeMap<ModuleId, glib::WeakRef<webkit6::WebView>>>,
}

impl TextSizeController {
    pub(crate) fn install(
        app: &gtk4::Application,
        editor: Option<Rc<NeovideEditorPane>>,
        driver: Rc<Driver>,
    ) -> Rc<Self> {
        let this = Rc::new(Self {
            driver,
            pages: RefCell::default(),
        });

        // App-level, so GTK takes them before nvim or the panel sees the key (see
        // `TEXT_SIZE_ACTIONS`'s own doc for why each accelerator is bound).
        for (name, step) in TEXT_SIZE_ACTIONS.iter().copied() {
            let action = gtk4::gio::SimpleAction::new(name, None);
            let weak = Rc::downgrade(&this);
            action.connect_activate(move |_, _| {
                if let Some(this) = weak.upgrade() {
                    this.driver
                        .post(HostEvent::TextSize(TextSizeEvent::Both { win: WinId::MAIN, step }));
                }
            });
            app.add_action(&action);
            let accels: Vec<String> = eitri_core::keymap::root::chords_of(name)
                .iter()
                .map(|root| crate::gtk_host::keys::gtk_accel(&root.chord()))
                .collect();
            app.set_accels_for_action(
                &format!("app.{name}"),
                &accels.iter().map(String::as_str).collect::<Vec<_>>(),
            );
        }

        if let Some(editor) = &editor {
            let weak = Rc::downgrade(&this);
            // `nvim_reported_s` itself is unit-tested extensively in the model, but this closure --
            // reporting it so the controller re-pushes the panel px -- is GTK wiring reached only
            // from `neovide-editor`'s tick callback, with no headless harness. A future edit dropping
            // the ScaleReported post would omit both the state update and the panel px update;
            // that compiles and passes the model's tests. Only a GUI pass (change S externally,
            // e.g. `:let`, and watch the panel follow) would catch it.
            editor.on_scale_factor_setting(move |value| {
                let Some(this) = weak.upgrade() else { return };
                // A synchronous echo during SetScale joins the driver's queue behind the whole batch.
                this.driver.post(HostEvent::Editor(EditorEvent::ScaleReported {
                    win: WinId::MAIN,
                    scale: value as f64,
                }));
            });
        }

        this
    }

    /// A wheel step names the pane under the pointer, which need not hold the keys.
    pub(crate) fn pane_step(&self, module: ModuleId, step: TextStep) {
        self.driver.post(HostEvent::TextSize(TextSizeEvent::Pane {
            win: WinId::MAIN,
            module,
            step,
        }));
    }

    /// The page `page` now has a view: it follows the editor from here on, at 1.0 relative to it the
    /// first time (a view built again for the same module keeps the factor it had). The level is set
    /// again at every committed load, though WebKit keeps it across loads (see
    /// [`crate::plugin_page::set_zoom`]).
    pub(crate) fn attach_page(self: &Rc<Self>, page: ModuleId, view: &webkit6::WebView) {
        let known = self
            .pages
            .borrow()
            .get(&page)
            .and_then(glib::WeakRef::upgrade)
            .is_some_and(|known| known == *view);
        if known {
            return;
        }
        self.pages.borrow_mut().insert(page.clone(), view.downgrade());
        self.driver.post(HostEvent::TextSize(TextSizeEvent::PageAttached {
            win: WinId::MAIN,
            module: page.clone(),
        }));
        {
            let weak = Rc::downgrade(self);
            let page = page.clone();
            view.connect_load_changed(move |view, event| {
                if event != webkit6::LoadEvent::Committed {
                    return;
                }
                let Some(this) = weak.upgrade() else { return };
                let level = this.driver.query(|c| c.page_zoom(WinId::MAIN, &page)).unwrap_or(1.0);
                crate::plugin_page::set_zoom(view, level);
            });
        }
        let level = self.driver.query(|c| c.page_zoom(WinId::MAIN, &page)).unwrap_or(1.0);
        crate::plugin_page::set_zoom(view, level);
    }

    /// The panel's un-zoomed size moved (it follows the editor's `guifont`, or the desktop's text scaling
    /// moved the conversion): the live size is re-derived from it with the zoom kept.
    pub(crate) fn set_base(&self, base: f32) {
        self.driver.post(HostEvent::TextSize(TextSizeEvent::Base {
            win: WinId::MAIN,
            px: base,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn live_panel_font_size_px_reads_the_shared_cell_not_a_snapshot() {
        let cell = Cell::new(14.0);
        assert_eq!(live_panel_font_size_px(&cell), 14.0);
        cell.set(22.4);
        assert_eq!(
            live_panel_font_size_px(&cell),
            22.4,
            "must read the CURRENT value, not one captured earlier"
        );
    }
}
