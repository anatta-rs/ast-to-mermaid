//! Storage implementation — self-contained, no external graph backend.
//!
//! The only backend shipped is the in-memory [`Store`], which is sufficient
//! for CLI one-shot runs and tests. There is no polystore trait anywhere in
//! this module.

pub mod memory;

pub use memory::Store;
