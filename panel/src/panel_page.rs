//! The panel's page and the typing-cadence pacer in front of it, as one value, and the two traits a
//! host implements to carry the panel: [`PageSurface`] (what the page can be asked to do) and
//! [`PanelHost`] (what the panel asks of the process around it). Every envelope goes through the pacer
//! of the page it is sent to, so the two travel together instead of the pacer being looked up by the
//! page. No toolkit type appears here; the WebKitGTK implementations live next to the window code.

use std::cell::RefCell;
use std::rc::Rc;

use eitri_core::panel_cadence::DEFAULT_CADENCE_HZ;

use crate::panel_pacer::{Pacer, Sink};

/// The panel's page: one web view showing the `agent-ui/web` bundle.
///
/// **Contracts every implementation keeps.** Everything runs on one thread. `send` is asynchronous: it
/// never delivers a page message, and never calls back into the panel, before it returns -- the panel
/// may call it while holding its own state. Envelopes sent before the page said `ready` may be lost;
/// the panel holds what matters until then.
pub trait PageSurface {
    /// Run `window.__eitriDispatch(<json>)` in the page if it installed one; a page without it loses the envelope.
    fn send(&self, envelope_json: &str);
    /// Whether the page is on screen; the panel marks a tab seen only while it is.
    fn is_visible(&self) -> bool;
    /// Replace the document: the themed bundle, or the page shown after repeated crashes.
    fn load_document(&self, html: &str, base_uri: &str);
    /// The view's own background behind the page.
    fn set_background(&self, rgb: [u8; 3]);
}

/// What the panel asks of the process around it.
pub trait PanelHost {
    /// Open an address the panel already checked (`web_url` / `clicked_link_url`) in the system's handler.
    fn open_external(&self, url: &str);
}

pub struct PanelPage {
    surface: Rc<dyn PageSurface>,
    pacer: RefCell<Pacer>,
}

impl PanelPage {
    pub fn new(surface: Rc<dyn PageSurface>) -> Self {
        PanelPage {
            surface,
            pacer: RefCell::new(Pacer::new(Some(DEFAULT_CADENCE_HZ))),
        }
    }

    pub fn pacer(&self) -> &RefCell<Pacer> {
        &self.pacer
    }

    /// Sends `payload` to the page, never delayed, behind anything the pacer holds. A re-entrant call --
    /// the pacer is borrowed only across plain sink sends, so none is expected -- goes straight out rather
    /// than panicking the tick.
    pub fn send(&self, payload: &str) {
        match self.pacer.try_borrow_mut() {
            Ok(mut pacer) => pacer.send_immediate(&self.sink(), payload),
            Err(_) => {
                eprintln!("[panel_pacer] BUG: the pacer was borrowed when an envelope was sent; sending it unordered");
                self.sink().send(payload);
            }
        }
    }

    /// Hands the page whatever the pacer holds, for a caller that sends its own (guarded) script.
    pub fn flush(&self) {
        if let Ok(mut pacer) = self.pacer.try_borrow_mut() {
            pacer.flush(&self.sink());
        };
    }

    /// Straight to the page, past the pacer, for the two callers that either already flushed it
    /// (`dispatch`) or carry no stream state to order behind (`set_theme`, which never flushed).
    pub fn send_direct(&self, payload: &str) {
        self.surface.send(payload);
    }

    pub fn is_visible(&self) -> bool {
        self.surface.is_visible()
    }

    pub fn load_document(&self, html: &str, base_uri: &str) {
        self.surface.load_document(html, base_uri);
    }

    pub fn set_background(&self, rgb: [u8; 3]) {
        self.surface.set_background(rgb);
    }

    /// The pacer's sink for this page, for the pump's stream sends.
    pub fn sink(&self) -> SurfaceSink<'_> {
        SurfaceSink(&*self.surface)
    }
}

/// The page as a [`Sink`]: what the pacer releases goes through [`PageSurface::send`].
pub struct SurfaceSink<'a>(&'a dyn PageSurface);

impl Sink for SurfaceSink<'_> {
    fn send(&self, payload: &str) {
        self.0.send(payload);
    }
}
