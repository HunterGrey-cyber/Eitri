//! A plugin page in the real WebKitGTK, on its own Xvfb (never the desktop): the scheme serves the site and
//! refuses a path out of it, the CSP blocks inline script, the navigation lock holds, a script's `click()` on
//! an outside link opens nothing while a real click on it (`xdotool`, aimed at this test's own server) is
//! handed out, an in-origin load that never commits leaves the running document its hello, and the
//! bridge round-trips through a real `PluginHost` whose manifest declares only `state`, so an
//! undeclared verb is refused. A second run, with its own site and host, follows the window's theme: the
//! first frame and a reload paint from the host's user style sheet, a sheet swapped on a loaded document is
//! recorded as restyled live or not, the `theme` event delivers the new set, and a document restored by going
//! back reads the current set without saying hello.
//!
//!     cargo test -p shell --test plugin_page -- --ignored
//!
//! A plain `main` (`harness = false`), because GTK must own the main thread. Under the GUI slot, one at a time.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
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
            "[\"tasks\", \"editor.open\", \"editor.assist\", \"layout.focus\", \"state\", \"net\"]",
            "[\"state\"]",
            1,
        )
        .replacen("[net]\norigins = [\"https://learn.example.org\"]\n", "", 1);
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
    };
    let window = gtk4::Window::new();
    window.set_title(Some(&window_title()));
    window.set_default_size(800, 600);
    let webview = plugin_page::build(&spec, hooks, &PageTheme::fallback());
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
        std::thread::sleep(Duration::from_millis(10));
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
        std::thread::sleep(Duration::from_millis(10));
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
    let until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < until {
        while context.iteration(false) {}
        std::thread::sleep(Duration::from_millis(10));
    }
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
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Runs GTK and the host for `long` without waiting on anything.
    fn settle(&self, long: Duration) {
        let until = Instant::now() + long;
        while Instant::now() < until {
            while self.context.iteration(false) {}
            self.host.pump(Instant::now() + Duration::from_secs(1));
            std::thread::sleep(Duration::from_millis(10));
        }
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
    let webview = plugin_page::build(&spec, hooks, &fallback);
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

fn main() {
    // Mirrors `#[ignore]` under a plain `main`: `cargo test --workspace` needs no display.
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!("plugin_page: ignored (drives real WebKitGTK on a display); run with `-- --ignored`");
        return;
    }
    let server = own_x_server::init_gtk("plugin_page", "1280x800x24");
    let outcome = run(server.display()).and_then(|mut report| {
        let (theme, live) = run_theme()?;
        report.extend(theme);
        report.push(format!("P2 live restyle: {}", if live { "live" } else { "not live" }));
        Ok(report)
    });
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
