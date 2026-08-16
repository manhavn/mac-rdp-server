#!/usr/bin/env bash
set -e

# Chuyển về thư mục chứa script
cd "$(dirname "$0")"
# Xác định đường dẫn binary (ưu tiên /usr/local/bin/mac-rdp-server)
if [ -f "/usr/local/bin/mac-rdp-server" ]; then
    BINARY_PATH="/usr/local/bin/mac-rdp-server"
elif command -v mac-rdp-server >/dev/null 2>&1; then
    BINARY_PATH="$(command -v mac-rdp-server)"
else
    BINARY_PATH="/usr/local/bin/mac-rdp-server"
fi

echo "============================================================"
echo "🚀 Thiết lập Mac RDP Server khởi động cùng macOS (LaunchAgent)"
echo "📍 Binary: $BINARY_PATH"
echo "============================================================"

USER_NAME="${RDP_USER:-dev}"
PASSWORD="${RDP_PASSWORD:-12345678}"
PORT="${RDP_PORT:-3389}"
COLOR="${RDP_COLOR:-6bit}"
FPS="${RDP_FPS:-60}"
PLIST_NAME="com.dev.mac-rdp-server"
LAUNCH_AGENTS_DIR="$HOME/Library/LaunchAgents"
PLIST_FILE="$LAUNCH_AGENTS_DIR/$PLIST_NAME.plist"

mkdir -p "$LAUNCH_AGENTS_DIR"

# Tạo file cấu hình LaunchAgent chạy trong Session GUI của User
cat <<EOF > "$PLIST_FILE"
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>$PLIST_NAME</string>
    <key>ProgramArguments</key>
    <array>
        <string>$BINARY_PATH</string>
        <string>-u</string>
        <string>$USER_NAME</string>
        <string>-p</string>
        <string>$PASSWORD</string>
        <string>-P</string>
        <string>$PORT</string>
        <string>-c</string>
        <string>$COLOR</string>
        <string>-f</string>
        <string>$FPS</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>WorkingDirectory</key>
    <string>$HOME</string>
    <key>StandardOutPath</key>
    <string>/tmp/mac-rdp-server.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/mac-rdp-server.log</string>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
EOF

echo "📄 Đã tạo file LaunchAgent: $PLIST_FILE"

# 1. Dọn dẹp tiến trình cũ và giải phóng cổng 3389
OLD_PIDS=$(lsof -ti:3389 2>/dev/null || true)
if [ -n "$OLD_PIDS" ]; then
    kill -9 $OLD_PIDS 2>/dev/null || true
fi

# 2. Gỡ bỏ triệt để đăng ký cũ khỏi launchd
launchctl bootout "gui/$(id -u)/$PLIST_NAME" 2>/dev/null || true
launchctl unload -w "$PLIST_FILE" 2>/dev/null || true

# 3. Kích hoạt và nạp vào phiên làm việc GUI hiện tại
launchctl enable "gui/$(id -u)/$PLIST_NAME" 2>/dev/null || true
if ! launchctl bootstrap "gui/$(id -u)" "$PLIST_FILE" 2>/dev/null; then
    launchctl load -w "$PLIST_FILE" 2>/dev/null || true
fi
launchctl kickstart -k "gui/$(id -u)/$PLIST_NAME" 2>/dev/null || true

sleep 1

# 4. Kiểm tra trạng thái hoạt động thực tế
if launchctl list | grep -q "$PLIST_NAME" || lsof -i:3389 >/dev/null 2>&1; then
    RUNNING_PID=$(lsof -ti:3389 2>/dev/null || launchctl list | grep "$PLIST_NAME" | awk '{print $1}')
    echo ""
    echo "============================================================"
    echo "✅ Cài đặt và khởi động thành công!"
    echo "📡 Mac RDP Server đang chạy ngầm trong GUI session (PID: $RUNNING_PID)"
    echo "🔄 Server sẽ TỰ ĐỘNG KHỞI ĐỘNG mỗi khi bạn đăng nhập Mac."
    echo "🔑 Tài khoản: $USER_NAME / $PASSWORD"
    echo "📄 File log: /tmp/mac-rdp-server.log"
    echo "🛑 Để gỡ bỏ tự khởi động: ./uninstall-autostart.sh"
    echo "============================================================"
else
    echo ""
    echo "============================================================"
    echo "⚠️ LaunchAgent đã được tạo nhưng chưa chạy được. Chi tiết lỗi:"
    cat /tmp/mac-rdp-server.log 2>/dev/null || true
    echo "============================================================"
fi
