//! Vagt-tilstand (trin 6d): vagten starter selv playbooks for nye emner i indbakken i de
//! projekter, hvor brugeren har skrevet `"watch": {"enabled": true}` i sin `project.json` —
//! inden for et budget (glidende time, lokal dag, stille timer) og kun mens appen kører.
//!
//! De rene dele: [`config`] (`project.json`- og workspace-`watch`, playbook-valg), [`budget`]
//! (ringe, dag, stille timer), [`state`] (`watch-state.json` med atomisk skrivning og karantæne)
//! og [`engine`] (motoren bag porte). Skallen er [`runtime`]: `WatchRuntime` i `AppState`,
//! timeren (første tick efter 60 s, derefter hvert minut, arbejdet i `spawn_blocking`) og
//! `AppPort`, der kun bruger appens egne indgange (`inbox_start`, `start_playbook_with`,
//! `spawn_for_tool` bag en omsluttet port). Log-præfiks `watch: `.

pub mod budget;
pub mod config;
pub mod engine;
pub mod runtime;
pub mod state;

pub use budget::{
    check_project, conservative_fill, local_day, local_minute, next_local_midnight_ms,
    quiet_ends_in_ms, Caps, Ring, Verdict, WaitWhy,
};
pub use config::{
    in_quiet, parse_quiet, parse_watch, pick_playbook, PlaybookRule, WatchConfig, WorkspaceWatch,
};
pub use engine::{WatchProjectView, WatchView};
pub use runtime::WatchRuntime;
pub use state::{ProjectState, WatchState, WatchStateFile};
