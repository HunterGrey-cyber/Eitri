use webkit6::{WebView, prelude::*};

/// Builds a placeholder for `agent-ui`. A second `WebView`-backed panel lets the shell's two-pane
/// layout and resize throttling in `layout.rs` be exercised before the chat interface is built.
pub(crate) fn build_placeholder_panel() -> gtk4::Widget {
    let webview = WebView::new();
    webview.load_html(
        "<html><body style='background:#1e1e2e;color:#cdd6f4;font-family:sans-serif; \
         display:flex;align-items:center;justify-content:center;height:100vh;margin:0;'> \
         <p>agent-ui not yet built</p></body></html>",
        None,
    );
    webview.set_hexpand(true);
    webview.set_vexpand(true);
    webview.upcast()
}
