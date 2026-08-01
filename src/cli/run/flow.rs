//! `a2m flow` — forward call graph from one entry point.

use crate::cli::ExitCode;
use crate::cli::flags::FlowFlags;
use crate::cli::format::parse_csv_exclude;
use crate::pipeline::{AnalyzeOptions, analyze};
use crate::render::Level;
use crate::render::flow::External;

/// Build the graph, walk it forward from `--target`, print the diagram.
pub fn run_flow(flags: &FlowFlags) -> ExitCode {
    let opts = AnalyzeOptions {
        level: Level::Flow,
        target: Some(flags.target.clone()),
        exclude: parse_csv_exclude(&flags.exclude),
        include_generated: flags.include_generated,
        flow_depth: flags.depth,
        flow_external: external_mode(flags),
        ..AnalyzeOptions::default()
    };

    let report = match analyze(&flags.path, &opts) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("flow: {e}");
            return ExitCode::Failure;
        }
    };

    if let Some(path) = flags.out.as_deref() {
        if let Err(e) = std::fs::write(path, &report.mermaid) {
            eprintln!("flow: write {}: {e}", path.display());
            return ExitCode::Failure;
        }
    } else {
        print!("{}", report.mermaid);
    }
    eprintln!(
        "flow: {} depth {} ({} files, {} atoms)",
        flags.target, flags.depth, report.files_parsed, report.atoms_indexed
    );
    ExitCode::Success
}

/// `--no-external` wins over `--include-external`; clap already rejects
/// passing both, this keeps the mapping total regardless.
fn external_mode(flags: &FlowFlags) -> External {
    if flags.no_external {
        External::Never
    } else if flags.include_external {
        External::Always
    } else {
        External::NearOnly
    }
}
