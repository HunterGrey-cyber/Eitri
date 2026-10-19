//! A plugin page in the real WebKitGTK, on its own Xvfb (never the desktop): the scheme serves the site and
//! refuses a path out of it, the CSP blocks inline script, the navigation lock holds, a link click is handed
//! out, and the bridge round-trips through a real `PluginHost` whose manifest declares only `state`, so an
//! undeclared verb is refused.
//!
//!     cargo test -p shell --test plugin_page -- --ignored
//!
//! A plain `main` (`harness = false`), because GTK must own the main thread. Under the GUI slot, one at a time.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eitri_core::plugin::host::{PagePort, PluginHost};
use eitri_core::plugin::load::load_plugin;
use eitri_core::plugin::state::PluginStore;
use eitri_core::plugin::test_support::demo_plugin;
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
<script src="/app.js"></script></head><body><a id="ext" href="https://example.invalid/x">x</a></body></html>
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
  location.href = "https://example.invalid/away";
  setTimeout(() => { report({ done: location.href }); }, 500);
})();
"#;

fn run() -> Result<Vec<String>, String> {
    let root = std::env::temp_dir().join(format!("eitri-plugin-page-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let root = std::fs::canonicalize(&root).map_err(|e| e.to_string())?;
    let (dir, settings) = demo_plugin(&root);
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
    let site = dir.join("build/site");
    std::fs::write(site.join("index.html"), INDEX).unwrap();
    std::fs::write(site.join("app.js"), APP).unwrap();
    std::fs::write(site.join("data.json"), "{\"ok\":true}").unwrap();
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
        on_loading: Rc::new(|| {}),
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
    window.set_default_size(800, 600);
    let webview = plugin_page::build(&spec, hooks);
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
        *opened.borrow() == ["https://example.invalid/x"],
        format!("the link click went outside: {:?}", opened.borrow()),
    );
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
    let _ = std::fs::remove_dir_all(&root);
    if all {
        Ok(report)
    } else {
        Err(report.join("\n"))
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
    match run() {
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
