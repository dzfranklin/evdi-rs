use std::cmp::Ordering;
use std::fs::File;
use std::io::Write;
use std::os::raw::c_uint;
use std::{fs, io};

use evdi_sys::*;
use lazy_static::lazy_static;
use regex::Regex;

use crate::handle::UnconnectedHandle;

const DEVICE_CARDS_DIR: &str = "/dev/dri";
const REMOVE_ALL_FILE: &str = "/sys/devices/evdi/remove_all";

/// Represents a /dev/dri/card*.
#[derive(Debug, PartialEq, Eq)]
pub struct Device {
    id: i32,
}

impl Device {
    /// Returns a device if one is available.
    ///
    /// If no device is available you will need to run Device::add() with superuser permissions.
    pub fn get() -> Option<Self> {
        if let Ok(mut devices) = Self::list_available() {
            devices.pop()
        } else {
            None
        }
    }

    /// Get the status of a device.
    pub fn status(&self) -> DeviceStatus {
        let sys = unsafe { evdi_check_device(self.id) };
        DeviceStatus::from(sys)
    }

    /// Open a device.
    pub fn open(&self) -> UnconnectedHandle {
        let sys = unsafe { evdi_open(self.id) };
        UnconnectedHandle::new(sys)
    }

    /// List all devices that have available status in a stable order
    pub fn list_available() -> io::Result<Vec<Self>> {
        lazy_static! {
            static ref RE: Regex = Regex::new(r"^card([0-9]+)$").unwrap();
        }

        let mut devices: Vec<Device> = vec![];

        for entry in fs::read_dir(DEVICE_CARDS_DIR)? {
            let name_os = entry?.file_name();
            let name = name_os.to_string_lossy();
            let id = RE
                .captures(&name)
                .and_then(|caps| caps.get(1))
                .and_then(|id| id.as_str().parse::<i32>().ok());

            if let Some(id) = id {
                devices.push(Device::new(id))
            }
        }

        let mut available: Vec<Device> = devices
            .into_iter()
            .filter(|device| device.status() == DeviceStatus::Available)
            .collect();

        available.sort();

        Ok(available)
    }

    /// Tell the kernel module to create a new device.
    ///
    /// Requires superuser permissions.
    pub fn add() -> bool {
        let status = unsafe { evdi_add_device() };
        status > 0
    }

    /// Remove all devices.
    ///
    /// Requires superuser permissions.
    pub fn remove_all() -> io::Result<()> {
        let mut f = File::with_options().write(true).open(REMOVE_ALL_FILE)?;
        f.write_all("1".as_ref())?;
        Ok(())
    }

    /// Create a struct representing /dev/dri/card{id}.
    ///
    /// This does not create the device or check if the device exists.
    pub fn new(id: i32) -> Self {
        Device { id }
    }
}

impl Ord for Device {
    fn cmp(&self, other: &Self) -> Ordering {
        self.id.cmp(&other.id)
    }
}

impl PartialOrd for Device {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Status of a [Device]
#[derive(Debug, PartialEq)]
pub enum DeviceStatus {
    Available,
    Unrecognized,
    NotPresent,
}

impl DeviceStatus {
    /// * `sys` - evdi_device_status
    ///
    /// Panics on unrecognized sys.
    fn from(sys: c_uint) -> Self {
        match sys {
            EVDI_STATUS_AVAILABLE => DeviceStatus::Available,
            EVDI_STATUS_UNRECOGNIZED => DeviceStatus::Unrecognized,
            EVDI_STATUS_NOT_PRESENT => DeviceStatus::NotPresent,
            _ => panic!("Invalid device status {}", sys),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_device_is_not_evdi() {
        let status = Device::new(0).status();
        assert_eq!(status, DeviceStatus::Unrecognized);
    }

    #[test]
    fn nonexistent_device_has_proper_status() {
        let status = Device::new(4200).status();
        assert_eq!(status, DeviceStatus::NotPresent);
    }

    #[test]
    fn add_fails_without_superuser() {
        let result = Device::add();
        assert_eq!(result, false);
    }

    #[test]
    fn remove_all_fails_without_superuser() {
        let result = Device::remove_all();
        assert!(result.is_err())
    }

    #[test]
    fn list_available_contains_at_least_one_device() {
        let results = Device::list_available().unwrap();
        assert!(!results.is_empty(), "No available devices. Have you added at least one device by running the binary add_device at least once?");
    }

    #[test]
    fn get_returns_a_device() {
        let result = Device::get();
        assert!(result.is_some());
    }

    #[test]
    fn can_open() {
        let device = Device::get().unwrap();
        device.open();
    }
}
