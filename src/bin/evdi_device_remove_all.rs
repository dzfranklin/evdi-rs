use evdi::prelude::*;

fn main() {
    DeviceNode::remove_all()
        .expect("Failed to remove all devices. Did you run with superuser permissions?")
}
