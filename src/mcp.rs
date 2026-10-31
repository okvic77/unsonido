//! A minimal Model Context Protocol (MCP) server so agents can drive the API with
//! discovered tools instead of hand-rolled curl calls.
//!
//! Implements the stateless "Streamable HTTP" transport on one route: `POST /mcp` takes
//! JSON-RPC 2.0 and answers plain `application/json` (`text/event-stream` for clients
//! that ask for nothing else). Only `initialize`, `tools/list` and `tools/call` are
//! implemented — everything else is notifications. Agents register
//! `http://127.0.0.1:7700/mcp` once and then see the tools below as callable functions.

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::alerts::AlertRequest;
use crate::api::AppState;
use crate::inputs::BackgroundRequest;

/// The one protocol revision we speak.
const PROTOCOL_VERSION: &str = "2025-06-18";
/// Older revisions whose core methods match ours; a client proposing one gets it echoed.
const SUPPORTED: [&str; 2] = ["2025-06-18", "2025-03-26"];

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/mcp",
        post(post_mcp)
            .get(reject_mcp)
            .delete(reject_mcp)
            .options(reject_mcp),
    )
}

/// One incoming JSON-RPC message. A missing (or null) `id` marks a notification,
/// which is acknowledged rather than answered.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IncomingMessage {
    #[serde(default = "two_point_oh")]
    jsonrpc: String,
    method: String,
    #[serde(default)]
    params: Option<Value>,
    id: Option<Value>,
}

fn two_point_oh() -> String {
    "2.0".into()
}

async fn post_mcp(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let message: Value = match serde_json::from_slice(&body) {
        Ok(message) => message,
        Err(err) => {
            return answered(
                rpc_error(Value::Null, -32700, &format!("parse error: {err}")),
                &headers,
            );
        }
    };
    // The 2025-06-18 revision retired JSON-RPC batches; anything that is not one object
    // is an invalid request.
    let parsed: IncomingMessage = match serde_json::from_value(message) {
        Ok(parsed) => parsed,
        Err(err) => {
            return answered(
                rpc_error(Value::Null, -32600, &format!("invalid request: {err}")),
                &headers,
            );
        }
    };
    if parsed.jsonrpc != "2.0" {
        return answered(
            rpc_error(Value::Null, -32600, "jsonrpc must be \"2.0\""),
            &headers,
        );
    }

    // Notifications get no response body, just an acknowledgment.
    match parsed.id {
        None | Some(Value::Null) => StatusCode::ACCEPTED.into_response(),
        Some(id) => answered(
            dispatch(&state, id, &parsed.method, parsed.params).await,
            &headers,
        ),
    }
}

/// Route one request to its handler and wrap the result as a JSON-RPC response.
async fn dispatch(state: &AppState, id: Value, method: &str, params: Option<Value>) -> Value {
    match method {
        "initialize" => initialize(id, params),
        "ping" => rpc_ok(id, json!({})),
        "tools/list" => rpc_ok(id, json!({ "tools": tools() })),
        "tools/call" => call_tool(state, id, params).await,
        other => rpc_error(id, -32601, &format!("method {other:?} is not supported")),
    }
}

/// Accept the client's revision when we support it, advertise ours otherwise.
fn initialize(id: Value, params: Option<Value>) -> Value {
    let asked = params
        .as_ref()
        .and_then(|params| params.get("protocolVersion"))
        .and_then(Value::as_str);
    let version = match asked {
        Some(v) if SUPPORTED.contains(&v) => v.to_string(),
        _ => PROTOCOL_VERSION.to_string(),
    };
    rpc_ok(
        id,
        json!({
            "protocolVersion": version,
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "unsonido",
                "title": "Unsonido audio alerts",
                "version": env!("CARGO_PKG_VERSION")
            },
            "instructions": "Speaks through the speakers with a chime. play_alert plays right now; \
             apply_alert stores an alert in a namespace under a name and auto-destructs once \
             played — give it `at` (RFC 3339 or local \"YYYY-MM-DD HH:MM\") to schedule it. \
             sync_namespace replaces a namespace wholesale, e.g. today's calendar. get_status \
             shows what is playing, queued, scheduled and recent."
        }),
    )
}

#[derive(Deserialize)]
struct CallParams {
    name: String,
    #[serde(default)]
    arguments: Option<Value>,
}

/// Run one tool and report per the MCP spec: a result with `isError` for tool failures,
/// a protocol error (-32602) for an unknown tool.
async fn call_tool(state: &AppState, id: Value, params: Option<Value>) -> Value {
    let params: CallParams = match params.map(serde_json::from_value::<CallParams>) {
        Some(Ok(parsed)) => parsed,
        Some(Err(err)) => return rpc_error(id, -32602, &format!("invalid params: {err}")),
        None => return rpc_error(id, -32602, "invalid params: `name` is required"),
    };
    let arguments = params.arguments.unwrap_or_else(|| json!({}));
    let outcome: Result<Value, String> = match params.name.as_str() {
        "play_alert" => match parse_args::<AlertRequest>(&arguments) {
            Ok(request) => into_json(state.alerts.submit(request).await),
            Err(message) => Err(message),
        },
        "apply_alert" => {
            #[derive(Deserialize)]
            struct Args {
                namespace: String,
                name: String,
                #[serde(flatten)]
                request: AlertRequest,
            }
            match parse_args::<Args>(&arguments) {
                Ok(args) => into_json(
                    state
                        .alerts
                        .apply(args.namespace, args.name, args.request)
                        .await,
                ),
                Err(message) => Err(message),
            }
        }
        "sync_namespace" => {
            #[derive(Deserialize)]
            struct Args {
                namespace: String,
                alerts: std::collections::BTreeMap<String, AlertRequest>,
            }
            match parse_args::<Args>(&arguments) {
                Ok(args) => into_json(
                    state
                        .alerts
                        .sync_namespace(args.namespace, args.alerts)
                        .await,
                ),
                Err(message) => Err(message),
            }
        }
        "get_alert" => {
            #[derive(Deserialize)]
            struct Args {
                namespace: String,
                name: String,
            }
            match parse_args::<Args>(&arguments) {
                Ok(args) => Ok(state
                    .alerts
                    .stored_alert(&args.namespace, &args.name)
                    .map(|alert| serde_json::to_value(alert).unwrap_or_default())
                    .unwrap_or_else(|| json!({ "found": false }))),
                Err(message) => Err(message),
            }
        }
        "delete_alert" => {
            #[derive(Deserialize)]
            struct Args {
                namespace: String,
                name: String,
            }
            match parse_args::<Args>(&arguments) {
                Ok(args) => {
                    Ok(json!({ "deleted": state.alerts.delete_alert(&args.namespace, &args.name) }))
                }
                Err(message) => Err(message),
            }
        }
        "delete_namespace" => {
            #[derive(Deserialize)]
            struct Args {
                namespace: String,
            }
            match parse_args::<Args>(&arguments) {
                Ok(args) => {
                    Ok(json!({ "deleted": state.alerts.delete_namespace(&args.namespace) }))
                }
                Err(message) => Err(message),
            }
        }
        "list_namespaces" => {
            Ok(serde_json::to_value(state.alerts.list_namespaces()).unwrap_or_default())
        }
        "get_status" => {
            let mut status = serde_json::to_value(state.alerts.status()).unwrap_or_default();
            status["background"] = json!(state.background.status());
            status["airplay"] = json!(state.airplay.status());
            Ok(status)
        }
        "skip_current" => {
            state.alerts.skip();
            Ok(json!({ "skipped": true }))
        }
        "clear_alerts" => {
            state.alerts.clear();
            Ok(json!({ "cleared": true }))
        }
        "set_volume" => {
            #[derive(Deserialize)]
            struct Args {
                volume: f32,
            }
            match parse_args::<Args>(&arguments) {
                Ok(args) if (0.0..=1.0).contains(&args.volume) => {
                    state.alerts.set_volume(args.volume);
                    Ok(json!({ "volume": args.volume }))
                }
                Ok(_) => Err("volume must be between 0.0 and 1.0".into()),
                Err(message) => Err(message),
            }
        }
        "play_background" => match parse_args::<BackgroundRequest>(&arguments) {
            Ok(request) => match state.background.play(request) {
                Ok(()) => Ok(serde_json::to_value(state.background.status()).unwrap_or_default()),
                Err(err) => Err(format!("{err:#}")),
            },
            Err(message) => Err(message),
        },
        "stop_background" => {
            state.background.stop();
            Ok(json!({ "stopped": true }))
        }
        unknown => {
            return rpc_error(
                id,
                -32602,
                &format!("unknown tool {unknown:?} (see tools/list)"),
            );
        }
    };
    match outcome {
        Ok(value) => {
            tracing::info!(tool = %params.name, "mcp tool call succeeded");
            rpc_ok(
                id,
                tool_body(
                    &serde_json::to_string_pretty(&value).unwrap_or_default(),
                    false,
                ),
            )
        }
        Err(message) => {
            tracing::info!(tool = %params.name, error = %message, "mcp tool call failed");
            rpc_ok(id, tool_body(&message, true))
        }
    }
}

/// Deserialize tool arguments, or explain which argument was wrong.
fn parse_args<T: serde::de::DeserializeOwned>(arguments: &Value) -> Result<T, String> {
    serde_json::from_value(arguments.clone()).map_err(|err| format!("invalid arguments: {err}"))
}

/// Map an alert-queue outcome to JSON, with its human error message on failure.
fn into_json<T: serde::Serialize>(
    result: Result<T, crate::alerts::AlertError>,
) -> Result<Value, String> {
    match result {
        Ok(value) => Ok(serde_json::to_value(value).unwrap_or_default()),
        Err(err) => Err(err.message()),
    }
}

/// Every tool the server can run.
fn tools() -> Vec<Value> {
    fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
        let mut value = json!({
            "name": name,
            "description": description,
            "inputSchema": {
                "type": "object",
                "properties": properties,
                "additionalProperties": false
            }
        });
        if !required.is_empty() {
            value["inputSchema"]["required"] = json!(required);
        }
        value
    }
    let alert_fields = json!({
        "text": { "type": "string", "description": "Text to speak" },
        "url": { "type": "string", "description": "http(s) audio file to play instead of text" },
        "provider": { "type": "string", "description": "TTS provider name; the default one when omitted" },
        "voice": { "type": "string" },
        "priority": { "enum": ["notification", "critical"],
            "description": "notification ducks the background music; critical interrupts it" },
        "repeat": { "type": "integer", "minimum": 1 },
        "chime": { "type": "boolean", "description": "Play a chime first (default true)" }
    });
    vec![
        tool(
            "play_alert",
            "Speak text or play an audio URL through the speakers right now.",
            alert_fields.clone(),
            &[],
        ),
        tool(
            "apply_alert",
            "Store an alert in a namespace under a name (kubectl-style): it plays now, or at `at`; \
             re-applying replaces the stored alert, and once it plays it self-destructs.",
            json!({
                "namespace": { "type": "string", "description": "e.g. calendar, agents, script" },
                "name": { "type": "string", "description": "Unique within the namespace" },
                "at": { "type": "string",
                    "description": "Play at this time instead of now: RFC 3339 (2026-11-02T15:45:00-06:00) \
                     or local \"YYYY-MM-DD HH:MM\"; a past time plays immediately" },
                "text": alert_fields["text"],
                "url": alert_fields["url"],
                "provider": alert_fields["provider"],
                "voice": alert_fields["voice"],
                "priority": alert_fields["priority"],
                "repeat": alert_fields["repeat"],
                "chime": alert_fields["chime"]
            }),
            &["namespace", "name"],
        ),
        tool(
            "sync_namespace",
            "Replace a namespace wholesale: apply every listed alert (created, replaced, or left as \
             it is when identical) and prune stored alerts missing from the set — e.g. re-upload \
             today's calendar in one call.",
            json!({
                "namespace": { "type": "string" },
                "alerts": { "type": "object", "description": "Alert name → alert body for apply_alert \
                    (same fields, plus optional `at`), e.g. {\"standup\": {\"text\": \"…\", \
                    \"at\": \"2026-11-02 08:55\"}}" }
            }),
            &["namespace", "alerts"],
        ),
        tool(
            "get_alert",
            "Return one stored alert, or {\"found\": false}.",
            json!({
                "namespace": { "type": "string" },
                "name": { "type": "string" }
            }),
            &["namespace", "name"],
        ),
        tool(
            "delete_alert",
            "Delete one stored alert: a waiting one leaves the queue, a playing one stops, a \
             scheduled one never happens.",
            json!({
                "namespace": { "type": "string" },
                "name": { "type": "string" }
            }),
            &["namespace", "name"],
        ),
        tool(
            "list_namespaces",
            "Every namespace with its playing, pending and scheduled alerts, including due times.",
            json!({}),
            &[],
        ),
        tool(
            "delete_namespace",
            "Delete a whole namespace and every alert in it.",
            json!({ "namespace": { "type": "string" } }),
            &["namespace"],
        ),
        tool(
            "get_status",
            "What is playing, queued, stored and recently played; volume, quiet hours, background \
             input and AirPlay receivers.",
            json!({}),
            &[],
        ),
        tool(
            "skip_current",
            "Skip the alert playing right now.",
            json!({}),
            &[],
        ),
        tool(
            "clear_alerts",
            "Clear every queued (not playing) alert.",
            json!({}),
            &[],
        ),
        tool(
            "set_volume",
            "Set the alert volume, 0.0 to 1.0.",
            json!({ "volume": { "type": "number", "minimum": 0, "maximum": 1 } }),
            &["volume"],
        ),
        tool(
            "play_background",
            "Play background music: an http(s) stream (e.g. Icecast, fallback URLs allowed) or a \
             looped local file, at an optional gain_db (default -6).",
            json!({
                "url": { "type": "string", "description": "http(s) stream URL" },
                "urls": { "type": "array", "items": { "type": "string" },
                    "description": "More URLs, tried in order when earlier ones fail" },
                "file": { "type": "string", "description": "Local file, looped" },
                "gain_db": { "type": "number", "description": "e.g. -12 for quiet" }
            }),
            &[],
        ),
        tool(
            "stop_background",
            "Stop the background music.",
            json!({}),
            &[],
        ),
    ]
}

/// `{"content": [{"type": "text", "text": ...}], "isError": ...}` — the tool result shape.
fn tool_body(text: &str, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error
    })
}

fn rpc_ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Answer as application/json, or as a one-event SSE stream when the client refuses JSON.
fn answered(value: Value, headers: &HeaderMap) -> Response {
    let accepts = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if accepts.contains("text/event-stream") && !accepts.contains("application/json") {
        (
            [(header::CONTENT_TYPE, "text/event-stream")],
            format!("event: message\ndata: {value}\n\n"),
        )
            .into_response()
    } else {
        (StatusCode::OK, Json(value)).into_response()
    }
}

/// This endpoint is stateless: there is nothing to open (GET) or terminate (DELETE) here.
async fn reject_mcp() -> (StatusCode, &'static str) {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        "unsonido's MCP endpoint is stateless: POST /mcp with JSON-RPC",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_are_well_formed() {
        let tools = tools();
        assert_eq!(tools.len(), 13);
        let mut names = Vec::new();
        for tool in &tools {
            let name = tool["name"].as_str().expect("tool name");
            assert!(!tool["description"].as_str().unwrap().is_empty());
            assert_eq!(tool["inputSchema"]["type"], "object");
            names.push(name.to_string());
        }
        // Unique, and the essentials are present.
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before);
        for essential in ["play_alert", "apply_alert", "sync_namespace", "get_status"] {
            assert!(
                names.contains(&essential.to_string()),
                "missing {essential}"
            );
        }
    }

    #[test]
    fn tool_schemas_mark_their_required_arguments() {
        let tools = tools();
        let apply = tools.iter().find(|t| t["name"] == "apply_alert").unwrap();
        assert_eq!(
            apply["inputSchema"]["required"],
            json!(["namespace", "name"])
        );
        // The referenced alert fields made it into the schema.
        assert_eq!(
            apply["inputSchema"]["properties"]["priority"]["enum"]
                .as_array()
                .unwrap()
                .len(),
            2
        );

        let play = tools.iter().find(|t| t["name"] == "play_alert").unwrap();
        assert!(play["inputSchema"].get("required").is_none());
    }

    #[test]
    fn empty_arguments_parse_and_unknown_fields_are_rejected() {
        let _ = parse_args::<serde_json::Value>(&json!({})).expect("empty arguments are valid");
        let err = serde_json::from_value::<AlertRequest>(json!({"nope": 1}))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown field"), "{err}");
    }

    #[test]
    fn initialize_echoes_supported_revisions() {
        for v in SUPPORTED {
            let params = json!({ "protocolVersion": v, "capabilities": {}, "clientInfo": {} });
            let body = initialize(json!(1), Some(params));
            assert_eq!(body["result"]["protocolVersion"], v);
        }
        let body = initialize(json!(1), Some(json!({ "protocolVersion": "1999-01-01" })));
        assert_eq!(body["result"]["protocolVersion"], PROTOCOL_VERSION);
    }
}
