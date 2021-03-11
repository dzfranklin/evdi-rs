use std::os::raw::c_uint;

use evdi_sys::*;

use crate::device_config::DeviceConfig;

/// Automatically disconnected on drop
#[derive(Debug)]
pub struct Handle {
    handle: evdi_handle
}

impl Handle {
    }

    fn new(handle: evdi_handle) -> Self {
        Self { handle }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { evdi_close(self.handle) }
    }
}

/// Automatically closed on drop
#[derive(Debug)]
pub struct UnconnectedHandle {
    handle: evdi_handle
}

impl UnconnectedHandle {
    /// Connect to an handle.
    pub fn connect(self, config: &DeviceConfig) -> Handle {
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

        Handle::new(self.handle)
    }

    pub(crate) fn new(handle: evdi_handle) -> Self {
        Self { handle }
    }
}

#[cfg(test)]
mod tests {
    use crate::device::Device;
    use crate::device_config::DeviceConfig;
    use lazy_static::lazy_static;

    lazy_static! {
        static ref SAMPLE_CONFIG: DeviceConfig = DeviceConfig::new(
            include_bytes!("sample_edid_1280_800"),
            1280,
            800
        );
    }

    #[test]
    fn can_connect() {
        Device::get().unwrap()
            .open()
            .connect(&SAMPLE_CONFIG);
    }
}
