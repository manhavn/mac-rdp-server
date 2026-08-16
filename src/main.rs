mod mac_display;
mod mac_input;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use ironrdp_server::{
    Credentials, ExactMatchCredentialValidator, RdpServer, TlsIdentityCtx,
    tokio_rustls::TlsAcceptor,
};
use mac_display::MacDisplay;
use mac_input::{MacInputHandler, check_accessibility_permission};
use tracing::{error, info};

/// Tự động sinh chứng chỉ TLS self-signed nếu chưa có trong thư mục certs/
fn ensure_tls_certificate(cert_path: &Path, key_path: &Path) -> Result<TlsAcceptor> {
    if !cert_path.exists() || !key_path.exists() {
        info!("Generating self-signed TLS certificates for RDP server...");
        let subject_alt_names = vec![
            "localhost".to_string(),
            "0.0.0.0".to_string(),
            "127.0.0.1".to_string(),
        ];
        let cert_params = rcgen::CertificateParams::new(subject_alt_names)?;
        let key_pair = rcgen::KeyPair::generate()?;
        let cert = cert_params.self_signed(&key_pair)?;

        if let Some(parent) = cert_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(cert_path, cert.pem())?;
        std::fs::write(key_path, key_pair.serialize_pem())?;
        info!(
            "Saved TLS cert to {:?} and key to {:?}",
            cert_path, key_path
        );
    }

    let identity = TlsIdentityCtx::init_from_paths(cert_path, key_path)
        .context("Failed to load TLS identity")?;
    identity
        .make_acceptor()
        .context("Failed to create TLS acceptor")
}

/// Tối ưu hóa UI macOS: Vô hiệu hóa hiệu ứng zoom/resize animation để cửa sổ mở và phóng to tức thì
fn optimize_macos_animations() {
    let _ = std::process::Command::new("defaults")
        .args(&[
            "write",
            "-g",
            "NSAutomaticWindowAnimationsEnabled",
            "-bool",
            "false",
        ])
        .status();
    let _ = std::process::Command::new("defaults")
        .args(&["write", "-g", "NSWindowResizeTime", "-float", "0.001"])
        .status();
    let _ = std::process::Command::new("defaults")
        .args(&[
            "write",
            "com.apple.dock",
            "expose-animation-duration",
            "-float",
            "0.001",
        ])
        .status();
    let _ = std::process::Command::new("defaults")
        .args(&[
            "write",
            "com.apple.dock",
            "springboard-show-duration",
            "-float",
            "0.001",
        ])
        .status();
    let _ = std::process::Command::new("defaults")
        .args(&[
            "write",
            "com.apple.dock",
            "springboard-hide-duration",
            "-float",
            "0.001",
        ])
        .status();
    let _ = std::process::Command::new("defaults")
        .args(&["write", "-g", "QLPanelAnimationDuration", "-float", "0.001"])
        .status();
    info!(
        "⚡ [MACOS OPTIMIZATION] UI Animations and window zoom delays disabled for instant response"
    );
}

const PID_FILE: &str = "/tmp/mac-rdp-server.pid";
const LOG_FILE: &str = "/tmp/mac-rdp-server.log";

/// Kiểm tra xem PID có đang thực sự chạy trên hệ thống hay không
fn is_process_running(pid: u32) -> bool {
    let output = std::process::Command::new("kill")
        .args(&["-0", &pid.to_string()])
        .output();
    matches!(output, Ok(out) if out.status.success())
}

/// Dừng Server đang chạy ngầm (--quit)
fn handle_quit() -> Result<()> {
    let mut stopped = false;

    if let Ok(pid_str) = std::fs::read_to_string(PID_FILE) {
        if let Ok(pid) = pid_str.trim().parse::<u32>() {
            if is_process_running(pid) {
                let _ = std::process::Command::new("kill")
                    .args(&["-TERM", &pid.to_string()])
                    .status();

                // Đợi tối đa 2 giây để tiến trình kết thúc êm ái
                for _ in 0..20 {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    if !is_process_running(pid) {
                        break;
                    }
                }

                // Nếu vẫn chưa thoát, buộc dừng bằng SIGKILL
                if is_process_running(pid) {
                    let _ = std::process::Command::new("kill")
                        .args(&["-9", &pid.to_string()])
                        .status();
                }

                println!("✅ Mac RDP Server (PID: {}) has been stopped.", pid);
                stopped = true;
            }
        }
        let _ = std::fs::remove_file(PID_FILE);
    }

    // Kiểm tra thêm cổng 3389 nếu còn tiến trình sót lại
    if let Ok(output) = std::process::Command::new("lsof")
        .args(&["-ti:3389"])
        .output()
    {
        let pids = String::from_utf8_lossy(&output.stdout);
        for p in pids.lines() {
            if let Ok(port_pid) = p.trim().parse::<u32>() {
                let _ = std::process::Command::new("kill")
                    .args(&["-9", &port_pid.to_string()])
                    .status();
                if !stopped {
                    println!(
                        "✅ Mac RDP Server (PID: {}) on port 3389 has been stopped.",
                        port_pid
                    );
                    stopped = true;
                }
            }
        }
    }

    if !stopped {
        println!("ℹ️ Mac RDP Server is not currently running.");
    }

    Ok(())
}

/// Khởi động Server chạy ngầm dưới dạng Daemon (--daemon)
fn handle_daemon(no_log: bool) -> Result<()> {
    if let Ok(pid_str) = std::fs::read_to_string(PID_FILE) {
        if let Ok(pid) = pid_str.trim().parse::<u32>() {
            if is_process_running(pid) {
                println!("⚠️ Mac RDP Server is already running (PID: {}).", pid);
                if !no_log {
                    println!("📄 Logs: {}", LOG_FILE);
                }
                println!("🛑 To stop server, run: ./mac-rdp-server --quit");
                return Ok(());
            }
        }
    }

    let current_exe = std::env::current_exe().context("Failed to get current executable path")?;
    let mut cmd = std::process::Command::new(current_exe);
    cmd.arg("--worker");
    if no_log {
        cmd.arg("--no-log");
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
    } else {
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(LOG_FILE)
            .context("Failed to open log file")?;
        cmd.stdout(std::process::Stdio::from(log_file.try_clone()?));
        cmd.stderr(std::process::Stdio::from(log_file));
    }
    cmd.envs(std::env::vars());

    let child = cmd
        .spawn()
        .context("Failed to spawn background daemon process")?;

    let pid = child.id();
    std::fs::write(PID_FILE, pid.to_string())?;

    println!("============================================================");
    println!("🚀 Mac RDP Server started in background (Daemon Mode)");
    println!("📡 Listening on: 0.0.0.0:3389");
    println!("🆔 PID: {}", pid);
    if no_log {
        println!("📄 Log file: DISABLED (--no-log)");
    } else {
        println!("📄 Log file: {}", LOG_FILE);
    }
    println!("🛑 To stop server, run: ./mac-rdp-server --quit");
    println!("============================================================");

    Ok(())
}

/// Kiểm tra trạng thái Server (--status)
fn handle_status() -> Result<()> {
    if let Ok(pid_str) = std::fs::read_to_string(PID_FILE) {
        if let Ok(pid) = pid_str.trim().parse::<u32>() {
            if is_process_running(pid) {
                println!("🟢 Mac RDP Server is RUNNING (PID: {})", pid);
                println!("📡 Listening on: 0.0.0.0:3389");
                println!("📄 Log file: {}", LOG_FILE);
                println!("🛑 To stop server, run: ./mac-rdp-server --quit");
                return Ok(());
            }
        }
    }

    println!("🔴 Mac RDP Server is STOPPED");
    println!("🚀 To start in background: ./mac-rdp-server --daemon");
    println!("🚀 To start in foreground: ./mac-rdp-server");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let no_log = args.iter().any(|arg| arg == "--no-log" || arg == "-nl")
        || std::env::var("RDP_NO_LOG")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

    // Xử lý tham số dòng lệnh CLI
    if args
        .iter()
        .any(|arg| arg == "--quit" || arg == "-q" || arg == "stop")
    {
        return handle_quit();
    }
    if args
        .iter()
        .any(|arg| arg == "--daemon" || arg == "-d" || arg == "start")
    {
        return handle_daemon(no_log);
    }
    if args
        .iter()
        .any(|arg| arg == "--status" || arg == "-s" || arg == "status")
    {
        return handle_status();
    }

    // Ghi PID vào PID_FILE
    let my_pid = std::process::id();
    let _ = std::fs::write(PID_FILE, my_pid.to_string());

    // Khởi tạo logger (tắt hoàn toàn khi có cờ --no-log)
    if no_log {
        tracing_subscriber::fmt().with_env_filter("off").init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                    "mac_rdp_server=info,ironrdp_server=info,ironrdp=warn".into()
                }),
            )
            .init();
    }

    let host = "0.0.0.0";
    let port = 3389;
    let bind_addr: SocketAddr = format!("{}:{}", host, port).parse()?;

    // Cấu hình tài khoản đăng nhập (Username & Password)
    let username = std::env::var("RDP_USER").unwrap_or_else(|_| "admin".to_string());
    let password = std::env::var("RDP_PASSWORD").unwrap_or_else(|_| "password123".to_string());

    let mode = std::env::var("RDP_MODE").unwrap_or_else(|_| "speed".to_string());
    let tile_custom = std::env::var("RDP_TILE").unwrap_or_else(|_| "320x24 (Default)".to_string());
    let color_custom = std::env::var("RDP_COLOR")
        .or_else(|_| std::env::var("RDP_BITS"))
        .unwrap_or_else(|_| "6bit (Default)".to_string());

    info!("============================================================");
    info!("🚀 MAC RDP SERVER (High-Speed & Low-Latency Remote)");
    info!("📡 Listening on: {}:{}", host, port);
    info!("🔑 Auth Username: {}", username);
    info!("🔑 Auth Password: {}", password);
    info!(
        "⚡ Active Profile: {} (Tùy biến qua RDP_MODE=speed / quality / balanced)",
        mode
    );
    info!(
        "🎨 Active Color Depth: {} (Tùy biến qua RDP_COLOR=4bit / 5bit / 6bit / 8bit)",
        color_custom
    );
    info!(
        "🧩 Active Tile Grid: {} (Tùy biến qua RDP_TILE=320x24 hoặc 320x32)",
        tile_custom
    );
    info!(
        "🚀 Adaptive Motion Controller: ENABLED (Vi mô: 60 FPS, 10%: 30 FPS, 20%: 20 FPS, 30%: 10 FPS, 50%: 2 FPS/0.3s, >70%: 1 FPS/0.5s)"
    );
    info!("💡 Cấu hình mặc định tối ưu:");
    info!("   - RDP_TILE=320x24 (Lưới ô gạch siêu tối ưu)");
    info!("   - RDP_COLOR=6bit (Nén lượng tử 6-bit sắc nét, mượt mà)");
    info!("   - RDP_FPS=60 (Chụp liên tục 60 FPS để chuột & gõ phím mượt 100%)");
    info!("============================================================");

    // Kiểm tra quyền Accessibility & Screen Recording
    check_accessibility_permission();
    mac_display::check_screen_recording_permission();

    // Tự động tắt hiệu ứng zoom/animation của macOS để hiển thị tức thì không bị delay
    optimize_macos_animations();

    // Chuẩn bị TLS
    let cert_path = Path::new("certs/rdp_cert.pem");
    let key_path = Path::new("certs/rdp_key.pem");
    let tls_acceptor = ensure_tls_certificate(cert_path, key_path)?;

    // Cấu hình Credential Validator
    let credentials = Credentials {
        username,
        password,
        domain: None,
    };
    let validator = Arc::new(ExactMatchCredentialValidator::new(credentials));

    // Khởi tạo Real macOS Display & Real Input Handler (đồng bộ tỉ lệ tọa độ)
    let display_handler =
        MacDisplay::new().context("Failed to initialize macOS display capture")?;
    let input_handler = MacInputHandler::new(
        display_handler.rdp_width,
        display_handler.rdp_height,
        display_handler.mac_logical_width,
        display_handler.mac_logical_height,
    );

    // Cấu hình Codec Capabilities chuẩn (FastPath Bitmap RDP6 Planar RLE)
    let codecs = ironrdp::pdu::rdp::capability_sets::BitmapCodecs(vec![]);

    // Xây dựng RDP Server
    let mut server = RdpServer::builder()
        .with_addr(bind_addr)
        .with_tls(tls_acceptor)
        .with_input_handler(input_handler)
        .with_display_handler(display_handler)
        .with_max_request_size(32 * 1024 * 1024) // 32 MB buffer to prevent subregion slice bug
        .with_bitmap_codecs(codecs)
        .build();

    server.set_credential_validator(Some(validator));

    // Khởi chạy server
    info!("Server is ready and accepting RDP connections...");
    if let Err(e) = server.run().await {
        error!("RDP Server exited with error: {:?}", e);
    }

    Ok(())
}
