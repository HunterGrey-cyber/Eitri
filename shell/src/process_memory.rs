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

    /// The case the trim exists for: memory freed underneath a live block. Measurement runs
    /// in a child with one arena and automatic trimming disabled, so its baseline stays resident.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn the_trim_returns_pages_freed_below_a_live_block() {
        use std::io::{BufRead, Write};
        use std::process::{Command, Stdio};
        const CHILD: &str = "EITRI_TRIM_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let idle = rss_anon_kib();
            let blocks: Vec<Box<[u8; 4096]>> = (0..16 * 1024).map(|i| Box::new([i as u8; 4096])).collect();
            let keep = std::hint::black_box(Box::new([1u8; 4096]));
            drop(blocks);
            let before = rss_anon_kib();
            assert!(
                before >= idle + 60 * 1024,
                "the freed allocation must still be resident"
            );
            trim_heap();
            println!("trimmed {before}");
            std::io::stdout().flush().unwrap();
            let mut ack = String::new();
            std::io::stdin().read_line(&mut ack).unwrap();
            assert_eq!(ack, "measure\n");
            let after = rss_anon_kib();
            assert_eq!(keep[0], 1);
            println!("rss {before} {after}");
            assert!(
                before.saturating_sub(after) >= 32 * 1024,
                "RssAnon {before} kB before, {after} kB after"
            );
            return;
        }
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "process_memory::tests::the_trim_returns_pages_freed_below_a_live_block",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("MALLOC_ARENA_MAX", "1")
            .env("MALLOC_TRIM_THRESHOLD_", "-1")
            .env("MALLOC_MMAP_THRESHOLD_", "131072")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (lines_tx, lines) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                if lines_tx.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        // Allocate between the child's trim and final read. In a shared process this hides all
        // the pages returned by the trim; the isolated measurement must still see its own reduction.
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let mut noise = None;
        let mut measured = false;
        let mut output = Vec::new();
        let status = loop {
            while let Ok(line) = lines.try_recv() {
                if line.contains("trimmed ") {
                    noise = Some(std::hint::black_box(vec![0x5au8; 96 * 1024 * 1024]));
                    child.stdin.take().unwrap().write_all(b"measure\n").unwrap();
                }
                measured |= line.starts_with("rss ");
                output.push(line);
            }
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("the heap measurement child hung: {output:?}");
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        reader.join().unwrap();
        for line in lines.try_iter() {
            measured |= line.starts_with("rss ");
            output.push(line);
        }
        let mut stderr = String::new();
        std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).unwrap();
        assert!(
            status.success() && measured,
            "heap measurement {status}: {output:?} {stderr}"
        );
        assert_eq!(noise.unwrap()[0], 0x5a);
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
