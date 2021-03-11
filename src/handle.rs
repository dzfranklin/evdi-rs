use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::io::Write;
use std::mem::forget;
use std::os::raw::{c_int, c_uint, c_void};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use evdi_sys::*;
use filedescriptor::{poll, pollfd, POLLIN};
use thiserror::Error;

use crate::device_config::DeviceConfig;

/// Represents an evdi handle that is connected and ready.
///
/// Automatically disconnected on drop.
#[derive(Debug)]
pub struct Handle {
    handle: evdi_handle,
    device_config: DeviceConfig,
    // NOTE: Not cleaned up on buffer deregister
    registered_update_ready_senders: HashMap<BufferId, Sender<()>>,
    mode: Receiver<evdi_mode>,
    mode_sender: Sender<evdi_mode>,
}

impl Handle {
    /// Ask the kernel module to update a buffer with the current display pixels.
    ///
    /// Blocks until the update is complete.
    ///
    /// ```
    /// # use evdi::{device::Device, device_config::DeviceConfig, handle::{Buffer, BufferId}};
    /// # use std::time::Duration;
    /// # let timeout = Duration::from_secs(1);
    /// # let mut handle = Device::get().unwrap().open().connect(&DeviceConfig::sample(), timeout);
    /// # handle.request_events();
    /// # let mode = handle.receive_mode(timeout).unwrap();
    /// let mut buf = Buffer::new(BufferId::new(1), &mode);
    /// handle.request_update(&mut buf, timeout).unwrap();
    /// ```
    pub fn request_update(
        &mut self,
        buffer: &mut Buffer,
        timeout: Duration,
    ) -> Result<(), RequestUpdateError> {
        // NOTE: We need to take &mut self to ensure we can't be called concurrently. This is
        //  required because evdi_grab_pixels grabs from the most recently updated buffer.
        //
        //  We need to take &mut buffer to ensure the buffer can't be read from while it's being
        //  updated.
        let user_data_sys = self as *const Handle;
        let handle_sys = self.handle;
        let id_sys = buffer.id.0;

        if let Some(sys) = buffer.attached_to {
            if sys != self.handle {
                return Err(RequestUpdateError::BufferAttachedToDifferentHandle);
            }
        } else {
            unsafe { evdi_register_buffer(handle_sys, buffer.sys()) };
            self.registered_update_ready_senders
                .insert(buffer.id, buffer.send_update_ready.clone());
            buffer.attached_to = Some(self.handle);
        }

        let ready = unsafe { evdi_request_update(handle_sys, id_sys) };
        if !ready {
            Self::request_events_sys(user_data_sys, handle_sys);
            buffer.update_ready.recv_timeout(timeout)?;
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

    pub fn enable_cursor_events(&self, enable: bool) {
        unsafe {
            evdi_enable_cursor_events(self.handle, enable);
        }
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
            eprintln!(
                "Dropping msg. Mode change receiver closed, but callback called: {:?}",
                err
            );
        }
    }

    extern "C" fn update_ready_handler_caller(buf: c_int, user_data: *mut c_void) {
        let handle = unsafe { Self::handle_from_user_data(user_data) };

        let id = BufferId(buf);
        let send = handle.registered_update_ready_senders.get(&id);

        if let Some(send) = send {
            if let Err(err) = send.send(()) {
                eprintln!(
                    "Dropping msg. Update ready receiver closed, but callback called: {:?}",
                    err
                );
            }
        } else {
            eprintln!(
                "Dropping msg. No update ready channel for buffer {:?}, but callback called",
                id
            );
        }
    }

    /// Safety: user_data must be a valid reference to a Handle.
    unsafe fn handle_from_user_data<'b>(user_data: *mut c_void) -> &'b Handle {
        (user_data as *mut Handle).as_ref().unwrap()
    }

    /// Takes a handle that has just been connected. Polls until ready.
    fn new(handle_sys: evdi_handle, device_config: DeviceConfig, ready_timeout: Duration) -> Self {
        let poll_fd = unsafe { evdi_get_event_ready(handle_sys) };
        poll(
            &mut [pollfd {
                fd: poll_fd,
                events: POLLIN,
                revents: 0,
            }],
            Some(ready_timeout),
        )
        .unwrap();

        let (mode_sender, mode) = channel();

        Self {
            handle: handle_sys,
            device_config,
            registered_update_ready_senders: HashMap::new(),
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

impl PartialEq for Handle {
    fn eq(&self, other: &Self) -> bool {
        self.handle == other.handle
    }
}

impl Eq for Handle {}

#[derive(Debug, Error)]
pub enum RequestUpdateError {
    #[error("Kernel chose to update async, timeout waiting for")]
    Timeout(#[from] RecvTimeoutError),
    #[error("The buffer provided is attached to a different handle")]
    BufferAttachedToDifferentHandle,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct BufferId(i32);

// TODO: Generate randomly?
impl BufferId {
    pub fn new(id: i32) -> BufferId {
        BufferId(id)
    }
}

#[derive(Debug)]
pub struct Buffer {
    pub id: BufferId,
    attached_to: Option<evdi_handle>,
    update_ready: Receiver<()>,
    send_update_ready: Sender<()>,
    buffer: Box<[u8]>,
    rects: Box<[evdi_rect]>,
    num_rects: i32,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
}

/// Can't have more than 16
/// see <https://displaylink.github.io/evdi/details/#grabbing-pixels>
const MAX_RECTS_BUFFER_LEN: usize = 16;

const BGRA_DEPTH: usize = 4;

impl Buffer {
    /// Allocate a buffer to store the screen of a device with a specific mode.
    pub fn new(id: BufferId, mode: &evdi_mode) -> Self {
        let width = mode.width as usize;
        let height = mode.height as usize;
        let bits_per_pixel = mode.bits_per_pixel as usize;
        let stride = bits_per_pixel / 8 * width;

        // NOTE: We use a boxed slice to prevent accidental re-allocation
        let buffer = vec![0u8; height * stride].into_boxed_slice();
        let rects = vec![
            evdi_rect {
                x1: 0,
                y1: 0,
                x2: 0,
                y2: 0,
            };
            MAX_RECTS_BUFFER_LEN
        ]
        .into_boxed_slice();

        let (send_update_ready, update_ready) = channel();

        Buffer {
            id,
            attached_to: None,
            update_ready,
            send_update_ready,
            buffer,
            rects,
            num_rects: -1,
            width,
            height,
            stride,
        }
    }

    /// Get a reference to the underlying bytes of this buffer.
    ///
    /// Use [`Buffer.width`], [`Buffer.height`], and [`Buffer.stride`] to interpret this.
    ///
    /// I believe this is in the format BGRA32. Some toy examples by other users assume that format.
    /// I've filed [an issue][issue] on the wrapped library to clarify this.
    ///
    /// [issue]: https://github.com/DisplayLink/evdi/issues/266
    pub fn bytes(&self) -> &[u8] {
        self.buffer.as_ref()
    }

    /// Write the pixels to a file in the unoptimized image format [PPM].
    ///
    /// This is useful when debugging, as you can open the file in an image viewer and see if the
    /// buffer is processed correctly.
    ///
    /// [PPM]: http://netpbm.sourceforge.net/doc/ppm.html
    pub fn debug_write_to_ppm(&self, f: &mut File) -> io::Result<()> {
        Self::write_line(f, "P6\n")?;
        Self::write_line(f, format!("{}\n", self.width.to_string()))?;
        Self::write_line(f, format!("{}\n", self.height.to_string()))?;
        Self::write_line(f, "255\n")?;

        for row in self.buffer.chunks_exact(self.stride) {
            for pixel in row[0..self.width].chunks_exact(BGRA_DEPTH) {
                let b = pixel[0];
                let g = pixel[1];
                let r = pixel[2];
                let _a = pixel[3];

                f.write_all(&[r, g, b])?;
            }
        }

        f.flush()?;

        Ok(())
    }

    fn write_line<S: AsRef<str>>(f: &mut File, line: S) -> io::Result<()> {
        f.write_all(line.as_ref().as_bytes())?;
        Ok(())
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
}

impl Drop for Buffer {
    fn drop(&mut self) {
        if let Some(handle) = self.attached_to.as_ref() {
            // NOTE: We don't unregister the channel from the handle because of the onerous changes
            // it would require us to make to our api surface.
            unsafe { evdi_unregister_buffer(*handle, self.id.0) };
        }
    }
}

/// Automatically closed on drop
#[derive(Debug)]
pub struct UnconnectedHandle {
    handle: evdi_handle,
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
        Device::get()
            .unwrap()
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
        Buffer::new(BufferId(1), &mode);
    }

    #[test]
    fn can_access_buffer_sys() {
        let handle = connect();
        handle.request_events();
        let mode = handle.receive_mode(TIMEOUT).unwrap();
        Buffer::new(BufferId(1), &mode).sys();
    }

    #[test]
    fn update_can_be_called_multiple_times() {
        let mut handle = connect();

        handle.request_events();
        let mode = handle.receive_mode(TIMEOUT).unwrap();

        let mut buf = Buffer::new(BufferId::new(1), &mode);

        for _ in 0..10 {
            {
                handle.request_update(&mut buf, TIMEOUT).unwrap();
            }
        }
    }

    fn get_update(handle: &mut Handle) -> Buffer {
        handle.request_events();
        let mode = handle.receive_mode(TIMEOUT).unwrap();
        let mut buf = Buffer::new(BufferId::new(1), &mode);

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

        let mut total: u32 = 0;
        let mut len: u32 = 0;
        for byte in buf.bytes().iter() {
            total += *byte as u32;
            len += 1;
        }

        let avg = total / len;

        assert!(
            avg > 10,
            "avg byte {:?} < 10, suggesting we aren't correctly grabbing the screen",
            avg
        );
    }

    #[test]
    fn can_output_debug() {
        let mut handle = connect();
        let buf = get_update(&mut handle);

        let mut f = File::with_options()
            .write(true)
            .create(true)
            .open("TEMP_debug_rect.pnm")
            .unwrap();

        buf.debug_write_to_ppm(&mut f).unwrap();
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
