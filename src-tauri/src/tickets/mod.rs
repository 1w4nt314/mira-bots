//! Step 3: tickets, per-agent queues, review and the dispatcher that types tickets into idle
//! agents' terminals.
//!
//! Dependency direction: `tickets` → `agent`, `hooks::status`, `events`, `config`. Nothing in
//! `agent`, `pipe` or `hooks` knows `tickets`.

pub mod dispatcher;
pub mod model;
pub mod prompt;
pub mod service;
pub mod state;
pub mod store;
