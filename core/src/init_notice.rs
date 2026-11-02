//! What the window says of `init.lua` when it opens: the things it asked for that were not done (every start, for as
//! long as they hold), and the default keys it took over (once, and again only when that set changes).

use std::ffi::OsStr;
use std::path::PathBuf;

use sha2::{Digest, Sha256};

/// The most problems a toast lists; the rest are counted, and all of them are on stderr.
const LISTED: usize = 5;
/// The most characters of one problem a toast shows.
const LINE_CHARS: usize = 220;

/// The toast for `problems` (`Kernel::startup_problems`), or `None` when there are none: the count, what was done
/// about them, and the first few with the file and line each one carries.
pub fn problems_text(problems: &[String]) -> Option<String> {
    if problems.is_empty() {
        return None;
    }
    let count = match problems.len() {
        1 => "1 problem".to_string(),
        n => format!("{n} problems"),
    };
    let mut text = format!("init.lua: {count}; the rest of it is in effect");
    for problem in problems.iter().take(LISTED) {
        text.push('\n');
        text.push_str(&shorten(problem));
    }
    if problems.len() > LISTED {
        text.push_str(&format!(
            "\n… and {} more; start eitri from a terminal to read them all",
            problems.len() - LISTED
        ));
    }
    Some(text)
}

/// The toast for what the launch could not read of the state it saved (the layout file, the tabs file): each file was
/// set aside or left as it was, and the window opened without it. `None` when nothing was wrong. A line may carry the
/// `[tabs]` or `[layout]` tag the log gives it; the toast drops that.
pub fn saved_state_text(problems: &[String]) -> Option<String> {
    if problems.is_empty() {
        return None;
    }
    let mut text = "Eitri could not use some of what it saved last time:".to_string();
    for problem in problems.iter().take(LISTED) {
        let line = problem
            .strip_prefix('[')
            .and_then(|rest| rest.split_once("] "))
            .map_or(problem.as_str(), |(_, rest)| rest);
        text.push('\n');
        text.push_str(&shorten(line));
    }
    if problems.len() > LISTED {
        text.push_str(&format!(
            "\n… and {} more; start eitri from a terminal to read them all",
            problems.len() - LISTED
        ));
    }
    Some(text)
}

/// `line` cut to [`LINE_CHARS`] from the middle: the place it names stays at the front and the words that say what is
/// in force stay at the end, which is where they are.
fn shorten(line: &str) -> String {
    let chars: Vec<char> = line.chars().collect();
    if chars.len() <= LINE_CHARS {
        return line.to_string();
    }
    let head = LINE_CHARS / 3;
    let tail = LINE_CHARS - head - 1;
    let start: String = chars[..head].iter().collect();
    let end: String = chars[chars.len() - tail..].iter().collect();
    format!("{start}…{end}")
}

/// The toast for the default keys `init.lua` took over (`Keymap::user_overrides`), or `None` when it took none.
pub fn overrides_text(overrides: &[String]) -> Option<String> {
    if overrides.is_empty() {
        return None;
    }
    Some(format!(
        "init.lua took over {}: {}. Delete those lines to have the defaults back.",
        if overrides.len() == 1 {
            "a default key".to_string()
        } else {
            format!("{} default keys", overrides.len())
        },
        overrides.join(", ")
    ))
}

/// Where the last announced set of takeovers is kept, by the rule of Eitri's other state files.
pub fn overrides_state_file(xdg_state_home: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    crate::state_paths::state_subdir(xdg_state_home, home, "keymap-overrides.json")
}

/// SHA-256, hex, over the takeovers in the order of the calls.
pub fn overrides_fingerprint(overrides: &[String]) -> String {
    Sha256::digest(overrides.join("\n").as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_problem_says_nothing() {
        assert_eq!(problems_text(&[]), None);
        assert_eq!(overrides_text(&[]), None);
    }

    /// A tabs or layout file that was cut in half is said in the window, with the reason the log has, and not only logged.
    #[test]
    fn an_unusable_saved_file_is_named_with_its_reason_and_the_log_tag_is_dropped() {
        assert_eq!(saved_state_text(&[]), None);
        let text = saved_state_text(&[
            "[tabs] the saved tabs are unusable (/s/tabs/a.json: not a saved-tabs file (EOF while parsing)); starting without them"
                .to_string(),
            "the saved layout is unusable (/s/layout/a.json: not a layout file); the default layout is open and the file is kept"
                .to_string(),
        ])
        .unwrap();
        assert_eq!(
            text,
            "Eitri could not use some of what it saved last time:\n\
             the saved tabs are unusable (/s/tabs/a.json: not a saved-tabs file (EOF while parsing)); starting without them\n\
             the saved layout is unusable (/s/layout/a.json: not a layout file); the default layout is open and the file is kept"
        );
        let many: Vec<String> = (1..=7).map(|n| format!("[tabs] problem {n}")).collect();
        let text = saved_state_text(&many).unwrap();
        assert!(text.contains("problem 5") && !text.contains("problem 6"), "{text}");
        assert!(
            text.ends_with("… and 2 more; start eitri from a terminal to read them all"),
            "{text}"
        );
    }

    #[test]
    fn a_few_problems_are_listed_and_the_rest_counted() {
        let lines: Vec<String> = (1..=7).map(|n| format!("/c/init.lua:{n}: bad")).collect();
        let text = problems_text(&lines).unwrap();
        assert!(
            text.starts_with("init.lua: 7 problems; the rest of it is in effect\n"),
            "{text}"
        );
        assert!(
            text.contains("/c/init.lua:1: bad") && text.contains("/c/init.lua:5: bad"),
            "{text}"
        );
        assert!(!text.contains("/c/init.lua:6: bad"), "{text}");
        assert!(
            text.ends_with("… and 2 more; start eitri from a terminal to read them all"),
            "{text}"
        );
        let one = problems_text(&lines[..1]).unwrap();
        assert!(one.starts_with("init.lua: 1 problem;"), "{one}");
    }

    #[test]
    fn a_long_problem_is_cut_in_the_middle_and_keeps_its_last_words() {
        let line = format!("/c/init.lua:3: {}; the default is used", "x".repeat(500));
        let text = problems_text(&[line]).unwrap();
        let shown = text.lines().nth(1).unwrap();
        assert_eq!(shown.chars().count(), LINE_CHARS, "{shown}");
        assert!(
            shown.starts_with("/c/init.lua:3: ") && shown.ends_with("the default is used"),
            "{shown}"
        );
    }

    #[test]
    fn the_takeovers_are_named_with_what_they_replaced() {
        let text = overrides_text(&["prefix d (was detach)".into(), "panel <leader>e (was follow)".into()]).unwrap();
        assert_eq!(
            text,
            "init.lua took over 2 default keys: prefix d (was detach), panel <leader>e (was follow). \
             Delete those lines to have the defaults back."
        );
        assert!(overrides_text(&["prefix d (was detach)".into()])
            .unwrap()
            .contains("a default key:"));
    }

    #[test]
    fn the_fingerprint_follows_the_set() {
        let a = overrides_fingerprint(&["prefix d (was detach)".into()]);
        assert_eq!(a, overrides_fingerprint(&["prefix d (was detach)".into()]));
        assert_ne!(
            a,
            overrides_fingerprint(&["prefix d (was detach)".into(), "prefix l (was tab.last)".into()])
        );
    }
}
