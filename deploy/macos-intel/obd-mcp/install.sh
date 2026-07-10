#!/usr/bin/env bash
# Installs the obdium MCP server into Claude Desktop / Claude Code on this Mac.
# Safe to re-run. Requires: macOS on Intel (x86_64).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="$HERE/obd-mcp"

# --- sanity checks -----------------------------------------------------------
if [[ ! -x "$BIN" ]]; then
  echo "error: $BIN not found or not executable" >&2
  exit 1
fi

arch="$(uname -m)"
if [[ "$arch" != "x86_64" ]]; then
  echo "warning: this binary is built for Intel (x86_64) but this machine is $arch." >&2
  echo "         On Apple Silicon it runs under Rosetta; for a native build use the" >&2
  echo "         macos-arm64 package instead." >&2
fi

# macOS Gatekeeper quarantines files copied from another machine. Clear it.
xattr -dr com.apple.quarantine "$HERE" 2>/dev/null || true

echo "obdium MCP server is at:"
echo "  $BIN"
echo
echo "Add this to your MCP client config (use this absolute path):"
echo
cat <<JSON
{
  "mcpServers": {
    "obdium": {
      "command": "$BIN",
      "args": []
    }
  }
}
JSON
echo
echo "Claude Desktop config lives at:"
echo "  ~/Library/Application Support/Claude/claude_desktop_config.json"
echo
echo "For Claude Code, run:  claude mcp add obdium \"$BIN\""
echo
echo "Done. Restart your Claude client to pick up the server."
