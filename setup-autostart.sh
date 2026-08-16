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

# Unload service cũ nếu đang chạy
launchctl bootout "gui/$(id -u)/$PLIST_NAME" 2>/dev/null || launchctl unload "$PLIST_FILE" 2>/dev/null || true

# Nạp service vào Session GUI hiện tại
launchctl bootstrap "gui/$(id -u)" "$PLIST_FILE" 2>/dev/null || launchctl load "$PLIST_FILE"

echo ""
echo "============================================================"
echo "✅ Cài đặt thành công!"
echo "📡 Mac RDP Server đang chạy ngầm trong GUI session hiện tại."
echo "🔄 Server sẽ TỰ ĐỘNG KHỞI ĐỘNG mỗi khi bạn đăng nhập Mac."
echo "🔑 Tài khoản: $USER_NAME / $PASSWORD"
echo "📄 File log: /tmp/mac-rdp-server.log"
echo "🛑 Để gỡ bỏ tự khởi động: ./uninstall-autostart.sh"
echo "============================================================"
