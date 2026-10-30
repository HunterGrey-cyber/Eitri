//! Child-surface requests stay on the GTK thread, with layout changes cached until the parent paints.

use wayland_client::{
    protocol::{
        wl_buffer::WlBuffer, wl_callback::WlCallback, wl_compositor::WlCompositor, wl_output::Transform,
        wl_region::WlRegion, wl_subcompositor::WlSubcompositor, wl_subsurface::WlSubsurface, wl_surface::WlSurface,
    },
    Dispatch, Proxy, QueueHandle,
};
use wayland_protocols::wp::{
    presentation_time::client::{wp_presentation::WpPresentation, wp_presentation_feedback::WpPresentationFeedback},
    viewporter::client::{wp_viewport::WpViewport, wp_viewporter::WpViewporter},
};

use super::{
    geometry::Placement,
    types::{FrameData, Globals, PaneId},
};
use crate::dmabuf_target::Chosen;

/// Owns one child's protocol objects. Constructors report failure instead of using an inert proxy.
/// All other methods send only when their target and referenced objects remain alive.
pub(crate) trait ChildRequests {
    fn create_surface(&mut self) -> bool;
    fn get_subsurface(&mut self, parent: &WlSurface) -> bool;
    fn place_below(&mut self, parent: &WlSurface);
    fn set_input_region_empty(&mut self) -> bool;
    fn set_opaque_region_all(&mut self) -> bool;
    fn set_buffer_transform_flipped(&mut self);
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
}

pub(crate) struct WaylandRequests<D> {
    compositor: WlCompositor,
    subcompositor: WlSubcompositor,
    viewporter: Option<WpViewporter>,
    presentation: Option<WpPresentation>,
    qh: QueueHandle<D>,
    surface: Option<WlSurface>,
    subsurface: Option<WlSubsurface>,
    viewport: Option<WpViewport>,
    parent: Option<WlSurface>,
}

impl<D> WaylandRequests<D> {
    pub(crate) fn new(globals: &Globals, qh: QueueHandle<D>) -> Self {
        Self {
            compositor: globals.compositor.clone(),
            subcompositor: globals.subcompositor.clone(),
            viewporter: globals.viewporter.clone(),
            presentation: globals.presentation.clone(),
            qh,
            surface: None,
            subsurface: None,
            viewport: None,
            parent: None,
        }
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
    fn set_buffer_transform_flipped(&mut self) {
        // GL stores the image bottom row first. This flip leaves viewport coordinates at the content's top left.
        if let Some(surface) = self.surface() {
            surface.set_buffer_transform(Transform::Flipped180);
        }
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
}

impl<R: ChildRequests> Child<R> {
    pub(crate) fn new(mut requests: R, parent: &WlSurface, chosen: &Chosen, viewporter: bool) -> Self {
        let mut ready = requests.create_surface() && requests.get_subsurface(parent);
        if ready {
            requests.place_below(parent);
            ready = requests.set_input_region_empty();
        }
        if ready {
            requests.set_buffer_transform_flipped();
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
        Self {
            requests,
            parent: parent.clone(),
            viewporter,
            ready,
            transaction: false,
            attached: false,
            placement: None,
            seq: 0,
        }
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
    }

    pub(crate) fn move_to(&mut self, placement: &Placement) {
        if !self.is_ready() || !self.valid_placement(placement) || self.placement.as_ref() == Some(placement) {
            return;
        }
        self.begin_transaction();
        self.requests.set_position(placement.position.0, placement.position.1);
        self.placement = Some(*placement);
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
    }

    pub(crate) fn has_buffer(&self) -> bool {
        self.attached && self.is_ready()
    }
    pub(crate) fn parent_is(&self, parent: &WlSurface) -> bool {
        self.parent == *parent
    }

    pub(crate) fn destroy(mut self) {
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
        Flip,
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
    }
    use Request::*;

    #[derive(Default)]
    struct Recording {
        sent: Rc<RefCell<Vec<Request>>>,
        viewport: bool,
        presentation: bool,
        refuse: bool,
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
        fn set_buffer_transform_flipped(&mut self) {
            self.sent.borrow_mut().push(Flip);
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
    }

    struct State;
    delegate_noop!(State: ignore WlSurface);
    delegate_noop!(State: ignore WlBuffer);
    delegate_noop!(State: ignore WlSubsurface);
    delegate_noop!(State: ignore WlRegion);
    delegate_noop!(State: ignore WpViewport);
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
    fn creation_places_below_flips_and_has_an_empty_input_region_and_desync() {
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
                Flip,
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
                Flip,
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
        requests.set_buffer_transform_flipped();
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
}
