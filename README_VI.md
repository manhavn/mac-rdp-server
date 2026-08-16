# Mac RDP Server (Rust on macOS / Apple Silicon)

> 🌐 [English Version (README.md)](README.md) | [Bản tiếng Việt (README_VI.md)](README_VI.md)

Dự án **Mac RDP Server** hiệu năng cao được viết bằng ngôn ngữ **Rust** (dựa trên nền tảng `ironrdp-server`), chạy native trực tiếp trên macOS (tối ưu hóa cho Apple Silicon M1/M2/M3/M4 và Intel), lắng nghe cổng chuẩn `3389` trên tất cả các địa chỉ mạng (`0.0.0.0`), hỗ trợ truyền hình ảnh thời gian thực và điều khiển chuột/bàn phím với cơ chế điều tiết chuyển động động (**Adaptive Motion Rate Controller**) và nén lượng tử màu sắc (**Color Quantization**).

---

## 🚀 Các tính năng nổi bật

- 📡 **Cổng kết nối RDP chuẩn:** Lắng nghe trên `0.0.0.0:3389` (tương thích $100\%$ với Microsoft Remote Desktop trên Windows, macOS, iOS, Android, Linux).
- 🖥️ **Chụp màn hình Native siêu tốc (CoreGraphics):** Tốc độ quét màn hình $60\text{Hz}$ liên tục với chi phí CPU cực thấp ($< 2\text{ms}$ mỗi frame trên chip M-series).
- 🧩 **Lưới ô gạch truyền tải động (Grid Tiling 320x24):** Chỉ truyền chính xác các ô gạch thay đổi ($< 1\text{ KB}$), gom nhiều ô gạch vào 1 gói tin FastPath để triệt tiêu hoàn toàn hiệu ứng quét từng lớp từ trên xuống.
- 🎨 **Nén lượng tử màu sắc (Color Quantization - Mặc định 6-bit):** Lọc bỏ nhiễu màu dư thừa, giúp thuật toán RDP6 Planar RLE nén chặt hơn $80\%$, giữ nguyên $100\%$ độ sắc nét của chữ viết và giao diện.
- ⚡ **Bộ điều tiết chuyển động động (Adaptive Motion Controller):**
  - **Vi mô ($\le 5\%$ ô gạch - Di chuột, gõ phím):** $0\text{ms}$ delay $\rightarrow$ **60 FPS** tức thì, cảm giác chuột native $100\%$.
  - **Nhỏ ($\le 10\%$):** $33\text{ms}$ delay $\rightarrow$ **30 FPS**.
  - **Vừa ($\le 20\%$):** $50\text{ms}$ delay $\rightarrow$ **20 FPS** (cuộn trang web, kéo cửa sổ).
  - **Lớn ($\le 30\%$):** $100\text{ms}$ delay $\rightarrow$ **10 FPS**.
  - **Rất lớn ($\le 50\%$):** $300\text{ms}$ delay $\rightarrow$ **2 FPS**.
  - **Toàn màn hình ($> 70\%$):** $500\text{ms}$ delay $\rightarrow$ **1 FPS** (triệt tiêu nghẽn mạng lúc chuyển desktop/phóng to).
- 🖱️ **Kéo thả cửa sổ Live (Live Window Dragging):** Tự động phát hiện trạng thái giữ chuột trái để bắn sự kiện `kCGEventLeftMouseDragged` (code 6), cho phép di chuyển cửa sổ mượt mà thời gian thực.
- 👻 **Không bóng mờ (Zero Ghosting):** Đồng bộ tuyệt đối bộ nhớ đệm tham chiếu với các ô gạch thực tế gửi tới Client, loại bỏ hoàn toàn bóng mờ khi thu nhỏ/phóng to cửa sổ.
- 🔒 **Bảo mật & Tự động sinh chứng chỉ TLS:** Tự động tạo chứng chỉ TLS self-signed trong thư mục `certs/`.

---

## 🛠️ Hướng dẫn cài đặt & Khởi chạy

### 1. Yêu cầu hệ thống:
- **Hệ điều hành:** macOS 12 Monterey trở lên (hỗ trợ đầy đủ Apple Silicon M-series & Intel x86_64).
- **Rust Toolchain:** `rustup` và `cargo` phiên bản mới nhất.

### 2. Cấp quyền truy cập macOS (Bắt buộc trong lần chạy đầu):
1. **Screen Recording (Ghi màn hình):** `System Settings` → `Privacy & Security` → `Screen Recording` → Bật quyền cho Terminal / iTerm / Ứng dụng của bạn.
2. **Accessibility (Trợ năng):** `System Settings` → `Privacy & Security` → `Accessibility` → Bật quyền để Server có thể gửi sự kiện chuột và phím.

### 3. Build Bản Release:
```bash
./build.sh
```
*(Tự động chạy `cargo fmt --all` định dạng mã nguồn và biên dịch tối ưu vào `./target/release/mac-rdp-server`).*

### 4. Khởi chạy & Quản lý Server:

#### Khởi chạy bằng Tham số Dòng lệnh CLI (Khuyên dùng):
```bash
# Chạy ngầm trong nền (Daemon Mode):
./target/release/mac-rdp-server -u dev -p 12345678 -d

# Chạy ngầm hoàn toàn không ghi log (--no-log):
./target/release/mac-rdp-server -u dev -p 12345678 -d --no-log

# Kiểm tra trạng thái hoạt động:
./target/release/mac-rdp-server --status

# Dừng Server đang chạy ngầm:
./target/release/mac-rdp-server --quit

# Chạy trực tiếp (Interactive Foreground):
./target/release/mac-rdp-server -u dev -p 12345678
```

---

## 💡 Bảng Tham số Dòng lệnh & Biến môi trường

Bạn có thể truyền cấu hình trực tiếp qua **tham số dòng lệnh CLI** (khuyên dùng) hoặc qua **biến môi trường**:

| Tham số CLI | Biến môi trường | Giá trị hỗ trợ | Mặc định | Mô tả |
| :--- | :--- | :--- | :--- | :--- |
| `-u`, `--user` | `RDP_USER` | Chuỗi tùy ý | `dev` | Tên đăng nhập RDP |
| `-p`, `--password` | `RDP_PASSWORD` | Chuỗi tùy ý | `12345678` | Mật khẩu đăng nhập RDP |
| `-P`, `--port` | `RDP_PORT` | `1..65535` | `3389` | Cổng TCP lắng nghe |
| `-H`, `--host` | `RDP_HOST` | Địa chỉ IP | `0.0.0.0` | Địa chỉ mạng lắng nghe |
| `-c`, `--color` | `RDP_COLOR` | `4bit`, `5bit`, `6bit`, `8bit` | `6bit` | Mức nén & độ sâu màu (4bit: siêu nhẹ, 6bit: sắc nét, 8bit: lossless) |
| `-t`, `--tile` | `RDP_TILE` | `320x24`, `320x32`, `240x24` | `320x24` | Kích thước ô gạch truyền tải |
| `-f`, `--fps` | `RDP_FPS` | `30`, `60`, `120` | `60` | Tần số quét màn hình tối đa cho chuột & bàn phím |
| `-r`, `--res` | `RDP_RES` | `native`, `1080p`, `720p` | `native` | Độ phân giải hiển thị RDP (`720p` giúp siêu nhẹ) |
| `-m`, `--mode` | `RDP_MODE` | `speed`, `balanced`, `quality` | `speed` | Profile cấu hình tổng thể |
| `-d`, `--daemon` | - | Cờ bật | - | Chạy server ngầm trong nền |
| `-q`, `--quit` | - | Cờ bật | - | Dừng server đang chạy ngầm |
| `-s`, `--status` | - | Cờ bật | - | Kiểm tra trạng thái hoạt động của server |
| `-nl`, `--no-log` | `RDP_NO_LOG` | Cờ bật / `1` | - | Tắt toàn bộ log xuất ra màn hình và file log |
| `-h`, `--help` | - | Cờ bật | - | Xem menu hướng dẫn trợ giúp |

#### Ví dụ tùy biến:
```bash
# Chế độ siêu tốc tối đa (4-bit màu, ô gạch 320x32, 60 FPS chạy ngầm):
./target/release/mac-rdp-server -u dev -p 12345678 -c 4bit -t 320x32 -f 60 -d

# Chế độ True Color Lossless (8-bit màu):
./target/release/mac-rdp-server -u dev -p 12345678 -c 8bit -d
```

---

## 💻 Kết nối từ Client

### 1. Từ Windows (Remote Desktop Connection / mstsc):
1. Nhấn `Win + R` → gõ `mstsc` → nhấn `Enter`.
2. Nhập địa chỉ: `<IP_MÁY_MAC>:3389` (ví dụ: `192.168.1.50:3389`).
3. Nhập User & Password đã cấu hình.
4. Khi có cảnh báo Certificate, chọn **Yes / Connect anyway**.

### 2. Từ macOS / iOS / iPadOS:
- Tải ứng dụng **Windows App** (trước đây là Microsoft Remote Desktop) từ App Store.
- Thêm PC: `<IP_MÁY_MAC>:3389`.

### 3. Từ Linux (FreeRDP):
```bash
xfreerdp /v:<IP_MÁY_MAC>:3389 /u:dev /p:12345678 /cert:ignore
```
