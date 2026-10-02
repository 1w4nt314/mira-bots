//! Hook and tool transport: the app side of the pipe that `mira-hook` and `mira-mcp` connect to.

pub mod handler;
pub mod protocol;
pub mod server;
#[cfg(unix)]
pub mod unix_socket;

pub use handler::{handle_connection, EmitFn, HandlerCtx, ToolHandler};
