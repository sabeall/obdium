# obdium — OBD-II MCP server (deployable package)

A self-contained build of the `obdium` MCP server for **Apple Silicon macOS (arm64)**.
It exposes vehicle diagnostics (live data, trouble codes, VIN decode, NHTSA recalls
& complaints, CAN/DBC decoding, and a structured `diagnose` tool) to Claude over MCP.

## Contents

```
obd-mcp/
├── obd-mcp                       the MCP server (arm64 binary)
├── data/
│   ├── code-descriptions.sqlite  DTC descriptions (real-dongle use)
│   └── model-pids.sqlite         extended (mode 22) PID definitions
├── mcp-config.json               config snippet to paste into your client
├── install.sh                    prints the exact config + clears quarantine
└── README.md                     this file
```

The binary finds `data/` automatically as long as it stays **next to** the
`obd-mcp` executable. Don't separate them.

## Install

1. Copy this whole `obd-mcp/` folder anywhere on the target Mac (e.g. `~/bin/obd-mcp`).
2. Run the helper (clears macOS quarantine and prints your config):
   ```bash
   ./install.sh
   ```
3. Register it with your Claude client:
   - **Claude Code:**
     ```bash
     claude mcp add obdium "/absolute/path/to/obd-mcp/obd-mcp"
     ```
   - **Claude Desktop:** add the block from `install.sh` output to
     `~/Library/Application Support/Claude/claude_desktop_config.json`,
     using the **absolute path** to the binary.
4. Restart the Claude client.

## Requirements & notes

- **Apple Silicon only.** This binary is `aarch64-apple-darwin`. On Intel Macs it
  may run under Rosetta; on Windows/Linux you must rebuild from source.
- **No network DB needed.** Vehicle recalls/complaints come from NHTSA over the
  network at query time — no giant local `vpic.sqlite` required. An internet
  connection is needed for the `known_issues` tool.
- **Simulator/demo mode works offline** and carries its own trouble-code
  descriptions, so it functions even without the `data/` files.
- **Real dongle use:** connect a Bluetooth/USB ELM327 adapter; on macOS it appears
  as `/dev/cu.*`. Use the `connect` tool with that port.

## Quick smoke test

```bash
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"connect","arguments":{"simulate":true,"scenario":"healthy"}}}' \
  '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"diagnose","arguments":{}}}' \
  | ./obd-mcp
```
You should see JSON-RPC responses including a `diagnose` result with a live-data chart.
