#[derive(Debug, Clone)]
pub struct DeviceConfig {
    edid: Vec<u8>,
    width_pixels: u32,
    height_pixels: u32,
}

impl DeviceConfig {
    pub fn sample() -> Self {
        Self::new(
            include_bytes!("sample_edid_1280_800"),
            1280,
            800,
        )
    }

    pub fn new(edid: &[u8], width_pixels: u32, height_pixels: u32) -> Self {
        Self { edid: edid.to_owned(), width_pixels, height_pixels }
    }

    pub fn edid(&self) -> &[u8] {
        &self.edid
    }

    pub fn sku_area_limit(&self) -> u32 {
        self.width_pixels * self.height_pixels
    }
}
