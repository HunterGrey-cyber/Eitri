//! The types the subsurface files share. They are `pub` inside this private module so that
//! `subsurface_testing` can re-export them to the crate's integration tests; nothing else outside
//! the crate reaches them.

use std::time::Instant;

use wayland_client::protocol::{
    wl_buffer::WlBuffer, wl_compositor::WlCompositor, wl_shm::WlShm, wl_subcompositor::WlSubcompositor,
};
use wayland_protocols::wp::{
    linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
    presentation_time::client::wp_presentation::WpPresentation,
    single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1,
    viewporter::client::wp_viewporter::WpViewporter,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PaneId(pub(crate) u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BufferKey {
    pub pane: PaneId,
    pub generation: u64,
    pub id: u64,
}

pub struct FrameData {
    pub pane: PaneId,
    pub seq: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DmabufFeedback {
    pub main_device: u64,
    pub formats: Vec<(u32, u64)>,
}

pub struct Globals {
    pub compositor: WlCompositor,
    pub subcompositor: WlSubcompositor,
    pub dmabuf: ZwpLinuxDmabufV1,
    pub viewporter: Option<WpViewporter>,
    pub presentation: Option<WpPresentation>,
    /// The underlay's colour: a single-pixel buffer, else a 1x1 shm buffer (both stretched by the viewport).
    pub single_pixel: Option<WpSinglePixelBufferManagerV1>,
    pub shm: Option<WlShm>,
    pub feedback: DmabufFeedback,
}

pub enum PresentEvent {
    Ready(Box<Globals>),
    Unavailable(String),
    FrameDone {
        pane: PaneId,
        seq: u64,
        at: Instant,
    },
    Presented {
        pane: PaneId,
        seq: u64,
        presented_ns: u64,
        refresh_ns: u32,
    },
    Discarded {
        pane: PaneId,
        seq: u64,
    },
    BufferCreated {
        key: BufferKey,
        buffer: WlBuffer,
    },
    BufferFailed {
        key: BufferKey,
    },
    BufferReleased {
        key: BufferKey,
    },
    FeedbackChanged(DmabufFeedback),
    ThreadFailed(String),
}

pub trait PresentSink {
    fn event(&self, event: PresentEvent);
    fn globals_ready(&self);
}
