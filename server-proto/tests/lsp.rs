use std::fmt::Debug;

use eitri_server_proto::*;
use prost::Message;
use tonic::server::NamedService;

fn round_trip<M: Message + Default + PartialEq + Debug>(message: M) {
    let encoded = message.encode_to_vec();
    assert_eq!(encoded.len(), message.encoded_len());
    assert_eq!(M::decode(encoded.as_slice()).unwrap(), message);
}

fn hex64() -> String {
    "ab".repeat(32)
}

fn open_request() -> OpenServerRequest {
    OpenServerRequest {
        command_id: "c".repeat(32),
        root: "/".repeat(LSP_PATH_MAX),
        argv: vec!["a".repeat(LSP_ARGV_ENTRY_MAX); LSP_ARGV_MAX],
        grace_ms: u64::MAX,
        client_id: "d".repeat(32),
    }
}

fn open_response() -> OpenServerResponse {
    OpenServerResponse {
        server_id: "s".repeat(32),
        resolved_command: "/".repeat(LSP_PATH_MAX),
        missing_command: "m".repeat(LSP_ARGV_ENTRY_MAX),
        refusal: "r".repeat(4096),
    }
}

fn chunk(len: usize, last: bool, saved: bool) -> LspChunk {
    LspChunk {
        data: vec![0xff; len],
        last,
        saved_sha256: if saved { hex64() } else { String::new() },
    }
}

fn registrations() -> Vec<LspRegistration> {
    // The count bound and the byte bound hold together: 256 entries sharing 1 MiB of params.
    let each = LSP_REGISTRATIONS_PARAMS_MAX / LSP_REGISTRATIONS_MAX;
    (0..LSP_REGISTRATIONS_MAX)
        .map(|index| LspRegistration {
            method: if index % 2 == 0 {
                "client/registerCapability"
            } else {
                "client/unregisterCapability"
            }
            .into(),
            params: vec![0xff; each],
        })
        .collect()
}

fn attached(reattached: bool) -> LspAttached {
    LspAttached {
        reattached,
        registrations: registrations(),
        requests_answered: u64::MAX,
        notifications_dropped: u64::MAX,
        dropped_saves: vec!["f".repeat(LSP_URI_MAX); LSP_DROPPED_SAVES_MAX],
    }
}

fn ended(reason: LspEndReason) -> LspEnded {
    LspEnded {
        code: Some(i32::MIN),
        signal: Some(i32::MIN),
        reason: reason as i32,
        stderr_tail: vec![0xff; LSP_STDERR_TAIL],
    }
}

fn client_frames() -> Vec<LspClientFrame> {
    use lsp_client_frame::Frame;
    vec![
        Frame::Hello(LspAttachHello {
            server_id: "s".repeat(32),
            client_id: "d".repeat(32),
        }),
        Frame::Chunk(chunk(LSP_CHUNK_MAX, true, true)),
        Frame::Chunk(chunk(LSP_CHUNK_MAX, false, false)),
    ]
    .into_iter()
    .map(|frame| LspClientFrame { frame: Some(frame) })
    .collect()
}

fn server_frames() -> Vec<LspServerFrame> {
    use lsp_server_frame::Frame;
    let mut frames = vec![
        Frame::Chunk(chunk(LSP_CHUNK_MAX, true, false)),
        Frame::Chunk(chunk(LSP_CHUNK_MAX, false, false)),
        Frame::Attached(attached(true)),
        Frame::Attached(attached(false)),
    ];
    for reason in [
        LspEndReason::Unspecified,
        LspEndReason::Exited,
        LspEndReason::Grace,
        LspEndReason::QueueFull,
        LspEndReason::Stopped,
        LspEndReason::Replaced,
        LspEndReason::Daemon,
        LspEndReason::Gone,
    ] {
        frames.push(Frame::Ended(ended(reason)));
    }
    frames
        .into_iter()
        .map(|frame| LspServerFrame { frame: Some(frame) })
        .collect()
}

fn server_info() -> LspServerInfo {
    LspServerInfo {
        server_id: "s".repeat(32),
        root: "/".repeat(LSP_PATH_MAX),
        command: "c".repeat(LSP_PATH_MAX),
        pid: u32::MAX,
        attached: true,
        detached_since_unix_ms: u64::MAX,
        queued_bytes: u64::MAX,
    }
}

fn list_response() -> ListServersResponse {
    ListServersResponse {
        servers: vec![server_info(); LSP_LIST_MAX],
        truncated: true,
    }
}

fn read_path_chunks() -> Vec<ReadPathChunk> {
    vec![
        ReadPathChunk {
            data: vec![0xff; LSP_CHUNK_MAX],
            ..Default::default()
        },
        ReadPathChunk {
            sha256: hex64(),
            ..Default::default()
        },
        ReadPathChunk {
            refusal: "r".repeat(4096),
            ..Default::default()
        },
    ]
}

#[test]
fn every_lsp_client_frame_round_trips() {
    for frame in client_frames() {
        round_trip(frame);
    }
    round_trip(LspClientFrame::default());
}

#[test]
fn every_lsp_server_frame_round_trips() {
    for frame in server_frames() {
        round_trip(frame);
    }
    round_trip(LspServerFrame::default());
}

#[test]
fn unary_lsp_messages_round_trip() {
    round_trip(open_request());
    round_trip(open_response());
    round_trip(ListServersRequest {});
    round_trip(list_response());
    round_trip(StopServerRequest {
        server_id: "s".repeat(32),
        command_id: "c".repeat(32),
    });
    round_trip(StopServerResponse {});
    round_trip(ReadPathRequest {
        path: "/".repeat(4096),
        limit: READ_PATH_MAX as u64,
    });
    for chunk in read_path_chunks() {
        round_trip(chunk);
    }
}

#[test]
fn optional_end_values_preserve_absence_and_explicit_zero() {
    for value in [None, Some(0)] {
        round_trip(LspEnded {
            code: value,
            signal: value,
            ..Default::default()
        });
    }
    assert_ne!(
        LspEnded {
            code: None,
            ..Default::default()
        }
        .encode_to_vec(),
        LspEnded {
            code: Some(0),
            ..Default::default()
        }
        .encode_to_vec()
    );
}

#[test]
fn every_lsp_frame_at_its_bounds_is_under_4_mib() {
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
    under_limit(list_response());
    for chunk in read_path_chunks() {
        under_limit(chunk);
    }
}

#[test]
fn the_lsp_service_is_registered_beside_helper_and_terminal() {
    assert_eq!(
        <helper_server::HelperServer<()> as NamedService>::NAME,
        "eitri.server.v1.Helper"
    );
    assert_eq!(
        <terminal_server::TerminalServer<()> as NamedService>::NAME,
        "eitri.server.v1.Terminal"
    );
    assert_eq!(<lsp_server::LspServer<()> as NamedService>::NAME, "eitri.server.v1.Lsp");
    // The client type exists for the same service, so a bridge can dial it on the helper's channel.
    fn _client_exists(channel: tonic::transport::Channel) -> lsp_client::LspClient<tonic::transport::Channel> {
        lsp_client::LspClient::new(channel)
    }
}
