//! Exercise a plugin page in WebKitGTK on an isolated Xvfb display. The scheme serves only
//! paths inside the site and sends `nosniff` so image files cannot run as scripts. CSP blocks
//! inline scripts, navigation stays locked, scripted clicks on external links open nothing, and
//! real clicks are handed out. An uncommitted same-origin load preserves the running document's
//! hello. A real `PluginHost` declaring only `state` rejects undeclared verbs.
//!
//! Theme checks cover the first frame, reload, a stylesheet swap on a loaded document, delivery
//! of the `theme` event, and back navigation reading the current theme without another hello.
//!
//! Network checks use HTTP and HTTPS listeners on a non-loopback address to avoid loopback
//! proxy exceptions. Compare a per-scheme `direct://` entry for HTTPS with the dead-proxy
//! backstop and with no proxy. Repeat without page policy so HTTP blocking can be attributed to
//! the proxy itself.
//!
//!     cargo test -p shell --test plugin_page -- --ignored
//!
//! Uses a plain `main` (`harness = false`) because GTK must own the main thread. Run one at a
//! time.

#[path = "support/web_clock.rs"]
mod web_clock;

fn build_with_clock(
    spec: &plugin_page::PageSpec,
    hooks: plugin_page::PageHooks,
    theme: &eitri_core::plugin::theme::PageTheme,
) -> webkit6::WebView {
    let manager = webkit6::UserContentManager::new();
    let turns = web_clock::install(&manager);
    let view = plugin_page::build_with_content(spec, hooks, theme, manager);
    web_clock::watch(&view, turns);
    view
}

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::Read;
use std::net::{IpAddr, TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eitri_core::plugin::host::{PagePort, PluginHost};
use eitri_core::plugin::load::load_plugin;
use eitri_core::plugin::state::PluginStore;
use eitri_core::plugin::test_support::demo_plugin;
use eitri_core::plugin::theme::PageTheme;
use eitri_core::theme::payload::{HlAttrs, NvimOptions, NvimThemePayload, PAYLOAD_VERSION};
use eitri_core::theme::ThemeTokens;
use gtk4::prelude::*;
use serde_json::Value;
use webkit6::prelude::WebViewExt;

#[path = "support/own_x_server.rs"]
mod own_x_server;

#[allow(dead_code)]
#[path = "../src/plugin_page.rs"]
mod plugin_page;

#[allow(dead_code, unused_imports)]
#[path = "../src/editor_hole.rs"]
mod editor_hole;
#[allow(dead_code, unused_imports)]
#[path = "../src/gtk_host/mod.rs"]
mod gtk_host;
#[allow(dead_code, unused_imports)]
#[path = "../src/host_clipboard.rs"]
mod host_clipboard;
// `gtk_host` tells a waiting launcher the window is up; nothing here has one, so the call does nothing.
#[allow(dead_code)]
#[path = "../src/launch_env.rs"]
mod launch_env;
#[allow(dead_code, unused_imports)]
#[path = "../src/module_grid.rs"]
mod module_grid;
#[allow(dead_code, unused_imports)]
#[path = "../src/terminal/mod.rs"]
mod terminal;
#[allow(dead_code, unused_imports)]
#[path = "../src/text_size.rs"]
mod text_size;
#[allow(dead_code, unused_imports)]
#[path = "../src/web_host.rs"]
mod web_host;
#[allow(dead_code, unused_imports)]
#[path = "../src/wheel_zoom.rs"]
mod wheel_zoom;

#[allow(dead_code, unused_imports)]
#[path = "../src/layout.rs"]
mod layout;

// The terminal's compiled move-test helpers refer to the link driver; this run creates no link.
#[allow(dead_code, unused_imports)]
#[path = "../src/linked_window/driver.rs"]
pub(crate) mod link_driver;
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LinkEnd {
    Bye { why: String },
    Eof,
}
mod linked_window {
    pub(crate) use super::link_driver as driver;
}

const INDEX: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>t</title>
<script>window.webkit.messageHandlers.eitriPlugin.postMessage(JSON.stringify({v:1,id:900,verb:"log.append",args:{stream:"probe",items:[{inline:true}]}}));</script>
<script src="/app.js"></script></head><body><a id="ext" href="https://example.invalid/x"
style="position:fixed;left:0;top:0;width:120px;height:40px;display:block">x</a></body></html>
"#;

const APP: &str = r#"(async () => {
  const report = (item) => window.eitriPlugin.call("log.append", { stream: "probe", items: [item] });
  const status = async (path) => { try { return (await fetch(path)).status; } catch (_) { return "failed"; } };
  const hello = await window.eitriPlugin.call("host.hello", {});
  await report({ hello: hello.capabilities });
  try { await window.eitriPlugin.call("task.start", { task: "watch", vars: {} }); await report({ undeclared: "accepted" }); }
  catch (e) { await report({ undeclared: e.code }); }
  await report({ data: await status("/data.json") });
  await report({ traversal: await status("/a%2f..%2f..%2fsecret.txt") });
  await report({ link: await status("/link.txt") });
  await report({ nosniff: (await fetch("/data.json")).headers.get("x-content-type-options") });
  const asScript = await new Promise((done) => {
    const tag = document.createElement("script");
    tag.src = "/fake.png";
    tag.onload = () => done("load");
    tag.onerror = () => done("error");
    document.head.append(tag);
  });
  await report({ imageScript: asScript, imageRan: window.imageRan === true });
  document.getElementById("ext").click();
  await report({ clicked: true });
  const save = document.createElement("a");
  save.href = URL.createObjectURL(new Blob(["saved\n"], { type: "application/x-ndjson" }));
  save.download = "../progress log.jsonl";
  document.body.append(save);
  save.click();
  await report({ saved: true });
  location.href = "/font.woff";
  await new Promise((resolve) => setTimeout(resolve, 500));
  const row = { action: "item.next", keys: ["]q"], context: "c", desc: "d" };
  try { await window.eitriPlugin.call("keys.declare", { rows: [row] }); await report({ declared: "ok" }); }
  catch (e) { await report({ declared: e.code }); }
  document.getElementById("ext").focus();
  location.href = "https://example.invalid/away";
  setTimeout(() => { report({ done: location.href }); }, 500);
})();
"#;

/// A fresh, canonical scratch root for one run.
fn scratch_root(label: &str) -> Result<PathBuf, String> {
    let root = std::env::temp_dir().join(format!("eitri-plugin-page-{label}{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    std::fs::canonicalize(&root).map_err(|e| e.to_string())
}

/// The demo plugin with a manifest that declares only `state` and no network origin.
fn state_only_plugin(root: &Path) -> (PathBuf, BTreeMap<String, String>) {
    let (dir, settings) = demo_plugin(root);
    let manifest = std::fs::read_to_string(dir.join("eitri-plugin.toml")).unwrap();
    let manifest = manifest
        .replacen(
            "[\"tasks\", \"editor.open\", \"editor.assist\", \"layout.focus\", \"state\", \"net\", \"editor.follow\", \"editor.commands\"]",
            "[\"state\"]",
            1,
        )
        .replacen(
            "[net]\norigins = [\"https://learn.example.org\"]\nimages = \"${images}\"\n",
            "",
            1,
        );
    let manifest = manifest[..manifest.find("[tasks.prepare]").unwrap()].to_string();
    std::fs::write(dir.join("eitri-plugin.toml"), manifest).unwrap();
    (dir, settings)
}

/// The title the first run's window carries, for `xdotool` to find it by.
fn window_title() -> String {
    format!("eitri-plugin-page-test-{}", std::process::id())
}

/// Runs `xdotool` on the window titled `title` on `display` (`%1` in `then` names it). The display is passed
/// explicitly, never inherited, so the input can reach no other server.
fn xdotool(display: &str, title: &str, then: &[&str]) -> Result<(), String> {
    let out = std::process::Command::new("timeout")
        .args(["10", "xdotool", "search", "--sync", "--onlyvisible", "--name"])
        .arg(format!("^{title}$"))
        .args(then)
        .env("DISPLAY", display)
        .output()
        .map_err(|e| format!("running timeout/xdotool: {e}"))?;
    match out.status.code() {
        Some(0) => Ok(()),
        // `timeout` answers 127 when it cannot run the command at all.
        Some(127) => Err("xdotool is not installed: the real-click check needs it".into()),
        _ => Err(format!(
            "xdotool failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

fn run(display: &str) -> Result<Vec<String>, String> {
    let root = scratch_root("")?;
    let (dir, settings) = state_only_plugin(&root);
    let site = dir.join("build/site");
    std::fs::write(site.join("index.html"), INDEX).unwrap();
    std::fs::write(site.join("app.js"), APP).unwrap();
    std::fs::write(site.join("data.json"), "{\"ok\":true}").unwrap();
    // Script text under an image's name: served as `image/png`, it must never run as a script.
    std::fs::write(site.join("fake.png"), "window.imageRan = true;\n").unwrap();
    std::fs::write(site.join("font.woff"), "wOFF").unwrap();
    std::fs::write(root.join("secret.txt"), "secret").unwrap();
    std::os::unix::fs::symlink(root.join("secret.txt"), site.join("link.txt")).unwrap();

    let plugin = Rc::new(load_plugin(dir.to_str().unwrap(), &settings, None)?);
    let store = PluginStore::new(root.join("state"), "demo");
    let opened: Rc<RefCell<Vec<String>>> = Rc::default();
    let host_cell: Rc<RefCell<Option<Rc<PluginHost>>>> = Rc::default();
    let hooks = plugin_page::PageHooks {
        on_message: {
            let cell = host_cell.clone();
            Rc::new(move |raw: &str| {
                let host = cell.borrow().clone();
                if let Some(host) = host {
                    host.page_message(raw, Instant::now());
                }
            })
        },
        on_committed: {
            let cell = host_cell.clone();
            Rc::new(move || {
                let host = cell.borrow().clone();
                if let Some(host) = host {
                    host.page_committed();
                }
            })
        },
        open_outside: {
            let opened = opened.clone();
            Rc::new(move |url: &str| opened.borrow_mut().push(url.to_owned()))
        },
    };
    let downloads = root.join("downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    let spec = plugin_page::PageSpec {
        id: "demo".into(),
        site_root: plugin.site_root.clone(),
        entry: "index.html".into(),
        origins: vec![],
        downloads: downloads.clone(),
        doc_slot: None,
        remote_images: false,
        console_to_stdout: false,
        agents_kit: false,
    };
    let window = gtk4::Window::new();
    window.set_title(Some(&window_title()));
    window.set_default_size(800, 600);
    let webview = build_with_clock(&spec, hooks, &PageTheme::fallback());
    window.set_child(Some(&webview));
    window.present();
    let page: Rc<dyn PagePort> = Rc::new(plugin_page::WebKitPluginPage(webview.downgrade()));
    let host = Rc::new(PluginHost::new(
        plugin,
        page,
        Arc::new(|| {}),
        Some(store.clone()),
        Vec::new(),
    ));
    *host_cell.borrow_mut() = Some(host.clone());

    let session = webview.network_session().ok_or("the view has no network session")?;
    if !session.is_ephemeral() {
        return Err("the page's network session is not ephemeral".into());
    }

    // Pump the host and GTK until the page reports it is done, or the hang guard ends the run.
    let context = gtk4::glib::MainContext::default();
    let deadline = Instant::now() + Duration::from_secs(60);
    let items = loop {
        while context.iteration(false) {}
        host.pump(Instant::now() + Duration::from_secs(1));
        let (items, _) = store.read_log("probe", None).unwrap_or_default();
        if items.iter().any(|v| v.get("done").is_some()) && downloads.join("progress_log.jsonl").exists() {
            break items;
        }
        if Instant::now() >= deadline {
            return Err(format!("the page never finished; it reported {items:?}"));
        }
        web_clock::pump(33);
    };
    let get = |key: &str| items.iter().find_map(|v| v.get(key).cloned()).unwrap_or(Value::Null);
    let mut report = Vec::new();
    let mut check = |ok: bool, what: String| {
        report.push(format!("{} {what}", if ok { "ok  " } else { "FAIL" }));
        ok
    };
    let mut all = true;
    all &= check(
        get("hello") == serde_json::json!(["state"]),
        format!("hello lists only state: {}", get("hello")),
    );
    all &= check(
        get("undeclared") == "not_declared",
        format!("an undeclared verb is refused: {}", get("undeclared")),
    );
    all &= check(get("data") == 200, format!("a site file is served: {}", get("data")));
    all &= check(
        get("traversal") != 200,
        format!("a path out of the site is refused: {}", get("traversal")),
    );
    all &= check(
        get("link") != 200,
        format!("a symlink out of the site is refused: {}", get("link")),
    );
    all &= check(get("inline").is_null(), "the inline script never ran".into());
    all &= check(
        get("nosniff") == "nosniff",
        format!("a site file is answered with nosniff: {}", get("nosniff")),
    );
    all &= check(
        get("imageScript") == "error" && get("imageRan") == false,
        format!(
            "a same-origin image never runs as a script: {} (ran: {})",
            get("imageScript"),
            get("imageRan")
        ),
    );
    all &= check(
        opened.borrow().is_empty(),
        format!(
            "a script's click() on an outside link opened nothing: {:?}",
            opened.borrow()
        ),
    );
    all &= check(
        get("declared") == "ok",
        format!(
            "after an in-origin load that never committed, the running document still declares its keys: {}",
            get("declared")
        ),
    );
    // WebKit's default answer to a main-frame response it cannot show is to ignore it, not to save it; the
    // load is abandoned either way, which is what the declaration above depends on.
    let font_saved = downloads.join("font.woff").exists();
    // A real click inside the link, in this window's own surface coordinates.
    let inside = gtk4::graphene::Point::new(60.0, 20.0);
    let at = webview
        .compute_point(&window, &inside)
        .ok_or("the view has no position in its window")?;
    let (dx, dy) = window.surface_transform();
    let (x, y) = (
        (f64::from(at.x()) + dx).round() as i32,
        (f64::from(at.y()) + dy).round() as i32,
    );
    let (x_, y_) = (x.to_string(), y.to_string());
    let clicked = xdotool(
        display,
        &window_title(),
        &["mousemove", "--window", "%1", &x_, &y_, "click", "1"],
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while clicked.is_ok() && opened.borrow().is_empty() && Instant::now() < deadline {
        while context.iteration(false) {}
        web_clock::pump(33);
    }
    all &= check(
        clicked.is_ok() && *opened.borrow() == ["https://example.invalid/x"],
        format!(
            "a real click on the link at ({x}, {y}) went outside: {:?} {}",
            opened.borrow(),
            clicked.err().unwrap_or_default()
        ),
    );
    // Enter on the link the page focused: WebKit turns it into a trusted click, so it is expected to go out too;
    // recorded rather than required, since whether the key reaches the view depends on this server's focus.
    let before = opened.borrow().len();
    let pressed = xdotool(
        display,
        &window_title(),
        &["windowfocus", "--sync", "%1", "key", "Return"],
    );
    web_clock::pump(2000);
    let enter = match pressed {
        Ok(()) => format!("{}", opened.borrow().len() > before),
        Err(e) => format!("not tried: {e}"),
    };
    let entry = "eitri-plugin://demo/index.html";
    all &= check(
        get("done") == entry,
        format!("the page did not navigate away: {}", get("done")),
    );
    all &= check(
        webview.uri().as_deref() == Some(entry),
        format!("the view stayed on {entry}"),
    );
    let saved = std::fs::read_to_string(downloads.join("progress_log.jsonl")).unwrap_or_default();
    all &= check(
        saved == "saved\n",
        format!("the page's own blob was saved under a safe name: {saved:?}"),
    );
    report.push(format!("info the in-origin font was saved as a download: {font_saved}"));
    report.push(format!("info Enter on the focused link went outside: {enter}"));
    let _ = std::fs::remove_dir_all(&root);
    if all {
        Ok(report)
    } else {
        Err(report.join("\n"))
    }
}

const THEME_INDEX: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>t</title>
<script src="/probe.js"></script></head><body><p>entry</p></body></html>
"#;

const THEME_SECOND: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>t</title>
<script src="/probe.js"></script></head><body><p>second</p></body></html>
"#;

/// Reports what the root's theme variables compute to, tagged with the page: in the first frame, after each
/// `theme` event, and for 600 ms after every `pageshow` (with `persisted`). With `?p2` it also keeps the root's
/// inline style removed for 3 s and logs every distinct `--nv-bg` it sees meanwhile, so what is measured is the
/// user style sheet alone. It never calls `host.hello`.
const PROBE: &str = r#"(() => {
  const page = location.pathname + location.search;
  const log = (item) =>
    window.eitriPlugin.call("log.append", { stream: "probe", items: [Object.assign({ page }, item)] });
  const read = () => {
    const style = getComputedStyle(document.documentElement);
    return { bg: style.getPropertyValue("--nv-bg").trim(), background: style.backgroundColor };
  };
  requestAnimationFrame(() => log(Object.assign({ first: true }, read())));
  window.eitriPlugin.on("theme", (data) => {
    const sent = data && data.vars ? data.vars["--nv-bg"] : null;
    requestAnimationFrame(() => log(Object.assign({ event: sent === undefined ? null : sent }, read())));
  });
  addEventListener("pageshow", (e) => {
    const start = performance.now();
    const seen = [];
    const poll = () => {
      const ms = Math.round(performance.now() - start);
      const bg = read().bg;
      if (seen.length === 0 || seen[seen.length - 1][1] !== bg) seen.push([ms, bg]);
      if (ms < 600) setTimeout(poll, 20);
      else log({ shown: e.persisted, seen });
    };
    poll();
  });
  if (location.search === "?p2") {
    const root = document.documentElement;
    root.removeAttribute("style");
    const observer = new MutationObserver(() => {
      if (root.hasAttribute("style")) root.removeAttribute("style");
    });
    observer.observe(root, { attributes: true, attributeFilter: ["style"] });
    log({ p2: "armed" });
    const start = performance.now();
    const seen = [];
    const poll = () => {
      const bg = read().bg;
      if (!seen.includes(bg)) seen.push(bg);
      if (performance.now() - start < 3000) {
        setTimeout(poll, 20);
      } else {
        observer.disconnect();
        log({ p2: "done", seen });
      }
    };
    poll();
  }
})();
"#;

/// A light colorscheme's page set, unlike the dark fallback in every colour it derives.
fn light_theme() -> PageTheme {
    let payload = NvimThemePayload {
        v: PAYLOAD_VERSION,
        groups: [(
            "Normal".to_owned(),
            HlAttrs {
                fg: Some(0x575279),
                bg: Some(0xfaf4ed),
                reverse: false,
            },
        )]
        .into_iter()
        .collect(),
        options: NvimOptions {
            background: "light".to_owned(),
            guifont: String::new(),
            colors_name: "dawn".to_owned(),
            cmdheight: None,
            start_scale: None,
        },
    };
    PageTheme::from_tokens(&ThemeTokens::derive(&payload))
}

/// `--nv-bg` as the sheet writes it, and the root's computed background for it.
fn expected(theme: &PageTheme) -> (String, String) {
    let bg = theme
        .vars()
        .iter()
        .find(|(name, _)| name == "--nv-bg")
        .map(|(_, value)| value.clone())
        .unwrap_or_default();
    let [r, g, b] = theme.background();
    (bg, format!("rgb({r}, {g}, {b})"))
}

/// Pumps GTK and the host until the probe stream holds an item at or after `from` that `wanted` accepts.
struct Probe {
    context: gtk4::glib::MainContext,
    host: Rc<PluginHost>,
    store: PluginStore,
}

impl Probe {
    fn items(&self) -> Vec<Value> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let Ok((items, next)) = self.store.read_log("probe", cursor.as_deref()) else {
                return all;
            };
            all.extend(items);
            match next {
                Some(next) => cursor = Some(next),
                None => return all,
            }
        }
    }

    fn wait(&self, from: usize, what: &str, wanted: impl Fn(&Value) -> bool) -> Result<Value, String> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            while self.context.iteration(false) {}
            self.host.pump(Instant::now() + Duration::from_secs(1));
            let items = self.items();
            if let Some(found) = items.iter().skip(from).find(|v| wanted(v)) {
                return Ok(found.clone());
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "{what}: never seen; after it was asked for: {:?}",
                    &items[from.min(items.len())..]
                ));
            }
            web_clock::pump(33);
        }
    }

    /// Runs GTK and the host for `long` without waiting on anything.
    fn settle(&self, long: Duration) {
        web_clock::pump(long.as_millis().try_into().unwrap());
        self.host.pump(Instant::now() + Duration::from_secs(1));
    }
}

/// The run's lines, and whether every check passed.
struct Report {
    lines: Vec<String>,
    all: bool,
}

impl Default for Report {
    fn default() -> Self {
        Report {
            lines: Vec::new(),
            all: true,
        }
    }
}

impl Report {
    fn check(&mut self, ok: bool, what: String) {
        self.lines.push(format!("{} {what}", if ok { "ok  " } else { "FAIL" }));
        self.all &= ok;
    }
}

fn on_page(item: &Value, page: &str) -> bool {
    item.get("page").and_then(Value::as_str) == Some(page)
}

/// Returns the report lines and whether a sheet swapped on a loaded document restyled it live.
fn run_theme() -> Result<(Vec<String>, bool), String> {
    let root = scratch_root("theme-")?;
    let (dir, settings) = state_only_plugin(&root);
    let site = dir.join("build/site");
    std::fs::write(site.join("index.html"), THEME_INDEX).unwrap();
    std::fs::write(site.join("second.html"), THEME_SECOND).unwrap();
    std::fs::write(site.join("probe.js"), PROBE).unwrap();

    let plugin = Rc::new(load_plugin(dir.to_str().unwrap(), &settings, None)?);
    let store = PluginStore::new(root.join("state"), "demo");
    let host_cell: Rc<RefCell<Option<Rc<PluginHost>>>> = Rc::default();
    let with_host = |act: fn(&PluginHost)| -> Rc<dyn Fn()> {
        let cell = host_cell.clone();
        Rc::new(move || {
            let host = cell.borrow().clone();
            if let Some(host) = host {
                act(&host);
            }
        })
    };
    let hooks = plugin_page::PageHooks {
        on_message: {
            let cell = host_cell.clone();
            Rc::new(move |raw: &str| {
                let host = cell.borrow().clone();
                if let Some(host) = host {
                    host.page_message(raw, Instant::now());
                }
            })
        },
        on_committed: with_host(PluginHost::page_committed),
        open_outside: Rc::new(|_: &str| {}),
    };
    let downloads = root.join("downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    let spec = plugin_page::PageSpec {
        id: "demo".into(),
        site_root: plugin.site_root.clone(),
        entry: "index.html".into(),
        origins: vec![],
        downloads,
        doc_slot: None,
        remote_images: false,
        console_to_stdout: false,
        agents_kit: false,
    };
    let fallback = PageTheme::fallback();
    let light = light_theme();
    let (dark_bg, dark_background) = expected(&fallback);
    let (light_bg, light_background) = expected(&light);
    if dark_bg != "#14161b" || dark_bg == light_bg {
        return Err(format!(
            "the probe's themes are not what it measures: {dark_bg} and {light_bg}"
        ));
    }

    let window = gtk4::Window::new();
    window.set_default_size(800, 600);
    let webview = build_with_clock(&spec, hooks, &fallback);
    window.set_child(Some(&webview));
    window.present();
    let page: Rc<dyn PagePort> = Rc::new(plugin_page::WebKitPluginPage(webview.downgrade()));
    let host = Rc::new(PluginHost::new(
        plugin,
        page.clone(),
        Arc::new(|| {}),
        Some(store.clone()),
        Vec::new(),
    ));
    *host_cell.borrow_mut() = Some(host.clone());
    let probe = Probe {
        context: gtk4::glib::MainContext::default(),
        host: host.clone(),
        store,
    };

    let mut report = Report::default();
    let field = |item: &Value, key: &str| item.get(key).and_then(Value::as_str).unwrap_or("").to_owned();
    let entry = "eitri-plugin://demo/index.html";

    // The first document's first frame already has the theme stylesheet.
    let first = probe.wait(0, "P1 first frame", |v| {
        on_page(v, "/index.html") && v.get("first").is_some()
    })?;
    report.check(
        field(&first, "bg") == dark_bg && field(&first, "background") == dark_background,
        format!("P1 the first frame reads the fallback: {first}"),
    );

    // A reload after the host changed the theme starts from the new sheet.
    let mark = probe.items().len();
    host.set_theme(&light);
    webview.reload();
    let reloaded = probe.wait(mark, "reload first frame", |v| {
        on_page(v, "/index.html") && v.get("first").is_some()
    })?;
    report.check(
        field(&reloaded, "bg") == light_bg && field(&reloaded, "background") == light_background,
        format!("a reload's first frame reads the new theme: {reloaded}"),
    );

    // Swap the sheet on a loaded document with root inline values held off, then check the theme event.
    let mark = probe.items().len();
    webview.load_uri(&format!("{entry}?p2"));
    probe.wait(mark, "P2 armed", |v| {
        on_page(v, "/index.html?p2") && field(v, "p2") == "armed"
    })?;
    page.set_theme(&fallback);
    let done = probe.wait(mark, "P2 done", |v| {
        on_page(v, "/index.html?p2") && field(v, "p2") == "done"
    })?;
    let seen: Vec<String> = done
        .get("seen")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default();
    let live = seen.contains(&dark_bg);
    report.lines.push(format!(
        "info P2 a swapped sheet restyles a loaded document live: {} (values seen: {seen:?})",
        if live { "yes" } else { "no" }
    ));
    let mark = probe.items().len();
    host.set_theme(&fallback);
    let event = probe.wait(mark, "P2 theme event", |v| {
        on_page(v, "/index.html?p2") && v.get("event").and_then(Value::as_str) == Some(dark_bg.as_str())
    })?;
    report.check(
        field(&event, "bg") == dark_bg,
        format!("P2 the theme event brings the new set: {event}"),
    );

    // Back navigation restores a document that reads the current theme without another hello.
    let mark = probe.items().len();
    webview.load_uri(entry);
    probe.wait(mark, "P4 entry", |v| {
        on_page(v, "/index.html") && v.get("shown").is_some()
    })?;
    let mark = probe.items().len();
    webview.load_uri("eitri-plugin://demo/second.html");
    probe.wait(mark, "P4 second page", |v| {
        on_page(v, "/second.html") && v.get("first").is_some()
    })?;
    host.set_theme(&light);
    let mark = probe.items().len();
    webview.go_back();
    let shown = probe.wait(mark, "P4 restored entry", |v| {
        on_page(v, "/index.html") && v.get("shown").is_some()
    })?;
    let readings: Vec<(u64, String)> = shown
        .get("seen")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|pair| Some((pair.get(0)?.as_u64()?, pair.get(1)?.as_str()?.to_owned())))
                .collect()
        })
        .unwrap_or_default();
    let in_time = readings.iter().any(|(ms, bg)| *ms <= 500 && *bg == light_bg);
    let stays = readings.last().is_some_and(|(_, bg)| *bg == light_bg);
    // The events the restored document heard, so a failure says whether the commit's event reached it.
    let heard = Value::Array(
        probe
            .items()
            .into_iter()
            .skip(mark)
            .filter(|v| on_page(v, "/index.html") && v.get("event").is_some())
            .collect(),
    );
    report.check(
        in_time && stays,
        format!(
            "P4 the entry gone back to reads the new set within 500 ms (persisted: {}): {readings:?}; heard {heard}",
            shown.get("shown").cloned().unwrap_or(Value::Null)
        ),
    );
    probe.settle(Duration::from_millis(100));

    window.close();
    let _ = std::fs::remove_dir_all(&root);
    if report.all {
        Ok((report.lines, live))
    } else {
        Err(report.lines.join("\n"))
    }
}

const P4_SCRIPT: &str = r#"(() => {
  const settle = (img) => new Promise((done) => {
    if (img.complete) { done(img.naturalWidth > 0 ? "load" : "error"); return; }
    img.addEventListener("load", () => done("load"), { once: true });
    img.addEventListener("error", () => done("error"), { once: true });
  });
  addEventListener("DOMContentLoaded", async () => {
    const [https, http] = await Promise.all([settle(document.getElementById("s")), settle(document.getElementById("h"))]);
    await window.eitriPlugin.call("log.append", { stream: "probe", items: [{ p4: true, https, http }] });
  });
})();
"#;

/// A port that notes every connection and what its first bytes look like (`tls` for a handshake, `plain` for a
/// request line), then closes it unanswered, so the image fails whichever way it went.
struct Listener {
    port: u16,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Listener {
    fn start() -> Result<Listener, String> {
        let listener = TcpListener::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let noted = seen.clone();
        // Detached: it blocks in `accept` until the process ends.
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
                let mut head = [0u8; 4];
                let kind = match stream.read_exact(&mut head) {
                    Ok(()) if head[0] == 0x16 => "tls",
                    Ok(()) if head[0].is_ascii_uppercase() => "plain",
                    Ok(()) => "empty",
                    Err(_) => "empty",
                };
                if head == *b"SYNC" {
                    let _ = std::io::Write::write_all(&mut stream, b"ok");
                    continue;
                }
                noted.lock().unwrap().push(kind.to_owned());
            }
        });
        Ok(Listener { port, seen })
    }

    fn seen(&self) -> Vec<String> {
        use std::io::Write;
        let mut marker = std::net::TcpStream::connect(("127.0.0.1", self.port)).expect("the listener marker connects");
        marker.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
        marker.write_all(b"SYNC").unwrap();
        let mut ack = [0; 2];
        marker
            .read_exact(&mut ack)
            .expect("the listener never acknowledged earlier accepts");
        assert_eq!(&ack, b"ok");
        self.seen.lock().unwrap().clone()
    }
}

/// This host's own non-loopback IPv4 address: the source address the kernel picks for a documentation address.
/// Nothing is sent.
fn own_address() -> Result<IpAddr, String> {
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| e.to_string())?;
    socket.connect("192.0.2.1:9").map_err(|e| e.to_string())?;
    let ip = socket.local_addr().map_err(|e| e.to_string())?.ip();
    if ip.is_loopback() || ip.is_unspecified() {
        return Err(format!("no non-loopback address to probe with: {ip}"));
    }
    Ok(ip)
}

/// How a probe view's session is set after the page is built, before anything it loads reaches the network.
#[derive(Clone, Copy)]
enum ProxyOverride {
    /// What `build` set from the page's own proxy plan.
    Keep,
    /// The backstop for every scheme.
    Backstop,
    /// No proxy at all.
    None,
}

/// Which listeners one view reached, and what the page saw.
struct ViewOutcome {
    https: Vec<String>,
    http: Vec<String>,
    page: Value,
    /// The view's network session, with the proxy the view ran under.
    session: webkit6::NetworkSession,
}

impl ViewOutcome {
    fn reached(&self) -> &'static str {
        match (self.https.is_empty(), self.http.is_empty()) {
            (true, true) => "neither",
            (false, true) => "https only",
            (true, false) => "http only",
            (false, false) => "both",
        }
    }

    fn detail(&self) -> String {
        format!(
            "{} (https listener {:?}, http listener {:?}, page {})",
            self.reached(),
            self.https,
            self.http,
            self.page
        )
    }
}

/// One view of a state-only plugin whose page holds an `https` and an `http` image on this host's listeners.
fn p4_view(label: &str, ip: IpAddr, remote_images: bool, proxy: ProxyOverride) -> Result<ViewOutcome, String> {
    let root = scratch_root(&format!("p4-{label}-"))?;
    let (dir, settings) = state_only_plugin(&root);
    let (secure, plain) = (Listener::start()?, Listener::start()?);
    let site = dir.join("build/site");
    let index = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>p4</title><script src=\"/p4.js\"></script>\
         </head><body><img id=\"s\" src=\"https://{ip}:{}/p.png\"><img id=\"h\" src=\"http://{ip}:{}/p.png\">\
         </body></html>\n",
        secure.port, plain.port
    );
    std::fs::write(site.join("index.html"), index).unwrap();
    std::fs::write(site.join("p4.js"), P4_SCRIPT).unwrap();

    let plugin = Rc::new(load_plugin(dir.to_str().unwrap(), &settings, None)?);
    let store = PluginStore::new(root.join("state"), "demo");
    let host_cell: Rc<RefCell<Option<Rc<PluginHost>>>> = Rc::default();
    let hooks = plugin_page::PageHooks {
        on_message: {
            let cell = host_cell.clone();
            Rc::new(move |raw: &str| {
                let host = cell.borrow().clone();
                if let Some(host) = host {
                    host.page_message(raw, Instant::now());
                }
            })
        },
        on_committed: Rc::new(|| {}),
        open_outside: Rc::new(|_: &str| {}),
    };
    let downloads = root.join("downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    let spec = plugin_page::PageSpec {
        id: "demo".into(),
        site_root: plugin.site_root.clone(),
        entry: "index.html".into(),
        origins: vec![],
        downloads,
        doc_slot: None,
        remote_images,
        console_to_stdout: false,
        agents_kit: false,
    };
    let window = gtk4::Window::new();
    window.set_default_size(400, 300);
    let webview = build_with_clock(&spec, hooks, &PageTheme::fallback());
    // Set before the main loop runs again: the images are asked for only once the entry has been served and
    // parsed, which needs the loop.
    let session = webview.network_session().ok_or("the view has no network session")?;
    match proxy {
        ProxyOverride::Keep => {}
        ProxyOverride::Backstop => session.set_proxy_settings(
            webkit6::NetworkProxyMode::Custom,
            Some(&webkit6::NetworkProxySettings::new(
                Some(eitri_core::plugin::page_policy::PROXY_BACKSTOP),
                &[],
            )),
        ),
        ProxyOverride::None => session.set_proxy_settings(webkit6::NetworkProxyMode::NoProxy, None),
    }
    window.set_child(Some(&webview));
    window.present();
    let page: Rc<dyn PagePort> = Rc::new(plugin_page::WebKitPluginPage(webview.downgrade()));
    let host = Rc::new(PluginHost::new(
        plugin,
        page,
        Arc::new(|| {}),
        Some(store.clone()),
        Vec::new(),
    ));
    *host_cell.borrow_mut() = Some(host.clone());
    let probe = Probe {
        context: gtk4::glib::MainContext::default(),
        host,
        store,
    };
    let page = probe.wait(0, &format!("P4 {label}"), |v| v.get("p4").is_some())?;
    // The page's image load/error reports precede the listeners' accept barriers below.
    window.close();
    let _ = std::fs::remove_dir_all(&root);
    Ok(ViewOutcome {
        https: secure.seen(),
        http: plain.seen(),
        page,
        session,
    })
}

/// Which listeners an `https` and an `http` image reach over `session` in a view with no content security
/// policy at all. A plugin page's policy keeps `http` images from ever being asked for, so only here does the
/// proxy alone decide whether an `http` request leaves.
fn p4_open_view(label: &str, ip: IpAddr, session: &webkit6::NetworkSession) -> Result<ViewOutcome, String> {
    let (secure, plain) = (Listener::start()?, Listener::start()?);
    let html = format!(
        "<!doctype html><html><body><img src=\"https://{ip}:{}/p.png\"><img src=\"http://{ip}:{}/p.png\">\
         </body></html>\n",
        secure.port, plain.port
    );
    let webview = webkit6::WebView::builder().network_session(session).build();
    let finished = Rc::new(std::cell::Cell::new(false));
    {
        let finished = finished.clone();
        webview.connect_load_changed(move |_, event| {
            if event == webkit6::LoadEvent::Finished {
                finished.set(true);
            }
        });
    }
    let window = gtk4::Window::new();
    window.set_default_size(400, 300);
    window.set_child(Some(&webview));
    window.present();
    webview.load_html(&html, None);
    let context = gtk4::glib::MainContext::default();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !finished.get() {
        while context.iteration(false) {}
        if Instant::now() >= deadline {
            window.close();
            return Err(format!("P4 {label} without a policy: the page never finished loading"));
        }
        web_clock::pump(33);
    }
    window.close();
    Ok(ViewOutcome {
        https: secure.seen(),
        http: plain.seen(),
        page: Value::Null,
        session: session.clone(),
    })
}

/// Check whether a per-scheme `direct://` entry lets HTTPS images through while HTTP stays on
/// the dead proxy. Compare the default `build` page with a plain backstop and with no proxy.
/// With remote images disabled, neither listener should be reached.
fn run_proxy_probe() -> Result<Vec<String>, String> {
    let ip = own_address()?;
    let candidate = p4_view("candidate", ip, true, ProxyOverride::Keep)?;
    let control = p4_view("control", ip, true, ProxyOverride::Backstop)?;
    let open = p4_view("noproxy", ip, true, ProxyOverride::None)?;
    let off = p4_view("off", ip, false, ProxyOverride::Keep)?;
    // The same sessions under no policy: the open session must let an `http` image out, or the probe could not
    // see one at all; then the candidate's must not, which is the proxy keeping `http` back on its own.
    let open_bare = p4_open_view("noproxy", ip, &open.session)?;
    let candidate_bare = p4_open_view("candidate", ip, &candidate.session)?;
    let control_bare = p4_open_view("control", ip, &control.session)?;
    let mut lines = vec![
        format!(
            "info P4 WebKitGTK {}.{}.{}, probing {ip}",
            webkit6::functions::major_version(),
            webkit6::functions::minor_version(),
            webkit6::functions::micro_version()
        ),
        format!("info P4 candidate: {}", candidate.detail()),
        format!("info P4 control: {}", control.detail()),
        format!("info P4 no proxy: {}", open.detail()),
        format!("info P4 remote images off: {}", off.detail()),
        format!("info P4 no proxy, no policy: {}", open_bare.detail()),
        format!("info P4 candidate, no policy: {}", candidate_bare.detail()),
        format!("info P4 control, no policy: {}", control_bare.detail()),
    ];
    if open_bare.reached() != "both" {
        return Err(format!(
            "P4 inconclusive: with no proxy and no policy the images did not both reach their listeners, so the \
             probe cannot tell what the proxy holds back: {}\n{}",
            open_bare.detail(),
            lines.join("\n")
        ));
    }
    if control.reached() != "neither" || control_bare.reached() != "neither" {
        return Err(format!(
            "P4 inconclusive: the plain backstop let a request through: {}; without the policy: {}\n{}",
            control.detail(),
            control_bare.detail(),
            lines.join("\n")
        ));
    }
    if !open.http.is_empty() || open.https.is_empty() {
        return Err(format!(
            "P4 stop: with no proxy the policy alone did not keep to https images: {}\n{}",
            open.detail(),
            lines.join("\n")
        ));
    }
    let passes = candidate.reached() == "https only" && candidate_bare.reached() == "https only";
    lines.push(format!("P4 https direct: {}", if passes { "passes" } else { "fails" }));
    lines.push(format!("P4 control: {}", control.reached()));
    lines.push(format!("P4 no proxy: {}", open.reached()));
    lines.push(format!("P4 candidate without the policy: {}", candidate_bare.reached()));
    if !passes {
        return Err(format!(
            "a page with remote images must reach https images and never http ones, and its proxy alone must hold \
             http back: {}; without the policy: {}\n{}",
            candidate.detail(),
            candidate_bare.detail(),
            lines.join("\n")
        ));
    }
    if off.reached() != "neither" {
        return Err(format!(
            "a page with remote images off must reach no remote image: {}\n{}",
            off.detail(),
            lines.join("\n")
        ));
    }
    Ok(lines)
}

/// Drives the same guarded restart as the terminal service, with the real pane and host.
#[allow(deprecated)]
fn terminal_at_restarts_the_base_terminal_in_place() -> Result<Vec<String>, String> {
    use eitri_core::layout::{Axis, Layout, ModuleId, Node};
    use eitri_core::wm::{Applied, ChatOp, EditorCall, Host, HostOp, PluginOp, ShellStart, TermOp, WinId};

    #[derive(Clone)]
    struct Pane(terminal::TerminalPane);
    impl gtk_host::RespawnPane for Pane {
        fn shell_start(&self) -> ShellStart {
            ShellStart::of(self.0.generation(), self.0.shell_pid()).typed(self.0.typed())
        }
        fn close(&self) {
            self.0.close();
        }
        fn set_start_dir(&self, dir: PathBuf) {
            self.0.set_start_dir(dir);
        }
        fn start(&self) {
            self.0.start();
        }
    }
    impl gtk_host::ServiceSet for Pane {
        fn editor(&self, _: &EditorCall) -> Result<(), String> {
            unreachable!()
        }
        fn plugin(&self, _: &PluginOp) -> Result<(), String> {
            unreachable!()
        }
        fn chat(&self, _: &ChatOp) -> Result<(), String> {
            unreachable!()
        }
        fn terminal(&self, op: &TermOp) -> Result<(), String> {
            match op {
                TermOp::Respawn { expect, dir, .. } => gtk_host::respawn_pane(Some(self.clone()), *expect, dir.clone()),
                _ => unreachable!(),
            }
        }
    }
    fn wait_for(mut predicate: impl FnMut() -> bool) -> Result<(), String> {
        let context = gtk4::glib::MainContext::default();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            while context.iteration(false) {}
            if predicate() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err("terminal restart did not settle in 5 s".into());
            }
            web_clock::pump(33);
        }
    }

    let root = scratch_root("terminal-at")?;
    let target = root.join("next");
    std::fs::create_dir(&target).map_err(|e| e.to_string())?;
    // No interactive user configuration or user startup commands enter these shells.
    std::env::set_var("SHELL", "/bin/sh");
    std::env::remove_var("ENV");
    std::env::remove_var("BASH_ENV");
    let mut report = Vec::new();
    for (page_has_keys, only_terminal) in [(false, false), (true, false), (false, true)] {
        let terminal_id = ModuleId::terminal();
        let page_id = ModuleId::lua("demo");
        let owner = if page_has_keys {
            page_id.clone()
        } else {
            terminal_id.clone()
        };
        let root_node = if only_terminal {
            Node::Leaf(terminal_id.clone())
        } else {
            Node::split(
                Axis::Row,
                0.5,
                Node::Leaf(terminal_id.clone()),
                Node::Leaf(page_id.clone()),
            )
        };
        let layout = Layout::new(root_node, owner).map_err(|e| e.to_string())?;
        let (view, _) = eitri_core::wm::LayoutView::for_test(layout);
        let grid = module_grid::ModuleGrid::new(view);
        let pane = terminal::TerminalPane::new(root.clone());
        let page = webkit6::WebView::new();
        page.load_html("<!doctype html><input autofocus>", None);
        let page_host = web_host::WebHost::new(&page);
        grid.add(
            terminal_id.clone(),
            pane.widget(),
            pane.widget(),
            module_grid::HostKind::Direct,
        );
        grid.add(page_id.clone(), &page_host, &page, module_grid::HostKind::Web);
        let window = gtk4::Window::new();
        window.set_default_size(900, 600);
        window.set_child(Some(&grid));
        let host = gtk_host::GtkHost::new();
        host.adopt_window(WinId::MAIN, &window);
        host.register_surface(WinId::MAIN, terminal_id, pane.widget().upcast_ref());
        host.set_services(Rc::new(Pane(pane.clone())));
        grid.apply();
        window.present();
        pane.start();
        wait_for(|| pane.shell_pid().is_some() && !page.is_loading())?;
        let focus: gtk4::Widget = if page_has_keys {
            page.clone().upcast()
        } else {
            pane.widget().clone().upcast()
        };
        if !focus.grab_focus() {
            return Err("the module could not take the keys".into());
        }
        wait_for(|| focus.has_focus() || GtkWindowExt::focus(&window).is_some_and(|w| w.is_ancestor(&focus)))?;
        let keys = GtkWindowExt::focus(&window);
        let geometry = (grid.allocation(), pane.widget().allocation(), page_host.allocation());
        let shell = ShellStart::of(pane.generation(), pane.shell_pid()).typed(pane.typed());
        let op = HostOp::Terminal(TermOp::Respawn {
            win: WinId::MAIN,
            module: ModuleId::terminal(),
            dir: target.clone(),
            expect: shell,
        });
        if host.apply(&op) != Applied::Done {
            return Err("the live pane refused its restart".into());
        }
        wait_for(|| pane.shell_pid().is_some_and(|pid| Some(pid) != shell.pid))?;
        if geometry != (grid.allocation(), pane.widget().allocation(), page_host.allocation())
            || keys != GtkWindowExt::focus(&window)
            || !window.is_visible()
            || terminal::exit_is_current(shell.generation, pane.generation())
            || (only_terminal && page_host.is_child_visible())
        {
            return Err(format!("restart changed geometry, keys, visibility or accepted an old exit: page_keys={page_has_keys} only_terminal={only_terminal}"));
        }
        let found = Rc::new(RefCell::new(None));
        let answer = found.clone();
        let pid = pane.shell_pid().unwrap();
        gtk4::glib::spawn_future_local(async move {
            *answer.borrow_mut() =
                Some(gtk4::gio::spawn_blocking(move || std::fs::read_link(format!("/proc/{pid}/cwd"))).await);
        });
        wait_for(|| found.borrow().is_some())?;
        let cwd = found
            .borrow_mut()
            .take()
            .unwrap()
            .map_err(|_| "the cwd worker panicked".to_string())?
            .map_err(|e| e.to_string())?;
        if cwd != target {
            return Err(format!("the restarted shell is in {}", cwd.display()));
        }
        report.push(format!("terminal_at: restart keeps allocation and keys (page={page_has_keys}, alone={only_terminal}); old exit is stale"));
        pane.close();
        window.destroy();
    }
    let _ = std::fs::remove_dir_all(root);
    Ok(report)
}

fn main() {
    // Mirrors `#[ignore]` under a plain `main`: `cargo test --workspace` needs no display.
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!("plugin_page: ignored (drives real WebKitGTK on a display); run with `-- --ignored`");
        return;
    }
    let server = own_x_server::init_gtk("plugin_page", "1280x800x24");
    let outcome = if args.iter().any(|arg| arg == "terminal_at") {
        terminal_at_restarts_the_base_terminal_in_place()
    } else {
        run(server.display()).and_then(|mut report| {
            let (theme, live) = run_theme()?;
            report.extend(theme);
            report.push(format!("P2 live restyle: {}", if live { "live" } else { "not live" }));
            report.extend(run_proxy_probe()?);
            report.extend(terminal_at_restarts_the_base_terminal_in_place()?);
            Ok(report)
        })
    };
    match outcome {
        Ok(report) => {
            for line in report {
                println!("plugin_page: {line}");
            }
            println!("plugin_page: ok");
            server.exit(0);
        }
        Err(e) => {
            eprintln!("plugin_page: FAILED:\n{e}");
            server.exit(1);
        }
    }
}
