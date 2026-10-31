//! Mapping a committed page waits for its restored document, with a bounded wait for pages that never answer.
use super::*;

/// The page said `page.ready`: one line says how long its arrival waited, or that none was waiting (a page that
/// answers too late, or one with no move behind it), so a run's log tells the two apart.
pub fn page_ready(w: &mut Window, win: WinId, module: &ModuleId, batch: &mut Batch) {
    let since = w
        .peers()
        .find_map(|(_, p)| p.pages_waiting.get(module))
        .map(|waiting| waiting.since);
    match since {
        Some(since) => {
            let ms = w.now().saturating_duration_since(since).as_millis();
            w.say(format!("[move] {module}: page ready after {ms} ms"));
        }
        None if w.has_peer() => w.say(format!("[move] {module}: page.ready with no arrival waiting")),
        None => {}
    }
    show_waiting(w, win, module, batch);
}

/// Maps a page whose arrival waits, now: its document is ready, its wait ran out, or the link ended.
pub fn show_waiting(w: &mut Window, win: WinId, module: &ModuleId, batch: &mut Batch) {
    let waiting = w.peers.values_mut().find_map(|p| p.pages_waiting.remove(module));
    if let Some(waiting) = waiting {
        w.cancel_timer(waiting.timer);
        if waiting.show {
            if let Ok(mut workspaces) = w.workspaces.try_borrow_mut() {
                if workspaces.current() == waiting.workspace {
                    if let Ok(mut layout) = w.layout.try_borrow_mut() {
                        let _ = layout.show(module);
                    }
                } else if let Some(layout) = workspaces.stored_mut_of(&waiting.workspace) {
                    let _ = layout.show(module);
                } else if let Ok(mut layout) = w.layout.try_borrow_mut() {
                    // A workspace that left the window cannot keep a committed page hidden forever.
                    let _ = layout.show(module);
                }
            }
        }
        w.lifecycle.ready_page(module);
        w.changed_by_controller(win, batch);
        focus::modules_changed(w, win, batch);
    }
}

pub fn page_timer(w: &mut Window, win: WinId, token: TimerToken, batch: &mut Batch) -> bool {
    let module = w.peers().find_map(|(_, p)| {
        p.pages_waiting
            .iter()
            .find(|(_, waiting)| waiting.timer == token)
            .map(|(m, _)| m.clone())
    });
    let Some(module) = module else { return false };
    w.say(format!(
        "[plugin] {module}: page.ready did not arrive within one second; showing the page"
    ));
    show_waiting(w, win, &module, batch);
    true
}

/// A page's refusal can end only the freeze of that page and move, never a different kind's transfer.
pub fn page_freeze_refused(w: &mut Window, win: WinId, module: &ModuleId, mv: MoveId, why: String, batch: &mut Batch) {
    let waiting = peer_with(w, mv, |entry| {
        entry.kind == MoveKind::PluginPage
            && entry.module == *module
            && entry.role == Role::Source
            && entry.stage == Stage::Freezing
    })
    .is_some();
    if waiting {
        let of = FreezeOf::Move {
            module: module.clone(),
            mv,
        };
        freeze_refused(w, win, of, why, batch);
    }
}
