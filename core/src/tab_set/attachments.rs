//! A tab's draft images: what the composer attached, imported in bounded chunks from the page, prepared off this
//! thread, and sent with the next message as a turn of its own.
//!
//! **Bytes never touch the window's thread as files.** Staging the page's chunks, decoding and normalizing, and
//! reading the prepared image back are file and process work, so they run on the window's attachment desk
//! ([`AttachmentDesk`]): a thread the host starts (`eitri_core::attachments::start_desk` on Linux) that owns the
//! private staging directory and the decoder worker. This side only sends it requests and reads what it reports,
//! on the tick. A prepared image comes back once, as its bytes in memory ([`ReadyImage`]), so a send reads no
//! file; the staged copy is removed once the send that carried it was answered, or when the image leaves the
//! draft, the queue or the tab.
//!
//! **The page names nothing.** Every page message is checked here against the tab it came from: an image is known
//! by the token the desk gave it, found only in that tab's own draft, and a new import must name the draft's
//! current generation, which moves on whenever a send or a queued message takes the draft's images. The page is
//! sent metadata and a thumbnail, never the bytes, never a path.
//!
//! **A message with images is a turn of its own.** A message put into a running turn carries text alone, so one
//! with images always waits for the turn's end (`steer_held` stops at it, keeping the queue's order), goes as its
//! own turn (`next_turn`), and one the agent refuses as busy is queued whole (`hold_whole`), never steered.

use super::*;
use base64::Engine as _;

/// The bytes one prepared image may have: the sidecar's per-image limit (`turn_content_limits.max_image_bytes`).
pub const IMAGE_MAX_BYTES: u64 = agent::turn_content::BUNDLED_MAX_IMAGE_BYTES;
/// The largest chunk the page may send, decoded.
pub const CHUNK_MAX_BYTES: usize = 262_144;
/// Images one tab's draft may hold at once, ready, importing or failed.
pub const DRAFT_MAX_IMAGES: usize = agent::turn_content::BUNDLED_MAX_IMAGES_PER_TURN;
/// A name the page gives an image is display text, cut to this many characters.
pub const NAME_MAX_CHARS: usize = 255;
/// Once a tab has sent more image bytes than this, the person is told once that the conversation is heavy.
pub const SESSION_WARN_BYTES: u64 = 16_777_216;
/// Why an image a waiting message took can be neither removed nor sent again.
pub const PARKED: &str =
    "this image goes with the message waiting to be sent; it is back in the draft if that message does not go";
/// Why an import is over when the page that was sending it is loaded again.
pub const ABANDONED: &str = "the page was reloaded before this image was sent whole: paste it again";

/// What the window asks of its attachment desk. Answers come back as [`DeskEvent`]s carrying `reply`.
pub enum DeskRequest {
    #[cfg(test)]
    ReservedBytes(mpsc::Sender<u64>),
    /// Reserve `len` bytes against the window's quota for a new import into `tab`'s draft of `generation`.
    Begin {
        reply: u64,
        tab: u64,
        generation: u64,
        len: u64,
    },
    /// The next bytes of an import, at `offset`.
    Chunk {
        reply: u64,
        token: String,
        offset: u64,
        data: Vec<u8>,
    },
    /// The import is complete: check its digest and prepare it.
    End { reply: u64, token: String, sha256: String },
    /// Forget these images and delete their files. Unknown tokens are ignored.
    Remove { tokens: Vec<String> },
    /// A file the person dropped on `tab`'s page: read, staged and prepared as an import of its own, for the draft of
    /// `generation`. The path comes from the window's drop target, never from a page.
    ImportFile {
        reply: u64,
        tab: u64,
        generation: u64,
        path: std::path::PathBuf,
    },
}

// Chunks hold image bytes: no derived Debug.
impl std::fmt::Debug for DeskRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(test)]
            DeskRequest::ReservedBytes(_) => write!(f, "ReservedBytes"),
            DeskRequest::Begin { reply, len, .. } => write!(f, "Begin({reply}, {len} bytes)"),
            DeskRequest::Chunk {
                reply, offset, data, ..
            } => {
                write!(f, "Chunk({reply}, at {offset}, {} bytes)", data.len())
            }
            DeskRequest::End { reply, .. } => write!(f, "End({reply})"),
            DeskRequest::Remove { tokens } => write!(f, "Remove({})", tokens.len()),
            DeskRequest::ImportFile { reply, .. } => write!(f, "ImportFile({reply})"),
        }
    }
}

/// What the desk reports.
pub enum DeskEvent {
    /// A `Begin`'s answer: the new image's token, or why not.
    Begun { reply: u64, result: Result<String, String> },
    /// A `Chunk`'s or an `End`'s answer. A refusal ended that import: the desk dropped it.
    Answered { reply: u64, result: Result<(), String> },
    /// An `ImportFile`'s answer: the new image's token and its length, now being prepared (a `Prepared` follows), or
    /// why the file could not be taken (nothing of it was kept).
    Imported {
        reply: u64,
        result: Result<(String, u64), String>,
    },
    /// An image whose `End` was accepted is ready, or was refused while it was prepared (the desk dropped it).
    Prepared {
        token: String,
        result: Result<PreparedImage, String>,
    },
}

/// A normalized image as the desk read it back: what it is, its bytes, and a PNG thumbnail for the page.
pub struct PreparedImage {
    pub meta: agent::AttachmentMeta,
    pub data: Arc<[u8]>,
    pub thumbnail_png: Vec<u8>,
}

impl std::fmt::Debug for PreparedImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedImage")
            .field("meta", &self.meta)
            .finish_non_exhaustive()
    }
}

/// The window's end of its attachment desk.
pub struct AttachmentDesk {
    requests: mpsc::Sender<DeskRequest>,
    events: mpsc::Receiver<DeskEvent>,
}

/// The desk's end: where its requests arrive and its events go.
pub struct DeskEnd {
    pub requests: mpsc::Receiver<DeskRequest>,
    pub events: mpsc::Sender<DeskEvent>,
}

impl AttachmentDesk {
    /// A desk and the end its thread (or a test) serves.
    pub fn channels() -> (AttachmentDesk, DeskEnd) {
        let (requests, requests_rx) = mpsc::channel();
        let (events_tx, events) = mpsc::channel();
        (
            AttachmentDesk { requests, events },
            DeskEnd {
                requests: requests_rx,
                events: events_tx,
            },
        )
    }

    #[cfg(all(test, target_os = "linux"))]
    pub(super) fn reserved_bytes(&self) -> u64 {
        let (tx, rx) = mpsc::channel();
        self.ask(DeskRequest::ReservedBytes(tx)).unwrap();
        rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap()
    }

    fn ask(&self, request: DeskRequest) -> Result<(), String> {
        self.requests
            .send(request)
            .map_err(|_| "this window's image import has stopped".to_string())
    }
}

/// An image ready to send: its bytes in memory, what it is, and what the page shows for it.
#[derive(Clone, PartialEq, Eq)]
pub struct ReadyImage {
    pub token: String,
    pub name: String,
    pub image: agent::TurnImage,
    /// `data:image/png;base64,...`, the only form of the image the page is given.
    pub thumbnail: String,
}

impl std::fmt::Debug for ReadyImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadyImage")
            .field("token", &self.token)
            .field("meta", &self.image.meta)
            .finish_non_exhaustive()
    }
}

/// A held message given back to the composer, with its images' tokens in order: the images are in the draft
/// again under those tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedDraft {
    pub text: String,
    pub attachments: Vec<String>,
}

// A message taken back without images reads as its text, as the queue's texts did before images.
impl PartialEq<&str> for QueuedDraft {
    fn eq(&self, other: &&str) -> bool {
        self.attachments.is_empty() && self.text == *other
    }
}

impl PartialEq<String> for QueuedDraft {
    fn eq(&self, other: &String) -> bool {
        self.attachments.is_empty() && self.text == *other
    }
}

/// A file the person dropped on a chat page, as the window's drop target read the drop: its path, `None` for a file
/// that is not on this computer (a remote location), and its name for the chip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedFile {
    pub path: Option<std::path::PathBuf>,
    pub name: String,
}

/// An answer the window owes a page for an attach request, once the desk has given it: `owed` is the panel's own
/// key for the page and request; `Ok(Some(token))` answers a begin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachAnswer {
    pub owed: String,
    pub result: Result<Option<String>, String>,
}

enum ItemState {
    ReadingFile,
    Importing {
        len: u64,
        received: u64,
        /// A chunk or the end is with the desk: the page sends one at a time.
        waiting: bool,
    },
    Ready(ReadyImage),
    Failed(String),
}

struct DraftItem {
    token: String,
    name: String,
    state: ItemState,
}

/// One tab's draft images and what its sends have carried, for its life in this window.
#[derive(Default)]
pub struct TabAttachments {
    /// Moves on whenever a send or a queued message takes the draft's images: an import begun for an older
    /// draft is refused. Resetting the tab also ends unfinished imports.
    generation: u64,
    items: Vec<DraftItem>,
    /// Image bytes this tab's turns have carried, each image counted once ([`TabAttachments::counted`]).
    sent_bytes: u64,
    /// The images already counted in `sent_bytes`: a message handed back and sent again carries the same images.
    counted: BTreeSet<String>,
    warned: bool,
    /// Said once with the next draft envelope (images a move left behind).
    note: Option<String>,
}

impl TabAttachments {
    pub(super) fn advance_generation(&mut self) -> Vec<String> {
        self.generation += 1;
        let removed = self
            .items
            .iter()
            .filter(|item| matches!(item.state, ItemState::ReadingFile | ItemState::Importing { .. }))
            .map(|item| item.token.clone())
            .collect();
        self.items
            .retain(|item| !matches!(item.state, ItemState::ReadingFile | ItemState::Importing { .. }));
        removed
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// How many images the draft holds.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    fn find(&self, token: &str) -> Option<usize> {
        self.items.iter().position(|item| item.token == token)
    }

    fn tokens(&self) -> Vec<String> {
        self.items.iter().map(|item| item.token.clone()).collect()
    }

    pub(super) fn clear_note(&mut self) {
        self.note = None;
    }

    /// A move left the draft's images behind: they are not carried, and the page that shows the tab next says so.
    pub(super) fn note_left_behind(&mut self, count: usize) {
        if count > 0 {
            self.note = Some(format!(
                "{count} image{} in the draft stayed in the other window: paste {} again",
                if count == 1 { "" } else { "s" },
                if count == 1 { "it" } else { "them" }
            ));
        }
    }
}

/// One image as the page draws it: metadata and a thumbnail, never the bytes.
fn item_json(tab: TabId, item: &DraftItem) -> serde_json::Value {
    let mut value = serde_json::json!({
        "tab": tab.0,
        "attachmentId": item.token,
        "name": item.name,
    });
    match &item.state {
        ItemState::ReadingFile | ItemState::Importing { .. } => value["state"] = "importing".into(),
        ItemState::Ready(ready) => {
            value["state"] = "ready".into();
            value["mediaType"] = ready.image.meta.media_type.clone().into();
            value["width"] = ready.image.meta.width.into();
            value["height"] = ready.image.meta.height.into();
            value["bytes"] = ready.image.meta.bytes.into();
            value["thumbnail"] = ready.thumbnail.clone().into();
        }
        ItemState::Failed(reason) => {
            value["state"] = "failed".into();
            value["reason"] = reason.clone().into();
        }
    }
    value
}

/// `{"kind":"attachment_state", ...}`: one image of a tab's draft changed.
fn state_envelope(tab: TabId, item: &DraftItem) -> String {
    let mut value = item_json(tab, item);
    value["kind"] = "attachment_state".into();
    value.to_string()
}

/// `{"kind":"draft_attachments","tab":..,"generation":..,"imageInput":..,"why"?,"note"?,"attachments":[..]}`: the
/// tab's whole draft of images, sent with its draft text, so a reload or a switch shows the same chips.
pub fn serialize_draft_attachments_for_js(
    tab: TabId,
    attachments: &TabAttachments,
    image_input: &Result<(), String>,
) -> String {
    let mut value = serde_json::json!({
        "kind": "draft_attachments",
        "tab": tab.0,
        "generation": attachments.generation,
        "imageInput": image_input.is_ok(),
        "attachments": attachments.items.iter().map(|item| item_json(tab, item)).collect::<Vec<_>>(),
    });
    if let Err(why) = image_input {
        value["why"] = why.clone().into();
    }
    if let Some(note) = &attachments.note {
        value["note"] = note.clone().into();
    }
    value.to_string()
}

/// What a page gave as an image's name, as display text: control characters dropped, at most
/// [`NAME_MAX_CHARS`] characters. Never read as a path.
fn display_name(name: &str) -> String {
    let name: String = name.chars().filter(|c| !c.is_control()).take(NAME_MAX_CHARS).collect();
    if name.trim().is_empty() {
        "image".to_string()
    } else {
        name
    }
}

/// A pending answer: which page request it is, about which tab, and what was asked.
pub(super) struct PendingAttach {
    owed: String,
    tab: TabId,
    what: Asked,
}

enum Asked {
    Begin {
        generation: u64,
        name: String,
        len: u64,
    },
    Chunk {
        token: String,
        len: u64,
    },
    End {
        token: String,
    },
    /// A dropped file being read for this draft, under its temporary chip's token.
    File {
        generation: u64,
        token: String,
    },
}

impl Asked {
    /// Whether this will add an image to the draft: what the draft's limit counts beside the images it holds.
    fn adds_an_image(&self) -> bool {
        matches!(self, Asked::Begin { .. })
    }
}

/// The window's side of image attachments, held by the tab set.
#[derive(Default)]
pub(super) struct Desk {
    desk: Option<AttachmentDesk>,
    /// Why this window takes no images, when it has no desk.
    why_not: Option<String>,
    pending: BTreeMap<u64, PendingAttach>,
    next_reply: u64,
    /// `attachment_state` envelopes for the pages, by tab.
    payloads: Vec<(TabId, String)>,
    /// One-line warnings for the pages, by tab.
    warnings: Vec<(TabId, String)>,
}

impl Desk {
    pub(super) fn remove(&self, tokens: Vec<String>) {
        if tokens.is_empty() {
            return;
        }
        if let Some(desk) = &self.desk {
            let _ = desk.ask(DeskRequest::Remove { tokens });
        }
    }
}

impl TabSet {
    #[cfg(all(test, target_os = "linux"))]
    pub(super) fn reserved_image_bytes(&self) -> u64 {
        self.attach.desk.as_ref().unwrap().reserved_bytes()
    }

    /// The window's attachment desk: from now on its tabs import images through it.
    pub fn install_attachments(&mut self, desk: AttachmentDesk) {
        self.attach.desk = Some(desk);
        self.attach.why_not = None;
    }

    /// This window takes no images, for `why` (no desk could start).
    pub fn refuse_attachments(&mut self, why: String) {
        self.attach.why_not = Some(why);
    }

    /// Whether `id`'s composer may take images now: `Err` says why not, in the composer's words. A remote tab
    /// takes none. A live session
    /// takes them when its provider does (`AgentBackend::why_no_images`); a Claude tab that has not started may
    /// import them for its first message, which its session refuses with the same words if it cannot carry them.
    pub fn image_input(&self, id: TabId) -> Result<(), String> {
        if self.attach.desk.is_none() {
            return Err(self
                .attach
                .why_not
                .clone()
                .unwrap_or_else(|| "this window cannot take images".to_string()));
        }
        let tab = self.get(id).ok_or_else(|| format!("no tab {}", id.0))?;
        // A remote tab's turns leave this thread as text alone (`TabSet::spawn_desk_send`): its composer takes no
        // images rather than sending a message without them.
        if self.sends_off_thread(id) {
            return Err(super::desk_sends::REMOTE_TAB_TAKES_NO_IMAGES.to_string());
        }
        match &tab.backend {
            TabBackend::Live(backend) => backend.why_no_images().map_or(Ok(()), Err),
            _ if tab.agent.is_external() => Err(format!(
                "this tab cannot take images: {} takes text only here",
                tab.agent.label()
            )),
            _ if self.kind != BackendKind::Sidecar => {
                Err("this tab cannot take images: its backend sends text only".to_string())
            }
            TabBackend::Failed { .. } => Err("this tab's session has ended; press r to start over".to_string()),
            _ => Ok(()),
        }
    }

    fn attach_request(
        &mut self,
        owed: &str,
        tab: TabId,
        what: Asked,
        request: impl FnOnce(u64) -> DeskRequest,
    ) -> Result<(), String> {
        let reply = self.attach.next_reply;
        self.attach.next_reply += 1;
        let desk = self.attach.desk.as_ref().ok_or("this window cannot take images")?;
        desk.ask(request(reply))?;
        self.attach.pending.insert(
            reply,
            PendingAttach {
                owed: owed.to_string(),
                tab,
                what,
            },
        );
        Ok(())
    }

    /// `attach_begin`: a new image of `byte_length` bytes for `id`'s draft of `generation`, answered under `owed`
    /// once the desk has reserved its room. `Err` refuses it now.
    pub fn attach_begin(
        &mut self,
        id: TabId,
        generation: u64,
        byte_length: u64,
        name: &str,
        owed: &str,
    ) -> Result<(), String> {
        if self.moving.contains(&id.0) {
            return Err(MOVING.to_string());
        }
        self.image_input(id)?;
        let tab = self.get(id).ok_or_else(|| format!("no tab {}", id.0))?;
        if generation != tab.attachments.generation {
            return Err("the draft has changed since this image was pasted: paste it again".to_string());
        }
        let begun = self
            .attach
            .pending
            .values()
            .filter(|p| p.tab == id && p.what.adds_an_image())
            .count();
        if tab.attachments.items.len() + begun >= DRAFT_MAX_IMAGES {
            return Err(format!("at most {DRAFT_MAX_IMAGES} images per message"));
        }
        let name = display_name(name);
        let what = Asked::Begin {
            generation,
            name,
            len: byte_length,
        };
        self.attach_request(owed, id, what, |reply| DeskRequest::Begin {
            reply,
            tab: id.0,
            generation,
            len: byte_length,
        })
    }

    /// Files the person dropped on the page showing `id`: each is read, staged and prepared on the desk, off this
    /// thread. An importing chip holds Send while the desk reads it (`DeskEvent::Imported`). `Err` refuses the whole
    /// drop now, before any file is opened. A file that is not on this computer (a link dragged from a browser is one)
    /// is left out: it is no image to attach, and a failed chip for it would hold Send.
    pub fn attach_files(&mut self, id: TabId, files: Vec<DroppedFile>) -> Result<(), String> {
        let files: Vec<(std::path::PathBuf, String)> = files
            .into_iter()
            .filter_map(|file| Some((file.path?, file.name)))
            .collect();
        if files.is_empty() {
            return Err("only files on this computer can be attached".to_string());
        }
        if self.moving.contains(&id.0) {
            return Err(MOVING.to_string());
        }
        self.image_input(id)?;
        let tab = self.get(id).ok_or_else(|| format!("no tab {}", id.0))?;
        let generation = tab.attachments.generation;
        let waiting = self
            .attach
            .pending
            .values()
            .filter(|p| p.tab == id && p.what.adds_an_image())
            .count();
        if tab.attachments.items.len() + waiting + files.len() > DRAFT_MAX_IMAGES {
            return Err(format!("at most {DRAFT_MAX_IMAGES} images per message"));
        }
        for (path, name) in files {
            let name = display_name(&name);
            let token = format!("file-{}", self.attach.next_reply);
            self.attach_request(
                "",
                id,
                Asked::File {
                    generation,
                    token: token.clone(),
                },
                |reply| DeskRequest::ImportFile {
                    reply,
                    tab: id.0,
                    generation,
                    path,
                },
            )?;
            let tab = self.get_mut(id).expect("checked tab");
            tab.attachments.items.push(DraftItem {
                token,
                name,
                state: ItemState::ReadingFile,
            });
            let payload = state_envelope(id, tab.attachments.items.last().unwrap());
            self.attach.payloads.push((id, payload));
        }
        Ok(())
    }

    /// Replace the file's temporary chip in place, preserving drop order even if reads finish out of order.
    fn imported(&mut self, id: TabId, generation: u64, chip: String, result: Result<(String, u64), String>) {
        let current = !self.moving.contains(&id.0)
            && self
                .get(id)
                .is_some_and(|tab| tab.attachments.generation == generation && tab.attachments.find(&chip).is_some());
        if !current {
            if let Ok((token, _)) = result {
                self.attach.remove(vec![token]);
            }
            let moving = self.moving.contains(&id.0);
            if let Some(tab) = self.get_mut(id) {
                let changed = tab.attachments.generation != generation;
                let present = tab.attachments.find(&chip);
                if let Some(at) = present {
                    tab.attachments.items.remove(at);
                    self.draft_changed(id);
                }
                // A missing chip in the same draft was removed by the person; its late read is silent.
                if changed || present.is_some() {
                    let why = if moving {
                        "the tab was moving before this dropped image was read; it was discarded: drop it again"
                    } else {
                        "the draft changed before this dropped image was read; it was discarded: drop it again"
                    };
                    self.attach.warnings.push((id, why.into()));
                }
            }
            return;
        }
        let tab = self.get_mut(id).expect("checked tab");
        let at = tab.attachments.find(&chip).expect("checked chip");
        let item = &mut tab.attachments.items[at];
        match result {
            Ok((token, len)) => {
                item.token = token;
                item.state = ItemState::Importing {
                    len,
                    received: len,
                    waiting: true,
                };
            }
            Err(why) => item.state = ItemState::Failed(why),
        }
        self.draft_changed(id);
    }

    pub(super) fn release_abandoned_imports(&mut self, id: TabId, tokens: Vec<String>) {
        let staged: Vec<_> = tokens.into_iter().filter(|token| !token.starts_with("file-")).collect();
        if !staged.is_empty() {
            self.attach.warnings.push((
                id,
                "the draft changed before an image finished importing; it was discarded: paste or drop it again".into(),
            ));
            self.attach.remove(staged);
        }
    }

    /// The importing image `token` of `id`'s draft, or why not.
    fn importing(&mut self, id: TabId, token: &str) -> Result<&mut DraftItem, String> {
        let tab = self.get_mut(id).ok_or_else(|| format!("no tab {}", id.0))?;
        let at = tab
            .attachments
            .find(token)
            .ok_or("this image is not in this tab's draft")?;
        let item = &mut tab.attachments.items[at];
        match item.state {
            ItemState::Importing { .. } => Ok(item),
            _ => Err("this image is not being imported".to_string()),
        }
    }

    /// An import that cannot go on: the image stays in the draft as failed, with `reason`, and its staged bytes go.
    fn fail_import(&mut self, id: TabId, token: &str, reason: &str) {
        let Some(tab) = self.get_mut(id) else { return };
        let Some(at) = tab.attachments.find(token) else { return };
        let item = &mut tab.attachments.items[at];
        item.state = ItemState::Failed(reason.to_string());
        let payload = state_envelope(id, item);
        self.attach.payloads.push((id, payload));
        self.attach.remove(vec![token.to_string()]);
    }

    /// `attach_chunk`: the next bytes of an import, base64, in order and one at a time. A chunk out of order, over
    /// [`CHUNK_MAX_BYTES`] or past the declared length is refused and the import ends as failed.
    pub fn attach_chunk(
        &mut self,
        id: TabId,
        token: &str,
        offset: u64,
        data_b64: &str,
        owed: &str,
    ) -> Result<(), String> {
        let item = self.importing(id, token)?;
        let ItemState::Importing { len, received, waiting } = &mut item.state else {
            unreachable!("checked importing");
        };
        if *waiting {
            let why = "image chunks must be sent one at a time".to_string();
            self.fail_import(id, token, &why);
            return Err(why);
        }
        // Checked on the encoded length first, so an oversized chunk is never decoded.
        let refusal = if data_b64.len() > CHUNK_MAX_BYTES.div_ceil(3) * 4 {
            Some("image chunk exceeds the 256 KB limit".to_string())
        } else if offset != *received {
            Some("image chunks arrived out of order".to_string())
        } else {
            None
        };
        let data = match refusal {
            Some(why) => Err(why),
            None => base64::engine::general_purpose::STANDARD
                .decode(data_b64)
                .map_err(|_| "image chunk is not base64".to_string()),
        };
        let data = data.and_then(|data| {
            if data.len() > CHUNK_MAX_BYTES {
                Err("image chunk exceeds the 256 KB limit".to_string())
            } else if data.is_empty() || data.len() as u64 > *len - *received {
                Err("image data exceeds its declared length".to_string())
            } else {
                Ok(data)
            }
        });
        let data = match data {
            Ok(data) => data,
            Err(why) => {
                self.fail_import(id, token, &why);
                return Err(why);
            }
        };
        let chunk = data.len() as u64;
        *waiting = true;
        let token = token.to_string();
        let sent = self.attach_request(
            owed,
            id,
            Asked::Chunk {
                token: token.clone(),
                len: chunk,
            },
            |reply| DeskRequest::Chunk {
                reply,
                token: token.clone(),
                offset,
                data,
            },
        );
        if let Err(why) = &sent {
            self.fail_import(id, &token, why);
        }
        sent
    }

    /// `attach_end`: the import is complete, with the SHA-256 the page computed over its bytes.
    pub fn attach_end(&mut self, id: TabId, token: &str, sha256: &str, owed: &str) -> Result<(), String> {
        let item = self.importing(id, token)?;
        let ItemState::Importing { len, received, waiting } = &mut item.state else {
            unreachable!("checked importing");
        };
        let refusal = if *waiting {
            Some("image chunks must be sent one at a time")
        } else if *received != *len {
            Some("image upload is incomplete")
        } else if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            Some("image upload failed its integrity check")
        } else {
            None
        };
        if let Some(why) = refusal {
            self.fail_import(id, token, why);
            return Err(why.to_string());
        }
        *waiting = true;
        let token = token.to_string();
        let sent = self.attach_request(owed, id, Asked::End { token: token.clone() }, |reply| {
            DeskRequest::End {
                reply,
                token: token.clone(),
                sha256: sha256.to_string(),
            }
        });
        if let Err(why) = &sent {
            self.fail_import(id, &token, why);
        }
        sent
    }

    /// The draft images of `id` a message already took that has not gone yet: a first message waiting for the trust
    /// question or its session's start, or a message held for the presence. They stay in the draft, so whichever way
    /// that message ends without going (refused, put off, cancelled, started over) they are the draft's again with
    /// nothing to give back; while it waits they can neither be taken out nor go with another message.
    pub(super) fn parked_images(&self, id: TabId) -> BTreeSet<String> {
        let Some(tab) = self.get(id) else {
            return BTreeSet::new();
        };
        let mut turns: Vec<&FirstTurn> = Vec::new();
        match &tab.backend {
            TabBackend::AwaitingTrust(AwaitingTrust {
                start: DeferredStart::FirstSend { first_turn, .. },
                ..
            }) => turns.push(first_turn),
            TabBackend::Starting(pending) => turns.extend(pending.first_turn.as_ref()),
            _ => {}
        }
        if let Some(held) = self.pending_sends.get(&id.0) {
            match &held.what {
                moving::TurnSend::First { turn, .. } | moving::TurnSend::Prompt { turn, .. } => turns.push(turn),
                moving::TurnSend::Queue | moving::TurnSend::Remote { .. } => {}
            }
        }
        turns
            .into_iter()
            .flat_map(|turn| turn.attachments.iter().map(|image| image.token.clone()))
            .collect()
    }

    /// `attach_remove` and `attach_cancel`: the image leaves the draft, whatever its state, and its files go.
    pub fn attach_remove(&mut self, id: TabId, token: &str) -> Result<(), String> {
        if self.parked_images(id).contains(token) {
            return Err(PARKED.to_string());
        }
        let tab = self.get_mut(id).ok_or_else(|| format!("no tab {}", id.0))?;
        let at = tab
            .attachments
            .find(token)
            .ok_or("this image is not in this tab's draft")?;
        tab.attachments.items.remove(at);
        // A file read owes no page answer; forget its request so a later draft change cannot relabel this removal.
        self.attach.pending.retain(|_, pending| {
            !(pending.tab == id && matches!(&pending.what, Asked::File { token: chip, .. } if chip == token))
        });
        self.attach.remove(vec![token.to_string()]);
        Ok(())
    }

    /// The draft images `tokens` name, in that order, for a message about to go: each must be in `id`'s draft and
    /// ready. They stay in the draft until the message has gone (or is queued), so a refusal leaves them there.
    pub fn draft_images(&self, id: TabId, tokens: &[String]) -> Result<Vec<ReadyImage>, String> {
        if tokens.is_empty() {
            return Ok(Vec::new());
        }
        let tab = self.get(id).ok_or_else(|| format!("no tab {}", id.0))?;
        let parked = self.parked_images(id);
        let mut images = Vec::with_capacity(tokens.len());
        for (index, token) in tokens.iter().enumerate() {
            if tokens[..index].contains(token) {
                return Err("the same image is attached twice".to_string());
            }
            if parked.contains(token) {
                return Err(PARKED.to_string());
            }
            let at = tab
                .attachments
                .find(token)
                .ok_or("an attached image is no longer in the draft: remove it and paste it again")?;
            match &tab.attachments.items[at].state {
                ItemState::Ready(ready) => images.push(ready.clone()),
                ItemState::ReadingFile | ItemState::Importing { .. } => {
                    return Err("an image is still being prepared".to_string())
                }
                ItemState::Failed(_) => return Err("an image could not be used: remove it first".to_string()),
            }
        }
        Ok(images)
    }

    /// Reads what the desk reported: each answer owed to a page, and the draft changes the pages are told of
    /// ([`TabSet::take_attachment_payloads`]).
    pub fn poll_attachments(&mut self) -> Vec<AttachAnswer> {
        let mut events = Vec::new();
        if let Some(desk) = &self.attach.desk {
            while let Ok(event) = desk.events.try_recv() {
                events.push(event);
            }
        }
        let mut answers = Vec::new();
        for event in events {
            match event {
                DeskEvent::Begun { reply, result } => {
                    let Some(pending) = self.attach.pending.remove(&reply) else {
                        // The tab left meanwhile ([`TabSet::forget_images_of`]): the room the desk kept goes.
                        if let Ok(token) = result {
                            self.attach.remove(vec![token]);
                        }
                        continue;
                    };
                    let Asked::Begin { generation, name, len } = pending.what else {
                        continue;
                    };
                    let result = result.and_then(|token| self.begun(pending.tab, generation, name, len, token));
                    answers.push(AttachAnswer {
                        owed: pending.owed,
                        result: result.map(Some),
                    });
                }
                DeskEvent::Answered { reply, result } => {
                    let Some(pending) = self.attach.pending.remove(&reply) else {
                        continue;
                    };
                    let (token, chunk) = match &pending.what {
                        Asked::Chunk { token, len } => (token.clone(), Some(*len)),
                        Asked::End { token } => (token.clone(), None),
                        Asked::Begin { .. } | Asked::File { .. } => continue,
                    };
                    match &result {
                        Ok(()) => {
                            if let Ok(item) = self.importing(pending.tab, &token) {
                                if let ItemState::Importing { received, waiting, .. } = &mut item.state {
                                    *received += chunk.unwrap_or(0);
                                    // After the end, the image waits for its preparation, not for the page.
                                    *waiting = chunk.is_none();
                                }
                            }
                        }
                        Err(why) => self.fail_import(pending.tab, &token, why),
                    }
                    answers.push(AttachAnswer {
                        owed: pending.owed,
                        result: result.map(|()| None),
                    });
                }
                DeskEvent::Imported { reply, result } => {
                    let Some(pending) = self.attach.pending.remove(&reply) else {
                        // The tab left meanwhile ([`TabSet::forget_images_of`]): what the desk staged for it goes.
                        if let Ok((token, _)) = result {
                            self.attach.remove(vec![token]);
                        }
                        continue;
                    };
                    let Asked::File { generation, token } = pending.what else {
                        continue;
                    };
                    self.imported(pending.tab, generation, token, result);
                }
                DeskEvent::Prepared { token, result } => self.prepared(&token, result),
            }
        }
        answers
    }

    /// A begin the desk took: the image joins the draft as importing, unless the tab or its draft moved on.
    fn begun(&mut self, id: TabId, generation: u64, name: String, len: u64, token: String) -> Result<String, String> {
        let moving = self.moving.contains(&id.0);
        let Some(tab) = self
            .get_mut(id)
            .filter(|t| t.attachments.generation == generation && !moving)
        else {
            self.attach.remove(vec![token]);
            return Err("the draft has changed since this image was pasted: paste it again".to_string());
        };
        tab.attachments.items.push(DraftItem {
            token: token.clone(),
            name,
            state: ItemState::Importing {
                len,
                received: 0,
                waiting: false,
            },
        });
        let payload = state_envelope(id, tab.attachments.items.last().unwrap());
        self.attach.payloads.push((id, payload));
        Ok(token)
    }

    /// A prepared image, or why it could not be: the draft's chip becomes ready or failed. One whose chip was
    /// removed meanwhile is forgotten.
    fn prepared(&mut self, token: &str, result: Result<PreparedImage, String>) {
        let found = self.tabs.iter().position(|t| t.attachments.find(token).is_some());
        let Some(position) = found else {
            self.attach.remove(vec![token.to_string()]);
            return;
        };
        let tab = &mut self.tabs[position];
        let id = tab.id;
        let at = tab.attachments.find(token).unwrap();
        let item = &mut tab.attachments.items[at];
        if !matches!(item.state, ItemState::Importing { .. }) {
            return;
        }
        item.state = match result {
            Ok(prepared) => ItemState::Ready(ReadyImage {
                token: token.to_string(),
                name: item.name.clone(),
                thumbnail: format!(
                    "data:image/png;base64,{}",
                    base64::engine::general_purpose::STANDARD.encode(&prepared.thumbnail_png)
                ),
                image: agent::TurnImage {
                    meta: prepared.meta,
                    data: prepared.data,
                },
            }),
            Err(reason) => ItemState::Failed(reason),
        };
        let payload = state_envelope(id, item);
        self.attach.payloads.push((id, payload));
    }

    /// `view`'s page was loaded again (its `ready`): an import that page was still sending, chunk by chunk, has no one
    /// left to finish it, so it fails with [`ABANDONED`] and its room in the window's quota is given back. An image
    /// whose bytes are all with the desk (its end was sent) is being prepared and stays.
    pub fn abandon_imports_of(&mut self, view: ViewId) {
        let mut ended = Vec::new();
        for tab in self.tabs.iter_mut().filter(|t| t.view_id() == view) {
            for item in &mut tab.attachments.items {
                if let ItemState::Importing { len, received, waiting } = item.state {
                    if received < len || !waiting {
                        item.state = ItemState::Failed(ABANDONED.to_string());
                        ended.push((tab.id, item.token.clone(), state_envelope(tab.id, item)));
                    }
                }
            }
        }
        for (tab, token, payload) in ended {
            self.attach.payloads.push((tab, payload));
            self.attach.remove(vec![token]);
        }
    }

    /// Whether an answer of the desk is still owed: the tick polls for it while one is.
    pub fn attachments_pending(&self) -> bool {
        !self.attach.pending.is_empty()
            || self.tabs.iter().any(|t| {
                t.attachments.items.iter().any(|item| {
                    matches!(
                        item.state,
                        ItemState::ReadingFile | ItemState::Importing { waiting: true, .. }
                    )
                })
            })
    }

    /// The draft's images of `id` changed as a whole (a send took them, a take-back gave them back): its page is
    /// sent the whole draft, with its new generation.
    pub(super) fn draft_changed(&mut self, id: TabId) {
        if let Some(payload) = self.draft_attachments_payload(id) {
            self.attach.payloads.push((id, payload));
        }
    }

    /// The `attachment_state` envelopes owed to the page of each tab.
    pub fn take_attachment_payloads(&mut self) -> Vec<(TabId, String)> {
        std::mem::take(&mut self.attach.payloads)
    }

    /// One-line warnings owed to the page of each tab (a conversation carrying many image bytes), for the tabs a page
    /// shows now (`shown`): a warning about a tab no page shows waits until one does, and goes with its tab.
    pub fn take_image_warnings(&mut self, shown: impl Fn(TabId) -> bool) -> Vec<(TabId, String)> {
        let warnings = std::mem::take(&mut self.attach.warnings);
        let (now, later): (Vec<_>, Vec<_>) = warnings.into_iter().partition(|(tab, _)| shown(*tab));
        self.attach.warnings = later.into_iter().filter(|(tab, _)| self.get(*tab).is_some()).collect();
        now
    }

    /// `id`'s draft images as the page draws them, with whether its composer takes images.
    pub fn draft_attachments_payload(&mut self, id: TabId) -> Option<String> {
        let input = self.image_input(id);
        let tab = self.get_mut(id)?;
        let payload = serialize_draft_attachments_for_js(id, &tab.attachments, &input);
        tab.attachments.note = None;
        Some(payload)
    }

    /// A turn carrying `images` went out (its send was answered) in `id`: they leave the draft, which moves to its
    /// next generation, their staged files go, and the tab's count of image bytes grows, with one warning once it
    /// passes [`SESSION_WARN_BYTES`].
    pub(super) fn images_sent(&mut self, id: TabId, images: &[ReadyImage]) {
        if images.is_empty() {
            return;
        }
        self.images_left_draft(id, images);
        let tokens: Vec<String> = images.iter().map(|image| image.token.clone()).collect();
        self.attach.remove(tokens);
        let Some(tab) = self.get_mut(id) else { return };
        let mut bytes = 0;
        for image in images {
            if tab.attachments.counted.insert(image.token.clone()) {
                bytes += image.image.meta.bytes;
            }
        }
        tab.attachments.sent_bytes += bytes;
        if tab.attachments.sent_bytes > SESSION_WARN_BYTES && !tab.attachments.warned {
            tab.attachments.warned = true;
            let megabytes = tab.attachments.sent_bytes.div_ceil(1_048_576);
            self.attach.warnings.push((
                id,
                format!(
                    "this conversation now carries {megabytes} MB of images; /compact or a new tab if Claude starts failing"
                ),
            ));
        }
    }

    /// `images` were taken from `id`'s draft by a send or a queued message: they leave it, and it moves on.
    /// A completion for images already taken leaves the next draft's imports alone.
    pub(super) fn images_left_draft(&mut self, id: TabId, images: &[ReadyImage]) {
        let Some(tab) = self.get_mut(id) else { return };
        let before = tab.attachments.items.len();
        tab.attachments
            .items
            .retain(|item| !images.iter().any(|image| image.token == item.token));
        if tab.attachments.items.len() != before {
            let removed = tab.attachments.advance_generation();
            self.release_abandoned_imports(id, removed);
            self.draft_changed(id);
        }
    }

    /// The newest queued message of `id` carries `images`, which leave the draft: it goes as a turn of its own.
    pub(super) fn carry_images_onto_last(&mut self, id: TabId, images: Vec<ReadyImage>) {
        if images.is_empty() {
            return;
        }
        self.images_left_draft(id, &images);
        if let Some(item) = self.get_mut(id).and_then(|t| t.session.queue.last_mut()) {
            item.attachments = images;
        }
    }

    /// `queue_message` for a message with images: it never goes into the running turn, and waits for its end.
    pub fn queue_message_with_images(
        &mut self,
        id: TabId,
        text: &str,
        wire: String,
        now_ms: u64,
        after_turn: bool,
        origin: PromptOrigin,
        images: Vec<ReadyImage>,
    ) -> Result<usize, String> {
        if !images.is_empty() && is_slash_command(text) {
            return Err(crate::agent_backend::NO_IMAGES_WITH_A_COMMAND.to_string());
        }
        let len = if after_turn {
            self.queue_after_turn(id, text, wire, now_ms, origin)?
        } else {
            self.queue_message(id, text, wire, now_ms, true, origin)?
        };
        self.carry_images_onto_last(id, images);
        Ok(len)
    }

    /// A message with images the agent refused as busy (an open turn this side had not seen): queued whole, to go
    /// as its own turn once that one ends. False when the tab cannot hold it (an external agent's).
    pub(super) fn hold_whole(&mut self, id: TabId, turn: &FirstTurn) -> bool {
        let Some(tab) = self.get_mut(id) else { return false };
        if tab.agent.is_external() {
            return false;
        }
        let mut item = Queued::held(&turn.typed, turn.wire.clone(), wall_clock_ms());
        item.typed_by_person = turn.typed_by_person;
        tab.session.queue.push(item);
        tab.session.queue_error = None;
        self.carry_images_onto_last(id, turn.attachments.clone());
        true
    }

    /// Returns still-reserved queued images to `id`'s draft, ready, under their tokens. Overflow and images whose
    /// submitted send already released their reservation are dropped with a notice.
    pub(super) fn images_back_to_draft(&mut self, id: TabId, images: Vec<ReadyImage>) {
        if images.is_empty() {
            return;
        }
        let pending = self
            .attach
            .pending
            .values()
            .filter(|p| p.tab == id && p.what.adds_an_image())
            .count();
        let Some(tab) = self.get_mut(id) else {
            self.attach
                .remove(images.into_iter().map(|image| image.token).collect());
            return;
        };
        let mut released = Vec::new();
        let mut unreserved = 0;
        for image in images {
            // A submitted prompt can return to the queue after its staging reservation was released.
            // Its bytes cannot rejoin the draft without reserving room in the window again.
            if tab.attachments.counted.contains(&image.token) {
                unreserved += 1;
                continue;
            }
            if tab.attachments.find(&image.token).is_some() {
                continue;
            }
            if tab.attachments.items.len() + pending >= DRAFT_MAX_IMAGES {
                released.push(image.token);
                continue;
            }
            tab.attachments.items.push(DraftItem {
                token: image.token.clone(),
                name: image.name.clone(),
                state: ItemState::Ready(image),
            });
        }
        if unreserved > 0 {
            let (noun, pronoun) = if unreserved == 1 {
                ("image", "it")
            } else {
                ("images", "them")
            };
            self.attach.warnings.push((id, format!("{unreserved} sent {noun} could not return to the draft because their reserved room was released; attach {pronoun} again")));
        }
        if !released.is_empty() {
            let count = released.len();
            let noun = if count == 1 { "image" } else { "images" };
            self.attach.warnings.push((id, format!("{count} queued {noun} could not return to the draft: at most {DRAFT_MAX_IMAGES} images per message; reserved room was released")));
            self.attach.remove(released);
        }
        self.draft_changed(id);
    }

    /// Puts a ready image straight into `id`'s draft, for a test that needs one without a desk.
    #[cfg(test)]
    pub(super) fn put_ready_for_test(&mut self, id: TabId, image: ReadyImage) {
        let tab = self.get_mut(id).expect("the tab");
        tab.attachments.items.push(DraftItem {
            token: image.token.clone(),
            name: image.name.clone(),
            state: ItemState::Ready(image),
        });
    }

    /// Every staged image `tab` holds, in its draft and its queue: what goes when it leaves this window.
    pub(super) fn forget_images_of(&mut self, tab: &Tab) {
        let mut tokens = tab.attachments.tokens();
        tokens.extend(
            tab.session
                .queue
                .iter()
                .flat_map(|item| item.attachments.iter().map(|image| image.token.clone())),
        );
        self.attach.remove(tokens);
        self.attach.pending.retain(|_, pending| pending.tab != tab.id);
    }
}
