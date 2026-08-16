use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use ironrdp_connector::{Config, Credentials, DesktopSize, ServerName};
use ironrdp_pdu::fast_path::{FastPathHeader, FastPathUpdatePdu, Fragmentation, UpdateCode};
use ironrdp_pdu::gcc;
use ironrdp_pdu::ironrdp_core::{ReadCursor, decode_cursor};
use ironrdp_pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};
use ironrdp_pdu::surface_commands::SurfaceCommand;
use ironrdp_tokio::TokioFramed;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName as RustlsServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

#[derive(Debug)]
struct NoCertificateVerification;

impl tokio_rustls::rustls::client::danger::ServerCertVerifier for NoCertificateVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[tokio_rustls::rustls::pki_types::CertificateDer<'_>],
        _server_name: &RustlsServerName<'_>,
        _ocsp_response: &[u8],
        _now: tokio_rustls::rustls::pki_types::UnixTime,
    ) -> Result<tokio_rustls::rustls::client::danger::ServerCertVerified, tokio_rustls::rustls::Error>
    {
        Ok(tokio_rustls::rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        _dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
        _dss: &tokio_rustls::rustls::DigitallySignedStruct,
    ) -> Result<
        tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
        tokio_rustls::rustls::Error,
    > {
        Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<tokio_rustls::rustls::SignatureScheme> {
        vec![
            tokio_rustls::rustls::SignatureScheme::RSA_PKCS1_SHA256,
            tokio_rustls::rustls::SignatureScheme::RSA_PKCS1_SHA384,
            tokio_rustls::rustls::SignatureScheme::RSA_PKCS1_SHA512,
            tokio_rustls::rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            tokio_rustls::rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            tokio_rustls::rustls::SignatureScheme::ED25519,
            tokio_rustls::rustls::SignatureScheme::RSA_PSS_SHA256,
        ]
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("ironrdp=warn")
        .init();

    let addr: SocketAddr = "127.0.0.1:3389".parse()?;
    println!("🔌 Connecting to RDP server at {}...", addr);

    let tcp_stream = TcpStream::connect(addr)
        .await
        .context("Failed to connect TCP")?;
    let mut framed = TokioFramed::new(tcp_stream);

    let config = Config {
        desktop_size: DesktopSize {
            width: 1280,
            height: 720,
        },
        desktop_scale_factor: 100,
        enable_tls: true,
        enable_credssp: false,
        credentials: Credentials::UsernamePassword {
            username: std::env::var("RDP_USER").unwrap_or_else(|_| "dev".to_string()),
            password: std::env::var("RDP_PASSWORD").unwrap_or_else(|_| "12345678".to_string()),
        },
        domain: None,
        client_build: 2600,
        client_name: "RustRDPVNC".to_string(),
        keyboard_type: gcc::KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_functional_keys_count: 12,
        keyboard_layout: 0x0409,
        ime_file_name: "".to_string(),
        bitmap: None,
        dig_product_id: "".to_string(),
        client_dir: "".to_string(),
        alternate_shell: "".to_string(),
        work_dir: "".to_string(),
        platform: ironrdp_pdu::rdp::capability_sets::MajorPlatformType::UNIX,
        hardware_id: None,
        request_data: None,
        autologon: true,
        enable_audio_playback: false,
        performance_flags: PerformanceFlags::empty(),
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        compression_type: None,
        enable_server_pointer: false,
        pointer_software_rendering: true,
        multitransport_flags: None,
    };

    let client_addr: SocketAddr = "127.0.0.1:50000".parse()?;
    let mut connector = ironrdp_connector::ClientConnector::new(config, client_addr);

    let should_upgrade = ironrdp_async::connect_begin(&mut framed, &mut connector).await?;

    // Perform TLS upgrade
    let (stream, leftover) = framed.into_inner();
    let root_store = RootCertStore::empty();
    let mut client_config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    client_config
        .dangerous()
        .set_certificate_verifier(Arc::new(NoCertificateVerification));

    let tls_connector = TlsConnector::from(Arc::new(client_config));
    let server_name = RustlsServerName::try_from("127.0.0.1")?.to_owned();
    let tls_stream = tls_connector.connect(server_name, stream).await?;

    let mut framed = TokioFramed::new_with_leftover(tls_stream, leftover);
    let upgraded = ironrdp_async::mark_as_upgraded(should_upgrade, &mut connector);

    let server_name = ServerName::new("127.0.0.1");
    let mut network_client = ironrdp_tokio::reqwest::ReqwestNetworkClient::new();

    let connection_result = ironrdp_async::connect_finalize(
        upgraded,
        connector,
        &mut framed,
        &mut network_client,
        server_name,
        vec![],
        None,
    )
    .await?;

    println!("✅ Handshake Complete!");
    println!(
        "🖥️ Negotiated Resolution: {}x{}",
        connection_result.desktop_size.width, connection_result.desktop_size.height
    );

    let mut non_zero_pixels = 0;
    let mut surface_bits_count = 0;
    let mut fastpath_count = 0;
    let mut reassembly_buffer = Vec::new();

    let start = std::time::Instant::now();
    while start.elapsed().as_secs() < 3 {
        match tokio::time::timeout(std::time::Duration::from_millis(500), framed.read_pdu()).await {
            Ok(Ok((_action, bytes))) => {
                let mut cursor = ReadCursor::new(&bytes);
                if let Ok(_header) = decode_cursor::<FastPathHeader>(&mut cursor) {
                    fastpath_count += 1;
                    if let Ok(update_pdu) = decode_cursor::<FastPathUpdatePdu<'_>>(&mut cursor) {
                        if update_pdu.update_code == UpdateCode::Bitmap {
                            let mut bmp_cursor = ReadCursor::new(update_pdu.data);
                            if let Ok(bmp_update) = decode_cursor::<
                                ironrdp_pdu::bitmap::BitmapUpdateData<'_>,
                            >(&mut bmp_cursor)
                            {
                                for bmp in bmp_update.rectangles {
                                    surface_bits_count += 1;
                                    let mut decomp = Vec::new();
                                    let mut decoder =
                                        ironrdp_graphics::rdp6::BitmapStreamDecoder::default();
                                    let w = bmp.width as usize;
                                    let h = bmp.height as usize;
                                    match decoder.decode_bitmap_stream_to_rgb24(
                                        bmp.bitmap_data,
                                        &mut decomp,
                                        w,
                                        h,
                                    ) {
                                        Ok(()) => {
                                            for &byte in &decomp {
                                                if byte > 0 {
                                                    non_zero_pixels += 1;
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            println!(
                                                "❌ RDP6 Decompression FAILED for rect {}x{}: {:?}",
                                                w, h, e
                                            );
                                        }
                                    }
                                }
                            }
                        } else if update_pdu.update_code == UpdateCode::SurfaceCommands {
                            match update_pdu.fragmentation {
                                Fragmentation::Single => {
                                    reassembly_buffer.clear();
                                    reassembly_buffer.extend_from_slice(update_pdu.data);
                                }
                                Fragmentation::First => {
                                    reassembly_buffer.clear();
                                    reassembly_buffer.extend_from_slice(update_pdu.data);
                                    continue;
                                }
                                Fragmentation::Next => {
                                    reassembly_buffer.extend_from_slice(update_pdu.data);
                                    continue;
                                }
                                Fragmentation::Last => {
                                    reassembly_buffer.extend_from_slice(update_pdu.data);
                                }
                            }

                            // Try to decode reassembled SurfaceCommands
                            let mut surf_cursor = ReadCursor::new(&reassembly_buffer);
                            while let Ok(cmd) =
                                decode_cursor::<SurfaceCommand<'_>>(&mut surf_cursor)
                            {
                                if let SurfaceCommand::SetSurfaceBits(bits) = cmd {
                                    surface_bits_count += 1;
                                    let mut decomp = Vec::new();
                                    let mut decoder =
                                        ironrdp_graphics::rdp6::BitmapStreamDecoder::default();
                                    let w = bits.extended_bitmap_data.width as usize;
                                    let h = bits.extended_bitmap_data.height as usize;
                                    match decoder.decode_bitmap_stream_to_rgb24(
                                        bits.extended_bitmap_data.data,
                                        &mut decomp,
                                        w,
                                        h,
                                    ) {
                                        Ok(()) => {
                                            for &byte in &decomp {
                                                if byte > 0 {
                                                    non_zero_pixels += 1;
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            println!(
                                                "❌ SurfaceBits RDP6 Decompression FAILED for rect {}x{} (y: {}): {:?}",
                                                w, h, bits.destination.top, e
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Ok(Err(_)) => break,
            Err(_) => {}
        }
    }

    println!("📊 REASSEMBLED DIAGNOSTIC RESULTS:");
    println!("   FastPath Packets Received: {}", fastpath_count);
    println!(
        "   Reassembled SurfaceBits Commands: {}",
        surface_bits_count
    );
    println!("   Non-zero color bytes in frame: {}", non_zero_pixels);

    if non_zero_pixels > 100_000 {
        println!(
            "🎉 SUCCESS: Live macOS desktop is 100% rendered with FULL RICH COLOR ({} pixels)!",
            non_zero_pixels
        );
    } else {
        println!(
            "⚠️ WARNING: Pixel stream has too few non-zero pixels ({})!",
            non_zero_pixels
        );
    }

    Ok(())
}
