//! Unified Noren tray integration for `noren-wm` + session-end watcher
//! (`WM_QUERYENDSESSION`/`WM_ENDSESSION`) and autostart settings via HKCU\...\Run.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use noren_sdk::tray::{TrayConfig, TrayMenuItem, TrayModulePayload, TrayService};
use noren_sdk::NipNode;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
};
use windows::Win32::UI::WindowsAndMessaging::*;

/// Sent to the main thread: toggle the paused state.
pub const WM_TRAY_PAUSE: u32 = WM_APP + 5;
/// Sent to the main thread: reload the configuration file.
pub const WM_TRAY_RELOAD: u32 = WM_APP + 6;
/// Sent to the main thread: open / close the cheatsheet.
pub const WM_TRAY_HELP: u32 = WM_APP + 12;
/// Sent to the main thread on logoff / shutdown (`wParam`: 0 = query, 1 = ending, 2 = cancelled).
pub const WM_TRAY_SESSION_END: u32 = WM_APP + 27;
/// Sent to the main thread: exit cleanly and relaunch elevated via UAC (`runas`).
pub const WM_TRAY_ELEVATE: u32 = WM_APP + 28;
/// Sent to the main thread: open Noren Control Center focused on the `wm` tab.
pub const WM_TRAY_SETTINGS: u32 = WM_APP + 30;

/// True = the app is paused: windows are restored to their original positions
/// and all hotkeys are passed through to Windows as usual.
pub static PAUSED: AtomicBool = AtomicBool::new(false);

static MAIN_THREAD: AtomicU32 = AtomicU32::new(0);
static SESSION_HWND: AtomicIsize = AtomicIsize::new(0);
static SESSION_WAS_PAUSED: AtomicBool = AtomicBool::new(false);
static TRAY_SERVICE: Mutex<Option<TrayService>> = Mutex::new(None);

const RUN_SUBKEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const RUN_VALUE: PCWSTR = w!("noren-wm");

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

// ---------------------------------------------------------------- autostart

pub fn autostart_enabled() -> bool {
    unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_SUBKEY,
            RUN_VALUE,
            RRF_RT_REG_SZ,
            None,
            None,
            None,
        )
        .is_ok()
    }
}

pub fn set_autostart(on: bool) -> bool {
    unsafe {
        if on {
            let Ok(exe) = std::env::current_exe() else {
                return false;
            };
            let data = wide(&format!("\"{}\"", exe.display()));
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_SUBKEY,
                RUN_VALUE,
                REG_SZ.0,
                Some(data.as_ptr() as *const c_void),
                (data.len() * std::mem::size_of::<u16>()) as u32,
            )
            .is_ok()
        } else {
            RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_SUBKEY, RUN_VALUE).is_ok()
        }
    }
}

// ---------------------------------------------------------------- tray payload & SDK service

unsafe fn post_main(msg: u32) {
    let tid = MAIN_THREAD.load(Ordering::Relaxed);
    if tid != 0 {
        let _ = PostThreadMessageW(tid, msg, WPARAM(0), LPARAM(0));
    }
}

pub fn build_wm_tray_payload() -> TrayModulePayload {
    let paused = PAUSED.load(Ordering::Relaxed);
    let log_dir = crate::log::log_dir();
    let _ = std::fs::create_dir_all(&log_dir);

    let mut items = vec![
        TrayMenuItem {
            id: "pause".to_string(),
            label: if paused { "Resume" } else { "Pause" }.to_string(),
            checked: paused,
            enabled: true,
            separator_before: false,
            open_path: None,
        },
        TrayMenuItem {
            id: "reload".to_string(),
            label: "Reload configuration".to_string(),
            checked: false,
            enabled: true,
            separator_before: false,
            open_path: None,
        },
        TrayMenuItem {
            id: "cheatsheet".to_string(),
            label: "Keyboard shortcuts".to_string(),
            checked: false,
            enabled: true,
            separator_before: false,
            open_path: None,
        },
        TrayMenuItem {
            id: "logs".to_string(),
            label: "Open log folder".to_string(),
            checked: false,
            enabled: true,
            separator_before: false,
            open_path: Some(log_dir.to_string_lossy().into_owned()),
        },
    ];

    if !crate::log::is_elevated() {
        items.push(TrayMenuItem {
            id: "elevate".to_string(),
            label: "Restart as administrator".to_string(),
            checked: false,
            enabled: true,
            separator_before: false,
            open_path: None,
        });
    }

    items.push(TrayMenuItem {
        id: "autostart".to_string(),
        label: "Run at login".to_string(),
        checked: autostart_enabled(),
        enabled: true,
        separator_before: false,
        open_path: None,
    });

    TrayModulePayload {
        module: "wm".to_string(),
        title: "Window Manager".to_string(),
        paused,
        items,
    }
}

pub fn start_tray_service(main_thread: u32, node: Option<Arc<NipNode>>) {
    MAIN_THREAD.store(main_thread, Ordering::Relaxed);
    let cfg = TrayConfig {
        module: "wm".to_string(),
        tooltip: "noren-wm".to_string(),
        initial_payload: build_wm_tray_payload(),
        node,
        on_local_action: Arc::new(|id| match id {
            "pause" => unsafe { post_main(WM_TRAY_PAUSE) },
            "reload" => unsafe { post_main(WM_TRAY_RELOAD) },
            "cheatsheet" => unsafe { post_main(WM_TRAY_HELP) },
            "logs" => crate::log::open_log_folder(),
            "elevate" => unsafe { post_main(WM_TRAY_ELEVATE) },
            "autostart" => {
                let _ = set_autostart(!autostart_enabled());
                sync_state();
            }
            _ => {}
        }),
        on_local_quit: Arc::new(|| unsafe { post_main(crate::safety::WM_QUIT_REQUEST) }),
        on_open_settings: Some(Arc::new(|tab| {
            let _ = noren_sdk::control_center::open_control_center(tab.or(Some("wm")));
        })),
        headless: false,
    };

    match TrayService::start(cfg) {
        Ok(svc) => {
            *TRAY_SERVICE.lock().unwrap() = Some(svc);
        }
        Err(e) => {
            crate::log_warn!("failed to start unified tray service: {e}");
        }
    }
}

pub fn sync_state() {
    if let Some(svc) = TRAY_SERVICE.lock().unwrap().as_ref() {
        svc.update_local_payload(build_wm_tray_payload());
    }
}

pub fn notify_topology_changed() {
    if let Some(svc) = TRAY_SERVICE.lock().unwrap().as_ref() {
        svc.notify_topology_changed();
    }
}

pub fn request_yield(requester_elevated: bool) -> bool {
    TRAY_SERVICE
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|svc| svc.request_yield(requester_elevated))
}

pub unsafe fn set_tip(_text: &str) {
    sync_state();
}

pub unsafe fn remove() {
    if let Some(svc) = TRAY_SERVICE.lock().unwrap().take() {
        svc.shutdown();
    }
    let raw = SESSION_HWND.swap(0, Ordering::AcqRel);
    if raw != 0 {
        let _ = DestroyWindow(HWND(raw as *mut c_void));
    }
}

// ---------------------------------------------------------------- session watcher

unsafe extern "system" fn session_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_QUERYENDSESSION {
        // Pulihkan semua window secara sinkron sebelum app lain menyimpan posisinya.
        let was_paused = PAUSED.swap(true, Ordering::Relaxed);
        SESSION_WAS_PAUSED.store(was_paused, Ordering::Relaxed);
        crate::safety::restore_all();
        crate::taskbar::show_all();
        let _ = PostThreadMessageW(
            MAIN_THREAD.load(Ordering::Relaxed),
            WM_TRAY_SESSION_END,
            WPARAM(0),
            LPARAM(0),
        );
        return LRESULT(1); // TRUE: izinkan logoff / shutdown
    }
    if msg == WM_ENDSESSION {
        if wparam.0 != 0 {
            crate::safety::restore_all();
            crate::taskbar::shutdown();
            let _ = PostThreadMessageW(
                MAIN_THREAD.load(Ordering::Relaxed),
                WM_TRAY_SESSION_END,
                WPARAM(1),
                LPARAM(0),
            );
        } else if !SESSION_WAS_PAUSED.load(Ordering::Relaxed) {
            // Shutdown dibatalkan: aktifkan kembali tiling.
            PAUSED.store(false, Ordering::Relaxed);
            let _ = PostThreadMessageW(
                MAIN_THREAD.load(Ordering::Relaxed),
                WM_TRAY_SESSION_END,
                WPARAM(2),
                LPARAM(0),
            );
        }
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Create a lightweight hidden top-level window to receive `WM_QUERYENDSESSION` / `WM_ENDSESSION`.
pub unsafe fn create_session_watcher(main_thread: u32) -> bool {
    MAIN_THREAD.store(main_thread, Ordering::Relaxed);
    let Ok(hmod) = GetModuleHandleW(None) else {
        return false;
    };
    let hinst = HINSTANCE(hmod.0);
    let class = w!("NorenWmSessionWnd");

    let wc = WNDCLASSW {
        lpfnWndProc: Some(session_proc),
        hInstance: hinst,
        lpszClassName: class,
        ..Default::default()
    };
    if RegisterClassW(&wc) == 0 {
        return false;
    }
    let Ok(hwnd) = CreateWindowExW(
        WS_EX_TOOLWINDOW,
        class,
        w!("noren-wm"),
        WS_POPUP,
        0,
        0,
        0,
        0,
        None,
        None,
        Some(hinst),
        None,
    ) else {
        return false;
    };
    SESSION_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
    true
}
