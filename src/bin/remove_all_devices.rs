use evdi::device::Device;

fn main() {
    Device::remove_all()
        .expect("Failed to remove all devices. Did you run with superuser permissions?")
}
