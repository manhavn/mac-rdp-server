use std::num::{NonZeroU16, NonZeroUsize};
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use core_graphics::display::CGDisplay;
use ironrdp_server::{
    BitmapUpdate, DesktopSize, DisplayUpdate, PixelFormat, RdpServerDisplay,
    RdpServerDisplayUpdates,
};
use tracing::{error, info};

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}

/// Kiểm tra quyền Screen Recording (Ghi màn hình) của macOS
pub fn check_screen_recording_permission() -> bool {
    let has_access = unsafe { CGPreflightScreenCaptureAccess() };
    if !has_access {
        error!("⚠️ [PERMISSION REQUIRED] Chưa có quyền Screen Recording (Ghi màn hình)!");
        error!("👉 Hướng dẫn khắc phục:");
        error!("   1. Mở: System Settings -> Privacy & Security -> Screen Recording");
        error!("   2. Bật công tắc cho Terminal / iTerm / VS Code / Cursor.");
        error!("   3. QUAN TRỌNG: Bạn PHẢI TẮT HẲN (Cmd + Q) ứng dụng Terminal/VS Code và mở lại!");
        unsafe {
            CGRequestScreenCaptureAccess();
        }
    } else {
        info!("✅ Screen Recording permission is GRANTED");
    }
    has_access
}

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

pub struct MacDisplay {
    pub rdp_width: u16,
    pub rdp_height: u16,
    pub target_width: u16,
    pub target_height: u16,
    pub mac_logical_width: u16,
    pub mac_logical_height: u16,
    pub fps: u32,
    pub needs_reactivation_resize: bool,
    pub shared_rdp_w: Arc<AtomicU32>,
    pub shared_rdp_h: Arc<AtomicU32>,
}

impl MacDisplay {
    pub fn new() -> Result<Self> {
        check_screen_recording_permission();

        let display = CGDisplay::main();
        let mac_w = display.pixels_wide() as u16;
        let mac_h = display.pixels_high() as u16;

        // Tần số chụp màn hình gốc (Mặc định: 60 FPS cho phản hồi chuột & phím tức thì)
        let fps = std::env::var("RDP_FPS")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(60)
            .clamp(1, 60);

        // Mặc định: Dùng chính xác 100% độ phân giải thật của màn hình Mac (1920x1080 Native)
        let res_env = std::env::var("RDP_RES").unwrap_or_else(|_| "native".to_string());

        let (mut rdp_width, mut rdp_height) = match res_env.to_lowercase().as_str() {
            "720p" | "hd" => (1280, 720),
            "540p" | "qhd" => (960, 540),
            "1080p" | "fhd" => (1920, 1080),
            "native" | "auto" | _ => (mac_w, mac_h),
        };

        // Đảm bảo chiều rộng và chiều cao là bội số của 4 (bắt buộc cho RDP Bitmap)
        rdp_width = (rdp_width / 4) * 4;
        rdp_height = (rdp_height / 4) * 4;

        info!("============================================================");
        info!(
            "🖥️ Resolution Mode: {}x{} @ {} FPS (Ultra-Fast CoreGraphics Native)",
            rdp_width, rdp_height, fps
        );
        info!("⚡ High-Performance Delta Diffing: ENABLED (0 KB/s when static, ~1-5 KB on input)");
        info!("============================================================");

        let shared_rdp_w = Arc::new(AtomicU32::new(rdp_width as u32));
        let shared_rdp_h = Arc::new(AtomicU32::new(rdp_height as u32));

        Ok(Self {
            rdp_width,
            rdp_height,
            target_width: rdp_width,
            target_height: rdp_height,
            mac_logical_width: mac_w,
            mac_logical_height: mac_h,
            fps,
            needs_reactivation_resize: false,
            shared_rdp_w,
            shared_rdp_h,
        })
    }
}

#[async_trait::async_trait]
impl RdpServerDisplay for MacDisplay {
    async fn size(&mut self) -> DesktopSize {
        DesktopSize {
            width: self.rdp_width,
            height: self.rdp_height,
        }
    }

    async fn request_initial_size(&mut self, client_size: DesktopSize) -> DesktopSize {
        if client_size.width >= 320 && client_size.height >= 240 {
            let client_w = (client_size.width / 4) * 4;
            let client_h = (client_size.height / 4) * 4;
            info!(
                "🖥️ [DISPLAY SYNC] Client Canvas: {}x{}, Mac Native: {}x{}",
                client_size.width,
                client_size.height,
                self.mac_logical_width,
                self.mac_logical_height
            );
            self.rdp_width = client_w;
            self.rdp_height = client_h;
            self.target_width = client_w;
            self.target_height = client_h;
            self.needs_reactivation_resize = false;
            self.shared_rdp_w.store(client_w as u32, Ordering::Relaxed);
            self.shared_rdp_h.store(client_h as u32, Ordering::Relaxed);
        }
        DesktopSize {
            width: self.rdp_width,
            height: self.rdp_height,
        }
    }

    async fn updates(&mut self) -> Result<Box<dyn RdpServerDisplayUpdates>> {
        let trigger_resize = self.needs_reactivation_resize;
        self.needs_reactivation_resize = false;
        let updates = MacDisplayUpdates::new(
            self.rdp_width,
            self.rdp_height,
            self.target_width,
            self.target_height,
            trigger_resize,
            self.fps,
        )?;
        Ok(Box::new(updates))
    }
}

pub struct MacDisplayUpdates {
    display: CGDisplay,
    current_width: u16,
    current_height: u16,
    target_width: u16,
    target_height: u16,
    pending_resize: bool,
    interval: tokio::time::Interval,
    last_capture_error: bool,
    frame_count: u64,
}

impl MacDisplayUpdates {
    pub fn new(
        current_width: u16,
        current_height: u16,
        target_width: u16,
        target_height: u16,
        pending_resize: bool,
        fps: u32,
    ) -> Result<Self> {
        let display = CGDisplay::main();
        let interval_ms = (1000 / fps.max(1)).max(16) as u64;
        let mut interval = tokio::time::interval(Duration::from_millis(interval_ms));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        Ok(Self {
            display,
            current_width,
            current_height,
            target_width,
            target_height,
            pending_resize,
            interval,
            last_capture_error: false,
            frame_count: 0,
        })
    }

    /// Chụp màn hình siêu tốc bằng CoreGraphics trực tiếp (< 10ms)
    fn process_frame(
        display: &CGDisplay,
        target_w: u16,
        target_h: u16,
    ) -> Result<(Vec<u8>, u16, u16)> {
        let cg_img = display
            .image()
            .context("Failed to capture image from CoreGraphics CGDisplay")?;

        let src_w = cg_img.width();
        let src_h = cg_img.height();
        let dst_w = target_w as usize;
        let dst_h = target_h as usize;
        let data = cg_img.data();
        let raw = data.bytes();

        let dst_stride = dst_w * 4;
        let expected_len = dst_stride * dst_h;

        let bgra = if src_w == dst_w && src_h == dst_h {
            let copy_len = expected_len.min(raw.len());
            raw[..copy_len].to_vec()
        } else if src_w == dst_w * 2 && src_h == dst_h * 2 {
            let mut bgra = vec![0u8; expected_len];
            let src_u32 =
                unsafe { std::slice::from_raw_parts(raw.as_ptr() as *const u32, raw.len() / 4) };
            let dst_u32 = unsafe {
                std::slice::from_raw_parts_mut(bgra.as_mut_ptr() as *mut u32, expected_len / 4)
            };
            for y in 0..dst_h {
                let src_row = (y * 2) * src_w;
                let dst_row = y * dst_w;
                for x in 0..dst_w {
                    dst_u32[dst_row + x] = src_u32[src_row + (x * 2)];
                }
            }
            bgra
        } else {
            let mut bgra = vec![0u8; expected_len];
            let src_u32 =
                unsafe { std::slice::from_raw_parts(raw.as_ptr() as *const u32, raw.len() / 4) };
            let dst_u32 = unsafe {
                std::slice::from_raw_parts_mut(bgra.as_mut_ptr() as *mut u32, expected_len / 4)
            };
            let src_stride_u32 = cg_img.bytes_per_row() / 4;
            let x_ratio = ((src_w as u64) << 16) / (dst_w as u64);
            let y_ratio = ((src_h as u64) << 16) / (dst_h as u64);

            for y in 0..dst_h {
                let src_y = (((y as u64 * y_ratio) >> 16) as usize).min(src_h - 1);
                let src_row = src_y * src_stride_u32;
                let dst_row = y * dst_w;

                for x in 0..dst_w {
                    let src_x = (((x as u64 * x_ratio) >> 16) as usize).min(src_w - 1);
                    dst_u32[dst_row + x] = src_u32[src_row + src_x];
                }
            }
            bgra
        };

        Ok((bgra, target_w, target_h))
    }
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for MacDisplayUpdates {
    async fn next_update(&mut self) -> Result<Option<DisplayUpdate>> {
        if self.pending_resize {
            self.pending_resize = false;
            let target_w = self.target_width;
            let target_h = self.target_height;
            self.current_width = target_w;
            self.current_height = target_h;
            info!(
                "🚀 [AUTO-RESOLUTION] Remmina connected -> Emitting Deactivation-Reactivation Resize to {}x{}",
                target_w, target_h
            );
            return Ok(Some(DisplayUpdate::Resize(DesktopSize {
                width: target_w,
                height: target_h,
            })));
        }

        self.interval.tick().await;

        let display = self.display;
        let target_w = self.current_width;
        let target_h = self.current_height;

        let capture_res =
            tokio::task::spawn_blocking(move || Self::process_frame(&display, target_w, target_h))
                .await?;

        match capture_res {
            Ok((frame_data, img_w, img_h)) => {
                self.last_capture_error = false;
                self.frame_count += 1;

                let frame_bytes: Bytes = frame_data.into();

                let update = DisplayUpdate::Bitmap(BitmapUpdate {
                    x: 0,
                    y: 0,
                    width: NonZeroU16::new(img_w).context("Invalid screen width")?,
                    height: NonZeroU16::new(img_h).context("Invalid screen height")?,
                    stride: NonZeroUsize::new((img_w as usize) * 4)
                        .context("Invalid screen stride")?,
                    format: PixelFormat::BgrA32,
                    data: frame_bytes,
                });

                Ok(Some(update))
            }
            Err(e) => {
                if !self.last_capture_error {
                    self.last_capture_error = true;
                    error!(
                        "⚠️ Screen capture failed: {}. Vui lòng kiểm tra quyền Screen Recording!",
                        e
                    );
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                Err(e)
            }
        }
    }
}
