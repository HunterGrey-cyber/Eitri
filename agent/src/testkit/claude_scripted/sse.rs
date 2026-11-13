//! Deterministic Messages API frames, shared by streamed and whole-message replies.
use serde_json::{json, Value};
use std::{io, io::Write, time::Duration};

use super::{Point, Reply};

#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub event: &'static str,
    pub data: Value,
    pub after: Option<Point>,
    pub delay: Duration,
}

impl Frame {
    pub fn write_to(&self, writer: &mut impl Write, wait: impl FnOnce(Duration)) -> io::Result<()> {
        if !self.delay.is_zero() {
            wait(self.delay);
        }
        writer.write_all(self.wire().as_bytes())
    }

    pub fn wire(&self) -> String {
        format!("event: {}\ndata: {}\n\n", self.event, self.data)
    }
}

pub fn message(reply: &Reply, index: usize, model: &str) -> Value {
    let mut content = reply.content.clone();
    for block in &mut content {
        if block["type"] == "thinking" {
            block["signature"] = json!("scripted");
        }
    }
    json!({"id":format!("msg_scripted_{index}"),"type":"message","role":"assistant",
        "model":model,"content":content,"stop_reason":reply.stop_reason,
        "stop_sequence":null,"usage":reply.usage})
}

// Preserve whitespace and UTF-8 while distributing whole words between the requested pieces.
fn text_chunks(text: &str, count: usize) -> Vec<String> {
    let mut ends: Vec<usize> = text
        .match_indices(char::is_whitespace)
        .map(|(i, c)| i + c.len())
        .collect();
    ends.push(text.len());
    ends.sort_unstable();
    ends.dedup();
    let count = count.max(1).min(ends.len());
    let mut start = 0;
    (1..=count)
        .map(|i| {
            let end = ends[(i * ends.len()).div_ceil(count) - 1];
            let part = text[start..end].to_owned();
            start = end;
            part
        })
        .collect()
}

pub fn frames(reply: &Reply, index: usize, model: &str) -> Vec<Frame> {
    let mut result = Vec::new();
    let mut seen_delta = false;
    let mut push = |event, data, after| {
        let mut delay = Duration::ZERO;
        if event == "content_block_delta" {
            if seen_delta {
                delay = Duration::from_millis(reply.pace_ms);
            }
            seen_delta = true;
        }
        result.push(Frame {
            event,
            data,
            after,
            delay,
        });
    };
    let mut start = message(reply, index, model);
    start["content"] = json!([]);
    start["stop_reason"] = Value::Null;
    start["usage"]["output_tokens"] = json!(0);
    push(
        "message_start",
        json!({"type":"message_start","message":start}),
        Some(Point::MessageStart),
    );
    if reply.cut == Some(Point::MessageStart) {
        return result;
    }
    for (i, block) in reply.content.iter().enumerate() {
        let kind = block["type"].as_str().expect("validated content type");
        let mut initial = block.clone();
        match kind {
            "text" => initial["text"] = json!(""),
            "tool_use" => initial["input"] = json!({}),
            "thinking" => {
                initial["thinking"] = json!("");
                initial["signature"] = json!("");
            }
            _ => unreachable!("validated content type"),
        }
        push(
            "content_block_start",
            json!({"type":"content_block_start","index":i,"content_block":initial}),
            None,
        );
        let deltas: Vec<Value> = match kind {
            "text" => text_chunks(block["text"].as_str().unwrap(), reply.chunks)
                .into_iter()
                .map(|text| json!({"type":"text_delta","text":text}))
                .collect(),
            "tool_use" => {
                let input = block["input"].to_string();
                let midpoint = input
                    .char_indices()
                    .nth(input.chars().count() / 2)
                    .map_or(input.len(), |(i, _)| i);
                [&input[..midpoint], &input[midpoint..]]
                    .into_iter()
                    .map(|s| json!({"type":"input_json_delta","partial_json":s}))
                    .collect()
            }
            "thinking" => vec![
                json!({"type":"thinking_delta","thinking":block["thinking"]}),
                json!({"type":"signature_delta","signature":"scripted"}),
            ],
            _ => unreachable!(),
        };
        for delta in deltas {
            push(
                "content_block_delta",
                json!({"type":"content_block_delta","index":i,"delta":delta}),
                None,
            );
        }
        push(
            "content_block_stop",
            json!({"type":"content_block_stop","index":i}),
            Some(Point::AfterBlock(i)),
        );
        if reply.cut == Some(Point::AfterBlock(i)) {
            return result;
        }
    }
    push(
        "message_delta",
        json!({"type":"message_delta","delta":{"stop_reason":reply.stop_reason,"stop_sequence":null},
        "usage":{"output_tokens":reply.usage["output_tokens"]}}),
        Some(Point::BeforeStop),
    );
    push("message_stop", json!({"type":"message_stop"}), None);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(reply: &Reply) -> String {
        frames(reply, 1, "scripted-model").iter().map(Frame::wire).collect()
    }

    const START: &str = r##"event: message_start
data: {"message":{"content":[],"id":"msg_scripted_1","model":"scripted-model","role":"assistant","stop_reason":null,"stop_sequence":null,"type":"message","usage":{"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"input_tokens":1000,"output_tokens":0}},"type":"message_start"}

"##;

    const END_TURN: &str = r##"event: message_delta
data: {"delta":{"stop_reason":"end_turn","stop_sequence":null},"type":"message_delta","usage":{"output_tokens":10}}

event: message_stop
data: {"type":"message_stop"}

"##;

    const END_TOOL: &str = r##"event: message_delta
data: {"delta":{"stop_reason":"tool_use","stop_sequence":null},"type":"message_delta","usage":{"output_tokens":10}}

event: message_stop
data: {"type":"message_stop"}

"##;

    const TEXT: &str = r##"event: content_block_start
data: {"content_block":{"text":"","type":"text"},"index":0,"type":"content_block_start"}

event: content_block_delta
data: {"delta":{"text":"hello","type":"text_delta"},"index":0,"type":"content_block_delta"}

event: content_block_stop
data: {"index":0,"type":"content_block_stop"}

"##;

    const TOOL: &str = r##"event: content_block_start
data: {"content_block":{"id":"tool_1","input":{},"name":"Read","type":"tool_use"},"index":0,"type":"content_block_start"}

event: content_block_delta
data: {"delta":{"partial_json":"{\"path\"","type":"input_json_delta"},"index":0,"type":"content_block_delta"}

event: content_block_delta
data: {"delta":{"partial_json":":\"file\"}","type":"input_json_delta"},"index":0,"type":"content_block_delta"}

event: content_block_stop
data: {"index":0,"type":"content_block_stop"}

"##;

    const THINKING: &str = r##"event: content_block_start
data: {"content_block":{"signature":"","thinking":"","type":"thinking"},"index":0,"type":"content_block_start"}

event: content_block_delta
data: {"delta":{"thinking":"consider","type":"thinking_delta"},"index":0,"type":"content_block_delta"}

event: content_block_delta
data: {"delta":{"signature":"scripted","type":"signature_delta"},"index":0,"type":"content_block_delta"}

event: content_block_stop
data: {"index":0,"type":"content_block_stop"}

"##;

    const CHUNKED: &str = r##"event: content_block_start
data: {"content_block":{"text":"","type":"text"},"index":0,"type":"content_block_start"}

event: content_block_delta
data: {"delta":{"text":"one two ","type":"text_delta"},"index":0,"type":"content_block_delta"}

event: content_block_delta
data: {"delta":{"text":"three four ","type":"text_delta"},"index":0,"type":"content_block_delta"}

event: content_block_delta
data: {"delta":{"text":"five six ","type":"text_delta"},"index":0,"type":"content_block_delta"}

event: content_block_delta
data: {"delta":{"text":"seven eight ","type":"text_delta"},"index":0,"type":"content_block_delta"}

event: content_block_delta
data: {"delta":{"text":"nine ten","type":"text_delta"},"index":0,"type":"content_block_delta"}

event: content_block_stop
data: {"index":0,"type":"content_block_stop"}

"##;

    #[test]
    fn pacing_waits_before_each_subsequent_delta_across_blocks_without_changing_bytes() {
        use std::cell::Cell;
        struct Trace<'a> {
            clock: &'a Cell<u64>,
            writes: Vec<(u64, String)>,
        }
        impl Write for Trace<'_> {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.writes
                    .push((self.clock.get(), String::from_utf8(bytes.to_vec()).unwrap()));
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut reply = Reply::text("one two\n");
        reply.content.push(json!({"type":"text","text":"three four\n"}));
        reply.chunks = 2;
        let unpaced = wire(&reply);
        for frame in frames(&reply, 1, "scripted-model") {
            frame
                .write_to(&mut Vec::new(), |_| panic!("default must not wait"))
                .unwrap();
        }
        reply.pace_ms = 120;
        let clock = Cell::new(0);
        let mut trace = Trace {
            clock: &clock,
            writes: Vec::new(),
        };
        for frame in frames(&reply, 1, "scripted-model") {
            frame
                .write_to(&mut trace, |delay| clock.set(clock.get() + delay.as_millis() as u64))
                .unwrap();
        }
        assert_eq!(
            trace.writes.iter().map(|(_, bytes)| bytes.as_str()).collect::<String>(),
            unpaced
        );
        let times: Vec<_> = trace
            .writes
            .iter()
            .filter(|(_, bytes)| bytes.starts_with("event: content_block_delta\n"))
            .map(|(time, _)| *time)
            .collect();
        assert_eq!(times, [0, 120, 240, 360]);
        assert_eq!(clock.get(), 360);
    }

    #[test]
    fn text_reply_golden_frames() {
        assert_eq!(wire(&Reply::text("hello")), format!("{START}{TEXT}{END_TURN}"));
    }

    #[test]
    fn tool_reply_golden_frames() {
        let mut reply = Reply::text("");
        reply.content = vec![json!({"type":"tool_use","id":"tool_1","name":"Read","input":{"path":"file"}})];
        reply.stop_reason = "tool_use".into();
        assert_eq!(wire(&reply), format!("{START}{TOOL}{END_TOOL}"));
    }

    #[test]
    fn thinking_reply_golden_frames() {
        let mut reply = Reply::text("");
        reply.content = vec![json!({"type":"thinking","thinking":"consider","signature":"scripted"})];
        assert_eq!(wire(&reply), format!("{START}{THINKING}{END_TURN}"));
    }

    #[test]
    fn five_chunks_golden_frames() {
        let mut reply = Reply::text("one two three four five six seven eight nine ten");
        reply.chunks = 5;
        assert_eq!(wire(&reply), format!("{START}{CHUNKED}{END_TURN}"));
    }

    #[test]
    fn cut_after_first_block_golden_frames() {
        let mut reply = Reply::text("hello");
        reply.cut = Some(Point::AfterBlock(0));
        assert_eq!(wire(&reply), format!("{START}{TEXT}"));
    }

    #[test]
    fn whole_message_supplies_the_streamed_thinking_signature() {
        let mut reply = Reply::text("");
        reply.content = vec![json!({"type":"thinking","thinking":"consider"})];
        assert_eq!(message(&reply, 1, "m")["content"][0]["signature"], "scripted");
        assert_eq!(wire(&reply), format!("{START}{THINKING}{END_TURN}"));
    }

    #[test]
    fn chunking_preserves_unicode_and_whitespace() {
        for text in ["", "é", "  one\t two\nthree  ", "你好 世界", "unbroken"] {
            for count in [0, 1, 3, 5, 100] {
                assert_eq!(text_chunks(text, count).concat(), text);
            }
        }
        assert_eq!(
            text_chunks("one two three four five six", 3),
            ["one two ", "three four ", "five six"]
        );
    }
}
