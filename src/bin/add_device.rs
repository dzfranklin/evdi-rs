use evdi::device::Device;

fn main() {
    Device::add().expect("Failed to add device. Did you run with superuser permissions?");
}
