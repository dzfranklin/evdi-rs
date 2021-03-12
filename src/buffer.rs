//! Buffer to receive virtual screen pixels

use crate::prelude::*;
use evdi_sys::*;
use rand::Rng;
use std::fs::File;
use std::io;
use std::io::Write;
use std::os::raw::{c_int, c_void};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub(crate) struct BufferId(i32);

impl BufferId {
    pub fn generate() -> Self {
        let id = rand::thread_rng().gen();
        Self::new(id)
    }

    pub(crate) fn new(id: i32) -> BufferId {
        BufferId(id)
    }

    pub(crate) fn sys(&self) -> i32 {
        self.0
    }
}

/// A buffer used to store the virtual screen pixels.
#[derive(Debug)]
pub struct Buffer {
    /// None if the buffer never been written to, otherwise Some(n) where n increases by some amount
    /// every time the buffer is written to.
    ///
    /// ```
    /// # use evdi::prelude::*;
    /// # use std::time::Duration;
    /// # let timeout = Duration::from_secs(1);
    /// # let mut handle = DeviceNode::get().unwrap().open().unwrap()
    /// #     .connect(&DeviceConfig::sample(), timeout).unwrap();
    /// # handle.request_events();
    /// # let mode = handle.receive_mode(timeout).unwrap();
    /// let mut buf = Buffer::new(&mode);
    ///
    /// assert!(buf.version.is_none());
    ///
    /// handle.request_update(&mut buf, timeout).unwrap();
    /// let after_first = buf.version;
    ///
    /// handle.request_update(&mut buf, timeout).unwrap();
    /// let after_second = buf.version;
    ///
    /// assert!(after_second.unwrap() > after_first.unwrap());
    /// ```
    pub version: Option<u32>,
    pub(crate) id: BufferId,
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
    pub fn new(mode: &Mode) -> Self {
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
            version: None,
            id: BufferId::generate(),
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
    /// Use [`Buffer::width`], [`Buffer::height`], and [`Buffer::stride`] to interpret this.
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

    /// Returns true if we weren't previously attached
    pub(crate) fn ensure_attached_to(
        &mut self,
        handle: evdi_handle,
    ) -> Result<bool, BufferAttachmentError> {
        if let Some(attached_to) = self.attached_to {
            if attached_to == handle {
                Ok(false)
            } else {
                Err(BufferAttachmentError::AlreadyToOther)
            }
        } else {
            self.attached_to = Some(handle);
            Ok(true)
        }
    }

    pub(crate) fn update_ready_sender(&self) -> Sender<()> {
        self.send_update_ready.clone()
    }

    pub(crate) fn block_until_update_ready(
        &self,
        timeout: Duration,
    ) -> Result<(), RecvTimeoutError> {
        self.update_ready.recv_timeout(timeout)
    }

    pub(crate) fn rects_ptr_sys(&self) -> *mut evdi_rect {
        self.rects.as_ptr() as *mut evdi_rect
    }

    pub(crate) fn rects_count_ptr_sys(&self) -> *mut c_int {
        &self.num_rects as *const _ as *mut c_int
    }

    pub(crate) fn sys(&self) -> evdi_buffer {
        evdi_buffer {
            id: self.id.0,
            buffer: self.buffer.as_ptr() as *mut c_void,
            width: self.width as c_int,
            height: self.height as c_int,
            stride: self.stride as c_int,
            rects: self.rects_ptr_sys() as *mut evdi_rect,
            rect_count: 0,
        }
    }

    pub(crate) fn mark_updated(&mut self) {
        self.version = if let Some(prev) = self.version {
            Some(prev + 1)
        } else {
            Some(0)
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

pub(crate) enum BufferAttachmentError {
    AlreadyToOther,
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::time::Duration;

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
    fn can_create_buffer() {
        let handle = handle_fixture();
        handle.request_events();
        let mode = handle.receive_mode(TIMEOUT).unwrap();
        Buffer::new(&mode);
    }

    #[test]
    fn can_access_buffer_sys() {
        let handle = handle_fixture();
        handle.request_events();
        let mode = handle.receive_mode(TIMEOUT).unwrap();
        Buffer::new(&mode).sys();
    }
}
