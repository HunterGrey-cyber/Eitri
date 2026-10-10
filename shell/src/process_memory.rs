//! Two things that keep this process's resident memory down without changing what anything in it does:
//! JavaScriptCore without its JIT in the UI process, and one heap trim once startup's one-off work is over.

use std::time::Duration;

/// How long after the window is built the heap is trimmed. Startup's transient allocations -- above
/// all the GPU driver compiling GTK's pipelines and the editor's shaders when its shader cache is
/// cold -- are over by then: on an Intel (i915) laptop the live heap stops growing about 1.7 s after
/// launch even under a heap profiler, while the editor is editable after 0.35-0.7 s. On a slower
/// machine the trim may come before the last of it and return less; pipelines compiled later, the
/// first time some widget is drawn, are not trimmed. Neither does harm.
pub(crate) const STARTUP_SETTLE: Duration = Duration::from_secs(2);

/// Turns JavaScriptCore's JIT off in this process, the UI process, before WebKit first starts it.
///
/// Every script the panel runs executes in WebKit's web process, whose JIT this does not touch
/// (JavaScriptCore's options are per process). Here JavaScriptCore only converts the page's
/// messages to values (`script-message-received`), which its interpreter does as well; with the JIT
/// on it still reserves its executable region and writes its thunks there at start-up, a few MiB
/// resident and the process's only writable-and-executable mapping. Must run before the first
/// `WebView` or `WebContext` exists: JavaScriptCore reads its options once, when it initialises.
/// Returns whether the option was accepted (`false` would mean this WebKit no longer knows it).
pub(crate) fn ui_process_jsc_without_jit() -> bool {
    webkit6::javascriptcore::functions::options_set_boolean(webkit6::javascriptcore::OPTIONS_USE_JIT.as_str(), false)
}

/// Schedules one [`trim_heap`] [`STARTUP_SETTLE`] after now, on a thread of its own so the GTK thread
/// does not run the walk itself. It may still wait once, for at most the 1-2 ms the trim holds the main
/// arena (the GTK thread's) while it hands that arena's pages back.
pub(crate) fn trim_heap_after_startup() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    gtk4::glib::timeout_add_local_once(STARTUP_SETTLE, || {
        let spawned = std::thread::Builder::new().name("heap-trim".into()).spawn(|| {
            trim_heap();
        });
        if let Err(e) = spawned {
            eprintln!("[memory] could not start the heap trim: {e}");
        }
    });
}

/// Returns to the kernel every whole page that malloc holds free, in every arena.
///
/// glibc gives memory back by itself only from the top of a heap; pages freed below a block that is
/// still in use stay resident for the life of the process. Startup frees a lot of that kind: with a
/// cold shader cache the GPU driver's compiles peak at about 30 MiB on an Intel (i915) GPU and
/// leave about 15 MiB of free but resident pages behind them. With a warm cache there is about 1 MiB
/// to return, and the call costs the same 1-2 ms. Nothing that is in use moves or changes.
pub(crate) fn trim_heap() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: `malloc_trim` takes no pointers and only releases pages malloc itself holds free; it
    // locks each arena while it walks it, so other threads may keep allocating meanwhile.
    unsafe {
        libc::malloc_trim(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rss_anon_kib() -> u64 {
        let status = std::fs::read_to_string("/proc/self/status").expect("/proc/self/status");
        let line = status
            .lines()
            .find(|l| l.starts_with("RssAnon:"))
            .expect("RssAnon line");
        line.split_whitespace().nth(1).expect("a number").parse().expect("kB")
    }

    /// The case the trim exists for: memory freed underneath a block that is still in use, which
    /// glibc keeps resident by itself. 64 MiB in 4 KiB blocks (below malloc's mmap threshold, so
    /// they come from an arena), one block kept after them, the rest freed: the trim must return
    /// most of it. Other tests run in this process meanwhile, so the bar is half, not all.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn the_trim_returns_pages_freed_below_a_live_block() {
        let blocks: Vec<Box<[u8; 4096]>> = (0..16 * 1024).map(|i| Box::new([i as u8; 4096])).collect();
        let keep = std::hint::black_box(Box::new([1u8; 4096]));
        drop(blocks);
        let before = rss_anon_kib();
        trim_heap();
        let after = rss_anon_kib();
        assert_eq!(keep[0], 1);
        assert!(
            before.saturating_sub(after) >= 32 * 1024,
            "RssAnon {before} kB before the trim, {after} kB after"
        );
    }

    /// The option name is one this WebKit knows: a misspelt or retired name makes the setter return
    /// false and leaves the JIT on without a word.
    #[test]
    fn javascriptcore_accepts_the_jit_option() {
        assert!(ui_process_jsc_without_jit());
        assert_eq!(
            webkit6::javascriptcore::functions::options_get_boolean(webkit6::javascriptcore::OPTIONS_USE_JIT.as_str()),
            Some(false)
        );
    }
}
