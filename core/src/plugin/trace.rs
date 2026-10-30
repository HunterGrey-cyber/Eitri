//! Test-only log lines a GUI pass reads: off unless a `--features probe` build turns them on from its
//! environment. Product code calls [`fn@line`] at the points a pass checks; in every other build the call prints
//! nothing.

use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Turns the trace lines on or off for the whole process.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// `[plugin <id>] trace: <event> <detail>`, or without the detail when it is empty.
pub fn format_line(id: &str, event: &str, detail: &str) -> String {
    if detail.is_empty() {
        format!("[plugin {id}] trace: {event}")
    } else {
        format!("[plugin {id}] trace: {event} {detail}")
    }
}

/// Prints one trace line on stdout when the trace is on.
pub fn line(id: &str, event: &str, detail: &str) {
    #[cfg(test)]
    CAPTURED.with(|captured| {
        if let Some(lines) = captured.borrow_mut().as_mut() {
            lines.push(format_line(id, event, detail));
        }
    });
    if enabled() {
        println!("{}", format_line(id, event, detail));
    }
}

#[cfg(test)]
thread_local! {
    static CAPTURED: std::cell::RefCell<Option<Vec<String>>> = const { std::cell::RefCell::new(None) };
}

/// Runs `f` and returns the trace lines this thread asked for meanwhile, whether or not the trace is on: a test
/// reads its own lines without turning the process-wide switch on under the other tests.
#[cfg(test)]
pub fn capture(f: impl FnOnce()) -> Vec<String> {
    CAPTURED.with(|captured| *captured.borrow_mut() = Some(Vec::new()));
    f();
    CAPTURED.with(|captured| captured.borrow_mut().take().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trace_line_names_the_plugin_the_event_and_its_detail() {
        assert_eq!(
            format_line("markdown", "follow", "on"),
            "[plugin markdown] trace: follow on"
        );
        assert_eq!(
            format_line("demo", "pull", "buf=3 tick=41 bytes=120"),
            "[plugin demo] trace: pull buf=3 tick=41 bytes=120"
        );
        assert_eq!(format_line("demo", "page built", ""), "[plugin demo] trace: page built");
    }

    #[test]
    fn the_switch_is_off_until_turned_on() {
        // Other tests never turn it on, so the starting value is the default.
        assert!(!enabled());
        set_enabled(true);
        assert!(enabled());
        set_enabled(false);
        assert!(!enabled());
    }
}
