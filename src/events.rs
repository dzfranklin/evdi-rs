use std::convert::{TryFrom, TryInto};
use std::os::raw::c_int;
use std::sync::Arc;
use std::time::Duration;

use derivative::Derivative;
use drm_fourcc::{DrmFormat, UnrecognizedFourcc};
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::time::sleep;

use crate::ffi::evdi_cursor_set;
use crate::prelude::*;
use crate::{ffi, OwnedLibcArray};

macro_rules! try_send {
    ($tx:expr, $val:expr) => {
        if $tx.send($val).is_err() {
            warn!("Failed to send: Channel closed");
        }
    };
}

#[derive(Derivative)]
#[derivative(Debug)]
pub struct HandleEvents {
    #[derivative(Debug = "ignore")]
    current_mode: watch::Receiver<Option<Mode>>,
    #[derivative(Debug = "ignore")]
    buffer_updates: broadcast::Sender<BufferId>,
}

impl HandleEvents {
    /// To receive a buffer update you must have been waiting before it was sent by the kernel
    #[instrument]
    pub(crate) async fn await_buffer_update(
        &self,
        id: BufferId,
        timeout: Duration,
    ) -> Result<(), AwaitEventError> {
        tokio::select! {
            _ = sleep(timeout) => {
                warn!("Timeout awaiting buffer update: {:?}", timeout);
                Err(AwaitEventError::Timeout)
            }
            result = self.await_buffer_update_unbounded(id) => result
        }
    }

    async fn await_buffer_update_unbounded(&self, id: BufferId) -> Result<(), AwaitEventError> {
        use broadcast::error::RecvError;
        let mut recv = self.buffer_updates.subscribe();
        loop {
            match recv.recv().await {
                Ok(potential) => {
                    if potential == id {
                        return Ok(());
                    }
                }
                Err(RecvError::Closed) => return Err(AwaitEventError::ChannelClosed),
                Err(RecvError::Lagged(skipped)) => {
                    warn!(skipped, "Lagged receiving buffer update");
                }
            }
        }
    }

    /// The most recent mode received, if a mode has been received.
    pub fn current_mode(&self) -> Option<Mode> {
        *self.current_mode.borrow()
    }

    /// If a mode event has been received, return it. Otherwise wait for a mode event.
    #[instrument]
    pub async fn await_mode(&self, timeout: Duration) -> Result<Mode, AwaitEventError> {
        if let Some(cached) = self.current_mode() {
            return Ok(cached);
        }

        let mut recv = self.current_mode.clone();
        tokio::select! {
            _ = sleep(timeout) => {
                warn!("Timeout awaiting mode: {:?}", timeout);
                Err(AwaitEventError::Timeout)
            },
            ret = recv.changed() => {
                if ret.is_err() {
                    error!("Mode watch channel closed");
                    Err(AwaitEventError::ChannelClosed)
                } else {
                    let mode = self.current_mode().expect("Can never be none after being changed");
                    Ok(mode)
                }
            }
        }
    }

    fn spawn_monitor(
        mut recv: mpsc::Receiver<Event>,
        current_mode_tx: watch::Sender<Option<Mode>>,
        buffer_updates_tx: broadcast::Sender<BufferId>,
    ) {
        tokio::spawn(async move {
            loop {
                match recv.recv().await {
                    Some(event) => match event {
                        Event::ModeChanged(mode) => try_send!(current_mode_tx, Some(mode)),
                        Event::UpdateReady(id) => try_send!(buffer_updates_tx, id),
                        _ => (),
                    },
                    None => {
                        warn!("Event channel closed");
                        return;
                    }
                }
            }
        });
    }

    pub(crate) fn new(recv: mpsc::Receiver<Event>) -> Self {
        let (current_mode_tx, current_mode) = watch::channel(None);
        let (buffer_updates, _) = broadcast::channel(16);

        Self::spawn_monitor(recv, current_mode_tx, buffer_updates.clone());

        Self {
            current_mode,
            buffer_updates,
        }
    }
}

#[derive(Debug, Error)]
pub enum AwaitEventError {
    #[error("Timeout")]
    Timeout,
    #[error("Event channel closed")]
    ChannelClosed,
}

#[derive(Clone, Debug)]
pub(crate) enum Event {
    DpmsModeChanged(DpmsMode),
    ModeChanged(Mode),
    /// An update for a buffer requested earlier is ready to be consumed.
    UpdateReady(BufferId),
    CrtcStateChanged(CrtcState),
    CursorChange(CursorChange),
    CursorMove(CursorMove),
    I2CRequest(DdcCiData),
}

#[derive(Debug, Copy, Clone)]
#[cfg_attr(
    feature = "serde",
    derive(serde_crate::Serialize, serde_crate::Deserialize),
    serde(crate = "serde_crate")
)]
pub enum DpmsMode {
    // Values from <https://displaylink.github.io/evdi/details/#dpms-mode-change>
    On = 0,
    Standby = 1,
    Suspend = 2,
    Off = 3,
}

impl From<c_int> for DpmsMode {
    fn from(sys: i32) -> Self {
        match sys {
            0 => DpmsMode::On,
            1 => DpmsMode::Standby,
            2 => DpmsMode::Suspend,
            3 => DpmsMode::Off,
            _ => panic!("Unexpected DPMS mode {}", sys),
        }
    }
}

#[derive(Debug, Copy, Clone)]
#[cfg_attr(
    feature = "serde",
    derive(serde_crate::Serialize, serde_crate::Deserialize),
    serde(crate = "serde_crate")
)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    /// Max updates per second.
    pub refresh_rate: u32,
    pub bits_per_pixel: u32,
    pub pixel_format: Result<DrmFormat, UnrecognizedFourcc>,
}

impl From<ffi::evdi_mode> for Mode {
    fn from(sys: ffi::evdi_mode) -> Self {
        Self {
            width: sys.width as u32,
            height: sys.height as u32,
            refresh_rate: sys.refresh_rate as u32,
            bits_per_pixel: sys.bits_per_pixel as u32,
            pixel_format: sys.pixel_format.try_into(),
        }
    }
}

/// A value [forwarded from the kernel][u-doc].
///
/// [u-doc]: https://displaylink.github.io/evdi/details/#crtc-state-change
#[derive(Debug, Clone)]
#[cfg_attr(
    feature = "serde",
    derive(serde_crate::Serialize, serde_crate::Deserialize),
    serde(crate = "serde_crate")
)]
pub struct CrtcState(pub i32);

impl From<c_int> for CrtcState {
    fn from(sys: i32) -> Self {
        Self(sys)
    }
}

/// This notification is sent for an update of cursor buffer or shape. It is also raised when
/// cursor is enabled or disabled (when cursor is moved on and off the screen).
#[derive(Debug, Clone)]
pub struct CursorChange {
    pub enabled: bool,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub pixel_format: Result<DrmFormat, UnrecognizedFourcc>,
    buffer: Arc<OwnedLibcArray<u32>>,
}

impl From<ffi::evdi_cursor_set> for CursorChange {
    fn from(sys: evdi_cursor_set) -> Self {
        let buffer = unsafe { OwnedLibcArray::new(sys.buffer, sys.buffer_length as usize) };

        Self {
            enabled: sys.enabled != 0,
            hotspot_x: sys.hot_x,
            hotspot_y: sys.hot_y,
            width: sys.width,
            height: sys.height,
            stride: sys.stride,
            pixel_format: DrmFormat::try_from(sys.pixel_format),
            buffer: Arc::new(buffer),
        }
    }
}

impl CursorChange {
    pub fn buffer(&self) -> &[u32] {
        self.buffer.as_slice()
    }
}

/// A cursor position change. Raised only when cursor is positioned on virtual screen.
#[derive(Debug, Copy, Clone)]
#[cfg_attr(
    feature = "serde",
    derive(serde_crate::Serialize, serde_crate::Deserialize),
    serde(crate = "serde_crate")
)]
pub struct CursorMove {
    pub x: i32,
    pub y: i32,
}

impl From<ffi::evdi_cursor_move> for CursorMove {
    fn from(sys: ffi::evdi_cursor_move) -> Self {
        Self { x: sys.x, y: sys.y }
    }
}

/// An i2c request has been made to the DDC/CI address (0x37).
//
// The kernel module will wait for a maximum of DDCCI_TIMEOUT_MS (50ms - The default DDC request
// timeout) for a response to this request to be passed back via evdi_ddcci_response.
#[derive(Debug, Clone)]
pub struct DdcCiData {
    flags: u16,
    buffer: Arc<OwnedLibcArray<u8>>,
}

impl From<ffi::evdi_ddcci_data> for DdcCiData {
    fn from(sys: ffi::evdi_ddcci_data) -> Self {
        // Ignore address, as per docs will always be 0x37.
        let buffer = unsafe { OwnedLibcArray::new(sys.buffer, sys.buffer_length as usize) };
        Self {
            flags: sys.flags,
            buffer: Arc::new(buffer),
        }
    }
}

/// DDC/CI data notification
///
/// All documentation of DdciData is my best guess based on reading the source and googling.
/// You may find reading about [general protocol i2c][i2c] that DDC/CI data is transmitted over
/// useful.
///
/// [i2c]: <http://ww1.microchip.com/downloads/en/AppNotes/I2C-Master-Mode-30003191A.pdf>
impl DdcCiData {
    /// Indicates driver intends to read data from the virtual display
    pub fn flag_read_request(&self) -> bool {
        self.flags & Self::FLAG_READ != 0
    }

    /// Indicates driver intends to write data to the virtual display
    pub fn flag_write_request(&self) -> bool {
        self.flags == Self::FLAG_WRITE
    }

    // Truncated to 64 bytes (DDCCI_BUFFER_SIZE)
    pub fn buffer(&self) -> &[u8] {
        self.buffer.as_slice()
    }

    const FLAG_READ: u16 = 1;
    const FLAG_WRITE: u16 = 0;
}

#[cfg(test)]
mod tests {
    use drm_fourcc::DrmFormat;

    use crate::ffi::evdi_mode;
    use crate::test_common::*;

    use super::*;

    #[ltest(atest)]
    async fn can_receive_mode() {
        let handle = handle_fixture();
        let mode = handle.events.await_mode(TIMEOUT).await.unwrap();
        assert!(mode.height > 100);
    }

    #[ltest]
    fn mode_from_sample() {
        let expected = Mode {
            width: 1280,
            height: 800,
            refresh_rate: 60,
            bits_per_pixel: 32,
            pixel_format: Ok(DrmFormat::Xrgb8888),
        };

        let actual: Mode = evdi_mode {
            width: 1280,
            height: 800,
            refresh_rate: 60,
            bits_per_pixel: 32,
            pixel_format: 875713112,
        }
        .into();

        assert_eq!(format!("{:?}", actual), format!("{:?}", expected));
    }
}
