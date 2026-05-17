#![cfg(target_os = "windows")]

use std::sync::{mpsc, OnceLock};

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const BTN_MUTE: u32 = 100;
const BTN_DEAFEN: u32 = 101;
const BTN_LEAVE: u32 = 102;
const THBN_CLICKED: u16 = 0x1800;
const ICON_SIZE: i32 = 16;

#[derive(Debug, Clone, Copy)]
pub enum ThumbAction {
    MuteToggle,
    DeafenToggle,
    LeaveVoice,
}

static THUMB_TX: OnceLock<mpsc::Sender<ThumbAction>> = OnceLock::new();

pub struct TaskbarToolbar {
    taskbar: ITaskbarList3,
    hwnd: HWND,
    action_rx: mpsc::Receiver<ThumbAction>,
    initialized: bool,
    icon_mic: HICON,
    icon_mic_off: HICON,
    icon_headphones: HICON,
    icon_leave: HICON,
}

impl TaskbarToolbar {
    pub fn new() -> std::result::Result<Self, Box<dyn std::error::Error>> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok();
        }

        let taskbar: ITaskbarList3 =
            unsafe { CoCreateInstance(&TaskbarList, None, CLSCTX_INPROC_SERVER)? };
        unsafe { taskbar.HrInit()? };

        let (tx, rx) = mpsc::channel();
        THUMB_TX.set(tx).ok();

        let icon_mic = render_mic_icon(false);
        let icon_mic_off = render_mic_icon(true);
        let icon_headphones = render_headphones_icon();
        let icon_leave = render_leave_icon();

        Ok(Self {
            taskbar,
            hwnd: HWND::default(),
            action_rx: rx,
            initialized: false,
            icon_mic,
            icon_mic_off,
            icon_headphones,
            icon_leave,
        })
    }

    /// Find the main window and install buttons + subclass.
    /// No-ops if already initialized. Returns true on first success.
    pub fn try_init(&mut self) -> bool {
        if self.initialized {
            return false;
        }
        let hwnd = unsafe {
            match FindWindowW(None, w!("Mello")) {
                Ok(h) => h,
                Err(_) => return false,
            }
        };
        self.hwnd = hwnd;

        unsafe {
            let _ = SetWindowSubclass(hwnd, Some(subclass_proc), 1, 0);
        }

        let mut buttons = [
            THUMBBUTTON {
                dwMask: THB_ICON | THB_TOOLTIP | THB_FLAGS,
                iId: BTN_MUTE,
                hIcon: self.icon_mic,
                dwFlags: THBF_ENABLED,
                ..Default::default()
            },
            THUMBBUTTON {
                dwMask: THB_ICON | THB_TOOLTIP | THB_FLAGS,
                iId: BTN_DEAFEN,
                hIcon: self.icon_headphones,
                dwFlags: THBF_ENABLED,
                ..Default::default()
            },
            THUMBBUTTON {
                dwMask: THB_ICON | THB_TOOLTIP | THB_FLAGS,
                iId: BTN_LEAVE,
                hIcon: self.icon_leave,
                dwFlags: THBF_ENABLED,
                ..Default::default()
            },
        ];

        set_tip(&mut buttons[0].szTip, "Mute");
        set_tip(&mut buttons[1].szTip, "Deafen");
        set_tip(&mut buttons[2].szTip, "Leave Voice");

        match unsafe { self.taskbar.ThumbBarAddButtons(hwnd, &buttons) } {
            Ok(()) => {
                log::info!("[taskbar] thumbnail toolbar added");
                self.initialized = true;
                self.update_state(false, false, false);
                true
            }
            Err(e) => {
                log::warn!("[taskbar] ThumbBarAddButtons failed: {}", e);
                false
            }
        }
    }

    pub fn poll_action(&self) -> Option<ThumbAction> {
        self.action_rx.try_recv().ok()
    }

    pub fn update_state(&self, in_voice: bool, muted: bool, deafened: bool) {
        if !self.initialized {
            return;
        }

        let voice_flags = if in_voice {
            THBF_ENABLED
        } else {
            THBF_DISABLED | THBF_NOBACKGROUND
        };

        let mut buttons = [
            THUMBBUTTON {
                dwMask: THB_ICON | THB_FLAGS | THB_TOOLTIP,
                iId: BTN_MUTE,
                hIcon: if muted {
                    self.icon_mic_off
                } else {
                    self.icon_mic
                },
                dwFlags: voice_flags,
                ..Default::default()
            },
            THUMBBUTTON {
                dwMask: THB_ICON | THB_FLAGS | THB_TOOLTIP,
                iId: BTN_DEAFEN,
                hIcon: self.icon_headphones,
                dwFlags: voice_flags,
                ..Default::default()
            },
            THUMBBUTTON {
                dwMask: THB_FLAGS | THB_TOOLTIP,
                iId: BTN_LEAVE,
                dwFlags: voice_flags,
                ..Default::default()
            },
        ];

        set_tip(&mut buttons[0].szTip, if muted { "Unmute" } else { "Mute" });
        set_tip(
            &mut buttons[1].szTip,
            if deafened { "Undeafen" } else { "Deafen" },
        );
        set_tip(&mut buttons[2].szTip, "Leave Voice");

        unsafe {
            if let Err(e) = self.taskbar.ThumbBarUpdateButtons(self.hwnd, &buttons) {
                log::warn!("[taskbar] ThumbBarUpdateButtons failed: {}", e);
            }
        }
    }
}

fn set_tip(buf: &mut [u16; 260], text: &str) {
    buf.fill(0);
    for (i, ch) in text.encode_utf16().take(259).enumerate() {
        buf[i] = ch;
    }
}

unsafe extern "system" fn subclass_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _uid_subclass: usize,
    _ref_data: usize,
) -> LRESULT {
    if msg == WM_COMMAND {
        let hi = ((wparam.0 >> 16) & 0xFFFF) as u16;
        let lo = (wparam.0 & 0xFFFF) as u32;
        if hi == THBN_CLICKED {
            if let Some(tx) = THUMB_TX.get() {
                let action = match lo {
                    BTN_MUTE => Some(ThumbAction::MuteToggle),
                    BTN_DEAFEN => Some(ThumbAction::DeafenToggle),
                    BTN_LEAVE => Some(ThumbAction::LeaveVoice),
                    _ => None,
                };
                if let Some(a) = action {
                    let _ = tx.send(a);
                }
            }
        }
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

// ── Icon rendering ────────────────────────────────────────────────────────

fn create_icon_from_rgba(rgba: &[u8]) -> HICON {
    let size = ICON_SIZE;
    unsafe {
        let mask = CreateBitmap(size, size, 1, 1, None);

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let hdc = GetDC(None);
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let Ok(dib) = CreateDIBSection(Some(hdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            ReleaseDC(None, hdc);
            return HICON::default();
        };
        ReleaseDC(None, hdc);

        if !bits.is_null() {
            let dest = std::slice::from_raw_parts_mut(bits as *mut u8, rgba.len());
            for i in (0..rgba.len()).step_by(4) {
                let (r, g, b, a) = (rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]);
                let af = a as f32 / 255.0;
                dest[i] = (b as f32 * af) as u8;
                dest[i + 1] = (g as f32 * af) as u8;
                dest[i + 2] = (r as f32 * af) as u8;
                dest[i + 3] = a;
            }
        }

        let icon_info = ICONINFO {
            fIcon: TRUE,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: dib,
        };

        let icon = CreateIconIndirect(&icon_info).unwrap_or_default();
        let _ = DeleteObject(mask.into());
        let _ = DeleteObject(dib.into());
        icon
    }
}

fn put_pixel(buf: &mut [u8], x: i32, y: i32, r: u8, g: u8, b: u8, a: u8) {
    if x < 0 || y < 0 || x >= ICON_SIZE || y >= ICON_SIZE {
        return;
    }
    let i = ((y * ICON_SIZE + x) * 4) as usize;
    buf[i] = r;
    buf[i + 1] = g;
    buf[i + 2] = b;
    buf[i + 3] = a;
}

fn draw_rect(buf: &mut [u8], x: i32, y: i32, w: i32, h: i32, r: u8, g: u8, b: u8) {
    for py in y..y + h {
        for px in x..x + w {
            put_pixel(buf, px, py, r, g, b, 255);
        }
    }
}

fn draw_circle(buf: &mut [u8], cx: i32, cy: i32, radius: i32, r: u8, g: u8, b: u8) {
    for py in (cy - radius)..=(cy + radius) {
        for px in (cx - radius)..=(cx + radius) {
            let dx = px - cx;
            let dy = py - cy;
            if dx * dx + dy * dy <= radius * radius {
                put_pixel(buf, px, py, r, g, b, 255);
            }
        }
    }
}

fn render_mic_icon(muted: bool) -> HICON {
    let n = (ICON_SIZE * ICON_SIZE * 4) as usize;
    let mut buf = vec![0u8; n];

    let (cr, cg, cb) = if muted {
        (224, 32, 32)
    } else {
        (255, 255, 255)
    };

    // Mic head (rounded rectangle approximation)
    draw_rect(&mut buf, 6, 2, 4, 7, cr, cg, cb);
    draw_circle(&mut buf, 7, 3, 1, cr, cg, cb);
    draw_circle(&mut buf, 8, 3, 1, cr, cg, cb);

    // Stand arc (simplified as U shape)
    put_pixel(&mut buf, 4, 6, cr, cg, cb, 255);
    put_pixel(&mut buf, 4, 7, cr, cg, cb, 255);
    put_pixel(&mut buf, 4, 8, cr, cg, cb, 255);
    put_pixel(&mut buf, 11, 6, cr, cg, cb, 255);
    put_pixel(&mut buf, 11, 7, cr, cg, cb, 255);
    put_pixel(&mut buf, 11, 8, cr, cg, cb, 255);
    put_pixel(&mut buf, 5, 9, cr, cg, cb, 255);
    put_pixel(&mut buf, 10, 9, cr, cg, cb, 255);
    draw_rect(&mut buf, 6, 10, 4, 1, cr, cg, cb);

    // Stem
    draw_rect(&mut buf, 7, 10, 2, 2, cr, cg, cb);

    // Base
    draw_rect(&mut buf, 5, 12, 6, 1, cr, cg, cb);

    if muted {
        // Diagonal slash
        for i in 0..ICON_SIZE {
            put_pixel(&mut buf, i, i, 224, 32, 32, 255);
            put_pixel(&mut buf, i + 1, i, 224, 32, 32, 255);
        }
    }

    create_icon_from_rgba(&buf)
}

fn render_headphones_icon() -> HICON {
    let n = (ICON_SIZE * ICON_SIZE * 4) as usize;
    let mut buf = vec![0u8; n];
    let c = (255, 255, 255);

    // Headband arc (top half)
    for angle_deg in 0..=180 {
        let a = (angle_deg as f32).to_radians();
        let px = 8.0 + 5.0 * a.cos();
        let py = 6.0 - 5.0 * a.sin();
        put_pixel(&mut buf, px as i32, py as i32, c.0, c.1, c.2, 255);
    }

    // Left ear pad
    draw_rect(&mut buf, 2, 6, 3, 5, c.0, c.1, c.2);

    // Right ear pad
    draw_rect(&mut buf, 11, 6, 3, 5, c.0, c.1, c.2);

    create_icon_from_rgba(&buf)
}

fn render_leave_icon() -> HICON {
    let n = (ICON_SIZE * ICON_SIZE * 4) as usize;
    let mut buf = vec![0u8; n];
    let c = (224, 32, 32);

    // Phone handset rotated 135° (hangup icon) - simplified as diagonal bar
    // Earpiece
    draw_rect(&mut buf, 2, 4, 3, 3, c.0, c.1, c.2);
    // Mouthpiece
    draw_rect(&mut buf, 11, 9, 3, 3, c.0, c.1, c.2);
    // Handle connecting them (diagonal)
    for i in 0..8 {
        let x = 4 + i;
        let y = 6 + i / 2;
        put_pixel(&mut buf, x, y, c.0, c.1, c.2, 255);
        put_pixel(&mut buf, x, y + 1, c.0, c.1, c.2, 255);
    }

    create_icon_from_rgba(&buf)
}
