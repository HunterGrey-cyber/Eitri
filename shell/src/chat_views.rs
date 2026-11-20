//! Every extra chat view's page (`agent#2` to `agent#9`), keyed by the module id it was made for.
//!
//! A page is built once for one view and answers for that view only: its `eitriAgent` handler was given
//! the view when the page was built (`agent_panel::build_chat_page`), and nothing can change that later.
//! So a page that leaves the layout waits here, idle, under the same id, and only a new view of that same
//! number takes it back; any other number gets a page of its own. Moving a page to another id would make
//! its answers count for a view it was never built for.
//!
//! Generic over the page so the pool's rules are tested without a display: the window keeps a [`ChatPage`] here,
//! the tests a plain value.

use crate::gtk_host::instance_pool::Retire;

/// An extra chat view's page: its web view and the page the panel drives in it.
#[derive(Clone)]
pub(crate) struct ChatPage {
    pub(crate) view: webkit6::WebView,
    pub(crate) page: std::rc::Rc<eitri_panel::panel_page::PanelPage>,
}

impl ChatPage {
    /// The page `agent_panel::build_chat_page` built, as the pool keeps it.
    pub(crate) fn of((view, page): (webkit6::WebView, std::rc::Rc<eitri_panel::panel_page::PanelPage>)) -> ChatPage {
        ChatPage { view, page }
    }
}

/// Keep the page for reuse: a kill closes its view's tabs before the page goes idle.
impl Retire for ChatPage {
    fn on_retire(&self) {}
}

/// The tests' page: a value the pool keeps, nothing behind it.
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct KeptPage;

#[cfg(test)]
impl Retire for KeptPage {
    fn on_retire(&self) {}
}
