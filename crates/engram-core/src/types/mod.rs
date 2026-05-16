//! Shared, I/O-free domain types used across coordinator, host agent,
//! and backend implementations.

pub mod egress;
pub mod event;
pub mod host;
pub mod ids;
pub mod image;
pub mod manifest;
pub mod registry;
pub mod sandbox;
pub mod session;
pub mod snapshot;
pub mod template;

pub use egress::*;
pub use event::*;
pub use host::*;
pub use ids::*;
pub use image::*;
pub use manifest::*;
pub use registry::*;
pub use sandbox::*;
pub use session::*;
pub use snapshot::*;
pub use template::*;
