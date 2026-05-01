//! In-memory storage for parsed code atoms + edges.
//!
//! One backend ships: the [`Store`] in [`memory`]. It is enough for CLI
//! one-shot runs, in-process library use, and the test suite.

pub mod memory;

pub use memory::Store;
