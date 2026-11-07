#[path = "support/randomized.rs"]
mod randomized;

use eitri_core::ide_server::frames::{Decoder, Inbound};
use eitri_core::remote_rv::{parse_rv_line, parse_rv_outbound};
use eitri_core::remote_wire::parse_phone_frame;
use std::panic::catch_unwind;

#[test]
fn remote_phone_frames_randomized() {
    randomized::exercise(
        |bytes| std::str::from_utf8(bytes).is_ok_and(|s| parse_phone_frame(s).is_ok()),
        randomized::PHONE,
        0xb4b1_0215_3241_ace1,
    );
}

#[test]
fn remote_rv_inbound_randomized() {
    randomized::exercise(
        |bytes| parse_rv_line(bytes).is_ok(),
        &[
            r#"{"v":1,"kind":"subscribe","key":"tab"}"#,
            r#"{"v":1,"kind":"unsubscribe","key":"tab"}"#,
            r#"{"v":1,"kind":"act","device":"phone","token":"test","key":"tab","cmd":{}}"#,
            r#"{"v":1,"kind":"ping"}"#,
            r#"{"v":1,"kind":"more","key":"tab","generation":1,"before_turn":2}"#,
            r#"{"v":1,"kind":"enroll","code":"test","device_name":"phone","host":"localhost","peer":"local","recipe":"direct"}"#,
            r#"{"v":1,"kind":"start_enroll"}"#,
            r#"{"v":1,"kind":"grant_request","device":"phone","grant":"watch"}"#,
            r#"{"v":1,"kind":"listen_request","addr":"198.51.100.2"}"#,
            r#"{"v":1,"kind":"revoke_self","device":"phone","token":"test"}"#,
            r#"{"v":1,"kind":"set_push_endpoint","device":"phone","token":"test","url":"https://example.org/push"}"#,
            r#"{"v":1,"kind":"devices_connected","devices":[{"device":"phone","host":"localhost","peer":"local","since":0}],"epoch":1,"seq":0}"#,
        ],
        0x15f0_9876_1234_abcd,
    );
}

#[test]
fn remote_rv_outbound_randomized() {
    randomized::exercise(
        |bytes| parse_rv_outbound(bytes).is_ok(),
        &[
            r#"{"v":1,"kind":"pong"}"#,
            r#"{"v":1,"kind":"snapshot","key":"tab","through":0,"generation":1,"unseen":null,"first_turn":0,"cut":[],"frame":{}}"#,
            r#"{"v":1,"kind":"events","key":"tab","from":0,"through":1,"generation":1,"unseen":0,"frame":[]}"#,
            r#"{"v":1,"kind":"page","key":"tab","generation":1,"unseen":null,"from_turn":0,"through_turn":1,"frame":{}}"#,
            r#"{"v":1,"kind":"chunk","of":0,"i":0,"last":true,"data":"hello"}"#,
            r#"{"v":1,"kind":"gone","key":"tab","why":"closed"}"#,
            r#"{"v":1,"kind":"ended","key":"tab","how":"failed"}"#,
            r#"{"v":1,"kind":"greeting","key":"tab","greeting":{}}"#,
            r#"{"v":1,"kind":"theme","theme":{}}"#,
            r#"{"v":1,"kind":"send_block","key":"tab","block":null}"#,
            r#"{"v":1,"kind":"result","command_id":null,"ok":true,"why":null}"#,
            r#"{"v":1,"kind":"refused","key":null,"why":"refused"}"#,
            r#"{"v":1,"kind":"enrolled","device":"phone","token":"test"}"#,
            r#"{"v":1,"kind":"enroll_refused","why":"refused"}"#,
            r#"{"v":1,"kind":"listen_answer","addr":"198.51.100.2","yes":false}"#,
        ],
        0x7193_6324_44ac_0123,
    );
}

fn masked(op: u8, payload: &[u8], key: [u8; 4], out: &mut Vec<u8>) {
    out.push(op);
    if payload.len() <= 125 {
        out.push(0x80 | payload.len() as u8);
    } else {
        out.push(0x80 | 126);
        out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    }
    out.extend_from_slice(&key);
    out.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i % 4]));
}

#[test]
fn remote_websocket_decoder_randomized() {
    let mut rng = randomized::Random(0x8ab1_4123_ace0_5eed);
    let mut counted = 0;
    let mut texts = 0;
    let mut oversized = 0;
    for i in 0..randomized::CASES {
        let limit = (rng.next() % 257) as usize;
        let len = match i % 5 {
            0 => limit.saturating_sub(1),
            1 => limit,
            2 => limit + 1,
            _ => (rng.next() % 1025) as usize,
        };
        let payload = rng.bytes(len);
        let key = rng.next().to_le_bytes()[..4].try_into().unwrap();
        let mut wire = Vec::new();
        if i % 2 == 0 {
            masked(0x81, &payload, key, &mut wire);
        } else {
            let split = len / 2;
            masked(0x01, &payload[..split], key, &mut wire);
            masked(0x89, b"ping", key, &mut wire);
            masked(0x80, &payload[split..], key, &mut wire);
        }
        if i % 5 == 4 {
            let at = rng.next() as usize % wire.len();
            wire[at] = rng.next() as u8;
        }
        let chunk = 1 + (rng.next() % 33) as usize;
        let outcome = catch_unwind(|| {
            let mut decoder = Decoder::new(limit, 2048);
            let mut events = Vec::new();
            for part in wire.chunks(chunk) {
                decoder.feed(part, &mut events);
            }
            let mut whole = Vec::new();
            Decoder::new(limit, 2048).feed(&wire, &mut whole);
            assert_eq!(events, whole, "read boundaries changed input {i}");
            events
        });
        assert!(outcome.is_ok(), "decoder panicked at input {i}");
        let events = outcome.unwrap();
        for event in &events {
            match event {
                Inbound::Text(bytes) => {
                    assert!(bytes.len() <= limit);
                    texts += 1;
                }
                Inbound::Oversized { .. } => oversized += 1,
                _ => {}
            }
        }
        if i % 5 != 4 {
            if len <= limit {
                assert!(events.contains(&Inbound::Text(payload)));
            } else {
                assert!(events.iter().any(|e| matches!(e, Inbound::Oversized { .. })));
            }
        }
        // Arbitrary bytes also exercise invalid opcodes, masks and extended lengths.
        let noise_len = (rng.next() % 257) as usize;
        let noise = rng.bytes(noise_len);
        assert!(
            catch_unwind(|| {
                let mut events = Vec::new();
                let mut decoder = Decoder::new(limit, 2048);
                for part in noise.chunks(chunk) {
                    decoder.feed(part, &mut events);
                }
                for event in events {
                    if let Inbound::Text(bytes) = event {
                        assert!(bytes.len() <= limit);
                    }
                }
            })
            .is_ok(),
            "arbitrary bytes panicked at input {i}"
        );
        counted += 1;
    }
    assert_eq!(counted, randomized::CASES);
    assert!(texts > 0 && oversized > 0);
}
