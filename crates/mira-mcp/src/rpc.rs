//! Pure JSON-RPC 2.0 / MCP handling: one message in, at most one response out. No I/O here
//! except through the [`ToolBackend`] (plan4 punkt 2, research4 Q1).

use serde_json::{json, Value};

use crate::bridge::ToolBackend;
use crate::tools;

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;

/// Protocol versions we answer with as-is. Anything else (e.g. "2026-07-28") gets [`LATEST`].
pub const KNOWN_VERSIONS: [&str; 4] = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
pub const LATEST: &str = "2025-11-25";

/// `initialize.instructions` (plan4 C4.6, plan5: reports).
pub const INSTRUCTIONS: &str = "Værktøjer fra mira-bots til tickets. Kald mira_submit_for_review med en kort opsummering når din ticket er færdig, og læg gerne en rapport på ticketen (report i mira_submit_for_review eller mira_add_report).";

pub fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn result_response(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// A `tools/call` result: one text block; `is_error` marks a business error for the model.
pub fn tool_result(text: String, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

/// Handles one parsed message for an agent with `roles` (role wire names, from
/// `MIRA_AGENT_ROLES`). `None` = nothing to send (notification, i.e. no `id`).
///
/// `tools/list` shows only the tools `roles` allow; `tools/call` of a known tool the roles do
/// not allow is an `isError` result ([`tools::ROLE_DENIED`]) without asking the app (which
/// refuses it too); an unknown tool is `-32602`.
pub fn handle(msg: &Value, backend: &dyn ToolBackend, roles: &[String]) -> Option<Value> {
    let Some(obj) = msg.as_object() else {
        return Some(error_response(
            Value::Null,
            INVALID_REQUEST,
            "Invalid Request",
        ));
    };
    // No `id`: a notification (notifications/initialized, notifications/cancelled, …).
    let id = obj.get("id")?.clone();
    if !(id.is_string() || id.is_number()) {
        return Some(error_response(Value::Null, INVALID_REQUEST, "Invalid id"));
    }
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        return Some(error_response(id, INVALID_REQUEST, "Missing method"));
    };
    let params = obj.get("params");
    let reply = match method {
        "initialize" => {
            let asked = params
                .and_then(|p| p.get("protocolVersion"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let version = if KNOWN_VERSIONS.contains(&asked) {
                asked
            } else {
                LATEST
            };
            result_response(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": crate::SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
                    "instructions": INSTRUCTIONS
                }),
            )
        }
        "ping" => result_response(id, json!({})),
        "tools/list" => result_response(id, json!({"tools": tools::definitions_for(roles)})),
        "tools/call" => {
            let name = params
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !tools::is_known(name) {
                return Some(error_response(
                    id,
                    INVALID_PARAMS,
                    &format!("Unknown tool: {name}"),
                ));
            }
            if !tools::is_allowed(name, roles) {
                return Some(result_response(
                    id,
                    tool_result(tools::ROLE_DENIED.to_string(), true),
                ));
            }
            let args = params
                .and_then(|p| p.get("arguments"))
                .cloned()
                .unwrap_or_else(|| json!({}));
            let result = match tools::validate_args(name, &args) {
                Err(e) => tool_result(e, true),
                Ok(clean) => match backend.call(name, clean) {
                    Ok(v) => tool_result(v.to_string(), false),
                    Err(e) => tool_result(e, true),
                },
            };
            result_response(id, result)
        }
        // Includes `server/discover` (2026-07-28 probe): -32601 makes Claude Code fall back to
        // `initialize` (research4 Q1).
        _ => error_response(id, METHOD_NOT_FOUND, &format!("Method not found: {method}")),
    };
    Some(reply)
}

/// Parses one stdin line and returns the response line (without newline, never pretty).
pub fn handle_line(line: &str, backend: &dyn ToolBackend, roles: &[String]) -> Option<String> {
    let reply = match serde_json::from_str::<Value>(line) {
        Ok(msg) => handle(&msg, backend, roles)?,
        Err(_) => error_response(Value::Null, PARSE_ERROR, "Parse error"),
    };
    Some(reply.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Records calls; answers with `answer`.
    struct FakeBackend {
        calls: RefCell<Vec<(String, Value)>>,
        answer: Result<Value, String>,
    }

    impl FakeBackend {
        fn ok(v: Value) -> Self {
            FakeBackend {
                calls: RefCell::default(),
                answer: Ok(v),
            }
        }
        fn err(e: &str) -> Self {
            FakeBackend {
                calls: RefCell::default(),
                answer: Err(e.into()),
            }
        }
    }

    impl ToolBackend for FakeBackend {
        fn call(&self, tool: &str, args: Value) -> Result<Value, String> {
            self.calls.borrow_mut().push((tool.to_string(), args));
            self.answer.clone()
        }
    }

    /// An agent without roles (the common tools only).
    fn line(b: &FakeBackend, s: &str) -> Option<Value> {
        line_as(b, s, &[])
    }

    fn line_as(b: &FakeBackend, s: &str, roles: &[&str]) -> Option<Value> {
        let roles: Vec<String> = roles.iter().map(|r| r.to_string()).collect();
        handle_line(s, b, &roles).map(|out| {
            assert!(!out.contains('\n'), "one line: {out}");
            serde_json::from_str(&out).unwrap()
        })
    }

    #[test]
    fn claude_code_handshake_sequence() {
        let b = FakeBackend::ok(json!({"id":"t1","shortId":"abcdef01"}));
        // Research4 Q1, verbatim (clientInfo shortened).
        let r = line(&b, r#"{"jsonrpc":"2.0","id":"server-discover-probe-1","method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}"#).unwrap();
        assert_eq!(r["id"], "server-discover-probe-1");
        assert_eq!(r["error"]["code"], -32601);
        assert!(r.get("result").is_none());

        let r = line(&b, r#"{"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{"roots":{"listChanged":true},"elicitation":{"form":{},"url":{}}},"clientInfo":{"name":"claude-code","version":"2.1.286"}},"jsonrpc":"2.0","id":0}"#).unwrap();
        assert_eq!(r["jsonrpc"], "2.0");
        assert_eq!(r["id"], 0);
        assert_eq!(r["result"]["protocolVersion"], "2025-11-25");
        assert_eq!(r["result"]["capabilities"], json!({"tools":{}}));
        assert_eq!(r["result"]["serverInfo"]["name"], "mira-bots");
        assert_eq!(
            r["result"]["serverInfo"]["version"],
            env!("CARGO_PKG_VERSION")
        );
        assert_eq!(r["result"]["instructions"], INSTRUCTIONS);
        assert!(INSTRUCTIONS.chars().count() < 300);

        assert_eq!(
            line(
                &b,
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
            ),
            None
        );

        let r = line(&b, r#"{"method":"tools/list","jsonrpc":"2.0","id":1}"#).unwrap();
        assert_eq!(r["result"]["tools"].as_array().unwrap().len(), 10);
        assert_eq!(
            r["result"]["tools"],
            json!(tools::definitions_for(&[] as &[&str]))
        );

        let r = line(&b, r#"{"method":"tools/call","params":{"name":"mira_create_ticket","arguments":{"title":" Ny "},"_meta":{"claudecode/toolUseId":"toolu_1","progressToken":2}},"jsonrpc":"2.0","id":2}"#).unwrap();
        assert_eq!(r["id"], 2);
        assert_eq!(r["result"]["isError"], false);
        let content = r["result"]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        let text: Value = serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text, json!({"id":"t1","shortId":"abcdef01"}));
        assert_eq!(
            b.calls.borrow().as_slice(),
            &[("mira_create_ticket".to_string(), json!({"title":"Ny"}))]
        );
    }

    #[test]
    fn version_negotiation() {
        let b = FakeBackend::ok(json!({}));
        let init = |v: &str| {
            line(
                &b,
                &json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":v}})
                    .to_string(),
            )
            .unwrap()["result"]["protocolVersion"]
                .clone()
        };
        assert_eq!(init("2026-07-28"), "2025-11-25");
        assert_eq!(init("1999-01-01"), "2025-11-25");
        for v in KNOWN_VERSIONS {
            assert_eq!(init(v), v);
        }
        // No params at all.
        let r = line(&b, r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#).unwrap();
        assert_eq!(r["result"]["protocolVersion"], LATEST);
    }

    #[test]
    fn ping_and_unknown_methods() {
        let b = FakeBackend::ok(json!({}));
        assert_eq!(
            line(&b, r#"{"jsonrpc":"2.0","id":"123","method":"ping"}"#).unwrap(),
            json!({"jsonrpc":"2.0","id":"123","result":{}})
        );
        let r = line(&b, r#"{"jsonrpc":"2.0","id":4,"method":"resources/list"}"#).unwrap();
        assert_eq!(r["id"], 4);
        assert_eq!(r["error"]["code"], -32601);
        // Unknown notification: silence.
        assert_eq!(
            line(
                &b,
                r#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":2}}"#
            ),
            None
        );
    }

    #[test]
    fn protocol_errors() {
        let b = FakeBackend::ok(json!({}));
        let r = line(&b, "{nope").unwrap();
        assert_eq!(
            r,
            json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}})
        );
        let r = line(&b, r#"{"jsonrpc":"2.0","id":3}"#).unwrap();
        assert_eq!(
            (r["id"].clone(), r["error"]["code"].clone()),
            (json!(3), json!(-32600))
        );
        let r = line(&b, r#"[1,2]"#).unwrap();
        assert_eq!(r["error"]["code"], -32600);
        let r = line(&b, r#"{"jsonrpc":"2.0","id":{"x":1},"method":"ping"}"#).unwrap();
        assert_eq!(
            (r["id"].clone(), r["error"]["code"].clone()),
            (Value::Null, json!(-32600))
        );
    }

    #[test]
    fn ids_are_echoed_with_their_type() {
        let b = FakeBackend::ok(json!({}));
        let r = line(&b, r#"{"jsonrpc":"2.0","id":42,"method":"ping"}"#).unwrap();
        assert!(r["id"].is_u64());
        assert_eq!(r["id"], 42);
        let r = line(&b, r#"{"jsonrpc":"2.0","id":"42","method":"ping"}"#).unwrap();
        assert_eq!(r["id"], "42");
        let r = line(&b, r#"{"jsonrpc":"2.0","id":-1.5,"method":"ping"}"#).unwrap();
        assert_eq!(r["id"], -1.5);
    }

    #[test]
    fn unknown_tool_is_a_protocol_error() {
        let b = FakeBackend::ok(json!({}));
        let r = line(&b, r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"mira_assign","arguments":{}}}"#).unwrap();
        assert_eq!(r["error"]["code"], -32602);
        assert_eq!(r["error"]["message"], "Unknown tool: mira_assign");
        let r = line(&b, r#"{"jsonrpc":"2.0","id":6,"method":"tools/call"}"#).unwrap();
        assert_eq!(r["error"]["code"], -32602);
        assert!(b.calls.borrow().is_empty());
    }

    #[test]
    fn backend_error_is_an_is_error_result() {
        let b = FakeBackend::err("Du har ingen ticket i gang");
        let r = line(&b, r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"mira_submit_for_review","arguments":{"summary":"ok"}}}"#).unwrap();
        assert!(r.get("error").is_none());
        assert_eq!(
            r["result"],
            json!({"content":[{"type":"text","text":"Du har ingen ticket i gang"}],"isError":true})
        );
    }

    #[test]
    fn invalid_arguments_never_reach_the_backend() {
        let b = FakeBackend::ok(json!({}));
        let r = line(&b, r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"mira_submit_for_review","arguments":{}}}"#).unwrap();
        assert_eq!(r["result"]["isError"], true);
        assert_eq!(
            r["result"]["content"][0]["text"],
            "summary skal være en tekst på 1–2000 tegn"
        );
        // Missing `arguments` = `{}`: fine for list.
        let r = line(
            &b,
            r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"mira_get_ticket"}}"#,
        )
        .unwrap();
        assert_eq!(r["result"]["isError"], true);
        assert!(b.calls.borrow().is_empty());
        let r = line(&b, r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"mira_list_tickets"}}"#).unwrap();
        assert_eq!(r["result"]["isError"], false);
        assert_eq!(
            b.calls.borrow().as_slice(),
            &[("mira_list_tickets".to_string(), json!({}))]
        );
    }

    #[test]
    fn tools_list_ignores_params() {
        let b = FakeBackend::ok(json!({}));
        let r = line(
            &b,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"cursor":"x"}}"#,
        )
        .unwrap();
        assert_eq!(r["result"]["tools"].as_array().unwrap().len(), 10);
        assert!(r["result"].get("nextCursor").is_none());
    }

    fn listed(b: &FakeBackend, roles: &[&str]) -> Vec<String> {
        let r = line_as(
            b,
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            roles,
        )
        .unwrap();
        r["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn tools_list_is_filtered_by_roles() {
        let b = FakeBackend::ok(json!({}));
        let common: Vec<String> = tools::COMMON_TOOLS.iter().map(|s| s.to_string()).collect();
        assert_eq!(listed(&b, &[]), common);
        assert_eq!(listed(&b, &["coder"]), common);
        let rev = listed(&b, &["reviewer"]);
        assert_eq!(rev.len(), 12);
        assert!(rev.contains(&"mira_approve_ticket".into()));
        assert!(rev.contains(&"mira_reject_ticket".into()));
        let co = listed(&b, &["coordinator"]);
        assert_eq!(co.len(), 14);
        assert!(!co.contains(&"mira_approve_ticket".into()));
        assert_eq!(listed(&b, &["reviewer", "coordinator"]).len(), 16);
    }

    #[test]
    fn hidden_tool_call_is_refused_without_the_backend() {
        let b = FakeBackend::ok(json!({"id":"x"}));
        let call = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"mira_approve_ticket","arguments":{"id":"abcdef01"}}}"#;
        let r = line_as(&b, call, &["coder"]).unwrap();
        assert!(r.get("error").is_none());
        assert_eq!(
            r["result"],
            json!({"content":[{"type":"text","text":"Din rolle tillader ikke dette værktøj"}],"isError":true})
        );
        assert!(b.calls.borrow().is_empty());
        let r = line_as(&b, call, &["reviewer"]).unwrap();
        assert_eq!(r["result"]["isError"], false);
        assert_eq!(b.calls.borrow().len(), 1);
        // Unknown names stay a protocol error, whatever the roles.
        let r = line_as(
            &b,
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"mira_nope"}}"#,
            &["coordinator"],
        )
        .unwrap();
        assert_eq!(r["error"]["code"], -32602);
    }
}
