# OBDium MCP Server

`obd-mcp` is a [Model Context Protocol](https://modelcontextprotocol.io) server
that exposes OBDium's ELM327 diagnostics to an MCP client such as **Claude
Code**. It lets an LLM read your vehicle's live sensor data, trouble codes and
VIN over your OBD-II adapter so you can troubleshoot your car conversationally.

It reuses the `obdium` library directly — the same OBD-II protocol handling, PID
decoding, DTC descriptions and VIN reading used by the desktop app.

## Building

The server lives in its own workspace crate (`mcp/`) that depends only on the
`obdium` library, so building or testing it never compiles the Tauri desktop
app.

```bash
cargo build --release -p obd-mcp
```

This produces `target/release/obd-mcp` at the workspace root.

The binary locates OBDium's `data/` directory automatically (it looks in the
current directory, then walks up from the executable, checking both there and a
sibling `backend/` folder), so it can be launched from anywhere.

## Connecting your Bluetooth OBD-II adapter

A Bluetooth ELM327 adapter is not accessed directly. Once you **pair it at the
operating-system level**, the OS exposes it as a *serial port*, which is what
this server (and OBDium) talk to:

- **macOS:** appears as `/dev/tty.*` / `/dev/cu.*`. Note that classic-Bluetooth
  serial (SPP) is required — many cheap BLE-only ELM327 clones do **not** create
  a serial port on modern macOS and will not be usable this way.
- **Linux:** bind it with `rfcomm` → `/dev/rfcomm0`.
- **Windows:** it shows up as an outgoing `COM` port.

Use the `list_serial_ports` tool to see what's available. With the engine's
ignition on, call `connect` with the port name.

## Using it with Claude Code

A project-scoped config is already checked in at `.mcp.json`. When you open this
project in Claude Code you'll be prompted to approve the `obdium` server; once
approved the tools become available.

To register it globally instead:

```bash
claude mcp add obdium -- /absolute/path/to/target/release/obd-mcp
```

## Tools

| Tool | Description |
| --- | --- |
| `list_serial_ports` | List serial ports with an ELM327 adapter (incl. paired Bluetooth). |
| `connect` | Connect to a vehicle. `simulate=true` runs the coherent simulator (optional `scenario`); `demo=true` replays bundled sample data; otherwise connects to real hardware on `port`. Args: `port`, `baud_rate` (default 38400), `protocol` (0–9, 0=auto), `demo`, `simulate`, `scenario`. |
| `disconnect` | Disconnect from the adapter. |
| `status` | Connection status, port, baud rate and active OBD-II protocol. |
| `read_trouble_codes` | Stored, permanent and freeze-frame DTCs with plain-language descriptions, plus check-engine (MIL) state. |
| `clear_trouble_codes` | Clear stored codes / turn off the check-engine light (service 04). **Destructive** — only on explicit request. |
| `read_live_data` | Snapshot of live sensors (RPM, speed, coolant, load, throttle, fuel trims, MAF, intake, module voltage, …). Unsupported sensors report "no data". |
| `read_vin` | Read the vehicle's VIN from the ECU. |

## Trying it without a car

There are two hardware-free modes, for two different jobs.

### Simulator mode (recommended)

`simulate=true` runs a **coherent vehicle simulator**: a synthetic drive cycle
where the engine warms up from cold, idles, accelerates, cruises and
decelerates on a loop, and every sensor is derived from that shared state — so
RPM tracks speed and gear, MAF tracks RPM and load, MAP tracks throttle, and
the coolant climbs smoothly to operating temperature. Poll `read_live_data`
repeatedly and you get a believable, evolving picture rather than random noise.

It can also inject a **fault scenario** via the `scenario` argument, which skews
the relevant live values *and* reports a matching trouble code with the
check-engine light on — ideal for practising a troubleshooting flow:

| `scenario` | What it simulates |
| --- | --- |
| `healthy` (default) | Everything nominal, no codes, MIL off. |
| `vacuum_leak` | Large positive fuel trims (lean), worst at idle. Sets `P0171`. |
| `misfire` | RPM jitter and erratic short-term fuel trim. Sets `P0300`/`P0301`. |
| `overheat` | Coolant climbs past the normal range into the red. Sets `P0217`. |

`clear_trouble_codes` clears the fault and turns the light off, just like a real
ECU.

```bash
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"connect","arguments":{"simulate":true,"scenario":"vacuum_leak"}}}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"read_live_data","arguments":{}}}' \
  '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"read_trouble_codes","arguments":{}}}' \
  | target/release/obd-mcp
```

Or in Claude Code, just ask it to *"connect in simulate mode with a vacuum leak
and help me diagnose it."*

### Demo mode

`demo=true` replays OBDium's **recorded sample responses**. Each read returns a
random recorded value for that PID, so the numbers are real but not physically
coherent between sensors or over time. It's best for exercising the raw
protocol/decoding path rather than for a realistic drive:

```bash
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"connect","arguments":{"demo":true}}}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"read_live_data","arguments":{}}}' \
  | target/release/obd-mcp
```

## Testing

```bash
cargo test -p obd-mcp
```

Because `obd-mcp` is its own crate, this builds only the `obdium` library and
the server — never the Tauri app. Two layers run:

- **Unit tests** (`mcp/src/main.rs`, `mcp/src/simulator.rs`) cover reading
  serialization, JSON-RPC dispatch (initialize, `tools/list`, notifications,
  unknown methods), tool-level error handling, and the simulator model itself
  (readings stay in physical ranges across a drive cycle, the engine warms up,
  idle vs. cruise is coherent, and each fault scenario skews the right values
  and sets the right code). No hardware or data files needed.
- **Integration tests** (`mcp/tests/mcp_server.rs`) spawn the real binary and
  drive it over stdio in demo and simulate modes. They assert that *every*
  emitted line is valid JSON — the regression guard ensuring the library's
  stdout debug output never leaks into the protocol stream — and check the
  `connect` → `read_live_data` / `read_trouble_codes` flows, including that a
  simulated `vacuum_leak` sets `P0171` and that clearing codes turns the light
  off.

## Notes

- Transport is line-delimited JSON-RPC 2.0 over stdio (MCP stdio transport).
- The `obdium` library logs debug output to stdout; the server redirects that to
  stderr on startup so it never corrupts the protocol stream.
- Full VIN attribute decoding (make/model/year) depends on the large VPIC
  database used by the desktop app and is not exposed here — `read_vin` returns
  the raw VIN read from the vehicle.
