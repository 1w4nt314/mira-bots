//! mira-mcp binary: MCP over stdio until stdin EOF, then exit 0. stdout carries only JSON-RPC
//! lines (see `mira_mcp::run_loop`); diagnostics go to stderr with `MIRA_MCP_DEBUG=1`.

fn main() {
    let debug = mira_mcp::debug_from_env();
    let backend = mira_mcp::PipeBackend::from_env(debug);
    mira_mcp::log(
        debug,
        &format!(
            "start (pipe {}, agent id {})",
            if backend.pipe.is_some() {
                "set"
            } else {
                "missing"
            },
            if backend.agent_id.is_some() {
                "set"
            } else {
                "missing"
            }
        ),
    );
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    mira_mcp::run_loop(stdin.lock(), stdout.lock(), &backend, debug);
    // Exit right away; a worker thread still blocked on the pipe is simply torn down.
    std::process::exit(0);
}
