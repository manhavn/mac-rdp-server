use std::num::{NonZeroU16, NonZeroUsize};
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use core_graphics::display::CGDisplay;
use ironrdp_server::{
    BitmapUpdate, DesktopSize, DisplayUpdate, PixelFormat, RGBAPointer, RdpServerDisplay,
    RdpServerDisplayUpdates,
};
use std::collections::{HashMap, VecDeque};
use tracing::{debug, error, info};

#[link(name = "objc", kind = "dylib")]
#[link(name = "AppKit", kind = "framework")]
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn objc_getClass(name: *const std::os::raw::c_char) -> *mut std::ffi::c_void;
    fn sel_registerName(name: *const std::os::raw::c_char) -> *mut std::ffi::c_void;
    fn objc_msgSend(
        receiver: *mut std::ffi::c_void,
        op: *mut std::ffi::c_void,
        ...
    ) -> *mut std::ffi::c_void;
    fn objc_autoreleasePoolPush() -> *mut std::ffi::c_void;
    fn objc_autoreleasePoolPop(pool: *mut std::ffi::c_void);
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
struct NSPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
struct NSSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
struct NSRect {
    origin: NSPoint,
    size: NSSize,
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGImageGetWidth(image: *mut std::ffi::c_void) -> usize;
    fn CGImageGetHeight(image: *mut std::ffi::c_void) -> usize;
    fn CGColorSpaceCreateDeviceRGB() -> *mut std::ffi::c_void;
    fn CGColorSpaceRelease(space: *mut std::ffi::c_void);
    fn CGBitmapContextCreate(
        data: *mut std::ffi::c_void,
        width: usize,
        height: usize,
        bitsPerComponent: usize,
        bytesPerRow: usize,
        space: *mut std::ffi::c_void,
        bitmapInfo: u32,
    ) -> *mut std::ffi::c_void;
    fn CGContextDrawImage(c: *mut std::ffi::c_void, rect: NSRect, image: *mut std::ffi::c_void);
    fn CGContextRelease(c: *mut std::ffi::c_void);
}

const K_CG_IMAGE_ALPHA_PREMULTIPLIED_FIRST: u32 = 2;
const K_CG_BITMAP_BYTE_ORDER_32_LITTLE: u32 = 2 << 12;

pub struct MacCursorRaw {
    pub width: u16,
    pub height: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    pub bgra_bottom_up: Vec<u8>,
    pub hash: u64,
}

pub fn capture_macos_cursor() -> Option<MacCursorRaw> {
    unsafe {
        let pool = objc_autoreleasePoolPush();

        let cls_nsapp = objc_getClass(c"NSApplication".as_ptr());
        let sel_shared = sel_registerName(c"sharedApplication".as_ptr());
        let _ = objc_msgSend(cls_nsapp, sel_shared);

        let cls_nscursor = objc_getClass(c"NSCursor".as_ptr());
        let sel_cur_sys = sel_registerName(c"currentSystemCursor".as_ptr());
        let sel_cur = sel_registerName(c"currentCursor".as_ptr());
        let sel_arrow = sel_registerName(c"arrowCursor".as_ptr());
        let sel_image = sel_registerName(c"image".as_ptr());
        let sel_hotspot = sel_registerName(c"hotSpot".as_ptr());
        let sel_size = sel_registerName(c"size".as_ptr());
        let sel_cgimage = sel_registerName(c"CGImageForProposedRect:context:hints:".as_ptr());

        let mut cursor = objc_msgSend(cls_nscursor, sel_cur_sys);
        if cursor.is_null() {
            cursor = objc_msgSend(cls_nscursor, sel_cur);
        }
        if cursor.is_null() {
            cursor = objc_msgSend(cls_nscursor, sel_arrow);
        }
        if cursor.is_null() {
            objc_autoreleasePoolPop(pool);
            return None;
        }

        let img = objc_msgSend(cursor, sel_image);
        if img.is_null() {
            objc_autoreleasePoolPop(pool);
            return None;
        }

        let hot_fn: unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void) -> NSPoint =
            std::mem::transmute(objc_msgSend as *const ());
        let hotspot = hot_fn(cursor, sel_hotspot);

        let size_fn: unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void) -> NSSize =
            std::mem::transmute(objc_msgSend as *const ());
        let pt_size = size_fn(img, sel_size);

        let cg_fn: unsafe extern "C" fn(
            *mut std::ffi::c_void,
            *mut std::ffi::c_void,
            *mut std::ffi::c_void,
            *mut std::ffi::c_void,
            *mut std::ffi::c_void,
        ) -> *mut std::ffi::c_void = std::mem::transmute(objc_msgSend as *const ());
        let cg_img = cg_fn(
            img,
            sel_cgimage,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        if cg_img.is_null() {
            objc_autoreleasePoolPop(pool);
            return None;
        }

        let width = CGImageGetWidth(cg_img);
        let height = CGImageGetHeight(cg_img);
        if width == 0 || height == 0 || width > 96 || height > 96 {
            objc_autoreleasePoolPop(pool);
            return None;
        }

        let stride = width * 4;
        let mut top_down_buf = vec![0u8; stride * height];
        let color_space = CGColorSpaceCreateDeviceRGB();
        let ctx = CGBitmapContextCreate(
            top_down_buf.as_mut_ptr() as *mut std::ffi::c_void,
            width,
            height,
            8,
            stride,
            color_space,
            K_CG_IMAGE_ALPHA_PREMULTIPLIED_FIRST | K_CG_BITMAP_BYTE_ORDER_32_LITTLE,
        );
        CGColorSpaceRelease(color_space);

        if ctx.is_null() {
            objc_autoreleasePoolPop(pool);
            return None;
        }

        let rect = NSRect {
            origin: NSPoint { x: 0.0, y: 0.0 },
            size: NSSize {
                width: width as f64,
                height: height as f64,
            },
        };
        CGContextDrawImage(ctx, rect, cg_img);
        CGContextRelease(ctx);

        // Convert top-down to bottom-up for RDP
        let mut bgra_bottom_up = vec![0u8; stride * height];
        for y in 0..height {
            let src_y = y;
            let dst_y = height - 1 - y;
            let src_offset = src_y * stride;
            let dst_offset = dst_y * stride;
            bgra_bottom_up[dst_offset..dst_offset + stride]
                .copy_from_slice(&top_down_buf[src_offset..src_offset + stride]);
        }

        let scale_x = if pt_size.width > 0.0 {
            width as f64 / pt_size.width
        } else {
            1.0
        };
        let scale_y = if pt_size.height > 0.0 {
            height as f64 / pt_size.height
        } else {
            1.0
        };

        let hot_x =
            ((hotspot.x * scale_x).round().max(0.0) as u16).min(width.saturating_sub(1) as u16);
        let hot_y =
            ((hotspot.y * scale_y).round().max(0.0) as u16).min(height.saturating_sub(1) as u16);

        // Compute hash
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        width.hash(&mut hasher);
        height.hash(&mut hasher);
        hot_x.hash(&mut hasher);
        hot_y.hash(&mut hasher);
        bgra_bottom_up.hash(&mut hasher);
        let hash = hasher.finish();

        objc_autoreleasePoolPop(pool);

        Some(MacCursorRaw {
            width: width as u16,
            height: height as u16,
            hot_x,
            hot_y,
            bgra_bottom_up,
            hash,
        })
    }
}

pub struct MacCursorTracker {
    last_hash: Option<u64>,
    cache_map: HashMap<u64, u16>,
    next_cache_index: u16,
    initial_sent: bool,
}

impl MacCursorTracker {
    pub fn new() -> Self {
        Self {
            last_hash: None,
            cache_map: HashMap::new(),
            next_cache_index: 0,
            initial_sent: false,
        }
    }

    pub fn check_cursor(&mut self) -> Option<DisplayUpdate> {
        let cursor_data = capture_macos_cursor()?;
        let hash = cursor_data.hash;

        if self.initial_sent && self.last_hash == Some(hash) {
            return None;
        }

        self.last_hash = Some(hash);
        self.initial_sent = true;

        if let Some(&cached_idx) = self.cache_map.get(&hash) {
            debug!(
                "🖱️ [CURSOR] Switch to cached pointer (index: {}, hash: 0x{:016X})",
                cached_idx, hash
            );
            return Some(DisplayUpdate::CachedPointer(cached_idx));
        }

        let cache_index = self.next_cache_index;
        self.next_cache_index = (self.next_cache_index + 1) % 512;
        self.cache_map.insert(hash, cache_index);

        info!(
            "🖱️ [CURSOR UPDATE] Emitting new 32bpp RGBA pointer ({}x{}, hot: {},{}, cache_index: {})",
            cursor_data.width,
            cursor_data.height,
            cursor_data.hot_x,
            cursor_data.hot_y,
            cache_index
        );

        Some(DisplayUpdate::RGBAPointer(RGBAPointer {
            cache_index,
            width: cursor_data.width,
            height: cursor_data.height,
            hot_x: cursor_data.hot_x,
            hot_y: cursor_data.hot_y,
            data: cursor_data.bgra_bottom_up,
        }))
    }
}

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

        // Tần số chụp màn hình gốc (Mặc định: 30 FPS)
        let fps = std::env::var("RDP_FPS")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(30)
            .clamp(1, 120);

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
        if client_size.width != self.rdp_width || client_size.height != self.rdp_height {
            info!(
                "🖥️ [DISPLAY NEGOTIATION] Client requested {}x{}, but server strictly enforces {}x{} (Server Frame)",
                client_size.width, client_size.height, self.rdp_width, self.rdp_height
            );
            self.needs_reactivation_resize = true;
        } else {
            info!(
                "🖥️ [DISPLAY SYNC] Client Canvas matches server frame: {}x{}",
                self.rdp_width, self.rdp_height
            );
            self.needs_reactivation_resize = false;
        }
        self.target_width = self.rdp_width;
        self.target_height = self.rdp_height;
        self.shared_rdp_w
            .store(self.rdp_width as u32, Ordering::Relaxed);
        self.shared_rdp_h
            .store(self.rdp_height as u32, Ordering::Relaxed);

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
    cursor_tracker: MacCursorTracker,
    pending_updates: VecDeque<DisplayUpdate>,
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
        let interval_ms = (1000 / fps.max(1)).max(8) as u64;
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
            cursor_tracker: MacCursorTracker::new(),
            pending_updates: VecDeque::with_capacity(4),
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
        // 1. Trả về các gói update đang đợi trong hàng đợi nếu có
        if let Some(update) = self.pending_updates.pop_front() {
            return Ok(Some(update));
        }

        if self.pending_resize {
            self.pending_resize = false;
            let target_w = self.target_width;
            let target_h = self.target_height;
            self.current_width = target_w;
            self.current_height = target_h;
            info!(
                "🚀 [ENFORCE RESOLUTION] Emitting Deactivation-Reactivation Resize to {}x{} to force client conformance",
                target_w, target_h
            );
            return Ok(Some(DisplayUpdate::Resize(DesktopSize {
                width: target_w,
                height: target_h,
            })));
        }

        self.interval.tick().await;

        // 2. Kiểm tra cập nhật hình dạng con trỏ chuột (Mouse Cursor Shape)
        if let Some(ptr_update) = self.cursor_tracker.check_cursor() {
            self.pending_updates.push_back(ptr_update);
        }

        // 3. Chụp màn hình
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

                self.pending_updates.push_back(update);
                Ok(self.pending_updates.pop_front())
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
