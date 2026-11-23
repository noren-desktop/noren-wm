# Architecture — `noren-wm`

This document describes the architecture of **noren-wm** and how it integrates with its separate layout engine repository (**vscroll-wm** / crate `vscroll-layout`). It is intended as a fast reference for both human contributors and AI coding agents.

---

## 1. Two-Repository Split

`noren-wm` is intentionally split into two independent Git repositories:

```text
┌──────────────────────────────────────────────────────────┐
│  noren-wm  (this repo)                                   │
│  Windows application, Win32 integration & Fluent UI      │
│  - Event loop, keyboard/event hooks, window placement    │
│  - Animations, mouse drag, taskbar hiding, crash safety  │
│  - Overlays: Overview, Cheatsheet, OSD, Pick, Snap Menu  │
│  - Config, keybindings, window rules, tray, logging      │
└────────────────────────────┬─────────────────────────────┘
                             │ Cargo dependency:
                             │ layout = { package = "vscroll-layout", git = "..." }
                             ▼
┌──────────────────────────────────────────────────────────┐
│  vscroll-wm  (crate: vscroll-layout)                     │
│  Pure Rust, zero-dependency layout engine                │
│  - Vertical scrolling strip with two lanes (L / R / Full)│
│  - Per-window heights, divider ratios, viewport scroll   │
│  - Operations: snap, maximize, focus, swap, join/split   │
│  - Snap layout presets & multi-workspace state           │
└──────────────────────────────────────────────────────────┘
```

In `Cargo.toml`, the engine crate `vscroll-layout` is aliased as `layout`, so Rust code in `noren-wm` imports it via `use layout::{...};`.

### Where Should a Change Go?

| Type of Change | Target Repo |
| :--- | :--- |
| Strip scrolling math, 2-lane placement, divider clamping, snap preset geometry, workspace strip ops, focus/swap/join rules | **`vscroll-wm`** (`vscroll-layout`) |
| Window placement/animations on Windows, Win32 hooks, taskbar hiding, safety/recovery, multi-workspace parking | **`noren-wm`** (`src/app/`, `src/hook.rs`, `src/winutil.rs`, `src/taskbar.rs`, `src/safety.rs`) |
| UI overlays (Overview, Cheatsheet, Workspace OSD, Pick marker, Snap Menu / Snap Assist), theming, tray | **`noren-wm`** (`src/overview.rs`, `src/cheatsheet.rs`, `src/osd.rs`, `src/pick_overlay.rs`, `src/snap_menu.rs`, `src/snap_slots.rs`, `src/theme.rs`, `src/tray.rs`) |
| Config keys, custom keybindings, window rules/ignore, layout persistence format, logging | **`noren-wm`** (`src/config.rs`, `src/bindings.rs`, `src/rules.rs`, `src/layout_state.rs`, `src/log.rs`) |

---

## 2. Module Map (`src/`)

### Entrypoint, Constants & Hooks
- **`src/main.rs`** — Single-instance check, DPI awareness, subsystem init, hotkey/hook registration, Win32 message loop (`GetMessageW`), graceful shutdown & window restoration, elevated relaunch.
- **`src/consts.rs`** — Hotkey IDs, custom `WM_*` internal thread messages, timing constants (`DEFAULT_ANIM_MS`, `EVENT_DEBOUNCE_MS`, `RECHECK_MS`, etc.).
- **`src/hook.rs`** — Dedicated low-level keyboard hook thread (`noren-kb-hook`) that intercepts `Win+...` chords and posts `WM_ACTION` / overlay navigation messages to the main thread; also hosts `win_event_proc` for `SetWinEventHook` callbacks.

### Application State & Controllers (`src/app/`)
- **`src/app/mod.rs`** — Core `App` struct holding `ws: layout::Workspaces`, `HWND` maps (`map`, `floating`, `sticky`, `blocked`), animation state, parked/hidden tracking, active sessions (`overview`, `snap_drag`, `snap_assist`, `join_aim`, `pick`), and `sync()` / `apply()`.
- **`src/app/events.rs`** — Dispatches hotkeys, `WM_WINEVENT` notifications, and debounce timers.
- **`src/app/actions.rs`** — Window actions: close, minimize, float/sticky toggle, pause toggle, window inspector dialog, config problem reporting.
- **`src/app/anim.rs`** — Smooth window movement/resize animations driven by a timer (`tick_anim`) with automatic zero-idle shutdown.
- **`src/app/mouse.rs`** — Post-drag detection for swapping windows or resizing the column divider via native Windows drag/resize.
- **`src/app/workspace.rs`** — Workspace switching, sending windows across workspaces, slide transition cues, and off-screen parking (`PARK_X`) with taskbar button hiding.
- **`src/app/overview_ctl.rs`** — Application-side controller for the Overview session (bridges `App` state and `overview.rs` rendering).
- **`src/app/pick.rs`** — Lift-and-drop (`pick`) mode controller (bridges `App` state and `pick_overlay.rs`).
- **`src/app/snap.rs`** — Top-edge drag tracking (`SnapDrag`) and empty-zone Snap Assist (`SnapAssist`) controller.
- **`src/app/persist.rs`** — Saves and restores workspace layouts across restarts using `layout_state.rs`.

### Win32 Platform & Safety
- **`src/winutil.rs`** — Win32 helpers: window enumeration, tiling eligibility filtering, DWM extended frame bounds, window placement, focus border color, single-instance mutex (`Local\noren-wm-single-instance`).
- **`src/taskbar.rs`** — Hides/shows taskbar buttons for parked (inactive workspace) windows via `ITaskbarList`.
- **`src/safety.rs`** — Tracks original window bounds and restores all managed windows on exit, `Ctrl+C`, emergency quit, or panic so no window is left stranded off-screen.
- **`src/tray.rs`** — System tray icon, context menu (Pause, Reload, Help, Run elevated, Run at login, Quit), and `HKCU\...\Run` autostart management.

### Configuration, Rules, Persistence & Logging
- **`src/config.rs`** — Parses `noren-wm.conf` (`key = value`), custom `snap_layout.*`, `bind.*`, `rule`, `alias.*`, overview colors, and environment variable overrides.
- **`src/bindings.rs`** — Single source of truth (`ACTIONS`) for all bindable shortcuts, chord parsing, conflict resolution, and human-readable labels.
- **`src/rules.rs`** — Parses `noren-wm.ignore` and `rule = ...` directives (hot-reloaded without restart).
- **`src/layout_state.rs`** — Plain-text serializer/deserializer (`noren-layout 1`) for `layout.state`.
- **`src/log.rs`** — File logger (`%LOCALAPPDATA%\noren-wm\noren-wm.log`) with 1 MiB rotation and panic crash reports.

### UI Overlays & Theming
- **`src/theme.rs`** — Reads Windows dark/light mode (`AppsUseLightTheme`) and accent color (`AccentColor`), provides Fluent Design / WinUI 3 drawing primitives and easing curves.
- **`src/overview.rs`** — Full-featured Overview panel (Task View + vertical strip minimap + workspace dock cards + DWM live thumbnails + drag-and-drop).
- **`src/cheatsheet.rs`** — Centered keyboard shortcut cheatsheet (`Win+/`) dynamically generated from `bindings.rs` and active config.
- **`src/osd.rs`** — Compact bottom-center workspace switch indicator with Fluent Motion slide/fade.
- **`src/pick_overlay.rs`** — Pure slot builder + translucent desktop drop-target marker for `pick` mode.
- **`src/snap_menu.rs`** — Top-edge Snap Layouts preset menu shown while dragging a window upward.
- **`src/snap_slots.rs`** — Snap Assist window-picker cards (DWM thumbnails + titles) for remaining empty zones in a preset.

---

## 3. Threading & Event Flow

`noren-wm` keeps all mutable application and layout state on **one main thread** (no mutex contention around `App` or `Workspaces`):

1. **Keyboard Hook Thread (`noren-kb-hook`)** — Installs `WH_KEYBOARD_LL` so keystrokes are processed immediately without blocking on Win32 window operations. Matched shortcuts post lightweight `WM_ACTION` / `WM_OV_*` / `WM_PICK_*` messages to `MAIN_THREAD` via `PostThreadMessageW`.
2. **WinEvent Hooks (`SetWinEventHook`)** — Out-of-context callbacks (`EVENT_SYSTEM_FOREGROUND`, `MOVESIZESTART/END`, `MINIMIZESTART/END`, `OBJECT_DESTROY/HIDE`, `CLOAKED/UNCLOAKED`) post `WM_WINEVENT` to the main thread.
3. **Main Thread Message Loop (`main.rs`)** — Receives all messages, mutates `App` (which delegates layout calculations to `layout::Workspaces`), and applies resulting window rectangles to Win32 (either directly or via `anim.rs`).
4. **Timers**:
   - **Animation timer (`anim_timer`)**: active only while windows are animating.
   - **Snap drag timer (`snap_timer`)**: active only while a window is being dragged.
   - **Sync debounce timer (`sync_timer`)**: coalesces bursts of WinEvents.
   - **Safety-net poll timer**: 1000 ms (or 300 ms if event hooks failed) to catch windows that initialize late.

---

## 4. Runtime Files & Environment Variables

- **Config file**: `noren-wm.conf` (search order: `NOREN_CONFIG` env var → next to `.exe` → current working directory → `%LOCALAPPDATA%\noren-wm\noren-wm.conf`).
- **Ignore file**: `noren-wm.ignore` (search order: `NOREN_IGNORE` env var → next to `.exe` → current working directory → `%LOCALAPPDATA%\noren-wm\noren-wm.ignore`).
- **Env overrides**: `NOREN_GAP`, `NOREN_ANIM_MS`.
- **Runtime data**: `%LOCALAPPDATA%\noren-wm\layout.state` and `%LOCALAPPDATA%\noren-wm\noren-wm.log`.

---

## 5. Developing Across `noren-wm` and `vscroll-wm`

### Local Development Override
To edit `vscroll-wm` and immediately test changes in `noren-wm` without committing/pushing first, create `.cargo/config.toml` (gitignored; see `.cargo/config.toml.example`):

```toml
[patch."https://github.com/jack-dozen/vscroll-wm.git"]
vscroll-layout = { path = "../../wm pokonya/vscroll-wm" }
```

> **Note:** Cargo resolves `.cargo/config.toml` relative to the current working directory. Always run `cargo build` / `cargo test` / `cargo run` from inside the `noren-wm` directory.

### Build & Test Commands
- In `noren-wm`:
  ```powershell
  cargo test
  cargo build --release   # outputs target\release\noren-wm.exe
  ```
- In `vscroll-wm`:
  ```powershell
  cargo test
  ```

### Release / Push Order When Modifying the Engine
1. Commit and push changes in **`vscroll-wm`** first (to `main`, or the tracked branch/tag).
2. In **`noren-wm`**, run `cargo update -p vscroll-layout` to update `Cargo.lock` to the published GitHub commit.
3. Commit and push **`noren-wm`**.
