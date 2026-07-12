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

mod dbc;
mod known_issues;
mod simulator;
use dbc::Dbc;
use known_issues::{KnownIssues, Vehicle};
use simulator::{Scenario, Simulator};

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "obdium-mcp";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Guidance surfaced to the MCP client at `initialize`, so every diagnostic
/// answer is organized consistently. The `diagnose` tool returns the same
/// structure inline with the data.
const SERVER_INSTRUCTIONS: &str = "\
When helping the user troubleshoot a vehicle, ALWAYS organize the answer into \
these sections, in this exact order:\n\
1. Diagnostics — interpret the live sensor data and trouble codes: what is \
normal, what is abnormal, and what it points to.\n\
2. Common Problems — the likely causes for this vehicle and code, ranked from \
most to least likely.\n\
3. NHTSA — real recalls and owner complaints for the vehicle. Call the \
`known_issues` tool to get this; if it is unavailable, say so.\n\
4. Summary — a short, plain-language conclusion the user can act on.\n\
5. Checklist — an ordered, actionable checklist of what the user should do \
next, written as markdown checkboxes (`- [ ] ...`).\n\
The `diagnose` tool bundles the trouble codes and live data together with this \
same section structure, so prefer it as the starting point for a diagnosis.";

/// Server state: the real OBD adapter plus an optional coherent simulator.
/// Exactly one is "connected" at a time; simulator mode short-circuits the
/// hardware path so every read comes from the synthetic drive cycle instead.
struct Server {
    obd: OBD,
    sim: Option<Simulator>,
    known_issues: KnownIssues,
}

impl Server {
    fn new() -> Self {
        Server {
            obd: OBD::new(),
            sim: None,
            known_issues: KnownIssues::default(),
        }
    }

    fn is_connected(&self) -> bool {
        self.sim.is_some() || self.obd.is_connected()
    }
}

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
    let mut server = Server::new();

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

        let response = handle(&mut server, method, params, id.clone());

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
fn handle(server: &mut Server, method: &str, params: Value, id: Option<Value>) -> Option<RpcResult> {
    match method {
        "initialize" => Some(Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
            "instructions": SERVER_INSTRUCTIONS
        }))),
        "notifications/initialized" | "notifications/cancelled" => None,
        "ping" => Some(Ok(json!({}))),
        "tools/list" => Some(Ok(json!({"tools": tool_definitions()}))),
        "tools/call" => Some(call_tool(server, params)),
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
            "name": "diagnose",
            "description": "One-shot diagnostic snapshot: reads the trouble codes and live data together and returns them alongside the required answer structure (Diagnostics, Common Problems, NHTSA, Summary, Checklist). Prefer this as the starting point when the user asks what's wrong. Requires a connection (real, demo, or simulate).",
            "inputSchema": empty
        },
        {
            "name": "list_serial_ports",
            "description": "List serial ports that appear to have an ELM327 OBD-II adapter attached. A Bluetooth adapter shows up here once paired at the OS level.",
            "inputSchema": empty
        },
        {
            "name": "list_ble_devices",
            "description": "Scan for nearby BLE (GATT) ELM327 adapters such as the Veepeak OBDCheck BLE, which do not appear as serial ports. Returns each device's name and id; pass either to `connect` with transport=\"ble\". Requires the server to be built with the `ble` feature.",
            "inputSchema": empty
        },
        {
            "name": "connect",
            "description": "Connect to a vehicle. With no special flags, connects to a real ELM327 adapter on `port` over serial. Set transport=\"ble\" to connect to a BLE (GATT) adapter such as the Veepeak OBDCheck BLE — in that case `port` is the BLE device's advertised name or id (see list_ble_devices) and `baud_rate` is ignored. Set simulate=true for a coherent synthetic drive cycle (engine warms up, idles, accelerates, cruises) with an optional fault `scenario` — best for troubleshooting practice without a car. Set demo=true to replay raw recorded sample data (incoherent, for protocol testing). Call this before reading data.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "port": {"type": "string", "description": "Serial port name (e.g. /dev/tty.OBDII, COM3) or, when transport=ble, the BLE device name/id. Ignored when demo/simulate is set."},
                    "transport": {"type": "string", "description": "Backend to use: \"serial\" (default) or \"ble\"."},
                    "baud_rate": {"type": "integer", "description": "Baud rate. Defaults to 38400. Ignored for BLE."},
                    "protocol": {"type": "integer", "description": "OBD-II protocol number 0-9. 0 = auto-detect (default)."},
                    "demo": {"type": "boolean", "description": "Replay recorded sample data (random per read, not physically coherent)."},
                    "simulate": {"type": "boolean", "description": "Run the coherent vehicle simulator instead of hardware."},
                    "scenario": {"type": "string", "description": "Fault scenario for simulate mode: healthy (default), vacuum_leak, misfire, or overheat. A fault skews the relevant live values and sets a matching trouble code with the check-engine light on."}
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
        },
        {
            "name": "known_issues",
            "description": "Look up real recalls and owner complaints for a vehicle from NHTSA (US public data). Identify the vehicle by `vin`, by explicit `make`+`model`+`year`, or, if omitted, from the currently connected vehicle's VIN. Optional `component` substring filters complaints (e.g. \"engine\", \"fuel\"). Requires network access; results are cached. Great for pairing a live trouble code with known problems on that model.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "vin": {"type": "string", "description": "VIN to decode and look up. Optional if make/model/year given or a vehicle is connected."},
                    "make": {"type": "string", "description": "Vehicle make, e.g. Toyota. Use with model+year to skip VIN decoding."},
                    "model": {"type": "string", "description": "Vehicle model, e.g. 4Runner."},
                    "year": {"type": "integer", "description": "Model year, e.g. 2013."},
                    "component": {"type": "string", "description": "Optional case-insensitive filter for complaints/recalls, e.g. \"engine\" or \"fuel system\"."}
                }
            }
        },
        {
            "name": "dbc_signals",
            "description": "List the manufacturer-specific CAN messages and signals defined in a DBC file (e.g. from commaai/opendbc) — data beyond the generic OBD-II PIDs. Provide the DBC via `dbc` (inline text) or `dbc_path` (file path). Optional `filter` matches message/signal names.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "dbc": {"type": "string", "description": "Inline DBC file contents."},
                    "dbc_path": {"type": "string", "description": "Path to a .dbc file (e.g. an opendbc platform file)."},
                    "filter": {"type": "string", "description": "Optional case-insensitive substring to filter message/signal names."}
                }
            }
        },
        {
            "name": "decode_can",
            "description": "Decode a raw CAN frame into physical signal values using a DBC file (e.g. from commaai/opendbc). Useful for manufacturer-specific signals not exposed as generic OBD-II PIDs. Provide the DBC via `dbc` or `dbc_path`, plus `can_id` (number or hex) and `data` (hex payload). Note: this decodes a frame you supply; it does not sniff the CAN bus itself.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "dbc": {"type": "string", "description": "Inline DBC file contents."},
                    "dbc_path": {"type": "string", "description": "Path to a .dbc file (e.g. an opendbc platform file)."},
                    "can_id": {"description": "CAN arbitration id, as a number or hex string like \"0x2E4\"."},
                    "data": {"type": "string", "description": "CAN payload as hex, e.g. \"0F A0 00 00 00 00 00 00\"."}
                },
                "required": ["can_id", "data"]
            }
        }
    ])
}

fn call_tool(server: &mut Server, params: Value) -> RpcResult {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);

    let result: Result<Value, String> = match name {
        "diagnose" => tool_diagnose(server),
        "list_serial_ports" => Ok(tool_list_serial_ports()),
        "list_ble_devices" => Ok(tool_list_ble_devices()),
        "connect" => tool_connect(server, &args),
        "disconnect" => {
            server.sim = None;
            server.obd.disconnect();
            Ok(json!({"connected": false}))
        }
        "status" => Ok(tool_status(server)),
        "read_trouble_codes" => tool_read_trouble_codes(server),
        "clear_trouble_codes" => tool_clear_trouble_codes(server),
        "read_live_data" => tool_read_live_data(server),
        "read_vin" => tool_read_vin(server),
        "known_issues" => tool_known_issues(server, &args),
        "dbc_signals" => tool_dbc_signals(&args),
        "decode_can" => tool_decode_can(&args),
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

fn require_connection(server: &Server) -> Result<(), String> {
    if server.is_connected() {
        Ok(())
    } else {
        Err("Not connected. Call the `connect` tool first (use simulate=true for a coherent simulated car, or demo=true for raw sample data).".into())
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

fn tool_connect(server: &mut Server, args: &Value) -> Result<Value, String> {
    let demo = args.get("demo").and_then(Value::as_bool).unwrap_or(false);
    let simulate = args.get("simulate").and_then(Value::as_bool).unwrap_or(false);

    // Coherent simulator: no hardware, no recorded replay — a synthetic drive
    // cycle. Takes precedence and requires no port.
    if simulate {
        let scenario_arg = args.get("scenario").and_then(Value::as_str).unwrap_or("");
        let scenario = Scenario::parse(scenario_arg).ok_or_else(|| {
            format!("Unknown scenario `{scenario_arg}`. Valid options: {}.", Scenario::VALID)
        })?;
        server.obd.disconnect();
        server.sim = Some(Simulator::new(scenario));
        return Ok(json!({
            "connected": true,
            "mode": "simulate",
            "scenario": scenario.label(),
            "port": "SIMULATOR",
        }));
    }

    // Any other mode uses the real OBD stack (demo replays recorded data).
    server.sim = None;
    let baud = args
        .get("baud_rate")
        .and_then(Value::as_u64)
        .unwrap_or(38400) as u32;
    let protocol = args.get("protocol").and_then(Value::as_u64).unwrap_or(0) as u8;
    let transport = args.get("transport").and_then(Value::as_str).unwrap_or("serial");

    // BLE (GATT) adapters aren't serial ports; `port` carries the device name/id.
    if transport == "ble" && !demo {
        let identifier = match args.get("port").and_then(Value::as_str) {
            Some(p) if !p.is_empty() => p,
            _ => return Err("`port` (BLE device name or id) is required for transport=ble.".into()),
        };
        return connect_ble(server, identifier, protocol);
    }

    let port = if demo {
        "DEMO MODE".to_string()
    } else {
        match args.get("port").and_then(Value::as_str) {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => return Err("`port` is required unless demo=true or simulate=true.".into()),
        }
    };

    match server.obd.connect(&port, baud, protocol) {
        Ok(()) => Ok(json!({
            "connected": server.obd.is_connected(),
            "mode": if demo { "demo" } else { "hardware" },
            "port": server.obd.serial_port_name(),
            "baud_rate": server.obd.serial_port_baud_rate(),
        })),
        Err(e) => Err(format!("Failed to connect: {e}")),
    }
}

/// List nearby BLE ELM327 adapters. Returns an empty list (with a note) when the
/// server is built without the `ble` feature.
#[cfg(feature = "ble")]
fn tool_list_ble_devices() -> Value {
    let devices: Vec<Value> = obdium::transport::scan_ble_adapters(false)
        .into_iter()
        .map(|(name, id)| json!({"name": name, "id": id}))
        .collect();
    json!({
        "devices": devices,
        "note": "Pass a device `name` or `id` to connect with transport=\"ble\"."
    })
}

#[cfg(not(feature = "ble"))]
fn tool_list_ble_devices() -> Value {
    json!({
        "devices": [],
        "note": "BLE support is not compiled into this build of obd-mcp. Rebuild with `--features ble`."
    })
}

/// Connect to a BLE adapter by advertised name/id. Feature-gated so the default
/// build links without btleplug.
#[cfg(feature = "ble")]
fn connect_ble(server: &mut Server, identifier: &str, protocol: u8) -> Result<Value, String> {
    match server.obd.connect_ble(identifier, protocol) {
        Ok(()) => Ok(json!({
            "connected": server.obd.is_connected(),
            "mode": "hardware",
            "transport": "ble",
            "port": server.obd.serial_port_name(),
        })),
        Err(e) => Err(format!("Failed to connect over BLE: {e}")),
    }
}

#[cfg(not(feature = "ble"))]
fn connect_ble(_server: &mut Server, _identifier: &str, _protocol: u8) -> Result<Value, String> {
    Err("BLE support is not compiled into this build of obd-mcp. Rebuild with `--features ble`.".into())
}

fn tool_status(server: &mut Server) -> Value {
    if let Some(sim) = &server.sim {
        return json!({
            "connected": true,
            "mode": "simulate",
            "scenario": sim.scenario().label(),
            "port": "SIMULATOR",
            "protocol_name": "Simulated ISO 15765-4 (CAN)",
            "elapsed_seconds": sim.elapsed(),
        });
    }
    let obd = &mut server.obd;
    let protocol = if obd.is_connected() {
        obd.get_protocol_name().ok()
    } else {
        None
    };
    json!({
        "connected": obd.is_connected(),
        "mode": if obd.is_connected() { "hardware/demo" } else { "disconnected" },
        "port": obd.serial_port_name(),
        "baud_rate": obd.serial_port_baud_rate(),
        "protocol_number": obd.get_protocol_number(),
        "protocol_name": protocol,
    })
}

/// The required answer structure, returned inline with `diagnose` so the client
/// organizes the response consistently even if it didn't read `instructions`.
fn presentation_guide() -> Value {
    json!({
        "format": "Organize the answer to the user in these sections, in this order. Use markdown headings.",
        "sections": [
            {"heading": "Diagnostics", "content": "Render the provided `chart` (Parameter | Value | Status) as a markdown table so it is easy to read, then interpret it and the trouble codes: what is normal, what is abnormal (see the Status column), and what it points to. Also show the trouble codes as a small table (Code | Description)."},
            {"heading": "Common Problems", "content": "The likely causes for this vehicle and code, ranked from most to least likely."},
            {"heading": "NHTSA", "content": "Real recalls and owner complaints for the vehicle. Call the `known_issues` tool to populate this; if unavailable, say so."},
            {"heading": "Summary", "content": "A short, plain-language conclusion the user can act on."},
            {"heading": "Checklist", "content": "An ordered, actionable checklist of what the user should do next, written as markdown checkboxes (`- [ ] ...`)."}
        ]
    })
}

/// Human labels for live-data keys, in the order they should appear in the
/// chart. Keeps the chart readable and stable.
const LIVE_DATA_LABELS: &[(&str, &str)] = &[
    ("engine_rpm", "Engine RPM"),
    ("vehicle_speed", "Vehicle speed"),
    ("engine_load", "Engine load"),
    ("coolant_temp", "Coolant temp"),
    ("intake_air_temp", "Intake air temp"),
    ("ambient_air_temp", "Ambient air temp"),
    ("intake_manifold_pressure", "Intake MAP"),
    ("maf_air_flow_rate", "MAF airflow"),
    ("throttle_position", "Throttle"),
    ("timing_advance", "Timing advance"),
    ("short_term_fuel_trim_bank1", "Short-term fuel trim (B1)"),
    ("long_term_fuel_trim_bank1", "Long-term fuel trim (B1)"),
    ("short_term_fuel_trim_bank2", "Short-term fuel trim (B2)"),
    ("long_term_fuel_trim_bank2", "Long-term fuel trim (B2)"),
    ("fuel_tank_level", "Fuel level"),
    ("control_module_voltage", "Module voltage"),
    ("engine_runtime", "Engine runtime"),
];

/// An at-a-glance status for a reading — only for values that can be judged
/// universally (independent of vehicle/engine state). Everything else is left
/// blank so we don't over-claim; the model interprets those in prose.
fn classify_reading(key: &str, value: f64) -> String {
    if key.contains("fuel_trim") {
        let a = value.abs();
        if a <= 10.0 {
            "ok".into()
        } else {
            let dir = if value > 0.0 { "lean" } else { "rich" };
            if a > 25.0 {
                format!("⚠ very {dir}")
            } else {
                format!("⚠ high — {dir}")
            }
        }
    } else if key == "control_module_voltage" {
        if value < 13.0 {
            "⚠ low (charging?)".into()
        } else if value > 15.0 {
            "⚠ high".into()
        } else {
            "ok".into()
        }
    } else if key == "coolant_temp" && value >= 110.0 {
        "⚠ overheating".into()
    } else {
        String::new()
    }
}

/// Build a readable chart (Parameter | Value | Status) from a live-data object.
fn live_data_chart(live: &Value) -> Value {
    let rows: Vec<Value> = LIVE_DATA_LABELS
        .iter()
        .filter_map(|(key, label)| {
            let reading = live.get(*key)?;
            let available = reading.get("available").and_then(Value::as_bool).unwrap_or(false);
            let (value, status) = if available {
                let display = reading
                    .get("display")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let status = reading
                    .get("value")
                    .and_then(Value::as_f64)
                    .map(|v| classify_reading(key, v))
                    .unwrap_or_default();
                (display, status)
            } else {
                ("NO DATA".to_string(), "n/a".to_string())
            };
            Some(json!({"parameter": label, "value": value, "status": status}))
        })
        .collect();

    json!({
        "columns": ["Parameter", "Value", "Status"],
        "rows": rows,
    })
}

fn tool_diagnose(server: &mut Server) -> Result<Value, String> {
    require_connection(server)?;
    // Reuse the same readers so diagnose stays in lockstep with the individual
    // tools (works in real, demo and simulate modes).
    let trouble_codes = tool_read_trouble_codes(server)?;
    let live_data = tool_read_live_data(server)?;
    let chart = live_data_chart(&live_data);
    Ok(json!({
        "trouble_codes": trouble_codes,
        "live_data": live_data,
        "chart": chart,
        "presentation": presentation_guide(),
        "hint": "For the NHTSA section, call `known_issues` (by VIN, make/model/year, or the connected vehicle).",
    }))
}

fn trouble_code_json(code: &TroubleCode) -> Value {
    json!({
        "code": code.dtc,
        "description": code.description,
        "category": code.category.to_string(),
        "permanent": code.permanant,
    })
}

fn tool_read_trouble_codes(server: &mut Server) -> Result<Value, String> {
    require_connection(server)?;

    if let Some(sim) = &server.sim {
        let current: Vec<Value> = sim.trouble_codes().iter().map(trouble_code_json).collect();
        return Ok(json!({
            "check_engine_light": sim.check_engine_light(),
            "reported_count": current.len(),
            "current": current,
            "permanent": [],
            "freeze_frame": [],
        }));
    }

    let obd = &mut server.obd;
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

fn tool_clear_trouble_codes(server: &mut Server) -> Result<Value, String> {
    require_connection(server)?;
    if let Some(sim) = &mut server.sim {
        sim.clear_codes();
        return Ok(json!({"cleared": true}));
    }
    match server.obd.clear_trouble_codes() {
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

/// Build a `Scalar` reading for the simulator (values it produces are always
/// "available", never NoData).
fn sim_scalar(value: f32, unit: Unit) -> Scalar {
    Scalar { value, unit }
}

fn sim_read_live_data(sim: &Simulator) -> Value {
    let f = sim.frame();
    // A single-bank (inline) engine: bank 2 sensors report no data, matching
    // what a typical 4-cylinder returns.
    json!({
        "engine_rpm": scalar_json(&sim_scalar(f.rpm.round(), Unit::RPM)),
        "vehicle_speed": scalar_json(&sim_scalar(f.speed.round(), Unit::KilometersPerHour)),
        "engine_load": scalar_json(&sim_scalar(f.load, Unit::Percent)),
        "coolant_temp": scalar_json(&sim_scalar(f.coolant.round(), Unit::Celsius)),
        "intake_air_temp": scalar_json(&sim_scalar(f.intake_air.round(), Unit::Celsius)),
        "ambient_air_temp": scalar_json(&sim_scalar(f.ambient.round(), Unit::Celsius)),
        "intake_manifold_pressure": scalar_json(&sim_scalar(f.map.round(), Unit::KiloPascal)),
        "maf_air_flow_rate": scalar_json(&sim_scalar(f.maf, Unit::GramsPerSecond)),
        "throttle_position": scalar_json(&sim_scalar(f.throttle, Unit::Percent)),
        "timing_advance": scalar_json(&sim_scalar(f.timing, Unit::Degrees)),
        "short_term_fuel_trim_bank1": scalar_json(&sim_scalar(f.stft1, Unit::Percent)),
        "long_term_fuel_trim_bank1": scalar_json(&sim_scalar(f.ltft1, Unit::Percent)),
        "short_term_fuel_trim_bank2": scalar_json(&Scalar::no_data()),
        "long_term_fuel_trim_bank2": scalar_json(&Scalar::no_data()),
        "fuel_tank_level": scalar_json(&sim_scalar(f.fuel_level, Unit::Percent)),
        "control_module_voltage": scalar_json(&sim_scalar(f.voltage, Unit::Volts)),
        "engine_runtime": scalar_json(&sim_scalar(f.runtime.round(), Unit::Seconds)),
    })
}

fn tool_read_live_data(server: &mut Server) -> Result<Value, String> {
    require_connection(server)?;

    if let Some(sim) = &server.sim {
        return Ok(sim_read_live_data(sim));
    }

    let obd = &mut server.obd;
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

fn tool_read_vin(server: &mut Server) -> Result<Value, String> {
    require_connection(server)?;
    if server.sim.is_some() {
        // A stable, valid-format sample VIN for the simulated vehicle.
        return Ok(json!({"vin": "1HGCM82633A004352", "note": "simulated vehicle"}));
    }
    match server.obd.get_vin() {
        Some(vin) => Ok(json!({"vin": vin.get_vin()})),
        None => Err("Could not read a VIN from the vehicle.".into()),
    }
}

/// Simulated vehicles report this stable, valid-format VIN.
const SIM_VIN: &str = "1HGCM82633A004352";

fn tool_known_issues(server: &mut Server, args: &Value) -> Result<Value, String> {
    let component = args.get("component").and_then(Value::as_str);

    // 1. Prefer explicit make/model/year (no VIN decode / network needed for it).
    let make = args.get("make").and_then(Value::as_str);
    let model = args.get("model").and_then(Value::as_str);
    let year = args
        .get("year")
        .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())));

    if let (Some(make), Some(model), Some(year)) = (make, model, year) {
        let vehicle = Vehicle::basic(make, model, year);
        return server.known_issues.lookup(&vehicle, component);
    }

    // 2. Otherwise we need a VIN: explicit arg, else the connected vehicle.
    let vin = match args.get("vin").and_then(Value::as_str) {
        Some(v) if !v.is_empty() => v.to_string(),
        _ => {
            if server.sim.is_some() {
                SIM_VIN.to_string()
            } else if server.obd.is_connected() {
                server
                    .obd
                    .get_vin()
                    .map(|v| v.get_vin().to_string())
                    .ok_or_else(|| "Connected, but could not read a VIN from the vehicle.".to_string())?
            } else {
                return Err("Provide `vin`, or `make`+`model`+`year`, or connect to a vehicle first.".into());
            }
        }
    };

    let vehicle = server.known_issues.decode_vin(&vin)?;
    server.known_issues.lookup(&vehicle, component)
}

// --- opendbc: CAN signal decoding --------------------------------------------

fn load_dbc_text(args: &Value) -> Result<String, String> {
    if let Some(t) = args.get("dbc").and_then(Value::as_str) {
        if !t.trim().is_empty() {
            return Ok(t.to_string());
        }
    }
    if let Some(p) = args.get("dbc_path").and_then(Value::as_str) {
        return std::fs::read_to_string(p)
            .map_err(|e| format!("failed to read dbc file `{p}`: {e}"));
    }
    Err("Provide `dbc` (inline DBC text) or `dbc_path` (path to a .dbc file, e.g. from opendbc).".into())
}

fn parse_can_id(v: &Value) -> Result<u32, String> {
    if let Some(n) = v.as_u64() {
        return Ok(n as u32);
    }
    if let Some(s) = v.as_str() {
        let s = s.trim();
        return if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            u32::from_str_radix(hex, 16).map_err(|_| format!("invalid hex can_id `{s}`"))
        } else {
            s.parse::<u32>().map_err(|_| format!("invalid can_id `{s}`"))
        };
    }
    Err("`can_id` must be a number or a hex string like \"0x2E4\".".into())
}

fn parse_hex_bytes(s: &str) -> Result<Vec<u8>, String> {
    let cleaned: String = s
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ':' && *c != ',')
        .collect();
    let cleaned = cleaned.strip_prefix("0x").unwrap_or(&cleaned);
    if cleaned.len() % 2 != 0 {
        return Err("hex `data` must have an even number of digits.".into());
    }
    (0..cleaned.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&cleaned[i..i + 2], 16)
                .map_err(|_| format!("invalid hex byte `{}`", &cleaned[i..i + 2]))
        })
        .collect()
}

fn tool_decode_can(args: &Value) -> Result<Value, String> {
    let dbc = Dbc::parse(&load_dbc_text(args)?);
    let can_id = parse_can_id(
        args.get("can_id")
            .ok_or("`can_id` is required (number or hex string).")?,
    )?;
    let data_str = args
        .get("data")
        .and_then(Value::as_str)
        .ok_or("`data` (hex payload, e.g. \"0F A0 00 ...\") is required.")?;
    let data = parse_hex_bytes(data_str)?;

    match dbc.decode(can_id, &data) {
        Some(signals) => Ok(json!({
            "can_id": format!("0x{can_id:X}"),
            "signals": signals.iter().map(|s| json!({
                "name": s.name,
                "value": s.value,
                "unit": s.unit,
                "raw": s.raw,
            })).collect::<Vec<_>>(),
        })),
        None => Err(format!(
            "no message with arbitration id 0x{can_id:X} in the provided DBC."
        )),
    }
}

fn tool_dbc_signals(args: &Value) -> Result<Value, String> {
    let dbc = Dbc::parse(&load_dbc_text(args)?);
    let filter = args
        .get("filter")
        .and_then(Value::as_str)
        .map(|s| s.to_lowercase());

    let messages: Vec<Value> = dbc
        .messages
        .iter()
        .filter(|m| match &filter {
            None => true,
            Some(f) => {
                m.name.to_lowercase().contains(f)
                    || m.signals.iter().any(|s| s.name.to_lowercase().contains(f))
            }
        })
        .map(|m| {
            json!({
                "id": format!("0x{:X}", m.arbitration_id()),
                "name": m.name,
                "extended": m.extended(),
                "signals": m.signals.iter().map(|s| json!({
                    "name": s.name,
                    "unit": s.unit,
                    "bits": format!("{}|{}", s.start_bit, s.length),
                    "endian": if s.little_endian { "little" } else { "big" },
                    "signed": s.signed,
                    "factor": s.factor,
                    "offset": s.offset,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();

    Ok(json!({"message_count": messages.len(), "messages": messages}))
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
        let mut server = Server::new();
        let res = handle(&mut server, "initialize", Value::Null, Some(json!(1)))
            .expect("initialize must reply")
            .expect("initialize must succeed");
        assert_eq!(res["protocolVersion"], json!(PROTOCOL_VERSION));
        assert_eq!(res["serverInfo"]["name"], json!(SERVER_NAME));
        assert!(res["capabilities"]["tools"].is_object());
        // The presentation structure is surfaced to the client at handshake.
        let instructions = res["instructions"].as_str().expect("instructions string");
        for section in ["Diagnostics", "Common Problems", "NHTSA", "Summary", "Checklist"] {
            assert!(instructions.contains(section), "instructions missing {section}");
        }
    }

    #[test]
    fn initialized_notification_has_no_reply() {
        let mut server = Server::new();
        // A notification has no id and must not produce a response.
        assert!(handle(&mut server, "notifications/initialized", Value::Null, None).is_none());
    }

    #[test]
    fn tools_list_advertises_all_tools() {
        let mut server = Server::new();
        let res = handle(&mut server, "tools/list", Value::Null, Some(json!(2)))
            .unwrap()
            .unwrap();
        let tools = res["tools"].as_array().expect("tools array");
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        for expected in [
            "diagnose",
            "list_serial_ports",
            "list_ble_devices",
            "connect",
            "disconnect",
            "status",
            "read_trouble_codes",
            "clear_trouble_codes",
            "read_live_data",
            "read_vin",
            "known_issues",
            "dbc_signals",
            "decode_can",
        ] {
            assert!(names.contains(&expected), "missing tool {expected}");
        }
        // Every tool must carry a JSON-schema object.
        assert!(tools.iter().all(|t| t["inputSchema"]["type"] == json!("object")));
    }

    #[test]
    fn unknown_method_with_id_is_method_not_found() {
        let mut server = Server::new();
        let err = handle(&mut server, "does/not/exist", Value::Null, Some(json!(9)))
            .expect("request must reply")
            .expect_err("must be an error");
        assert_eq!(err.0, -32601);
    }

    #[test]
    fn unknown_notification_has_no_reply() {
        let mut server = Server::new();
        assert!(handle(&mut server, "does/not/exist", Value::Null, None).is_none());
    }

    // --- tool-level behavior ----------------------------------------------

    #[test]
    fn reading_before_connect_is_a_tool_error() {
        let mut server = Server::new();
        let params = json!({"name": "read_live_data", "arguments": {}});
        let res = call_tool(&mut server, params).expect("tools/call replies");
        assert_eq!(res["isError"], json!(true));
        assert!(res["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Not connected"));
    }

    #[test]
    fn diagnose_bundles_data_with_the_required_sections() {
        let mut server = Server::new();
        server.sim = Some(Simulator::new(Scenario::VacuumLeak));
        let params = json!({"name": "diagnose", "arguments": {}});
        let res = call_tool(&mut server, params).expect("tools/call replies");
        assert_eq!(res["isError"], json!(false));
        let report: Value =
            serde_json::from_str(res["content"][0]["text"].as_str().unwrap()).unwrap();
        // Bundles both data sources.
        assert!(report["trouble_codes"]["current"].is_array());
        assert!(report["live_data"]["engine_rpm"].is_object());
        // Includes a readable chart with the expected columns and rows.
        assert_eq!(
            report["chart"]["columns"],
            json!(["Parameter", "Value", "Status"])
        );
        let rows = report["chart"]["rows"].as_array().expect("chart rows");
        assert!(rows.iter().any(|r| r["parameter"] == json!("Engine RPM")));
        // Vacuum leak drives fuel trims lean, so at least one row is flagged.
        assert!(
            rows.iter().any(|r| r["status"].as_str().is_some_and(|s| s.contains("lean"))),
            "expected a lean fuel-trim status flag in the chart"
        );
        // Carries the required section order.
        let headings: Vec<&str> = report["presentation"]["sections"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|s| s["heading"].as_str())
            .collect();
        assert_eq!(
            headings,
            ["Diagnostics", "Common Problems", "NHTSA", "Summary", "Checklist"]
        );
    }

    #[test]
    fn reading_classification_flags_are_universal() {
        assert_eq!(classify_reading("short_term_fuel_trim_bank1", 3.0), "ok");
        assert!(classify_reading("long_term_fuel_trim_bank1", 22.0).contains("lean"));
        assert!(classify_reading("short_term_fuel_trim_bank1", -30.0).contains("very rich"));
        assert!(classify_reading("control_module_voltage", 12.1).contains("low"));
        assert!(classify_reading("coolant_temp", 119.0).contains("overheating"));
        // No universal judgment for these -> blank.
        assert_eq!(classify_reading("engine_rpm", 3000.0), "");
        assert_eq!(classify_reading("vehicle_speed", 60.0), "");
    }

    #[test]
    fn diagnose_before_connect_is_a_tool_error() {
        let mut server = Server::new();
        let params = json!({"name": "diagnose", "arguments": {}});
        let res = call_tool(&mut server, params).expect("tools/call replies");
        assert_eq!(res["isError"], json!(true));
        assert!(res["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Not connected"));
    }

    #[test]
    fn known_issues_without_vehicle_or_connection_is_a_tool_error() {
        let mut server = Server::new();
        // No vin, no make/model/year, not connected -> actionable error, no network.
        let params = json!({"name": "known_issues", "arguments": {}});
        let res = call_tool(&mut server, params).expect("tools/call replies");
        assert_eq!(res["isError"], json!(true));
        assert!(res["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Provide `vin`"));
    }

    #[test]
    fn decode_can_decodes_a_frame_from_inline_dbc() {
        let mut server = Server::new();
        let dbc = "BO_ 200 ENGINE: 8 ECU\n SG_ RPM : 7|16@0+ (0.25,0) [0|16383] \"rpm\" X\n";
        let params = json!({"name": "decode_can", "arguments": {
            "dbc": dbc, "can_id": "0xC8", "data": "0F A0 00 00 00 00 00 00"
        }});
        let res = call_tool(&mut server, params).expect("tools/call replies");
        assert_eq!(res["isError"], json!(false));
        let out: Value =
            serde_json::from_str(res["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(out["signals"][0]["name"], json!("RPM"));
        assert_eq!(out["signals"][0]["value"], json!(1000.0));
        assert_eq!(out["signals"][0]["unit"], json!("rpm"));
    }

    #[test]
    fn decode_can_without_a_dbc_is_a_tool_error() {
        let mut server = Server::new();
        let params = json!({"name": "decode_can", "arguments": {"can_id": 200, "data": "00"}});
        let res = call_tool(&mut server, params).expect("tools/call replies");
        assert_eq!(res["isError"], json!(true));
        assert!(res["content"][0]["text"].as_str().unwrap().contains("Provide `dbc`"));
    }

    #[test]
    fn dbc_signals_lists_messages() {
        let mut server = Server::new();
        let dbc = "BO_ 100 SPEED: 8 ECU\n SG_ VEHICLE_SPEED : 0|8@1+ (1,0) [0|255] \"km/h\" X\n";
        let params = json!({"name": "dbc_signals", "arguments": {"dbc": dbc}});
        let res = call_tool(&mut server, params).expect("tools/call replies");
        assert_eq!(res["isError"], json!(false));
        let out: Value =
            serde_json::from_str(res["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(out["message_count"], json!(1));
        assert_eq!(out["messages"][0]["name"], json!("SPEED"));
        assert_eq!(out["messages"][0]["signals"][0]["name"], json!("VEHICLE_SPEED"));
    }

    #[test]
    fn calling_unknown_tool_is_a_tool_error() {
        let mut server = Server::new();
        let params = json!({"name": "frobnicate", "arguments": {}});
        let res = call_tool(&mut server, params).unwrap();
        assert_eq!(res["isError"], json!(true));
        assert!(res["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("unknown tool"));
    }
}
