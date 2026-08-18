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

/// Tối ưu hóa UI macOS: Vô hiệu hóa hiệu ứng zoom/resize animation, bóng mờ transparency và smooth scroll để phản hồi tức thì
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
        .args(&["write", "-g", "NSScrollAnimationEnabled", "-bool", "false"])
        .status();
    let _ = std::process::Command::new("defaults")
        .args(&["write", "-g", "NSScrollViewRubberbanding", "-bool", "false"])
        .status();
    let _ = std::process::Command::new("defaults")
        .args(&["write", "-g", "AppleReduceDesktopTinting", "-bool", "true"])
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
        "⚡ [MACOS OPTIMIZATION] UI Animations, transparency & smooth-scroll delays disabled for instant response"
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

#[derive(Debug, Clone)]
struct CliConfig {
    user: String,
    password: String,
    domain: Option<String>,
    port: u16,
    host: String,
    color: String,
    tile: String,
    fps: u32,
    res: String,
    mode: String,
    no_log: bool,
    verbose: bool,
    daemon: bool,
    quit: bool,
    status: bool,
    help: bool,
}

fn print_help() {
    println!("Mac RDP Server - High-Performance Remote Desktop for macOS");
    println!();
    println!("USAGE:");
    println!("  mac-rdp-server [OPTIONS]");
    println!();
    println!("OPTIONS:");
    println!("  -u, --user <USERNAME>       Set RDP auth username [default: dev]");
    println!("  -p, --password <PASSWORD>   Set RDP auth password [default: 12345678]");
    println!("  -D, --domain <DOMAIN>       Set RDP auth domain (optional) [default: none]");
    println!("  -P, --port <PORT>           Set listening port [default: 3389]");
    println!("  -H, --host <HOST>           Set listening host IP [default: 0.0.0.0]");
    println!("  -c, --color <DEPTH>         Color depth: 4bit, 5bit, 6bit, 8bit [default: 6bit]");
    println!(
        "  -t, --tile <WIDTHxHEIGHT>   Tile grid size: 320x24, 320x32, etc. [default: 320x24]"
    );
    println!("  -f, --fps <FPS>             Maximum capture FPS [default: 60]");
    println!("  -r, --res <RES>             Resolution: native, 1080p, 720p [default: native]");
    println!(
        "  -m, --mode <MODE>           Profile preset: speed, quality, balanced [default: speed]"
    );
    println!("  -v, --verbose, --debug      Enable verbose debug logging (real-time key events)");
    println!("  -d, --daemon                Run server in background (daemon mode)");
    println!("  -q, --quit                  Stop running background server");
    println!("  -s, --status                Check background server status");
    println!("  -nl, --no-log               Disable server logging (quiet mode)");
    println!("  -h, --help                  Print this help menu");
    println!();
    println!("EXAMPLES:");
    println!("  mac-rdp-server --user dev --password 12345678");
    println!("  mac-rdp-server -u dev -p 12345678 -D WORKGROUP");
    println!("  mac-rdp-server -u dev -p 12345678 -d");
    println!("  mac-rdp-server -u dev -p 12345678 -c 4bit -t 320x32 -f 60 -d");
    println!("  mac-rdp-server --quit");
}

fn parse_cli_args() -> CliConfig {
    let args: Vec<String> = std::env::args().collect();
    let mut user = std::env::var("RDP_USER").unwrap_or_else(|_| "dev".to_string());
    let mut password = std::env::var("RDP_PASSWORD").unwrap_or_else(|_| "12345678".to_string());
    let mut domain = std::env::var("RDP_DOMAIN")
        .ok()
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty());
    let mut port: u16 = std::env::var("RDP_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3389);
    let mut host = std::env::var("RDP_HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
    let mut color = std::env::var("RDP_COLOR")
        .or_else(|_| std::env::var("RDP_BITS"))
        .unwrap_or_else(|_| "6bit".to_string());
    let mut tile = std::env::var("RDP_TILE").unwrap_or_else(|_| "320x24".to_string());
    let mut fps: u32 = std::env::var("RDP_FPS")
        .ok()
        .and_then(|f| f.parse().ok())
        .unwrap_or(60);
    let mut res = std::env::var("RDP_RES").unwrap_or_else(|_| "native".to_string());
    let mut mode = std::env::var("RDP_MODE").unwrap_or_else(|_| "speed".to_string());
    let mut no_log = std::env::var("RDP_NO_LOG")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let mut verbose = false;
    let mut daemon = false;
    let mut quit = false;
    let mut status = false;
    let mut help = false;

    let mut i = 1;
    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "-u" | "--user" | "--username" => {
                if i + 1 < args.len() {
                    user = args[i + 1].clone();
                    i += 1;
                }
            }
            "-p" | "--password" | "--pass" => {
                if i + 1 < args.len() {
                    password = args[i + 1].clone();
                    i += 1;
                }
            }
            "-D" | "--domain" => {
                if i + 1 < args.len() {
                    let val = args[i + 1].trim().to_string();
                    domain = if val.is_empty() { None } else { Some(val) };
                    i += 1;
                }
            }
            "-P" | "--port" => {
                if i + 1 < args.len() {
                    if let Ok(val) = args[i + 1].parse() {
                        port = val;
                    }
                    i += 1;
                }
            }
            "-H" | "--host" => {
                if i + 1 < args.len() {
                    host = args[i + 1].clone();
                    i += 1;
                }
            }
            "-c" | "--color" | "--bits" => {
                if i + 1 < args.len() {
                    color = args[i + 1].clone();
                    i += 1;
                }
            }
            "-t" | "--tile" => {
                if i + 1 < args.len() {
                    tile = args[i + 1].clone();
                    i += 1;
                }
            }
            "-f" | "--fps" => {
                if i + 1 < args.len() {
                    if let Ok(val) = args[i + 1].parse() {
                        fps = val;
                    }
                    i += 1;
                }
            }
            "-r" | "--res" | "--resolution" => {
                if i + 1 < args.len() {
                    res = args[i + 1].clone();
                    i += 1;
                }
            }
            "-m" | "--mode" => {
                if i + 1 < args.len() {
                    mode = args[i + 1].clone();
                    i += 1;
                }
            }
            "-v" | "--verbose" | "--debug" => {
                verbose = true;
            }
            "-d" | "--daemon" | "start" => {
                daemon = true;
            }
            "-q" | "--quit" | "stop" => {
                quit = true;
            }
            "-s" | "--status" | "status" => {
                status = true;
            }
            "-nl" | "--no-log" => {
                no_log = true;
            }
            "-h" | "--help" | "help" => {
                help = true;
            }
            _ => {}
        }
        i += 1;
    }

    CliConfig {
        user,
        password,
        domain,
        port,
        host,
        color,
        tile,
        fps,
        res,
        mode,
        no_log,
        verbose,
        daemon,
        quit,
        status,
        help,
    }
}

/// Khởi động Server chạy ngầm dưới dạng Daemon (--daemon)
fn handle_daemon(cli: &CliConfig) -> Result<()> {
    if let Ok(pid_str) = std::fs::read_to_string(PID_FILE) {
        if let Ok(pid) = pid_str.trim().parse::<u32>() {
            if is_process_running(pid) {
                println!("⚠️ Mac RDP Server is already running (PID: {}).", pid);
                if !cli.no_log {
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
    cmd.arg("-u").arg(&cli.user);
    cmd.arg("-p").arg(&cli.password);
    if let Some(dom) = &cli.domain {
        cmd.arg("-D").arg(dom);
    }
    cmd.arg("-P").arg(cli.port.to_string());
    cmd.arg("-H").arg(&cli.host);
    cmd.arg("-c").arg(&cli.color);
    cmd.arg("-t").arg(&cli.tile);
    cmd.arg("-f").arg(cli.fps.to_string());
    cmd.arg("-r").arg(&cli.res);
    cmd.arg("-m").arg(&cli.mode);

    if cli.no_log {
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
    println!("📡 Listening on: {}:{}", cli.host, cli.port);
    println!("🔑 Auth User: {}", cli.user);
    if let Some(dom) = &cli.domain {
        println!("🏢 Auth Domain: {}", dom);
    } else {
        println!("🏢 Auth Domain: None (Open / Optional)");
    }
    println!("🎨 Color Depth: {}", cli.color);
    println!("🧩 Tile Grid: {}", cli.tile);
    println!("⚡ FPS: {}", cli.fps);
    println!("🆔 PID: {}", pid);
    if cli.no_log {
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
    let cli = parse_cli_args();

    if cli.help {
        print_help();
        return Ok(());
    }

    // Xử lý tham số quản lý tiến trình
    if cli.quit {
        return handle_quit();
    }
    if cli.daemon {
        return handle_daemon(&cli);
    }
    if cli.status {
        return handle_status();
    }

    // Đẩy cấu hình vào biến môi trường để các module và crate phụ thuộc nhận chính xác
    unsafe {
        std::env::set_var("RDP_USER", &cli.user);
        std::env::set_var("RDP_PASSWORD", &cli.password);
        if let Some(dom) = &cli.domain {
            std::env::set_var("RDP_DOMAIN", dom);
        } else {
            std::env::remove_var("RDP_DOMAIN");
        }
        std::env::set_var("RDP_HOST", &cli.host);
        std::env::set_var("RDP_PORT", cli.port.to_string());
        std::env::set_var("RDP_COLOR", &cli.color);
        std::env::set_var("RDP_TILE", &cli.tile);
        std::env::set_var("RDP_FPS", cli.fps.to_string());
        std::env::set_var("RDP_RES", &cli.res);
        std::env::set_var("RDP_MODE", &cli.mode);
    }

    // Ghi PID vào PID_FILE
    let my_pid = std::process::id();
    let _ = std::fs::write(PID_FILE, my_pid.to_string());

    // Khởi tạo logger (tắt hoàn toàn khi có cờ --no-log, bật debug chi tiết khi có cờ -v / --verbose)
    if cli.no_log {
        tracing_subscriber::fmt().with_env_filter("off").init();
    } else {
        let default_filter = if cli.verbose {
            "mac_rdp_server=debug,ironrdp_server=info,ironrdp=warn"
        } else {
            "mac_rdp_server=info,ironrdp_server=info,ironrdp=warn"
        };
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| default_filter.into()),
            )
            .init();
    }

    let bind_addr: SocketAddr = format!("{}:{}", cli.host, cli.port).parse()?;

    info!("============================================================");
    info!("🚀 MAC RDP SERVER (High-Speed & Low-Latency Remote)");
    info!("📡 Listening on: {}:{}", cli.host, cli.port);
    info!("🔑 Auth Username: {}", cli.user);
    info!("🔑 Auth Password: {}", cli.password);
    if let Some(dom) = &cli.domain {
        info!("🏢 Auth Domain: {}", dom);
    } else {
        info!("🏢 Auth Domain: None (Mở / Không bắt buộc domain, chấp nhận domain rỗng)");
    }
    info!(
        "⚡ Active Profile: {} (Tùy biến qua -m / --mode speed / quality / balanced)",
        cli.mode
    );
    info!(
        "🎨 Active Color Depth: {} (Tùy biến qua -c / --color 4bit / 5bit / 6bit / 8bit)",
        cli.color
    );
    info!(
        "🧩 Active Tile Grid: {} (Tùy biến qua -t / --tile 320x24 hoặc 320x32)",
        cli.tile
    );
    info!(
        "🚀 Adaptive Motion Controller: ENABLED (Vi mô: 60 FPS, 10%: 30 FPS, 20%: 20 FPS, 30%: 10 FPS, 50%: 2 FPS/0.3s, >70%: 1 FPS/0.5s)"
    );
    info!("💡 Cấu hình mặc định tối ưu:");
    info!("   - Lưới ô gạch: {} (Lưới ô gạch siêu tối ưu)", cli.tile);
    info!(
        "   - Mức nén màu: {} (Nén lượng tử 6-bit sắc nét, mượt mà)",
        cli.color
    );
    info!(
        "   - Tần số quét: {} FPS (Chụp liên tục 60 FPS để chuột & gõ phím mượt 100%)",
        cli.fps
    );
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
        username: cli.user,
        password: cli.password,
        domain: cli.domain,
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

    let codecs =
        ironrdp::pdu::rdp::capability_sets::server_codecs_capabilities(&["rfx", "nscodec"])
            .unwrap_or_else(|_| ironrdp::pdu::rdp::capability_sets::BitmapCodecs(vec![]));

    // Xây dựng RDP Server
    let mut server = RdpServer::builder()
        .with_addr(bind_addr)
        .with_tls(tls_acceptor)
        .with_input_handler(input_handler)
        .with_display_handler(display_handler)
        .with_honor_client_desktop_size(true)
        .with_max_request_size(8 * 1024 * 1024) // 8 MB buffer (Standard MS-RDPBCGR limit)
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
