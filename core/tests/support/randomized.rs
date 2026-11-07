use serde_json::Value;
use std::panic::catch_unwind;

pub const CASES: usize = 100_000;

pub struct Random(pub u64);
impl Random {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }
}

pub fn exercise(parse: fn(&[u8]) -> bool, fixtures: &[&str], seed: u64) {
    let mut rng = Random(seed);
    let mut counted = 0;
    let mut unknown = 0;
    let mut accepted = 0;
    for i in 0..CASES {
        let fixture = fixtures[i % fixtures.len()];
        let mut value: Value = serde_json::from_str(fixture).unwrap();
        // Vary envelope counters without invalidating the frame's shape.
        for field in ["seq", "ack", "generation", "through", "before_turn", "from_turn"] {
            if value.get(field).is_some() {
                value[field] = rng.next().into();
            }
        }
        let valid = serde_json::to_vec(&value).unwrap();
        let outcome = catch_unwind(|| parse(&valid));
        assert!(outcome.is_ok(), "valid frame panicked at input {i}");
        assert!(outcome.unwrap(), "valid fixture refused at input {i}: {fixture}");
        accepted += 1;

        let extra = format!("unknown_{:016x}", rng.next());
        value[extra] = match i % 4 {
            0 => Value::Null,
            1 => rng.next().into(),
            2 => Value::String(format!("雪\\\"{}", rng.next())),
            _ => serde_json::json!({"nested": [true, false, null]}),
        };
        let with_unknown = serde_json::to_vec(&value).unwrap();
        let outcome = catch_unwind(|| parse(&with_unknown));
        assert!(outcome.is_ok(), "unknown field panicked at input {i}");
        assert!(!outcome.unwrap(), "unknown field accepted at input {i}: {fixture}");
        unknown += 1;

        let mut malformed = valid;
        match i % 5 {
            0 => {
                let len = (rng.next() % 257) as usize;
                malformed = rng.bytes(len);
            }
            1 => {
                let len = (rng.next() as usize) % malformed.len();
                malformed.truncate(len);
            }
            2 => {
                let pos = rng.next() as usize % malformed.len();
                malformed[pos] = rng.next() as u8;
            }
            3 => malformed.extend_from_slice(b"{}"),
            _ => malformed = format!("{}0", rng.next()).into_bytes(),
        }
        assert!(
            catch_unwind(|| parse(&malformed)).is_ok(),
            "malformed input {i} panicked"
        );
        counted += 1;
    }
    assert_eq!((accepted, unknown, counted), (CASES, CASES, CASES));
}

pub const PHONE: &[&str] = &[
    r#"{"v":1,"seq":0,"ack":0,"kind":"hello","device":"phone","token":null,"layout":"phone","protocol":1}"#,
    r#"{"v":1,"seq":0,"ack":0,"kind":"subscribe_rows"}"#,
    r#"{"v":1,"seq":0,"ack":0,"kind":"subscribe","key":"tab"}"#,
    r#"{"v":1,"seq":0,"ack":0,"kind":"unsubscribe","key":"tab"}"#,
    r#"{"v":1,"seq":0,"ack":0,"kind":"more","key":"tab","generation":1,"before_turn":2}"#,
    r#"{"v":1,"seq":0,"ack":0,"kind":"act","key":"tab","cmd":{"kind":"send","text":"hello"}}"#,
    r#"{"v":1,"seq":0,"ack":0,"kind":"ping"}"#,
    r#"{"v":1,"seq":0,"ack":0,"kind":"enroll","instance":"0123456789abcdef","code":"test","device_name":"phone","recipe":"ssh"}"#,
    r#"{"v":1,"seq":0,"ack":0,"kind":"revoke_self"}"#,
    r#"{"v":1,"seq":0,"ack":0,"kind":"set_push_endpoint","url":"https://example.org/push"}"#,
];
