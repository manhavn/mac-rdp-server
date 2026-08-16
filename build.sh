#!/usr/bin/env bash
set -e

# Chuyển về thư mục gốc của dự án
cd "$(dirname "$0")"

echo "============================================================"
echo "🛠️  Formatting code with cargo fmt..."
echo "============================================================"
cargo fmt --all

echo ""
echo "============================================================"
echo "🚀 Building Mac RDP Server (Release Mode)..."
echo "============================================================"
cargo build --release

echo ""
echo "============================================================"
echo "✅ Build completed successfully!"
echo "📍 Binary: ./target/release/mac-rdp-server"
echo "💡 To run daemon:     ./target/release/mac-rdp-server --daemon"
echo "💡 To stop daemon:    ./target/release/mac-rdp-server --quit"
echo "💡 To check status:   ./target/release/mac-rdp-server --status"
echo "💡 To run foreground: RDP_USER=dev RDP_PASSWORD=12345678 ./target/release/mac-rdp-server"
echo "============================================================"
