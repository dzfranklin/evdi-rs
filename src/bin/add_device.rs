use evdi::device_node::DeviceNode;

fn main() {
    let succeeded = DeviceNode::add();
    if !succeeded {
        panic!("Failed to add device. Did you run with superuser permissions?");
    }
}
