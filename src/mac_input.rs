use ironrdp_server::{KeyboardEvent, MouseEvent, RdpServerInputHandler};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CGPoint {
    pub x: f64,
    pub y: f64,
}

// CoreGraphics C FFI declarations (Rust 2024 edition requires unsafe extern)
#[link(name = "CoreGraphics", kind = "framework")]
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn CGEventCreateMouseEvent(
        source: *const std::ffi::c_void,
        mouseType: u32,
        mouseCursorPosition: CGPoint,
        mouseButton: u32,
    ) -> *mut std::ffi::c_void;

    fn CGEventCreateKeyboardEvent(
        source: *const std::ffi::c_void,
        virtualKey: u16,
        keyDown: bool,
    ) -> *mut std::ffi::c_void;

    fn CGEventCreateScrollWheelEvent2(
        source: *const std::ffi::c_void,
        units: u32,
        wheelCount: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> *mut std::ffi::c_void;

    fn CGEventKeyboardSetUnicodeString(
        event: *mut std::ffi::c_void,
        stringLength: usize,
        unicodeString: *const u16,
    );

    fn CGEventSetFlags(event: *mut std::ffi::c_void, flags: u64);
    #[allow(dead_code)]
    fn CGEventGetFlags(event: *mut std::ffi::c_void) -> u64;

    fn CGEventSetIntegerValueField(event: *mut std::ffi::c_void, field: u32, value: i64);

    fn CGEventPost(tap: u32, event: *mut std::ffi::c_void);
    fn CFRelease(cf: *const std::ffi::c_void);
    fn AXIsProcessTrustedWithOptions(options: *const std::ffi::c_void) -> bool;
}

// CGEventType constants
const K_CG_EVENT_LEFT_MOUSE_DOWN: u32 = 1;
const K_CG_EVENT_LEFT_MOUSE_UP: u32 = 2;
const K_CG_EVENT_RIGHT_MOUSE_DOWN: u32 = 3;
const K_CG_EVENT_RIGHT_MOUSE_UP: u32 = 4;
const K_CG_EVENT_MOUSE_MOVED: u32 = 5;
const K_CG_EVENT_LEFT_MOUSE_DRAGGED: u32 = 6;
const K_CG_EVENT_RIGHT_MOUSE_DRAGGED: u32 = 7;
const K_CG_EVENT_OTHER_MOUSE_DOWN: u32 = 25;
const K_CG_EVENT_OTHER_MOUSE_UP: u32 = 26;
const K_CG_EVENT_OTHER_MOUSE_DRAGGED: u32 = 27;

// Mouse Buttons
const K_CG_MOUSE_BUTTON_LEFT: u32 = 0;
const K_CG_MOUSE_BUTTON_RIGHT: u32 = 1;
const K_CG_MOUSE_BUTTON_CENTER: u32 = 2;
const K_CG_MOUSE_BUTTON_4: u32 = 3;
const K_CG_MOUSE_BUTTON_5: u32 = 4;

// Mouse event field constants
const K_CG_MOUSE_EVENT_BUTTON_NUMBER: u32 = 3;

// Tap Location
const K_CG_HID_EVENT_TAP: u32 = 0;

// Scroll unit (line)
const K_CG_SCROLL_EVENT_UNIT_LINE: u32 = 1;

// CoreGraphics Modifier Flag Masks
pub const K_CG_EVENT_FLAG_MASK_ALPHA_SHIFT: u64 = 0x0001_0000; // Caps Lock
pub const K_CG_EVENT_FLAG_MASK_SHIFT: u64 = 0x0002_0000; // Shift
pub const K_CG_EVENT_FLAG_MASK_CONTROL: u64 = 0x0004_0000; // Control
pub const K_CG_EVENT_FLAG_MASK_ALTERNATE: u64 = 0x0008_0000; // Option / Alt
pub const K_CG_EVENT_FLAG_MASK_COMMAND: u64 = 0x0010_0000; // Command (Win)
pub const K_CG_EVENT_FLAG_MASK_NUMERIC_PAD: u64 = 0x0020_0000; // Numeric Pad
#[allow(dead_code)]
pub const K_CG_EVENT_FLAG_MASK_HELP: u64 = 0x0040_0000; // Help
#[allow(dead_code)]
pub const K_CG_EVENT_FLAG_MASK_SECONDARY_FN: u64 = 0x0080_0000; // Fn

/// Trạng thái của từng phím đang được giữ
#[derive(Clone, Copy, Debug)]
struct KeyState {
    first_pressed: Instant,
    last_event: Instant,
    is_modifier: bool,
}

fn current_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub struct MacInputHandler {
    current_x: AtomicI32,
    current_y: AtomicI32,
    left_down: AtomicBool,
    right_down: AtomicBool,
    middle_down: AtomicBool,
    button4_down: AtomicBool,
    button5_down: AtomicBool,
    scroll_accumulator: Mutex<(f64, f64)>, // (vertical dy, horizontal dx)
    rdp_w: Arc<AtomicU32>,
    rdp_h: Arc<AtomicU32>,
    mac_w: u16,
    mac_h: u16,
    pressed_keys: Arc<Mutex<HashMap<u16, KeyState>>>,
    last_global_input: Arc<AtomicU64>,
    shutdown_tx: watch::Sender<bool>,

    // Trạng thái cụ thể của từng phím bổ trợ (Modifiers)
    left_shift: Arc<AtomicBool>,
    right_shift: Arc<AtomicBool>,
    left_ctrl: Arc<AtomicBool>,
    right_ctrl: Arc<AtomicBool>,
    left_alt: Arc<AtomicBool>,
    right_alt: Arc<AtomicBool>,
    left_cmd: Arc<AtomicBool>,
    right_cmd: Arc<AtomicBool>,
    caps_lock: Arc<AtomicBool>,
}

impl MacInputHandler {
    pub fn new(rdp_w: Arc<AtomicU32>, rdp_h: Arc<AtomicU32>, mac_w: u16, mac_h: u16) -> Self {
        check_accessibility_permission();
        let cur_rdp_w = rdp_w.load(Ordering::Relaxed);
        let cur_rdp_h = rdp_h.load(Ordering::Relaxed);
        let scale_x = if cur_rdp_w > 0 {
            mac_w as f64 / cur_rdp_w as f64
        } else {
            1.0
        };
        let scale_y = if cur_rdp_h > 0 {
            mac_h as f64 / cur_rdp_h as f64
        } else {
            1.0
        };

        info!(
            "🖱️ Dynamic Coordinate Mapping: RDP ({}x{}) -> macOS ({}x{}) [scale: {:.2}x, {:.2}x]",
            cur_rdp_w, cur_rdp_h, mac_w, mac_h, scale_x, scale_y
        );

        let pressed_keys: Arc<Mutex<HashMap<u16, KeyState>>> = Arc::new(Mutex::new(HashMap::new()));
        let last_global_input = Arc::new(AtomicU64::new(current_timestamp_ms()));
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

        let left_shift = Arc::new(AtomicBool::new(false));
        let right_shift = Arc::new(AtomicBool::new(false));
        let left_ctrl = Arc::new(AtomicBool::new(false));
        let right_ctrl = Arc::new(AtomicBool::new(false));
        let left_alt = Arc::new(AtomicBool::new(false));
        let right_alt = Arc::new(AtomicBool::new(false));
        let left_cmd = Arc::new(AtomicBool::new(false));
        let right_cmd = Arc::new(AtomicBool::new(false));
        let caps_lock = Arc::new(AtomicBool::new(false));

        // Khởi động Watchdog chạy nền để tự động phát hiện và huỷ kẹt phím
        let keys_clone = Arc::clone(&pressed_keys);
        let input_clone = Arc::clone(&last_global_input);
        let l_shift_clone = Arc::clone(&left_shift);
        let r_shift_clone = Arc::clone(&right_shift);
        let l_ctrl_clone = Arc::clone(&left_ctrl);
        let r_ctrl_clone = Arc::clone(&right_ctrl);
        let l_alt_clone = Arc::clone(&left_alt);
        let r_alt_clone = Arc::clone(&right_alt);
        let l_cmd_clone = Arc::clone(&left_cmd);
        let r_cmd_clone = Arc::clone(&right_cmd);
        let caps_clone = Arc::clone(&caps_lock);

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(250));
            loop {
                tokio::select! {
                    _ = interval.tick() => {
                        let now = Instant::now();
                        let last_input_ms = input_clone.load(Ordering::Relaxed);
                        let ms_since_any_input = current_timestamp_ms().saturating_sub(last_input_ms);

                        let mut stuck_keys = Vec::new();
                        {
                            if let Ok(mut map) = keys_clone.lock() {
                                let mut to_remove = Vec::new();
                                for (&keycode, state) in map.iter() {
                                    let idle_duration = now.duration_since(state.last_event);
                                    let hold_duration = now.duration_since(state.first_pressed);

                                    if !state.is_modifier {
                                        // Phím thường (A-Z, 0-9, Arrows, Enter, ., Space...):
                                        // Khi người dùng giữ phím, RDP client luôn gửi repeat mỗi 30-50ms.
                                        // Nếu quá 1.5s không nhận thêm repeat hoặc release -> Phím đã bị kẹt do mất gói/mất focus!
                                        if idle_duration > Duration::from_millis(1500) {
                                            stuck_keys.push((keycode, state.is_modifier, hold_duration));
                                            to_remove.push(keycode);
                                        }
                                    } else {
                                        // Phím bổ trợ (Shift, Ctrl, Alt, Cmd/Win):
                                        // Tự động huỷ nếu toàn bộ phiên RDP không có bất kỳ thao tác nào trong > 5s
                                        // hoặc phím bổ trợ bị giữ đơn độc > 15s.
                                        if ms_since_any_input > 5000 || hold_duration > Duration::from_millis(15000) {
                                            stuck_keys.push((keycode, state.is_modifier, hold_duration));
                                            to_remove.push(keycode);

                                            // Reset modifier state
                                            match keycode {
                                                0x38 => l_shift_clone.store(false, Ordering::Relaxed),
                                                0x3C => r_shift_clone.store(false, Ordering::Relaxed),
                                                0x3B => l_ctrl_clone.store(false, Ordering::Relaxed),
                                                0x3E => r_ctrl_clone.store(false, Ordering::Relaxed),
                                                0x3A => l_alt_clone.store(false, Ordering::Relaxed),
                                                0x3D => r_alt_clone.store(false, Ordering::Relaxed),
                                                0x37 => l_cmd_clone.store(false, Ordering::Relaxed),
                                                0x36 => r_cmd_clone.store(false, Ordering::Relaxed),
                                                0x39 => caps_clone.store(false, Ordering::Relaxed),
                                                _ => {}
                                            }
                                        }
                                    }
                                }

                                for keycode in to_remove {
                                    map.remove(&keycode);
                                }
                            }
                        }

                        // Nhả các phím kẹt ở tầng CoreGraphics của macOS với flags sạch = 0
                        for (keycode, is_modifier, hold_duration) in stuck_keys {
                            if is_modifier {
                                warn!(
                                    "🛡️ [WATCHDOG AUTO-RELEASE] Phím bổ trợ '{}' (0x{:02X}) tự động nhả sau {:?} không hoạt động (Auto-released stuck modifier)",
                                    mac_key_name(keycode),
                                    keycode,
                                    hold_duration
                                );
                            } else {
                                warn!(
                                    "🛡️ [WATCHDOG AUTO-RELEASE] Phím '{}' (0x{:02X}) tự động nhả sau {:?} kẹt (Auto-released stuck normal key)",
                                    mac_key_name(keycode),
                                    keycode,
                                    hold_duration
                                );
                            }
                            Self::post_keyboard_event_raw(keycode, false, 0, None);
                        }
                    }
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            break;
                        }
                    }
                }
            }
        });

        info!(
            "🛡️ Key Stuck Watchdog & Auto-Release Protection: ACTIVATED (Auto-releases stuck keys after timeout)"
        );

        Self {
            current_x: AtomicI32::new(0),
            current_y: AtomicI32::new(0),
            left_down: AtomicBool::new(false),
            right_down: AtomicBool::new(false),
            middle_down: AtomicBool::new(false),
            button4_down: AtomicBool::new(false),
            button5_down: AtomicBool::new(false),
            scroll_accumulator: Mutex::new((0.0, 0.0)),
            rdp_w,
            rdp_h,
            mac_w,
            mac_h,
            pressed_keys,
            last_global_input,
            shutdown_tx,
            left_shift,
            right_shift,
            left_ctrl,
            right_ctrl,
            left_alt,
            right_alt,
            left_cmd,
            right_cmd,
            caps_lock,
        }
    }

    pub fn scale_x(&self) -> f64 {
        let cur_rdp_w = self.rdp_w.load(Ordering::Relaxed);
        if cur_rdp_w > 0 {
            self.mac_w as f64 / cur_rdp_w as f64
        } else {
            1.0
        }
    }

    pub fn scale_y(&self) -> f64 {
        let cur_rdp_h = self.rdp_h.load(Ordering::Relaxed);
        if cur_rdp_h > 0 {
            self.mac_h as f64 / cur_rdp_h as f64
        } else {
            1.0
        }
    }

    fn update_activity(&self) {
        self.last_global_input
            .store(current_timestamp_ms(), Ordering::Relaxed);
    }

    /// Tính toán chính xác cờ Modifier flags (Shift, Ctrl, Option, Cmd, CapsLock) hiện tại
    pub fn current_modifier_flags(&self) -> u64 {
        let mut flags: u64 = 0;
        if self.left_shift.load(Ordering::Relaxed) || self.right_shift.load(Ordering::Relaxed) {
            flags |= K_CG_EVENT_FLAG_MASK_SHIFT;
        }
        if self.left_ctrl.load(Ordering::Relaxed) || self.right_ctrl.load(Ordering::Relaxed) {
            flags |= K_CG_EVENT_FLAG_MASK_CONTROL;
        }
        if self.left_alt.load(Ordering::Relaxed) || self.right_alt.load(Ordering::Relaxed) {
            flags |= K_CG_EVENT_FLAG_MASK_ALTERNATE;
        }
        if self.left_cmd.load(Ordering::Relaxed) || self.right_cmd.load(Ordering::Relaxed) {
            flags |= K_CG_EVENT_FLAG_MASK_COMMAND;
        }
        if self.caps_lock.load(Ordering::Relaxed) {
            flags |= K_CG_EVENT_FLAG_MASK_ALPHA_SHIFT;
        }
        flags
    }

    pub fn post_mouse_event_raw(event_type: u32, button: u32, x: f64, y: f64) {
        unsafe {
            let point = CGPoint { x, y };
            let event = CGEventCreateMouseEvent(std::ptr::null(), event_type, point, button);
            if !event.is_null() {
                if button >= 3 {
                    CGEventSetIntegerValueField(
                        event,
                        K_CG_MOUSE_EVENT_BUTTON_NUMBER,
                        button as i64,
                    );
                }
                CGEventPost(K_CG_HID_EVENT_TAP, event);
                CFRelease(event);
            }
        }
    }

    fn post_mouse_event(&self, event_type: u32, button: u32, x: f64, y: f64) {
        self.update_activity();
        Self::post_mouse_event_raw(event_type, button, x, y);
    }

    fn post_scroll_event(&self, dy_lines: i32, dx_lines: i32) {
        self.update_activity();
        let flags = self.current_modifier_flags();
        unsafe {
            let event = CGEventCreateScrollWheelEvent2(
                std::ptr::null(),
                K_CG_SCROLL_EVENT_UNIT_LINE,
                2,
                dy_lines,
                dx_lines,
                0,
            );
            if !event.is_null() {
                CGEventSetFlags(event, flags);
                CGEventPost(K_CG_HID_EVENT_TAP, event);
                CFRelease(event);
            }
        }
    }

    fn scroll_accum(&self, dy_raw: f64, dx_raw: f64) {
        let (dy_lines, dx_lines) = {
            let mut accum = self.scroll_accumulator.lock().unwrap();
            accum.0 += dy_raw / 30.0;
            accum.1 += dx_raw / 30.0;

            let mut dy = accum.0.trunc() as i32;
            let mut dx = accum.1.trunc() as i32;

            if dy == 0 && dy_raw != 0.0 && accum.0.abs() >= 0.25 {
                dy = if accum.0 > 0.0 { 1 } else { -1 };
                accum.0 -= dy as f64;
            } else {
                accum.0 -= dy as f64;
            }

            if dx == 0 && dx_raw != 0.0 && accum.1.abs() >= 0.25 {
                dx = if accum.1 > 0.0 { 1 } else { -1 };
                accum.1 -= dx as f64;
            } else {
                accum.1 -= dx as f64;
            }

            (dy, dx)
        };

        if dy_lines != 0 || dx_lines != 0 {
            debug!(
                "📜 [SCROLL] dy: {}, dx: {} (raw dy: {}, dx: {})",
                dy_lines, dx_lines, dy_raw, dx_raw
            );
            self.post_scroll_event(dy_lines, dx_lines);
        }
    }

    /// Đẩy sự kiện bàn phím với flags chính xác và ký tự Unicode đính kèm
    pub fn post_keyboard_event_raw(
        keycode: u16,
        key_down: bool,
        flags: u64,
        unicode_char: Option<u16>,
    ) {
        unsafe {
            let event = CGEventCreateKeyboardEvent(std::ptr::null(), keycode, key_down);
            if !event.is_null() {
                // Ép cờ modifier chính xác, triệt tiêu toàn bộ ghost modifier flags kẹt trong hệ thống
                CGEventSetFlags(event, flags);

                // Đính kèm ký tự Unicode trực tiếp vào sự kiện để macOS luôn hiển thị chính xác ký tự
                if let Some(ch) = unicode_char {
                    CGEventKeyboardSetUnicodeString(event, 1, &ch);
                }

                CGEventPost(K_CG_HID_EVENT_TAP, event);
                CFRelease(event);
            }
        }
    }

    fn post_keyboard_event(&self, keycode: u16, key_down: bool) {
        self.update_activity();

        // 1. Cập nhật trạng thái từng phím Modifier
        match keycode {
            0x38 => self.left_shift.store(key_down, Ordering::Relaxed),
            0x3C => self.right_shift.store(key_down, Ordering::Relaxed),
            0x3B => self.left_ctrl.store(key_down, Ordering::Relaxed),
            0x3E => self.right_ctrl.store(key_down, Ordering::Relaxed),
            0x3A => self.left_alt.store(key_down, Ordering::Relaxed),
            0x3D => self.right_alt.store(key_down, Ordering::Relaxed),
            0x37 => self.left_cmd.store(key_down, Ordering::Relaxed),
            0x36 => self.right_cmd.store(key_down, Ordering::Relaxed),
            0x39 => {
                if key_down {
                    let prev = self.caps_lock.load(Ordering::Relaxed);
                    self.caps_lock.store(!prev, Ordering::Relaxed);
                }
            }
            _ => {}
        }

        // 2. Tính toán chính xác cờ Modifier
        let mut flags = self.current_modifier_flags();
        if is_numpad_key(keycode) {
            flags |= K_CG_EVENT_FLAG_MASK_NUMERIC_PAD;
        }

        // 3. Xác định xem có phím bổ trợ (Cmd, Ctrl, Alt) đang giữ không
        let has_cmd_ctrl_alt = self.left_ctrl.load(Ordering::Relaxed)
            || self.right_ctrl.load(Ordering::Relaxed)
            || self.left_alt.load(Ordering::Relaxed)
            || self.right_alt.load(Ordering::Relaxed)
            || self.left_cmd.load(Ordering::Relaxed)
            || self.right_cmd.load(Ordering::Relaxed);

        let is_shift =
            self.left_shift.load(Ordering::Relaxed) || self.right_shift.load(Ordering::Relaxed);

        // Chỉ đính kèm Unicode cho các dấu câu khi KHÔNG giữ Cmd/Ctrl/Alt để không làm hỏng phím tắt Alt/Cmd/Ctrl và bộ gõ tiếng Việt
        let unicode_char = get_unicode_override(keycode, is_shift, has_cmd_ctrl_alt);

        Self::post_keyboard_event_raw(keycode, key_down, flags, unicode_char);
    }

    /// Gửi ký tự Unicode trực tiếp: thực hiện cặp Down + Up liền nhau để đảm bảo không bị kẹt phím ảo
    fn post_unicode_char(&self, ch: u16) {
        self.update_activity();
        let flags = self.current_modifier_flags();
        unsafe {
            let event_down = CGEventCreateKeyboardEvent(std::ptr::null(), 0, true);
            if !event_down.is_null() {
                CGEventSetFlags(event_down, flags);
                CGEventKeyboardSetUnicodeString(event_down, 1, &ch);
                CGEventPost(K_CG_HID_EVENT_TAP, event_down);
                CFRelease(event_down);
            }
            let event_up = CGEventCreateKeyboardEvent(std::ptr::null(), 0, false);
            if !event_up.is_null() {
                CGEventSetFlags(event_up, flags);
                CGEventKeyboardSetUnicodeString(event_up, 1, &ch);
                CGEventPost(K_CG_HID_EVENT_TAP, event_up);
                CFRelease(event_up);
            }
        }
    }

    /// Nhả toàn bộ các phím đang được giữ
    pub fn release_all_keys(&self, reason: &str) {
        // Reset toàn bộ modifier state
        self.left_shift.store(false, Ordering::Relaxed);
        self.right_shift.store(false, Ordering::Relaxed);
        self.left_ctrl.store(false, Ordering::Relaxed);
        self.right_ctrl.store(false, Ordering::Relaxed);
        self.left_alt.store(false, Ordering::Relaxed);
        self.right_alt.store(false, Ordering::Relaxed);
        self.left_cmd.store(false, Ordering::Relaxed);
        self.right_cmd.store(false, Ordering::Relaxed);
        self.caps_lock.store(false, Ordering::Relaxed);

        if let Ok(mut map) = self.pressed_keys.lock() {
            if !map.is_empty() {
                info!(
                    "🧹 [RELEASE ALL KEYS] Nhả {} phím đang giữ ({})",
                    map.len(),
                    reason
                );
                for (&keycode, _) in map.iter() {
                    debug!(
                        "   - Releasing key '{}' (0x{:02X})",
                        mac_key_name(keycode),
                        keycode
                    );
                    Self::post_keyboard_event_raw(keycode, false, 0, None);
                }
                map.clear();
            }
        }
    }
}

impl Drop for MacInputHandler {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);

        // Nhả toàn bộ phím bàn phím khi phiên kết thúc
        self.release_all_keys("Session Disconnected / Dropped");

        // Nhả các nút chuột nếu đang nhấn
        let x = self.current_x.load(Ordering::Relaxed) as f64;
        let y = self.current_y.load(Ordering::Relaxed) as f64;
        if self.left_down.swap(false, Ordering::Relaxed) {
            info!("🧹 [SESSION CLEANUP] Tự động nhả chuột trái");
            Self::post_mouse_event_raw(K_CG_EVENT_LEFT_MOUSE_UP, K_CG_MOUSE_BUTTON_LEFT, x, y);
        }
        if self.right_down.swap(false, Ordering::Relaxed) {
            info!("🧹 [SESSION CLEANUP] Tự động nhả chuột phải");
            Self::post_mouse_event_raw(K_CG_EVENT_RIGHT_MOUSE_UP, K_CG_MOUSE_BUTTON_RIGHT, x, y);
        }
        if self.middle_down.swap(false, Ordering::Relaxed) {
            info!("🧹 [SESSION CLEANUP] Tự động nhả chuột giữa");
            Self::post_mouse_event_raw(K_CG_EVENT_OTHER_MOUSE_UP, K_CG_MOUSE_BUTTON_CENTER, x, y);
        }
        if self.button4_down.swap(false, Ordering::Relaxed) {
            info!("🧹 [SESSION CLEANUP] Tự động nhả nút chuột 4 (Back)");
            Self::post_mouse_event_raw(K_CG_EVENT_OTHER_MOUSE_UP, K_CG_MOUSE_BUTTON_4, x, y);
        }
        if self.button5_down.swap(false, Ordering::Relaxed) {
            info!("🧹 [SESSION CLEANUP] Tự động nhả nút chuột 5 (Forward)");
            Self::post_mouse_event_raw(K_CG_EVENT_OTHER_MOUSE_UP, K_CG_MOUSE_BUTTON_5, x, y);
        }
    }
}

impl RdpServerInputHandler for MacInputHandler {
    fn keyboard(&mut self, event: KeyboardEvent) {
        match event {
            KeyboardEvent::Pressed { code, extended } => {
                let mac_key = rdp_scancode_to_mac(code, extended);
                let is_mod = is_mac_modifier(mac_key);
                let now = Instant::now();

                {
                    if let Ok(mut map) = self.pressed_keys.lock() {
                        map.entry(mac_key)
                            .and_modify(|s| s.last_event = now)
                            .or_insert(KeyState {
                                first_pressed: now,
                                last_event: now,
                                is_modifier: is_mod,
                            });
                    }
                }

                debug!(
                    "⌨️ [KEY DOWN] '{}' (RDP: 0x{:02X}, Mac: 0x{:02X}, ext: {})",
                    mac_key_name(mac_key),
                    code,
                    mac_key,
                    extended
                );
                self.post_keyboard_event(mac_key, true);
            }
            KeyboardEvent::Released { code, extended } => {
                let mac_key = rdp_scancode_to_mac(code, extended);

                {
                    if let Ok(mut map) = self.pressed_keys.lock() {
                        map.remove(&mac_key);
                    }
                }

                debug!(
                    "⌨️ [KEY UP]   '{}' (RDP: 0x{:02X}, Mac: 0x{:02X}, ext: {})",
                    mac_key_name(mac_key),
                    code,
                    mac_key,
                    extended
                );
                self.post_keyboard_event(mac_key, false);
            }
            KeyboardEvent::UnicodePressed(ch) => {
                debug!(
                    "⌨️ [UNICODE] Pressed: {} ('{}')",
                    ch,
                    char::from_u32(ch as u32).unwrap_or('?')
                );
                self.post_unicode_char(ch);
            }
            KeyboardEvent::UnicodeReleased(ch) => {
                debug!(
                    "⌨️ [UNICODE] Released: {} ('{}')",
                    ch,
                    char::from_u32(ch as u32).unwrap_or('?')
                );
            }
            KeyboardEvent::Synchronize(flags) => {
                info!(
                    "🔄 [RDP SYNCHRONIZE] Nhận gói Synchronize (flags: {:?}) -> Nhả các phím kẹt",
                    flags
                );
                // Khi nhận Synchronize từ RDP Client (thường do chuyển focus/sync state), nhả toàn bộ phím thường
                if let Ok(mut map) = self.pressed_keys.lock() {
                    let to_release: Vec<u16> = map
                        .iter()
                        .filter(|(_, state)| !state.is_modifier)
                        .map(|(&k, _)| k)
                        .collect();
                    for keycode in to_release {
                        map.remove(&keycode);
                        info!(
                            "   - Sync-releasing key '{}' (0x{:02X})",
                            mac_key_name(keycode),
                            keycode
                        );
                        Self::post_keyboard_event_raw(keycode, false, 0, None);
                    }
                }
            }
        }
    }

    fn mouse(&mut self, event: MouseEvent) {
        match event {
            MouseEvent::Move { x, y } => {
                let mac_x = (x as f64) * self.scale_x();
                let mac_y = (y as f64) * self.scale_y();
                self.current_x.store(mac_x as i32, Ordering::Relaxed);
                self.current_y.store(mac_y as i32, Ordering::Relaxed);

                let is_left = self.left_down.load(Ordering::Relaxed);
                let is_right = self.right_down.load(Ordering::Relaxed);
                let is_middle = self.middle_down.load(Ordering::Relaxed);
                let is_b4 = self.button4_down.load(Ordering::Relaxed);
                let is_b5 = self.button5_down.load(Ordering::Relaxed);

                if is_left {
                    self.post_mouse_event(
                        K_CG_EVENT_LEFT_MOUSE_DRAGGED,
                        K_CG_MOUSE_BUTTON_LEFT,
                        mac_x,
                        mac_y,
                    );
                } else if is_right {
                    self.post_mouse_event(
                        K_CG_EVENT_RIGHT_MOUSE_DRAGGED,
                        K_CG_MOUSE_BUTTON_RIGHT,
                        mac_x,
                        mac_y,
                    );
                } else if is_middle {
                    self.post_mouse_event(
                        K_CG_EVENT_OTHER_MOUSE_DRAGGED,
                        K_CG_MOUSE_BUTTON_CENTER,
                        mac_x,
                        mac_y,
                    );
                } else if is_b4 {
                    self.post_mouse_event(
                        K_CG_EVENT_OTHER_MOUSE_DRAGGED,
                        K_CG_MOUSE_BUTTON_4,
                        mac_x,
                        mac_y,
                    );
                } else if is_b5 {
                    self.post_mouse_event(
                        K_CG_EVENT_OTHER_MOUSE_DRAGGED,
                        K_CG_MOUSE_BUTTON_5,
                        mac_x,
                        mac_y,
                    );
                } else {
                    self.post_mouse_event(
                        K_CG_EVENT_MOUSE_MOVED,
                        K_CG_MOUSE_BUTTON_LEFT,
                        mac_x,
                        mac_y,
                    );
                }
            }
            MouseEvent::LeftPressed => {
                self.left_down.store(true, Ordering::Relaxed);
                let x = self.current_x.load(Ordering::Relaxed) as f64;
                let y = self.current_y.load(Ordering::Relaxed) as f64;
                self.post_mouse_event(K_CG_EVENT_LEFT_MOUSE_DOWN, K_CG_MOUSE_BUTTON_LEFT, x, y);
            }
            MouseEvent::LeftReleased => {
                self.left_down.store(false, Ordering::Relaxed);
                let x = self.current_x.load(Ordering::Relaxed) as f64;
                let y = self.current_y.load(Ordering::Relaxed) as f64;
                self.post_mouse_event(K_CG_EVENT_LEFT_MOUSE_UP, K_CG_MOUSE_BUTTON_LEFT, x, y);
            }
            MouseEvent::RightPressed => {
                self.right_down.store(true, Ordering::Relaxed);
                let x = self.current_x.load(Ordering::Relaxed) as f64;
                let y = self.current_y.load(Ordering::Relaxed) as f64;
                self.post_mouse_event(K_CG_EVENT_RIGHT_MOUSE_DOWN, K_CG_MOUSE_BUTTON_RIGHT, x, y);
            }
            MouseEvent::RightReleased => {
                self.right_down.store(false, Ordering::Relaxed);
                let x = self.current_x.load(Ordering::Relaxed) as f64;
                let y = self.current_y.load(Ordering::Relaxed) as f64;
                self.post_mouse_event(K_CG_EVENT_RIGHT_MOUSE_UP, K_CG_MOUSE_BUTTON_RIGHT, x, y);
            }
            MouseEvent::MiddlePressed => {
                self.middle_down.store(true, Ordering::Relaxed);
                let x = self.current_x.load(Ordering::Relaxed) as f64;
                let y = self.current_y.load(Ordering::Relaxed) as f64;
                self.post_mouse_event(K_CG_EVENT_OTHER_MOUSE_DOWN, K_CG_MOUSE_BUTTON_CENTER, x, y);
            }
            MouseEvent::MiddleReleased => {
                self.middle_down.store(false, Ordering::Relaxed);
                let x = self.current_x.load(Ordering::Relaxed) as f64;
                let y = self.current_y.load(Ordering::Relaxed) as f64;
                self.post_mouse_event(K_CG_EVENT_OTHER_MOUSE_UP, K_CG_MOUSE_BUTTON_CENTER, x, y);
            }
            MouseEvent::Button4Pressed => {
                self.button4_down.store(true, Ordering::Relaxed);
                let x = self.current_x.load(Ordering::Relaxed) as f64;
                let y = self.current_y.load(Ordering::Relaxed) as f64;
                self.post_mouse_event(K_CG_EVENT_OTHER_MOUSE_DOWN, K_CG_MOUSE_BUTTON_4, x, y);
            }
            MouseEvent::Button4Released => {
                self.button4_down.store(false, Ordering::Relaxed);
                let x = self.current_x.load(Ordering::Relaxed) as f64;
                let y = self.current_y.load(Ordering::Relaxed) as f64;
                self.post_mouse_event(K_CG_EVENT_OTHER_MOUSE_UP, K_CG_MOUSE_BUTTON_4, x, y);
            }
            MouseEvent::Button5Pressed => {
                self.button5_down.store(true, Ordering::Relaxed);
                let x = self.current_x.load(Ordering::Relaxed) as f64;
                let y = self.current_y.load(Ordering::Relaxed) as f64;
                self.post_mouse_event(K_CG_EVENT_OTHER_MOUSE_DOWN, K_CG_MOUSE_BUTTON_5, x, y);
            }
            MouseEvent::Button5Released => {
                self.button5_down.store(false, Ordering::Relaxed);
                let x = self.current_x.load(Ordering::Relaxed) as f64;
                let y = self.current_y.load(Ordering::Relaxed) as f64;
                self.post_mouse_event(K_CG_EVENT_OTHER_MOUSE_UP, K_CG_MOUSE_BUTTON_5, x, y);
            }
            MouseEvent::VerticalScroll { value } => {
                self.scroll_accum(value as f64, 0.0);
            }
            MouseEvent::HorizontalScroll { value } => {
                self.scroll_accum(0.0, -(value as f64));
            }
            MouseEvent::Scroll { x, y } => {
                self.scroll_accum(y as f64, -(x as f64));
            }
            MouseEvent::RelMove { x, y } => {
                let cur_x =
                    self.current_x.load(Ordering::Relaxed) as f64 + (x as f64 * self.scale_x());
                let cur_y =
                    self.current_y.load(Ordering::Relaxed) as f64 + (y as f64 * self.scale_y());
                self.current_x.store(cur_x as i32, Ordering::Relaxed);
                self.current_y.store(cur_y as i32, Ordering::Relaxed);
                self.post_mouse_event(K_CG_EVENT_MOUSE_MOVED, K_CG_MOUSE_BUTTON_LEFT, cur_x, cur_y);
            }
        }
    }
}

/// Trả về ký tự Unicode đính kèm cho các phím dấu câu (Period, Comma, Slash, Bracket...) khi KHÔNG có phím bổ trợ (Cmd, Ctrl, Alt)
fn get_unicode_override(mac_key: u16, shift: bool, has_modifiers: bool) -> Option<u16> {
    // Nếu đang giữ phím bổ trợ (Cmd, Ctrl, Alt/Option), TUYỆT ĐỐI KHÔNG đính kèm chuỗi ký tự Unicode
    // để macOS xử lý phím tắt nguyên bản (Alt+C, Alt+B, Alt+F, Alt+Space, Cmd+C, Cmd+V, Ctrl+C...)
    if has_modifiers {
        return None;
    }

    // Chỉ đính kèm cho các dấu câu để không bao giờ bị lệch ký tự giữa các hệ điều hành/bàn phím
    // (Các chữ cái A-Z và số 0-9 để macOS xử lý tự nhiên bằng keycode nhằm hỗ trợ Bộ gõ tiếng Việt Telex/VNI)
    let ch = match (mac_key, shift) {
        // Dấu chấm .
        (0x2F, false) => '.',
        (0x2F, true) => '>',
        // Numpad .
        (0x41, _) => '.',
        // Dấu phẩy ,
        (0x2B, false) => ',',
        (0x2B, true) => '<',
        // Dấu gạch chéo /
        (0x2C, false) => '/',
        (0x2C, true) => '?',
        // Dấu chấm phẩy ;
        (0x29, false) => ';',
        (0x29, true) => ':',
        // Dấu nháy đơn '
        (0x27, false) => '\'',
        (0x27, true) => '"',
        // Dấu ngoặc vuông [ ]
        (0x21, false) => '[',
        (0x21, true) => '{',
        (0x1E, false) => ']',
        (0x1E, true) => '}',
        // Dấu gạch chéo ngược \
        (0x2A, false) => '\\',
        (0x2A, true) => '|',
        // Dấu trừ - và dấu bằng =
        (0x1B, false) => '-',
        (0x1B, true) => '_',
        (0x18, false) => '=',
        (0x18, true) => '+',
        // Dấu huyền `
        (0x32, false) => '`',
        (0x32, true) => '~',
        _ => return None,
    };
    Some(ch as u16)
}

/// Kiểm tra xem phím có thuộc cụm bàn phím số (Numpad) không
pub fn is_numpad_key(mac_key: u16) -> bool {
    matches!(
        mac_key,
        0x41 | 0x43
            | 0x45
            | 0x47
            | 0x4B
            | 0x4C
            | 0x4E
            | 0x51
            | 0x52
            | 0x53
            | 0x54
            | 0x55
            | 0x56
            | 0x57
            | 0x58
            | 0x59
            | 0x5B
            | 0x5C
    )
}

/// Kiểm tra xem keycode macOS có phải là phím bổ trợ (Modifier: Shift, Ctrl, Alt, Cmd, CapsLock) không
pub fn is_mac_modifier(mac_key: u16) -> bool {
    matches!(
        mac_key,
        0x37 | 0x36 | 0x3B | 0x3E | 0x38 | 0x3C | 0x3A | 0x3D | 0x39 | 0x3F
    )
}

/// Tên hiển thị thân thiện của phím macOS
pub fn mac_key_name(keycode: u16) -> &'static str {
    match keycode {
        0x00 => "A",
        0x01 => "S",
        0x02 => "D",
        0x03 => "F",
        0x04 => "H",
        0x05 => "G",
        0x06 => "Z",
        0x07 => "X",
        0x08 => "C",
        0x09 => "V",
        0x0B => "B",
        0x0C => "Q",
        0x0D => "W",
        0x0E => "E",
        0x0F => "R",
        0x10 => "Y",
        0x11 => "T",
        0x12 => "1",
        0x13 => "2",
        0x14 => "3",
        0x15 => "4",
        0x16 => "6",
        0x17 => "5",
        0x18 => "=",
        0x19 => "9",
        0x1A => "7",
        0x1B => "-",
        0x1C => "8",
        0x1D => "0",
        0x1E => "]",
        0x1F => "O",
        0x20 => "U",
        0x21 => "[",
        0x22 => "I",
        0x23 => "P",
        0x24 => "Enter",
        0x25 => "L",
        0x26 => "J",
        0x27 => "'",
        0x28 => "K",
        0x29 => ";",
        0x2A => "\\",
        0x2B => ",",
        0x2C => "/",
        0x2D => "N",
        0x2E => "M",
        0x2F => ".",
        0x30 => "Tab",
        0x31 => "Space",
        0x32 => "`",
        0x33 => "Backspace",
        0x35 => "Escape",
        0x36 => "Right Cmd",
        0x37 => "Left Cmd",
        0x38 => "Left Shift",
        0x39 => "Caps Lock",
        0x3A => "Left Option/Alt",
        0x3B => "Left Ctrl",
        0x3C => "Right Shift",
        0x3D => "Right Option/Alt",
        0x3E => "Right Ctrl",
        0x3F => "Fn",
        0x41 => "Numpad .",
        0x43 => "Numpad *",
        0x45 => "Numpad +",
        0x47 => "NumLock/Clear",
        0x4B => "Numpad /",
        0x4C => "Numpad Enter",
        0x4E => "Numpad -",
        0x52 => "Numpad 0",
        0x53 => "Numpad 1",
        0x54 => "Numpad 2",
        0x55 => "Numpad 3",
        0x56 => "Numpad 4",
        0x57 => "Numpad 5",
        0x58 => "Numpad 6",
        0x59 => "Numpad 7",
        0x5B => "Numpad 8",
        0x5C => "Numpad 9",
        0x60 => "F5",
        0x61 => "F6",
        0x62 => "F7",
        0x63 => "F3",
        0x64 => "F8",
        0x65 => "F9",
        0x67 => "F11",
        0x69 => "PrintScreen",
        0x6B => "ScrollLock",
        0x6D => "F10",
        0x6E => "Menu",
        0x6F => "F12",
        0x71 => "Pause",
        0x72 => "Insert/Help",
        0x73 => "Home",
        0x74 => "Page Up",
        0x75 => "Delete",
        0x76 => "F4",
        0x77 => "End",
        0x78 => "F2",
        0x79 => "Page Down",
        0x7A => "F1",
        0x7B => "Left Arrow",
        0x7C => "Right Arrow",
        0x7D => "Down Arrow",
        0x7E => "Up Arrow",
        _ => "Other/Unknown",
    }
}

/// Kiểm tra quyền Accessibility (Trợ năng) của macOS
pub fn check_accessibility_permission() -> bool {
    let trusted = unsafe { AXIsProcessTrustedWithOptions(std::ptr::null()) };
    if !trusted {
        error!(
            "⚠️ [PERMISSION REQUIRED] Cần cấp quyền Accessibility (Trợ năng) cho Terminal/App trong:"
        );
        error!("   System Settings -> Privacy & Security -> Accessibility");
    } else {
        info!("✅ Accessibility (Trợ năng) permission is GRANTED");
    }
    trusted
}

/// Bảng ánh xạ từ mã phím RDP (PS/2 Set 1) sang macOS Virtual KeyCode
fn rdp_scancode_to_mac(code: u8, extended: bool) -> u16 {
    if extended {
        return match code {
            0x48 => 0x7E, // Up Arrow
            0x50 => 0x7D, // Down Arrow
            0x4B => 0x7B, // Left Arrow
            0x4D => 0x7C, // Right Arrow
            0x52 => 0x72, // Insert / Help
            0x53 => 0x75, // Forward Delete
            0x47 => 0x73, // Home
            0x4F => 0x77, // End
            0x49 => 0x74, // Page Up
            0x51 => 0x79, // Page Down
            0x5B => 0x37, // Left Command (Win)
            0x5C => 0x36, // Right Command (Win)
            0x5D => 0x6E, // Menu / App
            0x1D => 0x3E, // Right Ctrl
            0x38 => 0x3D, // Right Option/Alt
            0x1C => 0x4C, // Numpad Enter
            0x35 => 0x4B, // Numpad /
            0x37 => 0x69, // PrintScreen
            0x46 => 0x71, // Pause/Break
            _ => code as u16,
        };
    }

    match code {
        0x01 => 0x35, // Escape
        0x02 => 0x12, // 1
        0x03 => 0x13, // 2
        0x04 => 0x14, // 3
        0x05 => 0x15, // 4
        0x06 => 0x17, // 5
        0x07 => 0x16, // 6
        0x08 => 0x1A, // 7
        0x09 => 0x1C, // 8
        0x0A => 0x19, // 9
        0x0B => 0x1D, // 0
        0x0C => 0x1B, // -
        0x0D => 0x18, // =
        0x0E => 0x33, // Backspace
        0x0F => 0x30, // Tab
        0x10 => 0x0C, // Q
        0x11 => 0x0D, // W
        0x12 => 0x0E, // E
        0x13 => 0x0F, // R
        0x14 => 0x11, // T
        0x15 => 0x10, // Y
        0x16 => 0x20, // U
        0x17 => 0x22, // I
        0x18 => 0x1F, // O
        0x19 => 0x23, // P
        0x1A => 0x21, // [
        0x1B => 0x1E, // ]
        0x1C => 0x24, // Enter
        0x1D => 0x3B, // Left Ctrl
        0x1E => 0x00, // A
        0x1F => 0x01, // S
        0x20 => 0x02, // D
        0x21 => 0x03, // F
        0x22 => 0x05, // G
        0x23 => 0x04, // H
        0x24 => 0x26, // J
        0x25 => 0x28, // K
        0x26 => 0x25, // L
        0x27 => 0x29, // ;
        0x28 => 0x27, // '
        0x29 => 0x32, // `
        0x2A => 0x38, // Left Shift
        0x2B => 0x2A, // \
        0x2C => 0x06, // Z
        0x2D => 0x07, // X
        0x2E => 0x08, // C
        0x2F => 0x09, // V
        0x30 => 0x0B, // B
        0x31 => 0x2D, // N
        0x32 => 0x2E, // M
        0x33 => 0x2B, // ,
        0x34 => 0x2F, // .
        0x35 => 0x2C, // /
        0x36 => 0x3C, // Right Shift
        0x37 => 0x43, // Numpad *
        0x38 => 0x3A, // Left Option / Alt
        0x39 => 0x31, // Space
        0x3A => 0x39, // CapsLock
        0x3B => 0x7A, // F1
        0x3C => 0x78, // F2
        0x3D => 0x63, // F3
        0x3E => 0x76, // F4
        0x3F => 0x60, // F5
        0x40 => 0x61, // F6
        0x41 => 0x62, // F7
        0x42 => 0x64, // F8
        0x43 => 0x65, // F9
        0x44 => 0x6D, // F10
        0x45 => 0x47, // NumLock
        0x46 => 0x6B, // ScrollLock
        0x47 => 0x59, // Numpad 7
        0x48 => 0x5B, // Numpad 8
        0x49 => 0x5C, // Numpad 9
        0x4A => 0x4E, // Numpad -
        0x4B => 0x56, // Numpad 4
        0x4C => 0x57, // Numpad 5
        0x4D => 0x58, // Numpad 6
        0x4E => 0x45, // Numpad +
        0x4F => 0x53, // Numpad 1
        0x50 => 0x54, // Numpad 2
        0x51 => 0x55, // Numpad 3
        0x52 => 0x52, // Numpad 0
        0x53 => 0x41, // Numpad .
        0x57 => 0x67, // F11
        0x58 => 0x6F, // F12
        _ => code as u16,
    }
}
