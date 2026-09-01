//! The rua language server, as a library so that its answers can be tested
//! without a transport in the way.

pub mod analysis;
pub mod docs;
pub mod index;

pub use analysis::{World, TOKEN_TYPES};
