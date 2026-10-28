//! Scrubbing a harness run for the repository, and the twin fixture built from a scenario's events.
//!
//! A run directory holds what the real CLI and the stand-in saw: the tap's two logs, the stand-in's request log
//! with every request body, and the CLI's own transcript. None of it may reach git as it is. [`Scrubber`] applies
//! one set of rules to all of it:
//!
//! - a request body keeps `model`, `stream`, `max_tokens` and `messages` (block types, ids, tool inputs and the
//!   text of user, tool-result and assistant blocks); `system` becomes `{sha256, chars}`; `tools` becomes
//!   `[{name, deferred}]` and one digest; a text block that is a system reminder (and every block of a `system`
//!   role message) becomes `{system_reminder: {sha256, chars, first_line}}`; every other field is named in
//!   `dropped_fields` and nothing more;
//! - the run directory, a home directory, the user's name, the host's name and the API key become `$RUN`,
//!   `$USER`, `$HOST` and `$KEY` in every string, keys of objects included.
//!
//! [`scrub_run`] writes the scrubbed files into a scenario directory under the names the staged layout uses;
//! [`twin`] folds a scenario's expected events into the shipped twin, where only the scenario's own words, ids
//! and a closed list of enum words stay readable.
use regex::{NoExpand, Regex};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
};

/// What the run directory's path becomes.
pub const RUN: &str = "$RUN";
/// What the user's name becomes.
pub const USER: &str = "$USER";
/// What the host's name becomes.
pub const HOST: &str = "$HOST";
/// What the API key becomes.
pub const KEY: &str = "$KEY";

/// The key the stand-in accepts in a harness run; it names no account.
pub const HARNESS_KEY: &str = "scripted-harness-no-credential";

/// The CLI's per-user directory under its temporary directory; the digits are the uid of whoever ran it.
const UID_DIR: &str = r"(/tmp/claude-)[0-9]+";

/// Longest first line a system-reminder digest carries.
const FIRST_LINE_CHARS: usize = 80;

/// Names that read as ordinary words in text: replacing them would damage the scenario's own wording.
const COMMON_NAMES: &[&str] = &[
    "root",
    "user",
    "host",
    "localhost",
    "nobody",
    "admin",
    "test",
    "ubuntu",
    "debian",
];

/// Who and where the run happened, as far as the scrub needs to know.
#[derive(Clone, Debug, Default)]
pub struct Identity {
    /// The home directory of the process that ran the scenario.
    pub home: Option<String>,
    pub users: Vec<String>,
    pub hosts: Vec<String>,
    /// Credentials that must never appear; [`HARNESS_KEY`] is always added.
    pub keys: Vec<String>,
}

impl Identity {
    /// The identity of this process: `$HOME`, `$USER`/`$LOGNAME`, the host name.
    pub fn from_env() -> Self {
        let mut id = Identity {
            home: std::env::var("HOME").ok(),
            ..Identity::default()
        };
        for var in ["USER", "LOGNAME"] {
            if let Ok(name) = std::env::var(var) {
                id.users.push(name);
            }
        }
        if let Ok(name) = std::env::var("HOSTNAME") {
            id.hosts.push(name);
        }
        if let Ok(text) = fs::read_to_string("/etc/hostname") {
            id.hosts.push(text.trim().to_owned());
        }
        let mut buf = [0u8; 256];
        // SAFETY: `buf` is writable for its whole length, which is what `gethostname` is told.
        if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0 {
            let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
            id.hosts.push(String::from_utf8_lossy(&buf[..end]).into_owned());
        }
        id
    }
}

struct Rule {
    re: Regex,
    to: String,
    /// The pattern starts with a group that holds the character before the match, which is kept: a path that
    /// follows a word character (the `$RUN` an earlier rule wrote) is not one to replace again.
    guarded: bool,
}

impl Rule {
    fn apply(&self, text: &str) -> String {
        if !self.re.is_match(text) {
            return text.to_owned();
        }
        if self.guarded {
            self.re
                .replace_all(text, |c: &regex::Captures| format!("{}{}", &c[1], self.to))
                .into_owned()
        } else {
            self.re.replace_all(text, NoExpand(&self.to)).into_owned()
        }
    }
}

/// One set of replacement rules; see the module documentation.
pub struct Scrubber {
    /// The run directory, the process's home and the credentials.
    paths: Vec<Rule>,
    /// Any other home directory: `/home/<name>` and `/Users/<name>`.
    other_homes: Rule,
    /// The user's and the host's names.
    names: Vec<Rule>,
    /// The per-user directory the CLI makes under its temporary directory: `/tmp/claude-<uid>`.
    uid_dir: Regex,
}

fn munge(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn word_rule(names: &BTreeSet<String>, to: &str) -> Option<Rule> {
    let names: Vec<String> = names
        .iter()
        .filter(|n| n.chars().count() >= 3 && !COMMON_NAMES.contains(&n.to_lowercase().as_str()))
        .map(|n| regex::escape(n))
        .collect();
    if names.is_empty() {
        return None;
    }
    let re = Regex::new(&format!(r"\b(?:{})\b", names.join("|"))).expect("names are escaped");
    Some(Rule {
        re,
        to: to.to_owned(),
        guarded: false,
    })
}

fn literal_rule(text: &str, to: &str) -> Option<Rule> {
    (!text.is_empty()).then(|| Rule {
        re: Regex::new(&regex::escape(text)).expect("escaped"),
        to: to.to_owned(),
        guarded: false,
    })
}

/// A directory path, matched only where no word character or `$` precedes it.
fn path_rule(path: &str, to: &str) -> Rule {
    Rule {
        re: Regex::new(&format!(r"(^|[^A-Za-z0-9_$]){}", regex::escape(path))).expect("escaped"),
        to: to.to_owned(),
        guarded: true,
    }
}

impl Scrubber {
    /// The rules for the run directory `run` and this process's identity.
    pub fn for_run(run: &Path) -> Self {
        Self::new(run, &Identity::from_env())
    }

    pub fn new(run: &Path, who: &Identity) -> Self {
        let run = run.to_string_lossy().trim_end_matches('/').to_owned();
        let mut paths = Vec::new();
        let mut keys: Vec<String> = who.keys.iter().filter(|k| k.len() >= 8).cloned().collect();
        keys.push(HARNESS_KEY.to_owned());
        keys.sort_by_key(|k| std::cmp::Reverse(k.len()));
        paths.extend(keys.iter().filter_map(|k| literal_rule(k, KEY)));
        if let Ok(re) = Regex::new(r"sk-ant-[A-Za-z0-9_-]{8,}") {
            paths.push(Rule {
                re,
                to: KEY.to_owned(),
                guarded: false,
            });
        }
        if run.len() > 1 {
            paths.push(path_rule(&run, RUN));
            // The CLI names a project directory after the working directory with every other character a dash.
            paths.extend(literal_rule(&munge(&run), "-RUN"));
        }
        if let Some(home) = who.home.as_deref().map(|h| h.trim_end_matches('/')) {
            if home.len() > 1 {
                paths.push(path_rule(home, RUN));
            }
        }
        let names = [
            word_rule(&who.users.iter().cloned().collect(), USER),
            word_rule(&who.hosts.iter().cloned().collect(), HOST),
        ];
        Scrubber {
            paths,
            other_homes: Rule {
                re: Regex::new(r"(^|[^A-Za-z0-9_$])(?:/home|/Users)/[A-Za-z0-9._-]+").expect("fixed pattern"),
                to: RUN.to_owned(),
                guarded: true,
            },
            names: names.into_iter().flatten().collect(),
            uid_dir: Regex::new(UID_DIR).expect("fixed pattern"),
        }
    }

    /// `text` with every rule applied. Applying it twice changes nothing more.
    pub fn text(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for rule in &self.paths {
            out = rule.apply(&out);
        }
        out = self.other_homes.apply(&out);
        for rule in &self.names {
            out = rule.apply(&out);
        }
        if self.uid_dir.is_match(&out) {
            out = self.uid_dir.replace_all(&out, "${1}$$UID").into_owned();
        }
        out
    }

    /// `value` with every string, and every object key, scrubbed.
    pub fn value(&self, value: &Value) -> Value {
        match value {
            Value::String(s) => Value::String(self.text(s)),
            Value::Array(items) => Value::Array(items.iter().map(|v| self.value(v)).collect()),
            Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (self.text(k), self.value(v))).collect()),
            other => other.clone(),
        }
    }

    /// A request body as the repository keeps it.
    pub fn request_body(&self, body: &Value) -> Value {
        let Some(object) = body.as_object() else {
            return json!({"unparsed": digest_text(&body.to_string())});
        };
        let mut out = Map::new();
        let mut dropped = BTreeSet::new();
        for (key, value) in object {
            match key.as_str() {
                "model" | "stream" | "max_tokens" => {
                    out.insert(key.clone(), self.value(value));
                }
                "messages" => {
                    out.insert(key.clone(), self.messages(value));
                }
                "system" => {
                    out.insert(key.clone(), digest_value(value));
                }
                "tools" => {
                    out.insert(key.clone(), tool_list(value));
                    out.insert("tools_sha256".into(), json!(sha256_hex(&value.to_string())));
                }
                other => {
                    dropped.insert(other.to_owned());
                }
            }
        }
        if !dropped.is_empty() {
            out.insert("dropped_fields".into(), json!(dropped));
        }
        Value::Object(out)
    }

    fn messages(&self, messages: &Value) -> Value {
        let Some(list) = messages.as_array() else {
            return self.value(messages);
        };
        Value::Array(
            list.iter()
                .map(|message| {
                    let system = message["role"] == "system";
                    match message.as_object() {
                        Some(object) => Value::Object(
                            object
                                .iter()
                                .map(|(k, v)| {
                                    let v = if k == "content" {
                                        self.content(v, system)
                                    } else {
                                        self.value(v)
                                    };
                                    (self.text(k), v)
                                })
                                .collect(),
                        ),
                        None => self.value(message),
                    }
                })
                .collect(),
        )
    }

    /// Message or tool-result content: a string, or a list of blocks.
    fn content(&self, content: &Value, system: bool) -> Value {
        match content {
            Value::String(text) if system || is_reminder(text) => self.reminder_block(text, system),
            Value::Array(blocks) => Value::Array(
                blocks
                    .iter()
                    .map(|block| {
                        let text = block["text"].as_str();
                        match (block["type"].as_str(), text) {
                            (Some("text"), Some(text)) if system || is_reminder(text) => {
                                self.reminder_block(text, system)
                            }
                            _ => match block.as_object() {
                                Some(object) => Value::Object(
                                    object
                                        .iter()
                                        .map(|(k, v)| {
                                            let v = if k == "content" {
                                                self.content(v, false)
                                            } else {
                                                self.value(v)
                                            };
                                            (self.text(k), v)
                                        })
                                        .collect(),
                                ),
                                None => self.value(block),
                            },
                        }
                    })
                    .collect(),
            ),
            other => self.value(other),
        }
    }

    fn reminder_block(&self, text: &str, system: bool) -> Value {
        let body = text
            .trim_start()
            .strip_prefix("<system-reminder>")
            .unwrap_or(text)
            .trim_start();
        let first: String = body
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(FIRST_LINE_CHARS)
            .collect();
        let mut digest = digest_text(text);
        digest["first_line"] = Value::String(self.text(&first));
        let key = if system { "system_message" } else { "system_reminder" };
        json!({ key: digest })
    }

    /// One line of the CLI's transcript. Besides the plain rules: a string that is a system reminder becomes a
    /// digest; an attachment of any type but `queued_command` (the CLI's own notes to itself: its attribution
    /// settings, system prompt, tool announcements) and the lines the CLI keeps of its own API requests keep
    /// their type, ids and shape and nothing else of their prose.
    pub fn transcript_line(&self, line: &Value) -> Value {
        let scrubbed = self.value(line);
        let mut out = digest_reminders(&scrubbed);
        let kind = line["type"].as_str().unwrap_or_default();
        let own_note = kind == "attachment" && line["attachment"]["type"] != "queued_command";
        if let Some(object) = out.as_object_mut() {
            if own_note {
                for key in ["attachment", "rendered"] {
                    if let Some(value) = object.get_mut(key) {
                        *value = digest_strings(value, "", &["type"]);
                    }
                }
            } else if TRANSCRIPT_REQUEST_LINES.contains(&kind) {
                for (key, value) in object.iter_mut() {
                    *value = digest_strings(value, key, TRANSCRIPT_REQUEST_KEEP);
                }
            }
        }
        out
    }

    /// The pieces one streamed block was sent in, scrubbed as the text they join into: a run path the stream cut
    /// in two matches no rule piece by piece. When the pieces scrubbed one by one already join into the scrubbed
    /// whole they keep their own cuts; otherwise the scrubbed whole is cut again into as many pieces.
    pub fn stream_pieces(&self, pieces: &[&str]) -> Vec<String> {
        let each: Vec<String> = pieces.iter().map(|p| self.text(p)).collect();
        let whole = self.text(&pieces.concat());
        if each.concat() == whole {
            each
        } else {
            cut_evenly(&whole, pieces.len())
        }
    }

    /// One line of the stand-in's endpoint log, every string scrubbed, and each streamed block of its answer
    /// scrubbed whole ([`Self::stream_pieces`]).
    pub fn endpoint_line(&self, line: &Value) -> Value {
        let mut out = self.value(line);
        let Some(events) = line.get("response_events").and_then(Value::as_array) else {
            return out;
        };
        let mut blocks: BTreeMap<(u64, &'static str), Vec<usize>> = BTreeMap::new();
        for (i, event) in events.iter().enumerate() {
            if let Some(key) = stream_delta(&event["data"]) {
                blocks.entry(key).or_default().push(i);
            }
        }
        for ((_, field), at) in blocks {
            let pieces: Vec<&str> = at
                .iter()
                .map(|&i| events[i]["data"]["delta"][field].as_str().unwrap_or_default())
                .collect();
            for (&i, piece) in at.iter().zip(self.stream_pieces(&pieces)) {
                out["response_events"][i]["data"]["delta"][field] = Value::String(piece);
            }
        }
        out
    }

    /// A whole tap log, each line as [`Self::tap_line`] scrubs it, and the CLI's relayed stream events of one
    /// block (`stream_event` lines, by parent tool use and block index, up to the block's stop) scrubbed whole as
    /// [`Self::stream_pieces`] does, so they read as the endpoint log's pieces of the same block.
    pub fn tap_lines(&self, lines: &[Value]) -> Vec<Value> {
        let mut out: Vec<Value> = lines.iter().map(|l| self.tap_line(l)).collect();
        let parsed: Vec<Option<Value>> = lines
            .iter()
            .map(|l| l["line"].as_str().and_then(|s| serde_json::from_str(s).ok()))
            .collect();
        type Key = (String, u64, &'static str);
        let mut open: BTreeMap<Key, Vec<usize>> = BTreeMap::new();
        let mut blocks: Vec<(&'static str, Vec<usize>)> = Vec::new();
        for (i, line) in parsed.iter().enumerate() {
            let Some(line) = line.as_ref().filter(|l| l["type"] == "stream_event") else {
                continue;
            };
            let parent = line["parent_tool_use_id"].to_string();
            let event = &line["event"];
            if let Some((index, field)) = stream_delta(event) {
                open.entry((parent, index, field)).or_default().push(i);
                continue;
            }
            let ends = match event["type"].as_str() {
                Some("content_block_start" | "content_block_stop") => event["index"].as_u64(),
                Some("message_start" | "message_stop") => None,
                _ => continue,
            };
            let done: Vec<Key> = open
                .keys()
                .filter(|k| k.0 == parent && ends.is_none_or(|index| k.1 == index))
                .cloned()
                .collect();
            for key in done {
                let at = open.remove(&key).expect("a key just listed");
                blocks.push((key.2, at));
            }
        }
        blocks.extend(open.into_iter().map(|(key, at)| (key.2, at)));
        for (field, at) in blocks {
            let pieces: Vec<&str> = at
                .iter()
                .map(|&i| {
                    parsed[i].as_ref().expect("parsed")["event"]["delta"][field]
                        .as_str()
                        .unwrap_or_default()
                })
                .collect();
            for (&i, piece) in at.iter().zip(self.stream_pieces(&pieces)) {
                let Some(Ok(mut inner)) = out[i]["line"].as_str().map(serde_json::from_str::<Value>) else {
                    continue;
                };
                inner["event"]["delta"][field] = Value::String(piece);
                out[i]["line"] = Value::String(inner.to_string());
            }
        }
        out
    }

    /// One tap-log line (`{index, time, line}`) with its `line` scrubbed as JSON when it is JSON.
    pub fn tap_line(&self, entry: &Value) -> Value {
        let Some(object) = entry.as_object() else {
            return self.value(entry);
        };
        Value::Object(
            object
                .iter()
                .map(|(k, v)| {
                    let v = match (k.as_str(), v.as_str()) {
                        ("line", Some(line)) => Value::String(match serde_json::from_str::<Value>(line) {
                            Ok(parsed) => digest_reminders(&self.value(&parsed)).to_string(),
                            Err(_) => self.text(line),
                        }),
                        _ => self.value(v),
                    };
                    (k.clone(), v)
                })
                .collect(),
        )
    }
}

/// The block index and the text field of a `content_block_delta` event that carries streamed text.
fn stream_delta(event: &Value) -> Option<(u64, &'static str)> {
    if event["type"] != "content_block_delta" {
        return None;
    }
    let field = match event["delta"]["type"].as_str()? {
        "input_json_delta" => "partial_json",
        "text_delta" => "text",
        "thinking_delta" => "thinking",
        _ => return None,
    };
    event["delta"][field].is_string().then_some(())?;
    Some((event["index"].as_u64()?, field))
}

/// `text` cut into `n` pieces whose lengths in chars differ by at most one, the longer ones last (two pieces cut
/// where the stand-in cuts a tool input: at half the char count).
fn cut_evenly(text: &str, n: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let n = n.max(1);
    (0..n)
        .map(|k| chars[k * chars.len() / n..(k + 1) * chars.len() / n].iter().collect())
        .collect()
}

/// Every string of `value` that is a system reminder, replaced by [`cli_text`], and every reminder embedded in a
/// longer string (the CLI quotes them inside its compaction summary, for one) replaced in place by the same.
fn digest_reminders(value: &Value) -> Value {
    static EMBEDDED: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"(?s)<system-reminder>.*?</system-reminder>").expect("fixed pattern"));
    match value {
        Value::String(s) if is_reminder(s) => Value::String(cli_text(s)),
        Value::String(s) if s.contains("<system-reminder>") => Value::String(
            EMBEDDED
                .replace_all(s, |c: &regex::Captures| cli_text(&c[0]))
                .into_owned(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(digest_reminders).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), digest_reminders(v))).collect()),
        other => other.clone(),
    }
}

/// Every string of `value` but those under a key in `keep` replaced by [`cli_text`].
fn digest_strings(value: &Value, key: &str, keep: &[&str]) -> Value {
    match value {
        Value::String(s) if !keep.contains(&key) && s.chars().count() > TWIN_SHORT => Value::String(cli_text(s)),
        Value::Array(items) => Value::Array(items.iter().map(|v| digest_strings(v, key, keep)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), digest_strings(v, k, keep)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Transcript lines in which the CLI records the requests it made, system prompt and all.
const TRANSCRIPT_REQUEST_LINES: &[&str] = &["api-request-shape", "api-request-blob", "api-request"];

/// What those lines keep of their strings: ids, digests the CLI computed itself and the line's kind.
const TRANSCRIPT_REQUEST_KEEP: &[&str] = &[
    "type",
    "sessionId",
    "hash",
    "id",
    "uuid",
    "version",
    "timestamp",
    "shapeHash",
    "requestDigest",
    "querySource",
    "role",
];

fn is_reminder(text: &str) -> bool {
    text.trim_start().starts_with("<system-reminder>")
}

/// `sha256` of the text in lowercase hex.
pub fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `{sha256, chars}` of a text.
pub fn digest_text(text: &str) -> Value {
    json!({"sha256": sha256_hex(text), "chars": text.chars().count()})
}

/// `{sha256, chars}` of a value: of the text itself for a string, of its compact JSON otherwise.
pub fn digest_value(value: &Value) -> Value {
    match value {
        Value::String(text) => digest_text(text),
        other => digest_text(&other.to_string()),
    }
}

fn tool_list(tools: &Value) -> Value {
    let Some(list) = tools.as_array() else {
        return json!([]);
    };
    Value::Array(
        list.iter()
            .map(|tool| {
                let name = tool["name"].as_str().or_else(|| tool["type"].as_str()).unwrap_or("?");
                json!({"name": name, "deferred": tool["defer_loading"] == true})
            })
            .collect(),
    )
}

// ---- files ----

fn write_jsonl(path: &Path, lines: &[Value]) -> io::Result<()> {
    let mut text = String::new();
    for line in lines {
        text.push_str(&line.to_string());
        text.push('\n');
    }
    fs::write(path, text)
}

fn read_jsonl(path: &Path) -> io::Result<Vec<Value>> {
    Ok(fs::read_to_string(path)?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|_| Value::String(l.to_owned())))
        .collect())
}

/// Rewrites a JSON-lines file in place with every string scrubbed.
pub fn scrub_jsonl_in_place(scrubber: &Scrubber, path: &Path) -> io::Result<()> {
    let lines: Vec<Value> = read_jsonl(path)?.iter().map(|l| scrubber.value(l)).collect();
    write_jsonl(path, &lines)
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// Where the CLI keeps its transcripts: `<run>/claude/projects/<project>/...`.
fn transcript_files(run: &Path) -> Vec<(PathBuf, PathBuf)> {
    let projects = run.join("claude/projects");
    let mut found = Vec::new();
    let mut slugs: Vec<PathBuf> = fs::read_dir(&projects)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    slugs.sort();
    for slug in slugs.iter().filter(|p| p.is_dir()) {
        let mut files = Vec::new();
        collect_files(slug, &mut files);
        for file in files {
            let rel = file.strip_prefix(slug).expect("under the project directory").to_owned();
            found.push((file, rel));
        }
    }
    found
}

/// Writes the scrubbed form of the run's tap logs, request log with its bodies, and CLI transcript into `out`:
/// `cli-stdin.jsonl`, `cli-stdout.jsonl` (every line keeps its arrival index and time), `endpoint.jsonl` with
/// `body_<n>.json` beside it, and `transcript/<path under the project directory>`. Returns the files written,
/// relative to `out`, sorted. A file the run did not produce is not an error.
pub fn scrub_run(scrubber: &Scrubber, run: &Path, out: &Path) -> io::Result<Vec<String>> {
    fs::create_dir_all(out)?;
    let mut written = BTreeSet::new();
    for name in ["cli-stdin.jsonl", "cli-stdout.jsonl"] {
        let src = run.join("log").join(name);
        if src.is_file() {
            let lines = scrubber.tap_lines(&read_jsonl(&src)?);
            write_jsonl(&out.join(name), &lines)?;
            written.insert(name.to_owned());
        }
    }
    let standin = run.join("standin");
    let endpoint = standin.join("endpoint.jsonl");
    if endpoint.is_file() {
        let lines: Vec<Value> = read_jsonl(&endpoint)?
            .iter()
            .map(|l| scrubber.endpoint_line(l))
            .collect();
        write_jsonl(&out.join("endpoint.jsonl"), &lines)?;
        written.insert("endpoint.jsonl".to_owned());
        let mut bodies: Vec<PathBuf> = fs::read_dir(&standin)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("body_") && n.ends_with(".json"))
            })
            .collect();
        bodies.sort();
        for body in bodies {
            let name = body.file_name().expect("a file name").to_string_lossy().into_owned();
            let scrubbed = match serde_json::from_str::<Value>(&fs::read_to_string(&body)?) {
                Ok(parsed) => scrubber.request_body(&parsed),
                Err(_) => json!({"unparsed": true}),
            };
            let text = serde_json::to_string_pretty(&scrubbed).map_err(io::Error::other)? + "\n";
            fs::write(out.join(&name), text)?;
            written.insert(name);
        }
    }
    for (file, rel) in transcript_files(run) {
        let dest_rel = Path::new("transcript").join(&rel);
        let dest = out.join(&dest_rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        match file.extension().and_then(|e| e.to_str()) {
            Some("jsonl") => {
                let lines: Vec<Value> = read_jsonl(&file)?.iter().map(|l| scrubber.transcript_line(l)).collect();
                write_jsonl(&dest, &lines)?;
            }
            Some("json") => {
                let parsed: Value = serde_json::from_str(&fs::read_to_string(&file)?).unwrap_or(Value::Null);
                let text = serde_json::to_string_pretty(&scrubber.value(&parsed)).map_err(io::Error::other)? + "\n";
                fs::write(&dest, text)?;
            }
            _ => continue,
        }
        written.insert(dest_rel.to_string_lossy().into_owned());
    }
    Ok(written.into_iter().collect())
}

/// Every file under `dir`, relative and sorted, `manifest.json` left out.
pub fn listing(dir: &Path) -> Vec<String> {
    let mut files = Vec::new();
    collect_files(dir, &mut files);
    let mut names: Vec<String> = files
        .iter()
        .filter_map(|f| f.strip_prefix(dir).ok())
        .map(|f| f.to_string_lossy().into_owned())
        .filter(|f| f != "manifest.json")
        .collect();
    names.sort();
    names
}

/// The Agent SDK version in the Verdandi checkout's lock file, when the run has one in view.
pub fn sdk_version() -> Option<String> {
    let checkout = std::env::var_os("EITRI_VERDANDI_CHECKOUT")?;
    let lock: Value =
        serde_json::from_str(&fs::read_to_string(Path::new(&checkout).join("package-lock.json")).ok()?).ok()?;
    lock["packages"]["node_modules/@anthropic-ai/claude-agent-sdk"]["version"]
        .as_str()
        .map(str::to_owned)
}

/// Scrubs a run directory that no test binary recorded (a command run in the sandbox) into `out` and writes its
/// `manifest.json` from `run.json` and the tap's record of the CLI's environment. Returns the files, manifest
/// included.
pub fn scrub_exec_run(scrubber: &Scrubber, run: &Path, out: &Path) -> io::Result<Vec<String>> {
    scrub_run(scrubber, run, out)?;
    let info: Value = fs::read_to_string(run.join("run.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    let env_names: Vec<String> = fs::read_to_string(run.join("log/cli-env.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<Map<String, Value>>(&t).ok())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    let manifest = scrubber.value(&json!({
        "scenario": info["scenario"],
        "layer": "exec",
        "cli_version": info["cli_version"],
        "cli_sha256": info["cli_sha256"],
        "sdk_version": sdk_version(),
        "verdandi_revision": info["verdandi_revision"],
        "eitri_revision": info["eitri_revision"],
        "mode": "exec",
        "env_names": env_names,
        "user_settings": info["user_settings"],
        "facts": {},
        "files": listing(out),
    }));
    fs::write(
        out.join("manifest.json"),
        serde_json::to_string_pretty(&manifest).map_err(io::Error::other)? + "\n",
    )?;
    let mut files = listing(out);
    files.push("manifest.json".into());
    files.sort();
    Ok(files)
}

// ---- twins ----

/// Keys whose string values are ids or words of the wire's own vocabulary: they stay as they are.
const TWIN_KEEP_KEYS: &[&str] = &[
    "type",
    "kind",
    "outcome",
    "name",
    "tool_name",
    "tool_use_id",
    "turn_id",
    "permission_id",
    "client_message_id",
    "session_id",
    "provider_session_id",
    "requested_provider_session_id",
    "attached_provider_session_id",
    "model",
    "reported",
    "stop_reason",
    "status",
    "mode",
    "provider_mode",
    "origin",
    "decision",
    "task_id",
    "id",
    "uuid",
    "command_id",
    "message_id",
    "source",
    "subtype",
    "role",
    "reason_type",
];

/// CLI enum words that stay readable wherever they appear: task status and kind, stop reasons, turn and close
/// reasons.
pub const TWIN_ENUM_WORDS: &[&str] = &[
    "completed",
    "failed",
    "error",
    "cancelled",
    "canceled",
    "interrupted",
    "killed",
    "stopped",
    "running",
    "pending",
    "done",
    "timeout",
    "timed_out",
    "end_turn",
    "max_tokens",
    "stop_sequence",
    "tool_use",
    "pause_turn",
    "refusal",
    "discarded",
    "refused",
    "closed_by_host",
    "closed_by_provider",
    "provider_exit",
    "allowed",
    "denied",
    "deferred",
    "background",
    "foreground",
    "local_bash",
    "local_agent",
    "local_workflow",
    "remote_agent",
    "text",
    "thinking",
    "default",
    "auto",
    "bypass",
    "plan",
    "acceptEdits",
    "dontAsk",
    "success",
    "unknown",
];

/// Strings of at most this many characters carry no prose worth hiding.
const TWIN_SHORT: usize = 3;

/// A CLI-authored string as the twin shows it: its length and the first 16 hex digits of its digest, short
/// enough to pass the panel's bounds on a command's hint.
pub fn cli_text(scrubbed: &str) -> String {
    format!(
        "<cli-text chars={} sha256={}>",
        scrubbed.chars().count(),
        &sha256_hex(scrubbed)[..16]
    )
}

fn collect_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Array(items) => items.iter().for_each(|v| collect_strings(v, out)),
        Value::Object(map) => {
            for (k, v) in map {
                out.push(k.clone());
                collect_strings(v, out);
            }
        }
        _ => {}
    }
}

struct Corpus(Vec<String>);

impl Corpus {
    fn of(scenario: &Value) -> Self {
        let mut strings = Vec::new();
        collect_strings(scenario, &mut strings);
        strings.retain(|s| !s.is_empty());
        Corpus(strings)
    }

    /// The scenario wrote this text, or a piece of it.
    fn says(&self, text: &str) -> bool {
        self.0.iter().any(|s| s.contains(text))
    }
}

fn twin_value(scrubber: &Scrubber, corpus: &Corpus, key: &str, value: &Value) -> Value {
    match value {
        Value::String(raw) => {
            let scrubbed = scrubber.text(raw);
            let keep = TWIN_KEEP_KEYS.contains(&key)
                || raw.chars().count() <= TWIN_SHORT
                || TWIN_ENUM_WORDS.contains(&raw.as_str())
                || corpus.says(raw)
                || (scrubbed.starts_with(RUN) && !scrubbed.contains(char::is_whitespace));
            Value::String(if keep { scrubbed } else { cli_text(&scrubbed) })
        }
        Value::Array(items) => Value::Array(items.iter().map(|v| twin_value(scrubber, corpus, key, v)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (scrubber.text(k), twin_value(scrubber, corpus, k, v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The twin of a scenario: its expected domain events with every string the scenario did not write replaced by
/// [`cli_text`]. `scenario` is the scenario file; what it says (prompts, replies, tool inputs, commands) is the
/// only free text that stays.
pub fn twin(scrubber: &Scrubber, scenario_id: &str, scenario: &Value, events: &[Value]) -> Value {
    let corpus = Corpus::of(scenario);
    json!({
        "scenario": scenario_id,
        "domain_events": events.iter().map(|e| twin_value(scrubber, &corpus, "", e)).collect::<Vec<_>>(),
    })
}

/// Probes answer questions about the CLI and are not folded by the panel: they get no twin.
pub fn wants_twin(scenario_id: &str) -> bool {
    !scenario_id.starts_with("probe_")
}

/// Writes `<twins_dir>/<scenario>.json` from the record directory's `scenario.json` and `expected-events.json`.
pub fn write_twin(scrubber: &Scrubber, scenario_id: &str, record_dir: &Path, twins_dir: &Path) -> io::Result<()> {
    let scenario: Value = serde_json::from_str(&fs::read_to_string(record_dir.join("scenario.json"))?)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("scenario.json: {e}")))?;
    let events: Vec<Value> = serde_json::from_str(&fs::read_to_string(record_dir.join("expected-events.json"))?)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("expected-events.json: {e}")))?;
    fs::create_dir_all(twins_dir)?;
    let text = serde_json::to_string_pretty(&twin(scrubber, scenario_id, &scenario, &events))
        .map_err(io::Error::other)?
        + "\n";
    fs::write(twins_dir.join(format!("{scenario_id}.json")), text)
}

/// Rewrites the wire recording and the expected events, which the replay compares as they are, where the scan
/// would refuse them: the CLI's per-user temporary directory, `/tmp/claude-<uid>`, becomes `/tmp/claude-$UID`
/// padded or cut to the same length, so the length-delimited frames stay whole.
pub fn scrub_wire_files(record_dir: &Path) -> io::Result<()> {
    let re = regex::bytes::Regex::new(UID_DIR).expect("fixed pattern");
    for entry in fs::read_dir(record_dir)?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let wire = name.starts_with("proto") && (name.ends_with(".bin") || name.ends_with(".txt"));
        if !(wire || name == "expected-events.json") || !entry.path().is_file() {
            continue;
        }
        let bytes = fs::read(entry.path())?;
        let scrubbed = re.replace_all(&bytes, |c: &regex::bytes::Captures| {
            let whole = &c[0];
            let digits = whole.len() - c[1].len();
            let mut token = c[1].to_vec();
            token.extend(b"$UID".iter().cycle().take(digits));
            token
        });
        if scrubbed.as_ref() != bytes.as_slice() {
            fs::write(entry.path(), scrubbed.as_ref())?;
        }
    }
    Ok(())
}

/// Scrubs everything a recording test left under `record_dir` besides the wire recording and the expected
/// events (which the replay compares as they are): the run's logs and transcript are added, `outgoing.jsonl` is
/// rewritten scrubbed.
pub fn scrub_record(scrubber: &Scrubber, run: &Path, record_dir: &Path) -> io::Result<()> {
    scrub_run(scrubber, run, record_dir)?;
    let outgoing = record_dir.join("outgoing.jsonl");
    if outgoing.is_file() {
        scrub_jsonl_in_place(scrubber, &outgoing)?;
    }
    scrub_delivered_text(scrubber, &record_dir.join("expected-events.json"))
}

/// The replay fills a steer's delivered text in from the scrubbed `outgoing.jsonl`, so the text of every
/// `user_message_delivered` event in the expected events gets the same rules; the rest of the file is left as it
/// is. A file that is missing, or that changes nothing, is not rewritten.
fn scrub_delivered_text(scrubber: &Scrubber, path: &Path) -> io::Result<()> {
    let Ok(text) = fs::read_to_string(path) else {
        return Ok(());
    };
    let mut events: Value = serde_json::from_str(&text)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("expected-events.json: {e}")))?;
    let mut changed = false;
    for event in events.as_array_mut().into_iter().flatten() {
        if event["type"] != "user_message_delivered" {
            continue;
        }
        if let Some(delivered) = event["text"].as_str() {
            let scrubbed = scrubber.text(delivered);
            if scrubbed != delivered {
                event["text"] = Value::String(scrubbed);
                changed = true;
            }
        }
    }
    if changed {
        fs::write(
            path,
            serde_json::to_string_pretty(&events).map_err(io::Error::other)? + "\n",
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn who() -> Identity {
        Identity {
            home: Some("/home/alice".into()),
            users: vec!["alice".into()],
            hosts: vec!["buildbox".into()],
            keys: vec!["sk-test-0123456789".into()],
        }
    }

    fn scrubber() -> Scrubber {
        Scrubber::new(Path::new("/mnt/eitri-runs/ch/ab12cd34/c00"), &who())
    }

    #[test]
    fn the_run_directory_a_home_the_names_and_the_key_are_replaced_in_text() {
        let s = scrubber();
        let anthropic_key = format!("sk-ant-{}", "api03-abcdefghij");
        let text = format!(
            "cd /mnt/eitri-runs/ch/ab12cd34/c00/project; ls /home/alice/.config /home/bob/x; whoami=alice \
             host=buildbox key=sk-test-0123456789 also {anthropic_key}"
        );
        let out = s.text(&text);
        assert_eq!(
            out,
            "cd $RUN/project; ls $RUN/.config $RUN/x; whoami=$USER host=$HOST key=$KEY also $KEY"
        );
        assert_eq!(s.text(&out), out, "scrubbing again changes nothing");
    }

    #[test]
    fn the_project_directory_name_the_cli_derives_from_the_run_path_is_replaced() {
        let out = scrubber().text("projects/-mnt-eitri-runs-ch-ab12cd34-c00-project/s.jsonl");
        assert_eq!(out, "projects/-RUN-project/s.jsonl");
    }

    #[test]
    fn a_name_is_replaced_only_as_a_whole_word_and_common_words_are_left_alone() {
        let s = Scrubber::new(
            Path::new("/mnt/eitri-runs/ch/ab12cd34/c00"),
            &Identity {
                users: vec!["alice".into(), "root".into(), "al".into()],
                ..Identity::default()
            },
        );
        assert_eq!(
            s.text("alice, alicephone, root cause, al"),
            "$USER, alicephone, root cause, al"
        );
    }

    #[test]
    fn a_home_path_inside_the_run_directory_is_not_replaced_twice() {
        let out = scrubber().text("/mnt/eitri-runs/ch/ab12cd34/c00/home/alice/.cache");
        assert_eq!(out, "$RUN/home/$USER/.cache");
    }

    #[test]
    fn object_keys_are_scrubbed_with_the_values() {
        let out = scrubber().value(&json!({"/home/alice/a.txt": ["alice", 3, null]}));
        assert_eq!(out, json!({"$RUN/a.txt": ["$USER", 3, null]}));
    }

    fn body() -> Value {
        json!({
            "model": "claude-opus-5-5",
            "stream": true,
            "max_tokens": 32000,
            "metadata": {"user_id": "device-abc"},
            "thinking": {"type": "adaptive"},
            "system": [{"type": "text", "text": "You are Claude Code in /home/alice"}],
            "tools": [
                {"name": "Bash", "description": "run a command", "input_schema": {}},
                {"name": "WebFetch", "defer_loading": true, "input_schema": {}}
            ],
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "<system-reminder>\nAttribution for commits at /home/alice\nmore\n</system-reminder>\n"},
                    {"type": "text", "text": "list /home/alice please"}
                ]},
                {"role": "system", "content": [{"type": "text", "text": "# Environment\nPrimary working directory: /home/alice"}]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "cat /home/alice/x"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": [
                        {"type": "text", "text": "out for alice"},
                        {"type": "text", "text": "<system-reminder>Task tools are idle</system-reminder>"}
                    ]}
                ]}
            ]
        })
    }

    #[test]
    fn a_request_body_keeps_its_conversation_and_digests_the_rest() {
        let out = scrubber().request_body(&body());
        assert_eq!(out["model"], "claude-opus-5-5");
        assert_eq!(out["stream"], true);
        assert_eq!(out["max_tokens"], 32000);
        let system_text = body()["system"].to_string();
        assert_eq!(
            out["system"],
            json!({"sha256": sha256_hex(&system_text), "chars": system_text.chars().count()})
        );
        assert_eq!(
            out["tools"],
            json!([{"name": "Bash", "deferred": false}, {"name": "WebFetch", "deferred": true}])
        );
        assert_eq!(out["tools_sha256"], sha256_hex(&body()["tools"].to_string()));
        assert_eq!(out["dropped_fields"], json!(["metadata", "thinking"]));
        assert!(out.get("metadata").is_none() && out.get("thinking").is_none());

        let user = &out["messages"][0]["content"];
        let reminder = &user[0]["system_reminder"];
        assert_eq!(reminder["first_line"], "Attribution for commits at $RUN");
        assert_eq!(
            reminder["chars"],
            body()["messages"][0]["content"][0]["text"]
                .as_str()
                .unwrap()
                .chars()
                .count()
        );
        assert_eq!(user[1]["text"], "list $RUN please");
        assert!(out["messages"][1]["content"][0]["system_message"]["sha256"].is_string());
        assert_eq!(out["messages"][2]["content"][0]["input"]["command"], "cat $RUN/x");
        assert_eq!(out["messages"][2]["content"][0]["id"], "toolu_1");
        let result = &out["messages"][3]["content"][0]["content"];
        assert_eq!(result[0]["text"], "out for $USER");
        assert!(result[1]["system_reminder"]["sha256"].is_string());
        assert!(!out.to_string().contains("alice"), "{out}");
    }

    #[test]
    fn a_body_that_is_not_an_object_is_digested() {
        let out = scrubber().request_body(&json!("plain"));
        assert!(out["unparsed"]["sha256"].is_string());
    }

    #[test]
    fn a_tap_line_keeps_its_index_and_time_and_scrubs_the_json_inside() {
        let entry = json!({"index": 7, "time": 1.5, "line": r#"{"cwd":"/home/alice/p","n":1}"#});
        let out = scrubber().tap_line(&entry);
        assert_eq!(out["index"], 7);
        assert_eq!(out["time"], 1.5);
        let inner: Value = serde_json::from_str(out["line"].as_str().unwrap()).unwrap();
        assert_eq!(inner, json!({"cwd": "$RUN/p", "n": 1}));
        let raw = scrubber().tap_line(&json!({"index": 8, "time": 2.0, "line": "not json alice"}));
        assert_eq!(raw["line"], "not json $USER");
    }

    #[test]
    fn a_reminder_inside_a_longer_string_of_a_tap_line_is_digested() {
        let inner = json!({"message": {"content": "Summary.\n\n<system-reminder>\nTools in /home/alice\n</system-reminder>\nGo on."}});
        let out = scrubber().tap_line(&json!({"index": 3, "time": 0.5, "line": inner.to_string()}));
        let parsed: Value = serde_json::from_str(out["line"].as_str().unwrap()).unwrap();
        let reminder = "<system-reminder>\nTools in $RUN\n</system-reminder>";
        assert_eq!(
            parsed["message"]["content"],
            format!("Summary.\n\n{}\nGo on.", cli_text(reminder))
        );
        assert!(!out["line"].as_str().unwrap().contains("Tools in"));
    }

    #[test]
    fn a_delivered_steer_reads_the_same_in_the_events_and_in_the_outgoing_log() {
        let run = temp("steer-run");
        let record = temp("steer-record");
        let steer = "look at /home/alice/notes.txt, alice";
        fs::write(
            record.join("outgoing.jsonl"),
            json!({"rpc": "steer_turn", "error": null, "text": steer}).to_string() + "\n",
        )
        .unwrap();
        let events = json!([
            {"type": "user_message_delivered", "text": steer, "client_message_id": "msg-2"},
            {"type": "content_delta", "text": "kept /home/alice as it is"},
        ]);
        fs::write(record.join("expected-events.json"), events.to_string()).unwrap();
        scrub_record(&scrubber(), &run, &record).unwrap();
        let outgoing: Value =
            serde_json::from_str(fs::read_to_string(record.join("outgoing.jsonl")).unwrap().trim()).unwrap();
        let after: Vec<Value> =
            serde_json::from_str(&fs::read_to_string(record.join("expected-events.json")).unwrap()).unwrap();
        assert_eq!(outgoing["text"], "look at $RUN/notes.txt, $USER");
        assert_eq!(after[0]["text"], outgoing["text"]);
        assert_eq!(
            after[1]["text"], "kept /home/alice as it is",
            "other events are compared as they are"
        );
        fs::remove_dir_all(&run).unwrap();
        fs::remove_dir_all(&record).unwrap();
    }

    #[test]
    fn a_run_path_the_stream_cut_in_two_is_scrubbed_in_the_endpoint_log() {
        let s = scrubber();
        let delta = |index: u64, piece: &str| {
            json!({"event": "content_block_delta", "data": {"type": "content_block_delta", "index": index,
                "delta": {"type": "input_json_delta", "partial_json": piece}}})
        };
        let line = json!({"arrival_index": 3, "response_events": [
            delta(0, r#"{"file_path":"/mnt/eitri-runs/ch"#),
            delta(0, r#"/ab12cd34/c00/project/a.txt"}"#),
            delta(1, r#"{"command":"echo"#),
            delta(1, r#" one"}"#),
        ]});
        let out = s.endpoint_line(&line);
        let pieces = |index: u64| -> Vec<String> {
            out["response_events"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["data"]["index"] == index)
                .map(|e| e["data"]["delta"]["partial_json"].as_str().unwrap().to_owned())
                .collect()
        };
        let path = pieces(0);
        assert_eq!(path.len(), 2, "as many pieces as were sent");
        assert_eq!(path.concat(), r#"{"file_path":"$RUN/project/a.txt"}"#);
        assert!(!out.to_string().contains("eitri-runs"), "{out}");
        assert_eq!(
            pieces(1),
            [r#"{"command":"echo"#, r#" one"}"#],
            "a block with nothing to scrub keeps its cuts"
        );
    }

    #[test]
    fn a_run_path_the_stream_cut_in_two_is_scrubbed_in_the_relayed_stream_events() {
        let s = scrubber();
        let tap = |i: u64, inner: Value| json!({"index": i, "time": 0.0, "line": inner.to_string()});
        let event = |event: Value| json!({"type": "stream_event", "parent_tool_use_id": null, "event": event});
        let delta = |piece: &str| {
            event(json!({"type": "content_block_delta", "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": piece}}))
        };
        let lines = [
            tap(0, event(json!({"type": "content_block_start", "index": 0}))),
            tap(1, delta(r#"{"file_path":"/mnt/eitri-runs/ch"#)),
            tap(2, json!({"type": "system", "subtype": "status"})),
            tap(3, delta(r#"/ab12cd34/c00/project/a.txt"}"#)),
            tap(4, event(json!({"type": "content_block_stop", "index": 0}))),
            tap(5, delta(r#"{"x":"/mnt/eitri-runs/ch/ab12cd34/c00/b"}"#)),
        ];
        let out = s.tap_lines(&lines);
        let piece = |i: usize| -> String {
            let inner: Value = serde_json::from_str(out[i]["line"].as_str().unwrap()).unwrap();
            inner["event"]["delta"]["partial_json"].as_str().unwrap().to_owned()
        };
        assert_eq!(piece(1) + &piece(3), r#"{"file_path":"$RUN/project/a.txt"}"#);
        assert_eq!(piece(5), r#"{"x":"$RUN/b"}"#, "a block after the stop is its own");
        assert_eq!(
            out.iter().map(|l| l["index"].as_u64().unwrap()).collect::<Vec<_>>(),
            [0, 1, 2, 3, 4, 5]
        );
        let text = Value::Array(out.clone()).to_string();
        assert!(!text.contains("eitri-runs"), "{text}");
    }

    #[test]
    fn a_cut_again_text_keeps_its_piece_count_and_chars() {
        assert_eq!(cut_evenly("abcde", 2), ["ab", "cde"]);
        assert_eq!(cut_evenly("éèê", 2), ["é", "èê"]);
        assert_eq!(cut_evenly("", 2), ["", ""]);
    }

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("scrub-test-{tag}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fake_run(root: &Path) {
        for sub in ["log", "standin", "claude/projects/-slug/s1/subagents"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        let tap = |i: u64, line: &str| json!({"index": i, "time": 0.1 * i as f64, "line": line}).to_string();
        fs::write(
            root.join("log/cli-stdin.jsonl"),
            [tap(0, r#"{"a":"/home/alice"}"#), tap(2, "plain")].join("\n") + "\n",
        )
        .unwrap();
        fs::write(root.join("log/cli-stdout.jsonl"), tap(1, r#"{"b":"alice"}"#) + "\n").unwrap();
        fs::write(
            root.join("standin/endpoint.jsonl"),
            json!({"arrival_index": 1, "body_file": "body_1.json", "path": "/home/alice"}).to_string() + "\n",
        )
        .unwrap();
        fs::write(root.join("standin/body_1.json"), body().to_string()).unwrap();
        fs::write(root.join("standin/requests.jsonl"), "{}\n").unwrap();
        fs::write(
            root.join("claude/projects/-slug/s1.jsonl"),
            json!({"cwd": "/home/alice", "type": "user"}).to_string() + "\n",
        )
        .unwrap();
        fs::write(
            root.join("claude/projects/-slug/s1/subagents/a.meta.json"),
            r#"{"who":"alice"}"#,
        )
        .unwrap();
        fs::write(
            root.join("claude/projects/-slug/s1/subagents/a.jsonl"),
            "{\"x\":\"buildbox\"}\n",
        )
        .unwrap();
    }

    #[test]
    fn a_run_is_scrubbed_into_a_scenario_directory_that_keeps_every_arrival_index() {
        let run = temp("run");
        let out = temp("out");
        fake_run(&run);
        let files = scrub_run(&scrubber(), &run, &out).unwrap();
        assert_eq!(
            files,
            [
                "body_1.json",
                "cli-stdin.jsonl",
                "cli-stdout.jsonl",
                "endpoint.jsonl",
                "transcript/s1.jsonl",
                "transcript/s1/subagents/a.jsonl",
                "transcript/s1/subagents/a.meta.json",
            ]
        );
        assert_eq!(files, listing(&out), "the files named are the files held");
        let indexes = |name: &str| -> Vec<u64> {
            read_jsonl(&out.join(name))
                .unwrap()
                .iter()
                .map(|l| l["index"].as_u64().unwrap())
                .collect()
        };
        assert_eq!(indexes("cli-stdin.jsonl"), [0, 2]);
        assert_eq!(indexes("cli-stdout.jsonl"), [1]);
        for name in &files {
            let text = fs::read_to_string(out.join(name)).unwrap();
            assert!(
                !text.contains("alice") && !text.contains("buildbox") && !text.contains("/home/"),
                "{name}: {text}"
            );
        }
        let endpoint = read_jsonl(&out.join("endpoint.jsonl")).unwrap();
        assert_eq!(endpoint[0]["path"], "$RUN");
        assert_eq!(
            endpoint[0]["body_file"], "body_1.json",
            "the body sits beside the log under the name the log gives"
        );
        let body: Value = serde_json::from_str(&fs::read_to_string(out.join("body_1.json")).unwrap()).unwrap();
        assert!(body["system"]["sha256"].is_string());
        fs::remove_dir_all(&run).unwrap();
        fs::remove_dir_all(&out).unwrap();
    }

    #[test]
    fn an_exec_run_gets_a_manifest_that_names_exactly_the_files_beside_it() {
        let run = temp("exec-run");
        let out = temp("exec-out");
        fake_run(&run);
        fs::write(
            run.join("run.json"),
            json!({"scenario": "x", "cli_version": "2.1.311", "cli_sha256": "ab", "verdandi_revision": "785a18a",
                   "eitri_revision": "e1", "user_settings": null})
            .to_string(),
        )
        .unwrap();
        fs::write(
            run.join("log/cli-env.json"),
            r#"{"ANTHROPIC_API_KEY":"present","HOME":"/home/alice"}"#,
        )
        .unwrap();
        let files = scrub_exec_run(&scrubber(), &run, &out).unwrap();
        let manifest: Value = serde_json::from_str(&fs::read_to_string(out.join("manifest.json")).unwrap()).unwrap();
        let mut listed: Vec<String> = manifest["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f.as_str().unwrap().to_owned())
            .collect();
        listed.push("manifest.json".into());
        listed.sort();
        assert_eq!(listed, files);
        assert_eq!(listed, {
            let mut held = listing(&out);
            held.push("manifest.json".into());
            held.sort();
            held
        });
        assert_eq!(manifest["scenario"], "x");
        assert_eq!(manifest["env_names"], json!(["ANTHROPIC_API_KEY", "HOME"]));
        assert_eq!(manifest["mode"], "exec");
        fs::remove_dir_all(&run).unwrap();
        fs::remove_dir_all(&out).unwrap();
    }

    #[test]
    fn outgoing_lines_are_scrubbed_in_place_and_keep_their_positions() {
        let dir = temp("outgoing");
        let path = dir.join("outgoing.jsonl");
        fs::write(
            &path,
            json!({"seq": 1, "rpc": "send_turn", "proto_events": 3, "text": "look at /home/alice/a"}).to_string()
                + "\n",
        )
        .unwrap();
        scrub_jsonl_in_place(&scrubber(), &path).unwrap();
        let lines = read_jsonl(&path).unwrap();
        assert_eq!(lines[0]["proto_events"], 3);
        assert_eq!(lines[0]["text"], "look at $RUN/a");
        fs::remove_dir_all(&dir).unwrap();
    }

    fn scenario() -> Value {
        json!({
            "id": "demo",
            "model": [{"text": "I will read it", "tool": {"name": "Read", "input": {"file_path": "a.txt"}}}],
            "driver": [{"send": "please read a.txt"}],
            "project": {"a.txt": "alpha\n"}
        })
    }

    fn events() -> Vec<Value> {
        vec![
            json!({"type": "session_opened", "session_id": "s-1", "provider_session_id": "p-1", "model": "claude-opus-5-5",
                   "cwd": "/mnt/eitri-runs/ch/ab12cd34/c00/project"}),
            json!({"type": "user_prompt_submitted", "text": "please read a.txt"}),
            json!({"type": "content_delta", "turn_id": "t1", "kind": "text", "text": "I will read"}),
            json!({"type": "tool_call_started", "turn_id": "t1", "tool_use_id": "toolu_1", "name": "Read",
                   "input": {"file_path": "/mnt/eitri-runs/ch/ab12cd34/c00/project/a.txt"}}),
            json!({"type": "tool_call_completed", "turn_id": "t1", "tool_use_id": "toolu_1", "is_error": false,
                   "content": "File contents of alice's /home/alice/a.txt follow"}),
            json!({"type": "commands_available", "commands": [{"name": "compact", "description": "Clear history but keep a summary"}]}),
            json!({"type": "turn_completed", "turn_id": "t1", "outcome": "completed", "result_text": "I will read it",
                   "stop_reason": "end_turn", "detail": {"reason": "completed"}}),
        ]
    }

    #[test]
    fn a_twin_keeps_the_scenarios_words_and_ids_and_digests_what_the_cli_wrote() {
        let out = twin(&scrubber(), "demo", &scenario(), &events());
        assert_eq!(out["scenario"], "demo");
        let e = &out["domain_events"];
        assert_eq!(e[0]["cwd"], "$RUN/project", "paths are relative to the run");
        assert_eq!(e[0]["session_id"], "s-1");
        assert_eq!(e[1]["text"], "please read a.txt");
        assert_eq!(e[2]["text"], "I will read");
        assert_eq!(e[3]["input"]["file_path"], "$RUN/project/a.txt");
        let content = e[4]["content"].as_str().unwrap();
        let said = "File contents of $USER's $RUN/a.txt follow";
        assert_eq!(content, cli_text(said));
        assert_eq!(e[5]["commands"][0]["name"], "compact");
        let described = "Clear history but keep a summary";
        assert_eq!(e[5]["commands"][0]["description"], cli_text(described));
        assert!(
            cli_text(described).len() < 60,
            "a digest fits where a command hint is bounded"
        );
        assert_eq!(e[6]["outcome"], "completed");
        assert_eq!(e[6]["stop_reason"], "end_turn");
        assert_eq!(e[6]["detail"]["reason"], "completed");
        assert_eq!(e[6]["result_text"], "I will read it");
        assert!(!out.to_string().contains("alice"));
    }

    #[test]
    fn the_clis_per_user_directory_loses_its_uid() {
        let uid_dir = format!("/tmp/claude-{}", 1000);
        let out = scrubber().text(&format!(
            "/mnt/eitri-runs/ch/ab12cd34/c00{uid_dir}/-mnt-x/tasks/a.output"
        ));
        assert_eq!(out, "$RUN/tmp/claude-$UID/-mnt-x/tasks/a.output");
    }

    #[test]
    fn a_transcript_keeps_queued_commands_and_digests_the_clis_notes_to_itself() {
        let s = scrubber();
        let note = json!({"type": "attachment", "uuid": "u1",
            "attachment": {"type": "commit_attribution", "commit": "A footer line the CLI would add", "pr": "x"},
            "rendered": [{"content": "<system-reminder>\nAttribution</system-reminder>"}]});
        let out = s.transcript_line(&note);
        assert_eq!(out["attachment"]["type"], "commit_attribution");
        assert_eq!(out["uuid"], "u1");
        let commit = out["attachment"]["commit"].as_str().unwrap();
        let footer = "A footer line the CLI would add";
        assert!(
            commit.starts_with(&format!("<cli-text chars={} sha256=", footer.chars().count())),
            "{commit}"
        );
        assert_eq!(out["attachment"]["pr"], "x", "a short string carries no prose");
        assert!(out["rendered"][0]["content"]
            .as_str()
            .unwrap()
            .starts_with("<cli-text chars="));

        let queued = json!({"type": "attachment", "attachment": {"type": "queued_command", "prompt": "STEER-NOW at /home/alice",
            "origin": {"kind": "human"}}});
        let out = s.transcript_line(&queued);
        assert_eq!(out["attachment"]["prompt"], "STEER-NOW at $RUN");
        assert_eq!(out["attachment"]["origin"]["kind"], "human");

        let request = json!({"type": "api-request-shape", "sessionId": "s1", "shapeHash": "abc123def",
            "shape": {"system": [{"type": "text", "text": "You are Claude Code, a long prompt"}]}});
        let out = s.transcript_line(&request);
        assert_eq!(out["sessionId"], "s1");
        assert_eq!(out["shapeHash"], "abc123def");
        assert_eq!(out["shape"]["system"][0]["type"], "text");
        assert!(out["shape"]["system"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("<cli-text chars=34 sha256="));

        let user = json!({"type": "user", "message": {"role": "user", "content": [
            {"type": "text", "text": "<system-reminder>\nTask tools are idle</system-reminder>"},
            {"type": "text", "text": "real prompt"}]}});
        let out = s.transcript_line(&user);
        assert!(out["message"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("<cli-text chars="));
        assert_eq!(out["message"]["content"][1]["text"], "real prompt");
    }

    #[test]
    fn the_wire_files_lose_the_uid_without_changing_a_length() {
        let dir = temp("wire");
        let frame = |text: &str| {
            let mut bytes = vec![text.len() as u8];
            bytes.extend(text.as_bytes());
            bytes
        };
        let uid = |digits: u32| format!("/tmp/claude-{digits}");
        let original = frame(&format!("see /run{}/x and {}/y", uid(1000), uid(12)));
        fs::write(dir.join("proto.bin"), &original).unwrap();
        fs::write(dir.join("proto-resume1.txt"), format!("at {}/z", uid(1000))).unwrap();
        fs::write(dir.join("expected-events.json"), format!("[\"{}\"]", uid(1000))).unwrap();
        fs::write(
            dir.join("outgoing.jsonl"),
            format!("{} stays: another scrub owns this file", uid(1000)),
        )
        .unwrap();
        scrub_wire_files(&dir).unwrap();
        let bin = fs::read(dir.join("proto.bin")).unwrap();
        assert_eq!(bin.len(), original.len());
        assert_eq!(&bin[1..], b"see /run/tmp/claude-$UID/x and /tmp/claude-$U/y");
        assert_eq!(
            fs::read_to_string(dir.join("proto-resume1.txt")).unwrap(),
            "at /tmp/claude-$UID/z"
        );
        assert_eq!(
            fs::read_to_string(dir.join("expected-events.json")).unwrap(),
            "[\"/tmp/claude-$UID\"]"
        );
        assert!(fs::read_to_string(dir.join("outgoing.jsonl"))
            .unwrap()
            .starts_with(&uid(1000)));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_probe_has_no_twin_and_a_scenario_has() {
        assert!(!wants_twin("probe_steer_fg"));
        assert!(wants_twin("c00"));
    }
}
