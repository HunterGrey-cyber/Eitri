//! Links and paths a page asks the panel to open, each checked before anything opens it: a file in the
//! project, a web address, a mail draft.

/// The file `gf` opens for a path the page sent: relative paths are under the project root. A path
/// holding a control character is refused outright: the path reaches the editor, where a newline
/// in a name has been able to end one command and start another, and no real file name the agent
/// shows needs one.
pub fn open_path_target(project_dir: &std::path::Path, path: &str) -> Result<std::path::PathBuf, String> {
    if path.chars().any(|c| c.is_ascii_control()) {
        return Err(format!("refused to open a path holding a control character: {path:?}"));
    }
    let candidate = std::path::Path::new(path);
    let resolved = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        project_dir.join(candidate)
    };
    if !resolved.exists() {
        return Err(format!("no such file: {path}"));
    }
    Ok(resolved)
}

/// The file a pending edit's proposal is about, for the path the page sent: relative paths are under the
/// project root, and a path holding a control character is refused as `gf` refuses it. Whether anything
/// is there is not asked here: a proposal may create the file, and what is there is read on a worker
/// (`eitri_core::scratch::proposal_texts`).
pub(super) fn proposal_target(project_dir: &std::path::Path, path: &str) -> Result<std::path::PathBuf, String> {
    if eitri_core::agent_bridge::path_holds_control(path) {
        return Err(format!(
            "refused to show a proposal for a path holding a control character: {path:?}"
        ));
    }
    let candidate = std::path::Path::new(path);
    Ok(if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        project_dir.join(candidate)
    })
}

/// `open_url`: what a panel key may hand `UriLauncher`. The page sends a WHATWG-normalized
/// `URL.href` (`nav.ts#webUrl`); no crate here parses the way a browser does (`url` is not a
/// dependency), so this is a strict re-check instead of a parse: ASCII only, no whitespace, control
/// character or backslash anywhere, an `http://` or `https://` scheme, and an authority that is a plain
/// host -- letters, digits, `.` and `-` only, no trailing dot, not the panel's own `eitri.invalid`
/// (`PANEL_BASE_URI`, where every relative link resolves) -- with an optional all-digit port. The scheme
/// and host are compared lower-cased, and the input comes back unchanged: this only ever says yes or no.
/// Anything a browser would re-normalize (`%`, `@`, `[`, non-ASCII, userinfo) is refused, not guessed at.
pub(super) fn web_url(url: &str) -> Option<&str> {
    if url
        .chars()
        .any(|c| !c.is_ascii() || c.is_ascii_whitespace() || c.is_ascii_control() || c == '\\')
    {
        return None;
    }
    let lower = url.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let (host, port) = authority.split_once(':').unwrap_or((authority, ""));
    let host_ok = !host.is_empty()
        && !host.ends_with('.')
        && host != "eitri.invalid"
        && host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    (host_ok && port.chars().all(|c| c.is_ascii_digit())).then_some(url)
}

/// What a click on a link in the panel may hand `UriLauncher`: a web address `web_url` accepts, or a
/// `mailto:` one. Every other scheme is refused, and so is a relative link (it resolves to the panel's
/// own base, which `web_url` refuses). The input comes back unchanged.
pub fn clicked_link_url(uri: &str) -> Option<&str> {
    web_url(uri).or_else(|| mail_url(uri))
}

/// A `mailto:` address with something after the scheme: ASCII only, no whitespace, control character or
/// backslash anywhere. `%`, `?`, `&` and `=` stay allowed, since a real mail link carries
/// `?subject=a%20b`; the mail client parses the rest. Its query may name only the headers a draft
/// needs ([`MAIL_HEADERS`]): some mail clients read `attach=<path>` as a local file to attach, which
/// would put a file of the user's into a draft addressed by a reply.
pub(super) fn mail_url(uri: &str) -> Option<&str> {
    if uri
        .chars()
        .any(|c| !c.is_ascii() || c.is_ascii_whitespace() || c.is_ascii_control() || c == '\\')
    {
        return None;
    }
    let scheme = uri.get(..7)?;
    if !(scheme.eq_ignore_ascii_case("mailto:") && uri.len() > 7) {
        return None;
    }
    let query = uri[7..].split_once('?').map_or("", |(_, query)| query);
    // `;` is split on too: a client that reads it as a separator must not find a key hidden in a value.
    let headers_known = query.split(['&', ';']).filter(|piece| !piece.is_empty()).all(|piece| {
        let key = piece.split_once('=').map_or(piece, |(key, _)| key);
        percent_decoded(key).is_some_and(|key| {
            MAIL_HEADERS
                .iter()
                .any(|known| key.eq_ignore_ascii_case(known.as_bytes()))
        })
    });
    headers_known.then_some(uri)
}

/// The `mailto:` query keys a clicked mail link may carry.
pub(super) const MAIL_HEADERS: [&str; 6] = ["to", "cc", "bcc", "subject", "body", "in-reply-to"];

/// `text` with every `%XX` replaced by its byte; `None` for a `%` not followed by two hex digits.
pub(super) fn percent_decoded(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text
                .get(i + 1..i + 3)
                .filter(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit()))?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;
    use crate::agent_panel::tests::*;

    /// A path `gf` sends the editor is a file name, and no file name the agent shows needs a control
    /// character; one holding a newline is refused before nvim sees it, whatever exists on disk.
    #[test]
    fn open_path_refuses_a_control_character_before_looking_at_the_disk() {
        let dir = std::env::temp_dir().join(format!("eitri-open-path-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("plain.rs"), "").unwrap();
        let hostile = "notes\nlua(io.open)(string.char(47),string.char(119))";
        std::fs::write(dir.join(hostile), "").unwrap();

        assert_eq!(open_path_target(&dir, "plain.rs"), Ok(dir.join("plain.rs")));
        let absolute = dir.join("plain.rs");
        assert_eq!(
            open_path_target(&dir, &absolute.to_string_lossy()),
            Ok(absolute.clone())
        );
        let refused = open_path_target(&dir, hostile).unwrap_err();
        assert!(refused.contains("control character"), "{refused}");
        for c in ['\0', '\r', '\t', '\x1b', '\x7f'] {
            let path = format!("a{c}b.rs");
            assert!(open_path_target(&dir, &path).is_err(), "{path:?}");
        }
        assert!(open_path_target(&dir, "missing.rs")
            .unwrap_err()
            .contains("no such file"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A proposal may create its file, so the resolver lets a missing path through (what is there is the
    /// worker's question); a path holding a control character is refused before anything looks at it.
    #[test]
    fn proposal_target_lets_a_missing_file_through_and_refuses_a_control_character() {
        let dir = std::path::Path::new("/nonexistent/eitri-project");
        assert_eq!(proposal_target(dir, "src/new.rs"), Ok(dir.join("src/new.rs")));
        assert_eq!(proposal_target(dir, "/abs/new.rs"), Ok(PathBuf::from("/abs/new.rs")));
        let refused = proposal_target(dir, "notes\nlua(io.open)(string.char(47),string.char(119))").unwrap_err();
        assert!(refused.contains("control character"), "{refused}");
        for c in ['\0', '\r', '\t', '\x1b', '\x7f'] {
            let path = format!("a{c}b.rs");
            assert!(proposal_target(dir, &path).is_err(), "{path:?}");
        }
    }

    #[test]
    fn a_clicked_web_link_goes_to_the_host_and_anything_else_is_refused_on_the_page() {
        let (handle, page, host) = handle_with_recording_page();
        handle.link_clicked(ViewId::BASE, Some("https://example.com/a"));
        handle.link_clicked(ViewId::BASE, Some("tel:123"));
        handle.link_clicked(ViewId::BASE, None);
        assert_eq!(*host.opened.borrow(), vec!["https://example.com/a".to_owned()]);
        let kinds: Vec<String> = page.sent.borrow().iter().map(|e| kind_of(e)).collect();
        assert_eq!(kinds, vec!["notice", "notice"]);
    }

    #[test]
    fn open_url_goes_to_the_host_only_for_a_web_address() {
        let (handle, page, host) = handle_with_recording_page();
        handle.page_message(
            ViewId::BASE,
            r#"{"type":"open_url","request_id":"a","url":"https://example.com/"}"#,
        );
        handle.page_message(
            ViewId::BASE,
            r#"{"type":"open_url","request_id":"b","url":"file:///etc/passwd"}"#,
        );
        assert_eq!(*host.opened.borrow(), vec!["https://example.com/".to_owned()]);
        assert_eq!(
            page.sent
                .borrow()
                .iter()
                .filter(|e| kind_of(e) == "command_result")
                .count(),
            2
        );
    }

    /// The addresses `gx` may hand `UriLauncher`. The page sends a normalized
    /// `URL.href`, so these are what a browser writes: a query, a fragment, a port, a numeric host, a
    /// punycode one, an `@` after the host's slash. An upper-case spelling (which only a page that did not
    /// normalize would send) is judged as its lower-case self and returned unchanged, never rewritten.
    #[test]
    fn web_url_accepts_plain_http_and_https_links() {
        for url in [
            "https://example.com/a?b#c",
            "http://x.y:8080/p",
            "https://xn--r8jz45g.jp/",
            "https://example.com/@user",
            "http://127.0.0.1:3000/",
            "HTTPS://EXAMPLE.COM/A",
        ] {
            assert_eq!(web_url(url), Some(url), "{url}");
        }
    }

    /// Every way a link can be something other than a web page a reader was shown: another scheme, a
    /// relative link, the panel's own address (spelled any way a browser resolves it), a backslash or
    /// userinfo that moves the real host, whitespace, a bracketed or non-ASCII host, and hosts a browser
    /// would re-normalize. Backslash cases are raw strings so the backslash reaches the function.
    #[test]
    fn web_url_refuses_every_non_web_or_disguised_link() {
        for url in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "mailto:a@b",
            "docs/a.md",
            "https://eitri.invalid/x",
            "https://EITRI.invalid/x",
            "https://eitri.invalid./x",
            "https://%65itri.invalid/x",
            r"https:\\eitri.invalid\x",
            r"https://example.com\@evil/",
            r"https://example.com/?a\b",
            "https://a b",
            "https://user@evil",
            "https://user:pw@example.com/",
            "https://例え.jp/",
            "https://[::1]/",
            "https://example.com./",
            "https://a$b.com/",
            "https://my_host.x/",
            "https://",
            "http://x.y:8080:9/p",
            "https://example.com:80@evil.example/",
            "https://example.com/a\nb",
            // The same rules past the host, where the host check cannot help: a space, a control character
            // and a non-ASCII character in the path or query (a browser percent-encodes all three).
            "https://example.com/a b",
            "https://example.com/a\u{7f}b",
            "https://example.com/?q=例え",
            "",
        ] {
            assert_eq!(web_url(url), None, "{url:?}");
        }
    }

    /// The panel document's own base (`PANEL_BASE_URI`) is where every relative link in a reply resolves;
    /// opening it would launch a browser at an address that never resolves. Tied to the constant, so a
    /// change of the base fails here until `web_url` follows it.
    #[test]
    fn web_url_refuses_the_panels_own_base() {
        assert_eq!(web_url(PANEL_BASE_URI), None);
        assert_eq!(web_url(&format!("{PANEL_BASE_URI}docs/a.md")), None);
    }

    /// A click (or Enter on a focused link) launches a web page or a mail draft and nothing else: any
    /// other scheme hands its data to another program on one click, and a relative link resolves to the
    /// panel's own address, which goes nowhere.
    #[test]
    fn a_clicked_link_launches_only_web_and_mail_addresses() {
        for uri in [
            "https://example.com/a",
            "http://x.y:8080/p",
            "mailto:a@b.example",
            "MAILTO:a@b",
            "mailto:a@b?subject=a%20b",
        ] {
            assert_eq!(clicked_link_url(uri), Some(uri), "{uri}");
        }
        for uri in [
            "ftp://x.example/",
            "sms:123",
            "tel:123",
            "callto:x",
            "xmpp:a@b",
            "matrix:r/x:y",
            "cid:x",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://eitri.invalid/docs/a.md",
            "mailto:",
            "mailto:a b",
            "mailto:a@b\n",
            r"mailto:a\b",
            "mailto:例@x",
            "",
        ] {
            assert_eq!(clicked_link_url(uri), None, "{uri:?}");
        }
    }

    /// A mail link may carry only the headers a draft needs. Any other key -- `attach` above all,
    /// which some mail clients honour by attaching a local file to the draft -- refuses the whole
    /// link, however its name is spelled or encoded.
    #[test]
    fn a_mail_link_carries_only_the_headers_a_draft_needs() {
        for uri in [
            "mailto:a@b?subject=hi",
            "mailto:a@b?to=c@d&cc=e@f&bcc=g@h",
            "mailto:a@b?SUBJECT=x&Body=y%0Az",
            "mailto:a@b?in-reply-to=%3Cid@x%3E",
            "mailto:a@b?%73ubject=x",
            "mailto:?to=a@b",
            "mailto:a@b?",
            "mailto:a@b?subject",
            "mailto:a@b?subject=x&",
        ] {
            assert_eq!(mail_url(uri), Some(uri), "{uri}");
        }
        for uri in [
            "mailto:a@b?attach=/etc/passwd",
            "mailto:a@b?subject=x&attach=/etc/passwd",
            "mailto:a@b?Attachment=/etc/passwd",
            "mailto:a@b?Attach%6Dent=/etc/passwd",
            "mailto:a@b?%61ttach=/etc/passwd",
            "mailto:a@b?subject=x;attach=/etc/passwd",
            "mailto:a@b?x-attach=y",
            "mailto:a@b?from=x",
            "mailto:a@b?=x",
            "mailto:a@b?%zz=x",
            "mailto:a@b?%+5ubject=x",
            "mailto:a@b?sub%00ject=x",
        ] {
            assert_eq!(mail_url(uri), None, "{uri:?}");
        }
    }

    /// The install address the panel and the first-start screen show passes the same two checks any link does, so
    /// the click and the keyboard route both reach `UriLauncher` with it.
    #[test]
    fn the_claude_install_address_passes_the_web_link_checks() {
        let url = agent::CLAUDE_INSTALL_URL;
        assert_eq!(web_url(url), Some(url));
        assert_eq!(clicked_link_url(url), Some(url));
    }
}
