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

#### Start in Background (Daemon Mode):
```bash
RDP_USER=dev RDP_PASSWORD=12345678 ./target/release/mac-rdp-server --daemon
```

#### Start in Background without Logs (--no-log):
```bash
RDP_USER=dev RDP_PASSWORD=12345678 ./target/release/mac-rdp-server --daemon --no-log
```

#### Check Server Status:
```bash
./target/release/mac-rdp-server --status
```

#### Stop Background Server:
```bash
./target/release/mac-rdp-server --quit
```

#### Run in Foreground (Interactive Mode):
```bash
RDP_USER=dev RDP_PASSWORD=12345678 cargo run --release
```

The server automatically runs with optimal defaults:
- **Tile Grid:** `320x24`
- **Color Depth:** `6bit` (`0xFC` mask)
- **Capture Rate:** `60 FPS`
- **Adaptive Motion Controller:** Enabled ($60\text{ FPS}$ micro $\rightarrow 1\text{ FPS}$ full-screen)

---

## 💡 Environment Variables

| Variable | Supported Values | Default | Description |
| :--- | :--- | :--- | :--- |
| `RDP_COLOR` / `RDP_BITS` | `4bit`, `5bit`, `6bit`, `8bit` | `6bit` | Compression & color depth (4bit: maximum throughput, 6bit: crisp UI, 8bit: lossless true color) |
| `RDP_TILE` | `320x24`, `320x32`, `240x24` | `320x24` | Transmission tile grid dimensions |
| `RDP_FPS` | `30`, `60`, `120` | `60` | Maximum capture and mouse sampling FPS |
| `RDP_RES` | `native`, `1080p`, `720p` | `native` | Output canvas resolution (`720p` for ultra-low bandwidth) |
| `RDP_MODE` | `speed`, `balanced`, `quality` | `speed` | Overall performance profile preset |
| `RDP_USER` | Any string | `admin` | Authentication username |
| `RDP_PASSWORD` | Any string | `password123` | Authentication password |

#### Custom Examples:
```bash
# Maximum speed mode (4-bit color, 320x32 tiles):
RDP_COLOR=4bit RDP_TILE=320x32 RDP_USER=dev RDP_PASSWORD=12345678 cargo run --release

# Lossless 8-bit True Color mode:
RDP_COLOR=8bit RDP_USER=dev RDP_PASSWORD=12345678 cargo run --release
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
