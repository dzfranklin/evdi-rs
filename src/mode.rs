use std::convert::TryInto;

use drm_fourcc::{DrmFormat, UnrecognizedFourcc};
use evdi_sys::evdi_mode;

#[derive(Debug, Copy, Clone)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    /// Max updates per second. You shouldn't call [`crate::handle::Handle::request_update`] faster
    /// than this.
    pub refresh_rate: u32,
    pub bits_per_pixel: u32,
    pub pixel_format: Result<DrmFormat, UnrecognizedFourcc>,
}

impl From<evdi_mode> for Mode {
    fn from(sys: evdi_mode) -> Self {
        Self {
            width: sys.width as u32,
            height: sys.height as u32,
            refresh_rate: sys.refresh_rate as u32,
            bits_per_pixel: sys.bits_per_pixel as u32,
            pixel_format: sys.pixel_format.try_into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::fii::evdi_mode;
    use crate::prelude::*;
    use drm_fourcc::DrmFormat;

    #[test]
    fn from_sample() {
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
