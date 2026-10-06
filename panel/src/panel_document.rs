//! The panel's document: the embedded bundle, the content security policy derived from it, and the
//! theme block written into the page before its first frame. Nothing here touches a toolkit, so every
//! host loads exactly the same bytes.

/// The panel document's base URI. A secure context, so `navigator.clipboard` exists and every `y`
/// reaches the clipboard -- with no base the document's origin is opaque and it does not (GUI pass,
/// 2026-09-25). `.invalid` never resolves (RFC 2606): nothing is ever fetched from it, and a
/// relative link is still a link click that the host's navigation guard hands to `link_clicked`.
pub const PANEL_BASE_URI: &str = "https://eitri.invalid/";

/// The bundle and the content security policy derived from it, computed once.
pub struct PanelDocument {
    html: &'static str,
    csp: String,
}

impl PanelDocument {
    pub fn new(html: &'static str) -> Self {
        PanelDocument {
            html,
            csp: crate::panel_csp::content_security_policy_for(html),
        }
    }

    /// The panel's Content-Security-Policy, the same string wherever it is applied (the page's own
    /// `<meta>` and the host's default policy for the view): the browser enforces both, so a script
    /// allowed by only one would still be blocked.
    pub fn csp(&self) -> &str {
        &self.csp
    }

    /// The panel document with `vars` already inlined, so the first frame the page paints -- on a cold
    /// start or after a reload -- is in nvim's colours instead of an unstyled white page.
    /// `applyTheme` later sets the same variables inline on `:root`, which overrides this block for
    /// live colorscheme changes.
    ///
    /// Inserted directly after the opening `<head>`, not before `</head>`: the single-file build
    /// inlines its script into `<head>`, and that script contains the literal `<head></head>`
    /// (DOMPurify), so the first `</head>` in the file is not the document's.
    ///
    /// The CSP `<meta>` goes in first, ahead of even the theme `<style>` -- a `<meta
    /// http-equiv="Content-Security-Policy">` only governs what loads *after* it in the document, so it
    /// must be the very first thing `<head>` contains, before the single-file build's own inlined
    /// `<script>`/`<style>`.
    pub fn themed(&self, vars: &[(String, String)]) -> String {
        let declarations: String = vars.iter().map(|(name, value)| format!("{name}:{value};")).collect();
        let style = format!("<style id=\"nv-theme\">:root{{{declarations}}}</style>");
        let csp_meta = format!("<meta http-equiv=\"Content-Security-Policy\" content=\"{}\">", self.csp);
        match self.html.find("<head>") {
            Some(at) => {
                let insert = at + "<head>".len();
                format!("{}{}{}{}", &self.html[..insert], csp_meta, style, &self.html[insert..])
            }
            None => {
                eprintln!("[agent_panel] the panel document has no <head>; loading it without an inline theme");
                self.html.to_string()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars() -> Vec<(String, String)> {
        vec![("--bg".to_string(), "#112233".to_string())]
    }

    #[test]
    fn a_document_with_a_head_gets_the_policy_meta_first_and_then_the_theme() {
        let document = PanelDocument::new("<html><head><script>1</script></head></html>");
        let themed = document.themed(&vars());
        let meta = format!(
            "<meta http-equiv=\"Content-Security-Policy\" content=\"{}\">",
            document.csp()
        );
        let expected_start = format!("<html><head>{meta}<style id=\"nv-theme\">:root{{--bg:#112233;}}</style><script>");
        assert!(themed.starts_with(&expected_start), "{themed}");
        assert!(themed.ends_with("1</script></head></html>"), "{themed}");
    }

    #[test]
    fn a_document_with_no_head_comes_back_unchanged() {
        let document = PanelDocument::new("<script>1</script>");
        assert_eq!(document.themed(&vars()), "<script>1</script>");
    }
}
