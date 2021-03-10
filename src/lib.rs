use evdi_sys::*;

pub struct LibVersion {
    pub major: i32,
    pub minor: i32,
    pub patch: i32,
}

impl LibVersion {
    /// Get the version of the evdi library linked against (not the kernel module).
    ///
    /// Uses semver. See <https://displaylink.github.io/evdi/details/#versioning>
    pub fn get() -> LibVersion {
        let sys = unsafe {
            let mut out = evdi_lib_version {
                version_major: -1,
                version_minor: -1,
                version_patchlevel: -1,
            };
            evdi_get_lib_version(&mut out);
            out
        };

        let version = LibVersion::new(&sys);

        // Ensure the struct was actually populated
        assert_ne!(version.major, -1);
        assert_ne!(version.minor, -1);
        assert_ne!(version.patch, -1);

        version
    }

    fn new(sys: &evdi_lib_version) -> LibVersion {
        LibVersion {
            major: sys.version_major,
            minor: sys.version_minor,
            patch: sys.version_patchlevel,
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
}
