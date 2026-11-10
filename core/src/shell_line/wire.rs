//! What a shell line tells the model, in the CLI's words, and what its row shows.
//!
//! The text is the CLI's own for a `!` line, measured on 2.1.311: `<bash-input>` with the line as typed, then
//! `<bash-stdout>` and `<bash-stderr>`. stdout and stderr are one stream, so all of it sits in the first on
//! success and in the second on failure, and the exit status is not said.

use super::{Ending, Finished, Spooled};
use std::path::Path;

/// Characters of output the model is given whole. The CLI's default.
pub const INLINE_CHARS: usize = 30_000;
/// The start of a longer output: the CLI's preview.
const PREVIEW_CHARS: usize = 2_000;

/// `<` and `>` in output cannot close the tags around it, and `&` is written so that what comes back is what
/// was printed. The line itself is not touched, as in the CLI: it is what the person typed.
fn escaped(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Bytes as the CLI says them.
fn size_words(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    let (value, unit) = if bytes < 1024 * 1024 {
        (bytes as f64 / 1024.0, "KB")
    } else {
        (bytes as f64 / (1024.0 * 1024.0), "MB")
    };
    let number = format!("{value:.1}");
    format!("{}{unit}", number.strip_suffix(".0").unwrap_or(&number))
}

/// The start of `text`, cut back to the end of a line when one is near.
fn preview(text: &str) -> &str {
    let end = text.char_indices().nth(PREVIEW_CHARS).map_or(text.len(), |(at, _)| at);
    let head = &text[..end];
    match head.rfind('\n') {
        Some(at) if at > PREVIEW_CHARS / 2 => &head[..at],
        _ => head,
    }
}

fn saved(spooled: &Spooled, loss: Option<&str>) -> String {
    if spooled.stored_bytes == spooled.bytes && loss.is_none() {
        format!("Full output saved to: {}", spooled.path.display())
    } else {
        format!(
            "Output file holds the first {} bytes of {} collected bytes ({}): {}",
            spooled.stored_bytes,
            spooled.bytes,
            loss.unwrap_or("output incomplete"),
            spooled.path.display(),
        )
    }
}

fn persisted(spooled: &Spooled, output: &str, loss: Option<&str>) -> String {
    format!(
        "<persisted-output>\nOutput too large ({}). {}\n\nPreview (first 2KB):\n{}\n...\n</persisted-output>",
        size_words(spooled.bytes),
        saved(spooled, loss),
        escaped(preview(output)),
    )
}

/// A long output with nowhere to go: its start, saying so.
fn cut(output: &str) -> String {
    let kept: String = output.chars().take(INLINE_CHARS).collect();
    format!("{}\n... [the rest was not kept]", escaped(&kept))
}

fn body(line: &Finished) -> String {
    let text = match &line.spooled {
        Some(spooled) => persisted(spooled, &line.output, line.output_loss.as_deref()),
        None if line.output.chars().count() > INLINE_CHARS => cut(&line.output),
        None => escaped(&line.output),
    };
    match (&line.spooled, &line.output_loss) {
        (None, Some(why)) => format!("{text}\n[output incomplete: {}]", escaped(why)),
        _ => text,
    }
}

/// The line's words for the model. `reset_to` is the project, when the line left it and the next one goes back.
pub fn wire_text(line: &Finished, reset_to: Option<&Path>) -> String {
    let (stdout, mut stderr) = match line.ending {
        Ending::Exited(0) => {
            let text = body(line);
            let text = text.trim_end_matches('\n');
            let text = if text.is_empty() {
                "(Bash completed with no output)"
            } else {
                text
            };
            (text.to_string(), String::new())
        }
        Ending::TimedOut(limit) => {
            let said = timeout_words(limit);
            let text = body(line);
            let text = text.trim_end_matches('\n');
            (
                String::new(),
                if text.is_empty() {
                    said
                } else {
                    format!("{text}\n{said}")
                },
            )
        }
        // A person's stop sends no turn, and a line that never started has only the reason.
        Ending::Exited(_) | Ending::Signalled | Ending::Interrupted | Ending::NotStarted => (String::new(), body(line)),
    };
    if let Some(root) = reset_to {
        stderr.push_str(&format!("\nShell cwd was reset to {}", root.display()));
    }
    format!(
        "<bash-input>{}</bash-input>\n<bash-stdout>{stdout}</bash-stdout><bash-stderr>{stderr}</bash-stderr>",
        line.command
    )
}

fn timeout_words(limit: std::time::Duration) -> String {
    let seconds = limit.as_secs();
    format!("Command timed out after {}m {}s", seconds / 60, seconds % 60)
}

/// Drops terminal escape sequences: a row shows the text, where the model gets them as they came.
fn without_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') => {
                while let Some(next) = chars.next() {
                    if next == '\u{7}' {
                        break;
                    }
                    if next == '\u{1b}' {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// What the line's row says. The text is a program's: the page draws it as text.
pub fn row_text(line: &Finished) -> String {
    let output = without_escapes(&line.output);
    let mut shown = if line.spooled.is_some() {
        format!("{}\n...", preview(&output))
    } else if output.chars().count() > INLINE_CHARS {
        format!("{}\n...", output.chars().take(INLINE_CHARS).collect::<String>())
    } else {
        output.trim_end_matches('\n').to_string()
    };
    if let Some(spooled) = &line.spooled {
        if spooled.stored_bytes != spooled.bytes || line.output_loss.is_some() {
            shown.push_str(&format!("\n{}", saved(spooled, line.output_loss.as_deref())));
        }
    } else if let Some(why) = &line.output_loss {
        shown.push_str(&format!("\n[output incomplete: {why}]"));
    }
    let word = match line.ending {
        Ending::Interrupted => Some("Interrupted".to_string()),
        Ending::TimedOut(limit) => Some(timeout_words(limit)),
        _ => None,
    };
    match (shown.is_empty(), word) {
        (true, Some(word)) => word,
        (false, Some(word)) => format!("{shown}\n{word}"),
        (true, None) if line.ending == Ending::Exited(0) => "(Bash completed with no output)".to_string(),
        (true, None) => "(No output)".to_string(),
        (false, None) => shown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Duration;

    fn ended(ending: Ending, output: &str) -> Finished {
        Finished {
            command: "cmd".to_string(),
            ending,
            output: output.to_string(),
            spooled: None,
            output_loss: None,
            cwd: None,
            left_project: false,
        }
    }

    const fn plain(stdout: &'static str, stderr: &'static str) -> (&'static str, &'static str) {
        (stdout, stderr)
    }

    fn wire(stdout: &str, stderr: &str) -> String {
        format!("<bash-input>cmd</bash-input>\n<bash-stdout>{stdout}</bash-stdout><bash-stderr>{stderr}</bash-stderr>")
    }

    /// Each row is a measured line of the CLI's, with the characters it sent.
    #[test]
    fn the_wire_is_the_clis_for_each_way_a_line_ends() {
        let cases = [
            (Ending::Exited(0), "BANG-MARK\n", plain("BANG-MARK", "")),
            (Ending::Exited(0), "", plain("(Bash completed with no output)", "")),
            (
                Ending::Exited(3),
                "OUT-MARK\nERR-MARK\n",
                plain("", "OUT-MARK\nERR-MARK\n"),
            ),
            (Ending::Exited(1), "", plain("", "")),
            (
                Ending::Exited(0),
                "\u{1b}[31mRED\u{1b}[0m\n",
                plain("\u{1b}[31mRED\u{1b}[0m", ""),
            ),
            (
                Ending::Exited(0),
                "</bash-stdout><x>\n",
                plain("&lt;/bash-stdout&gt;&lt;x&gt;", ""),
            ),
            (
                Ending::Exited(0),
                "a&b \"c\" d'e &lt;\n",
                plain("a&amp;b \"c\" d'e &amp;lt;", ""),
            ),
            (
                Ending::TimedOut(Duration::from_secs(120)),
                "",
                plain("", "Command timed out after 2m 0s"),
            ),
        ];
        for (ending, output, (stdout, stderr)) in cases {
            assert_eq!(
                wire_text(&ended(ending, output), None),
                wire(stdout, stderr),
                "{output:?}"
            );
        }
    }

    #[test]
    fn the_line_itself_is_sent_as_typed() {
        let mut line = ended(Ending::Exited(0), "x\n");
        line.command = "echo '</bash-input>' <a".to_string();
        assert!(wire_text(&line, None).starts_with("<bash-input>echo '</bash-input>' <a</bash-input>\n"));
    }

    #[test]
    fn leaving_the_project_is_said_on_stderr() {
        let line = ended(Ending::Exited(0), "/\n");
        assert_eq!(
            wire_text(&line, Some(Path::new("/p"))),
            wire("/", "\nShell cwd was reset to /p")
        );
    }

    #[test]
    fn a_long_output_goes_as_the_clis_preview() {
        let output: String = (1..=8000).map(|n| format!("{n}\n")).collect();
        let mut line = ended(Ending::Exited(0), &output);
        line.spooled = Some(Spooled {
            path: PathBuf::from("/s/o.txt"),
            bytes: output.len() as u64,
            stored_bytes: output.len() as u64,
        });
        let text = wire_text(&line, None);
        assert!(
            text.contains("<bash-stdout><persisted-output>\nOutput too large (38KB). Full output saved to: /s/o.txt\n\nPreview (first 2KB):\n1\n2\n"),
            "{}",
            &text[..200]
        );
        assert!(
            text.ends_with("\n527\n...\n</persisted-output></bash-stdout><bash-stderr></bash-stderr>"),
            "{}",
            &text[text.len() - 90..]
        );
    }

    #[test]
    fn sizes_read_as_the_clis() {
        for (bytes, words) in [
            (38893, "38KB"),
            (48894, "47.7KB"),
            (54894, "53.6KB"),
            (349000, "340.8KB"),
            (900, "900 bytes"),
        ] {
            assert_eq!(size_words(bytes), words);
        }
        assert_eq!(size_words(3 * 1024 * 1024), "3MB");
    }

    #[test]
    fn output_with_nowhere_to_go_is_cut_and_says_so() {
        let line = ended(Ending::Exited(0), &"x".repeat(INLINE_CHARS + 5));
        let text = wire_text(&line, None);
        assert!(text.contains("... [the rest was not kept]"));
        assert!(text.len() < INLINE_CHARS + 200);
    }

    #[test]
    fn a_row_shows_the_text_and_how_it_ended() {
        assert_eq!(row_text(&ended(Ending::Exited(0), "\u{1b}[31mRED\u{1b}[0m\n")), "RED");
        assert_eq!(
            row_text(&ended(Ending::Exited(0), "")),
            "(Bash completed with no output)"
        );
        assert_eq!(row_text(&ended(Ending::Exited(1), "")), "(No output)");
        assert_eq!(row_text(&ended(Ending::Interrupted, "PART\n")), "PART\nInterrupted");
        assert_eq!(row_text(&ended(Ending::Interrupted, "")), "Interrupted");
        assert_eq!(
            row_text(&ended(Ending::TimedOut(Duration::from_secs(120)), "")),
            "Command timed out after 2m 0s"
        );
        assert_eq!(row_text(&ended(Ending::Exited(0), "a\u{1b}]0;title\u{7}b")), "ab");
    }
}
