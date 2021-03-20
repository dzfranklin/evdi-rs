use evdi::prelude::*;
use std::error::Error;
use std::time::Duration;
use tracing_flame::FlameLayer;
use tracing_subscriber::{fmt, prelude::*};

fn setup_global_subscriber() {
    let fmt_layer = fmt::Layer::default();

    let (flame_layer, _guard) = FlameLayer::with_file("./tracing.folded").unwrap();

    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(flame_layer)
        .init();
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    setup_global_subscriber();

    const READY_TIMEOUT: Duration = Duration::from_secs(1);
    const RECEIVE_INITIAL_MODE_TIMEOUT: Duration = Duration::from_secs(1);
    const UPDATE_BUFFER_TIMEOUT: Duration = Duration::from_millis(100);

    // If get returns None you need to call DeviceNode::add with superuser permissions.
    let device = DeviceNode::get().unwrap();

    // Replace this with the details of the display you want to emulate
    let device_config = DeviceConfig::sample();

    let unconnected_handle = device.open()?;
    let mut handle = unconnected_handle
        .connect(&device_config, READY_TIMEOUT)
        .await?;

    // For simplicity don't handle the mode changing after we start
    handle.dispatch_events();
    handle.events.mode.changed().await.unwrap();
    let mode = handle
        .events
        .mode
        .borrow()
        .expect("Mode will only be none before the first mode is received.");

    // For simplicity, we only use one buffer. You may want to use more than one buffer so that you
    // can send the contents of one buffer while updating another.
    let buffer_id = handle.new_buffer(&mode);

    for _ in 0..mode.refresh_rate {
        handle
            .request_update(buffer_id, UPDATE_BUFFER_TIMEOUT)
            .await?;
        let buf = handle.get_buffer(buffer_id).expect("Buffer exists");
        // Do something with the bytes
        let _bytes = buf.bytes();
    }

    Ok(())
}
