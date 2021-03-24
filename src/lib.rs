#![feature(with_options)]
#![feature(try_trait)]
#![feature(debug_non_exhaustive)]

//! High-level bindings to [evdi](https://github.com/DisplayLink/evdi), a library for managing
//! virtual displays on linux.
//!
//! ## Tracing and logging
//! This library emits many [tracing](https://tracing.rs) events. Logs by libevdi are
//! converted to INFO-level tracing events.
//!
//! ## Not thread safe
//! Evdi is not thread safe, and you cannot block the thread it runs for too long as your real and
//! virtual devices will not receive events.
//!
//! ## Alpha quality
//! This library is alpha quality. If your display starts behaving weirdly, rebooting may help.
//!
//! ## Basic usage
//!
//! ```
//! # use std::error::Error;
//! # use std::time::Duration;
//! # use evdi::prelude::*;
//! #
//! const AWAIT_MODE_TIMEOUT: Duration = Duration::from_millis(250);
//! const UPDATE_BUFFER_TIMEOUT: Duration = Duration::from_millis(20);
//!
//! # tokio_test::block_on(async {
//! // If get returns None you need to call DeviceNode::add with superuser permissions or setup the
//! // kernel module to create a device on module load.
//! let device = DeviceNode::get().unwrap();
//!
//! // Replace this with the details of the display you want to emulate
//! let device_config = DeviceConfig::sample();
//!
//! let unconnected_handle = device.open()?;
//! let mut handle = unconnected_handle.connect(&device_config);
//!
//! // For simplicity don't handle the mode changing after we start
//! let mode = handle.events.await_mode(AWAIT_MODE_TIMEOUT).await?;
//!
//! // For simplicity, we only use one buffer. You may want to use more than one buffer so that you
//! // can send the contents of one buffer while updating another.
//! let buffer_id = handle.new_buffer(&mode);
//!
//! # let mut loop_count = 0;
//! loop {
//!     handle.request_update(buffer_id, UPDATE_BUFFER_TIMEOUT).await?;
//!     let buf = handle.get_buffer(buffer_id).expect("Buffer exists");
//!     // Do something with the bytes
//!     let _bytes = buf.bytes();
//! #
//! #   if loop_count > 2 { break; }
//! #   loop_count += 1;
//! }
//!
//! # Ok::<(), Box<dyn Error>>(())
//! # });
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
//!
//! Another alternative is [configuring the kernel module] to create devices when it loads.
//!
//! [module-params]: https://displaylink.github.io/evdi/details/#module-parameters

use std::ffi::CStr;
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::option::NoneError;
use std::os::raw::{c_char, c_void};

pub use drm_fourcc::DrmFormat;
pub use evdi_sys as ffi;
use lazy_static::lazy_static;
use regex::Regex;
use std::fmt::{Debug, Formatter};
use std::ptr::null_mut;
use std::slice;
use std::sync::Once;
use tracing::{info, instrument};

pub mod buffer;
pub mod device_config;
pub mod device_node;
pub mod events;
pub mod handle;
pub mod prelude;

#[cfg(test)]
mod test_common;

/// Check the status of the evdi kernel module for compatibility with this library version.
///
/// This is provided so that you can show a helpful error message early. [`DeviceNode::open`] will
/// perform this check for you.
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
#[tracing::instrument]
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

static LOGS_SETUP: Once = Once::new();

/// Configure libevdi to emit tracing events instead of writing to stdout.
///
/// Calling this function multiple times has no effect.
pub(crate) fn ensure_logs_setup() {
    LOGS_SETUP.call_once(|| unsafe {
        ffi::wrapper_evdi_set_logging(ffi::wrapper_log_cb {
            function: Some(logs_cb),
            user_data: null_mut(),
        });
    });
}

extern "C" fn logs_cb(_user_data: *mut c_void, msg: *const c_char) {
    let msg = unsafe { CStr::from_ptr(msg) }.to_str().unwrap().to_owned();
    info!(libevdi_unknown_level = true, "libevdi: {}", msg);
}

const MOD_VERSION_FILE: &str = "/sys/devices/evdi/version";

/// Version of kernel evdi module
pub struct KernelModVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl KernelModVersion {
    #[instrument]
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
    #[instrument]
    pub fn get() -> Self {
        let sys = unsafe {
            ensure_logs_setup();

            let mut out = ffi::evdi_lib_version {
                version_major: -1,
                version_minor: -1,
                version_patchlevel: -1,
            };
            ffi::evdi_get_lib_version(&mut out);
            out
        };

        let version = Self::new(
            &sys,
            ffi::EVDI_MODULE_COMPATIBILITY_VERSION_MAJOR,
            ffi::EVDI_MODULE_COMPATIBILITY_VERSION_MINOR,
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

    fn new(
        sys: &ffi::evdi_lib_version,
        compatible_mod_major: u32,
        compatible_mod_minor: u32,
    ) -> Self {
        Self {
            major: sys.version_major,
            minor: sys.version_minor,
            patch: sys.version_patchlevel,
            compatible_mod_major,
            compatible_mod_minor,
        }
    }
}

/// An array allocated as a specific size, of which only part is populated.
pub(crate) struct PreallocatedArray<T> {
    data: Box<[T]>,
    len: usize,
    max_len: usize,
}

impl<T> AsRef<[T]> for PreallocatedArray<T> {
    fn as_ref(&self) -> &[T] {
        if self.len > self.max_len {
            panic!(
                "SizedArray has length {}, but was allocated with length {}",
                self.len, self.max_len
            );
        }
        &self.data[0..self.len]
    }
}

impl<T> PreallocatedArray<T> {
    pub(crate) fn new(data: Box<[T]>, len: usize) -> Self {
        let max_len = data.len();
        Self { data, len, max_len }
    }

    pub(crate) fn data_ptr_mut(&mut self) -> *mut T {
        self.data.as_mut_ptr()
    }

    pub(crate) fn len_ptr_mut(&mut self) -> *mut usize {
        &mut self.len as _
    }
}

impl<T: Debug> Debug for PreallocatedArray<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.as_ref()).finish()
    }
}

pub(crate) struct OwnedLibcArray<T> {
    ptr: *const T,
    len: usize,
}

impl<T> OwnedLibcArray<T> {
    /// Safety: ptr must point to an array allocated by libc, and len must be the length of the
    /// array. LibcArray must "own" ptr: no one else can write to it, and we must be responsible for
    /// freeing it.
    pub(crate) unsafe fn new(ptr: *const T, len: usize) -> Self {
        Self { ptr, len }
    }

    pub(crate) fn as_slice(&self) -> &[T] {
        unsafe {
            // Safety: Invariants required by Self::new
            slice::from_raw_parts(self.ptr, self.len)
        }
    }
}

// Safety: By invariants of new no one else ever writes to, and we never write to.
unsafe impl<T> Send for OwnedLibcArray<T> {}

// Safety: By invariants of new no one else ever writes to, and we never write to.
unsafe impl<T> Sync for OwnedLibcArray<T> {}

impl<T> Drop for OwnedLibcArray<T> {
    fn drop(&mut self) {
        unsafe {
            libc::free(self.ptr as *mut c_void);
        }
    }
}

impl<T: Debug> Debug for OwnedLibcArray<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.as_slice().iter()).finish()
    }
}

#[cfg(test)]
mod tests {
    use crate::prelude::*;
    use crate::test_common::*;
    use crate::*;
    use logtest::Record;
    use tracing::log::Level;

    #[test]
    fn logs_correctly() {
        use logtest::Logger;
        let logger = Logger::start();

        ensure_logs_setup();

        DeviceNode::get().unwrap().open().unwrap(); // Generate some log msgs

        let records: Vec<Record> = logger
            .into_iter()
            .filter(|r| r.args().starts_with("libevdi"))
            .collect();

        assert!(records.len() > 2);

        for record in records {
            assert_eq!(record.level(), Level::Info);
            assert!(record.args().contains(" libevdi_unknown_level=true "));
            assert!(record.args().contains(" wraps="));
        }
    }

    #[ltest]
    fn get_lib_version_works() {
        LibVersion::get();
    }

    #[ltest]
    fn get_mod_version_works() {
        let result = KernelModVersion::get();
        assert!(result.is_some())
    }

    #[ltest]
    fn is_compatible_with_works() {
        let mod_version = KernelModVersion::get().unwrap();
        let lib_version = LibVersion::get();
        assert!(lib_version.is_compatible_with(mod_version));
    }
}
