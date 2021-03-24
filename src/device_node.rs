//! Device node (`/dev/dri/card*`)
use std::cmp::Ordering;
use std::fs::File;
use std::io::Write;
use std::os::raw::c_uint;
use std::{fs, io};

use lazy_static::lazy_static;
use regex::Regex;
use thiserror::Error;
use tracing::instrument;

use crate::prelude::*;
use crate::{ensure_logs_setup, ffi};

const DEVICE_CARDS_DIR: &str = "/dev/dri";
const REMOVE_ALL_FILE: &str = "/sys/devices/evdi/remove_all";

/// Represents a device node (`/dev/dri/card*`).
#[derive(Debug, PartialEq, Eq)]
pub struct DeviceNode {
    pub(crate) id: i32,
}

impl DeviceNode {
    // NOTE: This contains the main entrypoints.
    // We must ensure we setup logs before we call any ffi.

    /// Returns an evdi device node if one is available.
    ///
    /// If no device is available you will need to run Device::add() with superuser permissions.
    pub fn get() -> Option<Self> {
        if let Ok(mut devices) = Self::list_available() {
            devices.pop()
        } else {
            None
        }
    }

    /// Check if a device node is an evdi device node, is a different device node, or doesn't exist.
    pub fn status(&self) -> DeviceNodeStatus {
        ensure_logs_setup();
        let sys = unsafe { ffi::evdi_check_device(self.id) };
        DeviceNodeStatus::from(sys)
    }

    /// Open an evdi device node.
    ///
    /// Returns None if we fail to open the device, which may be because the device isn't an evdi
    /// device node.
    pub fn open(&self) -> Result<UnconnectedHandle, OpenDeviceError> {
        ensure_logs_setup();

        // Provide helpful info if the call would have failed with Unknown
        match check_kernel_mod() {
            KernelModStatus::NotInstalled => return Err(OpenDeviceError::KernelModuleNotInstalled),
            KernelModStatus::Outdated => return Err(OpenDeviceError::KernelModuleOutdated),
            KernelModStatus::Compatible => (),
        }

        // NOTE: Opening invalid devices can be very slow (~10sec on my laptop), so we check first
        match self.status() {
            DeviceNodeStatus::Unrecognized => Err(OpenDeviceError::NotEvdiDevice),
            DeviceNodeStatus::NotPresent => Err(OpenDeviceError::NonexistentDevice),
            DeviceNodeStatus::Available => {
                let sys = unsafe { ffi::evdi_open(self.id) };
                if !sys.is_null() {
                    info!("Opened device {}", self.id);
                    Ok(UnconnectedHandle::new(sys))
                } else {
                    Err(OpenDeviceError::Unknown)
                }
            }
        }
    }

    /// List all evdi device nodes that have available status in a stable order
    #[instrument]
    pub fn list_available() -> io::Result<Vec<Self>> {
        lazy_static! {
            static ref RE: Regex = Regex::new(r"^card([0-9]+)$").unwrap();
        }

        let mut devices: Vec<DeviceNode> = vec![];

        for entry in fs::read_dir(DEVICE_CARDS_DIR)? {
            let name_os = entry?.file_name();
            let name = name_os.to_string_lossy();
            let id = RE
                .captures(&name)
                .and_then(|caps| caps.get(1))
                .and_then(|id| id.as_str().parse::<i32>().ok());

            if let Some(id) = id {
                devices.push(DeviceNode::new(id))
            }
        }

        let mut available: Vec<DeviceNode> = devices
            .into_iter()
            .filter(|device| device.status() == DeviceNodeStatus::Available)
            .collect();

        available.sort();

        Ok(available)
    }

    /// Tell the kernel module to create a new evdi device node.
    ///
    /// **Requires superuser permissions.**
    #[instrument]
    pub fn add() -> bool {
        ensure_logs_setup();
        let status = unsafe { ffi::evdi_add_device() };
        status > 0
    }

    /// Remove all evdi device nodes.
    ///
    /// **Requires superuser permissions.**
    #[instrument]
    pub fn remove_all() -> io::Result<()> {
        let mut f = File::with_options().write(true).open(REMOVE_ALL_FILE)?;
        f.write_all("1".as_ref())?;
        f.flush()?;
        Ok(())
    }

    /// Create a struct representing /dev/dri/card{id}.
    ///
    /// This does not create the device or check if the device exists.
    pub fn new(id: i32) -> Self {
        DeviceNode { id }
    }
}

impl Ord for DeviceNode {
    fn cmp(&self, other: &Self) -> Ordering {
        self.id.cmp(&other.id)
    }
}

impl PartialOrd for DeviceNode {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Status of a [`DeviceNode`]
#[derive(Debug, PartialEq)]
pub enum DeviceNodeStatus {
    Available,
    Unrecognized,
    NotPresent,
}

#[derive(Debug, Error)]
pub enum OpenDeviceError {
    #[error("The kernel module evdi is not installed")]
    KernelModuleNotInstalled,
    #[error("The kernel module is too outdated to use with this library version")]
    KernelModuleOutdated,
    #[error("The device node does not exist")]
    NonexistentDevice,
    #[error("The device node is not an evdi device node")]
    NotEvdiDevice,
    #[error("The call to the c library failed. Maybe the device node is an incompatible version?")]
    Unknown,
}

impl DeviceNodeStatus {
    /// * `sys` - evdi_device_status
    ///
    /// Panics on unrecognized sys.
    fn from(sys: c_uint) -> Self {
        match sys {
            ffi::EVDI_STATUS_AVAILABLE => DeviceNodeStatus::Available,
            ffi::EVDI_STATUS_UNRECOGNIZED => DeviceNodeStatus::Unrecognized,
            ffi::EVDI_STATUS_NOT_PRESENT => DeviceNodeStatus::NotPresent,
            _ => panic!("Invalid device status {}", sys),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_common::*;

    #[ltest]
    fn default_device_is_not_evdi() {
        let status = DeviceNode::new(0).status();
        assert_eq!(status, DeviceNodeStatus::Unrecognized);
    }

    #[ltest]
    fn nonexistent_device_has_proper_status() {
        let status = DeviceNode::new(4200).status();
        assert_eq!(status, DeviceNodeStatus::NotPresent);
    }

    #[ltest]
    fn add_fails_without_superuser() {
        let result = DeviceNode::add();
        assert_eq!(result, false);
    }

    #[ltest]
    fn remove_all_fails_without_superuser() {
        let result = DeviceNode::remove_all();
        assert!(result.is_err())
    }

    #[ltest]
    fn list_available_contains_at_least_one_device() {
        let results = DeviceNode::list_available().unwrap();
        assert!(!results.is_empty(), "No available devices. Have you added at least one device by running the binary add_device at least once?");
    }

    #[ltest]
    fn get_returns_a_device() {
        let result = DeviceNode::get();
        assert!(result.is_some());
    }

    #[ltest]
    fn can_open() {
        let device = DeviceNode::get().unwrap();
        device.open().unwrap();
    }

    #[ltest]
    fn opening_nonexistent_device_fails() {
        let device = DeviceNode::new(4200);
        match device.open() {
            Err(OpenDeviceError::NonexistentDevice) => (),
            _ => panic!(),
        }
    }

    #[ltest]
    fn opening_non_evdi_device_fails() {
        let device = DeviceNode::new(0);
        match device.open() {
            Err(OpenDeviceError::NotEvdiDevice) => (),
            _ => panic!(),
        }
    }
}
