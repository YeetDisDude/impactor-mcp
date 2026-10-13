//! `plumemcp` - a Model Context Protocol server for Impactor.
//!
//! Speaks JSON-RPC 2.0 over stdio (newline-delimited messages). Every byte
//! written to stdout must be a protocol message, so all logging goes to stderr.

mod common;
mod tools;

use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::common::{Ctx, Progress};

const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

const INSTRUCTIONS: &str = "Impactor sideloading server. Typical flow: `account_list` / \
`account_login` (+ `account_submit_2fa`) -> `device_list` -> `package_inspect` -> \
`sign_and_install`. Account data is shared with the Impactor GUI and plumesign CLI \
(accounts.json in the PlumeImpactor config directory). Tools that may need to create a new \
development certificate accept `allow_certificate_reset`; leave it false unless the user \
explicitly agreed, because resetting revokes existing certificates and breaks existing \
SideStore/AltStore installs.";

pub type Writer = Arc<Mutex<std::io::Stdout>>;

pub fn write_message(writer: &Writer, message: &Value) {
    if let Ok(mut out) = writer.lock() {
        let _ = writeln!(out, "{}", message);
        let _ = out.flush();
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stderr)
        .init();

    let _ = rustls::crypto::ring::default_provider().install_default();

    let writer: Writer = Arc::new(Mutex::new(std::io::stdout()));

    if std::env::var("PLUME_MCP_AUTO_REFRESH").is_ok_and(|v| v == "1") {
        tools::refresh::spawn_auto_refresh();
    }

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let message: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                write_message(
                    &writer,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": null,
                        "error": { "code": -32700, "message": format!("Parse error: {e}") }
                    }),
                );
                continue;
            }
        };

        // Batches were removed from the protocol in 2025-06-18, but older
        // clients may still send them.
        if let Value::Array(batch) = message {
            for m in batch {
                handle_message(m, &writer);
            }
        } else {
            handle_message(message, &writer);
        }
    }
}

fn handle_message(message: Value, writer: &Writer) {
    let id = message.get("id").cloned();
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let params = message.get("params").cloned().unwrap_or(Value::Null);

    // Notifications (no id) and responses to our own requests need no reply.
    let Some(id) = id else { return };
    if method.is_empty() {
        return;
    }

    match method.as_str() {
        "initialize" => {
            let requested = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(SUPPORTED_PROTOCOL_VERSIONS[0]);
            let version = if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
                requested
            } else {
                SUPPORTED_PROTOCOL_VERSIONS[0]
            };
            respond(
                writer,
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": {
                        "name": "plume-impactor",
                        "title": "Impactor",
                        "version": env!("CARGO_PKG_VERSION")
                    },
                    "instructions": INSTRUCTIONS
                }),
            );
        }
        "ping" => respond(writer, id, json!({})),
        "tools/list" => respond(writer, id, json!({ "tools": tools::definitions() })),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            let progress_token = params
                .get("_meta")
                .and_then(|m| m.get("progressToken"))
                .cloned();
            let writer = writer.clone();

            // Each call gets its own thread + single-threaded runtime (the same
            // model the GUI uses). Many plume futures are not `Send`, and this
            // keeps long installs from blocking other requests.
            std::thread::spawn(move || {
                let ctx = Ctx {
                    progress: Progress::new(writer.clone(), progress_token),
                };
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("failed to build tokio runtime");

                // plume crates unwrap on unexpected server responses; report a
                // panic as a tool error instead of leaving the request unanswered.
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    rt.block_on(tools::call(&name, &args, &ctx))
                }))
                .unwrap_or_else(|panic| {
                    let msg = panic
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_else(|| "unknown panic".to_string());
                    Some(Err(format!("Internal error in `{name}`: {msg}")))
                });

                let response = match result {
                    Some(Ok(value)) => tool_result(value, false),
                    Some(Err(err)) => tool_result(Value::String(err), true),
                    None => {
                        write_message(
                            &writer,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": { "code": -32602, "message": format!("Unknown tool: {name}") }
                            }),
                        );
                        return;
                    }
                };
                respond(&writer, id, response);
            });
        }
        "resources/list" => respond(writer, id, json!({ "resources": [] })),
        "prompts/list" => respond(writer, id, json!({ "prompts": [] })),
        _ => write_message(
            writer,
            &json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("Method not found: {method}") }
            }),
        ),
    }
}

fn respond(writer: &Writer, id: Value, result: Value) {
    write_message(writer, &json!({ "jsonrpc": "2.0", "id": id, "result": result }));
}

fn tool_result(value: Value, is_error: bool) -> Value {
    let text = match &value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    };
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error
    })
}
