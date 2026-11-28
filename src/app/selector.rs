//! Controller for the in-slot Stacked Card Deck Selector (`Win+Alt+Tab`).
//!
//! Opens inside the focused window's slot, lets the user cycle through tiled windows
//! from other Spaces in the active workspace (including minimized tiled windows,
//! excluding floating windows and windows in the same Space), and steals the chosen
//! window into the active slot while moving the displaced window to a new Full Space.

use std::sync::atomic::Ordering;

use layout::{Rect, WindowId};
use windows::Win32::UI::WindowsAndMessaging::{
    IsIconic, SetForegroundWindow, ShowWindow, SW_RESTORE,
};

use super::App;
use crate::hook::{SEL_MOVED, SEL_OPEN};
use crate::slot_selector::{self, SelectorCard};
use crate::winutil::{title, wid};
use crate::{config, rules, safety};

pub(crate) struct SelectorSession {
    pub(crate) slot_rect: Rect,
    pub(crate) candidates: Vec<WindowId>,
    pub(crate) index: usize,
}

impl App {
    /// Open the in-slot Stacked Card Deck selector (or step if already open).
    pub(crate) unsafe fn selector_open(&mut self) {
        if self.selector.is_some() {
            self.selector_step(true);
            return;
        }
        self.sync();
        let Some(fid) = self.ws.cur().focused() else {
            SEL_OPEN.store(false, Ordering::Relaxed);
            return;
        };
        let Some(slot_rect) = self
            .ws
            .cur()
            .layout()
            .into_iter()
            .find(|p| p.id == fid)
            .map(|p| p.rect)
        else {
            SEL_OPEN.store(false, Ordering::Relaxed);
            return;
        };
        let candidates: Vec<WindowId> = self
            .ws
            .cur()
            .selector_candidates()
            .into_iter()
            .filter(|id| self.map.get(id).is_some_and(|&h| safety::alive(h)))
            .collect();
        self.selector = Some(SelectorSession {
            slot_rect,
            candidates,
            index: 0,
        });
        SEL_MOVED.store(false, Ordering::Relaxed);
        SEL_OPEN.store(true, Ordering::Relaxed);
        self.render_selector();
    }

    /// Cycle through the stacked card deck (`forward = true` next, `false` previous).
    pub(crate) unsafe fn selector_step(&mut self, forward: bool) {
        let Some(sess) = self.selector.as_mut() else {
            return;
        };
        let n = sess.candidates.len();
        if n == 0 {
            return;
        }
        sess.index = if forward {
            (sess.index + 1) % n
        } else {
            (sess.index + n - 1) % n
        };
        self.render_selector();
    }

    unsafe fn render_selector(&self) {
        let Some(sess) = self.selector.as_ref() else {
            return;
        };
        let total = sess.candidates.len();
        if total == 0 {
            slot_selector::show(
                sess.slot_rect,
                None,
                0,
                0,
                self.cfg.overview_colors,
                self.cfg.overview_thumbnails,
            );
            return;
        }
        let target = sess.candidates[sess.index];
        let hwnd = self.map.get(&target).copied();
        let (raw, label, minimized) = match hwnd {
            Some(h) => {
                let t = title(h);
                let mode = self.cfg.overview_label;
                let exe = if mode == config::LabelMode::App {
                    rules::exe_name(h)
                } else {
                    None
                };
                let lbl = config::label(mode, &t, exe.as_deref(), &self.cfg.aliases);
                let min = self.hidden.contains(&target) || IsIconic(h).as_bool();
                (h.0 as isize, lbl, min)
            }
            None => (0, String::new(), false),
        };
        slot_selector::show(
            sess.slot_rect,
            Some(SelectorCard {
                hwnd: raw,
                label,
                minimized,
            }),
            sess.index,
            total,
            self.cfg.overview_colors,
            self.cfg.overview_thumbnails,
        );
    }

    /// Commit: steal the selected candidate window into the active slot.
    pub(crate) unsafe fn selector_commit(&mut self) {
        SEL_OPEN.store(false, Ordering::Relaxed);
        SEL_MOVED.store(false, Ordering::Relaxed);
        slot_selector::hide();
        let Some(sess) = self.selector.take() else {
            return;
        };
        let Some(&target) = sess.candidates.get(sess.index) else {
            return;
        };
        // If the stolen window was minimized, restore it so it can fill the slot immediately.
        if let Some(&h) = self.map.get(&target) {
            if self.hidden.remove(&target) || IsIconic(h).as_bool() {
                let _ = ShowWindow(h, SW_RESTORE);
                self.ws.set_hidden(target, false);
            }
        }
        if self.ws.cur_mut().steal_into_slot(target) {
            self.apply();
            if let Some(&h) = self.map.get(&target) {
                let prev = self.last_fg;
                let _ = SetForegroundWindow(h);
                let next = wid(h);
                self.last_fg = Some(next);
                if let Some(p) = prev {
                    self.auto_minimize_float(p, next);
                }
            }
        }
    }

    /// Cancel without changing the layout.
    pub(crate) unsafe fn selector_cancel(&mut self) {
        SEL_OPEN.store(false, Ordering::Relaxed);
        SEL_MOVED.store(false, Ordering::Relaxed);
        slot_selector::hide();
        self.selector = None;
    }
}
