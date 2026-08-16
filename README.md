# Mac RDP Server (Rust on macOS / Apple Silicon)

> 🌐 [English Version (README.md)](README.md) | [Bản tiếng Việt (README_VI.md)](README_VI.md)

A high-performance **RDP (Remote Desktop Protocol) Server** written in **Rust** (built upon `ironrdp-server`), running natively on macOS (optimized for Apple Silicon M1/M2/M3/M4 and Intel x86_64). It listens on port `3389` across all interfaces (`0.0.0.0`), streaming live display frames and injecting real keyboard/mouse events with an **Adaptive Motion Rate Controller** and **Quantized RDP6 Planar Encoding**.

---

## 🚀 Key Features

- 📡 **Standard RDP Port:** Listens on `0.0.0.0:3389` (100% compatible with official Microsoft Remote Desktop clients on Windows, macOS, iOS, Android, and Linux FreeRDP).
- 🖥️ **Ultra-Fast Native CoreGraphics Capture:** Continuous 60 Hz frame grabbing with negligible CPU overhead ($< 2\text{ms}$ per capture on Apple Silicon).
- 🧩 **Grid Tiling & FastPath Batching (320x24):** Divides the screen into small tiles, transmitting only modified regions ($< 1\text{ KB}$ per tile). Multiple tiles are batched into single FastPath PDUs to eliminate slow top-to-bottom scanline rendering.
- 🎨 **6-bit Color Quantization:** Filters high-frequency color noise, boosting RDP6 Planar RLE compression efficiency by over **80%** while preserving sharp text and UI contrast.
- ⚡ **Adaptive Motion Rate Controller:**
  - **Micro motion ($\le 5\%$ tiles - Mouse cursor, typing):** $0\text{ms}$ delay $\rightarrow$ **60 FPS** instant response for a 100% native mouse feel.
  - **Minor change ($\le 10\%$):** $33\text{ms}$ delay $\rightarrow$ **30 FPS**.
  - **Medium change ($\le 20\%$):** $50\text{ms}$ delay $\rightarrow$ **20 FPS** (web scrolling, window moving).
  - **Substantial change ($\le 30\%$):** $100\text{ms}$ delay $\rightarrow$ **10 FPS**.
  - **Large change ($\le 50\%$):** $300\text{ms}$ delay $\rightarrow$ **2 FPS**.
  - **Massive change ($> 70\%$):** $500\text{ms}$ delay $\rightarrow$ **1 FPS** (prevents network bottleneck during full-screen zooms/workspace switches).
- 🖱️ **Live Window Dragging:** Tracks mouse button states atomically and emits `kCGEventLeftMouseDragged` (code 6) during drags, ensuring fluid real-time window movement.
- 👻 **Zero Ghosting:** Synchronizes reference framebuffers strictly with transmitted diffs to ensure no ghost shadows or leftover outlines remain after window minimization or maximization.
- 🔒 **TLS Security & Auto-Cert:** Automatically generates self-signed TLS certificates under the `certs/` directory.

---

## 🛠️ Getting Started

### 1. Prerequisites:
- **macOS:** macOS 12 Monterey or newer (Apple Silicon & Intel x86_64).
- **Rust Toolchain:** Latest stable `rustup` and `cargo`.

### 2. macOS Permissions (Required on first launch):
1. **Screen Recording:** Go to `System Settings` → `Privacy & Security` → `Screen Recording` → Enable permission for your Terminal or App.
2. **Accessibility:** Go to `System Settings` → `Privacy & Security` → `Accessibility` → Enable permission for keyboard & mouse input injection.

### 3. Build Release Binary:
```bash
./build.sh
```
*(Runs `cargo fmt --all` then builds `./target/release/mac-rdp-server`).*

### 4. Launching & Managing the Server:

#### Start with CLI Flags (Recommended):
```bash
# Start in background (Daemon Mode):
./target/release/mac-rdp-server -u dev -p 12345678 -d

# Start in background without logs (--no-log):
./target/release/mac-rdp-server -u dev -p 12345678 -d --no-log

# Check status:
./target/release/mac-rdp-server --status

# Stop background server:
./target/release/mac-rdp-server --quit

# Run in foreground (interactive):
./target/release/mac-rdp-server -u dev -p 12345678
```

---

## 💡 CLI Options & Configuration

You can configure the server using either **trailing CLI options** (recommended) or **environment variables**:

| CLI Option | Environment Variable | Supported Values | Default | Description |
| :--- | :--- | :--- | :--- | :--- |
| `-u`, `--user` | `RDP_USER` | Any string | `dev` | Authentication username |
| `-p`, `--password` | `RDP_PASSWORD` | Any string | `12345678` | Authentication password |
| `-P`, `--port` | `RDP_PORT` | `1..65535` | `3389` | Listening TCP port |
| `-H`, `--host` | `RDP_HOST` | IPv4 address | `0.0.0.0` | Listening host interface |
| `-c`, `--color` | `RDP_COLOR` | `4bit`, `5bit`, `6bit`, `8bit` | `6bit` | Color depth (4bit: max throughput, 6bit: crisp UI, 8bit: lossless) |
| `-t`, `--tile` | `RDP_TILE` | `320x24`, `320x32`, `240x24` | `320x24` | Tile grid transmission dimensions |
| `-f`, `--fps` | `RDP_FPS` | `30`, `60`, `120` | `60` | Maximum capture and mouse sampling FPS |
| `-r`, `--res` | `RDP_RES` | `native`, `1080p`, `720p` | `native` | Output canvas resolution (`720p` for ultra-low bandwidth) |
| `-m`, `--mode` | `RDP_MODE` | `speed`, `balanced`, `quality` | `speed` | Performance profile preset |
| `-d`, `--daemon` | - | Flag | - | Run server in background |
| `-q`, `--quit` | - | Flag | - | Stop running background server |
| `-s`, `--status` | - | Flag | - | Check server status |
| `-nl`, `--no-log` | `RDP_NO_LOG` | Flag / `1` | - | Disable stdout logging and log files |
| `-h`, `--help` | - | Flag | - | Display help menu |

#### Custom Examples:
```bash
# Maximum speed mode (4-bit color, 320x32 tiles, 60 FPS in background):
./target/release/mac-rdp-server -u dev -p 12345678 -c 4bit -t 320x32 -f 60 -d

# Lossless 8-bit True Color mode:
./target/release/mac-rdp-server -u dev -p 12345678 -c 8bit -d
```

---

## 💻 Connecting from Client

### 1. From Windows (mstsc):
1. Press `Win + R`, type `mstsc`, and press `Enter`.
2. In **Computer**, enter: `<MAC_IP_ADDRESS>:3389` (e.g. `192.168.1.50:3389`).
3. Enter your configured **Username** & **Password**.
4. Accept the self-signed certificate warning.

### 2. From macOS / iOS / iPadOS:
- Install **Windows App** (formerly Microsoft Remote Desktop) from the App Store.
- Add a PC with `<MAC_IP_ADDRESS>:3389`.

### 3. From Linux (FreeRDP):
```bash
xfreerdp /v:<MAC_IP_ADDRESS>:3389 /u:dev /p:12345678 /cert:ignore
```

---

## 📄 License
Licensed under Apache-2.0 / MIT.
