//! Simplify importing
//!
//! ```
//! use evdi::prelude::*;
//! ```
//!

#[allow(unused_imports)]
pub(crate) use tokio::{pin, select, spawn, time::sleep};
pub(crate) use tracing::{debug, error, info, instrument, span, warn, Level};

pub use crate::buffer::{Buffer, BufferId};
pub use crate::device_config::DeviceConfig;
pub use crate::device_node::DeviceNode;
pub use crate::events::{CursorChange, CursorMove, DdcCiData, Event, HandleEvents, Mode};
pub use crate::handle::{Handle, UnconnectedHandle};
pub use crate::DrmFormat;
pub use crate::{check_kernel_mod, ensure_logs_setup, KernelModStatus};
