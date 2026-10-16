//! `a2m graph` — whole-project knowledge graph.

use crate::cli::ExitCode;
use crate::cli::flags::{AnalyzeFormat, GraphFlags};
use crate::cli::format::parse_csv_exclude;
use crate::pipeline::{AnalyzeOptions, analyze};
use crate::render::graph::{GraphEdge, GraphOptions};
use crate::render::{Level, mermaid_to_dot};

/// Build the graph and print the knowledge-graph diagram.
pub fn run_graph(flags: &GraphFlags) -> ExitCode {
    let graph = match graph_options(flags) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("graph: {e}");
            return ExitCode::Failure;
        }
    };
    let opts = AnalyzeOptions {
        level: Level::Graph,
        exclude: parse_csv_exclude(&flags.exclude),
        include_generated: flags.include_generated,
        graph,
        ..AnalyzeOptions::default()
    };

    let report = match analyze(&flags.path, &opts) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("graph: {e}");
            return ExitCode::Failure;
        }
    };
    let rendered = match flags.format {
        AnalyzeFormat::Mermaid => report.mermaid,
        AnalyzeFormat::Dot => mermaid_to_dot(&report.mermaid),
    };

    if let Some(path) = flags.out.as_deref() {
        if let Err(e) = std::fs::write(path, &rendered) {
            eprintln!("graph: write {}: {e}", path.display());
            return ExitCode::Failure;
        }
    } else {
        print!("{rendered}");
    }
    eprintln!(
        "graph: {} files, {} atoms, {} edges resolved",
        report.files_parsed, report.atoms_indexed, report.edges_resolved
    );
    ExitCode::Success
}

/// Turn the CSV flags into [`GraphOptions`], rejecting unknown edge kinds
/// rather than silently drawing fewer edges than asked.
fn graph_options(flags: &GraphFlags) -> Result<GraphOptions, String> {
    let csv = |s: &str| -> Vec<String> {
        s.split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let edges = csv(&flags.edges)
        .iter()
        .map(|e| {
            GraphEdge::parse(e).ok_or_else(|| {
                format!(
                    "unknown edge kind {e:?} (expected: calls, imports, references, implements, extern, defines)"
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(GraphOptions {
        max_nodes: flags.max_nodes,
        kinds: csv(&flags.kinds),
        edges,
        group_depth: flags.group_depth,
        include_private: flags.include_private,
    })
}
