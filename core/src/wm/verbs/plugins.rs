//! Loading plugin pages into a running window.

use std::collections::BTreeSet;

use super::modules::{open_plugin_module, Cx};
use crate::layout::{ModuleId, OpenRule};
use crate::wm::chrome;
use crate::wm::host::{Batch, HostOp, PluginOp, SurfaceKind};
use crate::wm::module::SurfaceDecl;
use crate::wm::toasts::ToastKind;
use crate::wm::window::{LinkSide, Tiling, WinId, Window, WindowRole};

/// Loads outstanding for one window; a second request for the same name starts no worker.
#[derive(Default)]
pub struct PluginLoads {
    pending: BTreeSet<String>,
}

/// What a compositor-mode window says: there a page is a window of its own, which only init.lua can declare.
pub(super) fn compositor_refusal(name: &str) -> String {
    let line = crate::plugin::line::plugin_line(name).unwrap_or_else(|| "its eitri.plugin.load line".into());
    format!("in compositor mode a page is a window of its own; add {line} to init.lua")
}

/// Requests a load, or shows the page when it is already registered in this window.
pub fn run(w: &mut Window, cx: &Cx, win: WinId, name: &str) -> Batch {
    let mut batch = Batch::default();
    let why = if w.tiling != Tiling::Eitri {
        Some(compositor_refusal(name))
    } else if matches!(
        w.role(),
        WindowRole::Companion | WindowRole::Linked { side: LinkSide::Second }
    ) {
        Some("plugins load in window 1".into())
    } else if cfg!(target_os = "macos") {
        Some("plugin pages are not available on the Mac".into())
    } else if w.plugin_loads.pending.contains(name) {
        Some(format!("{name} is already loading"))
    } else {
        None
    };
    if let Some(why) = why {
        w.refuse(win, why, &mut batch);
        return batch;
    }
    let module = ModuleId::lua(name);
    if matches!(w.lifecycle.kind(&module), Some(SurfaceKind::PluginPage { .. })) {
        match open_plugin_module(w, cx, win, &module, Some(false), false, &mut batch) {
            Ok(()) => {
                let title = chrome::title(w, &module);
                chrome::show_toast(w, format!("{title} is already loaded; shown"), ToastKind::Other);
            }
            Err(why) => w.refuse(win, why.to_string(), &mut batch),
        }
    } else {
        w.plugin_loads.pending.insert(name.into());
        batch.push_effect(HostOp::Plugin(PluginOp::Load { win, name: name.into() }));
    }
    batch
}

/// Registers the worker's result before opening it, so the layout change builds its deferred page.
pub fn loaded(w: &mut Window, cx: &Cx, win: WinId, module: ModuleId, title: String, shown_while: bool) -> Batch {
    let mut batch = Batch::default();
    let Some(name) = module.as_str().strip_prefix("lua:") else {
        return batch;
    };
    w.plugin_loads.pending.remove(name);
    let decl = SurfaceDecl {
        module: module.clone(),
        kind: SurfaceKind::PluginPage { plugin: name.into() },
        deferred: true,
    };
    w.lifecycle.declare(&decl, true);
    w.lifecycle.mark_session_only(&module, title.clone());
    chrome::plugin_title(w, &module, &title);
    batch.push_layout(HostOp::CreateSurface {
        win,
        module: module.clone(),
        kind: decl.kind,
    });
    if !shown_while {
        if let Err(why) = open_plugin_module(w, cx, win, &module, Some(false), false, &mut batch) {
            w.refuse(win, why.to_string(), &mut batch);
            return batch;
        }
    }
    let text = if shown_while {
        format!("{title} loaded; it shows while an agent waits")
    } else {
        format!("{title} loaded for this run")
    };
    chrome::show_toast(w, text, ToastKind::Other);
    batch
}

/// An unsuccessful worker releases the name for retry and leaves the layout alone.
pub fn failed(w: &mut Window, name: &str, why: String) -> Batch {
    w.plugin_loads.pending.remove(name);
    chrome::show_toast(w, why, ToastKind::Other);
    Batch::default()
}

/// Keeps each runtime page's own rule beside the start-hidden declarations the module verbs already read.
pub fn add_rule(setup: &mut super::modules::ModuleSetup, module: &ModuleId, own_rule: Option<OpenRule>) {
    setup.start_hidden.retain(|(id, _)| id != module);
    setup.start_hidden.push((module.clone(), own_rule));
}

#[cfg(test)]
mod tests;
