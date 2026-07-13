//! Integration tests for the `obd-mcp` MCP server.
//!
//! These spawn the actual compiled binary and drive it over stdio with
//! JSON-RPC, exercising the real transport (including the stdout redirect that
//! keeps the library's debug output from corrupting the protocol stream) and
//! the demo-mode replay path, so no OBD hardware is required.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

/// Run the server, feed it the given JSON-RPC request lines, and return the
/// responses keyed by their `id`. Requests without an `id` (notifications) are
/// sent but produce no response, as required by the protocol.
fn run_session(requests: &[Value]) -> Vec<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_obd-mcp"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null()) // library debug output lives here; ignore it
        .spawn()
        .expect("failed to spawn obd-mcp");

    // Write on a separate thread so a full stdout pipe can never deadlock us.
    let mut stdin = child.stdin.take().unwrap();
    let payload: String = requests
        .iter()
        .map(|r| format!("{r}\n"))
        .collect();
    let writer = std::thread::spawn(move || {
        stdin.write_all(payload.as_bytes()).unwrap();
        // Drop stdin to signal EOF so the server exits.
    });

    let stdout = child.stdout.take().unwrap();
    let mut responses = Vec::new();
    for line in BufReader::new(stdout).lines() {
        let line = line.unwrap();
        if line.trim().is_empty() {
            continue;
        }
        // Every emitted line MUST be valid JSON — this is the regression guard
        // against the library's stdout debug output leaking into the stream.
        let value: Value =
            serde_json::from_str(&line).unwrap_or_else(|e| panic!("non-JSON line {line:?}: {e}"));
        responses.push(value);
    }

    writer.join().unwrap();
    let _ = child.wait();
    responses
}

fn by_id(responses: &[Value], id: i64) -> &Value {
    responses
        .iter()
        .find(|r| r["id"] == json!(id))
        .unwrap_or_else(|| panic!("no response for id {id}"))
}

/// Extract the text payload of a successful `tools/call` result and parse it as
/// JSON. Panics if the tool reported an error.
fn tool_json(response: &Value) -> Value {
    let result = &response["result"];
    assert_eq!(result["isError"], json!(false), "tool returned an error: {result}");
    let text = result["content"][0]["text"].as_str().expect("text content");
    serde_json::from_str(text).unwrap_or_else(|e| panic!("tool text not JSON: {text:?}: {e}"))
}

#[test]
fn initialize_handshake() {
    let responses = run_session(&[json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}
    })]);

    let init = by_id(&responses, 1);
    assert_eq!(init["jsonrpc"], json!("2.0"));
    assert_eq!(init["result"]["protocolVersion"], json!("2024-11-05"));
    assert!(init["result"]["serverInfo"]["name"].is_string());
}

#[test]
fn notification_receives_no_response() {
    let responses = run_session(&[
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
    ]);

    // Exactly one response (for the initialize request); the notification is silent.
    assert_eq!(responses.len(), 1);
    assert_eq!(responses[0]["id"], json!(1));
}

#[test]
fn tools_list_is_advertised() {
    let responses = run_session(&[json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}
    })]);

    let tools = by_id(&responses, 1)["result"]["tools"]
        .as_array()
        .expect("tools array");
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"connect"));
    assert!(names.contains(&"read_trouble_codes"));
    assert!(names.contains(&"read_live_data"));
}

#[test]
fn demo_connect_then_read_live_data() {
    let responses = run_session(&[
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
               "params": {"name": "connect", "arguments": {"demo": true}}}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
               "params": {"name": "read_live_data", "arguments": {}}}),
    ]);

    let connect = tool_json(by_id(&responses, 2));
    assert_eq!(connect["connected"], json!(true));
    assert_eq!(connect["mode"], json!("demo"));

    // Live data is a structured snapshot; each reading declares availability.
    let live = tool_json(by_id(&responses, 3));
    for key in ["engine_rpm", "coolant_temp", "vehicle_speed"] {
        assert!(live.get(key).is_some(), "missing reading {key}");
        assert!(live[key]["available"].is_boolean(), "{key} missing availability flag");
    }
}

#[test]
fn demo_read_trouble_codes_shape() {
    let responses = run_session(&[
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
               "params": {"name": "connect", "arguments": {"demo": true}}}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
               "params": {"name": "read_trouble_codes", "arguments": {}}}),
    ]);

    let dtc = tool_json(by_id(&responses, 2));
    assert!(dtc["check_engine_light"].is_boolean());
    for bucket in ["current", "permanent", "freeze_frame"] {
        assert!(dtc[bucket].is_array(), "{bucket} should be an array");
    }
}

#[test]
fn reading_before_connect_is_a_tool_error() {
    let responses = run_session(&[json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "read_live_data", "arguments": {}}
    })]);

    let result = &by_id(&responses, 1)["result"];
    assert_eq!(result["isError"], json!(true));
    assert!(result["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Not connected"));
}

#[test]
fn unknown_method_returns_json_rpc_error() {
    let responses = run_session(&[json!({
        "jsonrpc": "2.0", "id": 1, "method": "bogus/method", "params": {}
    })]);

    assert_eq!(by_id(&responses, 1)["error"]["code"], json!(-32601));
}

#[test]
fn simulate_connect_reports_healthy_and_reads_coherent_live_data() {
    let responses = run_session(&[
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
               "params": {"name": "connect", "arguments": {"simulate": true}}}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
               "params": {"name": "read_live_data", "arguments": {}}}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
               "params": {"name": "read_trouble_codes", "arguments": {}}}),
    ]);

    let connect = tool_json(by_id(&responses, 1));
    assert_eq!(connect["mode"], json!("simulate"));
    assert_eq!(connect["scenario"], json!("healthy"));

    // Live readings must be present, available, and in plausible ranges.
    let live = tool_json(by_id(&responses, 2));
    let rpm = live["engine_rpm"]["value"].as_f64().expect("rpm value");
    assert!((600.0..=6500.0).contains(&rpm), "rpm out of range: {rpm}");
    assert_eq!(live["engine_rpm"]["available"], json!(true));
    let coolant = live["coolant_temp"]["value"].as_f64().expect("coolant value");
    assert!((10.0..=115.0).contains(&coolant), "coolant out of range: {coolant}");

    // A healthy car: no codes, no check-engine light.
    let dtc = tool_json(by_id(&responses, 3));
    assert_eq!(dtc["check_engine_light"], json!(false));
    assert!(dtc["current"].as_array().unwrap().is_empty());
}

#[test]
fn simulate_vacuum_leak_sets_code_and_clearing_turns_it_off() {
    let responses = run_session(&[
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
               "params": {"name": "connect", "arguments": {"simulate": true, "scenario": "vacuum_leak"}}}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
               "params": {"name": "read_trouble_codes", "arguments": {}}}),
        json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
               "params": {"name": "clear_trouble_codes", "arguments": {}}}),
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
               "params": {"name": "read_trouble_codes", "arguments": {}}}),
    ]);

    // Fault present: MIL on, P0171 reported.
    let before = tool_json(by_id(&responses, 2));
    assert_eq!(before["check_engine_light"], json!(true));
    let codes = before["current"].as_array().unwrap();
    assert!(
        codes.iter().any(|c| c["code"] == json!("P0171")),
        "expected P0171 in {codes:?}"
    );

    // After clearing: MIL off, no codes.
    let after = tool_json(by_id(&responses, 4));
    assert_eq!(after["check_engine_light"], json!(false));
    assert!(after["current"].as_array().unwrap().is_empty());
}

#[test]
fn initialize_advertises_the_answer_structure() {
    let responses = run_session(&[json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}
    })]);
    let instructions = by_id(&responses, 1)["result"]["instructions"]
        .as_str()
        .expect("instructions string");
    for section in ["Diagnostics", "Common Problems", "Forums", "NHTSA", "Summary", "Checklist"] {
        assert!(instructions.contains(section), "missing {section}");
    }
}

#[test]
fn diagnose_bundles_codes_live_data_and_structure() {
    let responses = run_session(&[
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
               "params": {"name": "connect", "arguments": {"simulate": true, "scenario": "overheat"}}}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
               "params": {"name": "diagnose", "arguments": {}}}),
    ]);

    let report = tool_json(by_id(&responses, 2));
    assert!(report["trouble_codes"]["current"].is_array());
    assert!(report["live_data"]["coolant_temp"].is_object());
    let headings: Vec<&str> = report["presentation"]["sections"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["heading"].as_str())
        .collect();
    assert_eq!(
        headings,
        ["Diagnostics", "Common Problems", "Forums", "NHTSA", "Summary", "Checklist"]
    );
}

#[test]
fn simulate_rejects_unknown_scenario() {
    let responses = run_session(&[json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "connect", "arguments": {"simulate": true, "scenario": "banana"}}
    })]);

    let result = &by_id(&responses, 1)["result"];
    assert_eq!(result["isError"], json!(true));
    assert!(result["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Unknown scenario"));
}
