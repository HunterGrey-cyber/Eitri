//! A plugin page's rules that need no toolkit: its scheme and origin, the content security policy floor
//! the page cannot loosen, where a navigation may go, how its session is proxied (which hosts and schemes pass
//! the backstop), and which file under the site a request names. `shell` applies them to a `WebView`.

use std::path::{Path, PathBuf};

use super::manifest::Origin;
use super::roots::has_control;

pub const SCHEME: &str = "eitri-plugin";
/// The discard port on loopback: a request that gets past the policy dies here, unless it is for a declared
/// origin's host and port (see [`proxy_ignore_hosts`] for how far that reaches).
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
    ("avif", "image/avif"),
    ("bmp", "image/bmp"),
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
    format!("{SCHEME}://{id}/{}", encode_path(entry))
}

/// A relative path as it goes into one of this scheme's addresses: every byte outside `A-Za-z0-9-._~/`
/// percent-encoded, so the strict decoder gives back exactly the path.
pub(crate) fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The page's default policy. `remote_images` adds `https:` to `img-src` and nothing else: a page that shows
/// a document's remote pictures still runs only its own scripts and reaches only its declared origins.
pub fn csp_floor(origins: &[Origin], remote_images: bool) -> String {
    let mut connect = String::from("'self'");
    for o in origins {
        connect.push(' ');
        connect.push_str(&o.as_str());
    }
    let img = if remote_images {
        "'self' data: https:"
    } else {
        "'self' data:"
    };
    format!(
        "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src {img}; \
         font-src 'self'; connect-src {connect}; frame-src 'none'; base-uri 'none'; form-action 'none'"
    )
}

/// The proxy's ignore list: each declared origin's `host:port`, once. GLib matches such an entry for every
/// subdomain of the host too, and for either scheme on that port; ignore-hosts has no narrower form, so that is
/// what the backstop lets through.
pub fn proxy_ignore_hosts(origins: &[Origin]) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    for o in origins {
        // GLib reads an IPv6 literal's port only when the address is in brackets.
        let entry = if o.host().contains(':') {
            format!("[{}]:{}", o.host(), o.port())
        } else {
            format!("{}:{}", o.host(), o.port())
        };
        if !hosts.contains(&entry) {
            hosts.push(entry);
        }
    }
    hosts
}

/// How a page's ephemeral session is proxied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyPlan {
    /// Every request goes to [`PROXY_BACKSTOP`] unless a rule below says otherwise; without it the CSP alone guards.
    pub backstop: bool,
    /// The `host:port` entries that skip the backstop: the declared origins' ([`proxy_ignore_hosts`]).
    pub ignore_hosts: Vec<String>,
    /// `https` requests go out directly, while `http` and every other scheme stay on the backstop; the CSP decides
    /// which https requests the page may make.
    pub https_direct: bool,
}

impl ProxyPlan {
    /// One line for the log, naming what gets past the backstop.
    pub fn summary(&self) -> String {
        if !self.backstop {
            return "no backstop: the CSP alone".to_owned();
        }
        let hosts = if self.ignore_hosts.is_empty() {
            "no host".to_owned()
        } else {
            self.ignore_hosts.join(" ")
        };
        let mut line = format!("backstop {PROXY_BACKSTOP} except {hosts}");
        if self.https_direct {
            line.push_str("; https direct");
        }
        line
    }
}

/// The backstop always stays. With remote images, `https` gets a direct route of its own: WebKit's resolver
/// keeps a per-scheme entry apart from the default proxy, so `http` and every other scheme still die at the
/// closed port, and only the CSP's `img-src https:` lets a page use the direct route.
pub fn proxy_plan(origins: &[Origin], remote_images: bool) -> ProxyPlan {
    ProxyPlan {
        backstop: true,
        ignore_hosts: proxy_ignore_hosts(origins),
        https_direct: remote_images,
    }
}

/// Whether an outside address a navigation names is handed to the desktop: only for a link the user clicked.
/// `user_gesture` is the user's own press, not a script acting in a gesture's name. A script's `click()` on a
/// link is a link click too, and passing it would let a page send data out in a URL, or open tabs and mail
/// drafts in a loop.
pub fn opens_outside(link_clicked: bool, user_gesture: bool) -> bool {
    link_clicked && user_gesture
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
        // The host answers `/.eitri/` itself (the keyboard kit, a followed document's images): a script or an
        // image there is never a page. An SVG a document links to would otherwise run as the page, with the
        // bridge.
        if decoded_path(id, uri).is_some_and(|p| p.starts_with(".eitri/")) {
            return Navigation::Refuse;
        }
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

/// How many numbered names a download tries after its own before it is cancelled: each try is a file system
/// call, and a directory full of earlier exports must not make one save cost thousands of them.
pub const MAX_NAME_PROBES: u32 = 1000;

/// Where a download goes: `dir/<name>`, or `<stem>-<n>.<ext>` with the first `n` up to [`MAX_NAME_PROBES`] that
/// names no file yet; `None` when all of them are taken. It touches the disk, so it is called off the GTK thread.
pub fn download_destination(dir: &Path, suggested: &str) -> Option<PathBuf> {
    destination_with(dir, suggested, Path::exists)
}

/// [`download_destination`] with the existence check given, so the probing order and its cap are testable
/// without a disk.
pub fn destination_with(dir: &Path, suggested: &str, exists: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let name = download_name(suggested);
    let first = dir.join(&name);
    if !exists(&first) {
        return Some(first);
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_owned(), format!(".{ext}")),
        _ => (name.clone(), String::new()),
    };
    (1..=MAX_NAME_PROBES)
        .map(|n| dir.join(format!("{stem}-{n}{ext}")))
        .find(|p| !exists(p))
}

pub fn mime_for(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    MIME.iter().find(|(e, _)| *e == ext).map(|(_, m)| *m)
}

/// `%XX` decoded, strictly: a `%` without two hex digits, bytes that are not UTF-8, or a control character
/// refuse the path.
pub(crate) fn decoded(text: &str) -> Option<String> {
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
pub(crate) fn request_path<'a>(id: &str, uri: &'a str) -> Option<&'a str> {
    let rest = uri.strip_prefix(&origin(id))?;
    let rest = if rest.is_empty() { "/" } else { rest };
    let rest = rest.strip_prefix('/')?;
    Some(rest.split(['?', '#']).next().unwrap_or(""))
}

/// The decoded path a request for this plugin's origin names (query and fragment cut), for the trace; `None` for
/// another origin or a path that does not decode.
pub fn decoded_path(id: &str, uri: &str) -> Option<String> {
    decoded(request_path(id, uri)?)
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
            csp_floor(&origins, false),
            "default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
             font-src 'self'; connect-src 'self' https://learn.example.org; frame-src 'none'; base-uri 'none'; \
             form-action 'none'"
        );
        let none = csp_floor(&[], false);
        assert!(none.contains("connect-src 'self';"));
        let script = none
            .split(';')
            .find(|d| d.trim_start().starts_with("script-src"))
            .unwrap();
        assert_eq!(script.trim(), "script-src 'self'", "no inline or eval script, ever");
        assert_eq!(proxy_ignore_hosts(&origins), ["learn.example.org:443"]);
    }

    #[test]
    fn the_proxy_lets_through_each_declared_host_on_its_port_once() {
        let origins = [
            Origin::parse("https://learn.example.org").unwrap(),
            Origin::parse("https://api.example.org:8443").unwrap(),
            Origin::parse("https://learn.example.org:443").unwrap(),
            Origin::parse("https://learn.example.org:444").unwrap(),
        ];
        assert_eq!(
            proxy_ignore_hosts(&origins),
            ["learn.example.org:443", "api.example.org:8443", "learn.example.org:444"]
        );
        assert!(proxy_ignore_hosts(&[]).is_empty());
    }

    #[test]
    fn only_a_users_click_opens_outside() {
        assert!(opens_outside(true, true));
        assert!(!opens_outside(true, false), "a script's click() on a link");
        assert!(!opens_outside(false, true), "a gesture that is not a link click");
        assert!(!opens_outside(false, false));
    }

    #[test]
    fn remote_images_add_https_to_img_src_and_nothing_else() {
        let origins = [Origin::parse("https://learn.example.org").unwrap()];
        let off = csp_floor(&origins, false);
        let on = csp_floor(&origins, true);
        assert_eq!(on, off.replace("img-src 'self' data:;", "img-src 'self' data: https:;"));
        for directive in on.split(';').map(str::trim) {
            if !directive.starts_with("img-src") {
                assert!(
                    !directive.contains("https:") || directive.starts_with("connect-src"),
                    "{directive}"
                );
            }
        }
        assert!(
            on.contains("connect-src 'self' https://learn.example.org;"),
            "connect-src unchanged"
        );
        assert!(on.contains("script-src 'self';") && on.contains("frame-src 'none';"));
    }

    #[test]
    fn the_proxy_plan_for_each_images_value() {
        let origins = [Origin::parse("https://learn.example.org").unwrap()];
        let off = proxy_plan(&origins, false);
        assert_eq!(
            off,
            ProxyPlan {
                backstop: true,
                ignore_hosts: vec!["learn.example.org:443".to_owned()],
                https_direct: false,
            }
        );
        assert_eq!(
            off.summary(),
            "backstop http://127.0.0.1:9 except learn.example.org:443"
        );
        let bare = proxy_plan(&[], false);
        assert_eq!(
            bare,
            ProxyPlan {
                backstop: true,
                ignore_hosts: vec![],
                https_direct: false,
            }
        );
        assert_eq!(bare.summary(), "backstop http://127.0.0.1:9 except no host");
        let on = proxy_plan(&origins, true);
        assert_eq!(
            on,
            ProxyPlan {
                backstop: true,
                ignore_hosts: vec!["learn.example.org:443".to_owned()],
                https_direct: true,
            }
        );
        assert_eq!(
            on.summary(),
            "backstop http://127.0.0.1:9 except learn.example.org:443; https direct"
        );
        let open = ProxyPlan {
            backstop: false,
            ignore_hosts: vec![],
            https_direct: false,
        };
        assert_eq!(open.summary(), "no backstop: the CSP alone");
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
        for refused in [
            "eitri-plugin://demo/.eitri/doc/1/x.svg",
            "eitri-plugin://demo/%2Eeitri/doc/1/x.svg",
            "eitri-plugin://demo/.eitri/keys.js",
        ] {
            assert_eq!(navigation("demo", refused), Navigation::Refuse, "{refused}");
        }
        assert_eq!(navigation("demo", "eitri-plugin://demo/eitri/x.html"), Navigation::Stay);
        assert_eq!(mime_for("x.avif"), Some("image/avif"));
        assert_eq!(mime_for("x.BMP"), Some("image/bmp"));
    }

    #[test]
    fn a_download_gets_a_safe_name_and_never_replaces_a_file() {
        assert_eq!(download_name("progress.jsonl"), "progress.jsonl");
        assert_eq!(download_name("../../.bashrc"), "bashrc");
        assert_eq!(download_name("dir/a b\u{0}c.jsonl"), "a_b_c.jsonl");
        assert_eq!(download_name(""), "download");
        assert!(download_name(&"x".repeat(500)).len() <= 100);
        let s = Scratch::new("dl");
        assert_eq!(download_destination(&s.0, "log.jsonl"), Some(s.0.join("log.jsonl")));
        std::fs::write(s.0.join("log.jsonl"), "").unwrap();
        std::fs::write(s.0.join("log-1.jsonl"), "").unwrap();
        assert_eq!(download_destination(&s.0, "log.jsonl"), Some(s.0.join("log-2.jsonl")));
    }

    #[test]
    fn a_destination_probe_gives_up_after_its_cap() {
        let dir = Path::new("/downloads");
        assert_eq!(destination_with(dir, "x.txt", |_| true), None);
        assert_eq!(
            destination_with(dir, "x.txt", |p| p.ends_with("x.txt") || p.ends_with("x-1.txt")),
            Some(dir.join("x-2.txt"))
        );
        let last = format!("x-{MAX_NAME_PROBES}.txt");
        assert_eq!(
            destination_with(dir, "x.txt", |p| !p.ends_with(&last)),
            Some(dir.join(&last)),
            "the last numbered name is still tried"
        );
        let probed = std::cell::Cell::new(0u32);
        destination_with(dir, "x.txt", |_| {
            probed.set(probed.get() + 1);
            true
        });
        assert_eq!(
            probed.get(),
            MAX_NAME_PROBES + 1,
            "its own name, then each numbered one once"
        );
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
