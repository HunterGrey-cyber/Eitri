//! The agent panel's bottom band is one editor row high at any `gtk-xft-dpi`, in real WebKitGTK.
//!
//! **What this holds (the v1-scale sandbox pass, 2026-09-27, its defect A).** `--nv-editor-row` is
//! the editor's cell height, which `neovide-editor` reports in GTK logical px. WebKitGTK zooms the
//! page by `gtk-xft-dpi / 1024 / 96` (`shell/src/webkit_zoom.rs`'s module doc cites the source), so
//! at the owner's 144 dpi one CSS px is 1.5 logical px: the 22px row, sent as `22px`, drew a band
//! 33px high whose top reached 11px into the editor's statusline. This loads a page into a real
//! `webkit6::WebView`, gives an element the band's own `height: var(--nv-editor-row)` from the value
//! `webkit_zoom::EditorRow` produces (compiled in from `src/` below) through
//! `ThemeTokens::css_vars`, and reads its height back in GTK logical px -- CSS px times the zoom the
//! page itself reports (`devicePixelRatio` over the widget's scale factor; the page's own height in
//! CSS px times that zoom must also come back to the widget's height, or the premise is wrong). It
//! does so at 144 dpi, then live at 146 dpi (which WebKit ignores: within 2% of the zoom it applied),
//! 96 and 120 dpi, the way a desktop's text-scaling change arrives.
//!
//! Needs a display, and never the real desktop: run it under its own X server, with GTK told to use
//! it rather than a Wayland session the shell may have exported (and `GDK_SCALE=2` for scale 2) --
//!
//!     xvfb-run -a -s "-nolisten tcp -screen 0 1024x900x24" env GDK_BACKEND=x11 \
//!         cargo test -p shell --test band_xft_zoom -- --ignored
//!
//! A plain `main` (`harness = false`), because GTK must own the main thread; it refuses to start
//! unless `GDK_BACKEND=x11`. A few seconds.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::prelude::*;
use neovibe_core::theme::ThemeTokens;
use serde_json::Value;
use webkit6::prelude::*;
use webkit6::WebView;

// `unused_imports`: the module's own `#[cfg(test)] mod tests` comes along, its `#[test]`s dropped for
// want of a harness, and its `use super::*` with them.
#[allow(dead_code, unused_imports)]
#[path = "../src/webkit_zoom.rs"]
mod webkit_zoom;

/// The editor row the v1-scale pass measured, in GTK logical px.
const ROW: f32 = 22.0;

/// What `neovide-editor` would have, and `main.rs` hands the panel.
fn editor_row_var(css_px: f32) -> String {
    let mut tokens = ThemeTokens::fallback();
    tokens.editor_row_px = Some(css_px);
    tokens
        .css_vars()
        .into_iter()
        .find(|(name, _)| name == "--nv-editor-row")
        .map(|(_, value)| value)
        .expect("css_vars emits --nv-editor-row once it is Some")
}

const PAGE: &str = r#"<!doctype html><html><head><style>
html, body { margin: 0; height: 100%; }
.status-band { position: fixed; left: 0; right: 0; bottom: 0; height: var(--nv-editor-row, 24px);
  box-sizing: border-box; border-top: 1px solid #888; }
</style></head><body><div class="status-band"></div></body></html>"#;

const PROBE: &str = r#"JSON.stringify({
  dpr: window.devicePixelRatio,
  inner: window.innerHeight,
  band: document.querySelector('.status-band').getBoundingClientRect().height,
  bottom: document.querySelector('.status-band').getBoundingClientRect().bottom
})"#;

fn pump(ms: u64) {
    let context = gtk4::glib::MainContext::default();
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        while context.iteration(false) {}
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn eval(webview: &WebView, script: &str) -> Result<String, String> {
    let out: Rc<RefCell<Option<Result<String, String>>>> = Rc::new(RefCell::new(None));
    {
        let out = out.clone();
        webview.evaluate_javascript(script, None, None, None::<&gtk4::gio::Cancellable>, move |r| {
            *out.borrow_mut() = Some(r.map(|v| v.to_str().to_string()).map_err(|e| e.to_string()));
        });
    }
    let end = Instant::now() + Duration::from_secs(10);
    while out.borrow().is_none() {
        if Instant::now() > end {
            return Err(format!("no answer to {script:?} in 10s"));
        }
        pump(5);
    }
    let answer = out.borrow_mut().take().unwrap();
    answer
}

struct Reading {
    zoom: f64,
    inner: f64,
    band_css: f64,
    bottom_css: f64,
}

fn read(webview: &WebView) -> Result<Reading, String> {
    let v: Value = serde_json::from_str(&eval(webview, PROBE)?).map_err(|e| e.to_string())?;
    let num = |k: &str| v[k].as_f64().ok_or_else(|| format!("probe has no {k}: {v}"));
    Ok(Reading {
        zoom: num("dpr")? / f64::from(webview.scale_factor()),
        inner: num("inner")?,
        band_css: num("band")?,
        bottom_css: num("bottom")?,
    })
}

/// Sets `gtk-xft-dpi`, waits until the page's zoom is `expected_zoom`, re-sends the row as `main.rs`
/// does, and checks the band in logical px.
fn step(
    webview: &WebView,
    settings: &gtk4::Settings,
    row: &mut webkit_zoom::EditorRow,
    xft_dpi: i32,
    expected_zoom: f64,
) -> Result<String, String> {
    settings.set_gtk_xft_dpi(xft_dpi);
    if let Some(css_px) = row.follow_xft_dpi(settings.gtk_xft_dpi()) {
        let js = format!(
            "document.documentElement.style.setProperty('--nv-editor-row', {:?}); 0",
            editor_row_var(css_px)
        );
        eval(webview, &js)?;
    }
    // Long enough for a zoom WebKit should NOT apply (the 2% step below) to have arrived if it would.
    pump(400);
    let end = Instant::now() + Duration::from_secs(10);
    let r = loop {
        let r = read(webview)?;
        if (r.zoom - expected_zoom).abs() < 1e-6 || Instant::now() > end {
            break r;
        }
        pump(20);
    };
    let height = f64::from(webview.height());
    let band = r.band_css * r.zoom;
    let line = format!(
        "xft {xft_dpi}: scale {} zoom {:.4} (dpr {:.4}); page {:.1} CSS px = {:.2} logical of a {height} px widget; \
         band {:.4} CSS px = {band:.3} logical, bottom at {:.2} logical",
        webview.scale_factor(),
        r.zoom,
        r.zoom * f64::from(webview.scale_factor()),
        r.inner,
        r.inner * r.zoom,
        r.band_css,
        r.bottom_css * r.zoom,
    );
    println!("band_xft_zoom: {line}");
    if (r.zoom - expected_zoom).abs() > 1e-6 {
        return Err(format!(
            "WebKit's zoom is not gtk-xft-dpi/1024/96 = {expected_zoom}: {line}"
        ));
    }
    // `innerHeight` is whole CSS px, so it can be up to one CSS px short of the widget.
    if (r.inner * r.zoom - height).abs() > r.zoom + 0.01 {
        return Err(format!("the zoom does not scale CSS px to logical px: {line}"));
    }
    if (band - f64::from(ROW)).abs() > 0.05 {
        return Err(format!("the band is not one {ROW}px editor row: {line}"));
    }
    Ok(line)
}

fn run() -> Result<(), String> {
    let settings = gtk4::Settings::default().ok_or("no GtkSettings")?;
    // Before the `WebView` exists, as a desktop's value is.
    settings.set_gtk_xft_dpi(147_456);
    let window = gtk4::Window::new();
    window.set_default_size(560, 720);
    let webview = WebView::new();
    window.set_child(Some(&webview));
    window.present();

    let mut row = webkit_zoom::EditorRow::new(settings.gtk_xft_dpi());
    let css_px = row.set_cell_height(ROW);
    webview.load_html(PAGE, None);
    let end = Instant::now() + Duration::from_secs(20);
    while webview.is_loading() || webview.height() == 0 {
        if Instant::now() > end {
            return Err("the page never finished loading".into());
        }
        pump(10);
    }
    pump(200);
    eval(
        &webview,
        &format!(
            "document.documentElement.style.setProperty('--nv-editor-row', {:?}); 0",
            editor_row_var(css_px)
        ),
    )?;

    // The owner's 144 dpi; 146 dpi, which WebKit ignores (within 2% of the zoom it applied); then
    // live changes to 96 and 120 dpi.
    step(&webview, &settings, &mut row, 147_456, 1.5)?;
    step(&webview, &settings, &mut row, 146 * 1024, 1.5)?;
    step(&webview, &settings, &mut row, 98_304, 1.0)?;
    step(&webview, &settings, &mut row, 122_880, 1.25)?;
    window.destroy();
    pump(50);
    Ok(())
}

fn main() {
    // Mirrors `#[ignore]` under a plain `main`: `cargo test --workspace` needs no display.
    let args: Vec<String> = std::env::args().collect();
    if !args.iter().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!("band_xft_zoom: ignored (drives real WebKitGTK on a display); run with `-- --ignored`");
        return;
    }
    if std::env::var("GDK_BACKEND").as_deref() != Ok("x11") {
        eprintln!("band_xft_zoom: refusing to open windows without GDK_BACKEND=x11 -- run it under its own X server:");
        eprintln!(
            "    xvfb-run -a -s \"-nolisten tcp -screen 0 1024x900x24\" env GDK_BACKEND=x11 \
             cargo test -p shell --test band_xft_zoom -- --ignored"
        );
        std::process::exit(1);
    }
    if let Err(e) = gtk4::init() {
        eprintln!("band_xft_zoom: GTK could not initialise ({e}); it needs an X server of its own");
        std::process::exit(1);
    }
    match run() {
        Ok(()) => println!("band_xft_zoom: ok (the band is one {ROW}px editor row at 144, 146, 96 and 120 dpi)"),
        Err(e) => {
            eprintln!("band_xft_zoom: FAILED: {e}");
            std::process::exit(1);
        }
    }
}
