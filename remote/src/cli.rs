//! `eitri remote add-device`, `devices`, `revoke`, `grant` and `ssh-key`: the device store from a terminal.
//!
//! `devices` and `revoke` read and write the store themselves (`revoke` through `remote_devices::update`, so a window
//! enrolling at the same time cannot write the device back); `serve` sees the device go and closes its connections.
//! `add-device` and `grant` only ask a window, through `serve`'s control socket (bound whatever `serve` listens on for
//! phones): the desk's y/n there is the boundary, which nothing typed in a terminal can stand in for, since an agent
//! can type too. `ssh-key add` edits `~/.ssh/authorized_keys`
//! after a y/n here: it gives a phone's key one forward to the loopback port and nothing else.

use eitri_core::remote_devices::{Device, DeviceStore, Grant, Recipe};
use eitri_core::remote_pairing::{public_key_fingerprint, remote_state_dir, PairMode, DEVICES_FILE};
use eitri_core::remote_rv::{
    parse_rv_outbound, serve_control_socket_path, RvClient, RvInbound, RvOutbound, RV_PROTOCOL,
};
use std::io::{self, BufRead, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long `add-device` waits for `serve`'s answer: `serve` hands the request to a window and passes its word back.
const ANSWER_WITHIN: Duration = Duration::from_secs(10);

/// How long `grant` waits: the window's question waits three minutes for the desk, and its answer comes after.
const GRANT_ANSWER_WITHIN: Duration = Duration::from_secs(200);

/// The largest SSH public key file read; an RSA key of 16384 bits is under 3 KiB.
const SSH_KEY_FILE_MAX_BYTES: u64 = 64 * 1024;

/// The largest `authorized_keys` read to look for the key already there.
const AUTHORIZED_KEYS_MAX_BYTES: u64 = 4 * 1024 * 1024;

/// The longest name the `authorized_keys` line carries after `eitri-phone-`.
const KEY_NAME_MAX: usize = 64;

/// The file in the remote state directory that keeps the port `serve` listens on.
const PORT_FILE: &str = "port";

/// The key types sshd takes for a user key; anything else in the first field is refused.
const KEY_TYPES: &[&str] = &[
    "ssh-ed25519",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "ssh-rsa",
];

/// The usage line of `add-device`.
const ADD_DEVICE_USAGE: &str = "usage: eitri remote add-device [--ssh | --direct] <name>";

/// `eitri remote add-device [--ssh | --direct] <name>`: asks the focused window, through `serve`, to show a one-time
/// enrollment code for the phone `name`, and says where that phone will connect. `--ssh` makes a code for the SSH
/// recipe and `--direct` one for the direct address (`remote.origins` or `remote.listen`); with neither the window
/// takes the direct address when it has one, else SSH. Nothing is enrolled here: the phone presents the code and the
/// desk says yes in that window. With no server or no window, or a recipe the window has no route for, it exits 1. A
/// name that starts with `-`, an unknown option, both recipes or no name is refused (exit 2): an option typed where
/// the name goes would otherwise start an enrollment.
pub fn add_device(args: &[String]) -> i32 {
    let mut recipe = None;
    let mut names = Vec::new();
    for arg in args {
        let asked = match arg.as_str() {
            "--ssh" => PairMode::Ssh,
            "--direct" => PairMode::Direct,
            word if word.starts_with('-') => {
                eprintln!("eitri remote add-device: {word:?} is not a device name (it reads as an option)");
                eprintln!("{ADD_DEVICE_USAGE}");
                return 2;
            }
            _ => {
                names.push(arg);
                continue;
            }
        };
        if recipe.is_some_and(|had| had != asked) {
            eprintln!("eitri remote add-device: --ssh and --direct are two recipes; a code is for one");
            eprintln!("{ADD_DEVICE_USAGE}");
            return 2;
        }
        recipe = Some(asked);
    }
    let [name] = names.as_slice() else {
        eprintln!("{ADD_DEVICE_USAGE}");
        return 2;
    };
    let control_dir =
        eitri_core::panel_control::control_dir(std::env::var_os("XDG_RUNTIME_DIR").as_deref(), &std::env::temp_dir());
    match serve_control_socket_path(&control_dir) {
        Ok(path) => add_device_at(&path, name, recipe),
        Err(e) => {
            eprintln!("eitri remote add-device: {e}");
            1
        }
    }
}

/// [`add_device`] through the server socket at `path`.
pub fn add_device_at(path: &Path, name: &str, recipe: Option<PairMode>) -> i32 {
    let mut client = match RvClient::connect(path) {
        Ok(client) => client,
        Err(e) => {
            eprintln!(
                "eitri remote add-device: no Eitri window has remote control on ({} does not answer: {e})",
                path.display()
            );
            return 1;
        }
    };
    if let Err(e) = client.send(&RvInbound::StartEnroll { v: RV_PROTOCOL, recipe }) {
        eprintln!("eitri remote add-device: the server did not take the request: {e}");
        return 1;
    }
    let line = match client.recv(ANSWER_WITHIN) {
        Ok(Some(line)) => line,
        Ok(None) => {
            eprintln!("eitri remote add-device: no answer from the server");
            return 1;
        }
        Err(e) => {
            eprintln!("eitri remote add-device: the server closed: {e}");
            return 1;
        }
    };
    match parse_rv_outbound(line.as_bytes()) {
        Ok(RvOutbound::EnrollShown {
            recipe: shown_recipe,
            target,
            other,
            ..
        }) => {
            if recipe.is_some_and(|asked| asked != shown_recipe) {
                eprintln!(
                    "eitri remote add-device: the window showed a code for the {} recipe, not the one asked for",
                    shown_recipe.as_str()
                );
                return 1;
            }
            println!("{} will connect {}.", shown(name), shown(&target));
            if recipe.is_none() && other {
                let flag = match shown_recipe {
                    PairMode::Ssh => "--direct",
                    PairMode::Direct => "--ssh",
                };
                println!("(add-device {flag} makes a code for the window's other recipe.)");
            }
            println!("Scan the code the Eitri window shows with {name}, within three minutes; then answer y there.");
            0
        }
        Ok(RvOutbound::Result { why, .. }) => {
            eprintln!(
                "eitri remote add-device: {}",
                shown(why.as_deref().unwrap_or("refused"))
            );
            1
        }
        Ok(RvOutbound::Refused { why, .. }) => {
            eprintln!("eitri remote add-device: {}", shown(&why));
            1
        }
        _ => {
            eprintln!("eitri remote add-device: the server's answer was not understood");
            1
        }
    }
}

/// The device store's path: `devices.json` in the remote state directory.
fn store_path() -> Result<PathBuf, String> {
    remote_state_dir(
        std::env::var_os("XDG_STATE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
    .map(|dir| dir.join(DEVICES_FILE))
    .ok_or_else(|| "neither XDG_STATE_HOME nor HOME names an absolute directory".to_owned())
}

/// `serve`'s control socket in the control directory.
fn serve_path() -> io::Result<PathBuf> {
    let control_dir =
        eitri_core::panel_control::control_dir(std::env::var_os("XDG_RUNTIME_DIR").as_deref(), &std::env::temp_dir());
    serve_control_socket_path(&control_dir)
}

/// The store at `path`; a missing store holds no device.
fn load_store(path: &Path) -> io::Result<DeviceStore> {
    match DeviceStore::load(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(DeviceStore::default()),
        other => other,
    }
}

/// The device `word` names: its id, or a name only one device has.
fn resolve<'a>(store: &'a DeviceStore, word: &str) -> Result<&'a Device, String> {
    if let Some(device) = store.get(word) {
        return Ok(device);
    }
    let named: Vec<&Device> = store.devices().iter().filter(|d| d.name == word).collect();
    match named.as_slice() {
        [one] => Ok(one),
        [] => Err(format!("no device {}", shown(word))),
        many => Err(format!(
            "{} devices are named {}; give one of their ids: {}",
            many.len(),
            shown(word),
            many.iter().map(|d| shown(&d.id)).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// `text` with its control, invisible and direction-changing characters written as `\u{...}`, so a name a phone
/// chose cannot move the cursor, hide itself or reorder the line it is printed on.
pub(crate) fn shown(text: &str) -> String {
    text.chars()
        .map(|c| {
            let hidden = c.is_control()
                || matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2069}' | '\u{feff}');
            if hidden {
                format!("\\u{{{:x}}}", c as u32)
            } else {
                c.to_string()
            }
        })
        .collect()
}

fn grant_text(grant: Grant) -> &'static str {
    match grant {
        Grant::Watch => "watch",
        Grant::WatchSteer => "watch+steer",
    }
}

fn recipe_text(recipe: Recipe) -> &'static str {
    match recipe {
        Recipe::Ssh => "ssh",
        Recipe::Direct => "direct",
        Recipe::Unknown => "unknown",
    }
}

/// `secs` since the Unix epoch as `YYYY-MM-DD HH:MM UTC`.
fn utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let (h, m) = ((secs % 86_400) / 3600, (secs % 3600) / 60);
    // Days since the epoch to the proleptic Gregorian date (H. Hinnant's civil_from_days).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(mo <= 2);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02} UTC")
}

/// `eitri remote devices`: lists the enrolled devices.
pub fn devices(args: &[String]) -> i32 {
    if !args.is_empty() {
        eprintln!("usage: eitri remote devices");
        return 2;
    }
    match store_path() {
        Ok(path) => devices_at(&path, &mut io::stdout().lock(), &mut io::stderr().lock()),
        Err(e) => {
            eprintln!("eitri remote devices: {e}");
            1
        }
    }
}

/// [`devices`] from the store at `path`: one row per device (id, name, grant, recipe, the `Host` it enrolled
/// through, the `Host` and time it was last connected), then a line for each device no desk said yes to.
pub fn devices_at(path: &Path, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let store = match load_store(path) {
        Ok(store) => store,
        Err(e) => {
            let _ = writeln!(err, "eitri remote devices: {e}");
            return 1;
        }
    };
    if store.devices().is_empty() {
        let _ = writeln!(out, "no devices enrolled");
        return 0;
    }
    let mut rows = vec![[
        "DEVICE".to_owned(),
        "NAME".to_owned(),
        "GRANT".to_owned(),
        "RECIPE".to_owned(),
        "ENROLLED THROUGH".to_owned(),
        "LAST HOST".to_owned(),
        "LAST SEEN".to_owned(),
    ]];
    for d in store.devices() {
        rows.push([
            shown(&d.id),
            shown(&d.name),
            grant_text(d.grant).to_owned(),
            recipe_text(d.recipe).to_owned(),
            if d.enrolled_host.is_empty() {
                "-".to_owned()
            } else {
                shown(&d.enrolled_host)
            },
            d.last_host.as_deref().map_or_else(|| "-".to_owned(), shown),
            d.last_seen.map_or_else(|| "never".to_owned(), utc),
        ]);
    }
    let widths: Vec<usize> = (0..7)
        .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
        .collect();
    for row in &rows {
        let line: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, w)| format!("{cell:<w$}", w = *w))
            .collect();
        let _ = writeln!(out, "{}", line.join("  ").trim_end());
    }
    for d in store.devices().iter().filter(|d| d.confirmed.is_none()) {
        let _ = writeln!(
            out,
            "{}: no desk said yes to this device; if you did not add it, `eitri remote revoke {}` takes it out",
            shown(&d.id),
            shown(&d.id)
        );
    }
    0
}

/// `eitri remote revoke <device>`: takes a device out of the store, by id or by a name only it has.
pub fn revoke(args: &[String]) -> i32 {
    let [device] = args else {
        eprintln!("usage: eitri remote revoke <device>");
        return 2;
    };
    match store_path() {
        Ok(path) => revoke_at(&path, device, &mut io::stdout().lock(), &mut io::stderr().lock()),
        Err(e) => {
            eprintln!("eitri remote revoke: {e}");
            1
        }
    }
}

/// [`revoke`] on the store at `path`. The device goes with everything kept of it, its push endpoint included;
/// `serve`, watching the store, closes its connections.
pub fn revoke_at(path: &Path, word: &str, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let removed = eitri_core::remote_devices::update(path, |store| {
        let id = resolve(store, word)?.id.clone();
        store.remove(&id).ok_or_else(|| format!("no device {}", shown(word)))
    });
    match removed {
        Ok(Ok(device)) => {
            let _ = writeln!(out, "revoked {} ({})", device.id, shown(&device.name));
            if device.recipe != Recipe::Ssh {
                return 0;
            }
            let _ = writeln!(
                out,
                "its SSH key may still be in ~/.ssh/authorized_keys (a line ending eitri-phone-...): delete that line too"
            );
            0
        }
        Ok(Err(why)) => {
            let _ = writeln!(err, "eitri remote revoke: {why}");
            1
        }
        Err(e) => {
            let _ = writeln!(err, "eitri remote revoke: the device store: {e}");
            1
        }
    }
}

/// `eitri remote grant <device> watch|steer`: asks the focused window, through `serve`, to let the device watch only,
/// or watch and steer. The grant changes only after the desk's `y` there.
pub fn grant(args: &[String]) -> i32 {
    let [device, word] = args else {
        eprintln!("usage: eitri remote grant <device> watch|steer");
        return 2;
    };
    let grant = match word.as_str() {
        "watch" => Grant::Watch,
        "steer" | "watch+steer" => Grant::WatchSteer,
        _ => {
            eprintln!("usage: eitri remote grant <device> watch|steer");
            return 2;
        }
    };
    let paths = store_path().and_then(|store| serve_path().map(|serve| (store, serve)).map_err(|e| e.to_string()));
    match paths {
        Ok((store, serve)) => grant_at(
            &serve,
            &store,
            device,
            grant,
            &mut io::stdout().lock(),
            &mut io::stderr().lock(),
        ),
        Err(e) => {
            eprintln!("eitri remote grant: {e}");
            1
        }
    }
}

/// [`grant`] through the server socket at `serve`, naming the device as the store at `store` knows it.
pub fn grant_at(serve: &Path, store: &Path, word: &str, grant: Grant, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let device = match load_store(store)
        .map_err(|e| e.to_string())
        .and_then(|s| Ok(resolve(&s, word)?.id.clone()))
    {
        Ok(id) => id,
        Err(why) => {
            let _ = writeln!(err, "eitri remote grant: {why}");
            return 1;
        }
    };
    let _ = writeln!(
        out,
        "Asking the Eitri window: answer y there (within three minutes) to let {device} {}.",
        match grant {
            Grant::Watch => "only watch",
            Grant::WatchSteer => "watch and steer",
        }
    );
    let _ = out.flush();
    let frame = RvInbound::GrantRequest {
        v: RV_PROTOCOL,
        device: device.clone(),
        grant,
    };
    match ask_serve(serve, &frame, GRANT_ANSWER_WITHIN) {
        Ok(()) => {
            let _ = writeln!(out, "{device} may now {}", grant_text(grant));
            0
        }
        Err(why) => {
            let _ = writeln!(err, "eitri remote grant: {why}");
            1
        }
    }
}

/// Sends `frame` to `serve` at `path` and reads its one answer within `within`: `Ok` for a `result` that says so,
/// the reason for anything else.
fn ask_serve(path: &Path, frame: &RvInbound, within: Duration) -> Result<(), String> {
    let mut client = RvClient::connect(path).map_err(|e| {
        format!(
            "no Eitri window has remote control on ({} does not answer: {e})",
            path.display()
        )
    })?;
    client
        .send(frame)
        .map_err(|e| format!("the server did not take the request: {e}"))?;
    let line = match client.recv(within) {
        Ok(Some(line)) => line,
        Ok(None) => return Err("no answer from the server".to_owned()),
        Err(e) => return Err(format!("the server closed: {e}")),
    };
    match parse_rv_outbound(line.as_bytes()) {
        Ok(RvOutbound::Result { ok: true, .. }) => Ok(()),
        Ok(RvOutbound::Result { why, .. }) => Err(why.unwrap_or_else(|| "refused".to_owned())),
        Ok(RvOutbound::Refused { why, .. }) => Err(why),
        _ => Err("the server's answer was not understood".to_owned()),
    }
}

/// `eitri remote ssh-key add <file> [--name <name>] [--port <port>]`: shows the `authorized_keys` line that lets the
/// phone's public key in `<file>` open one forward to `serve`'s loopback port, and appends it after a `y`. The port is
/// the one `serve` keeps unless `--port` names another (as `remote.port` does); the name ends the line as
/// `eitri-phone-<name>`, from `--name` or else the key's own comment.
pub fn ssh_key(args: &[String]) -> i32 {
    const USAGE: &str = "usage: eitri remote ssh-key add <file> [--name <name>] [--port <port>]";
    let Some((sub, rest)) = args.split_first() else {
        eprintln!("{USAGE}");
        return 2;
    };
    let Some((file, options)) = rest.split_first().filter(|_| sub == "add") else {
        eprintln!("{USAGE}");
        return 2;
    };
    let (mut name, mut port) = (None, None);
    let mut words = options.iter();
    while let Some(option) = words.next() {
        match (option.as_str(), words.next()) {
            ("--name", Some(value)) if name.is_none() => name = Some(value.as_str()),
            ("--port", Some(value)) if port.is_none() => match value.parse::<u16>() {
                Ok(p) if p > 0 => port = Some(p),
                _ => {
                    eprintln!("eitri remote ssh-key: --port takes a port from 1 to 65535");
                    return 2;
                }
            },
            _ => {
                eprintln!("{USAGE}");
                return 2;
            }
        }
    }
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from).filter(|h| h.is_absolute()) else {
        eprintln!("eitri remote ssh-key: HOME does not name an absolute directory");
        return 1;
    };
    let port = match port {
        Some(port) => port,
        None => {
            let kept = remote_state_dir(std::env::var_os("XDG_STATE_HOME").as_deref(), Some(home.as_os_str()))
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no state directory"))
                .and_then(|dir| kept_port(&dir));
            match kept {
                Ok(port) => port,
                Err(e) => {
                    eprintln!(
                        "eitri remote ssh-key: no port is kept yet ({e}): turn remote control on once, or give --port"
                    );
                    return 1;
                }
            }
        }
    };
    ssh_key_add_at(
        Path::new(file),
        name,
        port,
        &home,
        &mut io::stdin().lock(),
        &mut io::stdout().lock(),
        &mut io::stderr().lock(),
    )
}

/// The port `serve` keeps in the remote state directory `dir` (`port`, one number and a newline, a private file).
pub fn kept_port(dir: &Path) -> io::Result<u16> {
    let text = agent::private_fs::read_private_nofollow_capped(&dir.join(PORT_FILE), 16)?;
    text.trim_end_matches('\n')
        .parse::<u16>()
        .ok()
        .filter(|p| *p > 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "the kept port is not a port"))
}

/// The key file the user named, followed if it is a link: a regular file of at most [`SSH_KEY_FILE_MAX_BYTES`].
fn read_key_file(path: &Path) -> io::Result<String> {
    if !std::fs::metadata(path)?.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
    }
    let mut text = String::new();
    std::fs::File::open(path)?
        .take(SSH_KEY_FILE_MAX_BYTES + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > SSH_KEY_FILE_MAX_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "too large for a public key"));
    }
    Ok(text)
}

/// One OpenSSH public key read from a file: its type, its base64 blob and its comment.
struct PublicKey {
    kind: String,
    blob: String,
    comment: Option<String>,
}

/// The one public key `text` holds: a single line (blank lines and `#` lines aside) of a known key type and a blob
/// that names that type. Anything else (a private key, two keys, an options prefix, a key type sshd does not take)
/// is refused with the reason; the text itself is never repeated.
fn one_public_key(text: &str) -> Result<PublicKey, &'static str> {
    if text.contains("PRIVATE KEY") {
        return Err(
            "that is a private key: give the phone's public key (the .pub file), and keep the private one secret",
        );
    }
    let mut lines = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'));
    let (Some(line), None) = (lines.next(), lines.next()) else {
        return Err("the file must hold exactly one OpenSSH public key");
    };
    let mut words = line.split_ascii_whitespace();
    let (Some(kind), Some(blob)) = (words.next(), words.next()) else {
        return Err("the file must hold exactly one OpenSSH public key");
    };
    if !KEY_TYPES.contains(&kind) || public_key_fingerprint(line).is_none() {
        return Err("the file must hold exactly one OpenSSH public key");
    }
    let comment: Vec<&str> = words.collect();
    Ok(PublicKey {
        kind: kind.to_owned(),
        blob: blob.to_owned(),
        comment: (!comment.is_empty()).then(|| comment.join(" ")),
    })
}

/// `name` as the line's name: letters, digits, `.`, `_` and `-`, at most [`KEY_NAME_MAX`]; others become `-`.
fn key_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .take(KEY_NAME_MAX)
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        "phone".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// The `authorized_keys` line for `key`: one forward to `127.0.0.1:<port>`, no shell, no command (`restrict` alone
/// still lets a session run one; `command="/bin/false"` ends that), no agent or X11 forwarding, no pty.
/// `port-forwarding` lets the key ask sshd to listen too (`ssh -R`), and an `authorized_keys` line has no way to say
/// "nowhere" (`permitlisten` takes no `none` there; sshd refuses the whole line): it names loopback port 1, which an
/// ordinary user cannot bind and nothing connects to. That closes TCP listeners only. sshd applies neither
/// `permitlisten` nor any other key option to a listener on a Unix-socket path (`ssh -R <path>:...`), so the key can
/// still have sshd bind one, as the user, at any free path the user may write; only `sshd_config` can refuse it
/// (`AllowStreamLocalForwarding local`), which [`ssh_key_add_at`] says before it asks. `permitopen` does cover the
/// other direction: a forward to a Unix-socket path is refused.
fn restricted_line(key: &PublicKey, port: u16, name: &str) -> String {
    format!(
        "restrict,port-forwarding,command=\"/bin/false\",permitopen=\"127.0.0.1:{port}\",permitlisten=\"127.0.0.1:1\" {} {} eitri-phone-{name}",
        key.kind, key.blob
    )
}

/// The `sshd_config` lines that stop the user's keys from asking sshd for a listener on a Unix-socket path, indented
/// for the terminal. `user` (the login name from the environment) is used only when it is a plain name an sshd
/// `Match User` pattern reads as itself; anything else leaves a placeholder for the user to fill in.
fn sshd_match_lines(user: Option<&std::ffi::OsStr>) -> String {
    let user = user
        .and_then(|u| u.to_str())
        .filter(|u| {
            !u.is_empty()
                && u.len() <= KEY_NAME_MAX
                && u.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
                && !u.starts_with('-')
        })
        .unwrap_or("<your user name>");
    format!("    Match User {user}\n        AllowStreamLocalForwarding local\n")
}

/// The login name from the environment, as the shell that runs the command knows it.
fn login_name() -> Option<std::ffi::OsString> {
    std::env::var_os("USER")
        .filter(|u| !u.is_empty())
        .or_else(|| std::env::var_os("LOGNAME"))
}

/// [`ssh_key`] with its inputs given: the key file, the name (else the key's comment), the port, the home whose
/// `.ssh/authorized_keys` is written, where the y/n is read and where it is asked.
pub fn ssh_key_add_at(
    key_file: &Path,
    name: Option<&str>,
    port: u16,
    home: &Path,
    answer: &mut dyn BufRead,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let text = match read_key_file(key_file) {
        Ok(text) => text,
        Err(e) => {
            let _ = writeln!(err, "eitri remote ssh-key: {}: {e}", key_file.display());
            return 1;
        }
    };
    let key = match one_public_key(&text) {
        Ok(key) => key,
        Err(why) => {
            let _ = writeln!(err, "eitri remote ssh-key: {why}");
            return 1;
        }
    };
    let name = key_name(name.or(key.comment.as_deref()).unwrap_or("phone"));
    let ssh_dir = home.join(".ssh");
    let path = ssh_dir.join("authorized_keys");
    let existing = match agent::private_fs::read_private_nofollow_capped(&path, AUTHORIZED_KEYS_MAX_BYTES) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            let _ = writeln!(err, "eitri remote ssh-key: {}: {e}", path.display());
            return 1;
        }
    };
    if existing
        .lines()
        .any(|l| l.split_ascii_whitespace().any(|w| w == key.blob))
    {
        // sshd takes the first line a key matches, so a restricted line after it would restrict nothing.
        let _ = writeln!(
            err,
            "eitri remote ssh-key: this key is already in {}; edit or delete that line first",
            path.display()
        );
        return 1;
    }
    let line = restricted_line(&key, port, &name);
    let _ = write!(
        out,
        "This line goes into {}:\n\n{line}\n\n\
         Over TCP it lets the key forward only to 127.0.0.1:{port}, and it gives the key no shell or command.\n\
         It cannot stop the key from asking sshd to listen on a Unix-socket path as you (ssh -R <path>:...);\n\
         no authorized_keys option can. To refuse that, add these lines at the end of sshd_config (as root;\n\
         they apply to every key of that user) and reload sshd:\n\n{}\n\
         Add it? (y/n) ",
        path.display(),
        sshd_match_lines(login_name().as_deref())
    );
    let _ = out.flush();
    let mut reply = String::new();
    if answer.read_line(&mut reply).is_err() || !matches!(reply.trim(), "y" | "Y" | "yes") {
        let _ = writeln!(out, "Nothing was written.");
        return 1;
    }
    match append_line(&ssh_dir, &path, &existing, &line) {
        Ok(()) => {
            let _ = writeln!(
                out,
                "Added. The phone's key may now forward to 127.0.0.1:{port}; Unix-socket listeners are up to sshd_config."
            );
            0
        }
        Err(e) => {
            let _ = writeln!(err, "eitri remote ssh-key: {}: {e}", path.display());
            1
        }
    }
}

/// Appends `line` to `path` in `ssh_dir` (made 0700 when missing; the file made 0600 when missing), on a line of its
/// own after `existing`. Nothing is written when the name is a link, or no longer the file opened.
fn append_line(ssh_dir: &Path, path: &Path, existing: &str, line: &str) -> io::Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(ssh_dir) {
        Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
        _ => {}
    }
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)?;
    let opened = file.metadata()?;
    let named = std::fs::symlink_metadata(path)?;
    if !named.is_file() || (named.dev(), named.ino()) != (opened.dev(), opened.ino()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "not a regular file of its own (a link, or replaced while opened)",
        ));
    }
    let lead = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    file.write_all(format!("{lead}{line}\n").as_bytes())?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;

    fn scratch(label: &str) -> std::path::PathBuf {
        let base = std::env::var_os("XDG_RUNTIME_DIR")
            .map(std::path::PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(std::env::temp_dir);
        let dir = base.join(format!("rc-{label}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A server that reads one line, checks it is `start_enroll` asking for `recipe`, and answers `answer`.
    fn serve_once(path: &Path, recipe: Option<&'static str>, answer: &'static str) -> std::thread::JoinHandle<()> {
        let listener = UnixListener::bind(path).unwrap();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let asked: serde_json::Value = serde_json::from_str(&line).unwrap();
            let mut wanted = serde_json::json!({"kind": "start_enroll", "v": 1});
            if let Some(recipe) = recipe {
                wanted["recipe"] = recipe.into();
            }
            assert_eq!(asked, wanted);
            (&stream).write_all(format!("{answer}\n").as_bytes()).unwrap();
        })
    }

    #[test]
    fn add_device_without_a_window_exits_nonzero() {
        let dir = scratch("none");
        let path = dir.join("remote.sock");
        let _ = std::fs::remove_file(&path);
        assert_eq!(add_device_at(&path, "Pixel", None), 1, "nothing serves the socket");

        let refused = serve_once(
            &path,
            None,
            r#"{"v":1,"kind":"refused","key":null,"why":"no Eitri window has the focus"}"#,
        );
        assert_eq!(add_device_at(&path, "Pixel", None), 1, "serve has no window to ask");
        refused.join().unwrap();
        std::fs::remove_file(&path).unwrap();

        let shown = serve_once(
            &path,
            None,
            r#"{"v":1,"kind":"enroll_shown","recipe":"direct","target":"directly to desk.tail1.ts.net:443 (https)","other":true}"#,
        );
        assert_eq!(add_device_at(&path, "Pixel", None), 0, "a window shows the code");
        shown.join().unwrap();
        std::fs::remove_file(&path).unwrap();

        let no_route = serve_once(
            &path,
            Some("ssh"),
            r#"{"v":1,"kind":"result","command_id":null,"ok":false,"why":"this window has no SSH route for phones"}"#,
        );
        assert_eq!(
            add_device_at(&path, "Pixel", Some(PairMode::Ssh)),
            1,
            "the window has no route for the recipe asked"
        );
        no_route.join().unwrap();
        std::fs::remove_file(&path).unwrap();

        let ssh = serve_once(
            &path,
            Some("ssh"),
            r#"{"v":1,"kind":"enroll_shown","recipe":"ssh","target":"through SSH to 127.0.0.1:41000 on this desk","other":false}"#,
        );
        assert_eq!(add_device_at(&path, "Pixel", Some(PairMode::Ssh)), 0);
        ssh.join().unwrap();
        std::fs::remove_file(&path).unwrap();

        let other = serve_once(
            &path,
            Some("ssh"),
            r#"{"v":1,"kind":"enroll_shown","recipe":"direct","target":"directly to desk.tail1.ts.net:443 (https)","other":true}"#,
        );
        assert_eq!(
            add_device_at(&path, "Pixel", Some(PairMode::Ssh)),
            1,
            "a code for another recipe than the one asked for"
        );
        other.join().unwrap();
        std::fs::remove_file(&path).unwrap();
        let _ = std::fs::remove_dir(&dir);

        assert_eq!(add_device(&[]), 2, "a name is required");
    }

    use eitri_core::remote_devices::{update, Confirmed};

    // Made with `ssh-keygen -t ed25519`.
    const ED25519: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIHsW2+maVvnEFmizxiNHThNbEgyl9S+3nG+QUAh2rC9R u0_a312@localhost";

    fn device(id: &str, name: &str, recipe: Recipe) -> Device {
        Device {
            id: id.into(),
            name: name.into(),
            token_sha256: [7; 32],
            grant: Grant::Watch,
            recipe,
            confirmed: Some(Confirmed { at: 1_762_400_000 }),
            push_endpoint: None,
            last_seen: None,
            last_host: None,
            enrolled_host: "127.0.0.1:43117".into(),
        }
    }

    /// A scratch directory removed when the test ends, passed or not.
    struct Tidy(PathBuf);

    impl Tidy {
        fn new(label: &str) -> Tidy {
            let dir = scratch(label);
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Tidy(dir)
        }
    }

    impl Drop for Tidy {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn store_with(dir: &Tidy, devices: Vec<Device>) -> PathBuf {
        let path = dir.0.join("remote").join(DEVICES_FILE);
        update(&path, move |store| {
            for d in devices {
                assert!(store.insert(d));
            }
        })
        .unwrap();
        path
    }

    fn texts(out: Vec<u8>) -> String {
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn revoke_removes_the_device_and_its_endpoint() {
        let mut pushed = device("d1", "Pixel", Recipe::Direct);
        pushed.push_endpoint = Some("https://ntfy.example/up7f3a".into());
        let dir = Tidy::new("revoke");
        let path = store_with(
            &dir,
            vec![
                pushed,
                device("d2", "Tablet", Recipe::Ssh),
                device("d3", "Twin", Recipe::Direct),
                device("d4", "Twin", Recipe::Direct),
            ],
        );
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(revoke_at(&path, "d1", &mut out, &mut err), 0);
        let store = DeviceStore::load(&path).unwrap();
        assert!(store.get("d1").is_none());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("ntfy.example"),
            "the endpoint goes with the device: {text}"
        );
        assert_eq!(texts(out), "revoked d1 (Pixel)\n");

        // Again: nothing to take out.
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(revoke_at(&path, "d1", &mut out, &mut err), 1);
        assert!(texts(err).contains("no device d1"));

        // By a name only one device has; a device of the SSH recipe reminds of its authorized_keys line.
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(revoke_at(&path, "Tablet", &mut out, &mut err), 0);
        assert!(texts(out).contains("authorized_keys"));
        assert!(DeviceStore::load(&path).unwrap().get("d2").is_none());

        // A name two devices share takes out neither.
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(revoke_at(&path, "Twin", &mut out, &mut err), 1);
        assert!(texts(err).contains("d3, d4"));
        assert_eq!(DeviceStore::load(&path).unwrap().devices().len(), 2);

        assert_eq!(revoke(&[]), 2, "a device is required");
    }

    #[test]
    fn devices_lists_grant_recipe_hosts_and_last_seen() {
        let mut seen = device("d1", "Pixel\u{202e}9", Recipe::Ssh);
        seen.last_seen = Some(1_762_437_900);
        seen.last_host = Some("127.0.0.1:43117".into());
        let mut written = device("d2", "Tablet", Recipe::Unknown);
        written.confirmed = None;
        written.enrolled_host = String::new();
        let dir = Tidy::new("devices");
        // Something other than an enrollment wrote this one, its id included.
        let mut planted = device("d3\u{1b}]0;owned\u{7}", "Planted", Recipe::Direct);
        planted.confirmed = None;
        let path = store_with(&dir, vec![seen, written, planted]);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(devices_at(&path, &mut out, &mut err), 0);
        let out = texts(out);
        assert!(
            !out.contains('\u{1b}') && !out.contains('\u{7}'),
            "an id's control characters are shown, not obeyed: {out:?}"
        );
        assert!(out.contains("d3\\u{1b}]0;owned\\u{7}"), "{out}");
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].split_whitespace().take(2).eq(["DEVICE", "NAME"]), "{out}");
        assert!(
            lines[1].contains("Pixel\\u{202e}9") && !lines[1].contains('\u{202e}'),
            "a hidden character is shown, not obeyed: {out}"
        );
        assert!(lines[1].contains("watch") && lines[1].contains("ssh") && lines[1].contains("2025-11-06 14:05 UTC"));
        assert!(lines[2].contains("unknown") && lines[2].ends_with("never"), "{out}");
        assert!(lines[4].starts_with("d2: no desk said yes"), "{out}");
        assert!(
            lines[5].starts_with("d3\\u{1b}]0;owned\\u{7}: no desk said yes"),
            "{out}"
        );

        let empty = dir.0.join("none").join(DEVICES_FILE);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(devices_at(&empty, &mut out, &mut err), 0);
        assert_eq!(texts(out), "no devices enrolled\n");
    }

    #[test]
    fn grant_asks_the_window_and_waits_for_its_word() {
        let dir = Tidy::new("grant");
        let path = store_with(&dir, vec![device("d1", "Pixel", Recipe::Direct)]);
        let sock = dir.0.join("remote.sock");
        for (answer, code) in [
            (r#"{"v":1,"kind":"result","command_id":null,"ok":true,"why":null}"#, 0),
            (
                r#"{"v":1,"kind":"result","command_id":null,"ok":false,"why":"the desk said no"}"#,
                1,
            ),
        ] {
            let listener = UnixListener::bind(&sock).unwrap();
            let served = std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(&stream).read_line(&mut line).unwrap();
                (&stream).write_all(format!("{answer}\n").as_bytes()).unwrap();
                serde_json::from_str::<serde_json::Value>(&line).unwrap()
            });
            let (mut out, mut err) = (Vec::new(), Vec::new());
            assert_eq!(
                grant_at(&sock, &path, "Pixel", Grant::WatchSteer, &mut out, &mut err),
                code
            );
            assert_eq!(
                served.join().unwrap(),
                serde_json::json!({"kind": "grant_request", "v": 1, "device": "d1", "grant": "watch+steer"}),
                "the device is named by its id"
            );
            std::fs::remove_file(&sock).unwrap();
        }
        assert_eq!(
            DeviceStore::load(&path).unwrap().get("d1").unwrap().grant,
            Grant::Watch,
            "the terminal changes nothing itself"
        );
        let (mut out, mut err) = (Vec::new(), Vec::new());
        assert_eq!(grant_at(&sock, &path, "nobody", Grant::Watch, &mut out, &mut err), 1);
        assert_eq!(grant(&args(&["d1", "admin"])), 2);
    }

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    #[test]
    fn ssh_key_add_refuses_a_private_key() {
        let dir = Tidy::new("ssh-private");
        let home = dir.0.clone();
        let private = format!(
            "-----BEGIN OPENSSH {kind} KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\n-----END OPENSSH {kind} KEY-----\n",
            kind = "PRIVATE"
        );
        let refused = [
            private,
            format!("{ED25519}\n{ED25519}\n"),
            format!("restrict {ED25519}\n"),
            "ssh-ed25519 bm90IGEga2V5 x\n".to_owned(),
            // The algorithm's name with no key after it.
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5 x\n".to_owned(),
            "ssh-dss AAAAB3NzaC1kc3MAAACBAP x\n".to_owned(),
            ED25519.replacen("ssh-ed25519", "ssh-rsa", 1),
            String::new(),
        ];
        for (n, text) in refused.iter().enumerate() {
            let file = home.join(format!("key{n}.pub"));
            std::fs::write(&file, text).unwrap();
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let mut yes = io::Cursor::new(b"y\n".to_vec());
            assert_eq!(
                ssh_key_add_at(&file, Some("pixel"), 43117, &home, &mut yes, &mut out, &mut err),
                1,
                "{text:?}"
            );
            let said = format!("{}{}", texts(out), texts(err));
            assert!(
                !said.contains("b3BlbnNzaC1rZXkt"),
                "the private key is never repeated: {said}"
            );
            assert!(!home.join(".ssh").exists(), "nothing written for {text:?}");
        }
    }

    #[test]
    fn the_sshd_match_block_names_a_plain_user_only() {
        use std::ffi::OsStr;
        assert_eq!(
            sshd_match_lines(Some(OsStr::new("grey"))),
            "    Match User grey\n        AllowStreamLocalForwarding local\n"
        );
        for odd in ["", "a b", "x\u{1b}[2J", "*", "root,grey", "!grey"] {
            assert_eq!(
                sshd_match_lines(Some(OsStr::new(odd))),
                "    Match User <your user name>\n        AllowStreamLocalForwarding local\n",
                "{odd:?}"
            );
        }
        assert!(sshd_match_lines(None).contains("<your user name>"));
    }

    #[test]
    fn ssh_key_add_writes_the_restricted_line_after_yes() {
        let dir = Tidy::new("ssh-add");
        let home = dir.0.clone();
        let file = home.join("phone.pub");
        std::fs::write(&file, format!("# from the phone\n{ED25519}\n")).unwrap();
        let state = home.join("state");
        std::fs::create_dir(&state).unwrap();
        std::fs::write(state.join("port"), "43117\n").unwrap();
        let port = kept_port(&state).expect("the port serve keeps");
        assert_eq!(port, 43117);

        // No: nothing written.
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut no = io::Cursor::new(b"n\n".to_vec());
        assert_eq!(ssh_key_add_at(&file, None, port, &home, &mut no, &mut out, &mut err), 1);
        let expected = format!(
            "restrict,port-forwarding,command=\"/bin/false\",permitopen=\"127.0.0.1:43117\",permitlisten=\"127.0.0.1:1\" {} eitri-phone-u0_a312-localhost",
            ED25519.rsplit_once(' ').unwrap().0
        );
        let shown = texts(out);
        assert!(shown.contains(&expected), "the line is shown before the question");
        let question = shown.find("Add it? (y/n)").expect("the question");
        let snippet = shown
            .find("    AllowStreamLocalForwarding local")
            .expect("the sshd_config lines for Unix-socket listeners");
        assert!(
            snippet < question,
            "what the line cannot stop is said before the question: {shown}"
        );
        assert!(shown.contains("Match User "), "{shown}");
        assert!(!home.join(".ssh").exists());

        // Yes: the line, alone, in a 0600 file in a 0700 directory.
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut yes = io::Cursor::new(b"y\n".to_vec());
        assert_eq!(
            ssh_key_add_at(&file, None, port, &home, &mut yes, &mut out, &mut err),
            0,
            "{}",
            texts(err)
        );
        assert!(
            !texts(out).contains("nothing else"),
            "the key can still ask for a Unix-socket listener unless sshd_config says no"
        );
        let keys = home.join(".ssh").join("authorized_keys");
        assert_eq!(std::fs::read_to_string(&keys).unwrap(), format!("{expected}\n"));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&keys).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(
            std::fs::metadata(home.join(".ssh")).unwrap().permissions().mode() & 0o777,
            0o700
        );

        // The same key again is refused: sshd would take the first line it matches.
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut yes = io::Cursor::new(b"y\n".to_vec());
        assert_eq!(
            ssh_key_add_at(&file, Some("x"), port, &home, &mut yes, &mut out, &mut err),
            1
        );
        assert!(texts(err).contains("already in"));

        // Another key goes on a line of its own after a file that did not end in a newline, with the name given.
        std::fs::write(&keys, &expected).unwrap();
        let ecdsa = home.join("tablet.pub");
        std::fs::write(&ecdsa, "ecdsa-sha2-nistp256 AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBIKHjx4LQJhnOei/m0u9w0ZnY6CD8by6wKufYum5v7HMmowgmWN7aRI056uis6Y1IHih6tZh2GMD4cjU5EdKX5Q=\n").unwrap();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut yes = io::Cursor::new(b"y\n".to_vec());
        assert_eq!(
            ssh_key_add_at(&ecdsa, Some("my tablet"), 50001, &home, &mut yes, &mut out, &mut err),
            0
        );
        let text = std::fs::read_to_string(&keys).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert_eq!(lines[0], expected);
        assert!(lines[1].starts_with(
            "restrict,port-forwarding,command=\"/bin/false\",permitopen=\"127.0.0.1:50001\",permitlisten=\"127.0.0.1:1\" ecdsa-sha2-nistp256 "
        ));
        assert!(lines[1].ends_with(" eitri-phone-my-tablet"), "{text}");

        assert_eq!(ssh_key(&args(&["add"])), 2);
        assert_eq!(ssh_key(&args(&["remove", "x"])), 2);
        assert_eq!(ssh_key(&args(&["add", "x", "--port", "0"])), 2);
    }
}
