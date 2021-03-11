use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::io::Write;
use std::mem::forget;
use std::os::raw::{c_int, c_uint, c_void};
use std::pin::Pin;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use bytes::{Buf, Bytes};
use chunked_bytes::ChunkedBytes;
use evdi_sys::*;
use filedescriptor::{poll, pollfd, POLLIN};

use crate::device_config::DeviceConfig;

/// Represents an EVDI handle that is connected and ready.
///
/// Automatically disconnected on drop.
#[derive(Debug)]
pub struct Handle {
    handle: evdi_handle,
    device_config: DeviceConfig,
    registered_buffers: HashMap<BufferID, (Sender<()>, Receiver<()>)>,
    mode: Receiver<evdi_mode>,
    mode_sender: Sender<evdi_mode>,
}

impl Handle {
    /// Ask the kernel module to update a buffer with the current display pixels.
    ///
    /// Blocks until the update is complete.
    ///
    /// ```
    /// # use evdi::{device::Device, device_config::DeviceConfig, handle::{Buffer, BufferID}};
    /// # use std::time::Duration;
    /// # let timeout = Duration::from_secs(1);
    /// # let mut handle = Device::get().unwrap().open().connect(&DeviceConfig::sample(), timeout);
    /// # handle.request_events();
    /// # let mode = handle.receive_mode(timeout).unwrap();
    /// let mut buf = Buffer::new(BufferID::new(1), &mode);
    /// handle.request_update(&mut buf, timeout).unwrap();
    /// assert!(buf.dirty_rects().len() > 0);
    /// ```
    pub fn request_update(&mut self, buffer: &mut Buffer, timeout: Duration) -> Result<(), RecvTimeoutError> {
        // NOTE: We need to take &mut self to ensure we can't be called concurrently. This is
        //  required because evdi_grab_pixels grabs from the most recently updated buffer.
        //
        //  We need to take &mut buffer to ensure the buffer can't be read from while it's being
        //  updated.
        let user_data_sys = self as *const Handle;
        let handle_sys = self.handle;
        let id_sys = buffer.id.0;

        let update_ready = self.ensure_registered_and_get_update_ready_receiver(&buffer);
        buffer.mark_updated();


        let ready = unsafe { evdi_request_update(handle_sys, id_sys) };
        if !ready {
            Self::request_events_sys(user_data_sys, handle_sys);
            update_ready.recv_timeout(timeout)?;
        }

        unsafe {
            evdi_grab_pixels(
                self.handle as *mut evdi_device_context,
                buffer.rects.as_mut_ptr(),
                &mut buffer.num_rects,
            )
        }

        Ok(())
    }

    /// Unregister a buffer from the handle.
    ///
    /// If you provide this buffer to [`Handle::request_update`] in the future the buffer will be
    /// registered again.
    ///
    /// If the buffer is not registered this has no effect.
    pub fn unregister_buffer(&mut self, id: BufferID) {
        let was_registered = self.registered_buffers.remove(&id).is_some();
        if was_registered {
            unsafe { evdi_unregister_buffer(self.handle, id.0) };
        }
    }

    /// Idempotently ensure a buffer is registered.
    fn ensure_registered_and_get_update_ready_receiver(&mut self, buffer: &Buffer) -> &Receiver<()> {
        let handle_sys = self.handle;
        let (_, recv) = self.registered_buffers
            .entry(buffer.id.clone())
            .or_insert_with(|| {
                unsafe { evdi_register_buffer(handle_sys, buffer.sys()) };
                channel()
            });
        recv
    }

    pub fn enable_cursor_events(&self, enable: bool) {
        unsafe { evdi_enable_cursor_events(self.handle, enable); }
    }

    /// Ask the kernel module to send us some events.
    ///
    /// I think this blocks, dispatches a certain number of events, and the then returns, so callers
    /// should call in a loop. However, the docs aren't clear.
    /// See <https://github.com/DisplayLink/evdi/issues/265>
    pub fn request_events(&self) {
        Self::request_events_sys(self as *const Handle, self.handle);
    }

    fn request_events_sys(user_data: *const Handle, handle: evdi_handle) {
        let mut ctx = evdi_event_context {
            dpms_handler: None,
            mode_changed_handler: Some(Self::mode_changed_handler_caller),
            update_ready_handler: Some(Self::update_ready_handler_caller),
            crtc_state_handler: None,
            cursor_set_handler: None,
            cursor_move_handler: None,
            ddcci_data_handler: None,
            // Safety: We cast to a mut pointer, but we never cast back to a mut reference
            user_data: user_data as *mut c_void,
        };
        unsafe { evdi_handle_events(handle, &mut ctx) };
    }

    /// Blocks until a mode event is received.
    ///
    /// A mode event will not be received unless [`Self::request_events`] is called.
    ///
    /// ```
    /// # use evdi::device::Device;
    /// # use evdi::device_config::DeviceConfig;
    /// # use std::time::Duration;
    /// # let device: Device = Device::get().unwrap();
    /// # let timeout = Duration::from_secs(1);
    /// # let mut handle = device.open().connect(&DeviceConfig::sample(), timeout);
    /// handle.request_events();
    ///
    /// let mode = handle.receive_mode(timeout).unwrap();
    /// ```
    pub fn receive_mode(&self, timeout: Duration) -> Result<evdi_mode, RecvTimeoutError> {
        self.mode.recv_timeout(timeout)
    }

    pub fn disconnect(self) -> UnconnectedHandle {
        let sys = self.handle;

        // Avoid running the destructor, which would close the underlying handle
        // Since we are stack-allocated we still get cleaned up
        forget(self);

        unsafe { evdi_disconnect(sys) };

        UnconnectedHandle::new(sys)
    }

    extern "C" fn mode_changed_handler_caller(mode: evdi_mode, user_data: *mut c_void) {
        let handle = unsafe { Self::handle_from_user_data(user_data) };
        if let Err(err) = handle.mode_sender.send(mode) {
            eprintln!("Dropping msg. Mode change receiver closed, but callback called: {:?}", err);
        }
    }

    extern "C" fn update_ready_handler_caller(buf: c_int, user_data: *mut c_void) {
        let handle = unsafe { Self::handle_from_user_data(user_data) };

        let id = BufferID(buf);

        let send = handle.registered_buffers
            .get(&id)
            .map(|(send, _)| send);

        if let Some(send) = send {
            if let Err(err) = send.send(()) {
                eprintln!("Dropping msg. Update ready receiver closed, but callback called: {:?}", err);
            }
        } else {
            eprintln!("Dropping msg. No update ready channel for buffer {:?}, but callback called", id);
        }
    }

    /// Safety: user_data must be a valid reference to a Handle.
    unsafe fn handle_from_user_data<'a>(user_data: *mut c_void) -> &'a Handle {
        (user_data as *mut Handle).as_ref().unwrap()
    }

    /// Takes a handle that has just been connected. Polls until ready.
    fn new(handle: evdi_handle, device_config: DeviceConfig, ready_timeout: Duration) -> Self {
        let poll_fd = unsafe { evdi_get_event_ready(handle) };
        poll(
            &mut [pollfd { fd: poll_fd, events: POLLIN, revents: 0 }],
            Some(ready_timeout),
        ).unwrap();

        let (mode_sender, mode) = channel();

        Self {
            handle,
            device_config,
            registered_buffers: HashMap::new(),
            mode,
            mode_sender,
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            evdi_disconnect(self.handle);
            evdi_close(self.handle);
        }
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct BufferID(i32);

impl BufferID {
    pub fn new(id: i32) -> BufferID {
        BufferID(id)
    }
}

#[derive(Debug)]
pub struct Buffer {
    pub id: BufferID,
    version: u32,
    buffer: Pin<Box<Vec<u8>>>,
    rects: Pin<Box<Vec<evdi_rect>>>,
    num_rects: i32,
    width: usize,
    height: usize,
    stride: usize,
    depth: usize,
}

/// Can't have more than 16
/// see <https://displaylink.github.io/evdi/details/#grabbing-pixels>
const MAX_RECTS_BUFFER_LEN: usize = 16;

const BGRA_DEPTH: usize = 4;

impl Buffer {
    /// Allocate a buffer to store the screen of a device with a specific mode.
    pub fn new(id: BufferID, mode: &evdi_mode) -> Self {
        let width = mode.width as usize;
        let height = mode.height as usize;
        let bits_per_pixel = mode.bits_per_pixel as usize;
        let stride = bits_per_pixel / 8 * width;

        let buffer = Box::pin(vec![0u8; height * stride]);
        let rects = Box::pin(vec![evdi_rect { x1: 0, y1: 0, x2: 0, y2: 0 }; MAX_RECTS_BUFFER_LEN]);

        let buf = Buffer {
            id,
            version: 0,
            buffer,
            rects,
            num_rects: -1,
            width,
            height,
            stride,
            depth: BGRA_DEPTH,
        };

        buf
    }

    /// The portions of the screen that changed before the last call to [`Handle::request_update`]
    /// and after the preceding call, if more than one call has occurred.
    pub fn dirty_rects(&self) -> Vec<DirtyRect> {
        (0..self.num_rects as usize)
            .map(|i| DirtyRect::new(self, i))
            .collect()
    }

    fn sys(&self) -> evdi_buffer {
        evdi_buffer {
            id: self.id.0,
            buffer: self.buffer.as_ptr() as *mut c_void,
            width: self.width as c_int,
            height: self.height as c_int,
            stride: self.stride as c_int,
            rects: self.rects.as_ptr() as *mut evdi_rect,
            rect_count: 0,
        }
    }

    /// MUST be called every time the `evdi_buffer` this represents may have been written to.
    fn mark_updated(&mut self) {
        self.version += 1;
    }
}

/// A dirty portion of a [`Buffer`].
///
/// A `Buffer` is updated every time you call [`Handle::request_update`]. A `DirtyRect` refers to
/// the pixels changed in a specific update, and is thus invalid if the buffer has been updated
/// since its creation. Member functions will return `None` if the `DirtyRect` is no longer valid.
pub struct DirtyRect<'a> {
    buf: &'a Buffer,
    i: usize,
    version: u32,
}

impl<'a> DirtyRect<'a> {
    /// The x and y coordinate bounds of this [`DirtyRect`].
    ///
    /// Returns `None` if the underlying [`Buffer`] has been updated since the update this
    /// `DirtyRect` corresponds to.
    pub fn bounds(&self) -> Option<DirtyRectBounds> {
        if self.is_valid() {
            Some(DirtyRectBounds::new(self.buf.rects[self.i]))
        } else {
            None
        }
    }

    /// Copy and return the bytes this `DirtyRect` refers to.
    ///
    /// You must not call [`Handle::update_buffer`] on the buffer this came from while this function
    /// is running.
    pub fn bytes(&self) -> Option<ChunkedBytes> {
        if !self.is_valid() {
            return None;
        }

        let buf = self.buf;

        let mut out = ChunkedBytes::with_profile(buf.width, self.buf.height);
        for line in 0..self.buf.height {
            let start_inclusive = buf.stride * line;
            let end_exclusive = start_inclusive + (buf.width * buf.depth);
            // TODO: Does this copy slow us down noticeably?
            let bytes = Bytes::copy_from_slice(&buf.buffer[start_inclusive..end_exclusive]);
            out.put_bytes(bytes);
        }

        Some(out)
    }

    /// Write the pixels to a file in the unoptimized image format [PPM].
    ///
    /// This is useful when debugging, as you can open the file in an image viewer and see if the
    /// buffer is processed correctly.
    /// 
    /// The same requirements as [`Self::bytes`] apply.
    ///
    /// [PPM]: http://netpbm.sourceforge.net/doc/ppm.html
    pub fn debug_write_to_ppm(&self, f: &mut File) -> Option<io::Result<()>> {
        if let Some(bytes) = self.bytes() {
            Some(Self::debug_write_bytes_to_ppm(bytes, self.buf.width, self.buf.height, f))
        } else {
            None
        }
    }

    fn debug_write_bytes_to_ppm(
        bytes: ChunkedBytes,
        width: usize,
        height: usize,
        f: &mut File,
    ) -> io::Result<()> {
        Self::write_line(f, "P6\n")?;
        Self::write_line(f, format!("{}\n", width.to_string()))?;
        Self::write_line(f, format!("{}\n", height.to_string()))?;
        Self::write_line(f, "255\n")?;

        for chunk in bytes.into_chunks() {
            for chunk in chunk.as_ref().chunks_exact(BGRA_DEPTH) {
                let b = chunk[0];
                let g = chunk[1];
                let r = chunk[2];
                let _a = chunk[3];

                f.write_all(&[r, g, b])?;
            }
        }

        Ok(())
    }

    fn write_line<S: AsRef<str>>(f: &mut File, line: S) -> io::Result<()> {
        f.write_all(line.as_ref().as_bytes())?;
        Ok(())
    }

    fn new(buf: &'a Buffer, i: usize) -> Self {
        Self { buf, i, version: buf.version }
    }

    fn is_valid(&self) -> bool {
        self.version == self.buf.version
    }
}

pub struct DirtyRectBounds {
    x1: u32,
    y1: u32,
    x2: u32,
    y2: u32,
}

impl DirtyRectBounds {
    fn new(sys: evdi_rect) -> Self {
        Self {
            x1: sys.x1 as u32,
            y1: sys.y1 as u32,
            x2: sys.x2 as u32,
            y2: sys.y2 as u32,
        }
    }

    fn width(&self) -> u32 {
        self.x2 - self.x1
    }

    fn height(&self) -> u32 {
        self.y2 - self.y1
    }
}

/// Automatically closed on drop
#[derive(Debug)]
pub struct UnconnectedHandle {
    handle: evdi_handle
}

impl UnconnectedHandle {
    /// Connect to an handle and block until ready.
    ///
    /// ```
    /// # use evdi::device::Device;
    /// # use evdi::device_config::DeviceConfig;
    /// # use std::time::Duration;
    /// let device: Device = Device::get().unwrap();
    /// let handle = device
    ///     .open()
    ///     .connect(&DeviceConfig::sample(), Duration::from_secs(1));
    /// ```
    pub fn connect(self, config: &DeviceConfig, ready_timeout: Duration) -> Handle {
        // NOTE: We deliberately take ownership to ensure a handle is connected at most once.

        let config: DeviceConfig = config.to_owned();
        let edid = Box::leak(Box::new(config.edid()));
        unsafe {
            evdi_connect(
                self.handle,
                edid.as_ptr(),
                edid.len() as c_uint,
                config.sku_area_limit(),
            );
        }

        let sys = self.handle;

        // Avoid running the destructor, which would close the underlying handle
        // Since we are stack-allocated we still get cleaned up
        forget(self);

        Handle::new(sys, config, ready_timeout)
    }

    pub(crate) fn new(handle: evdi_handle) -> Self {
        Self { handle }
    }
}

impl Drop for UnconnectedHandle {
    fn drop(&mut self) {
        unsafe { evdi_close(self.handle) };
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::device::Device;
    use crate::device_config::DeviceConfig;

    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(1);

    fn connect() -> Handle {
        Device::get().unwrap()
            .open()
            .connect(&DeviceConfig::sample(), TIMEOUT)
    }

    #[test]
    fn can_connect() {
        connect();
    }

    #[test]
    fn can_enable_cursor_events() {
        connect().enable_cursor_events(true);
    }

    #[test]
    fn can_receive_mode() {
        let handle = connect();
        handle.request_events();
        let mode = handle.receive_mode(TIMEOUT).unwrap();
        assert!(mode.height > 100);
    }

    #[test]
    fn can_create_buffer() {
        let handle = connect();
        handle.request_events();
        let mode = handle.receive_mode(TIMEOUT).unwrap();
        Buffer::new(BufferID(1), &mode);
    }

    #[test]
    fn can_access_buffer_sys() {
        let handle = connect();
        handle.request_events();
        let mode = handle.receive_mode(TIMEOUT).unwrap();
        Buffer::new(BufferID(1), &mode).sys();
    }

    #[test]
    fn update_includes_at_least_one_dirty_rect() {
        let mut handle = connect();
        let buf = get_update(&mut handle);

        assert!(buf.dirty_rects().len() > 0);
    }

    #[test]
    fn update_can_be_called_multiple_times() {
        let mut handle = connect();

        handle.request_events();
        let mode = handle.receive_mode(TIMEOUT).unwrap();

        let mut buf = Buffer::new(BufferID::new(1), &mode);

        for _ in 0..10 {
            handle.request_update(&mut buf, TIMEOUT).unwrap();
        }
    }

    fn get_update(handle: &mut Handle) -> Buffer {
        handle.request_events();
        let mode = handle.receive_mode(TIMEOUT).unwrap();
        let mut buf = Buffer::new(BufferID::new(1), &mode);

        // Give us some time to settle
        for _ in 0..20 {
            handle.request_update(&mut buf, TIMEOUT).unwrap();
        }

        buf
    }

    #[test]
    fn bytes_is_non_empty() {
        let mut handle = connect();
        let buf = get_update(&mut handle);
        let rects = buf.dirty_rects();
        let rect = &rects[0];

        let mut total: u32 = 0;
        let mut len: u32 = 0;
        for chunk in rect.bytes().unwrap().into_chunks() {
            for byte in chunk {
                total += byte as u32;
                len += 1;
            }
        }

        let avg = total / len;

        assert!(avg > 10, "avg byte {:?} < 10, suggesting we aren't correctly grabbing the screen", avg);
    }

    #[test]
    fn can_output_debug() {
        let mut handle = connect();
        let buf = get_update(&mut handle);
        let rects = buf.dirty_rects();
        let rect = &rects[0];

        let mut f = File::with_options()
            .write(true)
            .create(true)
            .open("TEMP_debug_rect.pnm")
            .unwrap();

        rect.debug_write_to_ppm(&mut f).unwrap().unwrap();
    }

    #[test]
    fn can_disconnect() {
        let mut handle = connect();

        for _ in 0..10 {
            let unconnected = handle.disconnect();
            handle = unconnected.connect(&DeviceConfig::sample(), TIMEOUT);
        }
    }
}
