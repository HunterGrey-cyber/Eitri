//! The GTK half of the window's clipboard for nvim: [`eitri_core::host_clipboard`]'s server thread
//! hands each request here, and the copy or the read runs on the GTK thread through the display's
//! `gdk::Clipboard`, the same clipboard every other GTK program and the bottom terminal use, on
//! Wayland and X11 alike.
//!
//! Nothing here blocks the GTK thread, and nothing here runs off it. A request reaches it through a
//! channel whose receiving end is a future on the GTK main loop, so a request runs only while that
//! loop does: one that arrives after the loop has ended is never taken, and its shim is answered
//! by the server's own bound. A copy is `set_content` with the text's bytes, which returns at once (GTK serves the text to
//! readers later from its own loop); a paste is an asynchronous read of at most
//! [`MAX_TEXT_BYTES`] bytes under [`ANSWER_WITHIN`], dropped (which cancels it) when the time
//! runs out. A clipboard owner that is slow or gone therefore costs nvim at most that bound, and
//! the GTK thread nothing.
//!
//! `+` and `*` both reach the clipboard (`CLIPBOARD`), never the primary selection: nvim's `tmux`
//! provider runs one command for both registers, so the shim cannot tell them apart.
//!
//! Two gates come first. The core server serves only the window's nvim and what descends from it
//! ([`host_clipboard::Admission`]); and a request is served only while this window is the active one
//! (it has the keyboard focus), which is the rule Wayland compositors such as GNOME's hold every
//! client to, so no process reaches the clipboard through the window while the person is elsewhere.
//!
//! The text is never logged, put in a toast or kept: errors name a size or GTK's reason only.

use std::os::unix::net::UnixListener;
use std::path::Path;

use eitri_core::host_clipboard::{
    self, Admission, Answer, ClipboardServer, Reply, Request, ANSWER_WITHIN, MAX_TEXT_BYTES,
};
use gtk4::gdk;
use gtk4::gio;
use gtk4::glib;
use gtk4::prelude::*;

/// The MIME types a paste asks for, in order: GTK maps X11's `UTF8_STRING` and Wayland's
/// `text/plain;charset=utf-8` to the first, and the second covers an owner that offers only plain text.
const TEXT_TYPES: &[&str] = &["text/plain;charset=utf-8", "text/plain"];

/// How much one read of the clipboard's stream asks for.
const CHUNK: usize = 64 * 1024;

/// The reason a request is refused while the window is not the active one.
pub(crate) const NOT_ACTIVE: &str =
    "the Eitri window is not active; its clipboard is served only while it has the focus";

/// Serves `listener` (the channel's `c.sock`) for `window`, to the nvim started with `switch_socket`,
/// or logs why not. Called on the GTK thread.
pub(crate) fn start(
    listener: UnixListener,
    switch_socket: &Path,
    attached: Option<host_clipboard::AttachedNvim>,
    followed: Option<host_clipboard::FollowedNvim>,
    window: &impl IsA<gtk4::Window>,
) -> Option<ClipboardServer> {
    let (requests, mut taken) = futures_channel::mpsc::unbounded::<(Request, Reply)>();
    let window = window.as_ref().downgrade();
    // Ends when the server thread drops its sender, i.e. when the server has stopped.
    glib::spawn_future_local(async move {
        while let Ok((request, reply)) = taken.recv().await {
            let active = window.upgrade().is_some_and(|window| window.is_active());
            answer(request, reply, active);
        }
    });
    // A person's own nvim the window holds is no child of the window: it is let in by its process, which only the
    // takeover learned.
    let mut admission = Admission::for_window(switch_socket);
    if let Some(nvim) = attached {
        admission = admission.with_attached(nvim);
    }
    // The server a followed `:restart` started is no child of the window either once the nvim that started it has
    // exited: it is let in by the process the editor learned from it.
    if let Some(followed) = followed {
        admission = admission.with_followed(followed);
    }
    let served = host_clipboard::serve(listener, admission, move |request, reply| {
        // A send fails only once the loop's end has been dropped; the reply then answers `Failed`.
        let _ = requests.unbounded_send((request, reply));
    });
    match served {
        Ok(server) => Some(server),
        Err(e) => {
            eprintln!("[clipboard] could not start the clipboard thread: {e} -- nvim's clipboard register reaches no clipboard");
            None
        }
    }
}

/// The refusal a request gets before the clipboard is touched, if any.
fn refusal(active: bool) -> Option<Answer> {
    (!active).then(|| Answer::Failed(NOT_ACTIVE.to_string()))
}

/// On the GTK thread: does what `request` asks with the default display's clipboard, if the window
/// is `active`. A paste's read runs as a future of its own, so a second request is taken while it
/// waits.
fn answer(request: Request, reply: Reply, active: bool) {
    if let Some(refused) = refusal(active) {
        reply.send(refused);
        return;
    }
    let Some(display) = gdk::Display::default() else {
        reply.send(Answer::Failed("the window has no display".to_string()));
        return;
    };
    let clipboard = display.clipboard();
    match request {
        Request::Copy(bytes) => {
            let answer = match clipboard.set_content(Some(&text_provider(copy_text(bytes)))) {
                Ok(()) => Answer::Done,
                Err(e) => Answer::Failed(format!("could not set the clipboard: {e}")),
            };
            reply.send(answer);
        }
        Request::Paste => {
            glib::spawn_future_local(async move {
                let read = glib::future_with_timeout(ANSWER_WITHIN, read_text(clipboard));
                reply.send(match read.await {
                    Ok(answer) => answer,
                    // The server has given up by now too; this is for the record only.
                    Err(_) => Answer::Failed(format!(
                        "the clipboard did not answer within {} s",
                        ANSWER_WITHIN.as_secs()
                    )),
                });
            });
        }
    }
}

/// The text a copy puts on the clipboard. nvim hands over its register's bytes, which are UTF-8
/// unless the buffer is in another encoding; a byte that is not becomes U+FFFD, since the text is
/// offered as UTF-8.
fn copy_text(bytes: Vec<u8>) -> String {
    match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    }
}

/// The clipboard's content for a copy: the text's bytes as offered under both text types, never as
/// a C string, so a NUL nvim hands over (a `^@` in the buffer) stays in the text instead of ending
/// it there.
fn text_provider(text: String) -> gdk::ContentProvider {
    let bytes = glib::Bytes::from_owned(text.into_bytes());
    let providers: Vec<gdk::ContentProvider> = TEXT_TYPES
        .iter()
        .map(|mime| gdk::ContentProvider::for_bytes(mime, &bytes))
        .collect();
    gdk::ContentProvider::new_union(&providers)
}

/// Reads the clipboard's text, at most [`MAX_TEXT_BYTES`].
async fn read_text(clipboard: gdk::Clipboard) -> Answer {
    match clipboard.read_future(TEXT_TYPES, glib::Priority::DEFAULT).await {
        Ok((stream, _mime)) => read_capped(&stream, MAX_TEXT_BYTES).await,
        Err(e) => Answer::Failed(no_text_reason(&e)),
    }
}

/// Reads `stream` to its end, at most `cap` bytes: a chunk that would take the text past it ends the
/// read with a refusal, never a cut text, and the text never holds more than `cap`.
async fn read_capped(stream: &impl IsA<gio::InputStream>, cap: usize) -> Answer {
    let mut text = Vec::new();
    loop {
        let chunk = match stream.read_bytes_future(CHUNK, glib::Priority::DEFAULT).await {
            Ok(chunk) => chunk,
            Err(e) => return Answer::Failed(format!("could not read the clipboard: {}", e.message())),
        };
        if chunk.is_empty() {
            break;
        }
        if text.len() + chunk.len() > cap {
            let _ = stream.close_future(glib::Priority::DEFAULT).await;
            return Answer::Failed(over_limit_paste(cap));
        }
        text.extend_from_slice(&chunk);
    }
    Answer::Text(text)
}

/// GTK's reason for a failed read, which names formats, never content; an empty clipboard and one
/// holding no text (an image) say so in nvim's terms.
fn no_text_reason(e: &glib::Error) -> String {
    if e.matches(gio::IOErrorEnum::NotSupported) {
        "the clipboard holds no text".to_string()
    } else {
        format!("could not read the clipboard: {}", e.message())
    }
}

/// The reason a paste over `cap` is refused. The read stops at the chunk that would pass it, so the
/// clipboard's real size is not known, only that it is over.
fn over_limit_paste(cap: usize) -> String {
    format!(
        "the clipboard holds more than {} of text; nothing was pasted",
        if cap % (1024 * 1024) == 0 {
            format!("{} MiB", cap / (1024 * 1024))
        } else {
            format!("{cap} bytes")
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_stream(bytes: &[u8]) -> gio::MemoryInputStream {
        gio::MemoryInputStream::from_bytes(&glib::Bytes::from(bytes))
    }

    fn read(bytes: &[u8], cap: usize) -> Answer {
        glib::MainContext::new().block_on(read_capped(&memory_stream(bytes), cap))
    }

    #[test]
    fn a_copy_keeps_utf8_and_replaces_only_bytes_that_are_not() {
        assert_eq!(copy_text("a\nb é\n".as_bytes().to_vec()), "a\nb é\n");
        assert_eq!(copy_text(vec![b'a', 0xff, b'\n']), "a\u{fffd}\n");
    }

    /// The read is cut at the cap, not one chunk later: a text of exactly the cap comes whole, one
    /// byte more is refused, NULs and invalid UTF-8 pass as the bytes they are.
    #[test]
    fn a_paste_is_read_whole_up_to_the_cap_and_refused_past_it() {
        let cap = 3 * CHUNK + 7;
        let mut text = vec![b'x'; cap];
        text[5] = 0;
        text[6] = 0xff;
        assert_eq!(read(&text, cap), Answer::Text(text.clone()));
        text.push(b'y');
        assert_eq!(
            read(&text, cap),
            Answer::Failed(format!(
                "the clipboard holds more than {cap} bytes of text; nothing was pasted"
            ))
        );
        assert_eq!(read(b"", cap), Answer::Text(Vec::new()));
    }

    #[test]
    fn the_production_cap_is_named_in_mib() {
        assert_eq!(
            over_limit_paste(MAX_TEXT_BYTES),
            "the clipboard holds more than 16 MiB of text; nothing was pasted"
        );
    }

    /// A copy is offered under both text types, as bytes: a NUL stays in the text.
    #[test]
    fn a_copy_is_offered_whole_under_both_text_types() {
        let provider = text_provider("a\0b\n".to_string());
        let formats = provider.formats();
        for mime in TEXT_TYPES {
            assert!(formats.contain_mime_type(mime), "{mime}");
            let out = gio::MemoryOutputStream::new_resizable();
            glib::MainContext::new()
                .block_on(provider.write_mime_type_future(mime, &out, glib::Priority::DEFAULT))
                .expect("written");
            out.close(gio::Cancellable::NONE).expect("closed");
            assert_eq!(out.steal_as_bytes().as_ref(), b"a\0b\n", "{mime}");
        }
    }

    #[test]
    fn nothing_is_served_while_the_window_is_not_active() {
        assert_eq!(refusal(false), Some(Answer::Failed(NOT_ACTIVE.to_string())));
        assert_eq!(refusal(true), None);
    }

    /// Requests reach GTK through a channel read on the GTK loop and a copy is never a C string:
    /// `MainContext::invoke` runs its closure on the calling thread once the loop has ended, and
    /// `set_text` ends a text at its first NUL.
    #[test]
    fn requests_never_go_through_invoke_and_a_copy_never_through_set_text() {
        let source = eitri_core::source_scan::read_required("shell/src/host_clipboard.rs");
        let code = eitri_core::source_scan::code_only(&source.text);
        let code = &code[..code.find("mod tests").expect("the test module")];
        for banned in [".invoke(", "invoke_with_priority(", ".set_text(", "set_text("] {
            assert!(!code.contains(banned), "{banned} in shell/src/host_clipboard.rs");
        }
        assert!(code.contains("spawn_future_local"), "the loop's end of the channel");
        assert!(code.contains("set_content("), "the copy");
        assert!(code.contains("read_capped("), "the paste's cap");
    }
}
