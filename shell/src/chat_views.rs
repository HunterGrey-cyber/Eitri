//! Every extra chat view's page (`agent#2` to `agent#9`), keyed by the module id it was made for.
//!
//! A page is built once for one view and answers for that view only: its `eitriAgent` handler was given
//! the view when the page was built (`agent_panel::build_chat_page`), and nothing can change that later.
//! So a page that leaves the layout waits here, idle, under the same id, and only a new view of that same
//! number takes it back; any other number gets a page of its own. Moving a page to another id would make
//! its answers count for a view it was never built for.
//!
//! Generic over the page so the pool's rules are tested without a display: the window keeps
//! `(WebView, Rc<PanelPage>)` here, the tests a plain value.

use crate::gtk_host::instance_pool::Retire;

/// Keep the page for reuse: a kill closes its view's tabs before the page goes idle.
impl Retire for (webkit6::WebView, std::rc::Rc<eitri_panel::panel_page::PanelPage>) {
    fn on_retire(&self) {}
}

#[cfg(test)]
impl Retire for () {
    fn on_retire(&self) {}
}
