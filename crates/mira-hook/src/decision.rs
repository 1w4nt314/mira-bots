//! The app's reply line and the PermissionRequest stdout JSON for Claude Code.

use serde_json::Value;

/// Default deny reason shown to Claude when the app sends none.
pub const DEFAULT_DENY_MESSAGE: &str = "Afvist i mira-bots";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny {
        message: String,
    },
    /// No decision: print nothing, so Claude Code shows its own terminal dialog.
    None,
}

/// Parses `{"v":1,"kind":"decision","decision":"allow"|"deny"|"none","message":...}`.
/// Anything invalid or unknown is `Decision::None`.
pub fn parse_reply(line: &str) -> Decision {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
        return Decision::None;
    };
    if v.get("v").and_then(Value::as_u64) != Some(1)
        || v.get("kind").and_then(Value::as_str) != Some("decision")
    {
        return Decision::None;
    }
    match v.get("decision").and_then(Value::as_str) {
        Some("allow") => Decision::Allow,
        Some("deny") => Decision::Deny {
            message: v
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        },
        _ => Decision::None,
    }
}

/// Stdout for Claude Code (research §1f), or `None` for no output at all.
pub fn to_stdout(d: &Decision) -> Option<String> {
    match d {
        Decision::Allow => Some(
            r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}"#
                .to_string(),
        ),
        Decision::Deny { message } => {
            let message = if message.trim().is_empty() {
                DEFAULT_DENY_MESSAGE
            } else {
                message.as_str()
            };
            Some(format!(
                r#"{{"hookSpecificOutput":{{"hookEventName":"PermissionRequest","decision":{{"behavior":"deny","message":{}}}}}}}"#,
                Value::String(message.to_string())
            ))
        }
        Decision::None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_reply_allow_deny_none() {
        assert_eq!(
            parse_reply(r#"{"v":1,"kind":"decision","decision":"allow","message":null}"#),
            Decision::Allow
        );
        assert_eq!(
            parse_reply(
                "{\"v\":1,\"kind\":\"decision\",\"decision\":\"deny\",\"message\":\"nej\"}\n"
            ),
            Decision::Deny {
                message: "nej".into()
            }
        );
        assert_eq!(
            parse_reply(r#"{"v":1,"kind":"decision","decision":"deny"}"#),
            Decision::Deny {
                message: String::new()
            }
        );
        assert_eq!(
            parse_reply(r#"{"v":1,"kind":"decision","decision":"none","message":null}"#),
            Decision::None
        );
    }

    #[test]
    fn parse_reply_garbage_is_none() {
        for line in [
            "",
            "allow",
            "{",
            "[]",
            r#"{"v":2,"kind":"decision","decision":"allow"}"#,
            r#"{"v":1,"kind":"hook","decision":"allow"}"#,
            r#"{"v":1,"kind":"decision","decision":"ALLOW"}"#,
            r#"{"v":1,"kind":"decision"}"#,
        ] {
            assert_eq!(parse_reply(line), Decision::None, "{line:?}");
        }
    }

    #[test]
    fn stdout_allow_matches_documented_format() {
        let s = to_stdout(&Decision::Allow).unwrap();
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(
            v,
            json!({"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}})
        );
    }

    #[test]
    fn stdout_deny_carries_message_or_default() {
        let s = to_stdout(&Decision::Deny {
            message: "brug \"npm ci\"".into(),
        })
        .unwrap();
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(
            v,
            json!({"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"brug \"npm ci\""}}})
        );
        let s = to_stdout(&Decision::Deny {
            message: String::new(),
        })
        .unwrap();
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(
            v["hookSpecificOutput"]["decision"]["message"],
            DEFAULT_DENY_MESSAGE
        );
    }

    #[test]
    fn stdout_none_is_empty() {
        assert_eq!(to_stdout(&Decision::None), None);
    }
}
