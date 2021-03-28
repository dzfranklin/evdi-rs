//! Performs most operations

use std::collections::HashMap;
use std::io;
use std::mem::forget;
use std::os::raw::{c_int, c_uint, c_void};
use std::time::Duration;

use evdi_sys::*;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::events::{AwaitEventError, Event};
use crate::prelude::*;
use derivative::Derivative;

/// Represents a handle that is open but not connected.
#[derive(Debug)]
pub struct UnconnectedHandle {
    device: DeviceNode,
    handle: evdi_handle,
}

impl UnconnectedHandle {
    /// Connect to a handle.
    ///
    /// ```
    /// # use evdi::prelude::*;
    /// # use std::time::Duration;
    /// # use evdi::handle::HandleConnectError;
    /// # tokio_test::block_on(async {
    /// let device: DeviceNode = DeviceNode::get().unwrap();
    /// let handle = device
    ///     .open()?
    ///     .connect(&DeviceConfig::sample());
    /// # Ok::<(), HandleConnectError>(())
    /// # });
    /// ```
    #[instrument]
    pub fn connect(self, config: &DeviceConfig) -> Handle {
        // NOTE: We deliberately take ownership to ensure a handle is connected at most once.

        let edid = Box::leak(Box::new(config.edid()));

        {
            unsafe {
                evdi_connect(
                    self.handle,
                    edid.as_ptr(),
                    edid.len() as c_uint,
                    config.sku_area_limit(),
                );
            }
            info!("evdi_connect")
        }

        let sys = self.handle;
        let device = self.device.clone();

        // Avoid running the destructor, which would close the underlying handle
        // Since we are stack-allocated we still get cleaned up
        forget(self);

        Handle::new(device, sys, config)
    }

    pub(crate) fn new(device: DeviceNode, handle: evdi_handle) -> Self {
        Self { handle, device }
    }
}

impl Drop for UnconnectedHandle {
    fn drop(&mut self) {
        unsafe { evdi_close(self.handle) };
    }
}

#[derive(Debug, Error)]
pub enum HandleConnectError {
    #[error("IO error opening ready file descriptor")]
    Io(#[from] std::io::Error),
    #[error("Timeout")]
    Timeout,
}

/// Represents an evdi handle that is connected and ready.
#[derive(Derivative)]
#[derivative(Debug)]
pub struct Handle {
    device: DeviceNode,
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
    /// # let mut handle = device.open()?.connect(&DeviceConfig::sample());
    /// // Initially events will be None
    /// let mode = handle.events.current_mode();
    /// assert!(mode.is_none());
    ///
    /// // Wait for a mode
    /// let mode: Mode = handle.events.await_mode(timeout).await?;
    /// # Ok::<_, Box<dyn std::error::Error>>(())
    /// # });
    /// ```
    pub events: HandleEvents,
    #[derivative(Debug = "ignore")]
    close_event_handler: crossbeam_channel::Sender<()>,
}

impl Handle {
    /// Allocate and register a buffer to store the screen of a device with a specific mode.
    ///
    /// You are responsible for re-creating buffers if the mode changes.
    #[instrument]
    pub fn new_buffer(&mut self, mode: &Mode) -> BufferId {
        let mut buffer = Buffer::new(mode);
        let id = buffer.id;
        unsafe { evdi_register_buffer(self.sys, buffer.sys()) };
        self.buffers.insert(id, buffer);
        id
    }

    /// De-allocate and unregister a buffer.
    #[instrument]
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
    /// #     .connect(&DeviceConfig::sample());
    /// # let mode = handle.events.await_mode(timeout).await.unwrap();
    /// let buf_id = handle.new_buffer(&mode);
    /// handle.request_update(buf_id, timeout).await?;
    /// let buf_data = handle.get_buffer(buf_id).expect("Buffer exists");
    /// # Ok::<(), Box<dyn Error>>(())
    /// # });
    /// ```
    ///
    /// Note: [`Handle::request_update`] happens to be implemented in such a way that it causes
    /// events available at the time it is called to be dispatched. Users should not rely on this.
    #[instrument]
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
        let handle_sys = self.sys;

        let buffer = self
            .buffers
            .get_mut(&buffer_id)
            .ok_or(RequestUpdateError::UnregisteredBuffer)?;

        let ready = unsafe {
            let span = span!(Level::INFO, "evdi_request_update", handle = ?handle_sys, ?buffer_id);
            let _enter = span.enter();
            evdi_request_update(handle_sys, buffer_id.sys())
        };

        if !ready {
            self.events.await_buffer_update(buffer_id, timeout).await?;
        }

        unsafe {
            let span = span!(Level::INFO, "evdi_grab_pixels", handle = ?handle_sys, ?buffer_id);
            let _enter = span.enter();
            evdi_grab_pixels(
                handle_sys as *mut evdi_device_context,
                buffer.rects.data_ptr_mut(),
                buffer.rects.len_ptr_mut() as _,
            )
        }

        buffer.mark_updated();

        Ok(())
    }

    #[instrument]
    pub fn enable_cursor_events(&self, enable: bool) {
        unsafe {
            evdi_enable_cursor_events(self.sys, enable);
        }
    }

    fn spawn_event_handler(
        handle_sys: evdi_handle,
        close_recv: crossbeam_channel::Receiver<()>,
        event_tx: mpsc::Sender<Event>,
        ready_fd: i32,
    ) {
        struct TransferWrapper(evdi_handle);
        // Safety: The intended usage seems to be to run this in another thread, so I think this
        //  is ok.
        unsafe impl Send for TransferWrapper {}
        unsafe impl Sync for TransferWrapper {}
        let handle_sys = TransferWrapper(handle_sys);

        /// Safety: tx was constructed from Box::leak
        fn send_event(tx: *mut c_void, event: Event) {
            let tx = unsafe {
                (tx as *mut mpsc::Sender<Event>)
                    .as_ref()
                    .expect("We never set user_data to nullptr")
            };

            debug!(?event, "Sending event");
            if let Err(err) = tx.blocking_send(event) {
                warn!(?err, ?tx, "Tried to send event but no receivers");
            };
        }

        extern "C" fn h_mode_changed(mode: evdi_mode, tx: *mut c_void) {
            send_event(tx, Event::ModeChanged(mode.into()));
        }
        extern "C" fn h_dpms(dpms_mode: c_int, tx: *mut c_void) {
            send_event(tx, Event::DpmsModeChanged(dpms_mode.into()));
        }
        extern "C" fn h_update_ready(buf: c_int, tx: *mut c_void) {
            send_event(tx, Event::UpdateReady(buf.into()));
        }
        extern "C" fn h_crtc_state(state: c_int, tx: *mut c_void) {
            send_event(tx, Event::CrtcStateChanged(state.into()));
        }
        extern "C" fn h_cursor_set(cursor_set: evdi_cursor_set, tx: *mut c_void) {
            send_event(tx, Event::CursorChange(cursor_set.into()));
        }
        extern "C" fn h_cursor_move(cursor_move: evdi_cursor_move, tx: *mut c_void) {
            send_event(tx, Event::CursorMove(cursor_move.into()));
        }
        extern "C" fn h_ddci_data(data: evdi_ddcci_data, tx: *mut c_void) {
            send_event(tx, Event::I2CRequest(data.into()));
        }

        tokio::task::spawn_blocking(move || {
            let event_tx = Box::leak(Box::new(event_tx));

            let mut pollfd = libc::pollfd {
                fd: ready_fd,
                events: libc::POLLIN,
                revents: 0,
            };

            loop {
                match close_recv.try_recv() {
                    Ok(()) | Err(crossbeam_channel::TryRecvError::Disconnected) => break,
                    Err(crossbeam_channel::TryRecvError::Empty) => {}
                }

                unsafe {
                    let ret = libc::poll(&mut pollfd, 1, 100);
                    if ret < 0 {
                        let err = io::Error::last_os_error();
                        error!("Error polling: {:?}", err);
                        continue;
                    }
                }

                match pollfd.revents {
                    libc::POLLHUP => {
                        warn!("POLLHUP");
                        break;
                    }
                    libc::POLLNVAL => {
                        warn!("POLLNVAL");
                        break;
                    }
                    libc::POLLERR => {
                        warn!("POLLERR");
                        break;
                    }
                    0 => {
                        // Timeout, just check if we should close
                        continue;
                    }
                    libc::POLLIN => {
                        // Pass through to handle events
                    }
                    revent => {
                        error!(revent, "Unexpected revent");
                        continue;
                    }
                }

                let mut ctx = evdi_event_context {
                    dpms_handler: Some(h_dpms),
                    mode_changed_handler: Some(h_mode_changed),
                    update_ready_handler: Some(h_update_ready),
                    crtc_state_handler: Some(h_crtc_state),
                    cursor_set_handler: Some(h_cursor_set),
                    cursor_move_handler: Some(h_cursor_move),
                    ddcci_data_handler: Some(h_ddci_data),
                    // Safety: We cast to a mut pointer, but we never cast back to a mut reference
                    user_data: event_tx as *mut mpsc::Sender<_> as *mut _,
                };

                unsafe { evdi_handle_events(handle_sys.0, &mut ctx) };
            }

            unsafe {
                drop(Box::from_raw(event_tx));
            }
        });
    }

    /// Disconnect the handle.
    ///
    /// A handle is automatically disconnected and closed on drop, you only need this if you want
    /// to keep the `UnconnectedHandle` around to potentially connect to later.
    #[instrument]
    pub fn disconnect(self) -> UnconnectedHandle {
        let sys = self.sys;
        let device = self.device.clone();

        // Avoid running the destructor, which would close the underlying handle
        // Since we are stack-allocated we still get cleaned up
        forget(self);

        unsafe { evdi_disconnect(sys) };

        UnconnectedHandle::new(device, sys)
    }

    /// Takes a handle that has just been connected
    fn new(device: DeviceNode, handle_sys: evdi_handle, device_config: &DeviceConfig) -> Self {
        let (close_event_handler, close_recv) = crossbeam_channel::bounded(1);
        let (event_tx, event_recv) = mpsc::channel(16);

        let ready_fd = unsafe { evdi_get_event_ready(handle_sys) };

        Self::spawn_event_handler(handle_sys, close_recv, event_tx, ready_fd);

        // select! {
        //     _ = sleep(Duration::from_millis(100)) => {
        //     // _ = sleep(ready_timeout) => {
        //         error!("Timed out waiting for handle ready");
        //         return Err(HandleConnect::Timeout);
        //     }
        //     _guard = ready_fd.readable()
        //         .instrument(span!(Level::INFO, "handle ready")) => {}
        // }

        Self {
            device,
            sys: handle_sys,
            device_config: device_config.to_owned(),
            buffers: HashMap::new(),
            events: HandleEvents::new(event_recv),
            close_event_handler,
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if self.close_event_handler.send(()).is_err() {
            error!(handle = ?self.sys, "Failed to send to close event loop channel");
        }

        unsafe {
            evdi_disconnect(self.sys);
            info!("evdi_disconnect");

            evdi_close(self.sys);
            info!("evdi_close");
        }
    }
}

impl PartialEq for Handle {
    fn eq(&self, other: &Self) -> bool {
        self.sys == other.sys
    }
}

impl Eq for Handle {}

#[derive(Debug, Error)]
pub enum RequestUpdateError {
    #[error("Kernel chose to update async, failed to await response")]
    AwaitUpdate(#[from] AwaitEventError),
    #[error("The buffer provided does not exist or is attached to a different handle")]
    UnregisteredBuffer,
}

#[cfg(test)]
pub mod tests {
    use std::fs::File;

    use crate::test_common::*;

    use super::*;

    #[ltest(atest)]
    async fn can_connect() {
        handle_fixture();
    }

    #[ltest(atest)]
    async fn can_enable_cursor_events() {
        handle_fixture().enable_cursor_events(true);
    }

    #[ltest(atest)]
    async fn update_can_be_called_multiple_times() {
        let mut handle = handle_fixture();

        let mode = handle.events.await_mode(TIMEOUT).await.unwrap();

        let buf_id = handle.new_buffer(&mode);

        for _ in 0..5 {
            handle.request_update(buf_id, TIMEOUT).await.unwrap();
        }
    }

    async fn get_update(handle: &mut Handle) -> &Buffer {
        let mode = handle.events.await_mode(TIMEOUT).await.unwrap();
        let buf_id = handle.new_buffer(&mode);

        // Give us some time to settle
        for _ in 0..5 {
            handle.request_update(buf_id, TIMEOUT).await.unwrap();
        }

        handle.get_buffer(buf_id).unwrap()
    }

    #[ltest(atest)]
    async fn bytes_is_non_empty() {
        let mut handle = handle_fixture();
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

    #[ltest(atest)]
    async fn can_output_debug() {
        let mut handle = handle_fixture();
        let buf = get_update(&mut handle).await;

        let mut f = File::create("TEMP_debug_rect.ppm").unwrap();

        buf.debug_write_to_ppm(&mut f).unwrap();
    }

    #[ltest(atest)]
    async fn can_disconnect() {
        let mut handle = handle_fixture();

        for _ in 0..10 {
            let unconnected = handle.disconnect();
            handle = unconnected.connect(&DeviceConfig::sample())
        }
    }

    #[ltest(atest)]
    async fn cannot_get_buffer_after_unregister() {
        let mut handle = handle_fixture();
        let mode = handle.events.await_mode(TIMEOUT).await.unwrap();

        let buf = handle.new_buffer(&mode);
        handle.unregister_buffer(buf);
        let res = handle.request_update(buf, TIMEOUT).await;
        assert!(matches!(
            res.unwrap_err(),
            RequestUpdateError::UnregisteredBuffer
        ));
    }
}
