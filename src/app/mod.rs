//! Application state: managed windows, synchronization, and arrangement.

mod actions;
mod anim;
mod events;
mod mouse;
mod overview_ctl;
mod persist;
mod pick;
mod selector;
mod snap;
mod workspace;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::time::Instant;

use layout::{Rect, WindowId, Workspaces};
use noren_sdk::protocol::{CAP_LAYOUT_RESERVED_AREA, CAP_TRAY_ACTIONS};
use noren_sdk::schema::FieldValue;
use noren_sdk::storage::{load_kv_file, save_kv_preserving_comments};
use noren_sdk::NipEvent;
use serde_json::{json, Value};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::Shell::ShellExecuteW;

#[allow(unused_imports)]
use windows::Win32::UI::Input::KeyboardAndMouse::*;
#[allow(unused_imports)]
use windows::Win32::UI::WindowsAndMessaging::*;

pub(crate) use self::actions::show_config_problems;
use self::anim::Anim;
use self::pick::PickSession;
use self::snap::{SnapAssist, SnapDrag};
use crate::consts::{DEFAULT_ANIM_MS, EVENT_DEBOUNCE_MS};
use crate::hook::OVERVIEW_VK;
use crate::winutil::{
    animations_enabled, bounds, collect, ext_bounds, is_hung, place, set_border, wid,
    work_area, Bounds,
};
use crate::{bindings, cheatsheet, config, nip, overview, rules, safety, taskbar};
use workspace::{slide_dir, Slide};

/// `join` targeting mode: hold the modifier to point `src` at `target` with the arrows,
/// then release the modifier to join them.
#[derive(Clone, Copy)]
pub(crate) struct JoinAim {
    pub(crate) src: WindowId,
    pub(crate) target: WindowId,
}

/// Currently open overview.
pub(crate) struct OverviewSession {
    /// Overview cursor (`None` when viewing an empty workspace).
    pub(crate) selected: Option<WindowId>,
    /// Foreground window before the overview opened, to restore on cancel.
    pub(crate) prev_fg: HWND,
    /// Data for rebuilding the panel when the center column shifts.
    pub(crate) model: overview::Model,
    pub(crate) titles: HashMap<u64, String>,
    pub(crate) sources: HashMap<u64, isize>,
    /// Window focused when the overview opened (0 = none), for highlighting.
    pub(crate) focused: u64,
}

pub(crate) struct App {
    /// All workspaces; the active one is arranged, the rest are parked off-screen.
    pub(crate) ws: Workspaces,
    pub(crate) map: HashMap<WindowId, HWND>,
    pub(crate) last_fg: Option<WindowId>,
    /// Frame bounds for each window after our last placement; if they change
    /// without us, the window was moved externally (Win+Arrow, drag, etc.).
    pub(crate) actual: HashMap<WindowId, Bounds>,
    pub(crate) anim: HashMap<WindowId, Anim>,
    /// Windows that still exist but are not currently visible (minimized, cloaked, etc.).
    /// Their slots remain in the strip.
    pub(crate) hidden: HashSet<WindowId>,
    /// Windows in other workspaces that are parked off-screen (taskbar buttons are
    /// hidden). apply() restores them when their workspace becomes active.
    pub(crate) parked: HashSet<WindowId>,
    /// Windows sliding off-screen to be parked; actually parked when their animation
    /// finishes (tick_anim). They are also always in `parked`.
    pub(crate) leaving: HashSet<WindowId>,
    /// Slide direction for the next apply() (switch / send workspace); used once.
    pub(crate) slide: Option<Slide>,
    /// Windows deliberately left floating (not tiled) via Win+Alt+F.
    pub(crate) floating: HashMap<WindowId, HWND>,
    /// Floating windows pinned always-on-top across all workspaces (Win+Alt+S).
    pub(crate) sticky: HashMap<WindowId, HWND>,
    /// Windows that refuse control (higher privileges). Leave them floating.
    pub(crate) blocked: HashMap<WindowId, HWND>,
    /// 0 = animation timer is stopped (no idle overhead).
    pub(crate) anim_timer: usize,
    pub(crate) anim_ms: f32,
    /// Focused window border setting, from config.
    pub(crate) focus_border: Option<config::BorderColor>,
    /// Window whose border is currently colored, so it can be restored.
    pub(crate) border_applied: Option<(HWND, u32)>,
    pub(crate) overview: Option<OverviewSession>,
    pub(crate) cfg: config::Config,
    /// One-shot timer to rearrange after an event (0 = none).
    pub(crate) sync_timer: usize,
    /// A new window appeared: schedule one recheck.
    pub(crate) recheck: bool,
    /// Layout last written to layout.state (persist.rs).
    pub(crate) saved_layout: Option<layout::SavedLayout>,
    /// Window identity (exe|class) for layout.state, computed once per window.
    pub(crate) idents: HashMap<WindowId, String>,
    /// Window drag tracked for the snap layouts menu (snap.rs).
    pub(crate) snap_drag: Option<SnapDrag>,
    /// 0 = drag-tracking timer is stopped.
    pub(crate) snap_timer: usize,
    /// Window-list session for empty preset zones.
    pub(crate) snap_assist: Option<SnapAssist>,
    /// `join` targeting mode (hold modifier + arrows, release to join).
    pub(crate) join_aim: Option<JoinAim>,
    /// Lift-and-drop (`pick`) mode with an overlay marker on the desktop.
    pub(crate) pick: Option<PickSession>,
    /// In-slot Stacked Card Deck selector (`Win+Grave`).
    pub(crate) selector: Option<selector::SelectorSession>,
    /// Reserved outer screen insets from `layout.reserved-area` providers (e.g. Bar).
    pub(crate) reserved: nip::ReservedAreaTracker,
    /// Optional NIP node runtime (None in tests or if pipe startup failed).
    pub(crate) nip: Option<nip::NipRuntime>,
}

impl App {
    pub(crate) unsafe fn new() -> Self {
        Self {
            ws: Workspaces::new(work_area()),
            map: HashMap::new(),
            last_fg: None,
            actual: HashMap::new(),
            anim: HashMap::new(),
            hidden: HashSet::new(),
            parked: HashSet::new(),
            leaving: HashSet::new(),
            slide: None,
            floating: HashMap::new(),
            sticky: HashMap::new(),
            blocked: HashMap::new(),
            anim_timer: 0,
            sync_timer: 0,
            recheck: false,
            saved_layout: None,
            idents: HashMap::new(),
            snap_drag: None,
            snap_timer: 0,
            snap_assist: None,
            join_aim: None,
            pick: None,
            selector: None,
            anim_ms: DEFAULT_ANIM_MS,
            focus_border: None,
            border_applied: None,
            overview: None,
            cfg: config::Config::default(),
            reserved: nip::ReservedAreaTracker::default(),
            nip: None,
        }
    }

    /// Windows work area minus any active `layout.reserved-area` outer insets.
    pub(crate) unsafe fn effective_work_area(&self) -> Rect {
        self.reserved.effective().apply_to(work_area())
    }

    /// JSON snapshot for the `workspace.state` capability.
    pub(crate) fn workspace_state_json(&self) -> Value {
        let active = self.ws.active();
        let count = self.ws.count();
        let paused = crate::tray::PAUSED.load(Ordering::Relaxed);
        let elevated = crate::log::is_elevated();
        let workspaces: Vec<Value> = (0..count)
            .map(|i| {
                json!({
                    "index": i,
                    "number": i + 1,
                    "active": i == active,
                    "windows": self.ws.window_ids(i).len(),
                })
            })
            .collect();
        json!({
            "active": active,
            "count": count,
            "paused": paused,
            "elevated": elevated,
            "workspaces": workspaces,
        })
    }

    pub(crate) fn publish_workspace_state(&mut self) {
        let state = self.workspace_state_json();
        if let Some(nip) = self.nip.as_mut() {
            nip.publish_workspace_state(state);
        }
    }

    /// Sync strip state with the current windows. True if anything changed.
    pub(crate) unsafe fn sync(&mut self) -> bool {
        let mut list: Vec<HWND> = Vec::new();
        let _ = EnumWindows(Some(collect), LPARAM(&mut list as *mut Vec<HWND> as isize));
        let live: HashMap<WindowId, HWND> = list.iter().map(|&h| (wid(h), h)).collect();
        let mut changed = false;
        self.floating.retain(|_, h| safety::alive(*h)); // Closed windows no longer need tracking.
        self.sticky.retain(|_, h| safety::alive(*h));
        self.blocked.retain(|_, h| safety::alive(*h));

        let gone: Vec<WindowId> = self
            .map
            .keys()
            .filter(|k| !live.contains_key(k))
            .copied()
            .collect();
        for id in gone {
            let h = self.map[&id];
            if safety::alive(h) {
                // Minimized / hidden / moved to a virtual desktop:
                // the window still exists, so retain its slot in the strip.
                if self.hidden.insert(id) {
                    self.ws.set_hidden(id, true);
                    self.anim.remove(&id);
                    changed = true;
                }
                continue;
            }
            // Actually closed.
            self.ws.remove_window(id);
            self.map.remove(&id);
            self.hidden.remove(&id);
            self.parked.remove(&id);
            self.leaving.remove(&id);
            taskbar::forget(h);
            safety::forget(h);
            self.actual.remove(&id);
            self.anim.remove(&id);
            changed = true;
        }
        let mut added = false;
        let work = self.effective_work_area();
        for &h in &list {
            let id = wid(h);
            if self.floating.contains_key(&id) || self.blocked.contains_key(&id) {
                continue;
            }
            if !self.map.contains_key(&id) {
                if let Some(rule) = rules::match_rule(&self.cfg.rules, h) {
                    match rule.action {
                        rules::RuleAction::Float {
                            width,
                            height,
                            sticky,
                        } => {
                            if width.is_some() || height.is_some() {
                                let cur = ext_bounds(h);
                                let w = width.unwrap_or(cur.right - cur.left).min(work.w);
                                let hh = height.unwrap_or(cur.bottom - cur.top).min(work.h);
                                let target = layout::Rect {
                                    x: work.x + (work.w - w) / 2,
                                    y: work.y + (work.h - hh) / 2,
                                    w,
                                    h: hh,
                                };
                                let _ = place(h, target);
                            }
                            if sticky {
                                let _ = SetWindowPos(
                                    h,
                                    Some(HWND_TOPMOST),
                                    0,
                                    0,
                                    0,
                                    0,
                                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                                );
                                self.sticky.insert(id, h);
                            }
                            self.floating.insert(id, h);
                            continue;
                        }
                        rules::RuleAction::Tile {
                            workspace,
                            span,
                            height,
                        } => {
                            safety::remember(h, work);
                            self.map.insert(id, h);
                            self.ws.add_window_with(id, workspace, span, height);
                            added = true;
                            continue;
                        }
                    }
                }
                safety::remember(h, work); // Original position, to restore on exit.
                self.map.insert(id, h);
                self.ws.add_window(id); // Add to the active workspace.
                added = true;
            } else if self.hidden.remove(&id) {
                // Reappeared (e.g. after Win+D): return it to its original slot.
                self.parked.remove(&id); // If in another workspace, apply() parks it again.
                self.ws.set_hidden(id, false);
                added = true;
            }
        }
        changed |= added;
        if added {
            self.recheck = true;
        }

        // If a window being targeted in `join` or `pick` disappears, cancel the mode.
        if let Some(aim) = self.join_aim {
            if !self.map.contains_key(&aim.src) || !self.map.contains_key(&aim.target) {
                self.join_aim = None;
                crate::hook::JOIN_ACTIVE.store(false, Ordering::Relaxed);
            }
        }
        if let Some(ref p) = self.pick {
            if !self.map.contains_key(&p.src) {
                self.pick = None;
                crate::hook::PICK_OPEN.store(false, Ordering::Relaxed);
                crate::pick_overlay::hide();
            }
        }
        // Follow the window focused by the user (click, Alt+Tab, taskbar button).
        // A window in another workspace switches us to that workspace. Ignore
        // a stale foreground window that has just been parked.
        let fg_hwnd = GetForegroundWindow();
        self.snap_assist_follow_focus(fg_hwnd);
        let fg = wid(fg_hwnd);
        if self.join_aim.is_none()
            && self.pick.is_none()
            && self.selector.is_none()
            && ((added && !self.parked.contains(&fg)) || self.last_fg != Some(fg))
        {
            let prev_fg = self.last_fg;
            self.last_fg = Some(fg);
            if let Some(prev) = prev_fg {
                self.auto_minimize_float(prev, fg);
            }
            if self.map.contains_key(&fg) && self.ws.cur().focused() != Some(fg) {
                let old = self.ws.active();
                if self.ws.focus_window(fg) {
                    self.slide = Some(Slide::switching(slide_dir(old, self.ws.active())));
                    self.update_workspace_tip();
                }
                changed = true;
            }
        }
        // Foreground can change without changing the layout (e.g. after Win+Shift+Arrow).
        self.refresh_border();
        changed || self.drifted()
    }

    /// Jika `float_auto_minimize` aktif dan fokus berpindah dari window float (non-sticky)
    /// ke window lain yang kita kelola, otomatis minimize window float itu agar tidak
    /// menutupi tile.
    pub(crate) unsafe fn auto_minimize_float(&self, prev: WindowId, fg: WindowId) {
        if !self.cfg.float_auto_minimize || prev == fg {
            return;
        }
        if self.sticky.contains_key(&prev) {
            return;
        }
        let Some(&ph) = self.floating.get(&prev) else {
            return;
        };
        if !(self.map.contains_key(&fg) || self.floating.contains_key(&fg)) {
            return; // fokus ke dialog anak / menu / Alt+Tab: jangan minimize
        }
        if safety::alive(ph) && !IsIconic(ph).as_bool() {
            let _ = ShowWindow(ph, SW_MINIMIZE);
        }
    }

    /// True if any window's position does not match our last placement.
    /// Ignored while the left mouse button is down (user is dragging).
    pub(crate) unsafe fn drifted(&self) -> bool {
        if !self.anim.is_empty() {
            return false; // Position is moving due to our animation.
        }
        if GetAsyncKeyState(VK_LBUTTON.0 as i32) < 0 {
            return false;
        }
        self.map
            .iter()
            .filter(|(id, _)| !self.hidden.contains(id))
            .any(|(id, &h)| self.actual.get(id) != Some(&bounds(ext_bounds(h))))
    }

    pub(crate) unsafe fn apply(&mut self) {
        let work = self.effective_work_area();
        self.ws.set_viewport(work);
        let now = Instant::now();
        let slide = self.slide.take(); // Workspace-switch slide direction; use once.
        // Run a second pass only if a window refused to shrink
        // (minimum height), requiring the layout to be recalculated.
        for _ in 0..2 {
            let mut grew = false;
            for p in self.ws.cur().layout() {
                let Some(&h) = self.map.get(&p.id) else {
                    continue;
                };
                if is_hung(h) {
                    continue; // jangan biarkan app yang hang membekukan seluruh WM
                }
                let t = p.rect;
                // Just unparked: start off-screen on the entering side, then slide in
                // (do not animate from -32768). If it is already sliding out (switching
                // back mid-animation), continue to its new position.
                let was_parked = self.parked.remove(&p.id);
                let was_leaving = self.leaving.remove(&p.id);
                if was_parked {
                    taskbar::show(h);
                }
                let at_park = was_parked && !was_leaving;
                let cur = ext_bounds(h);
                let same_size = cur.right - cur.left == t.w
                    && cur.bottom - cur.top == t.h
                    && !IsZoomed(h).as_bool();
                if same_size && !at_park {
                    // Only the position changed -> animation is allowed.
                    self.move_only(p.id, h, (cur.left, cur.top), t, work, now);
                    continue;
                }
                if same_size && self.anim_ms > 0.0 {
                    if let Some(s) = slide {
                        self.slide_from(p.id, h, t, (s.enter_from * work.w, 0), work, now);
                        continue;
                    }
                }
                // Size changed -> go directly to the final size, without animation.
                self.anim.remove(&p.id);
                let (a, denied) = place(h, t);
                if denied {
                    self.mark_blocked(p.id, h);
                    grew = true; // Recalculate the layout without this window.
                    continue;
                }
                let got = a.bottom - a.top;
                let got_w = a.right - a.left;
                self.actual.insert(p.id, bounds(a));
                if got > t.h + 2 || got_w > t.w + 2 {
                    // Sebagian app (Electron/UWP) menerapkan ukuran dengan jeda: jadwalkan
                    // ukur ulang lewat timer tanpa memblokir thread utama dengan sleep.
                    self.recheck = true;
                    self.schedule_sync(EVENT_DEBOUNCE_MS);
                }
                if got > t.h + 2 && self.ws.cur_mut().set_min_height(p.id, got) {
                    let ident = self
                        .idents
                        .entry(p.id)
                        .or_insert_with(|| persist::identity(h));
                    crate::log_warn!(
                        "[min-height] {ident} refused height below {got} px (requested {})",
                        t.h
                    );
                    grew = true;
                }
                if got_w > t.w + 2 && self.ws.cur_mut().set_min_width(p.id, got_w) {
                    let ident = self
                        .idents
                        .entry(p.id)
                        .or_insert_with(|| persist::identity(h));
                    crate::log_warn!(
                        "[min-width] {ident} refused width below {got_w} px (requested {})",
                        t.w
                    );
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }
        // Minimized window in the active workspace that is not tiled but remains parked.
        for id in self.ws.window_ids(self.ws.active()) {
            self.leaving.remove(&id);
            if self.parked.remove(&id) {
                if let Some(&h) = self.map.get(&id) {
                    taskbar::show(h);
                }
            }
        }
        self.park_inactive(slide, work, now);
        // Full-width window position-swap cue (see Strip::take_move_cue).
        if let Some((id, dir)) = self.ws.cur_mut().take_move_cue() {
            self.cue_slide(id, dir, work, now);
        }
        if !self.anim.is_empty() && self.anim_timer == 0 {
            // 10 ms = Windows minimum; in practice it ticks every ~15.6 ms (~64 fps).
            self.anim_timer = SetTimer(None, 0, 10, None);
        }
        self.refresh_border();
        self.publish_workspace_state();
    }

    /// Color the window CURRENTLY in the foreground (if tiled and focus_border
    /// is configured), and restore the previous window's border.
    ///
    /// Deliberately use the actual foreground, not the strip's focus target: a
    /// color applied before activation finishes can be lost during activation.
    pub(crate) unsafe fn refresh_border(&mut self) {
        let want = if let Some(aim) = self.join_aim {
            let color = self
                .cfg
                .join_border
                .or(self.focus_border)
                .map(crate::theme::resolve_border_color)
                .unwrap_or_else(|| crate::theme::Palette::resolve(self.cfg.overview_colors).accent);
            self.map.get(&aim.target).copied().map(|h| (h, color))
        } else {
            match self.focus_border {
                Some(bc) => {
                    let c = crate::theme::resolve_border_color(bc);
                    let fg = GetForegroundWindow();
                    let id = wid(fg);
                    (self.map.contains_key(&id) || self.floating.contains_key(&id))
                        .then_some((fg, c))
                }
                None => None,
            }
        };
        if want == self.border_applied {
            return;
        }
        if let Some((old, _)) = self.border_applied.take() {
            if want.map_or(true, |(h, _)| h != old) && safety::alive(old) {
                set_border(old, safety::DWM_COLOR_DEFAULT);
            }
        }
        if let Some((h, c)) = want {
            set_border(h, c);
            self.border_applied = Some((h, c));
        }
    }

    /// Apply config (gap, animation, border color).
    pub(crate) unsafe fn apply_config(&mut self, cfg: &config::Config) {
        self.cfg = cfg.clone();
        bindings::install(&cfg.bindings);
        OVERVIEW_VK.store(cfg.bindings.overview_cycle_vk(), Ordering::Relaxed);
        self.ws.set_gap(cfg.gap);
        self.focus_border = cfg.focus_border;
        self.refresh_border();
        self.anim_ms = cfg.anim_ms.unwrap_or(if animations_enabled() {
            DEFAULT_ANIM_MS
        } else {
            0.0
        });
    }

    /// Reload the config file and `noren-wm.ignore` without restart.
    pub(crate) unsafe fn reload_config(&mut self) {
        self.snap_reset(false); // Preset indices in the menu / slots may change.
        rules::reload();
        let report = config::load();
        self.apply_config(&report.cfg);
        let work = self.effective_work_area();
        let now_ignored: Vec<(WindowId, HWND, Option<i32>, Option<i32>, bool)> = self
            .map
            .iter()
            .filter_map(|(&id, &h)| {
                if rules::ignored(h).is_some() {
                    return Some((id, h, None, None, false));
                }
                if let Some(rule) = rules::match_rule(&self.cfg.rules, h) {
                    if let rules::RuleAction::Float {
                        width,
                        height,
                        sticky,
                    } = rule.action
                    {
                        return Some((id, h, width, height, sticky));
                    }
                }
                None
            })
            .collect();
        for (id, h, width, height, sticky) in now_ignored {
            self.ws.remove_window(id);
            self.map.remove(&id);
            self.hidden.remove(&id);
            self.actual.remove(&id);
            self.anim.remove(&id);
            let was_parked = self.parked.remove(&id);
            self.leaving.remove(&id);
            if was_parked {
                taskbar::show(h);
            }
            if width.is_some() || height.is_some() || was_parked {
                let cur = ext_bounds(h);
                let w = width
                    .unwrap_or((cur.right - cur.left).max(400))
                    .min(work.w);
                let hh = height
                    .unwrap_or((cur.bottom - cur.top).max(300))
                    .min(work.h);
                let _ = place(
                    h,
                    layout::Rect {
                        x: work.x + (work.w - w) / 2,
                        y: work.y + (work.h - hh) / 2,
                        w,
                        h: hh,
                    },
                );
            }
            if sticky {
                let _ = SetWindowPos(
                    h,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
                self.sticky.insert(id, h);
            }
            safety::forget(h);
            self.floating.insert(id, h);
        }
        self.sync();
        self.apply();
        actions::show_config_problems(&report);
    }

    /// Process all queued NIP events on the WM main thread.
    pub(crate) unsafe fn handle_nip_events(&mut self) {
        let Some(events) = self.nip.as_ref().map(|n| n.drain_events()) else {
            return;
        };
        let paused = crate::tray::PAUSED.load(Ordering::Relaxed);
        let mut need_reapply = false;

        for ev in events {
            match ev {
                NipEvent::PeerConnected(_) => {
                    self.publish_workspace_state();
                    crate::tray::notify_topology_changed();
                }
                NipEvent::PeerDisconnected { module, .. } => {
                    if self.reserved.remove_peer(&module) {
                        need_reapply = true;
                    }
                    crate::tray::notify_topology_changed();
                }
                NipEvent::StateReceived {
                    from, cap, data, ..
                } => {
                    if cap == CAP_LAYOUT_RESERVED_AREA {
                        let area = nip::ReservedArea::from_json(&data);
                        if self.reserved.set_peer(&from, area) {
                            need_reapply = true;
                        }
                    } else if cap == CAP_TRAY_ACTIONS {
                        crate::tray::notify_topology_changed();
                    }
                }
                NipEvent::ThemeChanged(theme) => {
                    crate::theme::set_noren_theme(theme);
                    self.refresh_border();
                }
                NipEvent::Command {
                    from,
                    name,
                    data,
                    reply,
                    ..
                } => {
                    self.handle_nip_command(&from, &name, data, reply);
                }
                NipEvent::EventReceived { name, .. } => {
                    if name == "tray.released" {
                        crate::tray::notify_topology_changed();
                    }
                }
            }
        }

        if need_reapply && !paused {
            self.apply();
        }
    }

    unsafe fn handle_nip_command(
        &mut self,
        from: &str,
        name: &str,
        data: Value,
        reply: noren_sdk::ReplyHandle,
    ) {
        match name {
            "ping" => {
                let _ = reply.ok(json!({
                    "ok": true,
                    "module": "wm",
                    "elevated": crate::log::is_elevated(),
                }));
            }
            "tray.invoke" => {
                let id = data.get("id").and_then(|v| v.as_str()).unwrap_or("");
                match id {
                    "pause" => {
                        self.toggle_pause();
                        let _ = reply.ok(json!({
                            "paused": crate::tray::PAUSED.load(Ordering::Relaxed)
                        }));
                    }
                    "reload" => {
                        self.reload_config();
                        let _ = reply.ok(json!({ "reloaded": true }));
                    }
                    "cheatsheet" => {
                        if !crate::tray::PAUSED.load(Ordering::Relaxed) {
                            cheatsheet::toggle(self.effective_work_area(), &self.cfg);
                        }
                        let _ = reply.ok(json!({ "toggled": true }));
                    }
                    "logs" => {
                        crate::log::open_log_folder();
                        let _ = reply.ok(json!({ "opened": true }));
                    }
                    "elevate" => {
                        let tid = crate::hook::MAIN_THREAD.load(Ordering::Relaxed);
                        if tid != 0 {
                            let _ = PostThreadMessageW(
                                tid,
                                crate::tray::WM_TRAY_ELEVATE,
                                windows::Win32::Foundation::WPARAM(0),
                                LPARAM(0),
                            );
                        }
                        let _ = reply.ok(json!({ "elevating": true }));
                    }
                    "autostart" => {
                        let next = !crate::tray::autostart_enabled();
                        let ok = crate::tray::set_autostart(next);
                        crate::tray::sync_state();
                        let _ = reply.ok(json!({ "autostart": next, "ok": ok }));
                    }
                    other => {
                        let _ = reply.err(format!("unsupported wm tray item '{other}'"));
                    }
                }
            }
            "tray.quit" => {
                let _ = reply.ok(json!({ "quitting": true }));
                let tid = crate::hook::MAIN_THREAD.load(Ordering::Relaxed);
                if tid != 0 {
                    let _ = PostThreadMessageW(
                        tid,
                        crate::safety::WM_QUIT_REQUEST,
                        windows::Win32::Foundation::WPARAM(0),
                        LPARAM(0),
                    );
                }
            }
            "tray.yield" => {
                let peer_elevated = self
                    .nip
                    .as_ref()
                    .and_then(|rt| {
                        rt.node()
                            .peers()
                            .into_iter()
                            .find(|p| p.module == from)
                            .map(|p| p.elevated)
                    })
                    .unwrap_or(false);
                let yielded = crate::tray::request_yield(peer_elevated);
                let _ = reply.ok(json!({ "yielded": yielded }));
            }
            "overview" => {
                if !crate::tray::PAUSED.load(Ordering::Relaxed) {
                    self.toggle_overview();
                }
                let _ = reply.ok(json!({ "overview": self.overview.is_some() }));
            }
            "cheatsheet" => {
                if !crate::tray::PAUSED.load(Ordering::Relaxed) {
                    cheatsheet::toggle(self.effective_work_area(), &self.cfg);
                }
                let _ = reply.ok(json!({ "toggled": true }));
            }
            "reload" => {
                self.reload_config();
                let _ = reply.ok(json!({ "reloaded": true }));
            }
            "toggle_pause" => {
                self.toggle_pause();
                let _ = reply.ok(json!({
                    "paused": crate::tray::PAUSED.load(Ordering::Relaxed)
                }));
            }
            "config.apply" => {
                match self.apply_config_from_nip(data) {
                    Ok(path) => {
                        self.reload_config();
                        let _ = reply.ok(json!({
                            "saved": true,
                            "path": path.display().to_string(),
                        }));
                    }
                    Err(e) => {
                        let _ = reply.err(e);
                    }
                }
            }
            "open_rules_editor" => {
                let target = ensure_canonical_file(
                    rules::canonical_ignore_path(),
                    Some(rules::active_ignore_path()),
                    "# noren-wm window ignore rules (class:..., exe:..., title:...)\n",
                );
                match target.and_then(|p| open_file_in_editor(&p).map(|()| p)) {
                    Ok(p) => {
                        let _ = reply.ok(json!({ "opened": p.display().to_string() }));
                    }
                    Err(e) => {
                        let _ = reply.err(e);
                    }
                }
            }
            "open_bindings_editor" | "open_snap_presets_editor" => {
                let target = ensure_canonical_file(
                    config::canonical_config_path(),
                    config::active_config_path(),
                    "# noren-wm configuration\n",
                );
                match target.and_then(|p| open_file_in_editor(&p).map(|()| p)) {
                    Ok(p) => {
                        let _ = reply.ok(json!({ "opened": p.display().to_string() }));
                    }
                    Err(e) => {
                        let _ = reply.err(e);
                    }
                }
            }
            other => {
                let _ = reply.err(format!("unsupported wm command '{other}'"));
            }
        }
    }

    fn apply_config_from_nip(&self, data: Value) -> Result<std::path::PathBuf, String> {
        let Some(manifest) = self.nip.as_ref().map(|n| n.manifest().clone()) else {
            return Err("NIP runtime not active".to_string());
        };
        let path = ensure_canonical_file(
            config::canonical_config_path(),
            config::active_config_path(),
            "# noren-wm configuration\n",
        )?;
        let mut values = load_kv_file(&path, &manifest).map_err(|e| e.to_string())?;
        let updates: HashMap<String, FieldValue> =
            serde_json::from_value(data).map_err(|e| format!("invalid config payload: {e}"))?;
        for (k, v) in updates {
            values.insert(k, v);
        }
        save_kv_preserving_comments(&path, &manifest, &values).map_err(|e| e.to_string())?;
        Ok(path)
    }
}

fn ensure_canonical_file(
    canonical: std::path::PathBuf,
    fallback: Option<std::path::PathBuf>,
    default_seed: &str,
) -> Result<std::path::PathBuf, String> {
    if canonical.exists() {
        return Ok(canonical);
    }
    if let Some(parent) = canonical.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let seed = fallback
        .filter(|p| *p != canonical && p.exists())
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_else(|| default_seed.to_string());
    std::fs::write(&canonical, seed).map_err(|e| e.to_string())?;
    Ok(canonical)
}

unsafe fn open_file_in_editor(path: &std::path::Path) -> Result<(), String> {
    let wide: Vec<u16> = path
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let res = ShellExecuteW(
        None,
        w!("open"),
        PCWSTR(wide.as_ptr()),
        PCWSTR::null(),
        None,
        SW_SHOWNORMAL,
    );
    if (res.0 as isize) <= 32 {
        return Err(format!("ShellExecuteW failed opening {}", path.display()));
    }
    Ok(())
}
