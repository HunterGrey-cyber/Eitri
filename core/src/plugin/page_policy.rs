//! A plugin page's rules that need no toolkit: its scheme and origin, the content security policy floor
//! the page cannot loosen, where a navigation may go, which hosts pass the proxy backstop, and which file
//! under the site a request names. `shell` applies them to a `WebView`.

use std::path::{Path, PathBuf};

use super::manifest::Origin;
use super::roots::has_control;

pub const SCHEME: &str = "eitri-plugin";
/// The discard port on loopback: a request that gets past the policy dies here, unless its host is one the
/// manifest declared.
pub const PROXY_BACKSTOP: &str = "http://127.0.0.1:9";
/// What the entry shows while the site has not been built. No script.
pub const NOT_BUILT_HTML: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>Not built</title></head>\
<body><p>This plugin's page has not been built yet.</p></body></html>\n";

const MIME: &[(&str, &str)] = &[
    ("html", "text/html"),
    ("htm", "text/html"),
    ("css", "text/css"),
    ("js", "text/javascript"),
    ("mjs", "text/javascript"),
    ("json", "application/json"),
    ("map", "application/json"),
    ("webmanifest", "application/manifest+json"),
    ("svg", "image/svg+xml"),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("ico", "image/x-icon"),
    ("woff", "font/woff"),
    ("woff2", "font/woff2"),
    ("ttf", "font/ttf"),
    ("otf", "font/otf"),
    ("txt", "text/plain"),
];

pub fn origin(id: &str) -> String {
    format!("{SCHEME}://{id}")
}

/// The address a page the host writes itself (the crash notice) is loaded at: inside the plugin's origin, so
/// the navigation lock lets it in, where `about:blank` would be refused.
pub fn host_page_base(id: &str) -> String {
    format!("{}/", origin(id))
}

/// The entry's address, each byte outside the unreserved set percent-encoded so a name holding `#`, `?` or
/// `%` reaches `site_file` as the file it names and not as a fragment, a query or an escape.
pub fn entry_uri(id: &str, entry: &str) -> String {
    let mut path = String::with_capacity(entry.len());
    for b in entry.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            path.push(char::from(b));
        } else {
            path.push_str(&format!("%{b:02X}"));
        }
    }
    format!("{SCHEME}://{id}/{path}")
}

pub fn csp_floor(origins: &[Origin]) -> String {
    let mut connect = String::from("'self'");
    for o in origins {
        connect.push(' ');
        connect.push_str(&o.as_str());
    }
    format!(
        "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
         font-src 'self'; connect-src {connect}; frame-src 'none'; base-uri 'none'; form-action 'none'"
    )
}

pub fn proxy_ignore_hosts(origins: &[Origin]) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    for o in origins {
        if !hosts.iter().any(|h| h == o.host()) {
            hosts.push(o.host().to_owned());
        }
    }
    hosts
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Navigation {
    /// Inside the plugin's own origin.
    Stay,
    /// A web or mail address: may be handed to the desktop on a click, after the window's own link check.
    Outside(String),
    Refuse,
}

pub fn navigation(id: &str, uri: &str) -> Navigation {
    let home = origin(id);
    if uri == home || uri.strip_prefix(&home).is_some_and(|rest| rest.starts_with('/')) {
        return Navigation::Stay;
    }
    // A `blob:` the page made itself is how it saves a file (an export): WebKit turns it into a download.
    if uri
        .strip_prefix("blob:")
        .and_then(|rest| rest.strip_prefix(&home))
        .is_some_and(|rest| rest.starts_with('/'))
    {
        return Navigation::Stay;
    }
    let lower = uri.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:") {
        return Navigation::Outside(uri.to_owned());
    }
    Navigation::Refuse
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteFile {
    pub path: PathBuf,
    pub mime: &'static str,
    pub len: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiteRefusal {
    WrongPlugin,
    BadPath,
    NotFound,
    OutsideSite,
    UnknownType,
}

impl SiteRefusal {
    pub fn message(&self) -> &'static str {
        match self {
            SiteRefusal::WrongPlugin => "not this plugin's origin",
            SiteRefusal::BadPath => "not a path this page may ask for",
            SiteRefusal::NotFound => "no such file",
            SiteRefusal::OutsideSite => "outside the plugin's site",
            SiteRefusal::UnknownType => "not a type the plugin page serves",
        }
    }
}

/// A page's suggested file name made safe for the download directory: its last component, `[A-Za-z0-9._-]`
/// kept and anything else `_`, no leading dot, at most 100 bytes.
pub fn download_name(suggested: &str) -> String {
    let last = suggested.rsplit(['/', '\\']).next().unwrap_or("");
    let mut name: String = last
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    while name.starts_with('.') {
        name.remove(0);
    }
    name.truncate(100);
    if name.is_empty() || name.chars().all(|c| c == '_') {
        "download".to_owned()
    } else {
        name
    }
}

/// Where a download goes: `dir/<name>`, or `<stem>-<n>.<ext>` with the first `n` that names no file yet.
pub fn download_destination(dir: &Path, suggested: &str) -> PathBuf {
    let name = download_name(suggested);
    let first = dir.join(&name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_owned(), format!(".{ext}")),
        _ => (name.clone(), String::new()),
    };
    (1..)
        .map(|n| dir.join(format!("{stem}-{n}{ext}")))
        .find(|p| !p.exists())
        .unwrap_or(first)
}

pub fn mime_for(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    MIME.iter().find(|(e, _)| *e == ext).map(|(_, m)| *m)
}

/// `%XX` decoded, strictly: a `%` without two hex digits, bytes that are not UTF-8, or a control character
/// refuse the path.
fn decoded(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = text
                .get(i + 1..i + 3)
                .filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let s = String::from_utf8(out).ok()?;
    (!has_control(&s)).then_some(s)
}

/// The path part of a request for this plugin's origin, with the query and fragment cut; `None` for another
/// origin.
fn request_path<'a>(id: &str, uri: &'a str) -> Option<&'a str> {
    let rest = uri.strip_prefix(&origin(id))?;
    let rest = if rest.is_empty() { "/" } else { rest };
    let rest = rest.strip_prefix('/')?;
    Some(rest.split(['?', '#']).next().unwrap_or(""))
}

/// Where every plugin's origin serves the keyboard kit, a path no site file is read for.
pub const KIT_PATH: &str = ".eitri/keys.js";

/// Whether `uri` asks this plugin's origin for the keyboard kit: its decoded path is [`KIT_PATH`] exactly, the
/// query and fragment ignored.
pub fn kit_request(id: &str, uri: &str) -> bool {
    request_path(id, uri).is_some_and(|p| decoded(p).is_some_and(|d| d == KIT_PATH))
}

pub fn is_entry(id: &str, entry: &str, uri: &str) -> bool {
    request_path(id, uri).is_some_and(|p| p.is_empty() || decoded(p).is_some_and(|d| d == entry))
}

pub fn site_file(site_root: &Path, id: &str, entry: &str, uri: &str) -> Result<SiteFile, SiteRefusal> {
    let raw = request_path(id, uri).ok_or(SiteRefusal::WrongPlugin)?;
    let mut path = if raw.is_empty() {
        entry.to_owned()
    } else {
        decoded(raw).ok_or(SiteRefusal::BadPath)?
    };
    if path.ends_with('/') {
        path.push_str("index.html");
    }
    if path.starts_with('/') || path.contains('\\') || path.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
        return Err(SiteRefusal::BadPath);
    }
    let mime = mime_for(&path).ok_or(SiteRefusal::UnknownType)?;
    let canonical = std::fs::canonicalize(site_root.join(&path)).map_err(|_| SiteRefusal::NotFound)?;
    if !canonical.starts_with(site_root) {
        return Err(SiteRefusal::OutsideSite);
    }
    let meta = std::fs::metadata(&canonical).map_err(|_| SiteRefusal::NotFound)?;
    if !meta.is_file() {
        return Err(SiteRefusal::NotFound);
    }
    Ok(SiteFile {
        path: canonical,
        mime,
        len: meta.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::test_support::Scratch;

    #[test]
    fn the_csp_floor_names_only_declared_origins() {
        let origins = [Origin::parse("https://learn.example.org").unwrap()];
        assert_eq!(
            csp_floor(&origins),
            "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
             font-src 'self'; connect-src 'self' https://learn.example.org; frame-src 'none'; base-uri 'none'; \
             form-action 'none'"
        );
        let none = csp_floor(&[]);
        assert!(none.contains("connect-src 'self';"));
        let script = none
            .split(';')
            .find(|d| d.trim_start().starts_with("script-src"))
            .unwrap();
        assert_eq!(script.trim(), "script-src 'self'", "no inline or eval script, ever");
        assert_eq!(proxy_ignore_hosts(&origins), ["learn.example.org"]);
    }

    #[test]
    fn navigation_stays_home_opens_web_and_mail_outside_and_refuses_the_rest() {
        assert_eq!(
            navigation("demo", "eitri-plugin://demo/rust/ch04-02.html#x"),
            Navigation::Stay
        );
        assert_eq!(navigation("demo", "eitri-plugin://demo"), Navigation::Stay);
        assert_eq!(
            navigation("demo", "eitri-plugin://other/index.html"),
            Navigation::Refuse
        );
        assert_eq!(navigation("demo", "eitri-plugin://demo.evil/x"), Navigation::Refuse);
        assert_eq!(
            navigation("demo", "https://doc.rust-lang.org/x"),
            Navigation::Outside("https://doc.rust-lang.org/x".into())
        );
        assert_eq!(
            navigation("demo", "HTTP://a.org/"),
            Navigation::Outside("HTTP://a.org/".into())
        );
        assert_eq!(
            navigation("demo", "mailto:a@example.org"),
            Navigation::Outside("mailto:a@example.org".into())
        );
        assert_eq!(
            navigation("demo", "blob:eitri-plugin://demo/0c7e-uuid"),
            Navigation::Stay,
            "the page's own download"
        );
        for refused in [
            "file:///etc/passwd",
            "about:blank",
            "data:text/html,x",
            "javascript:alert(1)",
            "",
            "blob:eitri-plugin://other/0c7e",
            "blob:https://a.org/0c7e",
        ] {
            assert_eq!(navigation("demo", refused), Navigation::Refuse, "{refused}");
        }
        assert_eq!(navigation("demo", &host_page_base("demo")), Navigation::Stay);
    }

    #[test]
    fn a_download_gets_a_safe_name_and_never_replaces_a_file() {
        assert_eq!(download_name("progress.jsonl"), "progress.jsonl");
        assert_eq!(download_name("../../.bashrc"), "bashrc");
        assert_eq!(download_name("dir/a b\u{0}c.jsonl"), "a_b_c.jsonl");
        assert_eq!(download_name(""), "download");
        assert!(download_name(&"x".repeat(500)).len() <= 100);
        let s = Scratch::new("dl");
        assert_eq!(download_destination(&s.0, "log.jsonl"), s.0.join("log.jsonl"));
        std::fs::write(s.0.join("log.jsonl"), "").unwrap();
        std::fs::write(s.0.join("log-1.jsonl"), "").unwrap();
        assert_eq!(download_destination(&s.0, "log.jsonl"), s.0.join("log-2.jsonl"));
    }

    fn site(s: &Scratch) -> std::path::PathBuf {
        let site = s.0.join("site");
        std::fs::create_dir_all(site.join("rust")).unwrap();
        std::fs::create_dir_all(site.join("empty")).unwrap();
        std::fs::write(site.join("index.html"), "<p>i</p>").unwrap();
        std::fs::write(site.join("rust/index.html"), "<p>r</p>").unwrap();
        std::fs::write(site.join("app.js"), "1").unwrap();
        std::fs::write(site.join("tool.exe"), "x").unwrap();
        std::fs::write(s.0.join("secret.txt"), "s").unwrap();
        std::os::unix::fs::symlink(s.0.join("secret.txt"), site.join("link.txt")).unwrap();
        std::fs::canonicalize(&site).unwrap()
    }

    #[test]
    fn the_scheme_serves_files_under_the_site_and_refuses_the_rest() {
        let s = Scratch::new("site");
        let root = site(&s);
        let get = |uri: &str| site_file(&root, "demo", "index.html", uri);
        let index = get("eitri-plugin://demo/").unwrap();
        assert_eq!(
            (index.path.clone(), index.mime, index.len),
            (root.join("index.html"), "text/html", 8)
        );
        assert_eq!(get("eitri-plugin://demo/app.js?v=3#x").unwrap().mime, "text/javascript");
        assert_eq!(
            get("eitri-plugin://demo/rust/").unwrap().path,
            root.join("rust/index.html")
        );
        assert_eq!(
            get("eitri-plugin://demo/a%2f..%2f..%2fsecret.txt"),
            Err(SiteRefusal::BadPath)
        );
        assert_eq!(get("eitri-plugin://demo/%00x.html"), Err(SiteRefusal::BadPath));
        assert_eq!(get("eitri-plugin://demo/bad%zz.html"), Err(SiteRefusal::BadPath));
        assert_eq!(get("eitri-plugin://demo/link.txt"), Err(SiteRefusal::OutsideSite));
        assert_eq!(
            get("eitri-plugin://demo/empty/"),
            Err(SiteRefusal::NotFound),
            "no directory listing"
        );
        assert_eq!(get("eitri-plugin://demo/nope.html"), Err(SiteRefusal::NotFound));
        assert_eq!(get("eitri-plugin://demo/tool.exe"), Err(SiteRefusal::UnknownType));
        assert_eq!(get("eitri-plugin://other/index.html"), Err(SiteRefusal::WrongPlugin));
        assert!(
            is_entry("demo", "index.html", "eitri-plugin://demo/")
                && is_entry("demo", "index.html", "eitri-plugin://demo/index.html")
        );
        assert!(!is_entry("demo", "index.html", "eitri-plugin://demo/app.js"));
        assert!(!NOT_BUILT_HTML.contains("<script"));
    }

    #[test]
    fn the_keyboard_kit_is_asked_for_by_one_path_and_nothing_else() {
        assert!(kit_request("snotra", "eitri-plugin://snotra/.eitri/keys.js"));
        assert!(kit_request("snotra", "eitri-plugin://snotra/.eitri/keys.js?v=1"));
        assert!(kit_request("snotra", "eitri-plugin://snotra/.eitri/keys.js#x"));
        assert!(kit_request("snotra", "eitri-plugin://snotra/%2Eeitri/keys%2Ejs"));
        assert!(!kit_request("snotra", "eitri-plugin://snotra/.eitri/other.js"));
        assert!(!kit_request("snotra", "eitri-plugin://snotra/x/.eitri/keys.js"));
        assert!(!kit_request("snotra", "eitri-plugin://snotra/.eitri/keys.js/"));
        assert!(!kit_request("snotra", "eitri-plugin://snotra/.eitri/keys.json"));
        assert!(!kit_request("snotra", "eitri-plugin://snotra/"));
        assert!(!kit_request("snotra", "eitri-plugin://other/.eitri/keys.js"));
        assert!(!kit_request("snotra", "https://snotra/.eitri/keys.js"));
    }

    #[test]
    fn an_entry_with_reserved_characters_round_trips_through_its_uri() {
        let s = Scratch::new("entry-uri");
        let root = site(&s);
        for name in [
            "chapter#1.html",
            "a?b.html",
            "100%.html",
            "50%25.html",
            "sp ace.html",
            "d\u{e9}j\u{e0}.html",
        ] {
            std::fs::write(root.join(name), "x").unwrap();
            let uri = entry_uri("demo", name);
            assert!(is_entry("demo", name, &uri), "{name}: {uri}");
            let got = site_file(&root, "demo", name, &uri).unwrap();
            assert_eq!(got.path, root.join(name), "{name}: {uri}");
        }
        assert_eq!(
            entry_uri("demo", "sub dir/chapter#1.html"),
            "eitri-plugin://demo/sub%20dir/chapter%231.html"
        );
    }
}
