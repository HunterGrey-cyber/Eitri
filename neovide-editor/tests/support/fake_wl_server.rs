//! A Wayland server stand-in serving one end of a socket pair from its own thread. It speaks the
//! wire format (an 8-byte header: object id, then `(size << 16) | opcode`) for `wl_display.sync`
//! (`wl_callback.done`, then `wl_display.delete_id`), `wl_display.get_registry` (a scripted list of
//! globals), `wl_registry.bind` (recorded) and `zwp_linux_dmabuf_v1.get_default_feedback` (a
//! scripted feedback, its format table passed as a memfd), and ignores every other request.
//!
//! [`FakeServer::delay_next`] holds the next reply back for a while; [`FakeServer::hold_next`] keeps
//! the next sync's callback pending until [`FakeServer::release`] names it.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, IntoRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A global the registry announces; its name is its position in the list, from 1.
pub struct Global {
    pub interface: &'static str,
    pub version: u32,
}

/// The dmabuf feedback sent for `get_default_feedback`: the format table, the main device, and
/// tranches of (target device, indices into the table).
pub struct Feedback {
    pub table: Vec<(u32, u64)>,
    pub main_device: u64,
    pub tranches: Vec<(u64, Vec<u16>)>,
}

#[derive(Default)]
struct Control {
    delay_next: Option<Duration>,
    hold_next: bool,
    /// Held callbacks and the serial their `done` will carry.
    held: HashMap<u32, u32>,
    release: Vec<u32>,
    /// Every bind, as (interface, version), in order.
    bound: Vec<(String, u32)>,
    /// How many requests have been read.
    requests: u64,
}

pub struct FakeServer {
    control: Arc<Mutex<Control>>,
    client: Option<UnixStream>,
}

impl FakeServer {
    pub fn start(globals: Vec<Global>, feedback: Option<Feedback>) -> Self {
        let (client, server) = UnixStream::pair().expect("a socket pair for the fake server");
        let control = Arc::new(Mutex::new(Control::default()));
        let serving = Arc::clone(&control);
        std::thread::Builder::new()
            .name("fake-wl-server".to_string())
            .spawn(move || serve(server, globals, feedback, serving))
            .expect("the fake server's thread starts");
        Self {
            control,
            client: Some(client),
        }
    }

    /// The client's end, for `wl_display_connect_to_fd`, which takes ownership of it.
    pub fn take_client_fd(&mut self) -> RawFd {
        self.client.take().expect("the client end is taken once").into_raw_fd()
    }

    fn control(&self) -> std::sync::MutexGuard<'_, Control> {
        self.control.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Holds the reply to the next request read for `ms` milliseconds (and every reply after it).
    pub fn delay_next(&self, ms: u64) {
        self.control().delay_next = Some(Duration::from_millis(ms));
    }

    /// Keeps the next `wl_display.sync`'s callback pending until it is released.
    pub fn hold_next(&self) {
        self.control().hold_next = true;
    }

    pub fn held(&self) -> Vec<u32> {
        self.control().held.keys().copied().collect()
    }

    /// Sends the held callback's `done` and `delete_id`.
    pub fn release(&self, callback_id: u32) {
        self.control().release.push(callback_id);
    }

    pub fn bound(&self) -> Vec<(String, u32)> {
        self.control().bound.clone()
    }

    pub fn requests(&self) -> u64 {
        self.control().requests
    }
}

const DISPLAY_ID: u32 = 1;

struct Message {
    object: u32,
    opcode: u16,
    body: Vec<u8>,
}

fn word(body: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes([body[at], body[at + 1], body[at + 2], body[at + 3]])
}

/// A string argument at `at`: its length (with the NUL), its bytes, padding to 4; returns the text
/// and the offset after it.
fn string(body: &[u8], at: usize) -> (String, usize) {
    let len = word(body, at) as usize;
    let text = String::from_utf8_lossy(&body[at + 4..at + 4 + len.saturating_sub(1)]).into_owned();
    (text, at + 4 + len.div_ceil(4) * 4)
}

struct Writer {
    out: Vec<u8>,
}

impl Writer {
    fn message(&mut self, object: u32, opcode: u16, body: &[u8]) {
        let size = 8 + body.len() as u32;
        self.out.extend_from_slice(&object.to_ne_bytes());
        self.out
            .extend_from_slice(&((size << 16) | u32::from(opcode)).to_ne_bytes());
        self.out.extend_from_slice(body);
    }
}

fn put_u32(body: &mut Vec<u8>, value: u32) {
    body.extend_from_slice(&value.to_ne_bytes());
}

fn put_string(body: &mut Vec<u8>, text: &str) {
    let len = text.len() + 1;
    put_u32(body, len as u32);
    body.extend_from_slice(text.as_bytes());
    body.push(0);
    body.resize(body.len() + (len.div_ceil(4) * 4 - len), 0);
}

fn put_array(body: &mut Vec<u8>, bytes: &[u8]) {
    put_u32(body, bytes.len() as u32);
    body.extend_from_slice(bytes);
    body.resize(body.len() + (bytes.len().div_ceil(4) * 4 - bytes.len()), 0);
}

fn done_and_delete(writer: &mut Writer, callback: u32, serial: u32) {
    writer.message(callback, 0, &serial.to_ne_bytes());
    writer.message(DISPLAY_ID, 1, &callback.to_ne_bytes());
}

/// Sends `bytes` with `fd` attached as SCM_RIGHTS ancillary data.
fn send_with_fd(stream: &UnixStream, bytes: &[u8], fd: RawFd) -> std::io::Result<()> {
    let mut iov = libc::iovec {
        iov_base: bytes.as_ptr() as *mut libc::c_void,
        iov_len: bytes.len(),
    };
    // SAFETY: CMSG_SPACE is a pure size computation.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u64; space.div_ceil(8)];
    // SAFETY: an all-zero msghdr is valid; every pointer set below outlives the sendmsg call.
    let mut header: libc::msghdr = unsafe { std::mem::zeroed() };
    header.msg_iov = &mut iov;
    header.msg_iovlen = 1;
    header.msg_control = control.as_mut_ptr().cast();
    header.msg_controllen = space as _;
    // SAFETY: the control buffer is `space` bytes, aligned for cmsghdr, enough for one fd.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&header);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>(), fd);
    }
    // SAFETY: the header points at live buffers for the duration of the call.
    let sent = unsafe { libc::sendmsg(stream.as_raw_fd(), &header, libc::MSG_NOSIGNAL) };
    if sent == bytes.len() as isize {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn memfd_with(bytes: &[u8]) -> std::fs::File {
    // SAFETY: a valid NUL-terminated name; a non-negative result is a new descriptor owned here.
    let fd = unsafe { libc::memfd_create(c"fake-format-table".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0, "memfd_create failed");
    // SAFETY: `fd` was just created and is owned by the File alone.
    let mut file = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(fd) };
    file.write_all(bytes).expect("the format table is written");
    file
}

fn send_feedback(stream: &mut UnixStream, writer: &mut Writer, object: u32, feedback: &Feedback) -> bool {
    // Whatever is queued goes first, so the format table arrives in order.
    if stream.write_all(&std::mem::take(&mut writer.out)).is_err() {
        return false;
    }
    let mut table = Vec::new();
    for (format, modifier) in &feedback.table {
        table.extend_from_slice(&format.to_ne_bytes());
        table.extend_from_slice(&[0; 4]);
        table.extend_from_slice(&modifier.to_ne_bytes());
    }
    let file = memfd_with(&table);
    let mut format_table = Writer { out: Vec::new() };
    format_table.message(object, 1, &(table.len() as u32).to_ne_bytes());
    if send_with_fd(stream, &format_table.out, file.as_raw_fd()).is_err() {
        return false;
    }
    let mut body = Vec::new();
    put_array(&mut body, &feedback.main_device.to_ne_bytes());
    writer.message(object, 2, &body);
    for (device, indices) in &feedback.tranches {
        let mut body = Vec::new();
        put_array(&mut body, &device.to_ne_bytes());
        writer.message(object, 4, &body);
        let mut body = Vec::new();
        let bytes: Vec<u8> = indices.iter().flat_map(|index| index.to_ne_bytes()).collect();
        put_array(&mut body, &bytes);
        writer.message(object, 5, &body);
        writer.message(object, 6, &0u32.to_ne_bytes());
        writer.message(object, 3, &[]);
    }
    writer.message(object, 0, &[]);
    true
}

fn serve(mut stream: UnixStream, globals: Vec<Global>, feedback: Option<Feedback>, control: Arc<Mutex<Control>>) {
    let lock = || control.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if stream.set_read_timeout(Some(Duration::from_millis(2))).is_err() {
        return;
    }
    let mut input = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut serial = 1u32;
    let mut registries = Vec::new();
    let mut objects: HashMap<u32, String> = HashMap::new();
    loop {
        let mut writer = Writer { out: Vec::new() };
        {
            let mut control = lock();
            let release = std::mem::take(&mut control.release);
            for id in release {
                if let Some(serial) = control.held.remove(&id) {
                    done_and_delete(&mut writer, id, serial);
                }
            }
        }
        match stream.read(&mut chunk) {
            Ok(0) => return,
            Ok(read) => input.extend_from_slice(&chunk[..read]),
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) => {}
            Err(_) => return,
        }
        let mut messages = Vec::new();
        while input.len() >= 8 {
            let size = (word(&input, 4) >> 16) as usize;
            if size < 8 || input.len() < size {
                break;
            }
            let opcode = (word(&input, 4) & 0xffff) as u16;
            messages.push(Message {
                object: word(&input, 0),
                opcode,
                body: input[8..size].to_vec(),
            });
            input.drain(..size);
        }
        for message in messages {
            let delay = {
                let mut control = lock();
                control.requests += 1;
                control.delay_next.take()
            };
            if let Some(delay) = delay {
                std::thread::sleep(delay);
            }
            match (message.object, message.opcode) {
                (DISPLAY_ID, 0) => {
                    let callback = word(&message.body, 0);
                    serial += 1;
                    let mut control = lock();
                    if std::mem::take(&mut control.hold_next) {
                        control.held.insert(callback, serial);
                    } else {
                        done_and_delete(&mut writer, callback, serial);
                    }
                }
                (DISPLAY_ID, 1) => {
                    let registry = word(&message.body, 0);
                    registries.push(registry);
                    for (index, global) in globals.iter().enumerate() {
                        let mut body = Vec::new();
                        put_u32(&mut body, index as u32 + 1);
                        put_string(&mut body, global.interface);
                        put_u32(&mut body, global.version);
                        writer.message(registry, 0, &body);
                    }
                }
                (object, 0) if registries.contains(&object) => {
                    let (interface, at) = string(&message.body, 4);
                    let version = word(&message.body, at);
                    let id = word(&message.body, at + 4);
                    lock().bound.push((interface.clone(), version));
                    objects.insert(id, interface);
                }
                (object, 2) if objects.get(&object).map(String::as_str) == Some("zwp_linux_dmabuf_v1") => {
                    let id = word(&message.body, 0);
                    if let Some(feedback) = &feedback {
                        if !send_feedback(&mut stream, &mut writer, id, feedback) {
                            return;
                        }
                    }
                }
                _ => {}
            }
        }
        if !writer.out.is_empty() && stream.write_all(&writer.out).is_err() {
            return;
        }
    }
}
