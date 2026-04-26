//! `ast-to-mermaid-mcp` binary — thin wrapper over `core::mcp::serve_stdio`.

fn main() -> std::process::ExitCode {
    match ast_to_mermaid_core::mcp::serve_stdio() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
