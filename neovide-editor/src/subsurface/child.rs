//! Child-surface requests stay on the GTK thread, with layout changes cached until the parent paints.

use std::os::fd::{AsFd, FromRawFd, OwnedFd};

use wayland_client::{
    protocol::{
        wl_buffer::WlBuffer,
        wl_callback::WlCallback,
        wl_compositor::WlCompositor,
        wl_region::WlRegion,
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
        wl_subcompositor::WlSubcompositor,
        wl_subsurface::WlSubsurface,
        wl_surface::WlSurface,
    },
    Dispatch, Proxy, QueueHandle,
};
use wayland_protocols::wp::{
    presentation_time::client::{wp_presentation::WpPresentation, wp_presentation_feedback::WpPresentationFeedback},
    single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1,
    viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
};

use super::{
    geometry::{Placement, Rect},
    types::{FrameData, Globals, PaneId},
};
use crate::dmabuf_target::Chosen;

/// The user data of the underlay's `wl_buffer`. Its one event, `release`, needs no answer: the buffer's pixel never
/// changes, and a new colour is a new buffer.
pub(crate) struct Underlay;

/// Owns one child's protocol objects. Constructors report failure instead of using an inert proxy.
/// All other methods send only when their target and referenced objects remain alive.
pub(crate) trait ChildRequests {
    fn create_surface(&mut self) -> bool;
    fn get_subsurface(&mut self, parent: &WlSurface) -> bool;
    fn place_below(&mut self, parent: &WlSurface);
    fn set_input_region_empty(&mut self) -> bool;
    fn set_opaque_region_all(&mut self) -> bool;
    fn set_sync(&mut self);
    fn set_desync(&mut self);
    fn set_position(&mut self, x: i32, y: i32);
    fn get_viewport(&mut self) -> bool;
    fn set_source(&mut self, x: i32, y: i32, width: i32, height: i32);
    fn set_destination(&mut self, width: i32, height: i32);
    fn set_buffer_scale(&mut self, scale: i32);
    fn attach(&mut self, buffer: &WlBuffer);
    fn attach_null(&mut self);
    fn damage_buffer(&mut self, x: i32, y: i32, width: i32, height: i32);
    fn frame(&mut self, data: FrameData);
    fn feedback(&mut self, data: FrameData);
    fn commit(&mut self);
    fn destroy_subsurface(&mut self);
    fn destroy_viewport(&mut self);
    fn destroy_surface(&mut self);
    fn is_ready(&self) -> bool;
    fn buffer_is_alive(&self, buffer: &WlBuffer) -> bool;
    /// The underlay: a second child surface of the same parent, with a viewport, no input and an opaque region over
    /// all of it, left in sync mode for its whole life so each of its changes lands with a parent commit. False
    /// (and nothing left behind) without a viewporter, or with neither single-pixel buffers nor `wl_shm`.
    fn create_underlay(&mut self) -> bool;
    fn place_underlay_below_child(&mut self);
    /// Attaches a new one-pixel buffer of this colour to the underlay; the one it replaces is destroyed once the
    /// next underlay commit is sent. False when no buffer could be made.
    fn underlay_buffer(&mut self, rgb: [u8; 3]) -> bool;
    /// The underlay's position (parent state) and its viewport's destination (its own).
    fn underlay_rect(&mut self, rect: Rect);
    fn underlay_attach_null(&mut self);
    fn underlay_commit(&mut self);
    /// Destroys the buffers an earlier underlay commit replaced, once the parent has committed and so applied it.
    fn release_retired_underlay_buffers(&mut self);
    fn destroy_underlay(&mut self);
}

pub(crate) struct WaylandRequests<D> {
    compositor: WlCompositor,
    subcompositor: WlSubcompositor,
    viewporter: Option<WpViewporter>,
    presentation: Option<WpPresentation>,
    single_pixel: Option<WpSinglePixelBufferManagerV1>,
    shm: Option<WlShm>,
    qh: QueueHandle<D>,
    surface: Option<WlSurface>,
    subsurface: Option<WlSubsurface>,
    viewport: Option<WpViewport>,
    parent: Option<WlSurface>,
    underlay: Option<UnderlayObjects>,
}

struct UnderlayObjects {
    surface: WlSurface,
    subsurface: WlSubsurface,
    viewport: WpViewport,
    buffer: Option<WlBuffer>,
    /// Replaced buffers, destroyed after the parent commit that applies the underlay commit replacing them (the
    /// underlay is in sync mode, so its own commit is only cached). Their pixel is never written again either way.
    retired: Vec<WlBuffer>,
}

impl UnderlayObjects {
    fn alive(&self) -> bool {
        self.surface.is_alive() && self.subsurface.is_alive() && self.viewport.is_alive()
    }
}

/// One opaque pixel in an anonymous file, `wl_shm`'s `xrgb8888` (little-endian B, G, R, X).
fn shm_pixel(rgb: [u8; 3]) -> Option<OwnedFd> {
    // SAFETY: a plain syscall with a NUL-terminated name; a valid descriptor is owned by the `OwnedFd` from here on.
    let fd = unsafe { libc::memfd_create(c"eitri-underlay".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    // SAFETY: `fd` was just returned by `memfd_create` and nothing else owns it.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let pixel = [rgb[2], rgb[1], rgb[0], 0xff];
    let mut file = std::fs::File::from(fd);
    std::io::Write::write_all(&mut file, &pixel).ok()?;
    Some(OwnedFd::from(file))
}

/// A colour channel spread over the whole `u32` range, as `wp_single_pixel_buffer_manager_v1` takes it.
fn channel32(value: u8) -> u32 {
    u32::from(value) * 0x0101_0101
}

impl<D> WaylandRequests<D> {
    pub(crate) fn new(globals: &Globals, qh: QueueHandle<D>) -> Self {
        Self {
            compositor: globals.compositor.clone(),
            subcompositor: globals.subcompositor.clone(),
            viewporter: globals.viewporter.clone(),
            presentation: globals.presentation.clone(),
            single_pixel: globals.single_pixel.clone(),
            shm: globals.shm.clone(),
            qh,
            surface: None,
            subsurface: None,
            viewport: None,
            parent: None,
            underlay: None,
        }
    }
    fn underlay(&mut self) -> Option<&mut UnderlayObjects> {
        self.underlay.as_mut().filter(|underlay| underlay.alive())
    }
    fn new_underlay_buffer(&self, rgb: [u8; 3]) -> Option<WlBuffer>
    where
        D: Dispatch<WlBuffer, Underlay> + Dispatch<WlShmPool, ()> + 'static,
    {
        if let Some(manager) = self.single_pixel.as_ref().filter(|m| m.is_alive()) {
            let [r, g, b] = rgb.map(channel32);
            let buffer = manager.create_u32_rgba_buffer(r, g, b, u32::MAX, &self.qh, Underlay);
            return buffer.is_alive().then_some(buffer);
        }
        let shm = self.shm.as_ref().filter(|shm| shm.is_alive())?;
        let fd = shm_pixel(rgb)?;
        let pool = shm.create_pool(fd.as_fd(), 4, &self.qh, ());
        let buffer = pool.create_buffer(0, 1, 1, 4, wl_shm::Format::Xrgb8888, &self.qh, Underlay);
        // The buffer keeps the pool's memory; the pool object itself is no longer needed.
        pool.destroy();
        buffer.is_alive().then_some(buffer)
    }
    fn surface(&self) -> Option<&WlSurface> {
        self.surface.as_ref().filter(|surface| surface.is_alive())
    }
    fn subsurface(&self) -> Option<&WlSubsurface> {
        self.subsurface.as_ref().filter(|surface| surface.is_alive())
    }
    fn viewport(&self) -> Option<&WpViewport> {
        self.viewport.as_ref().filter(|viewport| viewport.is_alive())
    }
}

impl<D> ChildRequests for WaylandRequests<D>
where
    D: Dispatch<WlSurface, ()>
        + Dispatch<WlSubsurface, ()>
        + Dispatch<WlRegion, ()>
        + Dispatch<WpViewport, ()>
        + Dispatch<WlCallback, FrameData>
        + Dispatch<WpPresentationFeedback, FrameData>
        + Dispatch<WlBuffer, Underlay>
        + Dispatch<WlShmPool, ()>
        + 'static,
{
    fn create_surface(&mut self) -> bool {
        if self.surface.is_some() || !self.compositor.is_alive() || self.compositor.version() < 4 {
            return false;
        }
        let surface = self.compositor.create_surface(&self.qh, ());
        let alive = surface.is_alive();
        self.surface = Some(surface);
        alive
    }
    fn get_subsurface(&mut self, parent: &WlSurface) -> bool {
        if self.subsurface.is_some() || !parent.is_alive() || !self.subcompositor.is_alive() {
            return false;
        }
        let Some(surface) = self.surface() else { return false };
        if surface == parent || surface.backend().upgrade() != parent.backend().upgrade() {
            return false;
        }
        let subsurface = self.subcompositor.get_subsurface(surface, parent, &self.qh, ());
        let alive = subsurface.is_alive();
        self.subsurface = Some(subsurface);
        self.parent = Some(parent.clone());
        alive
    }
    fn place_below(&mut self, parent: &WlSurface) {
        if self.parent.as_ref() == Some(parent) && parent.is_alive() {
            if let Some(subsurface) = self.subsurface() {
                subsurface.place_below(parent);
            }
        }
    }
    fn set_input_region_empty(&mut self) -> bool {
        let Some(surface) = self.surface() else { return false };
        if !self.compositor.is_alive() {
            return false;
        }
        let region = self.compositor.create_region(&self.qh, ());
        if !region.is_alive() {
            return false;
        }
        surface.set_input_region(Some(&region));
        region.destroy();
        true
    }
    fn set_opaque_region_all(&mut self) -> bool {
        let Some(surface) = self.surface() else { return false };
        if !self.compositor.is_alive() {
            return false;
        }
        let region = self.compositor.create_region(&self.qh, ());
        if !region.is_alive() {
            return false;
        }
        // The compositor clips this region, so future growth remains opaque without another request.
        region.add(0, 0, i32::MAX, i32::MAX);
        surface.set_opaque_region(Some(&region));
        region.destroy();
        true
    }
    fn set_sync(&mut self) {
        if let Some(subsurface) = self.subsurface() {
            subsurface.set_sync();
        }
    }
    fn set_desync(&mut self) {
        if let Some(subsurface) = self.subsurface() {
            subsurface.set_desync();
        }
    }
    fn set_position(&mut self, x: i32, y: i32) {
        if let Some(subsurface) = self.subsurface() {
            subsurface.set_position(x, y);
        }
    }
    fn get_viewport(&mut self) -> bool {
        if self.viewport.is_some() {
            return false;
        }
        let Some(surface) = self.surface() else { return false };
        let Some(viewporter) = self.viewporter.as_ref().filter(|v| v.is_alive()) else {
            return false;
        };
        let viewport = viewporter.get_viewport(surface, &self.qh, ());
        let alive = viewport.is_alive();
        self.viewport = Some(viewport);
        alive
    }
    fn set_source(&mut self, x: i32, y: i32, width: i32, height: i32) {
        if let Some(viewport) = self.viewport() {
            viewport.set_source(f64::from(x), f64::from(y), f64::from(width), f64::from(height));
        }
    }
    fn set_destination(&mut self, width: i32, height: i32) {
        if let Some(viewport) = self.viewport() {
            viewport.set_destination(width, height);
        }
    }
    fn set_buffer_scale(&mut self, scale: i32) {
        if let Some(surface) = self.surface() {
            surface.set_buffer_scale(scale);
        }
    }
    fn attach(&mut self, buffer: &WlBuffer) {
        if self.buffer_is_alive(buffer) {
            if let Some(surface) = self.surface() {
                surface.attach(Some(buffer), 0, 0);
            }
        }
    }
    fn attach_null(&mut self) {
        if let Some(surface) = self.surface() {
            surface.attach(None, 0, 0);
        }
    }
    fn damage_buffer(&mut self, x: i32, y: i32, width: i32, height: i32) {
        if let Some(surface) = self.surface() {
            surface.damage_buffer(x, y, width, height);
        }
    }
    fn frame(&mut self, data: FrameData) {
        if let Some(surface) = self.surface() {
            surface.frame(&self.qh, data);
        }
    }
    fn feedback(&mut self, data: FrameData) {
        if let (Some(surface), Some(presentation)) =
            (self.surface(), self.presentation.as_ref().filter(|p| p.is_alive()))
        {
            presentation.feedback(surface, &self.qh, data);
        }
    }
    fn commit(&mut self) {
        if let Some(surface) = self.surface() {
            surface.commit();
        }
    }
    fn destroy_subsurface(&mut self) {
        if let Some(subsurface) = self.subsurface.take().filter(|s| s.is_alive()) {
            subsurface.destroy();
        }
    }
    fn destroy_viewport(&mut self) {
        if let Some(viewport) = self.viewport.take().filter(|v| v.is_alive()) {
            viewport.destroy();
        }
    }
    fn destroy_surface(&mut self) {
        if let Some(surface) = self.surface.take().filter(|s| s.is_alive()) {
            surface.destroy();
        }
    }
    fn is_ready(&self) -> bool {
        self.surface().is_some()
            && self.subsurface().is_some()
            && self.parent.as_ref().is_some_and(|parent| parent.is_alive())
            && (self.viewport.is_none() || self.viewport().is_some())
    }
    fn buffer_is_alive(&self, buffer: &WlBuffer) -> bool {
        buffer.is_alive()
            && self
                .surface()
                .is_some_and(|s| s.backend().upgrade() == buffer.backend().upgrade())
    }
    fn create_underlay(&mut self) -> bool {
        if self.underlay.is_some() || !self.compositor.is_alive() || !self.subcompositor.is_alive() {
            return false;
        }
        let single_pixel = self.single_pixel.as_ref().is_some_and(|m| m.is_alive());
        let shm = self.shm.as_ref().is_some_and(|shm| shm.is_alive());
        let (Some(parent), Some(viewporter)) = (
            self.parent.clone().filter(|parent| parent.is_alive()),
            self.viewporter.clone().filter(|v| v.is_alive()),
        ) else {
            return false;
        };
        if !single_pixel && !shm {
            return false;
        }
        let surface = self.compositor.create_surface(&self.qh, ());
        let subsurface = self.subcompositor.get_subsurface(&surface, &parent, &self.qh, ());
        let viewport = viewporter.get_viewport(&surface, &self.qh, ());
        let input = self.compositor.create_region(&self.qh, ());
        surface.set_input_region(Some(&input));
        input.destroy();
        let opaque = self.compositor.create_region(&self.qh, ());
        // The compositor clips this region, so any size stays opaque without another request.
        opaque.add(0, 0, i32::MAX, i32::MAX);
        surface.set_opaque_region(Some(&opaque));
        opaque.destroy();
        let objects = UnderlayObjects {
            surface,
            subsurface,
            viewport,
            buffer: None,
            retired: Vec::new(),
        };
        if !objects.alive() {
            objects.viewport.destroy();
            objects.subsurface.destroy();
            objects.surface.destroy();
            return false;
        }
        self.underlay = Some(objects);
        true
    }
    fn place_underlay_below_child(&mut self) {
        let Some(child) = self.surface().cloned() else { return };
        if let Some(underlay) = self.underlay() {
            underlay.subsurface.place_below(&child);
        }
    }
    fn underlay_buffer(&mut self, rgb: [u8; 3]) -> bool {
        if self.underlay().is_none() {
            return false;
        }
        let Some(buffer) = self.new_underlay_buffer(rgb) else {
            return false;
        };
        let Some(underlay) = self.underlay() else {
            buffer.destroy();
            return false;
        };
        underlay.surface.attach(Some(&buffer), 0, 0);
        underlay.surface.damage_buffer(0, 0, 1, 1);
        if let Some(old) = underlay.buffer.replace(buffer) {
            underlay.retired.push(old);
        }
        true
    }
    fn underlay_rect(&mut self, rect: Rect) {
        if let Some(underlay) = self.underlay() {
            underlay.subsurface.set_position(rect.0, rect.1);
            underlay.viewport.set_destination(rect.2, rect.3);
        }
    }
    fn underlay_attach_null(&mut self) {
        if let Some(underlay) = self.underlay() {
            underlay.surface.attach(None, 0, 0);
            if let Some(old) = underlay.buffer.take() {
                underlay.retired.push(old);
            }
        }
    }
    fn underlay_commit(&mut self) {
        if let Some(underlay) = self.underlay() {
            underlay.surface.commit();
        }
    }
    fn release_retired_underlay_buffers(&mut self) {
        if let Some(underlay) = self.underlay() {
            for buffer in underlay.retired.drain(..) {
                if buffer.is_alive() {
                    buffer.destroy();
                }
            }
        }
    }
    fn destroy_underlay(&mut self) {
        let Some(underlay) = self.underlay.take() else { return };
        if underlay.subsurface.is_alive() {
            underlay.subsurface.destroy();
        }
        if underlay.viewport.is_alive() {
            underlay.viewport.destroy();
        }
        if underlay.surface.is_alive() {
            underlay.surface.destroy();
        }
        for buffer in underlay.buffer.into_iter().chain(underlay.retired) {
            if buffer.is_alive() {
                buffer.destroy();
            }
        }
    }
}

pub(crate) struct Child<R: ChildRequests> {
    requests: R,
    parent: WlSurface,
    viewporter: bool,
    ready: bool,
    transaction: bool,
    attached: bool,
    placement: Option<Placement>,
    seq: u64,
    /// The underlay was made (a viewporter and a way to make a one-pixel buffer) and has not failed since.
    underlay: bool,
    /// Its colour, once the pane has named one.
    underlay_color: Option<[u8; 3]>,
    /// What the compositor has been sent for it: the rectangle and the colour, while a buffer is attached.
    underlay_shown: Option<(Rect, [u8; 3])>,
    /// Set when a buffer for it could not be made; the pane says so once.
    underlay_lost: bool,
    /// Set when the underlay was sent a change: being in sync mode, it waits for the parent's next commit, which the
    /// pane then asks GDK for.
    underlay_sent: bool,
    /// Set when that change replaced a buffer, which the next `after_paint` releases.
    underlay_retired: bool,
}

impl<R: ChildRequests> Child<R> {
    pub(crate) fn new(mut requests: R, parent: &WlSurface, chosen: &Chosen, viewporter: bool) -> Self {
        let mut ready = requests.create_surface() && requests.get_subsurface(parent);
        if ready {
            requests.place_below(parent);
            ready = requests.set_input_region_empty();
        }
        // No buffer transform: the frame is rendered top row first (`SurfaceOrigin::TopLeft`), so the viewport's source
        // rectangle names the same pixels whichever order a compositor applies a transform and a crop in.
        if ready {
            ready = !chosen.opaque_region_needed || requests.set_opaque_region_all();
        }
        if ready {
            ready = !viewporter || requests.get_viewport();
        }
        if ready {
            requests.set_desync();
        } else {
            requests.destroy_subsurface();
            requests.destroy_viewport();
            requests.destroy_surface();
        }
        // Only a viewport can stretch one pixel over the editor and past it; without one there is no underlay.
        let underlay = ready && viewporter && requests.create_underlay();
        if underlay {
            requests.place_underlay_below_child();
        }
        Self {
            requests,
            parent: parent.clone(),
            viewporter,
            ready,
            transaction: false,
            attached: false,
            placement: None,
            seq: 0,
            underlay,
            underlay_color: None,
            underlay_shown: None,
            underlay_lost: false,
            underlay_sent: false,
            underlay_retired: false,
        }
    }

    /// True once after the underlay was sent a change that waits for a parent commit.
    pub(crate) fn take_underlay_sent(&mut self) -> bool {
        std::mem::take(&mut self.underlay_sent)
    }

    /// The underlay exists (it may not be shown yet).
    pub(crate) fn has_underlay(&self) -> bool {
        self.underlay
    }

    /// True once after a buffer for the underlay could not be made and it was taken down.
    pub(crate) fn take_underlay_lost(&mut self) -> bool {
        std::mem::take(&mut self.underlay_lost)
    }

    /// Names the underlay's colour, the editor's background. Returns true when that sent the underlay a change,
    /// which, the underlay being in sync mode, the compositor applies at the parent's next commit: the caller makes
    /// sure GTK paints the parent.
    pub(crate) fn set_underlay_color(&mut self, rgb: [u8; 3]) -> bool {
        self.underlay_color = Some(rgb);
        self.show_underlay()
    }

    /// Brings the underlay to the current placement and colour: shown under an attached child with a placement that
    /// names its rectangle, taken away otherwise. Sends nothing when nothing changed.
    fn show_underlay(&mut self) -> bool {
        if !self.underlay || !self.is_ready() {
            return false;
        }
        let want = match (
            self.attached,
            self.placement.and_then(|p| p.underlay),
            self.underlay_color,
        ) {
            (true, Some(rect), Some(color)) => Some((rect, color)),
            _ => None,
        };
        if want == self.underlay_shown {
            return false;
        }
        match want {
            Some((rect, color)) => {
                if self.underlay_shown.map(|(shown, _)| shown) != Some(rect) {
                    self.requests.underlay_rect(rect);
                }
                if self.underlay_shown.map(|(_, shown)| shown) != Some(color) && !self.requests.underlay_buffer(color) {
                    self.requests.destroy_underlay();
                    self.underlay = false;
                    self.underlay_shown = None;
                    self.underlay_lost = true;
                    return false;
                }
                self.requests.underlay_commit();
            }
            None => {
                self.requests.underlay_attach_null();
                self.requests.underlay_commit();
            }
        }
        // A new buffer over a shown one, or a null attach, leaves the previous buffer for `after_paint` to release.
        self.underlay_retired |= match (self.underlay_shown, want) {
            (Some((_, shown)), Some((_, color))) => shown != color,
            (Some(_), None) => true,
            (None, _) => false,
        };
        self.underlay_shown = want;
        self.underlay_sent = true;
        true
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.ready && self.requests.is_ready()
    }

    /// A replacement child keeps the pane's sequence increasing so a late callback from its
    /// predecessor cannot complete a commit on this surface.
    pub(crate) fn continue_after(&mut self, seq: u64) {
        self.seq = self.seq.max(seq);
    }

    /// `buffer_size` is the buffer's allocation (the pool's `buffer_size`): a wl_buffer has no size query, and
    /// with the viewporter a cropped buffer can be larger than the content. The size bounds the damage and is checked
    /// against the source rectangle, which must lie inside the buffer or the compositor ends the connection.
    ///
    /// Only the opening and a new placement open a transaction; a different allocation behind the same placement
    /// leaves the surface's size and position unchanged, so it commits at once without waiting for the parent.
    /// Returns zero when no commit could be sent. The caller keeps the cover and falls back when not ready.
    pub(crate) fn present(
        &mut self,
        buffer: &WlBuffer,
        buffer_size: (i32, i32),
        placement: &Placement,
        pane: PaneId,
    ) -> u64 {
        let size = buffer_size;
        if !self.is_ready() || !self.requests.buffer_is_alive(buffer) || !self.valid_buffer(placement, size) {
            return 0;
        }
        let Some(seq) = self.seq.checked_add(1) else { return 0 };
        if !self.attached {
            self.begin_transaction();
        }
        self.move_to(placement);
        if self.viewporter {
            self.requests.set_source(0, 0, placement.content.0, placement.content.1);
            self.requests.set_destination(placement.logical.0, placement.logical.1);
        } else {
            self.requests.set_buffer_scale(placement.buffer_scale);
        }
        self.requests.attach(buffer);
        self.requests.damage_buffer(0, 0, size.0, size.1);
        self.requests.frame(FrameData { pane, seq });
        self.requests.feedback(FrameData { pane, seq });
        self.requests.commit();
        self.seq = seq;
        self.attached = true;
        self.show_underlay();
        seq
    }

    pub(crate) fn begin_transaction(&mut self) {
        if self.is_ready() && !self.transaction {
            self.requests.set_sync();
            self.transaction = true;
        }
    }

    /// True from `set_sync` until `after_paint`: the child's commits are cached until the parent commits, so the
    /// caller must make sure GTK paints and commits the parent in this frame.
    pub(crate) fn in_transaction(&self) -> bool {
        self.transaction
    }

    /// Called only after GTK has committed the parent with the matching layout and cover.
    pub(crate) fn after_paint(&mut self) {
        if self.transaction {
            if self.is_ready() {
                self.requests.set_desync();
            }
            self.transaction = false;
        }
        if self.underlay_retired {
            self.requests.release_retired_underlay_buffers();
            self.underlay_retired = false;
        }
    }

    pub(crate) fn move_to(&mut self, placement: &Placement) {
        if !self.is_ready() || !self.valid_placement(placement) || self.placement.as_ref() == Some(placement) {
            return;
        }
        // A change to the underlay alone (the window's edge moved, the editor did not) leaves the editor's surface
        // out of sync mode: its frames keep committing at once, and the underlay's change lands at the next parent
        // commit.
        if self.placement.map(Placement::without_underlay) != Some(placement.without_underlay()) {
            self.begin_transaction();
            self.requests.set_position(placement.position.0, placement.position.1);
        }
        self.placement = Some(*placement);
        self.show_underlay();
    }

    pub(crate) fn hide(&mut self) {
        if !self.is_ready() {
            return;
        }
        self.begin_transaction();
        self.requests.attach_null();
        self.requests.commit();
        self.attached = false;
        self.placement = None;
        self.show_underlay();
    }

    pub(crate) fn has_buffer(&self) -> bool {
        self.attached && self.is_ready()
    }
    pub(crate) fn parent_is(&self, parent: &WlSurface) -> bool {
        self.parent == *parent
    }

    pub(crate) fn destroy(mut self) {
        if self.underlay {
            self.requests.destroy_underlay();
        }
        if self.ready {
            self.requests.destroy_subsurface();
            self.requests.destroy_viewport();
            self.requests.destroy_surface();
        }
    }

    fn valid_placement(&self, placement: &Placement) -> bool {
        let p = placement;
        if p.logical.0 < 1 || p.logical.1 < 1 || p.content.0 < 1 || p.content.1 < 1 {
            return false;
        }
        if p.underlay.is_some_and(|(_, _, width, height)| width < 1 || height < 1) {
            return false;
        }
        if self.viewporter {
            // wl_fixed is signed 24.8; keep integer source extents exactly representable on the wire.
            const MAX_SOURCE: i32 = (1 << 23) - 1;
            p.viewport == Some(p.logical)
                && p.buffer_scale == 1
                && p.content.0 <= MAX_SOURCE
                && p.content.1 <= MAX_SOURCE
        } else {
            p.viewport.is_none()
                && p.buffer_scale >= 1
                && p.logical.0.checked_mul(p.buffer_scale) == Some(p.content.0)
                && p.logical.1.checked_mul(p.buffer_scale) == Some(p.content.1)
        }
    }
    fn valid_buffer(&self, placement: &Placement, size: (i32, i32)) -> bool {
        self.valid_placement(placement)
            && if self.viewporter {
                size.0 >= placement.content.0 && size.1 >= placement.content.1
            } else {
                size == placement.content
                    && size.0 % placement.buffer_scale == 0
                    && size.1 % placement.buffer_scale == 0
            }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, os::unix::net::UnixStream, rc::Rc};
    use wayland_client::{delegate_noop, Connection};

    #[derive(Debug, PartialEq, Eq)]
    enum Request {
        CreateSurface,
        GetSubsurface,
        PlaceBelow,
        EmptyInput,
        Opaque(i32, i32, i32, i32),
        Sync,
        Desync,
        Position(i32, i32),
        GetViewport,
        Source(i32, i32, i32, i32),
        Destination(i32, i32),
        BufferScale(i32),
        Attach,
        AttachNull,
        Damage(i32, i32, i32, i32),
        Frame(PaneId, u64),
        Feedback(PaneId, u64),
        Commit,
        ParentCommit,
        DestroySubsurface,
        DestroyViewport,
        DestroySurface,
        CreateUnderlay,
        UnderlayBelowChild,
        UnderlayBuffer([u8; 3]),
        UnderlayRect(Rect),
        UnderlayAttachNull,
        UnderlayCommit,
        ReleaseRetired,
        DestroyUnderlay,
    }
    use Request::*;

    #[derive(Default)]
    struct Recording {
        sent: Rc<RefCell<Vec<Request>>>,
        viewport: bool,
        presentation: bool,
        refuse: bool,
        /// The compositor can make one-pixel buffers: `create_underlay` succeeds. Off, it records nothing.
        underlay: bool,
        /// Making an underlay buffer fails.
        refuse_buffer: bool,
    }
    impl ChildRequests for Recording {
        fn create_surface(&mut self) -> bool {
            self.sent.borrow_mut().push(CreateSurface);
            !self.refuse
        }
        fn get_subsurface(&mut self, _: &WlSurface) -> bool {
            self.sent.borrow_mut().push(GetSubsurface);
            true
        }
        fn place_below(&mut self, _: &WlSurface) {
            self.sent.borrow_mut().push(PlaceBelow);
        }
        fn set_input_region_empty(&mut self) -> bool {
            self.sent.borrow_mut().push(EmptyInput);
            true
        }
        fn set_opaque_region_all(&mut self) -> bool {
            self.sent.borrow_mut().push(Opaque(0, 0, i32::MAX, i32::MAX));
            true
        }
        fn set_sync(&mut self) {
            self.sent.borrow_mut().push(Sync);
        }
        fn set_desync(&mut self) {
            self.sent.borrow_mut().push(Desync);
        }
        fn set_position(&mut self, x: i32, y: i32) {
            self.sent.borrow_mut().push(Position(x, y));
        }
        fn get_viewport(&mut self) -> bool {
            self.sent.borrow_mut().push(GetViewport);
            self.viewport = true;
            true
        }
        fn set_source(&mut self, x: i32, y: i32, w: i32, h: i32) {
            self.sent.borrow_mut().push(Source(x, y, w, h));
        }
        fn set_destination(&mut self, w: i32, h: i32) {
            self.sent.borrow_mut().push(Destination(w, h));
        }
        fn set_buffer_scale(&mut self, scale: i32) {
            self.sent.borrow_mut().push(BufferScale(scale));
        }
        fn attach(&mut self, _: &WlBuffer) {
            self.sent.borrow_mut().push(Attach);
        }
        fn attach_null(&mut self) {
            self.sent.borrow_mut().push(AttachNull);
        }
        fn damage_buffer(&mut self, x: i32, y: i32, w: i32, h: i32) {
            self.sent.borrow_mut().push(Damage(x, y, w, h));
        }
        fn frame(&mut self, data: FrameData) {
            self.sent.borrow_mut().push(Frame(data.pane, data.seq));
        }
        fn feedback(&mut self, data: FrameData) {
            if self.presentation {
                self.sent.borrow_mut().push(Feedback(data.pane, data.seq));
            }
        }
        fn commit(&mut self) {
            self.sent.borrow_mut().push(Commit);
        }
        fn destroy_subsurface(&mut self) {
            self.sent.borrow_mut().push(DestroySubsurface);
        }
        fn destroy_viewport(&mut self) {
            if self.viewport {
                self.sent.borrow_mut().push(DestroyViewport);
            }
        }
        fn destroy_surface(&mut self) {
            self.sent.borrow_mut().push(DestroySurface);
        }
        fn is_ready(&self) -> bool {
            !self.refuse
        }
        fn buffer_is_alive(&self, _: &WlBuffer) -> bool {
            true
        }
        fn create_underlay(&mut self) -> bool {
            if self.underlay {
                self.sent.borrow_mut().push(CreateUnderlay);
            }
            self.underlay
        }
        fn place_underlay_below_child(&mut self) {
            self.sent.borrow_mut().push(UnderlayBelowChild);
        }
        fn underlay_buffer(&mut self, rgb: [u8; 3]) -> bool {
            self.sent.borrow_mut().push(UnderlayBuffer(rgb));
            !self.refuse_buffer
        }
        fn underlay_rect(&mut self, rect: Rect) {
            self.sent.borrow_mut().push(UnderlayRect(rect));
        }
        fn underlay_attach_null(&mut self) {
            self.sent.borrow_mut().push(UnderlayAttachNull);
        }
        fn underlay_commit(&mut self) {
            self.sent.borrow_mut().push(UnderlayCommit);
        }
        fn release_retired_underlay_buffers(&mut self) {
            self.sent.borrow_mut().push(ReleaseRetired);
        }
        fn destroy_underlay(&mut self) {
            self.sent.borrow_mut().push(DestroyUnderlay);
        }
    }

    struct State;
    delegate_noop!(State: ignore WlSurface);
    delegate_noop!(State: ignore WlBuffer);
    delegate_noop!(State: ignore WlSubsurface);
    delegate_noop!(State: ignore WlRegion);
    delegate_noop!(State: ignore WpViewport);
    delegate_noop!(State: ignore WlShmPool);
    delegate_noop!(State: ignore WlShm);
    delegate_noop!(State: ignore WpSinglePixelBufferManagerV1);
    impl Dispatch<WlBuffer, Underlay> for State {
        fn event(
            _: &mut Self,
            _: &WlBuffer,
            _: <WlBuffer as Proxy>::Event,
            _: &Underlay,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    delegate_noop!(State: ignore wayland_client::protocol::wl_compositor::WlCompositor);
    delegate_noop!(State: ignore wayland_client::protocol::wl_subcompositor::WlSubcompositor);
    delegate_noop!(State: ignore wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1);
    delegate_noop!(State: ignore wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter);
    delegate_noop!(State: ignore wayland_protocols::wp::presentation_time::client::wp_presentation::WpPresentation);

    impl Dispatch<WlCallback, FrameData> for State {
        fn event(
            _: &mut Self,
            _: &WlCallback,
            _: <WlCallback as Proxy>::Event,
            _: &FrameData,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }
    impl Dispatch<WpPresentationFeedback, FrameData> for State {
        fn event(
            _: &mut Self,
            _: &WpPresentationFeedback,
            _: <WpPresentationFeedback as Proxy>::Event,
            _: &FrameData,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    struct Objects {
        conn: Connection,
        _peer: UnixStream,
        parent: WlSurface,
        other: WlSurface,
        buffer: WlBuffer,
    }
    impl Objects {
        fn new() -> Self {
            // A connected socket pair gives proxies distinct identities without a display or a socket bind.
            let (socket, peer) = UnixStream::pair().unwrap();
            let conn = Connection::from_socket(socket).unwrap();
            let queue = conn.new_event_queue::<State>();
            let qh = queue.handle();
            let registry = conn.display().get_registry(&qh, ());
            let compositor =
                registry.bind::<wayland_client::protocol::wl_compositor::WlCompositor, _, _>(1, 4, &qh, ());
            let parent = compositor.create_surface(&qh, ());
            let other = compositor.create_surface(&qh, ());
            let buffer = WlBuffer::inert(conn.backend().downgrade());
            Self {
                conn,
                _peer: peer,
                parent,
                other,
                buffer,
            }
        }

        fn globals(&self, viewport: bool) -> Globals {
            let qh = self.conn.new_event_queue::<State>().handle();
            let registry = self.conn.display().get_registry(&qh, ());
            Globals {
                compositor: registry.bind(1, 4, &qh, ()),
                subcompositor: registry.bind(2, 1, &qh, ()),
                dmabuf: registry.bind(3, 4, &qh, ()),
                viewporter: viewport.then(|| registry.bind(4, 1, &qh, ())),
                presentation: None,
                single_pixel: viewport.then(|| registry.bind(5, 1, &qh, ())),
                shm: None,
                feedback: super::super::types::DmabufFeedback {
                    main_device: 0,
                    formats: vec![],
                },
            }
        }

        fn take_wire(&self) -> Vec<u8> {
            use std::io::{ErrorKind, Read};
            self.conn.flush().unwrap();
            self._peer.set_nonblocking(true).unwrap();
            let mut bytes = Vec::new();
            assert_eq!(
                (&self._peer).read_to_end(&mut bytes).unwrap_err().kind(),
                ErrorKind::WouldBlock
            );
            bytes
        }
    }
    delegate_noop!(State: ignore wayland_client::protocol::wl_registry::WlRegistry);

    fn chosen(opaque: bool) -> Chosen {
        Chosen {
            wrap_fourcc: crate::dmabuf_target::DRM_FORMAT_ABGR8888,
            opaque_region_needed: opaque,
            modifiers: vec![],
        }
    }
    fn placement(viewport: bool) -> Placement {
        super::super::geometry::placement((7.0, 9.0), (0.0, 0.0), (801, 601), 1.5, 2, viewport).unwrap()
    }
    fn setup(objects: &Objects, viewport: bool, opaque: bool, presentation: bool) -> Child<Recording> {
        Child::new(
            Recording {
                presentation,
                ..Recording::default()
            },
            &objects.parent,
            &chosen(opaque),
            viewport,
        )
    }
    /// A child on a compositor that can make one-pixel buffers, with the editor's background named.
    fn setup_with_underlay(objects: &Objects, viewport: bool) -> Child<Recording> {
        let mut child = Child::new(
            Recording {
                underlay: true,
                ..Recording::default()
            },
            &objects.parent,
            &chosen(true),
            viewport,
        );
        child.set_underlay_color(BACKGROUND);
        child
    }
    const BACKGROUND: [u8; 3] = [30, 32, 48];
    fn underlay_placement(position: (i32, i32)) -> Placement {
        let mut p = placement(true);
        p.position = position;
        p.with_underlay((0.0, 0.0), (1600, 1000))
    }
    fn take(child: &Child<Recording>) -> Vec<Request> {
        std::mem::take(&mut *child.requests.sent.borrow_mut())
    }
    fn present(child: &mut Child<Recording>, buffer: &WlBuffer, p: &Placement, size: (i32, i32)) -> u64 {
        child.present(buffer, size, p, PaneId(3))
    }

    #[test]
    fn a_replacement_child_never_reuses_its_predecessors_callback_sequence() {
        let objects = Objects::new();
        let p = placement(true);
        let mut previous = setup(&objects, true, false, false);
        let seq = present(&mut previous, &objects.buffer, &p, (1280, 1024));
        previous.destroy();
        let mut next = setup(&objects, true, false, false);
        next.continue_after(seq);
        assert_eq!(present(&mut next, &objects.buffer, &p, (1280, 1024)), seq + 1);
    }

    #[test]
    fn creation_places_below_sets_no_transform_and_has_an_empty_input_region_and_desync() {
        let o = Objects::new();
        let child = setup(&o, true, false, true);
        assert!(child.is_ready());
        assert!(!child.has_buffer());
        assert_eq!(
            take(&child),
            vec![
                CreateSurface,
                GetSubsurface,
                PlaceBelow,
                EmptyInput,
                GetViewport,
                Desync
            ]
        );
    }
    #[test]
    fn present_with_a_viewport_sets_source_and_destination_not_buffer_scale() {
        let o = Objects::new();
        let mut child = setup(&o, true, false, true);
        take(&child);
        assert_eq!(present(&mut child, &o.buffer, &placement(true), (1280, 1024)), 1);
        assert!(child.has_buffer());
        assert_eq!(
            take(&child),
            vec![
                Sync,
                Position(7, 9),
                Source(0, 0, 1202, 902),
                Destination(801, 601),
                Attach,
                Damage(0, 0, 1280, 1024),
                Frame(PaneId(3), 1),
                Feedback(PaneId(3), 1),
                Commit
            ]
        );
    }
    #[test]
    fn present_without_a_viewport_sets_the_integer_buffer_scale() {
        let o = Objects::new();
        let mut child = setup(&o, false, false, false);
        take(&child);
        assert_eq!(present(&mut child, &o.buffer, &placement(false), (1602, 1202)), 1);
        assert_eq!(
            take(&child),
            vec![
                Sync,
                Position(7, 9),
                BufferScale(2),
                Attach,
                Damage(0, 0, 1602, 1202),
                Frame(PaneId(3), 1),
                Commit
            ]
        );
    }
    #[test]
    fn a_transaction_is_sync_then_position_then_commit_and_desync_after_paint() {
        let o = Objects::new();
        let mut child = setup(&o, true, false, false);
        present(&mut child, &o.buffer, &placement(true), (1280, 1024));
        child.after_paint();
        take(&child);
        let mut p = placement(true);
        p.position = (11, 12);
        p.content = (1203, 903);
        p.logical = (802, 602);
        p.viewport = Some(p.logical);
        child.begin_transaction();
        child.begin_transaction();
        child.move_to(&p);
        assert_eq!(present(&mut child, &o.buffer, &p, (1280, 1024)), 2);
        assert!(child.in_transaction());
        child.requests.sent.borrow_mut().push(ParentCommit);
        child.after_paint();
        assert!(!child.in_transaction());
        child.after_paint();
        assert_eq!(
            take(&child),
            vec![
                Sync,
                Position(11, 12),
                Source(0, 0, 1203, 903),
                Destination(802, 602),
                Attach,
                Damage(0, 0, 1280, 1024),
                Frame(PaneId(3), 2),
                Commit,
                ParentCommit,
                Desync
            ]
        );
    }
    #[test]
    fn a_size_change_at_the_same_position_opens_a_transaction() {
        let o = Objects::new();
        let mut child = setup(&o, true, false, false);
        let p = placement(true);
        present(&mut child, &o.buffer, &p, (1280, 1024));
        child.after_paint();
        take(&child);
        let mut next = p;
        next.logical = (802, 601);
        next.content = (1203, 902);
        next.viewport = Some(next.logical);
        present(&mut child, &o.buffer, &next, (1280, 1024));
        child.requests.sent.borrow_mut().push(ParentCommit);
        child.after_paint();
        assert_eq!(
            take(&child),
            vec![
                Sync,
                Position(7, 9),
                Source(0, 0, 1203, 902),
                Destination(802, 601),
                Attach,
                Damage(0, 0, 1280, 1024),
                Frame(PaneId(3), 2),
                Commit,
                ParentCommit,
                Desync
            ]
        );
    }
    #[test]
    fn unchanged_content_presents_without_waiting_for_the_parent() {
        let o = Objects::new();
        let mut child = setup(&o, true, false, false);
        let p = placement(true);
        present(&mut child, &o.buffer, &p, (1280, 1024));
        child.after_paint();
        take(&child);
        assert_eq!(present(&mut child, &o.buffer, &p, (1280, 1024)), 2);
        child.after_paint();
        assert_eq!(
            take(&child),
            vec![
                Source(0, 0, 1202, 902),
                Destination(801, 601),
                Attach,
                Damage(0, 0, 1280, 1024),
                Frame(PaneId(3), 2),
                Commit
            ]
        );
    }
    #[test]
    fn after_paint_without_a_transaction_sends_nothing() {
        let o = Objects::new();
        let mut child = setup(&o, true, false, false);
        take(&child);
        child.after_paint();
        assert_eq!(take(&child), vec![]);
    }
    #[test]
    fn hide_is_a_sync_null_attach_commit() {
        let o = Objects::new();
        let mut child = setup(&o, false, false, false);
        present(&mut child, &o.buffer, &placement(false), (1602, 1202));
        child.after_paint();
        take(&child);
        child.hide();
        assert!(!child.has_buffer());
        assert_eq!(take(&child), vec![Sync, AttachNull, Commit]);
    }
    #[test]
    fn a_zero_size_unmaps_instead_of_attaching() {
        let o = Objects::new();
        let mut child = setup(&o, true, false, false);
        take(&child);
        let p = super::super::geometry::placement((0.0, 0.0), (0.0, 0.0), (0, 601), 1.5, 2, true);
        assert!(p.is_none());
        if p.is_none() {
            child.hide();
        }
        assert_eq!(take(&child), vec![Sync, AttachNull, Commit]);
    }
    #[test]
    fn the_opaque_region_covers_any_size_when_needed() {
        let o = Objects::new();
        let mut child = setup(&o, true, true, false);
        assert_eq!(
            take(&child),
            vec![
                CreateSurface,
                GetSubsurface,
                PlaceBelow,
                EmptyInput,
                Opaque(0, 0, i32::MAX, i32::MAX),
                GetViewport,
                Desync
            ]
        );
        let p = placement(true);
        present(&mut child, &o.buffer, &p, (1280, 1024));
        child.after_paint();
        take(&child);
        let mut larger = p;
        larger.logical = (1800, 1200);
        larger.content = (2700, 1800);
        larger.viewport = Some(larger.logical);
        present(&mut child, &o.buffer, &larger, (2816, 1920));
        assert_eq!(
            take(&child),
            vec![
                Sync,
                Position(7, 9),
                Source(0, 0, 2700, 1800),
                Destination(1800, 1200),
                Attach,
                Damage(0, 0, 2816, 1920),
                Frame(PaneId(3), 2),
                Commit
            ]
        );
    }
    #[test]
    fn destroy_order_is_subsurface_viewport_surface() {
        let o = Objects::new();
        for viewport in [false, true] {
            let child = setup(&o, viewport, false, false);
            let sent = child.requests.sent.clone();
            take(&child);
            child.destroy();
            let expected = if viewport {
                vec![DestroySubsurface, DestroyViewport, DestroySurface]
            } else {
                vec![DestroySubsurface, DestroySurface]
            };
            assert_eq!(*sent.borrow(), expected);
        }
    }
    #[test]
    fn parent_is_compares_the_object() {
        let o = Objects::new();
        let child = setup(&o, false, false, false);
        assert!(child.parent_is(&o.parent.clone()));
        assert!(!child.parent_is(&o.other));
        assert!(o.conn.backend().info(o.parent.id()).is_ok());
    }
    #[test]
    fn invalid_sizes_send_no_attach_or_viewport_request() {
        let o = Objects::new();
        for viewport in [false, true] {
            let mut child = setup(&o, viewport, false, false);
            take(&child);
            let p = placement(viewport);
            for size in [(0, 0), (-1, 10), (1, 1), (1603, 1202)] {
                if viewport && size.0 >= p.content.0 && size.1 >= p.content.1 {
                    continue;
                }
                assert_eq!(present(&mut child, &o.buffer, &p, size), 0);
                assert_eq!(take(&child), vec![]);
            }
        }
    }
    #[test]
    fn a_failed_creation_stays_inert() {
        let o = Objects::new();
        let mut child = Child::new(
            Recording {
                refuse: true,
                ..Recording::default()
            },
            &o.parent,
            &chosen(false),
            false,
        );
        assert!(!child.is_ready());
        assert_eq!(take(&child), vec![CreateSurface, DestroySubsurface, DestroySurface]);
        child.begin_transaction();
        child.move_to(&placement(false));
        child.hide();
        child.after_paint();
        assert_eq!(present(&mut child, &o.buffer, &placement(false), (1602, 1202)), 0);
        assert_eq!(take(&child), vec![]);
    }

    #[test]
    fn invalid_placements_send_nothing() {
        let o = Objects::new();
        for viewport in [false, true] {
            let mut child = setup(&o, viewport, false, false);
            take(&child);
            let valid = placement(viewport);
            let mut invalid = vec![];
            let mut p = valid;
            p.logical.0 = 0;
            invalid.push(p);
            p = valid;
            p.content.1 = -1;
            invalid.push(p);
            p = valid;
            p.buffer_scale = 0;
            invalid.push(p);
            p = valid;
            p.viewport = if viewport { None } else { Some(p.logical) };
            invalid.push(p);
            p = valid;
            p.logical.0 = i32::MAX;
            invalid.push(p);
            p = valid;
            p.content.0 = i32::MAX;
            invalid.push(p);
            for p in invalid {
                child.move_to(&p);
                assert_eq!(present(&mut child, &o.buffer, &p, (i32::MAX, i32::MAX)), 0);
                assert_eq!(take(&child), vec![], "{p:?}");
            }
            assert_eq!(present(&mut child, &o.buffer, &valid, valid.content), 1);
        }
    }

    #[test]
    fn an_allocation_change_commits_at_once_and_a_reopen_is_a_transaction() {
        let o = Objects::new();
        let mut child = setup(&o, true, false, false);
        let p = placement(true);
        present(&mut child, &o.buffer, &p, (1280, 1024));
        child.after_paint();
        take(&child);
        // The pool may hand out a different allocation for the same content between frames; the surface keeps its
        // size and position, so the commit must not be held back waiting for a parent commit that may never come.
        for size in [(1408, 1152), (1280, 1024)] {
            assert!(present(&mut child, &o.buffer, &p, size) > 0);
            assert!(
                !child.in_transaction(),
                "an allocation change alone must not open a transaction"
            );
            child.after_paint();
            assert_eq!(
                take(&child),
                vec![
                    Source(0, 0, 1202, 902),
                    Destination(801, 601),
                    Attach,
                    Damage(0, 0, size.0, size.1),
                    Frame(PaneId(3), child.seq),
                    Commit
                ]
            );
        }
        child.hide();
        child.after_paint();
        take(&child);
        present(&mut child, &o.buffer, &p, (1280, 1024));
        child.requests.sent.borrow_mut().push(ParentCommit);
        child.after_paint();
        assert_eq!(
            take(&child),
            vec![
                Sync,
                Position(7, 9),
                Source(0, 0, 1202, 902),
                Destination(801, 601),
                Attach,
                Damage(0, 0, 1280, 1024),
                Frame(PaneId(3), 4),
                Commit,
                ParentCommit,
                Desync
            ]
        );
    }

    #[test]
    fn wayland_requests_create_one_subsurface_and_stop_sending_after_destruction() {
        let o = Objects::new();
        let globals = o.globals(true);
        let qh = o.conn.new_event_queue::<State>().handle();
        let mut requests = WaylandRequests::new(&globals, qh);
        assert!(requests.create_surface());
        assert!(requests.get_subsurface(&o.parent));
        assert!(requests.get_viewport());
        assert!(requests.is_ready());
        o.take_wire();
        assert!(!requests.create_surface());
        assert!(!requests.get_subsurface(&o.parent));
        assert!(!requests.get_viewport());
        assert!(o.take_wire().is_empty());
        requests.destroy_subsurface();
        requests.destroy_viewport();
        requests.destroy_surface();
        o.take_wire();
        requests.set_sync();
        requests.set_desync();
        requests.set_position(1, 1);
        requests.place_below(&o.parent);
        assert!(!requests.set_input_region_empty());
        assert!(!requests.set_opaque_region_all());
        requests.set_source(0, 0, 10, 10);
        requests.set_destination(10, 10);
        requests.set_buffer_scale(1);
        requests.attach(&o.buffer);
        requests.attach_null();
        requests.damage_buffer(0, 0, 10, 10);
        requests.frame(FrameData {
            pane: PaneId(1),
            seq: 1,
        });
        requests.feedback(FrameData {
            pane: PaneId(1),
            seq: 1,
        });
        requests.commit();
        requests.destroy_subsurface();
        requests.destroy_viewport();
        requests.destroy_surface();
        assert!(!requests.is_ready());
        assert!(o.take_wire().is_empty());
    }

    #[test]
    fn unavailable_viewporter_and_dead_parent_leave_no_child_objects() {
        let o = Objects::new();
        let globals = o.globals(false);
        let qh = o.conn.new_event_queue::<State>().handle();
        let requests = WaylandRequests::new(&globals, qh.clone());
        let child = Child::new(requests, &o.parent, &chosen(true), true);
        assert!(!child.is_ready());
        assert!(child.requests.surface.is_none());
        assert!(child.requests.subsurface.is_none());
        o.other.destroy();
        let requests = WaylandRequests::new(&globals, qh);
        let child = Child::new(requests, &o.other, &chosen(false), false);
        assert!(!child.is_ready());
        assert!(child.requests.surface.is_none());
        assert!(child.requests.subsurface.is_none());
    }

    #[test]
    fn the_underlay_is_made_below_the_child_and_left_in_sync_mode() {
        let o = Objects::new();
        let child = setup_with_underlay(&o, true);
        assert!(child.has_underlay());
        assert_eq!(
            take(&child),
            vec![
                CreateSurface,
                GetSubsurface,
                PlaceBelow,
                EmptyInput,
                Opaque(0, 0, i32::MAX, i32::MAX),
                GetViewport,
                Desync,
                CreateUnderlay,
                UnderlayBelowChild
            ]
        );
    }

    #[test]
    fn without_a_viewport_there_is_no_underlay() {
        let o = Objects::new();
        let mut child = setup_with_underlay(&o, false);
        assert!(!child.has_underlay());
        take(&child);
        present(
            &mut child,
            &o.buffer,
            &placement(false).with_underlay((0.0, 0.0), (1600, 1000)),
            (1602, 1202),
        );
        assert!(!take(&child).iter().any(|r| matches!(
            r,
            CreateUnderlay | UnderlayBuffer(_) | UnderlayRect(_) | UnderlayCommit | UnderlayAttachNull
        )));
    }

    #[test]
    fn the_opening_shows_the_underlay_before_the_parent_commits() {
        let o = Objects::new();
        let mut child = setup_with_underlay(&o, true);
        take(&child);
        let p = underlay_placement((7, 9));
        let r = super::super::geometry::UNDERLAY_REACH;
        present(&mut child, &o.buffer, &p, (1280, 1024));
        child.requests.sent.borrow_mut().push(ParentCommit);
        child.after_paint();
        assert_eq!(
            take(&child),
            vec![
                Sync,
                Position(7, 9),
                Source(0, 0, 1202, 902),
                Destination(801, 601),
                Attach,
                Damage(0, 0, 1280, 1024),
                Frame(PaneId(3), 1),
                Commit,
                UnderlayRect((0, 0, 7 + 801 + r, 9 + 601 + r)),
                UnderlayBuffer(BACKGROUND),
                UnderlayCommit,
                ParentCommit,
                Desync
            ]
        );
    }

    #[test]
    fn a_move_takes_the_underlay_along_in_the_same_transaction() {
        let o = Objects::new();
        let mut child = setup_with_underlay(&o, true);
        present(&mut child, &o.buffer, &underlay_placement((7, 9)), (1280, 1024));
        child.after_paint();
        take(&child);
        let moved = underlay_placement((100, 120));
        let r = super::super::geometry::UNDERLAY_REACH;
        child.move_to(&moved);
        child.requests.sent.borrow_mut().push(ParentCommit);
        child.after_paint();
        assert_eq!(
            take(&child),
            vec![
                Sync,
                Position(100, 120),
                UnderlayRect((100 - r, 120 - r, 801 + 2 * r, 601 + 2 * r)),
                UnderlayCommit,
                ParentCommit,
                Desync
            ]
        );
        // An unchanged frame sends the underlay nothing.
        present(&mut child, &o.buffer, &moved, (1280, 1024));
        assert!(!take(&child)
            .iter()
            .any(|r| matches!(r, UnderlayRect(_) | UnderlayBuffer(_) | UnderlayCommit)));
    }

    #[test]
    fn move_to_with_only_the_underlay_changed_sends_only_the_underlay_and_opens_no_transaction() {
        let o = Objects::new();
        let mut child = setup_with_underlay(&o, true);
        let p = underlay_placement((7, 9));
        present(&mut child, &o.buffer, &p, (1280, 1024));
        child.after_paint();
        take(&child);
        assert!(child.take_underlay_sent());
        // The window shrinks to the editor's right edge plus 10 px: the editor stays, the underlay is cut there.
        let narrower = p.without_underlay().with_underlay((0.0, 0.0), (7 + 801 + 10, 1000));
        assert_eq!(narrower.without_underlay(), p.without_underlay());
        child.move_to(&narrower);
        assert!(
            !child.in_transaction(),
            "the editor's own surface stays out of sync mode"
        );
        let r = super::super::geometry::UNDERLAY_REACH;
        assert_eq!(
            take(&child),
            vec![UnderlayRect((0, 0, 7 + 801 + 10, 9 + 601 + r)), UnderlayCommit]
        );
        assert!(child.take_underlay_sent(), "the pane asks GDK for a parent commit");
        assert!(!child.take_underlay_sent());
        child.move_to(&narrower);
        assert_eq!(take(&child), vec![]);
        assert!(!child.take_underlay_sent());
    }

    #[test]
    fn a_new_background_is_a_new_buffer_and_the_same_one_sends_nothing() {
        let o = Objects::new();
        let mut child = setup_with_underlay(&o, true);
        present(&mut child, &o.buffer, &underlay_placement((7, 9)), (1280, 1024));
        child.after_paint();
        take(&child);
        assert!(!child.set_underlay_color(BACKGROUND));
        assert_eq!(take(&child), vec![]);
        assert!(child.set_underlay_color([250, 244, 237]));
        assert_eq!(take(&child), vec![UnderlayBuffer([250, 244, 237]), UnderlayCommit]);
        // The replaced buffer goes once the parent has painted (and so committed the cached underlay), and only once.
        child.after_paint();
        assert_eq!(take(&child), vec![ReleaseRetired]);
        child.after_paint();
        assert_eq!(take(&child), vec![]);
    }

    #[test]
    fn a_colour_named_after_the_opening_shows_the_underlay_then() {
        let o = Objects::new();
        let mut child = Child::new(
            Recording {
                underlay: true,
                ..Recording::default()
            },
            &o.parent,
            &chosen(true),
            true,
        );
        present(&mut child, &o.buffer, &underlay_placement((7, 9)), (1280, 1024));
        assert!(!take(&child)
            .iter()
            .any(|r| matches!(r, UnderlayBuffer(_) | UnderlayCommit)));
        assert!(child.set_underlay_color(BACKGROUND));
        let r = super::super::geometry::UNDERLAY_REACH;
        assert_eq!(
            take(&child),
            vec![
                UnderlayRect((0, 0, 7 + 801 + r, 9 + 601 + r)),
                UnderlayBuffer(BACKGROUND),
                UnderlayCommit
            ]
        );
    }

    #[test]
    fn hiding_takes_the_underlay_away_and_the_reopening_brings_it_back() {
        let o = Objects::new();
        let mut child = setup_with_underlay(&o, true);
        let p = underlay_placement((7, 9));
        present(&mut child, &o.buffer, &p, (1280, 1024));
        child.after_paint();
        take(&child);
        child.hide();
        assert_eq!(
            take(&child),
            vec![Sync, AttachNull, Commit, UnderlayAttachNull, UnderlayCommit]
        );
        child.after_paint();
        take(&child);
        present(&mut child, &o.buffer, &p, (1280, 1024));
        let sent = take(&child);
        let r = super::super::geometry::UNDERLAY_REACH;
        assert_eq!(
            sent[sent.len() - 3..],
            [
                UnderlayRect((0, 0, 7 + 801 + r, 9 + 601 + r)),
                UnderlayBuffer(BACKGROUND),
                UnderlayCommit
            ]
        );
    }

    #[test]
    fn a_buffer_that_cannot_be_made_takes_the_underlay_down_once() {
        let o = Objects::new();
        let mut child = Child::new(
            Recording {
                underlay: true,
                refuse_buffer: true,
                ..Recording::default()
            },
            &o.parent,
            &chosen(true),
            true,
        );
        child.set_underlay_color(BACKGROUND);
        present(&mut child, &o.buffer, &underlay_placement((7, 9)), (1280, 1024));
        let sent = take(&child);
        assert_eq!(sent[sent.len() - 2..], [UnderlayBuffer(BACKGROUND), DestroyUnderlay]);
        assert!(!child.has_underlay());
        assert!(child.take_underlay_lost());
        assert!(!child.take_underlay_lost());
        assert!(child.is_ready(), "the editor itself goes on");
        child.set_underlay_color([1, 2, 3]);
        child.destroy();
    }

    #[test]
    fn destroy_takes_the_underlay_down_before_the_child() {
        let o = Objects::new();
        let child = setup_with_underlay(&o, true);
        let sent = child.requests.sent.clone();
        take(&child);
        child.destroy();
        assert_eq!(
            *sent.borrow(),
            vec![DestroyUnderlay, DestroySubsurface, DestroyViewport, DestroySurface]
        );
    }

    #[test]
    fn wayland_requests_make_the_underlay_below_the_child_and_free_replaced_buffers() {
        let o = Objects::new();
        let globals = o.globals(true);
        let qh = o.conn.new_event_queue::<State>().handle();
        let mut requests = WaylandRequests::new(&globals, qh);
        assert!(requests.create_surface());
        assert!(requests.get_subsurface(&o.parent));
        assert!(requests.create_underlay());
        assert!(!requests.create_underlay(), "one underlay per child");
        requests.place_underlay_below_child();
        assert!(requests.underlay_buffer([1, 2, 3]));
        let first = requests.underlay.as_ref().unwrap().buffer.clone().unwrap();
        assert!(requests.underlay_buffer([4, 5, 6]));
        assert!(
            first.is_alive(),
            "a replaced buffer lives until the commit that stops using it"
        );
        requests.underlay_rect((1, 2, 3, 4));
        requests.underlay_commit();
        assert!(first.is_alive(), "the commit is cached until the parent commits");
        requests.release_retired_underlay_buffers();
        assert!(!first.is_alive());
        let second = requests.underlay.as_ref().unwrap().buffer.clone().unwrap();
        requests.underlay_attach_null();
        requests.underlay_commit();
        requests.release_retired_underlay_buffers();
        assert!(!second.is_alive());
        o.take_wire();
        requests.destroy_underlay();
        assert!(!o.take_wire().is_empty());
        requests.underlay_rect((1, 2, 3, 4));
        assert!(!requests.underlay_buffer([1, 2, 3]));
        requests.underlay_commit();
        assert!(o.take_wire().is_empty());
    }

    #[test]
    fn no_viewporter_or_no_pixel_source_makes_no_underlay() {
        let o = Objects::new();
        let qh = o.conn.new_event_queue::<State>().handle();
        let mut requests = WaylandRequests::new(&o.globals(false), qh.clone());
        assert!(requests.create_surface());
        assert!(requests.get_subsurface(&o.parent));
        o.take_wire();
        assert!(!requests.create_underlay());
        let mut globals = o.globals(true);
        globals.single_pixel = None;
        let mut requests = WaylandRequests::new(&globals, qh);
        assert!(requests.create_surface());
        assert!(requests.get_subsurface(&o.parent));
        o.take_wire();
        assert!(!requests.create_underlay());
        assert!(o.take_wire().is_empty());
    }
}
