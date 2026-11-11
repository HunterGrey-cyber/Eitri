//! Completion evidence for a takeover's ordinary and lost-link hand-back.

pub fn hand_back_complete(out: &str, link_loss: bool) -> bool {
    // Losing the origin link also closes the channel that would answer the teardown RPC.
    let teardown = if link_loss {
        out.contains("[takeover] the glue's teardown failed")
            || out.contains("[takeover] the glue's teardown did not answer")
    } else {
        out.contains("[takeover] the glue's teardown answered")
    };
    teardown
        && out.contains("[takeover] the terminal has nvim back")
        && (link_loss == out.contains("[takeover] handing nvim back: LinkLost"))
        && !out.contains("[takeover] the room did not answer")
        && !out.contains("[takeover] detach completion timed out")
}
