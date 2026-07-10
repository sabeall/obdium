# deploy/

Deployment artifacts for the **obdium OBD-II MCP server**.

This directory holds ready-to-ship builds of the MCP server plus everything a
target machine needs to run it. It is separate from the source tree so a package
can be copied out on its own. There is one subfolder per platform.

## Contents

```
deploy/
├── README.md                     this file (how to build/ship)
├── macos-arm64/                  Apple Silicon package
│   ├── obd-mcp-arm64-macos.tar.gz   ← move THIS to an Apple Silicon Mac
│   └── obd-mcp/                      unpacked package (binary + data + docs)
└── macos-intel/                  Intel package
    ├── obd-mcp-intel-macos.tar.gz   ← move THIS to an Intel Mac
    └── obd-mcp/                      unpacked package (binary + data + docs)
```

Each `obd-mcp/` package contains:

```
obd-mcp/
├── obd-mcp                       MCP server binary (arch-specific)
├── data/
│   ├── code-descriptions.sqlite  DTC descriptions (real-dongle use)
│   └── model-pids.sqlite         extended (mode 22) PID definitions
├── mcp-config.json               config snippet for your Claude client
├── install.sh                    clears macOS quarantine + prints config
└── README.md                     end-user install/usage instructions
```

## Which package?

| Target Mac | Package | Binary |
|------------|---------|--------|
| Apple Silicon (M1–M4) | `macos-arm64/` | `aarch64-apple-darwin` |
| Intel | `macos-intel/` | `x86_64-apple-darwin` |

The Intel binary also runs on Apple Silicon under Rosetta, but the arm64 package
is the native choice there. For Linux/Windows, build on that OS (see below).

## Deploying

Ship the matching `*.tar.gz` to the target Mac, then follow the package's own
`obd-mcp/README.md`. Short version:

```bash
tar -xzf obd-mcp-<arch>-macos.tar.gz -C ~/bin
cd ~/bin/obd-mcp && ./install.sh
claude mcp add obdium "$HOME/bin/obd-mcp/obd-mcp"
```

## Rebuilding the packages

From the repo root, after a successful build for the target:

| Target | Build command |
|--------|---------------|
| Apple Silicon macOS | `cargo build --release -p obd-mcp --target aarch64-apple-darwin` |
| Intel macOS | `cargo build --release -p obd-mcp --target x86_64-apple-darwin` |
| Linux / Windows | build on that OS: `cargo build --release -p obd-mcp` |

Then re-stage the package (arm64 shown; swap the target/dir for Intel):

```bash
PKG=deploy/macos-arm64/obd-mcp
rm -rf "$PKG" && mkdir -p "$PKG/data"
cp target/aarch64-apple-darwin/release/obd-mcp "$PKG/obd-mcp"
cp backend/data/code-descriptions.sqlite backend/data/model-pids.sqlite "$PKG/data/"
# README.md, install.sh, mcp-config.json are kept in version control
tar -czf deploy/macos-arm64/obd-mcp-arm64-macos.tar.gz -C deploy/macos-arm64 obd-mcp
```

> Note: `obd-mcp` and `data/` must stay side by side — the binary locates its
> databases relative to its own location.
