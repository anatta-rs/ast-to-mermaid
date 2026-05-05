//! Shared CLI infrastructure for the unified `a2m` binary.
//!
//! The crate ships one binary (`a2m`) with seven subcommands. The module is
//! split three ways:
//!
//! - [`flags`] — per-subcommand `clap::Args` structs ([`AnalyzeFlags`],
//!   [`WalkFlags`], [`IndexFlags`], [`DiffFlags`], [`GcFlags`],
//!   [`BundleFlags`], [`SequenceFlags`]) plus the [`ExitCode`] enum and the
//!   shared [`AnalyzeFormat`] / [`DiffFormat`] / [`CacheArgs`] helpers.
//! - [`run`] — the `run_*` entry points the binary dispatches to.
//! - [`format`] — small format / parse helpers (CSV exclude, byte size,
//!   duration, the parser-ignored summary comment).
//!
//! Everything that used to live in the old `cli_support` module is
//! re-exported from here so external embedders keep resolving the same
//! short names (`ast_to_mermaid::cli::run_analyze`, etc.).

pub mod flags;
pub mod format;
pub mod run;

pub use flags::{
    AnalyzeFlags, AnalyzeFormat, BundleFlags, CacheArgs, DiffFlags, DiffFormat, ExitCode, GcFlags,
    IndexFlags, SequenceFlags, WalkFlags,
};
pub use run::{run_analyze, run_bundle, run_diff, run_gc, run_index, run_sequence, run_walk};
