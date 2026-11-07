//! `serve`'s errands about devices: an enrollment, a grant, the listen address, a self-revocation, a push endpoint,
//! the report of who is connected.
//!
//! An enrollment starts with `start_enroll` (`eitri remote add-device`): this window makes a one-time code, keeps it
//! where the readers can check it, and shows it in the base page as a QR code with the pairing URI. A phone's
//! `enroll` is checked on the reader's thread: the code it presents is taken there (once, in time, and only if it is
//! this window's), so no code reaches the tick. A taken code asks the base page "Add device ...?"; only the desk's
//! `y` mints the token (256 random bits), writes the device (its hash, the grant chosen, how it connects, the desk's
//! confirmation and the `Host` it enrolled through) to the store through `remote_devices::update` on a thread of its
//! own, and then sends `enrolled` with the token, once, to the connection that asked; a connection gone by then
//! leaves no device behind. Anything else is `enroll_refused`, and nothing is written.
//!
//! A `listen_request` names an address `remote.listen` asks for: it is read on the reader's thread (one IP address,
//! no wildcard, held by one of this machine's links, whose kind is read from the link). An address the desk already
//! said yes to on a link of the same kind is answered yes at once, whichever window asks; any other asks the base
//! page, telling it when the link is not a tunnel, and a `y` is recorded beside the store before `listen_answer` says
//! yes. Anything not asked or not answered is no.
//!
//! A `grant_request` (`eitri remote grant`) is read against the store on the reader's thread: an unknown device, or a
//! wider grant for a phone of the SSH recipe, is refused there, and the device's present grant is answered yes at
//! once. Any other change asks the base page; only the desk's `y` changes the grant, through `remote_devices::update`
//! off the tick, and only while the device is still there with the grant it had when the desk was asked.
//!
//! A phone's `revoke_self` and `set_push_endpoint` carry its token: the token is checked against the store on the
//! reader's thread and dropped there, so only the device id goes on. A push endpoint is checked there too (`http` or
//! `https`, a host, no user information, at most [`PUSH_ENDPOINT_MAX_BYTES`]). Revoking takes the device out of the
//! store, and its push endpoint with it. At most [`PHONE_ERRANDS_MAX`] of these are at work at once, each on a thread
//! that may wait for the store's lock; one more is answered [`ERRANDS_BUSY`] at once.
//!
//! `devices_connected` is `serve`'s report, sent on every open and close, and written on the reader's thread in the
//! order it came: the devices it names get `last_seen` and `last_host`, and those named by the previous report and not
//! this one get `last_seen` (the close). Nothing is answered to a report. A device in the store with no desk's yes
//! recorded (written by something other than a window's enrollment) is shown to the base page once per process. Every
//! report also goes to every chat page whole (`remote_connected`: who is connected, with name, grant and `Host`; one
//! entry per device however many connections it holds), so the status band counts the phones that can watch; a report
//! that names nobody clears the mark. The window keeps the last list, and a document that says `ready` is told it.

use super::*;
use crate::agent_panel::remote_enroll::{
    ask_enroll, ask_listen, qr_rows, show_pairing, EnrollAsk, EnrollReply, ListenAsk, PairingShown,
    REMOTE_FIELD_MAX_BYTES,
};
use crate::agent_panel::remote_enroll::{
    ask_grant, grants_for, show_connected, show_flagged, ConnectedShown, GrantAsk, SSH_ONLY_WATCHES,
};
use eitri_core::remote_devices::{Confirmed, Device, DeviceStore, Recipe};
use eitri_core::remote_pairing::{
    base64url, confirmed_listen_with_kind, interface_kind, interface_of, named_listen_addr, pairing_uri, random_fill,
    record_listen, ssh_host_key_fingerprints, Endpoint, EnrollCode, EnrollCodes, IfaceKind, PairMode, PairingRoutes,
};
use eitri_core::remote_rv::{Connected, RvOutbound, Secret, RV_PROTOCOL};
use sha2::Digest;
use std::collections::BTreeSet;
use std::net::IpAddr;
use std::sync::Mutex;

/// What a `start_enroll` is told while this window has nowhere to send phones.
const NO_ENDPOINT: &str = "remote control has no address for phones in this window";

/// What a phone is told when it says it reached `serve` by another recipe than its code's.
pub(super) const RECIPE_DIFFERS: &str = "the phone says it reached the desk by another recipe than the code it shows: \
                                         show a code for that recipe (eitri remote add-device --ssh or --direct)";

/// What a phone presenting an SSH code over a connection that did not come through an SSH forward is told.
pub(super) const SSH_CODE_NOT_FORWARDED: &str =
    "the code is for the SSH recipe, and this connection did not come through an SSH forward on the desk";

/// What a phone is told when its name, `Host` or address is empty or too long to show.
const BAD_ENROLL: &str = "the enrollment names no device, or names it at more length than the desk shows";

/// What the phone is told when the store could not take the device.
const NOT_STORED: &str = "the desk could not store the device";

/// What a grant request or a phone naming a device the store does not hold is told.
const NO_SUCH_DEVICE: &str = "no such device";

/// What a phone is told when its device id and token are not a pair the store holds.
const NOT_THIS_DEVICE: &str = "the device and token are not a device of this desk";

/// What is told when the store could not be changed.
const NOT_CHANGED: &str = "the desk could not change the device store";

/// What a grant request is told for a device whose store entry does not say how it connects.
pub(super) const UNKNOWN_RECIPE_ONLY_WATCHES: &str = "a device whose recipe is not known may only watch";

/// What a grant change is told when the device's grant changed while the desk was asked.
const GRANT_MOVED: &str = "the device's grant changed while the desk was asked";

/// The phone-sent errands (a self-revocation, a push endpoint) that may be at work on the store at once. Each waits
/// for the store's lock on a thread of its own, so a phone that repeats them while the lock is held would otherwise
/// leave a thread behind for every one.
pub(super) const PHONE_ERRANDS_MAX: usize = 4;

/// What a phone-sent errand is told when [`PHONE_ERRANDS_MAX`] are already at work.
pub(super) const ERRANDS_BUSY: &str = "the desk is still changing the device store: try again";

/// The longest push endpoint kept.
pub const PUSH_ENDPOINT_MAX_BYTES: usize = 512;

/// What a phone is told when its push endpoint is not one.
const BAD_PUSH_ENDPOINT: &str =
    "a push endpoint is an http or https URL of at most 512 bytes, with a host and no user information";

/// The devices flagged in this process, by id: each is shown once, whichever window reads it first.
static FLAGGED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// The bytes of a device's token, and of its id's random part.
const TOKEN_BYTES: usize = 32;
const DEVICE_ID_BYTES: usize = 8;

/// The link an address is on: its interface's name and kind, or `None` when no link holds it.
type LinkOf = dyn Fn(IpAddr) -> Option<(String, IfaceKind)> + Send + Sync;

/// Where phones connect, by each recipe the window has, and where the desk's SSH host keys are read for the SSH one.
#[derive(Debug, Clone)]
pub struct Pairing {
    pub routes: PairingRoutes,
    pub ssh_dir: PathBuf,
}

/// The code on show and where it sends phones: an enrollment is stored under that code's recipe, never under what
/// the window would pair with by the time the phone presents it.
struct Issued {
    codes: EnrollCodes,
    endpoint: Option<Endpoint>,
}

/// What the errands share across the accepting, reading and writing threads.
pub(super) struct DevicesShared {
    instance: String,
    issued: Mutex<Issued>,
    pairing: Mutex<Option<Pairing>>,
    link_of: Mutex<Arc<LinkOf>>,
    /// The devices `serve`'s last report named, so the next tells which closed.
    connected: Mutex<BTreeSet<String>>,
    /// The phone-sent errands at work now, at most [`PHONE_ERRANDS_MAX`].
    pub(super) phone_errands: Arc<AtomicUsize>,
}

impl DevicesShared {
    pub(super) fn new(instance: &str) -> DevicesShared {
        DevicesShared {
            instance: instance.to_owned(),
            issued: Mutex::new(Issued {
                codes: EnrollCodes::default(),
                endpoint: None,
            }),
            pairing: Mutex::new(None),
            link_of: Mutex::new(Arc::new(|addr| {
                let iface = interface_of(addr)?;
                let kind = interface_kind(&iface);
                Some((iface, kind))
            })),
            connected: Mutex::new(BTreeSet::new()),
            phone_errands: Arc::new(AtomicUsize::new(0)),
        }
    }
}

/// An errand as the reader hands it to the tick.
pub(super) enum DevicesInbound {
    /// A code was made and is to be shown; `add-device` is told where its phone goes.
    StartEnroll(PairingShown, Endpoint),
    /// A phone presented this window's code in time: the desk is to be asked.
    Enroll(EnrollAsk),
    /// The desk said yes and the device is stored: the token goes to the phone.
    Enrolled { device: String, token: Secret },
    /// The enrollment ends here, and the phone is told why.
    EnrollRefused(&'static str),
    /// The desk is to be asked about listening on an address.
    AskListen(ListenAsk),
    /// The answer about an address.
    ListenAnswer { addr: String, yes: bool },
    /// Turned down before anything was done, and why.
    Refused(&'static str),
    /// The desk is to be asked about a grant.
    AskGrant(GrantAsk),
    /// The errand is over: `serve` is told how it went.
    Done(Result<(), &'static str>),
    /// The device whose token was checked takes itself out.
    RevokeSelf { device: String },
    /// The device whose token was checked names its push endpoint, already checked.
    SetPushEndpoint { device: String, url: Secret },
    /// `serve`'s report was written: the devices with no desk's yes not shown before in this process (`(id, name)`),
    /// and who is connected now, for the base page to count and name.
    Reported {
        flagged: Vec<(String, String)>,
        connected: Vec<ConnectedShown>,
    },
}

impl DevicesInbound {
    /// Reads an errand on the reader's thread; `None` for any other line.
    pub(super) fn read(line: RvInbound, shared: &Shared) -> Option<DevicesInbound> {
        let devices = &shared.devices;
        Some(match line {
            RvInbound::StartEnroll { recipe, .. } => start_enroll(devices, recipe, Instant::now()),
            RvInbound::Enroll {
                code,
                device_name,
                host,
                peer,
                recipe,
                ..
            } => {
                let fits = |text: &str| !text.is_empty() && text.len() <= REMOTE_FIELD_MAX_BYTES;
                if !(fits(&device_name) && fits(&host) && fits(&peer)) {
                    return Some(DevicesInbound::EnrollRefused(BAD_ENROLL));
                }
                let mut issued = devices.issued.lock().unwrap_or_else(|e| e.into_inner());
                match issued.codes.take(code.expose(), Instant::now()) {
                    Ok(_) => {
                        let Some(endpoint) = issued.endpoint.take() else {
                            return Some(DevicesInbound::EnrollRefused(NO_ENDPOINT));
                        };
                        if let Err(why) = transport_matches(&endpoint, recipe, &peer) {
                            return Some(DevicesInbound::EnrollRefused(why));
                        }
                        DevicesInbound::Enroll(EnrollAsk {
                            name: device_name,
                            host,
                            peer,
                            mode: endpoint.mode,
                            target: endpoint.describe(),
                        })
                    }
                    Err(refused) => DevicesInbound::EnrollRefused(refused.why()),
                }
            }
            RvInbound::ListenRequest { addr, .. } => read_listen(shared, addr),
            RvInbound::GrantRequest { device, grant, .. } => read_grant(&shared.store, device, grant),
            RvInbound::RevokeSelf { device, token, .. } => match verified(&shared.store, &device, &token) {
                Ok(()) => DevicesInbound::RevokeSelf { device },
                Err(why) => DevicesInbound::Done(Err(why)),
            },
            RvInbound::SetPushEndpoint { device, token, url, .. } => {
                if let Err(why) = push_endpoint_ok(url.expose()) {
                    return Some(DevicesInbound::Done(Err(why)));
                }
                match verified(&shared.store, &device, &token) {
                    Ok(()) => DevicesInbound::SetPushEndpoint { device, url },
                    Err(why) => DevicesInbound::Done(Err(why)),
                }
            }
            // Written here, on the reader's thread, so two reports are written in the order `serve` sent them.
            RvInbound::DevicesConnected { devices, .. } => record_connected(shared, devices, unix_now()),
            _ => return None,
        })
    }
}

/// Whether a phone that presented a code sending it to `endpoint` came the way the code says: it says it used the
/// code's recipe, and a code for the SSH recipe came through an SSH forward, which `serve` sees as a loopback peer or
/// its Unix socket. A direct code cannot be checked the same way (a proxy on the desk is a loopback peer too), so the
/// desk's question names where the code sends phones.
fn transport_matches(endpoint: &Endpoint, recipe: PairMode, peer: &str) -> Result<(), &'static str> {
    if recipe != endpoint.mode {
        return Err(RECIPE_DIFFERS);
    }
    let forwarded = peer == "unix"
        || peer
            .parse::<std::net::SocketAddr>()
            .is_ok_and(|addr| addr.ip().to_canonical().is_loopback());
    if endpoint.mode == PairMode::Ssh && !forwarded {
        return Err(SSH_CODE_NOT_FORWARDED);
    }
    Ok(())
}

/// Makes a code for the route `recipe` asks for (the window's default with `None`), keeps it for the readers and
/// builds what the page shows.
fn start_enroll(devices: &DevicesShared, recipe: Option<PairMode>, now: Instant) -> DevicesInbound {
    let Some(pairing) = devices.pairing.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
        return DevicesInbound::Refused(NO_ENDPOINT);
    };
    let endpoint = match pairing.routes.pick(recipe) {
        Ok(endpoint) => endpoint.clone(),
        Err(why) => return DevicesInbound::Refused(why),
    };
    let Ok(code) = EnrollCode::new(&devices.instance, now) else {
        return DevicesInbound::Refused("no random bytes for a code");
    };
    let keys = match endpoint.mode {
        PairMode::Ssh => ssh_host_key_fingerprints(&pairing.ssh_dir),
        PairMode::Direct => Vec::new(),
    };
    let uri = pairing_uri(&code, &endpoint, &keys);
    let Some(modules) = qr_rows(&uri) else {
        return DevicesInbound::Refused("the pairing address is too long for a QR code");
    };
    let shown = PairingShown {
        code: Secret::new(code.text()),
        uri,
        modules,
    };
    let mut issued = devices.issued.lock().unwrap_or_else(|e| e.into_inner());
    issued.codes.issue(code);
    issued.endpoint = Some(endpoint.clone());
    DevicesInbound::StartEnroll(shown, endpoint)
}

/// The store at `store`, or `None` (said why) when it cannot be read; a missing store holds no device.
fn read_store(store: &Path) -> Option<DeviceStore> {
    match DeviceStore::load(store) {
        Ok(store) => Some(store),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Some(DeviceStore::default()),
        Err(e) => {
            eprintln!("[agent_panel] remote: the device store was not read: {e}");
            None
        }
    }
}

/// Whether `token` is the token of `device` in the store. Read on the reader's thread, so the token goes no further.
fn verified(store: &Path, device: &str, token: &Secret) -> Result<(), &'static str> {
    let known = read_store(store).and_then(|store| store.verify(device, token.expose()));
    known.map(|_| ()).ok_or(NOT_THIS_DEVICE)
}

/// Reads a grant request: refused for an unknown device or a grant its recipe does not allow, answered yes when the
/// device already has it, else the desk is to be asked.
fn read_grant(store: &Path, device: String, to: Grant) -> DevicesInbound {
    if device.is_empty() || device.len() > REMOTE_FIELD_MAX_BYTES {
        return DevicesInbound::Done(Err(NO_SUCH_DEVICE));
    }
    let Some(store) = read_store(store) else {
        return DevicesInbound::Done(Err(NOT_CHANGED));
    };
    let Some(found) = store.get(&device) else {
        return DevicesInbound::Done(Err(NO_SUCH_DEVICE));
    };
    if found.grant == to {
        return DevicesInbound::Done(Ok(()));
    }
    // A device whose entry does not say how it connects might use SSH: it is offered what an SSH phone is.
    let (mode, refused) = match found.recipe {
        Recipe::Ssh => (PairMode::Ssh, SSH_ONLY_WATCHES),
        Recipe::Unknown => (PairMode::Ssh, UNKNOWN_RECIPE_ONLY_WATCHES),
        Recipe::Direct => (PairMode::Direct, SSH_ONLY_WATCHES),
    };
    if !grants_for(mode).contains(&to) {
        return DevicesInbound::Done(Err(refused));
    }
    DevicesInbound::AskGrant(GrantAsk {
        name: found.name.clone(),
        from: found.grant,
        device,
        to,
    })
}

/// Whether `url` may be kept as a push endpoint: ASCII without spaces or control characters, at most
/// [`PUSH_ENDPOINT_MAX_BYTES`], `http://` or `https://`, then an authority with a host, no `@` (user information
/// would be a password kept beside the store's hashes), and a port of digits if one is given.
pub(super) fn push_endpoint_ok(url: &str) -> Result<(), &'static str> {
    let bad = Err(BAD_PUSH_ENDPOINT);
    if url.len() > PUSH_ENDPOINT_MAX_BYTES || !url.bytes().all(|b| b.is_ascii_graphic()) || url.contains('\\') {
        return bad;
    }
    let lower = url.to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix("https://").or_else(|| lower.strip_prefix("http://")) else {
        return bad;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return bad;
    }
    let (host, port) = match authority.strip_prefix('[') {
        Some(bracketed) => match bracketed.split_once(']') {
            Some((host, "")) => (host, None),
            Some((host, after)) => match after.strip_prefix(':') {
                Some(port) => (host, Some(port)),
                None => return bad,
            },
            None => return bad,
        },
        None => match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        },
    };
    if host.is_empty() || (!authority.starts_with('[') && host.contains(['[', ']'])) {
        return bad;
    }
    if let Some(port) = port {
        if !port.bytes().all(|b| b.is_ascii_digit()) || !matches!(port.parse::<u16>(), Ok(p) if p > 0) {
            return bad;
        }
    }
    Ok(())
}

/// Reads a listen request: the address, its link, and whether the desk already said yes to it there.
fn read_listen(shared: &Shared, addr: String) -> DevicesInbound {
    if addr.len() > REMOTE_FIELD_MAX_BYTES {
        return DevicesInbound::Refused("the listen address is too long");
    }
    let no = |addr: String| DevicesInbound::ListenAnswer { addr, yes: false };
    let Some(ip) = named_listen_addr(&addr) else {
        eprintln!("[agent_panel] remote: {addr:?} is not one address to listen on");
        return no(addr);
    };
    let link_of = Arc::clone(&*shared.devices.link_of.lock().unwrap_or_else(|e| e.into_inner()));
    let Some((iface, kind)) = link_of(ip) else {
        eprintln!("[agent_panel] remote: no link of this machine holds {addr}");
        return no(addr);
    };
    let confirmed = shared
        .store
        .parent()
        .map(confirmed_listen_with_kind)
        .and_then(Result::ok)
        .flatten();
    if confirmed.as_ref().is_some_and(|(a, k)| *a == addr && *k == kind) {
        return DevicesInbound::ListenAnswer { addr, yes: true };
    }
    DevicesInbound::AskListen(ListenAsk { addr, iface, kind })
}

/// Hands `line` to the tick for `conn`, raising the wake. Waits while the readers' queue is full, so it runs on a
/// thread of its own, never the tick's.
fn hand_to_tick(shared: &Shared, conn: u64, line: DevicesInbound) {
    if shared.inbound.send(Inbound::Devices { conn, line }).is_ok() {
        (shared.wake)();
    }
}

/// Runs `work` on a thread of its own, then hands what it made to the tick for `conn`.
fn off_the_tick(shared: Arc<Shared>, conn: u64, work: impl FnOnce(&Shared) -> DevicesInbound + Send + 'static) {
    let spawned = std::thread::Builder::new().name("rv-devices".into()).spawn(move || {
        let line = work(&shared);
        hand_to_tick(&shared, conn, line);
    });
    if let Err(e) = spawned {
        eprintln!("[agent_panel] remote: no thread for a device errand: {e}");
    }
}

/// One of the [`PHONE_ERRANDS_MAX`] phone-sent errands at work, given back when dropped.
struct ErrandSlot(Arc<AtomicUsize>);

impl ErrandSlot {
    fn take(count: &Arc<AtomicUsize>) -> Option<ErrandSlot> {
        count
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < PHONE_ERRANDS_MAX).then_some(n + 1)
            })
            .ok()?;
        Some(ErrandSlot(Arc::clone(count)))
    }
}

impl Drop for ErrandSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The seconds since the Unix epoch now.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Mints a token and stores the device the desk said yes to; what the phone is told. Off the tick: the store's lock
/// may be held by another writer.
fn store_device(store: &Path, ask: &EnrollAsk, grant: Grant) -> DevicesInbound {
    let mut token = [0u8; TOKEN_BYTES];
    let mut id = [0u8; DEVICE_ID_BYTES];
    if random_fill(&mut token).and_then(|()| random_fill(&mut id)).is_err() {
        return DevicesInbound::EnrollRefused(NOT_STORED);
    }
    let token = base64url(&token);
    let id: String = std::iter::once("d".to_owned())
        .chain(id.iter().map(|b| format!("{b:02x}")))
        .collect();
    let device = Device {
        id: id.clone(),
        name: ask.name.clone(),
        token_sha256: sha2::Sha256::digest(token.as_bytes()).into(),
        grant,
        recipe: match ask.mode {
            PairMode::Ssh => Recipe::Ssh,
            PairMode::Direct => Recipe::Direct,
        },
        confirmed: Some(Confirmed { at: unix_now() }),
        push_endpoint: None,
        last_seen: None,
        last_host: None,
        enrolled_host: ask.host.clone(),
    };
    match eitri_core::remote_devices::update(store, |s| s.insert(device)) {
        Ok(true) => DevicesInbound::Enrolled {
            device: id,
            token: Secret::new(token),
        },
        Ok(false) => DevicesInbound::EnrollRefused(NOT_STORED),
        Err(e) => {
            eprintln!("[agent_panel] remote: the device store was not written: {e}");
            DevicesInbound::EnrollRefused(NOT_STORED)
        }
    }
}

/// Changes the grant of the device `ask` names to `to` after the desk's yes, if it is still there with the grant it
/// had when the desk was asked. Off the tick.
fn change_grant(store: &Path, ask: &GrantAsk, to: Grant) -> DevicesInbound {
    let changed = eitri_core::remote_devices::update(store, |s| match s.get_mut(&ask.device) {
        None => Err(NO_SUCH_DEVICE),
        Some(d) if d.grant != ask.from => Err(GRANT_MOVED),
        Some(d) => {
            d.grant = to;
            Ok(())
        }
    });
    DevicesInbound::Done(changed.unwrap_or_else(|e| {
        eprintln!("[agent_panel] remote: the grant was not written: {e}");
        Err(NOT_CHANGED)
    }))
}

/// Applies `change` to the device `device` in the store; [`NO_SUCH_DEVICE`] when it is gone. Off the tick.
fn change_device(store: &Path, device: &str, change: impl FnOnce(&mut DeviceStore)) -> DevicesInbound {
    let changed = eitri_core::remote_devices::update(store, |s| {
        if s.get(device).is_none() {
            return Err(NO_SUCH_DEVICE);
        }
        change(s);
        Ok(())
    });
    DevicesInbound::Done(changed.unwrap_or_else(|e| {
        eprintln!("[agent_panel] remote: the device store was not written: {e}");
        Err(NOT_CHANGED)
    }))
}

/// Writes `serve`'s report to the store at `now` (seconds since the Unix epoch): `last_seen` and `last_host` for the
/// devices connected, `last_seen` for those the last report named and this one does not. Returns the devices with no
/// desk's yes that this process has not shown yet. On the reader's thread: it may wait for the store's lock.
fn record_connected(shared: &Shared, connected: Vec<Connected>, now: u64) -> DevicesInbound {
    let mut previous = shared.devices.connected.lock().unwrap_or_else(|e| e.into_inner());
    let named: BTreeSet<String> = connected.iter().map(|c| c.device.clone()).collect();
    let written = eitri_core::remote_devices::update(&shared.store, |s| {
        for c in &connected {
            if let Some(d) = s.get_mut(&c.device) {
                d.last_seen = Some(now);
                if !c.host.is_empty() && c.host.len() <= REMOTE_FIELD_MAX_BYTES {
                    d.last_host = Some(c.host.clone());
                }
            }
        }
        for closed in previous.difference(&named) {
            if let Some(d) = s.get_mut(closed) {
                d.last_seen = Some(now);
            }
        }
        let unconfirmed = s
            .devices()
            .iter()
            .filter(|d| d.confirmed.is_none())
            .map(|d| (d.id.clone(), d.name.clone()))
            .collect::<Vec<_>>();
        (unconfirmed, shown_connected(s, &connected))
    });
    *previous = named;
    match written {
        Ok((unconfirmed, shown)) => {
            let mut flagged = FLAGGED.lock().unwrap_or_else(|e| e.into_inner());
            DevicesInbound::Reported {
                flagged: unconfirmed
                    .into_iter()
                    .filter(|(id, _)| flagged.insert(id.clone()))
                    .collect(),
                connected: shown,
            }
        }
        Err(e) => {
            eprintln!("[agent_panel] remote: the connected devices were not recorded: {e}");
            // The store could not be read for names, but the desk must still see that someone can watch.
            DevicesInbound::Reported {
                flagged: Vec::new(),
                connected: one_per_device(&connected).iter().map(ConnectedShown::unnamed).collect(),
            }
        }
    }
}

/// `report` with one entry per device, in the order the devices first appear: a phone whose connections overlap (a
/// reconnect before the old socket closed) is one device, shown with its oldest connection.
fn one_per_device(report: &[Connected]) -> Vec<Connected> {
    let mut one: Vec<Connected> = Vec::new();
    for c in report {
        match one.iter_mut().find(|seen| seen.device == c.device) {
            Some(seen) => {
                if c.since < seen.since {
                    *seen = c.clone();
                }
            }
            None => one.push(c.clone()),
        }
    }
    one
}

/// The devices of `report` the store still holds, with their names and grants. A device the store no longer holds
/// (revoked since `serve` sent the report) is not shown: `serve` closes it and says so in the next report.
fn shown_connected(store: &DeviceStore, report: &[Connected]) -> Vec<ConnectedShown> {
    one_per_device(report)
        .iter()
        .filter_map(|c| {
            store.get(&c.device).map(|d| ConnectedShown {
                device: d.id.clone(),
                name: d.name.clone(),
                grant: d.grant,
                host: c.host.clone(),
                since: c.since,
            })
        })
        .collect()
}

impl RemoteSocket {
    /// Where this window sends phones: the endpoint a pairing URI names, and the directory the desk's SSH host keys
    /// are read from for the SSH recipe. Until it is set, `start_enroll` is refused.
    pub fn set_pairing(&self, pairing: Pairing) {
        *self.shared.devices.pairing.lock().unwrap_or_else(|e| e.into_inner()) = Some(pairing);
    }

    /// Carries out an errand on the tick.
    pub(super) fn take_devices(&self, handle: &AgentPanelHandle, conn: u64, line: DevicesInbound) {
        match line {
            DevicesInbound::Refused(why) => self.answer(conn, refused_line(None, why)),
            DevicesInbound::Done(outcome) => self.answer(conn, result_line(None, outcome)),
            DevicesInbound::AskGrant(ask) => {
                let shared = Arc::clone(&self.shared);
                let asked = ask.clone();
                let reply = Box::new(move |answer: EnrollReply| match answer {
                    EnrollReply::No(why) => off_the_tick(shared, conn, move |_| DevicesInbound::Done(Err(why))),
                    EnrollReply::Yes(to) => {
                        off_the_tick(shared, conn, move |shared| change_grant(&shared.store, &asked, to))
                    }
                });
                let outcome = handle.run(|state, out| ask_grant(state, ask, Instant::now(), reply, out));
                if let Err(why) = outcome {
                    self.answer(conn, result_line(None, Err(why)));
                }
            }
            DevicesInbound::RevokeSelf { device } => self.phone_errand(conn, move |shared| {
                change_device(&shared.store, &device, |s| {
                    s.remove(&device);
                })
            }),
            DevicesInbound::SetPushEndpoint { device, url } => self.phone_errand(conn, move |shared| {
                change_device(&shared.store, &device, |s| {
                    if let Some(d) = s.get_mut(&device) {
                        d.push_endpoint = Some(url.expose().to_owned());
                    }
                })
            }),
            DevicesInbound::Reported { flagged, connected } => {
                for (device, name) in flagged {
                    eprintln!("[agent_panel] remote: device {device:?} is in the device store with no desk's yes");
                    handle.run(|_state, out| show_flagged(&device, &name, out));
                }
                handle.run(|state, out| show_connected(state, &connected, out));
            }
            DevicesInbound::StartEnroll(shown, endpoint) => {
                handle.run(|_state, out| show_pairing(&shown, out));
                let line = RvOutbound::EnrollShown {
                    v: RV_PROTOCOL,
                    recipe: endpoint.mode,
                    target: endpoint.describe(),
                };
                self.answer(conn, line.line());
            }
            DevicesInbound::Enroll(ask) => {
                let shared = Arc::clone(&self.shared);
                let asked = ask.clone();
                let reply = Box::new(move |answer: EnrollReply| match answer {
                    EnrollReply::No(why) => off_the_tick(shared, conn, move |_| DevicesInbound::EnrollRefused(why)),
                    EnrollReply::Yes(grant) => {
                        off_the_tick(shared, conn, move |shared| store_device(&shared.store, &asked, grant))
                    }
                });
                let outcome = handle.run(|state, out| ask_enroll(state, ask, Instant::now(), reply, out));
                if let Err(why) = outcome {
                    self.answer(conn, enroll_refused_line(why));
                }
            }
            DevicesInbound::Enrolled { device, token } => {
                if !self.conns.borrow().contains_key(&conn) {
                    // The phone left before its token could reach it: nobody holds the token, so the device goes.
                    let store = self.shared.store.clone();
                    let spawned = std::thread::Builder::new().name("rv-devices".into()).spawn(move || {
                        if let Err(e) = eitri_core::remote_devices::update(&store, |s| s.remove(&device)) {
                            eprintln!("[agent_panel] remote: a device nobody holds was not removed: {e}");
                        }
                    });
                    if let Err(e) = spawned {
                        eprintln!("[agent_panel] remote: no thread to remove a device nobody holds: {e}");
                    }
                    return;
                }
                let line = RvOutbound::Enrolled {
                    v: RV_PROTOCOL,
                    device,
                    token,
                }
                .line();
                self.answer(conn, line);
            }
            DevicesInbound::EnrollRefused(why) => self.answer(conn, enroll_refused_line(why)),
            DevicesInbound::AskListen(ask) => {
                let shared = Arc::clone(&self.shared);
                let asked = ask.clone();
                let reply = Box::new(move |yes: bool| {
                    off_the_tick(shared, conn, move |shared| {
                        let recorded = yes
                            && shared.store.parent().is_some_and(|dir| {
                                record_listen(dir, &asked.addr, asked.kind, unix_now())
                                    .map_err(|e| {
                                        eprintln!("[agent_panel] remote: the listen yes was not recorded: {e}")
                                    })
                                    .is_ok()
                            });
                        DevicesInbound::ListenAnswer {
                            addr: asked.addr,
                            yes: recorded,
                        }
                    })
                });
                let addr = ask.addr.clone();
                let outcome = handle.run(|state, out| ask_listen(state, ask, Instant::now(), reply, out));
                if outcome.is_err() {
                    self.answer(conn, listen_answer_line(addr, false));
                }
            }
            DevicesInbound::ListenAnswer { addr, yes } => self.answer(conn, listen_answer_line(addr, yes)),
        }
    }

    /// Runs a phone-sent errand off the tick when fewer than [`PHONE_ERRANDS_MAX`] are at work, else answers
    /// [`ERRANDS_BUSY`] at once. The desk's answers are not counted: one question is asked at a time.
    fn phone_errand(&self, conn: u64, work: impl FnOnce(&Shared) -> DevicesInbound + Send + 'static) {
        let Some(slot) = ErrandSlot::take(&self.shared.devices.phone_errands) else {
            self.answer(conn, result_line(None, Err(ERRANDS_BUSY)));
            return;
        };
        // The slot goes with the closure: when the work is done, or when no thread could take it.
        off_the_tick(Arc::clone(&self.shared), conn, move |shared| {
            let line = work(shared);
            drop(slot);
            line
        });
    }

    /// Reads which link holds an address with `link_of` in place of this machine's own links.
    #[cfg(test)]
    pub(super) fn set_link_of(&self, link_of: impl Fn(IpAddr) -> Option<(String, IfaceKind)> + Send + Sync + 'static) {
        *self.shared.devices.link_of.lock().unwrap_or_else(|e| e.into_inner()) = Arc::new(link_of);
    }
}

fn enroll_refused_line(why: &str) -> String {
    RvOutbound::EnrollRefused {
        v: RV_PROTOCOL,
        why: why.to_owned(),
    }
    .line()
}

fn listen_answer_line(addr: String, yes: bool) -> String {
    RvOutbound::ListenAnswer {
        v: RV_PROTOCOL,
        addr,
        yes,
    }
    .line()
}

#[cfg(test)]
#[path = "devices_tests.rs"]
mod tests;
