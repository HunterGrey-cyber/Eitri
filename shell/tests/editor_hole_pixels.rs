//! The shell's transparent editor rectangle and themed surroundings, rendered on a private Xvfb.
//! Run with `cargo test -p shell --test editor_hole_pixels -- --ignored`.

use gtk4::prelude::*;
use gtk4::{gdk, graphene};

#[path = "support/own_x_server.rs"]
mod own_x_server;

#[allow(dead_code, unused_imports)]
#[path = "../src/editor_hole.rs"]
mod editor_hole;
#[allow(dead_code, unused_imports)]
#[path = "../src/gtk_host/mod.rs"]
mod gtk_host;
// `gtk_host` tells a waiting launcher the window is up; nothing here has one, so the call does nothing.
#[allow(dead_code)]
#[path = "../src/launch_env.rs"]
mod launch_env;
#[allow(dead_code, unused_imports)]
#[path = "../src/module_grid.rs"]
mod module_grid;
#[allow(dead_code, unused_imports)]
#[path = "../src/web_host.rs"]
mod web_host;
#[path = "../src/theme"]
mod theme {
    #[allow(dead_code, unused_imports)]
    pub(crate) mod gtk_css;
    #[allow(dead_code, unused_imports)]
    pub(crate) mod restyle;
}

const BACKGROUND: [u8; 4] = [0x1e, 0x20, 0x30, 255];
const NEW_BACKGROUND: [u8; 4] = [0x50, 0x60, 0x70, 255];
const OUTSIDE: [(usize, usize); 16] = [
    (10, 10),
    (200, 10),
    (600, 10),
    (790, 10),
    (10, 60),
    (200, 60),
    (600, 60),
    (790, 60),
    (10, 250),
    (60, 250),
    (600, 250),
    (790, 250),
    (10, 590),
    (200, 590),
    (600, 590),
    (790, 590),
];

#[allow(dead_code)]
#[path = "support/frame_turns.rs"]
mod frame_turns;

fn pump() {
    frame_turns::settle();
}

fn rgba(bytes: [u8; 4]) -> gdk::RGBA {
    gdk::RGBA::new(
        f32::from(bytes[0]) / 255.0,
        f32::from(bytes[1]) / 255.0,
        f32::from(bytes[2]) / 255.0,
        f32::from(bytes[3]) / 255.0,
    )
}

struct Scene {
    window: gtk4::Window,
    backdrop: editor_hole::HoleBackdrop,
    editor: gtk4::Box,
    float: gtk4::Box,
    provider: gtk4::CssProvider,
}

impl Scene {
    fn new() -> Self {
        let mut tokens = eitri_core::theme::ThemeTokens::fallback();
        tokens.bg = eitri_core::theme::color::Rgb {
            r: 0x1e,
            g: 0x20,
            b: 0x30,
        };
        let provider = gtk4::CssProvider::new();
        provider.load_from_string(&format!(
            "{}\n.hole-test-float {{ background-color: #b04060; }}",
            theme::gtk_css::gtk_css(&tokens)
        ));
        gtk4::style_context_add_provider_for_display(
            &gdk::Display::default().expect("private display"),
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_USER,
        );
        let window = gtk4::Window::new();
        window.set_decorated(false);
        window.set_default_size(800, 600);
        window.add_css_class("editor-hole");
        let backdrop = editor_hole::HoleBackdrop::new();
        backdrop.set_background(rgba(BACKGROUND));
        let root = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        root.add_css_class("shell-root");
        let bar = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        bar.set_size_request(-1, 40);
        root.append(&bar);
        let grid = gtk4::Fixed::new();
        grid.add_css_class("content-area");
        grid.set_vexpand(true);
        root.append(&grid);
        let editor = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        editor.set_size_request(400, 300);
        grid.put(&editor, 100.0, 80.0);
        let float = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
        float.set_size_request(100, 100);
        float.add_css_class("hole-test-float");
        grid.put(&float, 250.0, 180.0);
        float.set_visible(false);
        let overlay = gtk4::Overlay::new();
        overlay.set_child(Some(&backdrop));
        overlay.add_overlay(&root);
        overlay.set_measure_overlay(&root, true);
        window.set_child(Some(&overlay));
        backdrop.set_hole_widget(Some(editor.upcast_ref()));
        backdrop.set_open(true);
        window.present();
        pump();
        let bounds = editor.compute_bounds(&backdrop).expect("editor bounds");
        assert_eq!(bounds, graphene::Rect::new(100.0, 120.0, 400.0, 300.0));
        assert_eq!((window.width(), window.height()), (800, 600));
        Self {
            window,
            backdrop,
            editor,
            float,
            provider,
        }
    }

    fn pixels(&self) -> Vec<u8> {
        self.window.queue_draw();
        pump();
        let snapshot = gtk4::Snapshot::new();
        let paintable = gtk4::WidgetPaintable::new(Some(&self.window));
        paintable.snapshot(&snapshot, 800.0, 600.0);
        let node = snapshot.to_node().expect("window render node");
        let renderer = self.window.renderer().expect("window renderer");
        let texture = renderer.render_texture(&node, Some(&graphene::Rect::new(0.0, 0.0, 800.0, 600.0)));
        assert_eq!((texture.width(), texture.height()), (800, 600));
        let mut pixels = vec![0; 800 * 600 * 4];
        texture.download(&mut pixels, 800 * 4);
        pixels
    }
}

impl Drop for Scene {
    fn drop(&mut self) {
        self.window.destroy();
        gtk4::style_context_remove_provider_for_display(
            &gdk::Display::default().expect("private display"),
            &self.provider,
        );
        pump();
    }
}

fn pixel(pixels: &[u8], x: usize, y: usize) -> [u8; 4] {
    let start = (y * 800 + x) * 4;
    let argb = u32::from_ne_bytes(pixels[start..start + 4].try_into().expect("ARGB32 pixel"));
    [(argb >> 16) as u8, (argb >> 8) as u8, argb as u8, (argb >> 24) as u8]
}

fn check_outside(pixels: &[u8], color: [u8; 4]) {
    for (x, y) in OUTSIDE {
        assert_eq!(pixel(pixels, x, y), color, "outside pixel at ({x}, {y})");
    }
}

fn the_hole_is_transparent_and_the_rest_is_the_background(scene: &Scene) {
    let pixels = scene.pixels();
    for y in (120..420).step_by(13) {
        for x in (100..500).step_by(13) {
            assert_eq!(pixel(&pixels, x, y)[3], 0, "editor pixel at ({x}, {y})");
        }
    }
    check_outside(&pixels, BACKGROUND);
}

fn a_float_over_the_hole_is_opaque(scene: &Scene) {
    scene.float.set_visible(true);
    let pixels = scene.pixels();
    for y in (220..320).step_by(11) {
        for x in (250..350).step_by(11) {
            assert_eq!(pixel(&pixels, x, y), [0xb0, 0x40, 0x60, 255]);
        }
    }
    assert_eq!(pixel(&pixels, 110, 130)[3], 0);
    check_outside(&pixels, BACKGROUND);
    scene.float.set_visible(false);
}

fn a_closed_hole_paints_the_background(scene: &Scene) {
    scene.backdrop.set_open(false);
    let pixels = scene.pixels();
    for (x, y) in [(100, 120), (300, 270), (499, 419)] {
        assert_eq!(pixel(&pixels, x, y), BACKGROUND);
    }
    check_outside(&pixels, BACKGROUND);
    scene.backdrop.set_open(true);
}

fn the_backdrop_paints_the_new_background_after_set_background(scene: &Scene) {
    scene.backdrop.set_background(rgba(NEW_BACKGROUND));
    let pixels = scene.pixels();
    check_outside(&pixels, NEW_BACKGROUND);
    assert_eq!(pixel(&pixels, 300, 270)[3], 0);
    scene.backdrop.set_background(rgba(BACKGROUND));
}

fn an_unmapped_editor_leaves_no_hole(scene: &Scene) {
    scene.editor.set_visible(false);
    let pixels = scene.pixels();
    assert_eq!(pixel(&pixels, 300, 270), BACKGROUND);
    check_outside(&pixels, BACKGROUND);
    scene.editor.set_visible(true);
}

fn main() {
    if !std::env::args().any(|a| a == "--ignored" || a == "--include-ignored") {
        println!("editor_hole_pixels: ignored (needs its own Xvfb); run with `-- --ignored`");
        return;
    }
    let _server = own_x_server::init_gtk("editor_hole_pixels", "1024x768x24");
    let scene = Scene::new();
    for check in [
        the_hole_is_transparent_and_the_rest_is_the_background,
        a_float_over_the_hole_is_opaque,
        a_closed_hole_paints_the_background,
        the_backdrop_paints_the_new_background_after_set_background,
        an_unmapped_editor_leaves_no_hole,
    ] {
        check(&scene);
    }
    println!("editor_hole_pixels: all five pixel checks passed");
}
