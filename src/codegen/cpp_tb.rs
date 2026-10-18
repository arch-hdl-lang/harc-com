//! Compatibility surface for the retiring v1 C++ testbench backend.
//!
//! New shared code must depend on the focused modules in [`super`] or on the
//! private native implementation, never on this compatibility module.

pub use super::native::*;
