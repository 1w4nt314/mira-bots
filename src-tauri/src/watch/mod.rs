//! Vagt-tilstand (trin 6d): vagten starter selv playbooks for nye emner i indbakken i de
//! projekter, hvor brugeren har skrevet `"watch": {"enabled": true}` i sin `project.json` —
//! inden for et budget (glidende time, lokal dag, stille timer) og kun mens appen kører.
//!
//! Batch 1 (plan6d punkt 1–7) er de rene dele uden tråde: [`config`] (`project.json`- og
//! workspace-`watch`, playbook-valg), [`budget`] (ringe, dag, stille timer) og [`state`]
//! (`watch-state.json` med atomisk skrivning og karantæne). Motoren og timeren kommer i batch 4.

pub mod budget;
pub mod config;
pub mod state;

pub use budget::{
    check_project, conservative_fill, local_day, local_minute, next_local_midnight_ms,
    quiet_ends_in_ms, Caps, Ring, Verdict, WaitWhy,
};
pub use config::{
    in_quiet, parse_quiet, parse_watch, pick_playbook, PlaybookRule, WatchConfig, WorkspaceWatch,
};
pub use state::{ProjectState, WatchState, WatchStateFile};
