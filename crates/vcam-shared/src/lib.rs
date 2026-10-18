//! Code shared by the Rust VCam app and its Frame Server media source.

pub mod frame;
pub mod pattern;
pub mod shm;

pub use frame::{FrameFormat, PixelFormat};
