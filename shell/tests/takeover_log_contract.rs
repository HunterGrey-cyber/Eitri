//! Hand-back evidence checked without a display, nvim or a socket.

#[path = "support/takeover_log.rs"]
mod takeover_log;

use takeover_log::hand_back_complete;

const RETURNED: &str = "[takeover] the terminal has nvim back";
const LOST: &str = "[takeover] handing nvim back: LinkLost";

#[test]
fn the_lost_origin_link_cannot_answer_its_teardown_rpc() {
    for teardown in ["failed", "did not answer"] {
        let out = format!("[takeover] the glue's teardown {teardown}\n{LOST}\n{RETURNED}");
        assert!(hand_back_complete(&out, true), "{out}");
        assert!(!hand_back_complete(&out, false), "an ordinary detach needs an answer");
        assert!(
            !hand_back_complete(&out.replace(LOST, ""), true),
            "the cause is required"
        );
        assert!(
            !hand_back_complete(&out.replace(RETURNED, ""), true),
            "the room must return"
        );
    }
}

#[test]
fn an_ordinary_hand_back_needs_teardown_and_room_completion_without_a_timeout() {
    let out = format!("[takeover] the glue's teardown answered\n{RETURNED}");
    assert!(hand_back_complete(&out, false));
    assert!(!hand_back_complete(&out, true));
    assert!(!hand_back_complete(RETURNED, false));
    assert!(!hand_back_complete(&out.replace(RETURNED, ""), false));
    for failure in [
        "[takeover] the room did not answer",
        "[takeover] detach completion timed out",
        LOST,
    ] {
        assert!(!hand_back_complete(&format!("{out}\n{failure}"), false), "{failure}");
    }
}
