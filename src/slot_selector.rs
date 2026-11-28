//! In-slot Stacked Card Deck Selector (`Win+Alt+Tab`).
//!
//! Renders directly inside the active slot's `Rect`:
//! - Up to 2 stacked card silhouettes behind the front card (deck feel).
//! - Elevated front card with accent border + crisp GPU-composited DWM Live Thumbnail.
//! - Header bar showing App name, Window title, optional `[Minimized]` tag, and counter (`2 / 7`).
//! - Never takes focus (`WS_EX_NOACTIVATE`); original window stays untouched until committed.

use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::Mutex;

use layout::Rect;
use windows::core::{w, BOOL};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmQueryThumbnailSourceSize, DwmRegisterThumbnail, DwmUnregisterThumbnail,
    DwmUpdateThumbnailProperties, DWM_THUMBNAIL_PROPERTIES, DWM_TNP_OPACITY,
    DWM_TNP_RECTDESTINATION, DWM_TNP_VISIBLE,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, InvalidateRect, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX,
    DT_SINGLELINE, DT_VCENTER, HFONT, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::config::OverviewColors;
use crate::consts::{WM_SEL_CANCEL, WM_SEL_COMMIT};
use crate::theme::{self, Canvas, Palette};

#[derive(Debug, Clone)]
pub struct SelectorCard {
    pub hwnd: isize,
    pub label: String,
    pub minimized: bool,
}

struct State {
    hwnd: isize,
    card: Option<SelectorCard>,
    index: usize,
    total: usize,
    thumb: isize,
    colors: OverviewColors,
}

static STATE: Mutex<State> = Mutex::new(State {
    hwnd: 0,
    card: None,
    index: 0,
    total: 0,
    thumb: 0,
    colors: OverviewColors {
        selected: 0,
        active: 0,
        view: 0,
        background: 0,
        box_fill: 0,
    },
});

static MAIN_THREAD: AtomicU32 = AtomicU32::new(0);
static FONT_TITLE: AtomicIsize = AtomicIsize::new(0);
static FONT_SUB: AtomicIsize = AtomicIsize::new(0);

unsafe fn ensure_fonts() -> (HFONT, HFONT) {
    let mut ft = FONT_TITLE.load(Ordering::Relaxed);
    if ft == 0 {
        let f = theme::create_font(-16, 600);
        FONT_TITLE.store(f.0 as isize, Ordering::Relaxed);
        ft = f.0 as isize;
    }
    let mut fs = FONT_SUB.load(Ordering::Relaxed);
    if fs == 0 {
        let f = theme::create_font(-13, 400);
        FONT_SUB.store(f.0 as isize, Ordering::Relaxed);
        fs = f.0 as isize;
    }
    (HFONT(ft as *mut c_void), HFONT(fs as *mut c_void))
}

/// Compute front card, header, thumbnail, and hint rects inside `(w, h)`.
fn card_geometry(w: i32, h: i32) -> (RECT, RECT, RECT, RECT) {
    let pad_x = (w / 14).clamp(18, 48);
    let pad_top = (h / 12).clamp(26, 56);
    let pad_bot = (h / 14).clamp(28, 48);
    let front = RECT {
        left: pad_x,
        top: pad_top,
        right: (w - pad_x).max(pad_x + 120),
        bottom: (h - pad_bot).max(pad_top + 100),
    };
    let header_h = 38;
    let header = RECT {
        left: front.left + 14,
        top: front.top + 8,
        right: front.right - 14,
        bottom: front.top + 8 + header_h,
    };
    let thumb_area = RECT {
        left: front.left + 12,
        top: header.bottom + 6,
        right: front.right - 12,
        bottom: (front.bottom - 12).max(header.bottom + 24),
    };
    let hint = RECT {
        left: pad_x,
        top: front.bottom + 6,
        right: w - pad_x,
        bottom: h - 4,
    };
    (front, header, thumb_area, hint)
}

fn fit_aspect(bounds: RECT, src: SIZE) -> RECT {
    let bw = (bounds.right - bounds.left).max(1) as f64;
    let bh = (bounds.bottom - bounds.top).max(1) as f64;
    if src.cx <= 0 || src.cy <= 0 {
        return bounds;
    }
    let scale = (bw / src.cx as f64).min(bh / src.cy as f64);
    let dw = ((src.cx as f64 * scale).round() as i32).max(1);
    let dh = ((src.cy as f64 * scale).round() as i32).max(1);
    let x = bounds.left + ((bounds.right - bounds.left) - dw) / 2;
    let y = bounds.top + ((bounds.bottom - bounds.top) - dh) / 2;
    RECT {
        left: x,
        top: y,
        right: x + dw,
        bottom: y + dh,
    }
}

unsafe fn post(msg: u32) {
    let tid = MAIN_THREAD.load(Ordering::Relaxed);
    if tid != 0 {
        let _ = PostThreadMessageW(tid, msg, WPARAM(0), LPARAM(0));
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_MOUSEACTIVATE => LRESULT(3), // MA_NOACTIVATE
        WM_LBUTTONUP => {
            post(WM_SEL_COMMIT);
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            post(WM_SEL_CANCEL);
            LRESULT(0)
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);

            let (card, index, total, colors) = {
                let g = STATE.lock().unwrap();
                (g.card.clone(), g.index, g.total, g.colors)
            };

            let pal = Palette::resolve(colors);
            let (font_title, font_sub) = ensure_fonts();
            let mut canvas = Canvas::new(hdc, rc);

            canvas.fill_rect(rc, pal.surface, 255);

            let (front, header, thumb_area, hint) = card_geometry(w, h);

            // Draw up to 2 stacked card silhouettes behind the front card.
            let stack_count = total.saturating_sub(1).min(2);
            let back2_fill = theme::mix(pal.surface, pal.card, 40);
            let back1_fill = theme::mix(pal.surface, pal.card, 70);
            if stack_count >= 2 {
                let back2 = RECT {
                    left: front.left + 22,
                    top: (front.top - 16).max(4),
                    right: front.right - 22,
                    bottom: front.bottom - 12,
                };
                canvas.fill_round_rect(back2, 12.0, back2_fill, 255);
                canvas.stroke_round_rect(back2, 12.0, pal.stroke, 150, 1.0);
            }
            if stack_count >= 1 {
                let back1 = RECT {
                    left: front.left + 11,
                    top: (front.top - 8).max(4),
                    right: front.right - 11,
                    bottom: front.bottom - 6,
                };
                canvas.fill_round_rect(back1, 12.0, back1_fill, 255);
                canvas.stroke_round_rect(back1, 12.0, pal.stroke, 200, 1.0);
            }

            // Front elevated card.
            canvas.fill_round_rect(front, 12.0, pal.card, 255);
            canvas.stroke_round_rect(front, 12.0, pal.accent, 255, 2.0);

            if let Some(c) = card {
                let display = if c.minimized {
                    format!("{}  [Minimized]", c.label)
                } else {
                    c.label
                };
                let title_rc = RECT {
                    left: header.left,
                    top: header.top,
                    right: (header.right - 76).max(header.left + 40),
                    bottom: header.bottom,
                };
                canvas.draw_text(
                    &display,
                    title_rc,
                    font_title,
                    pal.text,
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
                );

                // Counter pill (e.g. "2 / 7")
                let counter_rc = RECT {
                    left: (header.right - 68).max(header.left),
                    top: header.top + 4,
                    right: header.right,
                    bottom: header.bottom - 4,
                };
                let pill_fill = theme::mix(pal.card, pal.accent, 25);
                canvas.fill_round_rect(counter_rc, 8.0, pill_fill, 255);
                canvas.stroke_round_rect(counter_rc, 8.0, pal.accent, 180, 1.0);
                let counter_txt = format!("{} / {}", index + 1, total);
                canvas.draw_text(
                    &counter_txt,
                    counter_rc,
                    font_sub,
                    pal.text,
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                );

                if c.minimized {
                    canvas.draw_text(
                        "Minimized window — press Enter to restore into this slot",
                        thumb_area,
                        font_sub,
                        pal.text_muted,
                        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
                    );
                }
            } else {
                canvas.draw_text(
                    "No other windows in this workspace",
                    front,
                    font_title,
                    pal.text_muted,
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                );
            }

            canvas.draw_text(
                "Tab / Arrows cycle   ·   Enter or release Win/Alt to place   ·   Esc cancel",
                hint,
                font_sub,
                pal.text_muted,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
            );

            canvas.present();
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

pub unsafe fn init(main_thread: u32) -> bool {
    MAIN_THREAD.store(main_thread, Ordering::Relaxed);
    let Ok(hmod) = GetModuleHandleW(None) else {
        return false;
    };
    let hinst = HINSTANCE(hmod.0);
    let class = w!("NorenWmSlotSelector");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinst,
        lpszClassName: class,
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        ..Default::default()
    };
    if RegisterClassW(&wc) == 0 {
        return false;
    }

    // Omit WS_EX_LAYERED so DWM thumbnails stay sharp (same as snap_slots.rs).
    let Ok(hwnd) = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        class,
        w!("noren-wm slot selector"),
        WS_POPUP,
        0,
        0,
        100,
        100,
        None,
        None,
        Some(hinst),
        None,
    ) else {
        return false;
    };
    let pal = Palette::resolve(OverviewColors::default());
    theme::apply_window_chrome(hwnd, &pal, false);
    STATE.lock().unwrap().hwnd = hwnd.0 as isize;
    true
}

unsafe fn unregister_thumb(st: &mut State) {
    if st.thumb != 0 {
        let _ = DwmUnregisterThumbnail(st.thumb);
        st.thumb = 0;
    }
}

/// Show or update the Stacked Card Deck selector inside `slot_rect`.
pub unsafe fn show(
    slot_rect: Rect,
    card: Option<SelectorCard>,
    index: usize,
    total: usize,
    colors: OverviewColors,
    thumbnails: bool,
) {
    let mut st = STATE.lock().unwrap();
    if st.hwnd == 0 {
        return;
    }
    let hwnd = HWND(st.hwnd as *mut c_void);
    unregister_thumb(&mut st);

    let pal = Palette::resolve(colors);
    theme::apply_window_chrome(hwnd, &pal, false);

    st.card = card.clone();
    st.index = index;
    st.total = total;
    st.colors = colors;

    let w = slot_rect.w.max(180);
    let h = slot_rect.h.max(150);
    let _ = SetWindowPos(
        hwnd,
        Some(HWND_TOPMOST),
        slot_rect.x,
        slot_rect.y,
        w,
        h,
        SWP_NOACTIVATE | SWP_SHOWWINDOW,
    );

    if thumbnails {
        if let Some(c) = card {
            if !c.minimized && c.hwnd != 0 {
                let src = HWND(c.hwnd as *mut c_void);
                if let Ok(thumb) = DwmRegisterThumbnail(hwnd, src) {
                    let (_, _, thumb_area, _) = card_geometry(w, h);
                    let src_size = DwmQueryThumbnailSourceSize(thumb).unwrap_or_default();
                    let dst = fit_aspect(thumb_area, src_size);
                    let props = DWM_THUMBNAIL_PROPERTIES {
                        dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_VISIBLE | DWM_TNP_OPACITY,
                        rcDestination: dst,
                        opacity: 255,
                        fVisible: BOOL(1),
                        ..Default::default()
                    };
                    if DwmUpdateThumbnailProperties(thumb, &props).is_ok() {
                        st.thumb = thumb;
                    } else {
                        let _ = DwmUnregisterThumbnail(thumb);
                    }
                }
            }
        }
    }

    drop(st);
    let _ = InvalidateRect(Some(hwnd), None, false);
}

pub unsafe fn hide() {
    let mut st = STATE.lock().unwrap();
    if st.hwnd == 0 {
        return;
    }
    unregister_thumb(&mut st);
    st.card = None;
    let hwnd = HWND(st.hwnd as *mut c_void);
    drop(st);
    let _ = ShowWindow(hwnd, SW_HIDE);
}
