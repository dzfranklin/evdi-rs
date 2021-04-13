// Hardcoded values copied from my laptop (System76 Darter Pro)
// See Wikipedia <https://en.wikipedia.org/wiki/Extended_Display_Identification_Data#EDID_1.4_data_format>
// and the spec <https://glenwing.github.io/docs/VESA-EEDID-A2.pdf>

fn header() -> [u8; 19] {
    const MAGIC_NO: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];
    const MFG_ID: [u8; 2] = [0x30, 0xe4]; // LGD (LG Displays)
    const MFG_PRODUCT_CODE: [u8; 2] = [0xe5, 0x05];
    const SERIAL_NO: [u8; 4] = [0x00, 0x00, 0x00, 0x00];
    const WEEK_OF_MFG: [u8; 1] = [0x00];
    const YEAR_OF_MFG: [u8; 1] = [0x1c];
    const EDID_VERSION: [u8; 2] = [0x01, 0x04]; // 1.4
    [0; 19] // TODO
}

struct ScreenSizeCm {
    horizontal: u8,
    vertical: u8,
}

fn basic_display_params(size: ScreenSizeCm, gamma: u8) -> [u8; 5] {
    // Bitmask. This indicates digital=true, bit depth=reserved, video interface=DisplayPort
    const VIDEO_PARAMS: u8 = 0b_1_111_0101;

    // Bitmask. This indicates dpms standby=false,dpms suspend=false,dpms active-off=false,
    // display type=RGB 4:4:4 + YCrCb 4:2:2,standard sRGB=false, timing mode includes native pixel
    // format and refresh rate=true, continuous timings with GTF or CVT=false
    const SUPPORTED_FEATURES: u8 = 0b_000_00_0_1_0;

    [
        VIDEO_PARAMS,
        size.horizontal,
        size.vertical,
        gamma,
        SUPPORTED_FEATURES,
    ]
}

// CIE 1931 xy coordinates for red, green, blue, and white point.
// On Windows, get with https://docs.microsoft.com/en-us/windows/win32/wmicoreprov/wmimonitorcolorcharacteristics
struct ChromaticityCoords {
    red: ChromaticityCoord,
    green: ChromaticityCoord,
    blue: ChromaticityCoord,
    default_white_point: ChromaticityCoord,
}

struct ChromaticityCoord {
    x: f32,
    y: f32,
}

fn chromaticity_coords(data: ChromaticityCoords) -> [u8; 10] {
    let [rxm, rxl] = bin_frac(data.red.x);
    let [rym, ryl] = bin_frac(data.red.y);
    let [gxm, gxl] = bin_frac(data.green.x);
    let [gym, gyl] = bin_frac(data.green.y);
    let [bxm, bxl] = bin_frac(data.blue.x);
    let [bym, byl] = bin_frac(data.blue.y);
    let [wxm, wxl] = bin_frac(data.default_white_point.x);
    let [wym, wyl] = bin_frac(data.default_white_point.y);

    [
        rxl | ryl << 2 | gxl << 4 | gyl << 6,
        bxl | byl << 2 | wxl << 4 | wyl << 6,
        rxm,
        rym,
        gxm,
        gym,
        bxm,
        bym,
        wxm,
        wym,
    ]
}

/// Computes 10-bit binary fraction representation per section 3.7 of spec
fn bin_frac(f: f32) -> [u8; 2] {
    let n = (f * 1024_f32).round() as u16;
    let msb = (n >> 2) as u8; // most significant 8 bits
    let lsb = (n & 0b00_000_00011) as u8; // least significant 2 bits
    [msb, lsb]
}

fn timing() {
    const ESTABLISHED_TIMING_BITMAP: [u8; 3] = [0, 0, 0]; // no longer commonly used
    const STD_TIMING: [u8; 15] = [0x01; 15]; // unused marker

    // Detailed timing descriptors. I'm just copying my laptop, I don't think this is hugely
    // important for our purposes
    const DESCRIPTOR_1: [u8; 18] = [
        0b00100100, 0b00110110, 0b10000000, 0b10100000, 0b01110000, 0b00111000, 0b00011111,
        0b01000000, 0b00110000, 0b00100000, 0b00110101, 0b00000000, 0b01011000, 0b11000010,
        0b00010000, 0b00000000, 0b00000000, 0b00011001,
    ];

    // [&ESTABLISHED_TIMING_BITMAP].concat();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bin_frac_correctly_converts_examples_in_spec() {
        assert_eq!(bin_frac(0.610), [0b10011100, 0b01]);
        assert_eq!(bin_frac(0.307), [0b01001110, 0b10]);
        assert_eq!(bin_frac(0.150), [0b00100110, 0b10]);
    }
}
