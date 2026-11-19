use std::fmt::Debug;

use eitri_server_proto::*;
use prost::Message;

fn round_trip<M: Message + Default + PartialEq + Debug>(message: M) {
    let encoded = message.encode_to_vec();
    assert_eq!(encoded.len(), message.encoded_len());
    assert_eq!(M::decode(encoded.as_slice()).unwrap(), message);
}

fn size() -> TermSize {
    TermSize {
        columns: TERM_COLUMNS_MAX,
        rows: TERM_ROWS_MAX,
        cell_width: u32::MAX,
        cell_height: u32::MAX,
    }
}

fn colors() -> TermColors {
    TermColors {
        background: 0xffffff,
        foreground: 0xffffff,
        cursor: Some(0xffffff),
    }
}

fn hello() -> AttachHello {
    AttachHello {
        terminal_id: "t".repeat(TERM_ID_MAX),
        client_id: "c".repeat(TERM_ID_MAX),
        input_sent: u64::MAX,
        size: Some(size()),
        colors: Some(colors()),
    }
}

fn client_frames() -> Vec<TerminalClientFrame> {
    use terminal_client_frame::Frame;
    vec![
        Frame::Hello(hello()),
        Frame::Input(InputBytes {
            offset: u64::MAX,
            data: vec![0xff; TERM_INPUT_MAX],
        }),
        Frame::Resize(size()),
        Frame::Colors(colors()),
    ]
    .into_iter()
    .map(|frame| TerminalClientFrame { frame: Some(frame) })
    .collect()
}

fn exited() -> TerminalExited {
    TerminalExited {
        code: Some(i32::MIN),
        signal: Some(i32::MIN),
        left: true,
    }
}

fn server_frames() -> Vec<TerminalServerFrame> {
    use terminal_server_frame::Frame;
    let mut frames = Vec::new();
    for tail_kind in [TailKind::None, TailKind::ReadAhead, TailKind::Raw] {
        frames.push(Frame::Snapshot(SnapshotPart {
            offset: u64::MAX,
            columns: TERM_COLUMNS_MAX,
            rows: TERM_ROWS_MAX,
            total_bytes: TERM_SNAPSHOT_MAX as u64,
            replay: vec![0xff; TERM_SNAPSHOT_PART_MAX],
            last: true,
            last_input: Some(char::MAX as u32),
            tail_kind: tail_kind as i32,
            // The unfinished parser tail has the same 64 KiB bound as live output.
            tail: if tail_kind == TailKind::None {
                Vec::new()
            } else {
                vec![0xff; TERM_OUTPUT_MAX]
            },
            tail_calls: u32::MAX,
            history_at_cap: true,
            history_trimmed: u32::MAX,
            output_while_detached: true,
            input_written: u64::MAX,
        }));
    }
    frames.extend([
        Frame::Snapshot(SnapshotPart {
            offset: u64::MAX,
            columns: TERM_COLUMNS_MAX,
            rows: TERM_ROWS_MAX,
            total_bytes: TERM_SNAPSHOT_MAX as u64,
            replay: vec![0xff; TERM_SNAPSHOT_PART_MAX],
            ..Default::default()
        }),
        Frame::Output(OutputBytes {
            offset: u64::MAX,
            data: vec![0xff; TERM_OUTPUT_MAX],
        }),
        Frame::Resized(Resized {
            offset: u64::MAX,
            size: Some(size()),
        }),
        Frame::InputWritten(InputWritten { upto: u64::MAX }),
        Frame::Exited(exited()),
    ]);
    for reason in [
        DetachReason::Unspecified,
        DetachReason::Replaced,
        DetachReason::Closed,
        DetachReason::Stopped,
    ] {
        frames.push(Frame::Detached(Detached { reason: reason as i32 }));
    }
    for reason in [WaitReason::Unspecified, WaitReason::OpenSequence, WaitReason::TooDeep] {
        frames.push(Frame::Waiting(Waiting { reason: reason as i32 }));
    }
    frames
        .into_iter()
        .map(|frame| TerminalServerFrame { frame: Some(frame) })
        .collect()
}

fn terminal_info() -> TerminalInfo {
    TerminalInfo {
        terminal_id: "t".repeat(TERM_ID_MAX),
        start_dir: "/".repeat(TERM_PATH_MAX),
        cwd: "/".repeat(TERM_PATH_MAX),
        pid: u32::MAX,
        started_unix_ms: u64::MAX,
        alive: true,
        exit: Some(exited()),
        attached: true,
        detached_since_unix_ms: u64::MAX,
        foreground_job: Some(true),
        foreground_name: "j".repeat(TERM_NAME_MAX),
        size: Some(size()),
        open_command_id: "o".repeat(TERM_ID_MAX),
    }
}

fn open_request() -> OpenTerminalRequest {
    OpenTerminalRequest {
        command_id: "o".repeat(TERM_ID_MAX),
        cwd: "/".repeat(TERM_PATH_MAX),
        size: Some(size()),
        colors: Some(colors()),
    }
}

fn open_response() -> OpenTerminalResponse {
    OpenTerminalResponse {
        terminal_id: "t".repeat(TERM_ID_MAX),
        daemon_instance: "f".repeat(32),
    }
}

fn list_response() -> ListTerminalsResponse {
    ListTerminalsResponse {
        terminals: vec![terminal_info(); TERM_LIST_MAX],
        truncated: true,
    }
}

fn close_request() -> CloseTerminalRequest {
    CloseTerminalRequest {
        terminal_id: "t".repeat(TERM_ID_MAX),
        command_id: "c".repeat(TERM_ID_MAX),
    }
}

#[test]
fn every_client_frame_round_trips() {
    for frame in client_frames() {
        round_trip(frame);
    }
    round_trip(TerminalClientFrame::default());
}

#[test]
fn every_server_frame_round_trips() {
    for frame in server_frames() {
        round_trip(frame);
    }
    round_trip(TerminalServerFrame::default());
}

#[test]
fn unary_messages_round_trip() {
    round_trip(open_request());
    round_trip(open_response());
    round_trip(ListTerminalsRequest {});
    round_trip(list_response());
    round_trip(close_request());
    round_trip(CloseTerminalResponse { exit: Some(exited()) });
    round_trip(HelloResponse {
        daemon_instance: "0123456789abcdef0123456789abcdef".into(),
        boot_id: "01234567-89ab-cdef-0123-456789abcdef".into(),
        ..Default::default()
    });
}

#[test]
fn optional_values_preserve_absence_and_explicit_zero() {
    for value in [None, Some(0)] {
        round_trip(TermColors {
            cursor: value,
            ..Default::default()
        });
        round_trip(SnapshotPart {
            last_input: value,
            ..Default::default()
        });
        round_trip(TerminalExited {
            code: value.map(|value| value as i32),
            signal: value.map(|value| value as i32),
            left: false,
        });
    }
    for value in [None, Some(false), Some(true)] {
        round_trip(TerminalInfo {
            foreground_job: value,
            ..Default::default()
        });
    }
}

#[test]
fn every_frame_at_its_bounds_is_under_4_mib() {
    fn under_limit(message: impl Message) {
        assert!(
            message.encoded_len() < 4 * 1024 * 1024,
            "{} bytes",
            message.encoded_len()
        );
    }
    for frame in client_frames() {
        under_limit(frame);
    }
    for frame in server_frames() {
        under_limit(frame);
    }
    under_limit(open_request());
    under_limit(open_response());
    under_limit(ListTerminalsRequest {});
    under_limit(list_response());
    under_limit(close_request());
    under_limit(CloseTerminalResponse { exit: Some(exited()) });
}
