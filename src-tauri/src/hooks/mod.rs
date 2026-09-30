//! Pure hook logic: event model, status mapping and hooks.json generation.

pub mod event;
pub mod settings;
pub mod status;

/// JSON fixtures (one per event in hooks.json) shared by the unit tests.
#[cfg(test)]
pub(crate) mod fixtures {
    pub const SESSION_START: &str = include_str!("../../tests/fixtures/session_start.json");
    pub const USER_PROMPT_SUBMIT: &str =
        include_str!("../../tests/fixtures/user_prompt_submit.json");
    pub const PRE_TOOL_USE: &str = include_str!("../../tests/fixtures/pre_tool_use.json");
    pub const PERMISSION_REQUEST: &str =
        include_str!("../../tests/fixtures/permission_request.json");
    pub const PERMISSION_DENIED: &str =
        include_str!("../../tests/fixtures/permission_denied.json");
    pub const POST_TOOL_USE: &str = include_str!("../../tests/fixtures/post_tool_use.json");
    pub const POST_TOOL_USE_FAILURE: &str =
        include_str!("../../tests/fixtures/post_tool_use_failure.json");
    pub const NOTIFICATION_PERMISSION: &str =
        include_str!("../../tests/fixtures/notification_permission.json");
    pub const NOTIFICATION_IDLE: &str =
        include_str!("../../tests/fixtures/notification_idle.json");
    pub const NOTIFICATION_OTHER: &str =
        include_str!("../../tests/fixtures/notification_other.json");
    pub const STOP: &str = include_str!("../../tests/fixtures/stop.json");
    pub const STOP_FAILURE: &str = include_str!("../../tests/fixtures/stop_failure.json");
    pub const SESSION_END: &str = include_str!("../../tests/fixtures/session_end.json");
    pub const SUBAGENT_STOP: &str = include_str!("../../tests/fixtures/subagent_stop.json");
}
