#!/usr/bin/env bash
set -e

PLIST_NAME="com.dev.mac-rdp-server"
PLIST_FILE="$HOME/Library/LaunchAgents/$PLIST_NAME.plist"

echo "============================================================"
echo "🛑 Gỡ bỏ Mac RDP Server khỏi LaunchAgent tự khởi động"
echo "============================================================"

if [ -f "$PLIST_FILE" ]; then
    launchctl bootout "gui/$(id -u)/$PLIST_NAME" 2>/dev/null || launchctl unload "$PLIST_FILE" 2>/dev/null || true
    rm -f "$PLIST_FILE"
    echo "✅ Đã gỡ bỏ: $PLIST_FILE"
    echo "✅ Mac RDP Server đã ngừng tự khởi động."
else
    echo "ℹ️ Không tìm thấy cấu hình LaunchAgent tự khởi động."
fi

# Dừng server nếu đang còn chạy
/usr/local/bin/mac-rdp-server --quit 2>/dev/null || mac-rdp-server --quit 2>/dev/null || "$(dirname "$0")/target/release/mac-rdp-server" --quit 2>/dev/null || true
echo "============================================================"
