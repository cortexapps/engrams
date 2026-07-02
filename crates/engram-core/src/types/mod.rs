//! Shared, I/O-free domain types used across coordinator, host agent,
//! and backend implementations.

pub mod capability;
pub mod capture_progress;
pub mod catalog;
pub mod cow_state;
pub mod egress;
pub mod evacuation;
pub mod event;
pub mod harness;
pub mod host;
pub mod ids;
pub mod image;
pub mod integration;
pub mod manifest;
pub mod org_secret;
pub mod port;
pub mod registry;
pub mod sandbox;
pub mod session;
pub mod shell;
pub mod snapshot;

pub use capability::*;
pub use capture_progress::*;
pub use catalog::*;
pub use cow_state::*;
pub use egress::*;
pub use evacuation::*;
pub use event::*;
pub use harness::*;
pub use host::*;
pub use ids::*;
pub use image::*;
pub use integration::*;
pub use manifest::*;
pub use org_secret::*;
pub use registry::*;
pub use sandbox::*;
pub use session::*;
pub use shell::*;
pub use snapshot::*;
