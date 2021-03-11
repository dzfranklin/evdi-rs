use evdi::device::Device;

fn main() {
    let succeeded = Device::add();
    if !succeeded {
        panic!("Failed to add device. Did you run with superuser permissions?");
    }
}
