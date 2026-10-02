//! The agent panel's Content-Security-Policy, with every inline script pinned by its SHA-256.
//!
//! The panel's single-file bundle inlines its whole frontend as one `<script>`. Allowing inline
//! script by keyword would let anything that slips past the markdown sanitizer -- an `on…=`
//! handler, a `javascript:` URL, an injected `<script>` -- run with the page's full reach into the
//! host. Pinning the hash of the bundle's own script lets exactly that script run and nothing else.
//!
//! The hash is computed from the document as it is embedded in the binary, not by the build
//! script: the build script reruns only when a watched input changes, while `include_str!` tracks
//! the built file itself, so a hash carried through the build environment could describe a
//! different bundle than the one embedded -- a panel that never draws, with no error to say why.
//!
//! The extractor follows the HTML tokenizer only as far as the hash needs. Anything that would make
//! the browser read a script element differently from this simple scan (a comment opener in script
//! data, a carriage return the parser would normalise, a NUL, an end tag that is not really one) is
//! refused, and a refused document gets `script-src 'none'`: a panel that visibly fails to start is
//! the failure that can be seen and fixed; quietly allowing inline script again is not.

/// Every directive after `script-src`, unchanged: styles stay inline (the theme block and the
/// bundle's own `<style>`), images and fonts only as `data:`, and nothing that could reach the
/// network, frame, embed, submit or rebase.
const AFTER_SCRIPT_SRC: &str = "style-src 'unsafe-inline'; img-src data:; font-src data:; media-src 'none'; \
     connect-src 'none'; frame-src 'none'; object-src 'none'; form-action 'none'; base-uri 'none'";

/// The policy for a document: `default-src 'none'`, then one `'sha256-…'` source per inline
/// script in `html`, then [`AFTER_SCRIPT_SRC`]. A document the extractor refuses gets
/// `script-src 'none'` and one stderr line naming why.
pub(crate) fn content_security_policy_for(html: &str) -> String {
    let script_sources = match inline_script_hashes(html) {
        Ok(hashes) => hashes.join(" "),
        Err(reason) => {
            eprintln!("[agent_panel] the panel's script cannot be pinned by hash, so none will run: {reason}");
            "'none'".to_string()
        }
    };
    format!("default-src 'none'; script-src {script_sources}; {AFTER_SCRIPT_SRC}")
}

/// One CSP source expression, `'sha256-<base64>'` with its quotes, per inline `<script>` element
/// in `html`, in document order. The hash covers the element's text exactly as written: no trim,
/// no newline change.
pub(crate) fn inline_script_hashes(html: &str) -> Result<Vec<String>, String> {
    // Only ASCII letters change case, so every byte offset in `lower` is the same in `html`.
    let lower = html.to_ascii_lowercase();
    let mut hashes = Vec::new();
    let mut from = 0;
    while let Some(found) = lower[from..].find("<script") {
        let tag_start = from + found;
        let name_end = tag_start + "<script".len();
        if !lower[name_end..].starts_with(|c: char| is_tag_name_end(c)) {
            // `<scripts`, `<script-x` or the end of input: not a script start tag.
            from = name_end;
            continue;
        }
        let tag_close = name_end
            + lower[name_end..]
                .find('>')
                .ok_or_else(|| format!("a <script start tag at byte {tag_start} never ends"))?;
        let tag = &lower[tag_start..tag_close];
        if !tag.matches('"').count().is_multiple_of(2) || !tag.matches('\'').count().is_multiple_of(2) {
            return Err(format!(
                "the <script start tag at byte {tag_start} has an unbalanced quote, so its end may not be the first '>'"
            ));
        }
        if has_src_attribute(&tag["<script".len()..]) {
            return Err(format!(
                "the <script at byte {tag_start} loads an external file, which the panel must not do"
            ));
        }
        let body_start = tag_close + 1;
        let body_len = lower[body_start..]
            .find("</script")
            .ok_or_else(|| format!("the <script at byte {tag_start} is never closed"))?;
        let body_end = body_start + body_len;
        let after_end_name = body_end + "</script".len();
        if !lower[after_end_name..].starts_with(|c: char| is_tag_name_end(c)) {
            return Err(format!(
                "the <script at byte {tag_start} meets `</script` followed by another name character, \
                 which the parser does not treat as its end"
            ));
        }
        let body = &html[body_start..body_end];
        if body.contains("<!--") {
            return Err(format!(
                "the <script at byte {tag_start} contains `<!--`, which can move where the parser ends it"
            ));
        }
        if body.contains('\r') {
            return Err(format!(
                "the <script at byte {tag_start} contains a carriage return, which the parser rewrites before hashing"
            ));
        }
        if body.contains('\0') {
            return Err(format!(
                "the <script at byte {tag_start} contains a NUL, which the parser replaces before hashing"
            ));
        }
        hashes.push(format!("'sha256-{}'", sha256_base64(body.as_bytes())));
        from = after_end_name;
    }
    if hashes.is_empty() {
        return Err("the document has no inline <script>".to_string());
    }
    Ok(hashes)
}

/// What may follow a tag name for it to be that name and not a longer one.
fn is_tag_name_end(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\x0c' | '\r' | ' ' | '/' | '>')
}

/// Whether the (lowercased) attribute text of a start tag names a `src` attribute. A `src` inside
/// some attribute's value also counts: refusing too much only blanks the panel, loudly.
fn has_src_attribute(attributes: &str) -> bool {
    let bytes = attributes.as_bytes();
    attributes.match_indices("src").any(|(at, _)| {
        let before = at.checked_sub(1).map(|i| bytes[i]);
        let after = bytes.get(at + 3).copied();
        matches!(before, Some(b'\t' | b'\n' | b'\x0c' | b'\r' | b' ' | b'/'))
            && matches!(
                after,
                None | Some(b'\t' | b'\n' | b'\x0c' | b'\r' | b' ' | b'/' | b'=' | b'>')
            )
    })
}

fn sha256_base64(data: &[u8]) -> String {
    // SHA-256 is always compiled into GLib; neither call needs GTK to be initialised.
    let mut checksum =
        gtk4::glib::Checksum::new(gtk4::glib::ChecksumType::Sha256).expect("GLib always provides SHA-256");
    checksum.update(data);
    gtk4::glib::base64_encode(&checksum.digest()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest;

    fn independent_sha256_source(body: &str) -> String {
        format!(
            "'sha256-{}'",
            gtk4::glib::base64_encode(&sha2::Sha256::digest(body.as_bytes()))
        )
    }

    #[test]
    fn hashes_the_exact_body_of_an_inline_script() {
        assert_eq!(
            inline_script_hashes("<head><script type=\"module\" crossorigin>abc</script></head>"),
            Ok(vec!["'sha256-ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0='".to_string()])
        );
        assert_eq!(
            inline_script_hashes("<script></script>"),
            Ok(vec!["'sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU='".to_string()])
        );
        // Whitespace is part of what the browser hashes, so it must be part of ours.
        let body = "\n x \n";
        assert_eq!(
            inline_script_hashes(&format!("<script>{body}</script>")),
            Ok(vec![independent_sha256_source(body)])
        );
        assert_ne!(independent_sha256_source(body), independent_sha256_source(body.trim()));
    }

    #[test]
    fn finds_every_inline_script_and_not_tag_text_inside_one() {
        let first = "a(\"<script>x\")";
        let html = format!("<SCRIPT>{first}</Script><script>b</script><scripts>c</scripts>");
        assert_eq!(
            inline_script_hashes(&html),
            Ok(vec![independent_sha256_source(first), independent_sha256_source("b")])
        );
        // A self-closing-looking tag is still a start tag whose body runs to `</script`.
        assert_eq!(
            inline_script_hashes("<script/>d</script >"),
            Ok(vec![independent_sha256_source("d")])
        );
    }

    #[test]
    fn refuses_what_the_parser_would_read_differently() {
        for html in [
            "<script>a<!--b</script>",
            "<script>a\r\nb</script>",
            "<script>a\0b</script>",
            "<script src=\"x\"></script>",
            "<script type=module SRC=x></script>",
            "<script defer src></script>",
            "<script>never closed",
            "<script type=\"a>b\">c</script>",
            "<script>a</scriptx>b</script>",
            "<script",
            "<head></head>",
            "<scripts>x</scripts>",
        ] {
            assert!(inline_script_hashes(html).is_err(), "{html:?} was accepted");
        }
        // Words that merely contain the letters are not a `src` attribute.
        assert!(inline_script_hashes("<script data-srcset=\"1\" crossorigin>a</script>").is_ok());
    }

    #[test]
    fn a_refused_document_gets_script_src_none() {
        let policy = content_security_policy_for("<head></head>");
        assert!(
            policy.starts_with("default-src 'none'; script-src 'none'; style-src "),
            "{policy}"
        );
        let before_style = &policy[..policy.find("style-src").unwrap()];
        assert!(!before_style.contains("unsafe"), "{policy}");
    }

    #[test]
    fn a_pinned_document_lists_its_hashes_in_script_src() {
        assert_eq!(
            content_security_policy_for("<script>abc</script><script></script>"),
            "default-src 'none'; script-src 'sha256-ungWv48Bz+pBQUDeXa4iI7ADYaOWF3qctBD/YfIAFa0=' \
             'sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU='; style-src 'unsafe-inline'; img-src data:; \
             font-src data:; media-src 'none'; connect-src 'none'; frame-src 'none'; object-src 'none'; \
             form-action 'none'; base-uri 'none'"
        );
    }
}
