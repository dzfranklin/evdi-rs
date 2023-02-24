pub use crate::prelude::*;
pub use std::time::Duration;
pub use test_log::test as ltest;
pub use tokio::test as atest;

pub const TIMEOUT: Duration = Duration::from_secs(5);

pub fn handle_fixture() -> Handle {
    DeviceNode::get()
        .unwrap()
        .open()
        .unwrap()
        .connect(&DeviceConfig::sample())
}
