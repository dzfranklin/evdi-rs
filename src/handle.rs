//! Performs most operations

use std::collections::HashMap;
use std::mem::forget;
use std::os::raw::{c_int, c_uint, c_void};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use evdi_sys::*;
use filedescriptor::{poll, pollfd, POLLIN};
use thiserror::Error;

use crate::prelude::*;

/// Represents a handle that is open but not connected.
#[derive(Debug)]
pub struct UnconnectedHandle {
    handle: evdi_handle,
}

impl UnconnectedHandle {
    /// Connect to an handle and block until ready.
    ///
    /// ```
    /// # use evdi::prelude::*;
    /// # use std::time::Duration;
    /// let device: DeviceNode = DeviceNode::get().unwrap();
    /// let handle = device
    ///     .open().unwrap()
    ///     .connect(&DeviceConfig::sample(), Duration::from_secs(1));
    /// ```
    pub fn connect(
        self,
        config: &DeviceConfig,
        ready_timeout: Duration,
    ) -> Result<Handle, PollReadyError> {
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

        let handle = Handle::new(sys, config);

        let poll_fd = unsafe { evdi_get_event_ready(handle.sys) };
        poll(
            &mut [pollfd {
                fd: poll_fd,
                events: POLLIN,
                revents: 0,
            }],
            Some(ready_timeout),
        )?;

        Ok(handle)
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

#[derive(Debug, Error)]
pub enum PollReadyError {
    #[error("The polling library we currently use doesn't provide detailed errors")]
    Generic(#[from] anyhow::Error),
}

/// Represents an evdi handle that is connected and ready.
#[derive(Debug)]
pub struct Handle {
    sys: evdi_handle,
    device_config: DeviceConfig,
    buffers: HashMap<BufferId, Buffer>,
    mode: Receiver<Mode>,
    mode_sender: Sender<Mode>,
}

impl Handle {
    /// Allocate and register a buffer to store the screen of a device with a specific mode.
    ///
    /// You are responsible for re-creating buffers if the mode changes.
    ///
    /// ```
    /// # use evdi::prelude::*;
    /// # use std::time::Duration;
    /// # use std::error::Error;
    /// # fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    /// # let timeout = Duration::from_secs(1);
    /// # let mut handle = DeviceNode::get().expect("At least on evdi device available").open()?
    /// #     .connect(&DeviceConfig::sample(), timeout)?;
    /// # handle.request_events();
    /// # let mode = handle.receive_mode(timeout)?;
    /// #
    /// let buffer_id = handle.new_buffer(&mode);
    /// let buffer_data = handle.get_buffer(buffer_id).expect("Buffer exists");
    /// assert_eq!(buffer_data.width, mode.width as usize);
    ///
    /// handle.unregister_buffer(buffer_id);
    /// assert!(handle.get_buffer(buffer_id).is_none());
    /// # Ok(())
    /// # }
    pub fn new_buffer(&mut self, mode: &Mode) -> BufferId {
        let buffer = Buffer::new(mode);
        let id = buffer.id;
        unsafe { evdi_register_buffer(self.sys, buffer.sys()) };
        self.buffers.insert(id, buffer);
        id
    }

    /// De-allocate and unregister a buffer.
    ///
    /// After calling this, [`Handle::get_buffer(id)`] will return `None`.
    pub fn unregister_buffer(&mut self, id: BufferId) {
        let removed = self.buffers.remove(&id);
        if removed.is_some() {
            unsafe { evdi_unregister_buffer(self.sys, id.sys()) };
        }
    }

    /// Get buffer data, if the [`BufferId`] provided is associated with this handle.
    pub fn get_buffer(&self, id: BufferId) -> Option<&Buffer> {
        self.buffers.get(&id)
    }

    /// Ask the kernel module to update a buffer with the current display pixels.
    ///
    /// Blocks until the update is complete.
    ///
    /// ```
    /// # use evdi::prelude::*;
    /// # use std::time::Duration;
    /// # let timeout = Duration::from_secs(1);
    /// # let mut handle = DeviceNode::get().unwrap().open().unwrap()
    /// #     .connect(&DeviceConfig::sample(), timeout).unwrap();
    /// # handle.request_events();
    /// # let mode = handle.receive_mode(timeout).unwrap();
    /// let buf_id = handle.new_buffer(&mode);
    /// handle.request_update(buf_id, timeout).unwrap();
    /// let buf_data = handle.get_buffer(buf_id).unwrap();
    /// ```
    pub fn request_update(
        &mut self,
        buffer_id: BufferId,
        timeout: Duration,
    ) -> Result<(), RequestUpdateError> {
        // NOTE: We need to take &mut self to ensure we can't be called concurrently. This is
        //  required because evdi_grab_pixels grabs from the most recently updated buffer.
        //
        //  We need to take &mut buffer to ensure the buffer can't be read from while it's being
        //  updated.
        let user_data_sys = self as *const Handle;
        let handle_sys = self.sys;

        let buffer = self
            .buffers
            .get_mut(&buffer_id)
            .ok_or(RequestUpdateError::UnregisteredBuffer)?;

        let ready = unsafe { evdi_request_update(handle_sys, buffer_id.sys()) };
        if !ready {
            Self::request_events_sys(user_data_sys, handle_sys);
            buffer.block_until_update_ready(timeout)?;
        }

        unsafe {
            evdi_grab_pixels(
                handle_sys as *mut evdi_device_context,
                buffer.rects_ptr_sys(),
                buffer.rects_count_ptr_sys(),
            )
        }

        buffer.mark_updated();

        Ok(())
    }

    pub fn enable_cursor_events(&self, enable: bool) {
        unsafe {
            evdi_enable_cursor_events(self.sys, enable);
        }
    }

    /// Ask the kernel module to send us some events.
    ///
    /// I think this blocks, dispatches a certain number of events, and the then returns, so callers
    /// should call in a loop. However, the docs aren't clear.
    /// See <https://github.com/DisplayLink/evdi/issues/265>
    pub fn request_events(&self) {
        Self::request_events_sys(self as *const Handle, self.sys);
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
    /// # use evdi::prelude::*;
    /// # use std::time::Duration;
    /// # let device: DeviceNode = DeviceNode::get().unwrap();
    /// # let timeout = Duration::from_secs(1);
    /// # let mut handle = device.open().unwrap()
    /// #   .connect(&DeviceConfig::sample(), timeout).unwrap();
    /// handle.request_events();
    /// let mode = handle.receive_mode(timeout).unwrap();
    /// ```
    pub fn receive_mode(&self, timeout: Duration) -> Result<Mode, RecvTimeoutError> {
        self.mode.recv_timeout(timeout)
    }

    /// Returns a mode event if one is currently available without blocking.
    ///
    /// A mode event will not be received unless [`Self::request_events`] has been called.
    ///
    /// ```
    /// # use evdi::prelude::*;
    /// # use std::time::Duration;
    /// # let device: DeviceNode = DeviceNode::get().unwrap();
    /// # let timeout = Duration::from_secs(1);
    /// # let mut handle = device.open().unwrap()
    /// #   .connect(&DeviceConfig::sample(), timeout).unwrap();
    /// handle.request_events();
    /// if let Some(mode) = handle.try_receive_mode() {
    ///     // use the mode
    /// }
    /// ```
    pub fn try_receive_mode(&self) -> Option<Mode> {
        self.mode.try_recv().ok()
    }

    /// Disconnect the handle.
    ///
    /// A handle is automatically disconnected and closed on drop, you only need this if you want
    /// to keep the `UnconnectedHandle` around to potentially connect to later.
    pub fn disconnect(self) -> UnconnectedHandle {
        let sys = self.sys;

        // Avoid running the destructor, which would close the underlying handle
        // Since we are stack-allocated we still get cleaned up
        forget(self);

        unsafe { evdi_disconnect(sys) };

        UnconnectedHandle::new(sys)
    }

    extern "C" fn mode_changed_handler_caller(mode: evdi_mode, user_data: *mut c_void) {
        let handle = unsafe { Self::handle_from_user_data(user_data) };
        if let Err(err) = handle.mode_sender.send(mode.into()) {
            eprintln!(
                "Dropping msg. Mode change receiver closed, but callback called: {:?}",
                err
            );
        }
    }

    extern "C" fn update_ready_handler_caller(buf: c_int, user_data: *mut c_void) {
        let handle = unsafe { Self::handle_from_user_data(user_data) };

        let id = BufferId::new(buf);
        let buf = handle.buffers.get(&id);

        if let Some(buf) = buf {
            if let Err(err) = buf.update_ready_sender().send(()) {
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

    /// Takes a handle that has just been connected and polled until ready.
    fn new(handle_sys: evdi_handle, device_config: DeviceConfig) -> Self {
        let (mode_sender, mode) = channel();

        Self {
            sys: handle_sys,
            device_config,
            buffers: HashMap::new(),
            mode,
            mode_sender,
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            evdi_disconnect(self.sys);
            evdi_close(self.sys);
        }
    }
}

impl PartialEq for Handle {
    fn eq(&self, other: &Self) -> bool {
        self.sys == other.sys
    }
}

impl Eq for Handle {}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum RequestUpdateError {
    #[error("Kernel chose to update async, timeout waiting for")]
    Timeout(#[from] RecvTimeoutError),
    #[error("The buffer provided does not exist or is attached to a different handle")]
    UnregisteredBuffer,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use std::fs::File;

    const TIMEOUT: Duration = Duration::from_secs(1);

    fn handle_fixture() -> Handle {
        DeviceNode::get()
            .unwrap()
            .open()
            .unwrap()
            .connect(&DeviceConfig::sample(), TIMEOUT)
            .unwrap()
    }

    #[test]
    fn can_connect() {
        handle_fixture();
    }

    #[test]
    fn can_enable_cursor_events() {
        handle_fixture().enable_cursor_events(true);
    }

    #[test]
    fn can_receive_mode() {
        let handle = handle_fixture();
        handle.request_events();
        let mode = handle.receive_mode(TIMEOUT).unwrap();
        assert!(mode.height > 100);
    }

    #[test]
    fn update_can_be_called_multiple_times() {
        let mut handle = handle_fixture();

        handle.request_events();
        let mode = handle.receive_mode(TIMEOUT).unwrap();

        let buf_id = handle.new_buffer(&mode);

        for _ in 0..10 {
            handle.request_update(buf_id, TIMEOUT).unwrap();
        }
    }

    fn get_update(handle: &mut Handle) -> &Buffer {
        handle.request_events();
        let mode = handle.receive_mode(TIMEOUT).unwrap();
        let buf_id = handle.new_buffer(&mode);

        // Give us some time to settle
        for _ in 0..20 {
            handle.request_update(buf_id, TIMEOUT).unwrap();
        }

        handle.get_buffer(buf_id).unwrap()
    }

    #[test]
    fn bytes_is_non_empty() {
        let mut handle = handle_fixture();
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
        let mut handle = handle_fixture();
        let buf = get_update(&mut handle);

        let mut f = File::create("TEMP_debug_rect.pnm").unwrap();

        buf.debug_write_to_ppm(&mut f).unwrap();
    }

    #[test]
    fn can_disconnect() {
        let mut handle = handle_fixture();

        for _ in 0..10 {
            let unconnected = handle.disconnect();
            handle = unconnected
                .connect(&DeviceConfig::sample(), TIMEOUT)
                .unwrap();
        }
    }

    #[test]
    fn try_receive_returns_none_initially() {
        let handle = handle_fixture();
        assert!(handle.try_receive_mode().is_none());
    }

    #[test]
    fn cannot_get_buffer_after_unregister() {
        let mut handle = handle_fixture();
        handle.request_events();
        let mode = handle.receive_mode(TIMEOUT).unwrap();

        let buf = handle.new_buffer(&mode);
        handle.unregister_buffer(buf);
        let res = handle.request_update(buf, TIMEOUT);
        assert_eq!(res.unwrap_err(), RequestUpdateError::UnregisteredBuffer);
    }
}
