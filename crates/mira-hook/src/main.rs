//! mira-hook binary. Always exits 0; stdout is either empty or Claude Code's decision JSON.

use std::io::{Read, Write};

fn main() {
    let debug = mira_hook::debug_from_env();
    let mut input = Vec::new();
    let read_ok = std::io::stdin()
        .lock()
        .take(mira_hook::MAX_STDIN as u64 + 1)
        .read_to_end(&mut input)
        .is_ok();
    if read_ok {
        if let Some(out) = mira_hook::run(
            &input,
            mira_hook::transport::pipe_name_from_env(),
            mira_hook::agent_id_from_env(),
            debug,
        ) {
            let mut stdout = std::io::stdout().lock();
            let _ = stdout.write_all(out.as_bytes());
            let _ = stdout.flush();
        }
    }
    // Exit right away; a still-blocked worker thread is simply torn down.
    std::process::exit(0);
}
