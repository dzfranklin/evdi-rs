//! Config of virtual output display

use derivative::Derivative;

/// Describes a virtual display to output to
#[derive(Derivative)]
#[derivative(Debug, Clone)]
pub struct DeviceConfig {
    #[derivative(Debug = "ignore")]
    edid: Vec<u8>,
    pub width_pixels: u32,
    pub height_pixels: u32,
}

impl DeviceConfig {
    /// A valid config that can be used in testing.
    pub fn sample() -> Self {
        Self::new(include_bytes!("sample_edid_1280_800"), 1280, 800)
    }

    /// Create a config.
    ///
    /// `edid` is the bytes of an [Extended Display Identification Data][edid_wiki]
    ///
    /// [edid_wiki]: https://en.wikipedia.org/wiki/Extended_Display_Identification_Data
    pub fn new<B: AsRef<[u8]>>(edid: B, width_pixels: u32, height_pixels: u32) -> Self {
        Self {
            edid: edid.as_ref().to_owned(),
            width_pixels,
            height_pixels,
        }
    }

    pub fn edid(&self) -> &[u8] {
        &self.edid
    }

    pub(crate) fn sku_area_limit(&self) -> u32 {
        self.width_pixels * self.height_pixels
    }
}
