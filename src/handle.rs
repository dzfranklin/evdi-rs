//! Performs most operations

use std::collections::HashMap;
use std::mem::forget;
use std::os::raw::{c_int, c_uint, c_void};
use std::time::Duration;

use evdi_sys::*;
use thiserror::Error;
use tokio::io::unix::AsyncFd;
use tokio::sync::watch;
use tokio::time;

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
    /// use evdi::handle::PollReadyError;
    /// # tokio_test::block_on(async {
    /// let device: DeviceNode = DeviceNode::get().unwrap();
    /// let handle = device
    ///     .open().unwrap()
    ///     .connect(&DeviceConfig::sample(), Duration::from_secs(1))
    ///     .await?;
    /// # Ok::<(), PollReadyError>(())
    /// # });
    /// ```
    pub async fn connect(
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

        let raw_fd = unsafe { evdi_get_event_ready(handle.sys) };
        let fd = AsyncFd::new(raw_fd)?;

        tokio::select! {
            guard = fd.readable() => {
                guard?.retain_ready()
            },
            _ = tokio::time::sleep(ready_timeout) => {
                return Err(PollReadyError::Timeout)
            },
        }

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
    #[error("IO")]
    Io(#[from] std::io::Error),
    #[error("Timeout")]
    Timeout,
}

/// Represents an evdi handle that is connected and ready.
#[derive(Debug)]
pub struct Handle {
    sys: evdi_handle,
    device_config: DeviceConfig,
    buffers: HashMap<BufferId, Buffer>,
    /// Holds [`::tokio::sync::mpsc::Receiver`]s for events.
    ///
    /// ```
    /// # use evdi::prelude::*;
    /// # use std::time::Duration;
    /// # tokio_test::block_on(async {
    /// # let device: DeviceNode = DeviceNode::get().unwrap();
    /// # let timeout = Duration::from_secs(1);
    /// # let mut handle = device.open().unwrap()
    /// #   .connect(&DeviceConfig::sample(), timeout).await.unwrap();
    /// #
    /// // Initially events will be None
    /// {
    ///     let mode = handle.events.mode.borrow();
    ///     assert!(mode.is_none());
    /// }
    ///
    /// handle.dispatch_events();
    ///
    /// // Wait until we get a mode
    /// handle.events.mode.changed().await;
    ///
    /// let mode = handle.events.mode.borrow();
    /// assert!(mode.is_some());
    /// # });
    /// ```
    pub events: HandleEvents,
}

impl Handle {
    /// Allocate and register a buffer to store the screen of a device with a specific mode.
    ///
    /// You are responsible for re-creating buffers if the mode changes.
    pub fn new_buffer(&mut self, mode: &Mode) -> BufferId {
        let buffer = Buffer::new(mode);
        let id = buffer.id;
        unsafe { evdi_register_buffer(self.sys, buffer.sys()) };
        self.buffers.insert(id, buffer);
        id
    }

    /// De-allocate and unregister a buffer.
    pub fn unregister_buffer(&mut self, id: BufferId) {
        let removed = self.buffers.remove(&id);
        if removed.is_some() {
            unsafe { evdi_unregister_buffer(self.sys, id.sys()) };
        }
    }

    /// Get buffer data if the [`BufferId`] provided is associated with this handle.
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
    /// # use std::error::Error;
    /// # tokio_test::block_on(async {
    /// # let timeout = Duration::from_secs(1);
    /// # let mut handle = DeviceNode::get().unwrap().open()?
    /// #     .connect(&DeviceConfig::sample(), timeout).await?;
    /// # handle.dispatch_events();
    /// # handle.events.mode.changed().await;
    /// # let mode = handle.events.mode.borrow().unwrap();
    /// let buf_id = handle.new_buffer(&mode);
    /// handle.request_update(buf_id, timeout).await?;
    /// let buf_data = handle.get_buffer(buf_id).expect("Buffer exists");
    /// # Ok::<(), Box<dyn Error>>(())
    /// # });
    /// ```
    ///
    /// Note: [`Handle::request_update`] happens to be implemented in such a way that it causes
    /// events available at the time it is called to be dispatched. Users should not rely on this.
    pub async fn request_update(
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
            Self::dispatch_events_sys(user_data_sys, handle_sys);
            tokio::select! {
                resp = buffer.update_ready.recv() => {
                    resp.expect("Buffer update channel must not be closed");
                }
                _ = time::sleep(timeout) => {
                    return Err(RequestUpdateError::Timeout)
                }
            }
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

    /// Dispatch events received from the kernel module to the channels in [`Handle::events`].
    ///
    /// If you want to receive events in [`Handle::events`] you must call this in some sort of loop.
    ///
    /// Note: [`Handle::request_update`] happens to be implemented in such a way that it causes
    /// events available at the time it is called to be dispatched. Users should not rely on this.
    pub fn dispatch_events(&self) {
        Self::dispatch_events_sys(self as *const Handle, self.sys);
    }

    fn dispatch_events_sys(user_data: *const Handle, handle: evdi_handle) {
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

        if let Err(err) = handle.events.mode_sender.send(Some(mode.into())) {
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
            tokio::spawn(async move {
                if let Err(err) = buf.send_update_ready.send(()).await {
                    eprintln!(
                        "Dropping msg. Update ready receiver closed, but callback called: {:?}",
                        err
                    );
                }
            });
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
        Self {
            sys: handle_sys,
            device_config,
            buffers: HashMap::new(),
            events: HandleEvents::default(),
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

#[derive(Debug)]
pub struct HandleEvents {
    pub mode: watch::Receiver<Option<Mode>>,
    mode_sender: watch::Sender<Option<Mode>>,
}

impl Default for HandleEvents {
    fn default() -> Self {
        let (mode_sender, mode) = watch::channel(None);

        Self { mode, mode_sender }
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum RequestUpdateError {
    #[error("Kernel chose to update async, timeout waiting for response")]
    Timeout,
    #[error("The buffer provided does not exist or is attached to a different handle")]
    UnregisteredBuffer,
}

#[cfg(test)]
pub mod tests {
    use std::fs::File;
    use std::time::Duration;

    use super::*;

    const TIMEOUT: Duration = Duration::from_secs(1);

    pub async fn handle_fixture() -> Handle {
        DeviceNode::get()
            .unwrap()
            .open()
            .unwrap()
            .connect(&DeviceConfig::sample(), TIMEOUT)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn can_connect() {
        handle_fixture().await;
    }

    #[tokio::test]
    async fn can_enable_cursor_events() {
        handle_fixture().await.enable_cursor_events(true);
    }

    #[tokio::test]
    async fn can_receive_mode() {
        let mut handle = handle_fixture().await;
        handle.dispatch_events();
        handle.events.mode.changed().await.unwrap();
        let mode = handle.events.mode.borrow().unwrap();
        assert!(mode.height > 100);
    }

    #[tokio::test]
    async fn update_can_be_called_multiple_times() {
        let mut handle = handle_fixture().await;

        handle.dispatch_events();
        handle.events.mode.changed().await.unwrap();
        let mode = handle.events.mode.borrow().unwrap();

        let buf_id = handle.new_buffer(&mode);

        for _ in 0..5 {
            handle.request_update(buf_id, TIMEOUT).await.unwrap();
        }
    }

    async fn get_update(handle: &mut Handle) -> &Buffer {
        handle.dispatch_events();
        handle.events.mode.changed().await.unwrap();
        let mode = handle.events.mode.borrow().unwrap();
        let buf_id = handle.new_buffer(&mode);

        // Give us some time to settle
        for _ in 0..5 {
            handle.request_update(buf_id, TIMEOUT).await.unwrap();
        }

        handle.get_buffer(buf_id).unwrap()
    }

    #[tokio::test]
    async fn bytes_is_non_empty() {
        let mut handle = handle_fixture().await;
        let buf = get_update(&mut handle).await;

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

    #[tokio::test]
    async fn can_output_debug() {
        let mut handle = handle_fixture().await;
        let buf = get_update(&mut handle).await;

        let mut f = File::create("TEMP_debug_rect.pnm").unwrap();

        buf.debug_write_to_ppm(&mut f).unwrap();
    }

    #[tokio::test]
    async fn can_disconnect() {
        let mut handle = handle_fixture().await;

        for _ in 0..10 {
            let unconnected = handle.disconnect();
            handle = unconnected
                .connect(&DeviceConfig::sample(), TIMEOUT)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn cannot_get_buffer_after_unregister() {
        let mut handle = handle_fixture().await;
        handle.dispatch_events();
        handle.events.mode.changed().await.unwrap();
        let mode = handle.events.mode.borrow().unwrap();

        let buf = handle.new_buffer(&mode);
        handle.unregister_buffer(buf);
        let res = handle.request_update(buf, TIMEOUT).await;
        assert_eq!(res.unwrap_err(), RequestUpdateError::UnregisteredBuffer);
    }
}
