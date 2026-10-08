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
    /// Where the theme block goes (directly after the opening `<head>`), or `None` when the bundle
    /// has no `<head>` this module is sure the parser opens as the document's own.
    head_end: Option<usize>,
    csp: String,
}

/// What a document the panel cannot be built from shows instead: no script, and the policy meta
/// first in its `<head>`. [`PanelDocument::themed`] returns it in place of the bundle.
const UNBUILDABLE_BODY: &str =
    "<body><p>The Eitri panel could not be built: its document has no usable &lt;head&gt;.</p></body>";

impl PanelDocument {
    pub fn new(html: &'static str) -> Self {
        let head_end = head_insert_point(html);
        let csp = if head_end.is_some() {
            crate::panel_csp::content_security_policy_for(html)
        } else {
            eprintln!(
                "[agent_panel] the panel document has no <head> it can place a policy in; \
                 loading a document that runs no script instead"
            );
            crate::panel_csp::script_less_policy()
        };
        PanelDocument { html, head_end, csp }
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
    ///
    /// A bundle whose `<head>` cannot be found with certainty never loads as the panel: a host with
    /// no default policy of its own (the Mac app's WKWebView) would otherwise run it with no script
    /// policy at all. It gets a small document with the script-less policy and a sentence saying
    /// what went wrong.
    pub fn themed(&self, vars: &[(String, String)]) -> String {
        let csp_meta = format!("<meta http-equiv=\"Content-Security-Policy\" content=\"{}\">", self.csp);
        let Some(insert) = self.head_end else {
            return format!("<!doctype html><html><head>{csp_meta}</head>{UNBUILDABLE_BODY}</html>");
        };
        let declarations: String = vars.iter().map(|(name, value)| format!("{name}:{value};")).collect();
        let style = format!("<style id=\"nv-theme\">:root{{{declarations}}}</style>");
        format!("{}{}{}{}", &self.html[..insert], csp_meta, style, &self.html[insert..])
    }
}

/// The byte offset just after the document's opening `<head>` tag, when everything before it is
/// something the HTML parser reads as the start of a document and nothing else: optional white space,
/// an optional `<!doctype html>`, an optional `<html>` start tag. Any other
/// prefix -- a comment, a script, a stray element or text -- could hide the `<head>` found later (a
/// `<head>` inside a comment is not one), so it refuses and the caller fails closed.
///
/// The `html` and `head` start tags are accepted in any letter case, with double-quoted or bare
/// attributes only ([`start_tag_len`]); a form that is not understood is a refusal, never a guess.
fn head_insert_point(html: &str) -> Option<usize> {
    let mut at = skip_space(html, 0);
    if html
        .as_bytes()
        .get(at..at + 9)
        .is_some_and(|w| w.eq_ignore_ascii_case(b"<!doctype"))
    {
        let rest = &html[at + 9..];
        let end = rest.find('>')?;
        let words = rest[..end].trim_matches(|c: char| c.is_ascii() && is_space(c as u8));
        if !words.eq_ignore_ascii_case("html") {
            return None;
        }
        at += 9 + end + 1;
        at = skip_space(html, at);
    }
    if let Some(len) = start_tag_len(&html[at..], "html") {
        at += len;
        at = skip_space(html, at);
    }
    let len = start_tag_len(&html[at..], "head")?;
    Some(at + len)
}

fn is_space(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | 0x0c | b'\r' | b' ')
}

/// `from` moved past HTML white space.
fn skip_space(html: &str, from: usize) -> usize {
    let mut at = from;
    while html.as_bytes().get(at).is_some_and(|b| is_space(*b)) {
        at += 1;
    }
    at
}

/// The length of the start tag `<name ...>` at the front of `s`, if it is one of the plain forms:
/// the name in any letter case, then white space or `>`; attributes that are a bare or
/// double-quoted `name` / `name=value` / `name="value"` (names of letters, digits, `-`, `_`, `:`;
/// a bare value of those characters or `.`; a quoted one holds anything but `"`); no `/`, no single
/// quote, no duplicate-or-odd syntax. Everything else is `None`.
fn start_tag_len(s: &str, name: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let open = 1 + name.len();
    if bytes.first() != Some(&b'<') || bytes.len() <= open || !bytes[1..open].eq_ignore_ascii_case(name.as_bytes()) {
        return None;
    }
    let mut at = open;
    match bytes[at] {
        b'>' => return Some(at + 1),
        b if is_space(b) => {}
        _ => return None,
    }
    let word = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b':');
    loop {
        while bytes.get(at).is_some_and(|b| is_space(*b)) {
            at += 1;
        }
        match bytes.get(at)? {
            b'>' => return Some(at + 1),
            b if word(*b) => {}
            _ => return None,
        }
        while bytes.get(at).is_some_and(|b| word(*b)) {
            at += 1;
        }
        if bytes.get(at) != Some(&b'=') {
            continue;
        }
        at += 1;
        match bytes.get(at)? {
            b'"' => {
                at += 1;
                at += s[at..].find('"')? + 1;
            }
            b if word(*b) || *b == b'.' => {
                while bytes.get(at).is_some_and(|b| word(*b) || *b == b'.') {
                    at += 1;
                }
            }
            _ => return None,
        }
        // A value must be followed by white space or the tag's end, so `a="x"b` is refused.
        if !bytes.get(at).is_some_and(|b| is_space(*b) || *b == b'>') {
            return None;
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

    fn fails_closed(html: &'static str) {
        let document = PanelDocument::new(html);
        let themed = document.themed(&vars());
        let meta = format!(
            "<meta http-equiv=\"Content-Security-Policy\" content=\"{}\">",
            document.csp()
        );
        assert!(
            themed.starts_with(&format!("<!doctype html><html><head>{meta}</head>")),
            "{html:?}: {themed}"
        );
        assert!(
            document.csp().contains("script-src 'none';"),
            "{html:?}: {}",
            document.csp()
        );
        assert!(!themed.contains("<script"), "{html:?}: {themed}");
        assert!(themed.contains("could not be built"), "{themed}");
    }

    fn takes_theme_after_head(html: &'static str, head: &str) {
        let document = PanelDocument::new(html);
        let themed = document.themed(&vars());
        let meta = format!(
            "<meta http-equiv=\"Content-Security-Policy\" content=\"{}\">",
            document.csp()
        );
        let at = html.find(head).unwrap() + head.len();
        let expected = format!(
            "{}{meta}<style id=\"nv-theme\">:root{{--bg:#112233;}}</style>{}",
            &html[..at],
            &html[at..]
        );
        assert_eq!(themed, expected);
        assert!(
            !document.csp().contains("script-src 'none';"),
            "{html:?}: {}",
            document.csp()
        );
    }

    #[test]
    fn the_normal_bundle_shape_keeps_every_byte_around_the_insert() {
        takes_theme_after_head(
            "<!doctype html>\n<html lang=\"en\">\n  <head>\n    <meta charset=\"UTF-8\" />\n    <script type=\"module\" crossorigin>1</script></head><body></body></html>",
            "<head>",
        );
    }

    #[test]
    fn the_forms_the_parser_accepts_for_the_opening_tags_are_found() {
        for (html, head) in [
            ("<HEAD><script>1</script></HEAD>", "<HEAD>"),
            ("<head lang=en><script>1</script></head>", "<head lang=en>"),
            (
                "<head lang=\"en\" data-x=\"a>b\"><script>1</script></head>",
                "<head lang=\"en\" data-x=\"a>b\">",
            ),
            ("<head\n\tlang=en\n><script>1</script></head>", "<head\n\tlang=en\n>"),
            ("<head hidden><script>1</script></head>", "<head hidden>"),
            ("<head ><script>1</script></head>", "<head >"),
            ("<!DOCTYPE HTML><HTML LANG=en><Head><script>1</script></Head>", "<Head>"),
            (
                "  \n<!doctype  html >\n<html>\n<head><script>1</script></head>",
                "<head>",
            ),
            ("<html><head><script>1</script></head>", "<head>"),
        ] {
            takes_theme_after_head(html, head);
        }
    }

    #[test]
    fn a_document_with_no_head_never_loads_as_the_panel() {
        fails_closed("<script>1</script>");
        fails_closed("");
        fails_closed("<html><body><script>1</script></body></html>");
    }

    #[test]
    fn a_head_the_parser_may_not_open_as_the_documents_is_refused() {
        for html in [
            // Inside a comment, a script, an attribute, a different element or text.
            "<!-- <head> --><script>1</script>",
            "<!-- --><head><script>1</script></head>",
            "<script>var s = '<head>';</script><head></head>",
            "<p title='<head>'>x</p><head><script>1</script></head>",
            "text <head><script>1</script></head>",
            "<body><head><script>1</script></head></body>",
            "<style>a{}</style><head><script>1</script></head>",
            // Other tags and spellings.
            "<header><script>1</script></header>",
            "<heads><script>1</script></heads>",
            "<head/><script>1</script>",
            "<head 'x'><script>1</script>",
            "<head a='x'><script>1</script>",
            "<head a=\"x\"b><script>1</script>",
            "<head a=\"x><script>1</script>",
            "<head a=x\"y b=\"z>w\"><script>1</script>",
            "<head a=><script>1</script>",
            "<head",
            "<head ",
            "<!doctype html public \"x\"><head><script>1</script></head>",
            "<!doctype html><!-- --><head><script>1</script></head>",
            // A byte-order mark is text to a tokenizer that was not given bytes; a decoder strips it, but this
            // check relies on nothing outside itself. A non-ASCII letter that merely truncates to white space.
            "\u{feff}<head><script>1</script></head>",
            "<!doctype \u{10c}html><head><script>1</script></head>",
            "<!doctype html\u{120}><head><script>1</script></head>",
            "<html a='1'><head><script>1</script></head>",
            "<html><html><head><script>1</script></head>",
        ] {
            fails_closed(html);
        }
    }
}
