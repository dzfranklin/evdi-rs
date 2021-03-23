use evdi::handle::RequestUpdateError;
use evdi::prelude::*;
use std::error::Error;
use std::time::Duration;
use tokio::time::sleep;
use tracing_flame::FlameLayer;
use tracing_subscriber::{fmt, prelude::*};

const RUN_FOR: Duration = Duration::from_secs(5);

const RECEIVE_INITIAL_MODE_TIMEOUT: Duration = Duration::from_secs(1);
const UPDATE_BUFFER_TIMEOUT: Duration = Duration::from_millis(500);

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    setup_tracing();

    // If get returns None you need to call DeviceNode::add with superuser permissions.
    let device = DeviceNode::get().unwrap();

    // Replace this with the details of the display you want to emulate
    let device_config = DeviceConfig::sample();

    let unconnected_handle = device.open()?;
    let mut handle = unconnected_handle.connect(&device_config);

    // For simplicity don't handle the mode changing after we start
    let mode = handle
        .events
        .await_mode(RECEIVE_INITIAL_MODE_TIMEOUT)
        .await?;

    handle.enable_cursor_events(true);

    // For simplicity, we only use one buffer. You may want to use more than one buffer so that you
    // can send the contents of one buffer while updating another.
    let buffer_id = handle.new_buffer(&mode);

    tokio::select! {
        _ = sleep(RUN_FOR) => {}
        res = mainloop(&mut handle, buffer_id) => res.unwrap(),
    }

    println!(
        "\n\nTo visualize the traces run `inferno-flamegraph < tracing.folded > flamegraph.svg` or \
        `inferno-flamegraph --flamechart < tracing.folded > flamechart.svg`\n\n"
    );

    Ok(())
}

async fn mainloop(handle: &mut Handle, buffer_id: BufferId) -> Result<(), RequestUpdateError> {
    loop {
        handle
            .request_update(buffer_id, UPDATE_BUFFER_TIMEOUT)
            .await?;
        let buf = handle.get_buffer(buffer_id).expect("Buffer exists");
        // Do something with the bytes
        let _bytes = buf.bytes();
    }
}

fn setup_tracing() {
    let fmt_layer = fmt::Layer::default();

    let (flame_layer, _guard) = FlameLayer::with_file("./tracing.folded").unwrap();

    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(flame_layer)
        .init();
}
