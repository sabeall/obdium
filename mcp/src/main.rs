//! obd-mcp — a Model Context Protocol (MCP) server that exposes OBDium's
//! ELM327 diagnostics to an MCP client (e.g. Claude Code) over stdio.
//!
//! It reuses the `obdium` library's OBD-II protocol, PID decoding, DTC
//! descriptions and VIN reading, and surfaces them as MCP tools so an LLM
//! can read live vehicle data and trouble codes to help troubleshoot a car.
//!
//! Transport: line-delimited JSON-RPC 2.0 on stdin/stdout, per the MCP
//! stdio transport. All human/debug logging goes to stderr so it never
//! corrupts the protocol stream.

use std::io::{BufRead, Write};

use obdium::diagnostics::TroubleCode;
use obdium::scalar::{Scalar, Unit};
use obdium::{BankNumber, OBD};
use serde_json::{json, Value};

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "obdium-mcp";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The library uses `println!` for debug output, which would corrupt the
/// JSON-RPC stream on stdout. On Unix we redirect the process's stdout (fd 1)
/// to stderr (fd 2) and keep a private handle to the *original* stdout for
/// protocol output. On other platforms we just use stdout directly.
#[cfg(unix)]
fn take_protocol_stdout() -> std::fs::File {
    use std::os::fd::FromRawFd;
    unsafe {
        let saved = libc::dup(1); // clone original stdout
        libc::dup2(2, 1); // point fd 1 at stderr so println! is harmless
        std::fs::File::from_raw_fd(saved)
    }
}

#[cfg(not(unix))]
fn take_protocol_stdout() -> std::io::Stdout {
    std::io::stdout()
}

/// The library reads its SQLite databases and demo data via relative paths
/// (`./data/...`), so it must run with the working directory set to the folder
/// that contains `data/` (the `obdium` crate: `backend/`). Rather than force
/// that on the MCP client config, we locate it ourselves: check the current
/// dir, then walk up from the running executable. Since this crate lives in a
/// workspace, the data folder is a sibling (`backend/data`), so at each
/// ancestor we also probe a `backend/` subdirectory. Failing that we warn on
/// stderr and carry on so tools that don't need the DBs still work.
fn ensure_data_dir() {
    const MARKER: &str = "data/code-descriptions.sqlite";

    if std::path::Path::new(MARKER).exists() {
        return;
    }

    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors() {
            // Skip candidates inside a build dir (e.g. target/release/data is a
            // stub copied by the build); we want the real crate data/.
            if ancestor.components().any(|c| c.as_os_str() == "target") {
                continue;
            }
            for candidate in [ancestor.to_path_buf(), ancestor.join("backend")] {
                if candidate.join(MARKER).exists() {
                    let _ = std::env::set_current_dir(&candidate);
                    eprintln!(
                        "[obd-mcp] working directory set to {}",
                        candidate.display()
                    );
                    return;
                }
            }
        }
    }

    eprintln!(
        "[obd-mcp] warning: could not find `{MARKER}`. Trouble-code descriptions \
         and demo mode may be unavailable. Launch from `backend/` or set `cwd`."
    );
}

fn main() {
    ensure_data_dir();
    let mut out = take_protocol_stdout();
    let stdin = std::io::stdin();
    let mut obd = OBD::new();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }

        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[obd-mcp] failed to parse request: {e}");
                continue;
            }
        };

        // Notifications have no `id` and must not receive a response.
        let id = request.get("id").cloned();
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let params = request.get("params").cloned().unwrap_or(Value::Null);

        let response = handle(&mut obd, method, params, id.clone());

        if let (Some(id), Some(response)) = (id, response) {
            let envelope = match response {
                Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                Err((code, message)) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": code, "message": message}
                }),
            };
            if writeln!(out, "{envelope}").and_then(|_| out.flush()).is_err() {
                break;
            }
        }
    }
}

type RpcResult = Result<Value, (i64, String)>;

/// Dispatch a JSON-RPC method. Returns `None` for notifications (no reply).
fn handle(obd: &mut OBD, method: &str, params: Value, id: Option<Value>) -> Option<RpcResult> {
    match method {
        "initialize" => Some(Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION}
        }))),
        "notifications/initialized" | "notifications/cancelled" => None,
        "ping" => Some(Ok(json!({}))),
        "tools/list" => Some(Ok(json!({"tools": tool_definitions()}))),
        "tools/call" => Some(call_tool(obd, params)),
        _ => {
            // Only reply to requests (those with an id), not notifications.
            id.map(|_| Err((-32601, format!("method not found: {method}"))))
        }
    }
}

fn tool_definitions() -> Value {
    let empty = json!({"type": "object", "properties": {}});
    json!([
        {
            "name": "list_serial_ports",
            "description": "List serial ports that appear to have an ELM327 OBD-II adapter attached. A Bluetooth adapter shows up here once paired at the OS level.",
            "inputSchema": empty
        },
        {
            "name": "connect",
            "description": "Connect to an ELM327 adapter on a serial port. Set demo=true to replay recorded sample data instead of using real hardware. Call this before reading data.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "port": {"type": "string", "description": "Serial port name, e.g. /dev/tty.OBDII or COM3. Ignored when demo=true."},
                    "baud_rate": {"type": "integer", "description": "Baud rate. Defaults to 38400."},
                    "protocol": {"type": "integer", "description": "OBD-II protocol number 0-9. 0 = auto-detect (default)."},
                    "demo": {"type": "boolean", "description": "Replay recorded sample data instead of connecting to hardware."}
                }
            }
        },
        {
            "name": "disconnect",
            "description": "Disconnect from the current OBD-II adapter.",
            "inputSchema": empty
        },
        {
            "name": "status",
            "description": "Report whether an adapter is connected, the active serial port, baud rate and OBD-II protocol.",
            "inputSchema": empty
        },
        {
            "name": "read_trouble_codes",
            "description": "Read diagnostic trouble codes (DTCs): current/stored, permanent, and freeze-frame, each with a plain-language description. Also reports the check-engine (MIL) light state.",
            "inputSchema": empty
        },
        {
            "name": "clear_trouble_codes",
            "description": "Clear stored diagnostic trouble codes and turn off the check-engine light (OBD-II service 04). This is destructive: it erases stored codes and freeze-frame data and resets readiness monitors. Only call when the user explicitly asks.",
            "inputSchema": empty
        },
        {
            "name": "read_live_data",
            "description": "Read a snapshot of live sensor values (RPM, speed, coolant temp, engine load, throttle, fuel trims, MAF, intake, module voltage, etc). Sensors the vehicle does not support are reported as no data.",
            "inputSchema": empty
        },
        {
            "name": "read_vin",
            "description": "Read the vehicle's VIN (Vehicle Identification Number) from the ECU.",
            "inputSchema": empty
        }
    ])
}

fn call_tool(obd: &mut OBD, params: Value) -> RpcResult {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);

    let result: Result<Value, String> = match name {
        "list_serial_ports" => Ok(tool_list_serial_ports()),
        "connect" => tool_connect(obd, &args),
        "disconnect" => {
            obd.disconnect();
            Ok(json!({"connected": false}))
        }
        "status" => Ok(tool_status(obd)),
        "read_trouble_codes" => tool_read_trouble_codes(obd),
        "clear_trouble_codes" => tool_clear_trouble_codes(obd),
        "read_live_data" => tool_read_live_data(obd),
        "read_vin" => tool_read_vin(obd),
        other => Err(format!("unknown tool: {other}")),
    };

    // MCP tool results are returned as `content` blocks; tool-level failures
    // are signalled with `isError: true` rather than a JSON-RPC error.
    Ok(match result {
        Ok(value) => json!({
            "content": [{"type": "text", "text": pretty(&value)}],
            "isError": false
        }),
        Err(message) => json!({
            "content": [{"type": "text", "text": message}],
            "isError": true
        }),
    })
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn require_connection(obd: &OBD) -> Result<(), String> {
    if obd.is_connected() {
        Ok(())
    } else {
        Err("Not connected. Call the `connect` tool first (use demo=true to try sample data).".into())
    }
}

fn tool_list_serial_ports() -> Value {
    let ports: Vec<Value> = OBD::get_open_serial_ports()
        .into_iter()
        .map(|(port, baud)| json!({"port": port, "baud_rate": baud}))
        .collect();
    json!({
        "ports": ports,
        "note": "A Bluetooth ELM327 must be paired at the OS level first so it appears as a serial port. BLE-only clones do not create a serial port on macOS."
    })
}

fn tool_connect(obd: &mut OBD, args: &Value) -> Result<Value, String> {
    let demo = args.get("demo").and_then(Value::as_bool).unwrap_or(false);
    let baud = args
        .get("baud_rate")
        .and_then(Value::as_u64)
        .unwrap_or(38400) as u32;
    let protocol = args.get("protocol").and_then(Value::as_u64).unwrap_or(0) as u8;

    let port = if demo {
        "DEMO MODE".to_string()
    } else {
        match args.get("port").and_then(Value::as_str) {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => return Err("`port` is required unless demo=true.".into()),
        }
    };

    match obd.connect(&port, baud, protocol) {
        Ok(()) => Ok(json!({
            "connected": obd.is_connected(),
            "demo": demo,
            "port": obd.serial_port_name(),
            "baud_rate": obd.serial_port_baud_rate(),
        })),
        Err(e) => Err(format!("Failed to connect: {e}")),
    }
}

fn tool_status(obd: &mut OBD) -> Value {
    let protocol = if obd.is_connected() {
        obd.get_protocol_name().ok()
    } else {
        None
    };
    json!({
        "connected": obd.is_connected(),
        "port": obd.serial_port_name(),
        "baud_rate": obd.serial_port_baud_rate(),
        "protocol_number": obd.get_protocol_number(),
        "protocol_name": protocol,
    })
}

fn trouble_code_json(code: &TroubleCode) -> Value {
    json!({
        "code": code.dtc,
        "description": code.description,
        "category": code.category.to_string(),
        "permanent": code.permanant,
    })
}

fn tool_read_trouble_codes(obd: &mut OBD) -> Result<Value, String> {
    require_connection(obd)?;

    let check_engine = obd.has_check_engine_light();
    let count = obd.get_num_trouble_codes();
    let current: Vec<Value> = obd.get_trouble_codes().iter().map(trouble_code_json).collect();
    let permanent: Vec<Value> = obd
        .get_permanant_trouble_codes()
        .iter()
        .map(trouble_code_json)
        .collect();
    let freeze_frame: Vec<Value> = obd
        .get_freeze_frame_dtc()
        .iter()
        .map(trouble_code_json)
        .collect();

    Ok(json!({
        "check_engine_light": check_engine,
        "reported_count": count,
        "current": current,
        "permanent": permanent,
        "freeze_frame": freeze_frame,
    }))
}

fn tool_clear_trouble_codes(obd: &mut OBD) -> Result<Value, String> {
    require_connection(obd)?;
    match obd.clear_trouble_codes() {
        Ok(()) => Ok(json!({"cleared": true})),
        Err(e) => Err(format!("Failed to clear trouble codes: {e}")),
    }
}

/// Serialize a Scalar as a value/unit/display object, or null-ish for no data.
fn scalar_json(s: &Scalar) -> Value {
    if s.unit == Unit::NoData {
        json!({"available": false, "display": "NO DATA"})
    } else {
        json!({
            "available": true,
            "value": s.value,
            "unit": s.unit.as_str(),
            "display": s.to_string(),
        })
    }
}

fn tool_read_live_data(obd: &mut OBD) -> Result<Value, String> {
    require_connection(obd)?;

    // Ordered list of (label, reading). Kept explicit so the set is obvious.
    let readings = json!({
        "engine_rpm": scalar_json(&obd.rpm()),
        "vehicle_speed": scalar_json(&obd.vehicle_speed()),
        "engine_load": scalar_json(&obd.engine_load()),
        "coolant_temp": scalar_json(&obd.coolant_temp()),
        "intake_air_temp": scalar_json(&obd.intake_air_temp()),
        "ambient_air_temp": scalar_json(&obd.ambient_air_temp()),
        "intake_manifold_pressure": scalar_json(&obd.intake_manifold_abs_pressure()),
        "maf_air_flow_rate": scalar_json(&obd.maf_air_flow_rate()),
        "throttle_position": scalar_json(&obd.throttle_position()),
        "timing_advance": scalar_json(&obd.timing_advance()),
        "short_term_fuel_trim_bank1": scalar_json(&obd.short_term_fuel_trim(&BankNumber::Bank1)),
        "long_term_fuel_trim_bank1": scalar_json(&obd.long_term_fuel_trim(&BankNumber::Bank1)),
        "short_term_fuel_trim_bank2": scalar_json(&obd.short_term_fuel_trim(&BankNumber::Bank2)),
        "long_term_fuel_trim_bank2": scalar_json(&obd.long_term_fuel_trim(&BankNumber::Bank2)),
        "fuel_tank_level": scalar_json(&obd.fuel_tank_level()),
        "control_module_voltage": scalar_json(&obd.control_module_voltage()),
        "engine_runtime": scalar_json(&obd.engine_runtime()),
    });

    Ok(readings)
}

fn tool_read_vin(obd: &mut OBD) -> Result<Value, String> {
    require_connection(obd)?;
    match obd.get_vin() {
        Some(vin) => Ok(json!({"vin": vin.get_vin()})),
        None => Err("Could not read a VIN from the vehicle.".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use obdium::diagnostics::TroubleCode;

    // --- scalar serialization ---------------------------------------------

    #[test]
    fn scalar_json_reports_available_reading() {
        let s = Scalar {
            value: 60.0,
            unit: Unit::Celsius,
        };
        let v = scalar_json(&s);
        assert_eq!(v["available"], json!(true));
        assert_eq!(v["value"], json!(60.0));
        assert_eq!(v["unit"], json!("°C"));
        assert_eq!(v["display"], json!("60°C"));
    }

    #[test]
    fn scalar_json_reports_no_data() {
        let v = scalar_json(&Scalar::no_data());
        assert_eq!(v["available"], json!(false));
        assert_eq!(v["display"], json!("NO DATA"));
        assert!(v.get("value").is_none());
    }

    // --- trouble code serialization ---------------------------------------

    #[test]
    fn trouble_code_json_maps_fields() {
        let code = TroubleCode {
            category: Default::default(),
            dtc: "P0301".to_string(),
            description: "Cylinder 1 Misfire Detected".to_string(),
            permanant: true,
        };
        let v = trouble_code_json(&code);
        assert_eq!(v["code"], json!("P0301"));
        assert_eq!(v["description"], json!("Cylinder 1 Misfire Detected"));
        assert_eq!(v["permanent"], json!(true));
        assert!(v["category"].is_string());
    }

    // --- JSON-RPC dispatch -------------------------------------------------

    #[test]
    fn initialize_returns_handshake() {
        let mut obd = OBD::new();
        let res = handle(&mut obd, "initialize", Value::Null, Some(json!(1)))
            .expect("initialize must reply")
            .expect("initialize must succeed");
        assert_eq!(res["protocolVersion"], json!(PROTOCOL_VERSION));
        assert_eq!(res["serverInfo"]["name"], json!(SERVER_NAME));
        assert!(res["capabilities"]["tools"].is_object());
    }

    #[test]
    fn initialized_notification_has_no_reply() {
        let mut obd = OBD::new();
        // A notification has no id and must not produce a response.
        assert!(handle(&mut obd, "notifications/initialized", Value::Null, None).is_none());
    }

    #[test]
    fn tools_list_advertises_all_tools() {
        let mut obd = OBD::new();
        let res = handle(&mut obd, "tools/list", Value::Null, Some(json!(2)))
            .unwrap()
            .unwrap();
        let tools = res["tools"].as_array().expect("tools array");
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        for expected in [
            "list_serial_ports",
            "connect",
            "disconnect",
            "status",
            "read_trouble_codes",
            "clear_trouble_codes",
            "read_live_data",
            "read_vin",
        ] {
            assert!(names.contains(&expected), "missing tool {expected}");
        }
        // Every tool must carry a JSON-schema object.
        assert!(tools.iter().all(|t| t["inputSchema"]["type"] == json!("object")));
    }

    #[test]
    fn unknown_method_with_id_is_method_not_found() {
        let mut obd = OBD::new();
        let err = handle(&mut obd, "does/not/exist", Value::Null, Some(json!(9)))
            .expect("request must reply")
            .expect_err("must be an error");
        assert_eq!(err.0, -32601);
    }

    #[test]
    fn unknown_notification_has_no_reply() {
        let mut obd = OBD::new();
        assert!(handle(&mut obd, "does/not/exist", Value::Null, None).is_none());
    }

    // --- tool-level behavior ----------------------------------------------

    #[test]
    fn reading_before_connect_is_a_tool_error() {
        let mut obd = OBD::new();
        let params = json!({"name": "read_live_data", "arguments": {}});
        let res = call_tool(&mut obd, params).expect("tools/call replies");
        assert_eq!(res["isError"], json!(true));
        assert!(res["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Not connected"));
    }

    #[test]
    fn calling_unknown_tool_is_a_tool_error() {
        let mut obd = OBD::new();
        let params = json!({"name": "frobnicate", "arguments": {}});
        let res = call_tool(&mut obd, params).unwrap();
        assert_eq!(res["isError"], json!(true));
        assert!(res["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown tool"));
    }
}
