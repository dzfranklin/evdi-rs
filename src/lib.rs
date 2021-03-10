#![feature(with_options)]
#![feature(try_trait)]

pub mod device;

use std::fs::File;
use std::io::Read;
use regex::Regex;
use lazy_static::lazy_static;
use evdi_sys::*;
use std::option::NoneError;

const MOD_VERSION_FILE: &str = "/sys/devices/evdi/version";

pub struct ModVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32
}

impl ModVersion {
    pub fn get() -> Option<Self> {
        lazy_static! {
            static ref RE: Regex = Regex::new(r"(?P<maj>[0-9]+)\.(?P<min>[0-9]+)\.(?P<pat>[0-9]+)")
                .unwrap();
        }
        let mut file = File::open(MOD_VERSION_FILE).map_err(|_| NoneError)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents).map_err(|_| NoneError)?;

        let caps = RE.captures(&contents)?;

        let major = caps.name("maj")?.as_str().parse().map_err(|_| NoneError)?;
        let minor = caps.name("min")?.as_str().parse().map_err(|_| NoneError)?;
        let patch = caps.name("pat")?.as_str().parse().map_err(|_| NoneError)?;

        Some(Self { major, minor, patch })
    }
}

/// Version of the userspace EVDI library
pub struct LibVersion {
    pub major: i32,
    pub minor: i32,
    pub patch: i32,
    compatible_mod_major: u32,
    compatible_mod_minor: u32,
}

impl LibVersion {
    /// Get the version of the EVDI library linked against (not the kernel module).
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

    pub fn is_compatible_with(&self, other: ModVersion) -> bool {
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
    use crate::*;

    #[test]
    fn get_lib_version_works() {
        LibVersion::get();
    }

    #[test]
    fn get_mod_version_works() {
        let result = ModVersion::get();
        assert!(result.is_some())
    }

    #[test]
    fn installed_kernel_mod_is_compatible() {
        let mod_version = ModVersion::get().unwrap();
        let lib_version = LibVersion::get();
        assert!(lib_version.is_compatible_with(mod_version));
    }
}
