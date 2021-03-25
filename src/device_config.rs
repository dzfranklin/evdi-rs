//! Config of virtual output display

use derivative::Derivative;

/// Describes a virtual display to output to. The EDID must match width_pixels and height_pixels.
#[derive(Derivative)]
#[derivative(Debug, Clone)]
#[cfg_attr(
    feature = "serde",
    derive(serde_crate::Serialize, serde_crate::Deserialize),
    serde(crate = "serde_crate")
)]
pub struct DeviceConfig {
    #[derivative(Debug = "ignore")]
    edid: Vec<u8>,
    pub width_pixels: u32,
    pub height_pixels: u32,
}

impl DeviceConfig {
    /// A valid config that can be used in testing that came from the monitor in my laptop.
    pub fn sample() -> Self {
        Self::new(
            include_bytes!("../sample_data/sample_edid_darp_1920_1080.bin"),
            1920,
            1080,
        )
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
