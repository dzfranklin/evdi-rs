//! Buffer to receive virtual screen pixels

use std::fs::File;
use std::io;
use std::io::Write;
use std::os::raw::{c_int, c_void};

use drm_fourcc::UnrecognizedFourcc;
use evdi_sys::*;
use rand::Rng;

use crate::prelude::*;
use tokio::sync::broadcast;

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct BufferId(i32);

impl BufferId {
    pub(crate) fn generate() -> Self {
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
    /// # use std::error::Error;
    /// # tokio_test::block_on(async {
    /// let timeout = Duration::from_secs(1);
    /// # let mut handle = DeviceNode::get().expect("At least on evdi device available").open()?
    /// #     .connect(&DeviceConfig::sample(), timeout)
    /// #     .await?;
    /// # handle.dispatch_events();
    /// # handle.events.mode.changed().await?;
    /// # let mode = handle.events.mode.borrow().unwrap();
    /// let buf_id = handle.new_buffer(&mode);
    ///
    /// assert!(handle.get_buffer(buf_id).expect("Buffer exists").version.is_none());
    ///
    /// handle.request_update(buf_id, timeout).await?;
    /// let after_first = handle
    ///     .get_buffer(buf_id).expect("buffer exists")
    ///     .version.expect("Buffer must have been updated");
    ///
    /// handle.request_update(buf_id, timeout).await?;
    /// let after_second = handle
    ///     .get_buffer(buf_id).expect("buffer exists")
    ///     .version.expect("Buffer must have been updated");
    ///
    /// assert!(after_second > after_first);
    /// # Ok::<(), Box<dyn Error>>(())
    /// # });
    /// ```
    pub version: Option<u32>,
    pub(crate) id: BufferId,
    pub(crate) update_ready: broadcast::Receiver<()>,
    pub(crate) send_update_ready: broadcast::Sender<()>,
    buffer: Box<[u8]>,
    rects: Box<[evdi_rect]>,
    num_rects: i32,
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub pixel_format: Result<DrmFormat, UnrecognizedFourcc>,
}

/// Can't have more than 16
/// see <https://displaylink.github.io/evdi/details/#grabbing-pixels>
const MAX_RECTS_BUFFER_LEN: usize = 16;

const BGRA_DEPTH: usize = 4;

/// Owned and created by [`Handle`]s. The general flow is
///
/// ```
/// # use evdi::prelude::*;
/// # use std::time::Duration;
/// # use std::error::Error;
/// # tokio_test::block_on(async {
/// # let timeout = Duration::from_secs(1);
/// # let mut handle = DeviceNode::get().expect("At least on evdi device available").open()?
/// #     .connect(&DeviceConfig::sample(), timeout).await?;
/// # handle.dispatch_events();
/// # handle.events.mode.changed().await?;
/// # let mode = handle.events.mode.borrow().unwrap();
/// #
/// let buffer_id: BufferId = handle.new_buffer(&mode);
/// let buffer_data: &Buffer = handle.get_buffer(buffer_id).expect("Buffer exists");
///
/// handle.unregister_buffer(buffer_id);
/// assert!(handle.get_buffer(buffer_id).is_none());
/// # Ok::<(), Box<dyn Error>>(())
/// # });
/// ```
impl Buffer {
    /// Get a reference to the underlying bytes of this buffer.
    ///
    /// Use [`Buffer::width`], [`Buffer::height`], [`Buffer::stride`], and [`Buffer::pixel_format`]
    /// to interpret this.
    pub fn bytes(&self) -> &[u8] {
        self.buffer.as_ref()
    }

    /// Write the pixels to a file in the unoptimized image format [PPM].
    ///
    /// This is useful when debugging, as you can open the file in an image viewer and see if the
    /// buffer is processed correctly.
    ///
    /// Panics: If the pixel format isn't Xbgr8888, to simplify the implementation. This function
    /// should therefore only be used as a debug helper where you can guarantee that your kernel
    /// won't output in a different format.
    ///
    /// [PPM]: http://netpbm.sourceforge.net/doc/ppm.html
    pub fn debug_write_to_ppm(&self, f: &mut File) -> io::Result<()> {
        assert_eq!(
            self.pixel_format.expect("Unrecognized pixel format"),
            DrmFormat::Xrgb8888,
            "Only xbgr8888 pixel format supported by debug_write_to_ppm"
        );
        Self::write_line(f, "P6\n")?;
        Self::write_line(f, format!("{}\n", self.width.to_string()))?;
        Self::write_line(f, format!("{}\n", self.height.to_string()))?;
        Self::write_line(f, "255\n")?;

        for stride in self.buffer.chunks_exact(self.stride) {
            let row = &stride[0..self.width];
            for pixel in row.chunks_exact(BGRA_DEPTH) {
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

    /// Allocate a buffer to store the screen of a device with a specific mode.
    pub(crate) fn new(mode: &Mode) -> Self {
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

        let (send_update_ready, update_ready) = broadcast::channel(10);

        Buffer {
            version: None,
            id: BufferId::generate(),
            update_ready,
            send_update_ready,
            buffer,
            rects,
            num_rects: -1,
            width,
            height,
            stride,
            pixel_format: mode.pixel_format,
        }
    }
}

#[cfg(test)]
pub mod tests {
    use crate::handle::tests::handle_fixture;

    #[tokio::test]
    async fn can_create_buffer() {
        let mut handle = handle_fixture().await;
        handle.dispatch_events();
        handle.events.mode.changed().await.unwrap();
        let mode = handle.events.mode.borrow().unwrap();
        handle.new_buffer(&mode);
    }

    #[tokio::test]
    async fn can_access_buffer_sys() {
        let mut handle = handle_fixture().await;
        handle.dispatch_events();
        handle.events.mode.changed().await.unwrap();
        let mode = handle.events.mode.borrow().unwrap();
        handle.new_buffer(&mode).sys();
    }
}
