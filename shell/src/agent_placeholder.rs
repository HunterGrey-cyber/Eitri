use webkit6::{WebView, prelude::*};

/// Placeholder for the future `agent-ui` module (see
/// docs/superpowers/specs/2026-09-06-shell-scaffolding-design.md's module build order:
/// neovide-editor -> shell -> agent -> agent-ui). A real chat/agent interface lands in a
/// later plan; this pane exists so `shell`'s two-pane layout and the P7 resize-throttle
/// fix (see `layout.rs`) have a genuine second `WebView`-backed panel to be tested against,
/// matching the architecture's stated intent that this pane is always WebKitGTK-hosted.
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
