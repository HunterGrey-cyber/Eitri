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
//! `https`, a host, no user information, at most [`eitri_core::remote_devices::PUSH_ENDPOINT_MAX_BYTES`]). Revoking
//! takes the device out of the store, and its push endpoint with it. At most [`PHONE_ERRANDS_MAX`] of these are at work at once, each on a thread
//! that may wait for the store's lock; one more is answered [`ERRANDS_BUSY`] at once.
//!
//! `devices_connected` is `serve`'s report, sent on every open and close, and written on the reader's thread in the
//! order it came: the devices it names get `last_seen` and `last_host`, and those named by the previous report and not
//! this one get `last_seen` (the close). Nothing is answered to a report. A device in the store with no desk's yes
//! recorded (written by something other than a window's enrollment) is shown to the base page once per process. Every
//! report also goes to every chat page whole (`remote_connected`: who is connected, with name, grant and `Host`; one
//! entry per device however many connections it holds), so the status band counts the phones that can watch; a report
//! that names nobody clears the mark. The window keeps the last list, and a document that says `ready` is told it.
//!
//! The base page's device list (`remote_enroll::DeskRequest`, taken at each pump) is carried out on threads of its
//! own: the list reads the store and each device's last [`DEVICE_RECORDS`] audit records (`remote_devices`; where a
//! device's notifications go is shown by host only), `a` makes a code exactly as `start_enroll` does, and `x` reads
//! the device's name from the store and asks the base page "Revoke <name>?". Only the desk's gated `y` to that takes
//! the device out, through `remote_devices::update` off the tick; it is recorded as `revoke` with `by: desk`, the list
//! is read again, and `serve`, which watches the store, closes the phone. What the page asked for that cannot be done
//! is said in the base page, since no connection asked.
//!
//! What only the window sees goes to its audit log: a code made (`enroll_offered`), a presented code that is not the
//! one on show or ran out (`refused` with `enroll_code_invalid`), a code that lapsed unused (`enroll_expired`: its time
//! ran out, read at the socket's upkeep every few seconds and at the next presented code, or a newer code replaced
//! it), the desk's `y` once the device is stored (`enroll_confirmed`), every other end of an enrollment
//! (`enroll_refused`, why as a word), the listen question's `y` once recorded (`listen_changed`), the grant question's
//! `y` once written (`grant_changed`), a phone's `revoke_self` once done (`revoke`, `by: device`) and a device found
//! with no desk's yes (`device_flagged`). A name, `Host` or peer a phone gave is stored as given; a code or a token
//! never is.

use super::*;
use crate::agent_panel::remote_enroll::{
    ask_enroll, ask_listen, qr_rows, show_pairing, EnrollAsk, EnrollReply, ListenAsk, PairingShown,
    REMOTE_FIELD_MAX_BYTES,
};
use crate::agent_panel::remote_enroll::{
    ask_grant, grants_for, show_connected, show_flagged, ConnectedShown, GrantAsk, SSH_ONLY_WATCHES,
};
use crate::agent_panel::remote_enroll::{ask_revoke, show_devices, DeskRequest, DeviceRow, RevokeAsk};
use crate::agent_panel::remote_enroll::{grant_word, ANOTHER_DEVICE_WAITS, THE_DESK_DID_NOT_ANSWER, THE_DESK_SAID_NO};
use eitri_core::remote_audit::{DeviceRef, Event as AuditEvent, Listener, Reason};
use eitri_core::remote_devices::{Confirmed, Device, DeviceStore, Recipe};
use eitri_core::remote_pairing::{
    base64url, confirmed_listen_with_kind, interface_kind, interface_of, named_listen_addr, pairing_uri, random_fill,
    record_listen, ssh_host_key_fingerprints, Endpoint, EnrollCode, EnrollCodes, IfaceKind, PairMode, PairingRoutes,
};
use eitri_core::remote_rv::{Connected, RvOutbound, Secret, RV_PROTOCOL};
use sha2::Digest;
use std::collections::{BTreeSet, HashMap};
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

/// What the base page is told when the device store cannot be read for the list.
const STORE_UNREAD: &str = "the device store could not be read";

/// The newest audit records the device list shows for each device.
pub(super) const DEVICE_RECORDS: usize = 20;

/// The phone-sent errands (a self-revocation, a push endpoint) that may be at work on the store at once. Each waits
/// for the store's lock on a thread of its own, so a phone that repeats them while the lock is held would otherwise
/// leave a thread behind for every one.
pub(super) const PHONE_ERRANDS_MAX: usize = 4;

/// What a phone-sent errand is told when [`PHONE_ERRANDS_MAX`] are already at work.
pub(super) const ERRANDS_BUSY: &str = "the desk is still changing the device store: try again";

use eitri_core::remote_devices::push_endpoint_ok;
#[cfg(test)]
pub(super) use eitri_core::remote_devices::{BAD_PUSH_ENDPOINT, PUSH_ENDPOINT_MAX_BYTES};

/// The devices flagged in this process, by id: each is shown once, whichever window reads it first.
static FLAGGED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// The bytes of a device's token, and of its id's random part.
const TOKEN_BYTES: usize = 32;
const DEVICE_ID_BYTES: usize = 8;

/// The link an address is on: its interface's name and kind, or `None` when no link holds it.
type LinkOf = dyn Fn(IpAddr) -> Option<(String, IfaceKind)> + Send + Sync;

/// What reads the device list for the store at a path ([`device_rows`], a test's stand-in).
type ListReader = dyn Fn(&Path) -> DevicesInbound + Send + Sync;

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
    /// When the code on show runs out, until it is presented or its lapse is recorded.
    lapses: Option<Instant>,
}

/// Records `enroll_expired` once when the code on show ran out unused by `now`.
fn note_lapsed(issued: &mut Issued, audit: &AuditSink, instance: &str, now: Instant) {
    if issued.lapses.is_some_and(|at| now >= at) {
        issued.lapses = None;
        audit.record(AuditEvent::EnrollExpired {
            instance: instance.to_owned(),
        });
    }
}

/// An enrolling phone as the log names it: by the name it claims, since it has no id yet.
fn claimed(name: &str) -> DeviceRef {
    DeviceRef {
        id: String::new(),
        name: name.to_owned(),
    }
}

/// `enroll_refused` for the phone `ask` describes, `why` a word.
fn enroll_refused(audit: &AuditSink, ask: &EnrollAsk, why: &str) {
    audit.record(AuditEvent::EnrollRefused {
        device: claimed(&ask.name),
        recipe: ask.mode.as_str().to_owned(),
        host: ask.host.clone(),
        peer: ask.peer.clone(),
        why: why.to_owned(),
    });
}

/// The log's word for what a phone was told when its enrollment ended without a device.
fn refusal_word(why: &str) -> &'static str {
    match why {
        THE_DESK_SAID_NO => "desk_said_no",
        THE_DESK_DID_NOT_ANSWER => "desk_did_not_answer",
        ANOTHER_DEVICE_WAITS => "another_device_waits",
        NOT_STORED => "not_stored",
        RECIPE_DIFFERS => "recipe_differs",
        SSH_CODE_NOT_FORWARDED => "ssh_code_not_forwarded",
        NO_ENDPOINT => "no_endpoint",
        BAD_ENROLL => "bad_fields",
        _ => "refused",
    }
}

/// Whether the device list is being read, and whether it was asked for again meanwhile.
#[derive(Default)]
struct Listing {
    running: bool,
    again: bool,
    generation: u64,
    shown: u64,
}

impl Listing {
    fn take_generation(&mut self, generation: u64) -> bool {
        if generation < self.shown {
            return false;
        }
        self.shown = generation;
        true
    }
}

fn show_listed(listing: &Mutex<Listing>, handle: &AgentPanelHandle, generation: u64, rows: &[DeviceRow]) {
    if lock(listing).take_generation(generation) {
        handle.run(|state, out| show_devices(state, rows, out));
    }
}

/// What the errands share across the accepting, reading and writing threads.
pub(super) struct DevicesShared {
    pub(super) report_order: Mutex<()>,
    /// The `(epoch, seq)` of the last report taken: an older one, read late off another connection, is dropped.
    last_report: Mutex<Option<(u64, u64)>>,
    instance: String,
    issued: Mutex<Issued>,
    pairing: Mutex<Option<Pairing>>,
    link_of: Mutex<Arc<LinkOf>>,
    list_reader: Mutex<Arc<ListReader>>,
    /// The device list's read: at most one runs, and the requests made while it runs are answered by one more.
    listing: Mutex<Listing>,
    /// The devices `serve`'s last report named, so the next tells which closed.
    connected: Mutex<BTreeSet<String>>,
    /// The phone-sent errands at work now, at most [`PHONE_ERRANDS_MAX`].
    pub(super) phone_errands: Arc<AtomicUsize>,
    #[cfg(test)]
    workers: Arc<AtomicUsize>,
}

impl DevicesShared {
    pub(super) fn new(instance: &str) -> DevicesShared {
        DevicesShared {
            report_order: Mutex::new(()),
            last_report: Mutex::new(None),
            instance: instance.to_owned(),
            issued: Mutex::new(Issued {
                codes: EnrollCodes::default(),
                endpoint: None,
                lapses: None,
            }),
            pairing: Mutex::new(None),
            link_of: Mutex::new(Arc::new(|addr| {
                let iface = interface_of(addr)?;
                let kind = interface_kind(&iface);
                Some((iface, kind))
            })),
            list_reader: Mutex::new(Arc::new(device_rows)),
            listing: Mutex::new(Listing::default()),
            connected: Mutex::new(BTreeSet::new()),
            phone_errands: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            workers: Arc::new(AtomicUsize::new(0)),
        }
    }
}

/// An errand as the reader hands it to the tick.
pub(super) enum DevicesInbound {
    /// A code was made and is to be shown; `add-device` is told where its phone goes, and whether the window has a
    /// route by the other recipe.
    StartEnroll(PairingShown, Endpoint, bool),
    /// A phone presented this window's code in time: the desk is to be asked.
    Enroll(EnrollAsk),
    /// The desk said yes and the device (`name`) is stored: the token goes to the phone.
    Enrolled {
        device: String,
        name: String,
        token: Secret,
    },
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
    /// The device whose token was checked (the store's `name` for it) takes itself out.
    RevokeSelf { device: String, name: String },
    /// The device whose token was checked names its push endpoint, already checked.
    SetPushEndpoint { device: String, url: Secret },
    /// The device list, read for the base page.
    Listed(u64, Vec<DeviceRow>),
    /// The base page is to be asked whether to revoke a device.
    AskRevoke(RevokeAsk),
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
            RvInbound::StartEnroll { recipe, .. } => start_enroll(devices, &shared.audit, recipe, Instant::now()),
            RvInbound::Enroll {
                code,
                device_name,
                host,
                peer,
                recipe,
                ..
            } => {
                let audit = &shared.audit;
                let refused_as = |mode: PairMode, why: &'static str| {
                    audit.record(AuditEvent::EnrollRefused {
                        device: claimed(&device_name),
                        recipe: mode.as_str().to_owned(),
                        host: host.clone(),
                        peer: peer.clone(),
                        why: refusal_word(why).to_owned(),
                    });
                    Some(DevicesInbound::EnrollRefused(why))
                };
                let fits = |text: &str| !text.is_empty() && text.len() <= REMOTE_FIELD_MAX_BYTES;
                if !(fits(&device_name) && fits(&host) && fits(&peer)) {
                    return refused_as(recipe, BAD_ENROLL);
                }
                let now = Instant::now();
                let mut issued = devices.issued.lock().unwrap_or_else(|e| e.into_inner());
                note_lapsed(&mut issued, audit, &devices.instance, now);
                match issued.codes.take(code.expose(), now) {
                    Ok(_) => {
                        issued.lapses = None;
                        let Some(endpoint) = issued.endpoint.take() else {
                            return refused_as(recipe, NO_ENDPOINT);
                        };
                        if let Err(why) = transport_matches(&endpoint, recipe, &peer) {
                            return refused_as(endpoint.mode, why);
                        }
                        DevicesInbound::Enroll(EnrollAsk {
                            name: device_name,
                            host,
                            peer,
                            mode: endpoint.mode,
                            target: endpoint.describe(),
                        })
                    }
                    Err(refused) => {
                        audit.record(AuditEvent::Refused {
                            reason: Reason::EnrollCodeInvalid,
                            device: Some(claimed(&device_name)),
                            listener: None,
                            peer: Some(peer.clone()),
                            key: None,
                            kind: None,
                            dropped: None,
                        });
                        DevicesInbound::EnrollRefused(refused.why())
                    }
                }
            }
            RvInbound::ListenRequest { addr, .. } => read_listen(shared, addr),
            RvInbound::GrantRequest { device, grant, .. } => read_grant(&shared.store, device, grant),
            RvInbound::RevokeSelf { device, token, .. } => match verified(&shared.store, &device, &token) {
                Ok(name) => DevicesInbound::RevokeSelf { device, name },
                Err(why) => DevicesInbound::Done(Err(why)),
            },
            RvInbound::SetPushEndpoint { device, token, url, .. } => {
                if let Err(why) = push_endpoint_ok(url.expose()) {
                    return Some(DevicesInbound::Done(Err(why)));
                }
                match verified(&shared.store, &device, &token) {
                    Ok(_) => DevicesInbound::SetPushEndpoint { device, url },
                    Err(why) => DevicesInbound::Done(Err(why)),
                }
            }
            // Written here, on the reader's thread, so two reports are written in the order `serve` sent them.
            RvInbound::DevicesConnected {
                devices, epoch, seq, ..
            } => {
                // Under `report_order` (taken by the reader for this kind), so the check and the write are one step.
                let mut last = shared.devices.last_report.lock().unwrap_or_else(|e| e.into_inner());
                if last.is_some_and(|taken| (epoch, seq) <= taken) {
                    return None;
                }
                *last = Some((epoch, seq));
                drop(last);
                record_connected(shared, devices, unix_now())
            }
            _ => return None,
        })
    }
}

/// Whether a phone that presented a code sending it to `endpoint` agrees with it: it says it used the code's recipe,
/// and a code for the SSH recipe came from a loopback peer or `serve`'s Unix socket, as an SSH forward on the desk
/// does. This is a check of what the phone says against the code, not a proof of its transport: a proxy on the desk is
/// a loopback peer too, so a phone that names the code's recipe falsely is not caught here, and the desk's question
/// names where the code sends phones and where this one came from.
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
/// builds what the page shows. The code on show before it, unused, is recorded as lapsed.
fn start_enroll(devices: &DevicesShared, audit: &AuditSink, recipe: Option<PairMode>, now: Instant) -> DevicesInbound {
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
    // The code on show until now can no longer be presented: unused, it lapses here.
    if issued.lapses.take().is_some() {
        audit.record(AuditEvent::EnrollExpired {
            instance: devices.instance.clone(),
        });
    }
    issued.lapses = Some(code.expires);
    issued.codes.issue(code);
    issued.endpoint = Some(endpoint.clone());
    let other = match endpoint.mode {
        PairMode::Ssh => pairing.routes.direct.is_some(),
        PairMode::Direct => pairing.routes.ssh.is_some(),
    };
    DevicesInbound::StartEnroll(shown, endpoint, other)
}

/// The device list for the store at `store`: each device with its newest [`DEVICE_RECORDS`] records from the audit
/// log beside it. Off the tick: it reads every retained file of the log.
pub(super) fn device_rows(store: &Path) -> DevicesInbound {
    let Some(read) = read_store(store) else {
        return DevicesInbound::Refused(STORE_UNREAD);
    };
    let ids: Vec<&str> = read.devices().iter().map(|d| d.id.as_str()).collect();
    let mut records = store
        .parent()
        .map(|state| last_records(&eitri_core::remote_audit::audit_dir(state), &ids))
        .unwrap_or_default();
    let rows = read
        .devices()
        .iter()
        .map(|d| DeviceRow {
            id: d.id.clone(),
            name: d.name.clone(),
            grant: d.grant,
            recipe: match d.recipe {
                Recipe::Ssh => "ssh",
                Recipe::Direct => "direct",
                Recipe::Unknown => "unknown",
            },
            enrolled_at: d.confirmed.map(|c| c.at),
            last_seen: d.last_seen,
            last_host: d.last_host.clone(),
            push_host: d.push_endpoint.as_deref().and_then(push_host),
            records: records.remove(&d.id).unwrap_or_default(),
        })
        .collect();
    DevicesInbound::Listed(0, rows)
}

/// The newest records about each of `devices` in the log at `dir`, each as the page draws it, from one read of the
/// log; none when the log cannot be read (said on stderr).
fn last_records(dir: &Path, devices: &[&str]) -> HashMap<String, Vec<serde_json::Value>> {
    match eitri_core::remote_audit::newest_by_device(dir, devices, DEVICE_RECORDS) {
        Ok(found) => found
            .into_iter()
            .map(|(id, records)| (id, records.iter().map(record_shown).collect()))
            .collect(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => HashMap::new(),
        Err(e) => {
            eprintln!("[agent_panel] remote: the audit log was not read for the device list: {e}");
            HashMap::new()
        }
    }
}

/// A record as the device list draws it: its time, event and writer, and every field of its own as `[name, text]`
/// (a string as it is, anything else as its JSON).
fn record_shown(record: &eitri_core::remote_audit::Record) -> serde_json::Value {
    let fields: Vec<[String; 2]> = record
        .fields
        .iter()
        .map(|(name, value)| {
            let text = match value {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            [name.clone(), text]
        })
        .collect();
    serde_json::json!({
        "time": record.time,
        "event": record.event,
        "writer": record.writer,
        "fields": fields,
    })
}

/// The host (and port) of a push endpoint: its path and query can hold the topic that lets anyone post to the phone.
fn push_host(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    (!host.is_empty()).then(|| host.to_owned())
}

/// The store's name for `device`, for the revoke question, or why the desk cannot be asked.
fn read_revoke(store: &Path, device: String) -> DevicesInbound {
    let Some(read) = read_store(store) else {
        return DevicesInbound::Refused(STORE_UNREAD);
    };
    match read.get(&device) {
        Some(found) => DevicesInbound::AskRevoke(RevokeAsk {
            name: found.name.clone(),
            device,
        }),
        None => DevicesInbound::Refused(NO_SUCH_DEVICE),
    }
}

/// Takes `ask`'s device out of the store after the desk's yes, records it, and reads the list again. Off the tick.
fn revoke_by_desk(shared: &Shared, ask: &RevokeAsk) -> DevicesInbound {
    match eitri_core::remote_devices::update(&shared.store, |s| s.remove(&ask.device).is_some()) {
        Ok(true) => {
            shared.audit.record(AuditEvent::Revoke {
                device: DeviceRef {
                    id: ask.device.clone(),
                    name: ask.name.clone(),
                },
                by: "desk".into(),
            });
            let read = Arc::clone(&*shared.devices.list_reader.lock().unwrap_or_else(|e| e.into_inner()));
            read(&shared.store)
        }
        Ok(false) => DevicesInbound::Refused(NO_SUCH_DEVICE),
        Err(e) => {
            eprintln!("[agent_panel] remote: the revoke was not written: {e}");
            DevicesInbound::Refused(NOT_CHANGED)
        }
    }
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

/// The store's name for `device` when `token` is its token. Read on the reader's thread, so the token goes no further.
fn verified(store: &Path, device: &str, token: &Secret) -> Result<String, &'static str> {
    let known = read_store(store).and_then(|store| store.verify(device, token.expose()));
    known.map(|(name, _)| name).ok_or(NOT_THIS_DEVICE)
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

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Runs `work` on a thread of its own, then hands what it made to the tick for `conn`.
fn off_the_tick(shared: Arc<Shared>, conn: u64, work: impl FnOnce(&Shared) -> DevicesInbound + Send + 'static) {
    started_off_the_tick(shared, conn, work);
}

/// [`off_the_tick`], saying whether a thread could be had (when not, it is said on stderr and `work` never runs).
fn started_off_the_tick(
    shared: Arc<Shared>,
    conn: u64,
    work: impl FnOnce(&Shared) -> DevicesInbound + Send + 'static,
) -> bool {
    #[cfg(test)]
    let worker = {
        shared.devices.workers.fetch_add(1, Ordering::SeqCst);
        ErrandSlot(Arc::clone(&shared.devices.workers))
    };
    let spawned = std::thread::Builder::new().name("rv-devices".into()).spawn(move || {
        #[cfg(test)]
        let _worker = worker;
        let line = work(&shared);
        hand_to_tick(&shared, conn, line);
    });
    if let Err(e) = spawned {
        eprintln!("[agent_panel] remote: no thread for a device errand: {e}");
        return false;
    }
    true
}

/// Lets the list be read again when a read panics, which would otherwise leave it marked as running for good.
struct ListingReset<'a>(&'a Mutex<Listing>);

impl Drop for ListingReset<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let mut listing = lock(self.0);
            listing.running = false;
            listing.again = false;
        }
    }
}

/// Reads the device list for the desk off the tick. Each read takes the store and every retained file of the audit
/// log, so one runs at a time: a request made while it runs (`r` held down) is answered by a single read after it,
/// and only that newer list is drawn.
fn list_off_the_tick(shared: Arc<Shared>) {
    {
        let mut listing = lock(&shared.devices.listing);
        if listing.running {
            listing.again = true;
            return;
        }
        listing.running = true;
    }
    let mine = Arc::clone(&shared);
    let started = started_off_the_tick(shared, DESK, |shared| loop {
        let _reset = ListingReset(&shared.devices.listing);
        let read = Arc::clone(&*lock(&shared.devices.list_reader));
        let generation = {
            let mut listing = lock(&shared.devices.listing);
            listing.generation += 1;
            listing.generation
        };
        let line = match read(&shared.store) {
            DevicesInbound::Listed(_, rows) => DevicesInbound::Listed(generation, rows),
            other => other,
        };
        let mut listing = lock(&shared.devices.listing);
        if std::mem::take(&mut listing.again) {
            continue;
        }
        listing.running = false;
        return line;
    });
    if !started {
        let mut listing = lock(&mine.devices.listing);
        listing.running = false;
        listing.again = false;
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
            name: ask.name.clone(),
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
    record_connected_locked(shared, connected, now, &mut previous)
}

/// Checks serve's liveness off the tick. The phone-list lock covers the probe and the empty report, so a new report
/// cannot be erased by a check that began before it. No lock probe is requested while the list is empty.
pub(super) fn check_serve(shared: &Shared) {
    note_lapsed_now(shared, Instant::now());
    let _ordered = shared.devices.report_order.lock().unwrap_or_else(|e| e.into_inner());
    let mut previous = shared.devices.connected.lock().unwrap_or_else(|e| e.into_inner());
    let mut probe = shared.serve_probe.lock().unwrap_or_else(|e| e.into_inner());
    let Some(probe) = probe.as_mut() else { return };
    let (running, sandbox) = probe(!previous.is_empty());
    if let Some(sandbox) = sandbox {
        if shared.inbound.send(Inbound::Sandbox(sandbox)).is_ok() {
            (shared.wake)();
        }
    }
    if !previous.is_empty() && running == Some(false) {
        let line = record_connected_locked(shared, Vec::new(), unix_now(), &mut previous);
        hand_to_tick(shared, 0, line);
    }
}

/// Records the code on show as lapsed when it ran out unused by `now`.
fn note_lapsed_now(shared: &Shared, now: Instant) {
    let devices = &shared.devices;
    let mut issued = devices.issued.lock().unwrap_or_else(|e| e.into_inner());
    note_lapsed(&mut issued, &shared.audit, &devices.instance, now);
}

fn record_connected_locked(
    shared: &Shared,
    connected: Vec<Connected>,
    now: u64,
    previous: &mut BTreeSet<String>,
) -> DevicesInbound {
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
    /// Where this window sends phones: the routes a pairing URI names, and the directory the desk's SSH host keys are
    /// read from for the SSH recipe. Until it is set, `start_enroll` is refused.
    pub fn set_pairing(&self, pairing: Pairing) {
        *self.shared.devices.pairing.lock().unwrap_or_else(|e| e.into_inner()) = Some(pairing);
    }

    /// The routes this window's enrollment codes send phones by. Read off the tick, like setting them.
    pub fn pairing_routes(&self) -> Option<PairingRoutes> {
        self.shared
            .devices
            .pairing
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|p| p.routes.clone())
    }

    /// Starts what the base page asked of the device list on a thread of its own; what it finds comes back as an
    /// errand of the connection [`DESK`].
    pub(super) fn desk_request(&self, request: DeskRequest) {
        let shared = Arc::clone(&self.shared);
        match request {
            DeskRequest::List => list_off_the_tick(shared),
            DeskRequest::StartEnroll(recipe) => off_the_tick(shared, DESK, move |shared| {
                start_enroll(&shared.devices, &shared.audit, recipe, Instant::now())
            }),
            DeskRequest::RevokeAsk(device) => {
                off_the_tick(shared, DESK, move |shared| read_revoke(&shared.store, device))
            }
        }
    }

    /// Carries out an errand on the tick.
    pub(super) fn take_devices(&self, handle: &AgentPanelHandle, conn: u64, line: DevicesInbound) {
        match line {
            DevicesInbound::Refused(why) | DevicesInbound::Done(Err(why)) if conn == DESK => {
                let text = format!("Remote control: {why}");
                handle.run(|_state, out| {
                    out.send(ViewId::BASE, eitri_core::agent_bridge::serialize_notice_for_js(&text))
                });
            }
            DevicesInbound::Refused(why) => self.answer(conn, refused_line(None, why)),
            DevicesInbound::Done(outcome) => self.answer(conn, result_line(None, outcome)),
            DevicesInbound::Listed(generation, rows) => {
                show_listed(&self.shared.devices.listing, handle, generation, &rows);
            }
            DevicesInbound::AskRevoke(ask) => {
                let shared = Arc::clone(&self.shared);
                let asked = ask.clone();
                let reply =
                    Box::new(move |()| off_the_tick(shared, DESK, move |shared| revoke_by_desk(shared, &asked)));
                handle.run(|state, out| ask_revoke(state, ask, Instant::now(), reply, out));
            }
            DevicesInbound::AskGrant(ask) => {
                let shared = Arc::clone(&self.shared);
                let asked = ask.clone();
                let reply = Box::new(move |answer: EnrollReply| match answer {
                    EnrollReply::No(why) => off_the_tick(shared, conn, move |_| DevicesInbound::Done(Err(why))),
                    EnrollReply::Yes(to) => off_the_tick(shared, conn, move |shared| {
                        let changed = change_grant(&shared.store, &asked, to);
                        if matches!(changed, DevicesInbound::Done(Ok(()))) {
                            shared.audit.record(AuditEvent::GrantChanged {
                                device: DeviceRef {
                                    id: asked.device.clone(),
                                    name: asked.name.clone(),
                                },
                                from: grant_word(asked.from).to_owned(),
                                to: grant_word(to).to_owned(),
                            });
                        }
                        changed
                    }),
                });
                let outcome = handle.run(|state, out| ask_grant(state, ask, Instant::now(), reply, out));
                if let Err(why) = outcome {
                    self.answer(conn, result_line(None, Err(why)));
                }
            }
            DevicesInbound::RevokeSelf { device, name } => self.phone_errand(conn, move |shared| {
                let done = change_device(&shared.store, &device, |s| {
                    s.remove(&device);
                });
                if matches!(done, DevicesInbound::Done(Ok(()))) {
                    shared.audit.record(AuditEvent::Revoke {
                        device: DeviceRef { id: device, name },
                        by: "device".into(),
                    });
                }
                done
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
                    self.shared.audit.record(AuditEvent::DeviceFlagged {
                        device: DeviceRef {
                            id: device.clone(),
                            name: name.clone(),
                        },
                    });
                    handle.run(|_state, out| show_flagged(&device, &name, out));
                }
                handle.run(|state, out| show_connected(state, &connected, out));
            }
            DevicesInbound::StartEnroll(shown, endpoint, other) => {
                self.shared.audit.record(AuditEvent::EnrollOffered {
                    recipe: endpoint.mode.as_str().to_owned(),
                    instance: self.shared.devices.instance.clone(),
                });
                handle.run(|_state, out| show_pairing(&shown, out));
                let line = RvOutbound::EnrollShown {
                    v: RV_PROTOCOL,
                    recipe: endpoint.mode,
                    target: endpoint.describe(),
                    other,
                };
                self.answer(conn, line.line());
            }
            DevicesInbound::Enroll(ask) => {
                let shared = Arc::clone(&self.shared);
                let asked = ask.clone();
                let refused = ask.clone();
                let reply = Box::new(move |answer: EnrollReply| match answer {
                    EnrollReply::No(why) => {
                        enroll_refused(&shared.audit, &asked, refusal_word(why));
                        off_the_tick(shared, conn, move |_| DevicesInbound::EnrollRefused(why))
                    }
                    EnrollReply::Yes(grant) => off_the_tick(shared, conn, move |shared| {
                        let stored = store_device(&shared.store, &asked, grant);
                        match &stored {
                            DevicesInbound::Enrolled { device, .. } => {
                                shared.audit.record(AuditEvent::EnrollConfirmed {
                                    device: DeviceRef {
                                        id: device.clone(),
                                        name: asked.name.clone(),
                                    },
                                    recipe: asked.mode.as_str().to_owned(),
                                    grant: grant_word(grant).to_owned(),
                                    host: asked.host.clone(),
                                    peer: asked.peer.clone(),
                                })
                            }
                            _ => enroll_refused(&shared.audit, &asked, refusal_word(NOT_STORED)),
                        }
                        stored
                    }),
                });
                let outcome = handle.run(|state, out| ask_enroll(state, ask, Instant::now(), reply, out));
                if let Err(why) = outcome {
                    enroll_refused(&self.shared.audit, &refused, refusal_word(why));
                    self.answer(conn, enroll_refused_line(why));
                }
            }
            DevicesInbound::Enrolled { device, name, token } => {
                if !self.conns.borrow().contains_key(&conn) {
                    self.shared.audit.record(AuditEvent::EnrollRefused {
                        device: DeviceRef {
                            id: device.clone(),
                            name,
                        },
                        recipe: String::new(),
                        host: String::new(),
                        peer: String::new(),
                        why: "phone_left".into(),
                    });
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
                        if recorded {
                            shared.audit.record(AuditEvent::ListenChanged {
                                listener: Listener::Address {
                                    addr: asked.addr.clone(),
                                    iface: asked.iface.clone(),
                                    kind: asked.kind.as_str().to_owned(),
                                },
                                confirmed: true,
                            });
                        }
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

    /// Records the code on show as lapsed when it would have run out unused by `now`, as the upkeep does.
    #[cfg(test)]
    pub(super) fn note_lapsed_at(&self, now: Instant) {
        note_lapsed_now(&self.shared, now);
    }

    /// Whether a device worker still owes a result to the tick.
    #[cfg(test)]
    pub(super) fn devices_busy(&self) -> bool {
        self.shared.devices.workers.load(Ordering::SeqCst) > 0
    }

    /// Reads the device list with `read` in place of [`device_rows`].
    #[cfg(test)]
    pub(super) fn set_list_reader(&self, read: impl Fn(&Path) -> DevicesInbound + Send + Sync + 'static) {
        *self
            .shared
            .devices
            .list_reader
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Arc::new(read);
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
