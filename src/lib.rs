#![feature(with_options)]
#![feature(try_trait)]

//! High-level bindings to [evdi](https://github.com/DisplayLink/evdi), a library for managing
//! virtual displays on linux.
//!
//! This library is alpha quality. If your display starts behaving weirdly, rebooting may help.
//!
//! The underlying library this wraps handles errors loosly. Many errors are handled by logging
//! a message and continuing. This wrapper only adds error information when doing so is easy. Normal
//! usage of this api may lead to silent failures or crashes.
//!
//! ## Basic usage
//!
//! ```
//! # use std::error::Error;
//! # use std::time::Duration;
//! # use evdi::prelude::*;
//! #
//! const READY_TIMEOUT: Duration = Duration::from_secs(1);
//! const RECEIVE_INITIAL_MODE_TIMEOUT: Duration = Duration::from_secs(1);
//! const UPDATE_BUFFER_TIMEOUT: Duration = Duration::from_millis(100);
//!
//! # fn main() -> Result<(), Box<dyn Error + Sync + Send>> {
//! // If get returns None you need to call DeviceNode::add with superuser permissions.
//! let device = DeviceNode::get().unwrap();
//!
//! // Replace this with the details of the display you want to emulate
//! let device_config = DeviceConfig::sample();
//!
//! let unconnected_handle = device.open()?;
//! let mut handle = unconnected_handle.connect(&device_config, READY_TIMEOUT)?;
//!
//! // For simplicity don't handle mode changed events in this example
//! handle.request_events();
//! let mode = handle.receive_mode(RECEIVE_INITIAL_MODE_TIMEOUT)?;
//!
//! // For simplicity, we only use one buffer. You may want to use more than one buffer so that you
//! // can send the contents of one buffer while updating another.
//! let buffer_id = handle.new_buffer(&mode);
//!
//! # let mut loop_count = 0;
//! loop {
//!     handle.request_update(buffer_id, UPDATE_BUFFER_TIMEOUT)?;
//!     let buf = handle.get_buffer(buffer_id).expect("Buffer exists");
//!     // Do something with the bytes
//!     let _bytes = buf.bytes();
//! #
//! #   if loop_count > 10 { break; }
//! #   loop_count += 1;
//! }
//!
//! # Ok(())
//! # }
//! ```
//!
//! # Managing device nodes
//! Creating and removing device nodes requires superuser permissions.
//!
//! I include the helper binaries `evdi_device_add` and `evdi_device_remove_all` that do nothing but
//! call [`DeviceNode::add`](crate::device_node::DeviceNode::add) and
//! [`DeviceNode::remove_all`](crate::device_node::DeviceNode::remove_all) so that you can easily
//! manage devices while testing.
//!
//! For example:
//!
//! ```bash
//! > # (while in the checked out source code of this library)
//! > cargo build --bin evdi_device_add
//! > sudo target/debug/evdi_device_add
//! ```
//!
//! You will probably want to create your own seperate binaries that manage device nodes so that
//! your users don't need to run your main binary with superuser permissions.

use std::ffi::CStr;
use std::fs::File;
use std::io::Read;
use std::option::NoneError;
use std::os::raw::{c_char, c_void};

pub use drm_fourcc::DrmFormat;
pub use evdi_sys as fii;
use evdi_sys::*;
use lazy_static::lazy_static;
use regex::Regex;

pub mod buffer;
pub mod device_config;
pub mod device_node;
pub mod handle;
pub mod mode;
pub mod prelude;

/// Check the status of the evdi kernel module for compatibility with this library version.
///
/// ```
/// # use evdi::prelude::*;
/// match check_kernel_mod() {
///     KernelModStatus::NotInstalled => {
///         println!("You need to install the evdi kernel module");
///     }
///     KernelModStatus::Outdated => {
///         println!("Your version of the evdi kernel module is too outdated to use with this library version");
///     }
///     KernelModStatus::Compatible => {
///         println!("You have a compatible version of the evdi kernel module installed");
///     }
/// }
/// ```
pub fn check_kernel_mod() -> KernelModStatus {
    let mod_version = KernelModVersion::get();
    if let Some(mod_version) = mod_version {
        let lib_version = LibVersion::get();
        if lib_version.is_compatible_with(mod_version) {
            KernelModStatus::Compatible
        } else {
            KernelModStatus::Outdated
        }
    } else {
        KernelModStatus::NotInstalled
    }
}

/// Status of the evdi kernel module
pub enum KernelModStatus {
    NotInstalled,
    Outdated,
    Compatible,
}

/// Set the callback to receive log messages, instead of having them written to stdout.
///
/// The callback is global.
///
/// ```
/// # use evdi::prelude::*;
/// set_logging(|msg| println!("{}", msg));
/// ```
pub fn set_logging<F>(cb: F)
where
    F: Fn(String),
{
    unsafe {
        wrapper_evdi_set_logging(wrapper_log_cb {
            function: Some(logging_cb_caller::<F>),
            user_data: Box::into_raw(Box::new(cb)) as *mut c_void,
        });
    }
}

extern "C" fn logging_cb_caller<F: Fn(String)>(user_data: *mut c_void, msg: *const c_char) {
    let msg = unsafe { CStr::from_ptr(msg) }.to_str().unwrap().to_owned();
    let cb = unsafe { (user_data as *mut F).as_ref().unwrap() };
    cb(msg);
}

const MOD_VERSION_FILE: &str = "/sys/devices/evdi/version";

/// Version of kernel evdi module
pub struct KernelModVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl KernelModVersion {
    pub fn get() -> Option<Self> {
        lazy_static! {
            static ref RE: Regex =
                Regex::new(r"(?P<maj>[0-9]+)\.(?P<min>[0-9]+)\.(?P<pat>[0-9]+)").unwrap();
        }
        let mut file = File::open(MOD_VERSION_FILE).map_err(|_| NoneError)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents).map_err(|_| NoneError)?;

        let caps = RE.captures(&contents)?;

        let major = caps.name("maj")?.as_str().parse().map_err(|_| NoneError)?;
        let minor = caps.name("min")?.as_str().parse().map_err(|_| NoneError)?;
        let patch = caps.name("pat")?.as_str().parse().map_err(|_| NoneError)?;

        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

/// Version of the userspace evdi library
pub struct LibVersion {
    pub major: i32,
    pub minor: i32,
    pub patch: i32,
    compatible_mod_major: u32,
    compatible_mod_minor: u32,
}

impl LibVersion {
    /// Get the version of the evdi library linked against (not the kernel module).
    ///
    /// Uses semver. See <https://displaylink.github.io/evdi/details/#versioning>
    pub fn get() -> Self {
        let sys = unsafe {
            let mut out = evdi_lib_version {
                version_major: -1,
                version_minor: -1,
                version_patchlevel: -1,
            };
            evdi_get_lib_version(&mut out);
            out
        };

        let version = Self::new(
            &sys,
            EVDI_MODULE_COMPATIBILITY_VERSION_MAJOR,
            EVDI_MODULE_COMPATIBILITY_VERSION_MINOR,
        );

        // Ensure the struct was actually populated
        assert_ne!(version.major, -1);
        assert_ne!(version.minor, -1);
        assert_ne!(version.patch, -1);

        version
    }

    pub fn is_compatible_with(&self, other: KernelModVersion) -> bool {
        // Based on the private function is_evdi_compatible
        other.major == self.compatible_mod_major && other.minor >= self.compatible_mod_minor
    }

    fn new(sys: &evdi_lib_version, compatible_mod_major: u32, compatible_mod_minor: u32) -> Self {
        Self {
            major: sys.version_major,
            minor: sys.version_minor,
            patch: sys.version_patchlevel,
            compatible_mod_major,
            compatible_mod_minor,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::channel;

    use crate::prelude::*;
    use crate::*;

    #[test]
    fn cb_to_set_logging_receives_multiple_msgs() {
        let (send, recv) = channel();

        set_logging(move |msg| send.send(msg).unwrap());

        DeviceNode::get().unwrap().open().unwrap(); // Generate a log msg
        recv.recv().unwrap(); // Block until we receive the msg

        DeviceNode::get().unwrap().open().unwrap(); // Generate a log msg
        recv.recv().unwrap(); // Block until we receive the msg
    }

    #[test]
    fn get_lib_version_works() {
        LibVersion::get();
    }

    #[test]
    fn get_mod_version_works() {
        let result = KernelModVersion::get();
        assert!(result.is_some())
    }

    #[test]
    fn is_compatible_with_works() {
        let mod_version = KernelModVersion::get().unwrap();
        let lib_version = LibVersion::get();
        assert!(lib_version.is_compatible_with(mod_version));
    }
}
