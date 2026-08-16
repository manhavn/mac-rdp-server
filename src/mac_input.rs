use ironrdp_server::{KeyboardEvent, MouseEvent, RdpServerInputHandler};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use tracing::{debug, error, info};

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
    ) -> *mut std::ffi::c_void;

    fn CGEventKeyboardSetUnicodeString(
        event: *mut std::ffi::c_void,
        stringLength: usize,
        unicodeString: *const u16,
    );

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

// Tap Location
const K_CG_HID_EVENT_TAP: u32 = 0;

// Scroll unit (line)
const K_CG_SCROLL_EVENT_UNIT_LINE: u32 = 1;

pub struct MacInputHandler {
    current_x: AtomicI32,
    current_y: AtomicI32,
    left_down: AtomicBool,
    right_down: AtomicBool,
    middle_down: AtomicBool,
    scale_x: f64,
    scale_y: f64,
}

impl MacInputHandler {
    pub fn new(rdp_w: u16, rdp_h: u16, mac_w: u16, mac_h: u16) -> Self {
        check_accessibility_permission();
        let scale_x = if rdp_w > 0 {
            mac_w as f64 / rdp_w as f64
        } else {
            1.0
        };
        let scale_y = if rdp_h > 0 {
            mac_h as f64 / rdp_h as f64
        } else {
            1.0
        };

        info!(
            "🖱️ Mouse Coordinate Mapping: RDP ({}x{}) -> macOS ({}x{}) [scale: {:.2}x, {:.2}x]",
            rdp_w, rdp_h, mac_w, mac_h, scale_x, scale_y
        );

        Self {
            current_x: AtomicI32::new(0),
            current_y: AtomicI32::new(0),
            left_down: AtomicBool::new(false),
            right_down: AtomicBool::new(false),
            middle_down: AtomicBool::new(false),
            scale_x,
            scale_y,
        }
    }

    fn post_mouse_event(&self, event_type: u32, button: u32, x: f64, y: f64) {
        unsafe {
            let point = CGPoint { x, y };
            let event = CGEventCreateMouseEvent(std::ptr::null(), event_type, point, button);
            if !event.is_null() {
                CGEventPost(K_CG_HID_EVENT_TAP, event);
                CFRelease(event);
            }
        }
    }

    fn post_scroll_event(&self, lines: i32) {
        unsafe {
            let event = CGEventCreateScrollWheelEvent2(
                std::ptr::null(),
                K_CG_SCROLL_EVENT_UNIT_LINE,
                1,
                lines,
                0,
            );
            if !event.is_null() {
                CGEventPost(K_CG_HID_EVENT_TAP, event);
                CFRelease(event);
            }
        }
    }

    fn post_keyboard_event(&self, keycode: u16, key_down: bool) {
        unsafe {
            let event = CGEventCreateKeyboardEvent(std::ptr::null(), keycode, key_down);
            if !event.is_null() {
                CGEventPost(K_CG_HID_EVENT_TAP, event);
                CFRelease(event);
            }
        }
    }

    fn post_unicode_char(&self, ch: u16, key_down: bool) {
        unsafe {
            let event = CGEventCreateKeyboardEvent(std::ptr::null(), 0, key_down);
            if !event.is_null() {
                CGEventKeyboardSetUnicodeString(event, 1, &ch);
                CGEventPost(K_CG_HID_EVENT_TAP, event);
                CFRelease(event);
            }
        }
    }
}

impl RdpServerInputHandler for MacInputHandler {
    fn keyboard(&mut self, event: KeyboardEvent) {
        match event {
            KeyboardEvent::Pressed { code, extended } => {
                let mac_key = rdp_scancode_to_mac(code, extended);
                debug!(
                    "Key Pressed: RDP code=0x{:02X} (ext: {}) -> Mac=0x{:02X}",
                    code, extended, mac_key
                );
                self.post_keyboard_event(mac_key, true);
            }
            KeyboardEvent::Released { code, extended } => {
                let mac_key = rdp_scancode_to_mac(code, extended);
                debug!(
                    "Key Released: RDP code=0x{:02X} (ext: {}) -> Mac=0x{:02X}",
                    code, extended, mac_key
                );
                self.post_keyboard_event(mac_key, false);
            }
            KeyboardEvent::UnicodePressed(ch) => {
                debug!("Unicode Pressed: {}", ch);
                self.post_unicode_char(ch, true);
            }
            KeyboardEvent::UnicodeReleased(ch) => {
                debug!("Unicode Released: {}", ch);
                self.post_unicode_char(ch, false);
            }
            KeyboardEvent::Synchronize(_) => {}
        }
    }

    fn mouse(&mut self, event: MouseEvent) {
        match event {
            MouseEvent::Move { x, y } => {
                let mac_x = (x as f64) * self.scale_x;
                let mac_y = (y as f64) * self.scale_y;
                self.current_x.store(mac_x as i32, Ordering::Relaxed);
                self.current_y.store(mac_y as i32, Ordering::Relaxed);

                let is_left = self.left_down.load(Ordering::Relaxed);
                let is_right = self.right_down.load(Ordering::Relaxed);
                let is_middle = self.middle_down.load(Ordering::Relaxed);

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
            MouseEvent::VerticalScroll { value } => {
                // RDP sends +/- 120 per wheel notch
                let lines = (value / 40) as i32;
                self.post_scroll_event(lines);
            }
            _ => {}
        }
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
            0x53 => 0x75, // Forward Delete
            0x47 => 0x73, // Home
            0x4F => 0x77, // End
            0x49 => 0x74, // Page Up
            0x51 => 0x79, // Page Down
            0x5B => 0x37, // Left Command (Win)
            0x5C => 0x36, // Right Command (Win)
            0x1D => 0x3E, // Right Ctrl
            0x38 => 0x3D, // Right Option/Alt
            0x1C => 0x4C, // Numpad Enter
            0x35 => 0x4B, // Numpad /
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
        0x57 => 0x67, // F11
        0x58 => 0x6F, // F12
        _ => code as u16,
    }
}
