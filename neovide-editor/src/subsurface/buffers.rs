//! Compositor-owned buffers stay unavailable until their asynchronous release reaches the pool.

use std::collections::VecDeque;
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicU64, Ordering};

use wayland_client::{protocol::wl_buffer::WlBuffer, Dispatch, Proxy, QueueHandle};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::{Flags, ZwpLinuxBufferParamsV1};

use super::geometry::{bucket, too_large, BUCKET};
use super::types::{BufferKey, DmabufFeedback, Globals, PaneId, PresentEvent};
use crate::dmabuf_target::{
    check_params, usable_modifiers, Chosen, DmabufPool, FrameSlot, DRM_FORMAT_ABGR8888, DRM_FORMAT_XBGR8888,
};

pub(crate) fn choose_format(feedback: &DmabufFeedback, egl: &[(u64, bool)], same_device: bool) -> Option<Chosen> {
    if !same_device {
        return None;
    }
    let usable = usable_modifiers(egl, |_| true);
    for wrap_fourcc in [DRM_FORMAT_XBGR8888, DRM_FORMAT_ABGR8888] {
        let mut modifiers = Vec::new();
        for &(format, modifier) in &feedback.formats {
            if format == wrap_fourcc && usable.contains(&modifier) && !modifiers.contains(&modifier) {
                modifiers.push(modifier);
            }
        }
        if !modifiers.is_empty() {
            return Some(Chosen {
                wrap_fourcc,
                opaque_region_needed: wrap_fourcc == DRM_FORMAT_ABGR8888,
                modifiers,
            });
        }
    }
    None
}

pub(crate) const MAX_BUFFERS: usize = 4;
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Acquire {
    Ready(FrameSlot),
    Pending,
    Full,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PoolChange {
    None,
    Freed,
    Created,
    Failed,
}

trait SlotSource {
    /// Always a new buffer: each entry names its own dmabuf, so an idle one of the same size is never handed out.
    fn allocate(&mut self, size: (i32, i32)) -> Result<FrameSlot, String>;
    fn bind(&mut self, slot: FrameSlot) -> Result<(), String>;
    fn set_busy(&self, slot: FrameSlot, busy: bool);
    fn submit(&mut self, slot: FrameSlot);
    fn discard(&mut self, slot: FrameSlot);
}
impl SlotSource for DmabufPool {
    fn allocate(&mut self, size: (i32, i32)) -> Result<FrameSlot, String> {
        self.allocate_slot(size.0, size.1)
    }
    fn bind(&mut self, slot: FrameSlot) -> Result<(), String> {
        self.bind(slot)
    }
    fn set_busy(&self, slot: FrameSlot, busy: bool) {
        self.set_busy(slot, busy);
    }
    fn submit(&mut self, slot: FrameSlot) {
        self.submit(slot);
    }
    fn discard(&mut self, slot: FrameSlot) {
        self.discard(slot);
    }
}
trait Destroy {
    fn destroy(&self);
}
impl Destroy for WlBuffer {
    fn destroy(&self) {
        if self.is_alive() {
            self.destroy();
        }
    }
}
impl Destroy for ZwpLinuxBufferParamsV1 {
    fn destroy(&self) {
        if self.is_alive() {
            self.destroy();
        }
    }
}
trait CreateRequest<S> {
    type Buffer: Destroy;
    type Params: Destroy;
    fn create(&mut self, source: &S, slot: FrameSlot, key: BufferKey, chosen: &Chosen) -> Result<Self::Params, String>;
    /// The same import with the buffer named at once, so the frame that needs it can render into it. The compositor
    /// reports a failed import as the params' `failed` or as a protocol error, so it is asked only for a format and
    /// modifier it has already created a buffer for.
    fn create_immed(
        &mut self,
        source: &S,
        slot: FrameSlot,
        key: BufferKey,
        chosen: &Chosen,
    ) -> Result<(Self::Params, Self::Buffer), String>;
}
struct Entry<B: Destroy, P: Destroy> {
    slot: FrameSlot,
    key: BufferKey,
    buffer: Option<B>,
    params: Option<P>,
    busy: bool,
    retire: bool,
}
impl<B: Destroy, P: Destroy> Drop for Entry<B, P> {
    fn drop(&mut self) {
        // Params still here without a buffer were never answered. Destroying them now would make the client drop
        // the compositor's `created`, and the wl_buffer it names would then never be destroyed; the handle is let
        // go instead, so whoever receives that `created` can still destroy the buffer. Params beside a buffer
        // belong to an immediate create, which gets no `created`, and are destroyed with it.
        let params = self.params.take();
        if let Some(buffer) = self.buffer.take() {
            if let Some(params) = params {
                params.destroy();
            }
            buffer.destroy();
        }
    }
}
enum Answer<B> {
    Created(B),
    Failed,
    Released,
}
struct Pool<S, B: Destroy, P: Destroy> {
    source: S,
    chosen: Chosen,
    pane: PaneId,
    generation: u64,
    next_id: u64,
    viewporter: bool,
    entries: Vec<Entry<B, P>>,
    failures: VecDeque<(BufferKey, String)>,
    local_failed: Vec<BufferKey>,
    created_count: u64,
    last_content: Option<(i32, i32)>,
    /// The compositor has created a buffer of this pool's format and modifier, so later buffers may be created
    /// immediately.
    proven: bool,
}
impl<S: SlotSource, B: Destroy, P: Destroy> Pool<S, B, P> {
    fn new(source: S, chosen: Chosen, pane: PaneId, viewporter: bool) -> Self {
        Self {
            source,
            chosen,
            pane,
            generation: NEXT_GENERATION.fetch_add(1, Ordering::Relaxed),
            next_id: 0,
            viewporter,
            entries: Vec::new(),
            failures: VecDeque::new(),
            local_failed: Vec::new(),
            created_count: 0,
            last_content: None,
            proven: false,
        }
    }
    fn size(entry: &Entry<B, P>) -> (i32, i32) {
        (entry.slot.width, entry.slot.height)
    }
    fn fits(&self, entry: &Entry<B, P>, content: (i32, i32)) -> bool {
        if self.viewporter {
            entry.slot.width >= content.0 && entry.slot.height >= content.1
        } else {
            Self::size(entry) == content
        }
    }
    fn remove(&mut self, index: usize) {
        let entry = self.entries.remove(index);
        let slot = entry.slot;
        drop(entry);
        self.source.set_busy(slot, false);
        self.source.discard(slot);
    }
    fn wrong_size(&self, entry: &Entry<B, P>, content: (i32, i32)) -> bool {
        if self.viewporter {
            too_large(Self::size(entry), content)
        } else {
            Self::size(entry) != content
        }
    }
    fn serves(&self, entry: &Entry<B, P>, content: (i32, i32)) -> bool {
        !entry.busy && entry.buffer.is_some() && self.fits(entry, content)
    }
    /// Destroys free buffers of the wrong size. A buffer whose create is unanswered is never dropped, and
    /// with a viewporter the smallest oversized free buffer that still covers the content is kept until a
    /// buffer of the right size is created, so a shrinking drag crops it instead of missing a frame.
    fn trim(&mut self, content: (i32, i32)) {
        let spare = if self.viewporter
            && !self
                .entries
                .iter()
                .any(|e| self.serves(e, content) && !self.wrong_size(e, content))
        {
            self.entries
                .iter()
                .filter(|e| self.serves(e, content))
                .min_by_key(|e| Self::area(e))
                .map(|e| e.slot)
        } else {
            None
        };
        let mut i = 0;
        while i < self.entries.len() {
            let retire = self.wrong_size(&self.entries[i], content) && Some(self.entries[i].slot) != spare;
            let e = &mut self.entries[i];
            e.retire = retire;
            if retire && !e.busy && e.buffer.is_some() {
                self.remove(i);
            } else {
                i += 1;
            }
        }
    }
    fn area(entry: &Entry<B, P>) -> i64 {
        i64::from(entry.slot.width) * i64::from(entry.slot.height)
    }
    /// Makes room for one more buffer by destroying the smallest free created buffer that cannot serve
    /// `content`; unanswered creates and busy buffers are never evicted.
    fn make_room(&mut self, content: (i32, i32)) -> bool {
        if self.entries.len() < MAX_BUFFERS {
            return true;
        }
        let victim = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.busy && e.buffer.is_some() && !self.fits(e, content))
            .min_by_key(|(_, e)| Self::area(e))
            .map(|(i, _)| i);
        match victim {
            Some(i) => {
                self.remove(i);
                true
            }
            None => false,
        }
    }
    /// Starts one create; true when its request was sent and the buffer now waits for the answer.
    fn start<R: CreateRequest<S, Buffer = B, Params = P>>(&mut self, requests: &mut R, size: (i32, i32)) -> bool {
        if self.entries.len() >= MAX_BUFFERS || !self.failures.is_empty() {
            return false;
        }
        let key = BufferKey {
            pane: self.pane,
            generation: self.generation,
            id: self.next_id,
        };
        self.next_id = self.next_id.wrapping_add(1);
        let slot = match self.source.allocate(size) {
            Ok(slot) => slot,
            Err(err) => {
                self.failures.push_back((key, err));
                self.local_failed.push(key);
                return false;
            }
        };
        // Pending buffers must not be reused by the allocator before the compositor answers.
        self.source.set_busy(slot, true);
        match requests.create(&self.source, slot, key, &self.chosen) {
            Ok(params) => {
                self.entries.push(Entry {
                    slot,
                    key,
                    buffer: None,
                    params: Some(params),
                    busy: false,
                    retire: false,
                });
                true
            }
            Err(err) => {
                self.source.set_busy(slot, false);
                self.source.discard(slot);
                self.failures.push_back((key, err));
                self.local_failed.push(key);
                false
            }
        }
    }
    fn acquire<R: CreateRequest<S, Buffer = B, Params = P>>(
        &mut self,
        requests: &mut R,
        content: (i32, i32),
    ) -> Acquire {
        if !self.failures.is_empty() || !self.local_failed.is_empty() {
            return Acquire::Pending;
        }
        // An empty rectangle is shown by unmapping, never by a buffer; asking for one is not a failure.
        if content.0 < 1 || content.1 < 1 {
            return Acquire::Pending;
        }
        let last = self.last_content.replace(content).unwrap_or(content);
        self.trim(content);
        let selected = self
            .entries
            .iter()
            .filter(|e| self.serves(e, content))
            .min_by_key(|e| Self::area(e))
            .map(|e| e.slot);
        if let Some(slot) = selected {
            self.start_ahead(requests, content, last, slot);
            // Prefetch allocation binds its own target, so restore the selected frame's target last.
            if let Err(err) = self.source.bind(slot) {
                if let Some(key) = self.entries.iter().find(|e| e.slot == slot).map(|e| e.key) {
                    self.failures.push_back((key, err));
                }
                return Acquire::Pending;
            }
            return Acquire::Ready(slot);
        }
        // A create already sent for a size that fits answers soon, and a frame of a size that held waits for it. A
        // frame whose size just changed (a transaction) gets its own buffer at once in a proven pool if there is
        // room: the answer would come a frame after the parent has shown the new size.
        let fitting_pending = self.entries.iter().any(|e| e.buffer.is_none() && self.fits(e, content));
        if fitting_pending && (!self.proven || content == last) {
            return Acquire::Pending;
        }
        if !self.make_room(content) {
            return if !fitting_pending && self.entries.iter().all(|e| e.busy) {
                Acquire::Full
            } else {
                Acquire::Pending
            };
        }
        let size = if self.viewporter { bucket(content) } else { content };
        if !self.proven {
            self.start(requests, size);
            return Acquire::Pending;
        }
        // A size no buffer covers (a jump past the prefetch, a scale change) would otherwise show the old size
        // until the compositor answers; an immediate create lets this frame render at the new size.
        let Some(slot) = self.start_immediate(requests, size) else {
            return Acquire::Pending;
        };
        if let Err(err) = self.source.bind(slot) {
            if let Some(key) = self.entries.iter().find(|e| e.slot == slot).map(|e| e.key) {
                self.failures.push_back((key, err));
            }
            return Acquire::Pending;
        }
        Acquire::Ready(slot)
    }
    /// Allocates and creates one buffer the compositor names at once; its slot when the buffer exists.
    fn start_immediate<R: CreateRequest<S, Buffer = B, Params = P>>(
        &mut self,
        requests: &mut R,
        size: (i32, i32),
    ) -> Option<FrameSlot> {
        if self.entries.len() >= MAX_BUFFERS || !self.failures.is_empty() {
            return None;
        }
        let key = BufferKey {
            pane: self.pane,
            generation: self.generation,
            id: self.next_id,
        };
        self.next_id = self.next_id.wrapping_add(1);
        let slot = match self.source.allocate(size) {
            Ok(slot) => slot,
            Err(err) => {
                self.failures.push_back((key, err));
                self.local_failed.push(key);
                return None;
            }
        };
        match requests.create_immed(&self.source, slot, key, &self.chosen) {
            Ok((params, buffer)) => {
                self.entries.push(Entry {
                    slot,
                    key,
                    buffer: Some(buffer),
                    params: Some(params),
                    busy: false,
                    retire: false,
                });
                self.created_count += 1;
                Some(slot)
            }
            Err(err) => {
                self.source.discard(slot);
                self.failures.push_back((key, err));
                self.local_failed.push(key);
                None
            }
        }
    }
    /// Starts the buffers the next frames will want while this frame's buffer is chosen. The compositor keeps
    /// the buffer on screen until the next commit replaces it, so every size a frame may show needs two buffers:
    /// one on screen, one to render the next frame into. With a viewporter that size is the content, grown by a
    /// bucket on each side that is within a quarter bucket of its bucket's edge and not shrinking (each side on
    /// its own, so a buffer grown for one side is never too large on the other), and a shrinking side near
    /// its edge gets its buffers a bucket larger while its target stays the content; buffers too large for the
    /// content do not count, since they are destroyed once a right-size one exists. Without a viewporter only
    /// a size that held since the last frame gets its second buffer, so a resize does not allocate twice per
    /// size. `selected` is the buffer this frame renders into; it is never given up.
    fn start_ahead<R: CreateRequest<S, Buffer = B, Params = P>>(
        &mut self,
        requests: &mut R,
        content: (i32, i32),
        last: (i32, i32),
        selected: FrameSlot,
    ) {
        let (target, size) = if self.viewporter {
            let own = bucket(content);
            // A side within a quarter bucket of its edge may cross it next frame in either direction: a growing
            // one needs the next bucket now, and a shrinking one gets its buffers a bucket larger too, so that
            // coming back over the edge it just crossed still finds a free buffer.
            let side = |content: i32, last: i32, own: i32| {
                if content < own.saturating_sub(BUCKET / 4) {
                    (content, own)
                } else if content >= last {
                    (own.saturating_add(BUCKET), own.saturating_add(BUCKET))
                } else {
                    (content, own.saturating_add(BUCKET))
                }
            };
            let (width, height) = (side(content.0, last.0, own.0), side(content.1, last.1, own.1));
            ((width.0, height.0), (width.1, height.1))
        } else if content == last {
            (content, content)
        } else {
            return;
        };
        let viewporter = self.viewporter;
        let covers = |e: &Entry<B, P>| {
            if viewporter {
                e.slot.width >= target.0 && e.slot.height >= target.1 && !too_large(Self::size(e), content)
            } else {
                Self::size(e) == target
            }
        };
        // Each round adds one covering buffer and eviction never takes a covering one, so the count rises until
        // two cover the target or the pool has no room.
        while self.entries.iter().filter(|e| covers(e)).count() < 2 {
            if !(self.make_room(content) || self.give_up_for_target(content, selected, covers))
                || !self.start(requests, size)
            {
                return;
            }
        }
    }
    /// When every free buffer still fits the content, one that does not cover the target is destroyed so the
    /// target can be started, as long as two created buffers that fit the content remain: the one this frame
    /// renders into and one for the next frame while the compositor holds this one. A corner drag needs this:
    /// buffers grown for the side that neared its edge first fill the pool, and are too short once the other
    /// side nears its own edge.
    fn give_up_for_target(
        &mut self,
        content: (i32, i32),
        selected: FrameSlot,
        covers: impl Fn(&Entry<B, P>) -> bool,
    ) -> bool {
        let fitting = self
            .entries
            .iter()
            .filter(|e| e.buffer.is_some() && self.fits(e, content))
            .count();
        let victim = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.busy && e.buffer.is_some() && e.slot != selected && self.fits(e, content) && !covers(e))
            .min_by_key(|(_, e)| Self::area(e))
            .map(|(i, _)| i);
        match victim {
            Some(i) if fitting > 2 => {
                self.remove(i);
                true
            }
            _ => false,
        }
    }
    fn created(&mut self, key: BufferKey, buffer: B) -> PoolChange {
        let Some(e) = self.entries.iter_mut().find(|e| e.key == key && e.buffer.is_none()) else {
            buffer.destroy();
            return PoolChange::None;
        };
        if let Some(params) = e.params.take() {
            params.destroy();
        }
        e.buffer = Some(buffer);
        self.source.set_busy(e.slot, false);
        self.created_count += 1;
        self.proven = true;
        PoolChange::Created
    }
    fn failed(&mut self, key: BufferKey) -> PoolChange {
        if let Some(i) = self.entries.iter().position(|e| e.key == key) {
            if let Some(params) = self.entries[i].params.take() {
                params.destroy();
            }
            self.remove(i);
            PoolChange::Failed
        } else if let Some(i) = self.local_failed.iter().position(|k| *k == key) {
            self.local_failed.remove(i);
            PoolChange::Failed
        } else {
            PoolChange::None
        }
    }
    fn released(&mut self, key: BufferKey) -> PoolChange {
        let Some(i) = self
            .entries
            .iter()
            .position(|e| e.key == key && e.buffer.is_some() && e.busy)
        else {
            return PoolChange::None;
        };
        self.entries[i].busy = false;
        self.source.set_busy(self.entries[i].slot, false);
        if self.entries[i].retire {
            self.remove(i);
        }
        PoolChange::Freed
    }
    /// Routes the compositor's answer for `key`. A key from another pane or an earlier generation of this
    /// pool is ignored, except that a buffer created for it is destroyed: the answer was delivered here, so
    /// nothing else will ever free it.
    fn answer(&mut self, key: BufferKey, answer: Answer<B>) -> PoolChange {
        if key.pane != self.pane || key.generation != self.generation {
            if let Answer::Created(buffer) = answer {
                buffer.destroy();
            }
            return PoolChange::None;
        }
        match answer {
            Answer::Created(buffer) => self.created(key, buffer),
            Answer::Failed => self.failed(key),
            Answer::Released => self.released(key),
        }
    }
    fn submit(&mut self, slot: FrameSlot) {
        if let Some(e) = self
            .entries
            .iter_mut()
            .find(|e| e.slot == slot && e.buffer.is_some() && !e.busy)
        {
            self.source.submit(slot);
            self.source.set_busy(slot, true);
            e.busy = true;
        }
    }
}

struct WaylandCreate<'a, D> {
    globals: &'a Globals,
    qh: &'a QueueHandle<D>,
}
impl<D> CreateRequest<DmabufPool> for WaylandCreate<'_, D>
where
    D: Dispatch<ZwpLinuxBufferParamsV1, BufferKey> + Dispatch<WlBuffer, BufferKey> + 'static,
{
    type Buffer = WlBuffer;
    type Params = ZwpLinuxBufferParamsV1;
    fn create(
        &mut self,
        source: &DmabufPool,
        slot: FrameSlot,
        key: BufferKey,
        chosen: &Chosen,
    ) -> Result<Self::Params, String> {
        let (params, width, height) = self.params(source, slot, key, chosen)?;
        params.create(width, height, chosen.wrap_fourcc, Flags::empty());
        Ok(params)
    }
    fn create_immed(
        &mut self,
        source: &DmabufPool,
        slot: FrameSlot,
        key: BufferKey,
        chosen: &Chosen,
    ) -> Result<(Self::Params, Self::Buffer), String> {
        let (params, width, height) = self.params(source, slot, key, chosen)?;
        let buffer = params.create_immed(width, height, chosen.wrap_fourcc, Flags::empty(), self.qh, key);
        if !buffer.is_alive() {
            params.destroy();
            return Err("linux-dmabuf buffer could not be created".into());
        }
        Ok((params, buffer))
    }
}
impl<D> WaylandCreate<'_, D>
where
    D: Dispatch<ZwpLinuxBufferParamsV1, BufferKey> + Dispatch<WlBuffer, BufferKey> + 'static,
{
    /// Params carrying the slot's planes, checked against what the compositor advertised.
    fn params(
        &mut self,
        source: &DmabufPool,
        slot: FrameSlot,
        key: BufferKey,
        chosen: &Chosen,
    ) -> Result<(ZwpLinuxBufferParamsV1, i32, i32), String> {
        let planes = source.planes(slot);
        check_params(&planes)?;
        if !chosen.modifiers.contains(&planes.modifier) {
            return Err("dmabuf has an unadvertised modifier".into());
        }
        if !self.globals.dmabuf.is_alive() {
            return Err("linux-dmabuf global is no longer alive".into());
        }
        let params = self.globals.dmabuf.create_params(self.qh, key);
        if !params.is_alive() {
            return Err("linux-dmabuf params could not be created".into());
        }
        for (p, fd) in planes.fds.iter().enumerate() {
            params.add(
                fd.as_fd(),
                p as u32,
                planes.offsets[p],
                planes.strides[p],
                (planes.modifier >> 32) as u32,
                planes.modifier as u32,
            );
        }
        Ok((params, planes.width, planes.height))
    }
}

pub(crate) struct WlBufferPool {
    inner: Pool<DmabufPool, WlBuffer, ZwpLinuxBufferParamsV1>,
    device_checked: bool,
    last_failure: Option<String>,
}
impl WlBufferPool {
    pub(crate) fn new(mut pool: DmabufPool, chosen: Chosen, pane: PaneId, viewporter: bool) -> Self {
        pool.restrict_modifiers(&chosen.modifiers);
        Self {
            inner: Pool::new(pool, chosen, pane, viewporter),
            device_checked: false,
            last_failure: None,
        }
    }
    pub(crate) fn acquire<D>(&mut self, globals: &Globals, qh: &QueueHandle<D>, content: (i32, i32)) -> Acquire
    where
        D: Dispatch<ZwpLinuxBufferParamsV1, BufferKey> + Dispatch<WlBuffer, BufferKey> + 'static,
    {
        if !self.device_checked {
            self.device_checked = true;
            if let Err(err) = self.inner.source.check_device(globals.feedback.main_device) {
                let key = BufferKey {
                    pane: self.inner.pane,
                    generation: self.inner.generation,
                    id: self.inner.next_id,
                };
                self.inner.next_id = self.inner.next_id.wrapping_add(1);
                self.inner.failures.push_back((key, err));
                self.inner.local_failed.push(key);
                return Acquire::Pending;
            }
        }
        self.inner.acquire(&mut WaylandCreate { globals, qh }, content)
    }
    pub(crate) fn wl_buffer(&self, slot: FrameSlot) -> Option<&WlBuffer> {
        self.inner
            .entries
            .iter()
            .find(|e| e.slot == slot)?
            .buffer
            .as_ref()
            .filter(|b| b.is_alive())
    }
    pub(crate) fn buffer_size(&self, slot: FrameSlot) -> (i32, i32) {
        self.inner
            .entries
            .iter()
            .find(|e| e.slot == slot)
            .map(Pool::<DmabufPool, WlBuffer, ZwpLinuxBufferParamsV1>::size)
            .unwrap_or((0, 0))
    }
    /// Fences the rendered frame and reserves it immediately before the child's commit.
    pub(crate) fn submit(&mut self, slot: FrameSlot) {
        self.inner.submit(slot);
    }
    /// Local validation and allocation failures follow the same fallback path as compositor failures.
    pub(crate) fn take_event(&mut self) -> Option<PresentEvent> {
        let (key, reason) = self.inner.failures.pop_front()?;
        self.last_failure = Some(reason);
        Some(PresentEvent::BufferFailed { key })
    }
    /// Includes both resolved GPU paths when a device mismatch produced the last local failure.
    pub(crate) fn failure_reason(&self) -> Option<&str> {
        self.last_failure.as_deref()
    }
    pub(crate) fn on_event(&mut self, event: &PresentEvent) -> PoolChange {
        match event {
            PresentEvent::BufferCreated { key, buffer } => self.inner.answer(*key, Answer::Created(buffer.clone())),
            PresentEvent::BufferFailed { key } => self.inner.answer(*key, Answer::Failed),
            PresentEvent::BufferReleased { key } => self.inner.answer(*key, Answer::Released),
            _ => PoolChange::None,
        }
    }
    pub(crate) fn created_count(&self) -> u64 {
        self.inner.created_count
    }
    /// Call after destroying the child, with the pool's GL context current when available. A create still
    /// unanswered keeps its params: its `created` later reaches this pane's next pool, which destroys the
    /// buffer as another generation's, or, with no pool left, whoever receives it must destroy the buffer.
    pub(crate) fn release(mut self, context_is_current: bool) {
        self.inner.entries.clear();
        self.inner.source.release(context_is_current);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
    fn feedback(formats: Vec<(u32, u64)>) -> DmabufFeedback {
        DmabufFeedback {
            main_device: 1,
            formats,
        }
    }

    #[test]
    fn choose_format_prefers_xbgr_with_a_usable_modifier() {
        let chosen = choose_format(
            &feedback(vec![
                (DRM_FORMAT_ABGR8888, 8),
                (DRM_FORMAT_XBGR8888, 9),
                (DRM_FORMAT_XBGR8888, 8),
            ]),
            &[(8, false), (9, false)],
            true,
        )
        .unwrap();
        assert_eq!(chosen.wrap_fourcc, DRM_FORMAT_XBGR8888);
        assert_eq!(chosen.modifiers, vec![9, 8]);
        assert!(!chosen.opaque_region_needed);
    }

    #[test]
    fn choose_format_falls_back_to_abgr_with_an_opaque_region() {
        let chosen = choose_format(
            &feedback(vec![(DRM_FORMAT_XBGR8888, 0), (DRM_FORMAT_ABGR8888, 8)]),
            &[(8, false)],
            true,
        )
        .unwrap();
        assert_eq!(chosen.wrap_fourcc, DRM_FORMAT_ABGR8888);
        assert!(chosen.opaque_region_needed);
        assert_eq!(chosen.modifiers, vec![8]);
    }

    #[test]
    fn choose_format_refuses_another_device() {
        assert!(choose_format(&feedback(vec![(DRM_FORMAT_XBGR8888, 8)]), &[(8, false)], false).is_none());
    }

    #[test]
    fn choose_format_never_picks_linear_or_external_only() {
        assert!(choose_format(
            &feedback(vec![
                (DRM_FORMAT_XBGR8888, 0),
                (DRM_FORMAT_ABGR8888, 9),
                (DRM_FORMAT_ABGR8888, DRM_FORMAT_MOD_INVALID)
            ]),
            &[(0, false), (9, true), (DRM_FORMAT_MOD_INVALID, false)],
            true
        )
        .is_none());
    }

    #[derive(Default)]
    struct FakeSlots {
        sizes: Vec<(i32, i32)>,
        busy: RefCell<Vec<bool>>,
        freed: Vec<usize>,
        bound: Option<FrameSlot>,
    }

    impl SlotSource for FakeSlots {
        fn allocate(&mut self, size: (i32, i32)) -> Result<FrameSlot, String> {
            self.sizes.push(size);
            self.busy.get_mut().push(false);
            let id = self.sizes.len() - 1;
            let slot = FrameSlot {
                id,
                framebuffer: 0,
                width: size.0,
                height: size.1,
            };
            self.bind(slot)?;
            Ok(slot)
        }
        fn bind(&mut self, slot: FrameSlot) -> Result<(), String> {
            assert!(!self.busy.borrow()[slot.id]);
            self.bound = Some(slot);
            Ok(())
        }
        fn set_busy(&self, slot: FrameSlot, busy: bool) {
            self.busy.borrow_mut()[slot.id] = busy;
        }
        fn submit(&mut self, slot: FrameSlot) {
            assert_eq!(self.bound, Some(slot));
        }
        fn discard(&mut self, slot: FrameSlot) {
            assert!(!self.busy.borrow()[slot.id]);
            self.freed.push(slot.id);
        }
    }

    #[derive(Debug, Default)]
    struct FakeObject(Rc<Cell<usize>>);
    impl Destroy for FakeObject {
        fn destroy(&self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[derive(Default)]
    struct Requests {
        keys: Vec<BufferKey>,
        params: Vec<Rc<Cell<usize>>>,
        refuse: bool,
        /// The keys of immediate creates (also in `keys`) and their buffers' destroy counts.
        immediate: Vec<BufferKey>,
        immediate_buffers: Vec<Rc<Cell<usize>>>,
    }
    impl CreateRequest<FakeSlots> for Requests {
        type Buffer = FakeObject;
        type Params = FakeObject;
        fn create(&mut self, _: &FakeSlots, _: FrameSlot, key: BufferKey, _: &Chosen) -> Result<FakeObject, String> {
            if self.refuse {
                return Err("buffer import refused".into());
            }
            self.keys.push(key);
            let params = FakeObject::default();
            self.params.push(params.0.clone());
            Ok(params)
        }
        fn create_immed(
            &mut self,
            source: &FakeSlots,
            slot: FrameSlot,
            key: BufferKey,
            chosen: &Chosen,
        ) -> Result<(FakeObject, FakeObject), String> {
            let params = self.create(source, slot, key, chosen)?;
            self.immediate.push(key);
            let buffer = FakeObject::default();
            self.immediate_buffers.push(buffer.0.clone());
            Ok((params, buffer))
        }
    }
    type FakePool = Pool<FakeSlots, FakeObject, FakeObject>;
    fn pool(viewporter: bool) -> FakePool {
        Pool::new(
            FakeSlots::default(),
            Chosen {
                wrap_fourcc: DRM_FORMAT_ABGR8888,
                opaque_region_needed: true,
                modifiers: vec![8],
            },
            PaneId(1),
            viewporter,
        )
    }
    /// A buffer for `size`: the first one waits for `created`, later ones are created immediately.
    fn ready(pool: &mut FakePool, requests: &mut Requests, size: (i32, i32)) -> FrameSlot {
        match pool.acquire(requests, size) {
            Acquire::Ready(slot) => {
                assert!(pool.proven, "only a proven pool hands out a buffer it has just created");
                slot
            }
            Acquire::Pending => {
                let key = *requests.keys.last().unwrap();
                assert_eq!(pool.created(key, FakeObject::default()), PoolChange::Created);
                match pool.acquire(requests, size) {
                    Acquire::Ready(slot) => slot,
                    other => panic!("{other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn params_are_destroyed_once_after_created_or_failed() {
        for succeeds in [false, true] {
            let (mut p, mut r) = (pool(true), Requests::default());
            p.acquire(&mut r, (900, 400));
            assert_eq!(r.params[0].get(), 0);
            if succeeds {
                p.created(r.keys[0], FakeObject::default());
            } else {
                p.failed(r.keys[0]);
            }
            assert_eq!(r.params[0].get(), 1);
            drop(p);
            assert_eq!(r.params[0].get(), 1);
        }
    }

    #[test]
    fn teardown_keeps_unanswered_params_and_the_next_pool_destroys_a_late_created_buffer() {
        let (mut previous, mut requests) = (pool(true), Requests::default());
        assert_eq!(previous.acquire(&mut requests, (900, 400)), Acquire::Pending);
        let key = requests.keys[0];
        drop(previous);
        assert_eq!(requests.params[0].get(), 0);

        let mut next = pool(true);
        let buffer = FakeObject::default();
        let destroyed = buffer.0.clone();
        assert_eq!(next.answer(key, Answer::Created(buffer)), PoolChange::None);
        assert_eq!(destroyed.get(), 1);
        assert_eq!(requests.params[0].get(), 0);
    }

    #[test]
    fn a_refused_create_frees_the_slot_and_reports_failure() {
        let (mut p, mut r) = (
            pool(true),
            Requests {
                refuse: true,
                ..Requests::default()
            },
        );
        assert_eq!(p.acquire(&mut r, (900, 400)), Acquire::Pending);
        assert!(r.keys.is_empty());
        assert_eq!(p.source.freed, vec![0]);
        let (key, reason) = p.failures.pop_front().unwrap();
        assert_eq!(reason, "buffer import refused");
        assert_eq!(p.failed(key), PoolChange::Failed);
    }

    #[test]
    fn acquire_chooses_the_smallest_usable_created_buffer() {
        let (mut p, mut r) = (pool(true), Requests::default());
        let small = ready(&mut p, &mut r, (900, 400));
        let large = ready(&mut p, &mut r, (1100, 400));
        assert_ne!(small, large);
        let sent = r.keys.len();
        assert_eq!(p.acquire(&mut r, (900, 400)), Acquire::Ready(small));
        assert_eq!(p.source.bound, Some(small));
        assert_eq!(r.keys.len(), sent, "a size two buffers already cover starts no create");
    }

    #[test]
    fn the_first_buffer_waits_for_created_and_a_later_jump_renders_at_once() {
        let (mut p, mut r) = (pool(true), Requests::default());
        assert_eq!(p.acquire(&mut r, (900, 400)), Acquire::Pending);
        assert!(
            r.immediate.is_empty(),
            "nothing is known about the format before the first created"
        );
        let first = r.keys[0];
        p.created(first, FakeObject::default());
        assert!(matches!(p.acquire(&mut r, (900, 400)), Acquire::Ready(_)));

        // A window that grows past every buffer and prefetch at once (tiling, a scale change).
        let Acquire::Ready(slot) = p.acquire(&mut r, (2900, 1800)) else {
            panic!("a jump in a proven pool must not wait for the compositor");
        };
        assert_eq!((slot.width, slot.height), (2944, 1920));
        assert_eq!(p.source.bound, Some(slot));
        assert_eq!(r.immediate.len(), 1);
        p.submit(slot);
        assert!(p.entries.iter().any(|e| e.slot == slot && e.busy));
    }

    #[test]
    fn a_jump_onto_an_unanswered_prefetch_still_renders_at_once() {
        let (mut p, mut r) = (pool(true), Requests::default());
        let slot = ready(&mut p, &mut r, (1000, 1000));
        p.submit(slot);
        // Near the bucket edge on both sides: the next bucket is prefetched, and its creates are not answered yet.
        assert!(matches!(p.acquire(&mut r, (1020, 1020)), Acquire::Ready(_)));
        assert!(p.entries.iter().any(|e| e.buffer.is_none() && e.slot.width >= 1100));
        let before = r.immediate.len();
        let Acquire::Ready(slot) = p.acquire(&mut r, (1100, 1100)) else {
            panic!("a proven pool must not wait for a prefetch to be answered");
        };
        assert!(slot.width >= 1100 && slot.height >= 1100);
        assert_eq!(r.immediate.len(), before + 1);
        // The same size again waits for the answers rather than creating more.
        p.submit(slot);
        assert_eq!(p.acquire(&mut r, (1100, 1100)), Acquire::Pending);
        assert_eq!(r.immediate.len(), before + 1);
    }

    #[test]
    fn the_integer_path_renders_a_new_size_at_once_once_proven() {
        let (mut p, mut r) = (pool(false), Requests::default());
        ready(&mut p, &mut r, (1000, 400));
        let Acquire::Ready(slot) = p.acquire(&mut r, (1500, 700)) else {
            panic!("not ready");
        };
        assert_eq!((slot.width, slot.height), (1500, 700));
        assert_eq!(r.immediate.len(), 1);
    }

    #[test]
    fn an_immediate_buffer_and_its_params_are_destroyed_once_with_it() {
        let (mut p, mut r) = (pool(true), Requests::default());
        ready(&mut p, &mut r, (900, 400));
        let Acquire::Ready(_) = p.acquire(&mut r, (2900, 1800)) else {
            panic!("not ready");
        };
        let key = r.immediate[0];
        let params = r.params[r.keys.iter().position(|k| *k == key).unwrap()].clone();
        let buffer = r.immediate_buffers[0].clone();
        assert_eq!((params.get(), buffer.get()), (0, 0));
        // Back to the small size: the large free buffer is too large and is destroyed.
        assert!(matches!(p.acquire(&mut r, (900, 400)), Acquire::Ready(_)));
        assert!(!p.entries.iter().any(|e| e.key == key));
        assert_eq!((params.get(), buffer.get()), (1, 1));
        drop(p);
        assert_eq!((params.get(), buffer.get()), (1, 1));
    }

    #[test]
    fn a_failed_immediate_import_reports_failed() {
        let (mut p, mut r) = (pool(true), Requests::default());
        ready(&mut p, &mut r, (900, 400));
        let Acquire::Ready(_) = p.acquire(&mut r, (2900, 1800)) else {
            panic!("not ready");
        };
        let key = r.immediate[0];
        assert_eq!(p.answer(key, Answer::Failed), PoolChange::Failed);
        assert!(!p.entries.iter().any(|e| e.key == key));
        assert_eq!(r.immediate_buffers[0].get(), 1);
    }

    #[test]
    fn a_refused_immediate_create_frees_the_slot_and_reports_failure() {
        let (mut p, mut r) = (pool(true), Requests::default());
        ready(&mut p, &mut r, (900, 400));
        r.refuse = true;
        assert_eq!(p.acquire(&mut r, (2900, 1800)), Acquire::Pending);
        assert_eq!(p.failures.len(), 1);
        assert!(r.immediate.is_empty());
        assert!(p.entries.iter().all(|e| (e.slot.width, e.slot.height) != (2944, 1920)));
    }

    #[test]
    fn acquire_waits_for_created() {
        let (mut p, mut r) = (pool(true), Requests::default());
        assert_eq!(p.acquire(&mut r, (900, 400)), Acquire::Pending);
        assert_eq!(p.acquire(&mut r, (900, 400)), Acquire::Pending);
        assert_eq!(r.keys.len(), 1);
        p.created(r.keys[0], FakeObject::default());
        assert!(matches!(p.acquire(&mut r, (900, 400)), Acquire::Ready(_)));
    }

    #[test]
    fn a_committed_slot_is_busy_until_released() {
        let (mut p, mut r) = (pool(true), Requests::default());
        let slot = ready(&mut p, &mut r, (900, 400));
        p.submit(slot);
        assert_eq!(p.acquire(&mut r, (900, 400)), Acquire::Pending);
        assert_eq!(p.released(r.keys[0]), PoolChange::Freed);
        assert_eq!(p.acquire(&mut r, (900, 400)), Acquire::Ready(slot));
    }

    #[test]
    fn at_four_busy_acquire_is_full_and_nothing_waits() {
        let (mut p, mut r) = (pool(true), Requests::default());
        for _ in 0..MAX_BUFFERS {
            let slot = ready(&mut p, &mut r, (900, 400));
            p.submit(slot);
        }
        assert_eq!(p.acquire(&mut r, (900, 400)), Acquire::Full);
        assert_eq!(r.keys.len(), MAX_BUFFERS);
    }

    #[test]
    fn a_failed_buffer_reports_failed() {
        let (mut p, mut r) = (pool(true), Requests::default());
        p.acquire(&mut r, (900, 400));
        assert_eq!(p.failed(r.keys[0]), PoolChange::Failed);
        assert_eq!(p.entries.len(), 0);
    }

    #[test]
    fn a_release_for_an_old_generation_is_ignored() {
        let (mut p, mut r) = (pool(true), Requests::default());
        let slot = ready(&mut p, &mut r, (900, 400));
        p.submit(slot);
        let mut old = r.keys[0];
        old.generation = old.generation.wrapping_sub(1);
        assert_eq!(p.answer(old, Answer::Released), PoolChange::None);
        assert_eq!(p.acquire(&mut r, (900, 400)), Acquire::Pending);
        assert_eq!(p.answer(r.keys[0], Answer::Released), PoolChange::Freed);
    }

    #[test]
    fn a_buffer_created_for_an_old_generation_is_destroyed() {
        let (mut p, mut r) = (pool(true), Requests::default());
        p.acquire(&mut r, (900, 400));
        let mut old = r.keys[0];
        old.generation = old.generation.wrapping_sub(1);
        let buffer = FakeObject::default();
        let destroyed = buffer.0.clone();
        assert_eq!(p.answer(old, Answer::Created(buffer)), PoolChange::None);
        assert_eq!(destroyed.get(), 1, "nothing else would ever free that buffer");
        assert!(
            p.entries[0].buffer.is_none(),
            "this generation's create still waits for its own answer"
        );
    }

    #[test]
    fn an_empty_content_waits_without_creating_or_failing() {
        let (mut p, mut r) = (pool(true), Requests::default());
        for content in [(0, 400), (900, 0), (0, 0)] {
            assert_eq!(p.acquire(&mut r, content), Acquire::Pending);
        }
        assert!(r.keys.is_empty());
        assert!(
            p.failures.is_empty() && p.local_failed.is_empty(),
            "an empty rectangle is not a failure"
        );
        assert!(matches!(
            ready(&mut p, &mut r, (900, 400)),
            FrameSlot { width: 1024, .. }
        ));
    }

    #[test]
    fn a_too_large_buffer_is_freed_idle_or_at_release() {
        for busy in [false, true] {
            let (mut p, mut r) = (pool(true), Requests::default());
            let slot = ready(&mut p, &mut r, (900, 400));
            if busy {
                p.submit(slot);
                // The shrink gets its own bucket at once while the oversized buffer is still on screen.
                let Acquire::Ready(small) = p.acquire(&mut r, (80, 80)) else {
                    panic!("not ready");
                };
                assert_eq!((small.width, small.height), (128, 128));
                assert!(!p.source.freed.contains(&slot.id), "a busy buffer is the compositor's");
                p.released(r.keys[0]);
                assert!(p.source.freed.contains(&slot.id));
            } else {
                // The oversized buffer crops the smaller content until one of the right size exists.
                assert_eq!(p.acquire(&mut r, (80, 80)), Acquire::Ready(slot));
                p.created(*r.keys.last().unwrap(), FakeObject::default());
                assert!(matches!(
                    p.acquire(&mut r, (80, 80)),
                    Acquire::Ready(FrameSlot {
                        width: 128,
                        height: 128,
                        ..
                    })
                ));
                assert!(p.source.freed.contains(&slot.id));
            }
        }
    }

    /// Plays one frame per size and returns the sizes that found no free buffer. Each create is answered at the
    /// start of the frame after it was sent. With `hold`, a committed buffer stays the compositor's until the
    /// next frame's commit replaces it, as wlroots and Mutter keep a dmabuf on screen; without it, each buffer
    /// is released before the next frame; the last one is released when the drag ends.
    fn drag(
        p: &mut FakePool,
        r: &mut Requests,
        sizes: impl IntoIterator<Item = (i32, i32)>,
        hold: bool,
    ) -> Vec<(i32, i32)> {
        let mut shown = None;
        let mut misses = Vec::new();
        for size in sizes {
            let sent: Vec<BufferKey> = p.entries.iter().filter(|e| e.buffer.is_none()).map(|e| e.key).collect();
            for key in sent {
                assert_eq!(p.created(key, FakeObject::default()), PoolChange::Created);
            }
            match p.acquire(r, size) {
                Acquire::Ready(slot) => {
                    let key = p.entries.iter().find(|e| e.slot == slot).unwrap().key;
                    p.submit(slot);
                    let replaced = if hold { shown.replace(key) } else { Some(key) };
                    if let Some(old) = replaced {
                        assert_eq!(p.released(old), PoolChange::Freed);
                    }
                }
                _ => misses.push(size),
            }
        }
        // The held buffer goes back when the drag ends, so a later drag on the same pool starts with it free.
        if let Some(old) = shown {
            assert_eq!(p.released(old), PoolChange::Freed);
        }
        misses
    }

    #[test]
    fn a_drag_across_bucket_edges_never_misses_and_grows_only_the_side_that_nears_its_edge() {
        for hold in [true, false] {
            let (mut p, mut r) = (pool(true), Requests::default());
            ready(&mut p, &mut r, (1000, 400));
            let misses = drag(&mut p, &mut r, (1001..=1400).map(|w| (w, 400)), hold);
            assert!(misses.is_empty(), "hold {hold}: misses: {misses:?}");
            // The first 1024, then two each of 1152, 1280, 1408 and 1536, a quarter bucket before each edge.
            assert_eq!(
                r.keys.len(),
                9,
                "hold {hold}: two creates per bucket edge, none churned"
            );
            assert!(
                p.entries.iter().all(|e| e.slot.height == 512),
                "the height never nears its edge"
            );
            assert!(p.entries.len() <= MAX_BUFFERS);
        }
    }

    #[test]
    fn a_shrinking_drag_crops_a_larger_buffer_until_its_own_bucket_is_created() {
        for hold in [true, false] {
            let (mut p, mut r) = (pool(true), Requests::default());
            ready(&mut p, &mut r, (1400, 400));
            let misses = drag(&mut p, &mut r, (700..1400).rev().map(|w| (w, 400)), hold);
            assert!(misses.is_empty(), "hold {hold}: misses: {misses:?}");
            // Two buffers each of 1536 down to 896: the start's own bucket and the one above it, then near each
            // edge the shrinking side's buffers are made a bucket larger, in case it comes back over the edge.
            assert_eq!(r.keys.len(), 12, "hold {hold}");
            drag(&mut p, &mut r, [(700, 400), (700, 400)], hold);
            assert!(p
                .entries
                .iter()
                .all(|e| !too_large(Pool::<FakeSlots, FakeObject, FakeObject>::size(e), (700, 400))));
        }
    }

    #[test]
    fn a_held_buffer_leaves_a_second_one_for_the_next_frame() {
        // One pixel a frame on each moving side, while the compositor holds the shown buffer until it is
        // replaced: both sides growing, one growing while the other shrinks, then one side shrinking alone.
        let (mut p, mut r) = (pool(true), Requests::default());
        ready(&mut p, &mut r, (800, 400));
        let out = (0..400).map(|i| (800 + i, 400 + i));
        let across = (0..400).map(|i| (1199 - i, 799 + i));
        let down = (0..799).map(|i| (800, 1198 - i));
        let misses = drag(&mut p, &mut r, out.chain(across).chain(down), true);
        assert!(misses.is_empty(), "misses: {misses:?}");
        assert!(p.entries.len() <= MAX_BUFFERS);
    }

    #[test]
    fn a_corner_drag_with_sides_at_different_rates_never_misses() {
        // Buffers grown for the faster side fill the pool before the slower side nears its own edge; those
        // must give way to buffers that cover both sides.
        for (a, b) in [(1, 2), (2, 1), (1, 3), (3, 1), (2, 5), (5, 2)] {
            for start in [(900, 400), (1000, 470), (960, 300), (1010, 420)] {
                let (mut p, mut r) = (pool(true), Requests::default());
                ready(&mut p, &mut r, start);
                let sizes = (0..400).map(|i| (start.0 + a * i, start.1 + b * i));
                let misses = drag(&mut p, &mut r, sizes, true);
                assert!(misses.is_empty(), "{a}:{b} from {start:?}: misses: {misses:?}");
                assert!(p.entries.len() <= MAX_BUFFERS);
            }
        }
    }

    #[test]
    fn a_side_that_crosses_back_over_the_edge_it_just_left_never_misses() {
        // A drag that overshoots and comes back: one side or both reverse at many points, near and far from
        // a bucket edge, while the compositor holds the shown buffer.
        for (a, b) in [(1, 2), (2, 1), (3, 5), (5, 3), (1, 7), (7, 1), (4, 4)] {
            for flip in [(true, false), (false, true), (true, true)] {
                for at in (5..140).step_by(3) {
                    for ow in (0..128).step_by(16) {
                        let start = (1000 + ow, 600 + ow / 2);
                        let (mut w, mut h) = start;
                        let mut sizes = Vec::new();
                        for i in 0..300 {
                            let back = i >= at && i < at * 2;
                            w += if back && flip.0 { -a } else { a };
                            h += if back && flip.1 { -b } else { b };
                            sizes.push((w, h));
                        }
                        let (mut p, mut r) = (pool(true), Requests::default());
                        ready(&mut p, &mut r, start);
                        let misses = drag(&mut p, &mut r, sizes, true);
                        assert!(
                            misses.is_empty(),
                            "{a}:{b} {flip:?} at {at} from {start:?}: misses: {misses:?}"
                        );
                        assert!(p.entries.len() <= MAX_BUFFERS);
                    }
                }
            }
        }
    }

    #[test]
    fn an_unanswered_create_is_never_dropped() {
        // Before the first answer every size waits for its own create.
        let (mut p, mut r) = (pool(false), Requests::default());
        for width in 1000..=1010 {
            assert_eq!(p.acquire(&mut r, (width, 400)), Acquire::Pending);
        }
        assert_eq!(
            r.keys.len(),
            MAX_BUFFERS,
            "unanswered creates fill the pool and nothing evicts them"
        );
        assert!(r.params.iter().all(|c| c.get() == 0));
        for &key in &r.keys {
            p.created(key, FakeObject::default());
        }
        assert!(
            r.params.iter().all(|c| c.get() == 1),
            "params go once their answer arrives"
        );
        assert!(matches!(p.acquire(&mut r, (1010, 400)), Acquire::Ready(_)));
        assert!(
            p.entries.iter().all(|e| (e.slot.width, e.slot.height) == (1010, 400)),
            "answered buffers of other sizes are freed"
        );

        let (mut p, mut r) = (pool(true), Requests::default());
        ready(&mut p, &mut r, (1000, 400));
        assert_eq!(r.keys.len(), 3, "two buffers of the next bucket up are being created");
        assert!(matches!(p.acquire(&mut r, (100, 100)), Acquire::Ready(_)));
        assert!(
            p.entries.iter().any(|e| e.key == r.keys[1]),
            "a prefetch that became too large still waits"
        );
        assert_eq!(r.params[1].get(), 0);
        drop(p);
        assert_eq!(
            r.params[1].get(),
            0,
            "teardown leaves an unanswered create to its answer"
        );
    }

    #[test]
    fn a_drag_inside_one_bucket_creates_no_buffer_and_never_misses() {
        for hold in [true, false] {
            // Near the bucket's edge the next bucket is prefetched, and it serves the rest of the drag.
            let (mut p, mut r) = (pool(true), Requests::default());
            ready(&mut p, &mut r, (1000, 400));
            assert_eq!(p.created_count, 1);
            let misses = drag(&mut p, &mut r, (1001..=1060).map(|w| (w, 400)), hold);
            assert!(misses.is_empty(), "hold {hold}: misses: {misses:?}");
            assert_eq!(
                p.created_count, 3,
                "hold {hold}: only the next bucket's two buffers were added"
            );
            assert_eq!(r.keys.len(), 3);
            assert_eq!(p.entries.iter().filter(|e| e.slot.width == 1152).count(), 2);

            // Away from the edge only the second buffer of the content's own bucket is added.
            let (mut p, mut r) = (pool(true), Requests::default());
            ready(&mut p, &mut r, (900, 400));
            let misses = drag(&mut p, &mut r, (901..=960).map(|w| (w, 400)), hold);
            assert!(misses.is_empty(), "hold {hold}: misses: {misses:?}");
            assert_eq!(p.created_count, 2, "hold {hold}");
            assert!(p.entries.iter().all(|e| (e.slot.width, e.slot.height) == (1024, 512)));
        }
    }

    #[test]
    fn the_integer_path_allocates_per_size() {
        let (mut p, mut r) = (pool(false), Requests::default());
        let slot = ready(&mut p, &mut r, (1000, 400));
        assert_eq!((slot.width, slot.height), (1000, 400));
        let Acquire::Ready(slot) = p.acquire(&mut r, (1001, 400)) else {
            panic!("not ready");
        };
        assert_eq!((slot.width, slot.height), (1001, 400));
        assert_eq!(p.created_count, 2);
    }

    #[test]
    fn the_integer_path_keeps_a_second_buffer_only_for_a_size_that_holds() {
        let (mut p, mut r) = (pool(false), Requests::default());
        ready(&mut p, &mut r, (1000, 400));
        assert_eq!(
            r.keys.len(),
            2,
            "a size seen twice gets the buffer the next frame needs"
        );
        let misses = drag(&mut p, &mut r, [(1000, 400); 10], true);
        assert!(misses.is_empty(), "misses: {misses:?}");
        assert_eq!(p.created_count, 2);

        // A size that changes every frame gets its one buffer at once and starts no second.
        let sent = r.keys.len();
        let misses = drag(&mut p, &mut r, (1001..=1003).map(|w| (w, 400)), true);
        assert!(misses.is_empty(), "misses: {misses:?}");
        assert_eq!(r.keys.len(), sent + 3);
        assert_eq!(r.immediate.len(), 3);
    }
}
