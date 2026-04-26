//! `ast-to-mermaid` CLI binary — thin wrapper over `core::cli::run`.

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    ast_to_mermaid_core::cli::run(&args).into()
}
