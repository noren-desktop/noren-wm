//! Noren Integration Protocol (NIP) integration for `noren-wm`:
//! - Publishes `workspace.state`, `launcher.actions`, and `settings.schema` (`ModuleManifest`).
//! - Consumes `layout.reserved-area` (outer viewport insets) and `osd.workspace` hand-off.
//! - Wakes the Win32 main thread via `PostThreadMessageW(..., WM_NIP_EVENT, ...)` on NIP activity.
//! - Enforces the elevated-WM command allowlist (`NOREN.md` §6.10).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use layout::Rect;
use noren_sdk::protocol::{
    CAP_LAUNCHER_ACTIONS, CAP_LAYOUT_RESERVED_AREA, CAP_OSD_WORKSPACE, CAP_SETTINGS_SCHEMA,
    CAP_WORKSPACE_STATE,
};
use noren_sdk::schema::{
    AdvancedEditor, ConfigFormat, FieldDef, FieldGroup, FieldKind, ModuleManifest,
};
use noren_sdk::storage::{
    install_manifest, load_installed_manifests, load_kv_file, save_kv_preserving_comments,
};
use noren_sdk::{NipEvent, NipNode, NodeConfig, SdkError};
use serde_json::{json, Value};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_APP};

/// Posted to WM's Win32 main thread when one or more `NipEvent` items are queued.
pub const WM_NIP_EVENT: u32 = WM_APP + 31;

/// Commands accepted by `noren-wm` over NIP (enforced at the pipe transport boundary).
pub const ALLOWED_COMMANDS: &[&str] = &[
    "overview",
    "cheatsheet",
    "reload",
    "toggle_pause",
    "ping",
    "config.apply",
    "open_rules_editor",
    "open_bindings_editor",
    "open_snap_presets_editor",
];

fn field_bool(key: &str, label: &str, default_val: bool, description: Option<&str>) -> FieldDef {
    FieldDef {
        key: key.to_string(),
        label: label.to_string(),
        kind: FieldKind::Boolean,
        description: description.map(ToString::to_string),
        requires_running: false,
        default_bool: Some(default_val),
        default_number: None,
        default_text: None,
        min: None,
        max: None,
        step: None,
        choices: Vec::new(),
        nullable: false,
        placeholder: None,
        command: None,
    }
}

fn field_number(
    key: &str,
    label: &str,
    default_val: f64,
    min: f64,
    max: f64,
    step: f64,
    description: Option<&str>,
) -> FieldDef {
    FieldDef {
        key: key.to_string(),
        label: label.to_string(),
        kind: FieldKind::Number,
        description: description.map(ToString::to_string),
        requires_running: false,
        default_bool: None,
        default_number: Some(default_val),
        default_text: None,
        min: Some(min),
        max: Some(max),
        step: Some(step),
        choices: Vec::new(),
        nullable: false,
        placeholder: None,
        command: None,
    }
}

fn field_choice(
    key: &str,
    label: &str,
    choices: &[&str],
    default_val: &str,
    description: Option<&str>,
) -> FieldDef {
    FieldDef {
        key: key.to_string(),
        label: label.to_string(),
        kind: FieldKind::Choice,
        description: description.map(ToString::to_string),
        requires_running: false,
        default_bool: None,
        default_number: None,
        default_text: Some(default_val.to_string()),
        min: None,
        max: None,
        step: None,
        choices: choices.iter().map(|s| (*s).to_string()).collect(),
        nullable: false,
        placeholder: None,
        command: None,
    }
}

fn field_color(
    key: &str,
    label: &str,
    nullable: bool,
    default_val: &str,
    description: Option<&str>,
) -> FieldDef {
    FieldDef {
        key: key.to_string(),
        label: label.to_string(),
        kind: FieldKind::Color,
        description: description.map(ToString::to_string),
        requires_running: false,
        default_bool: None,
        default_number: None,
        default_text: Some(default_val.to_string()),
        min: None,
        max: None,
        step: None,
        choices: Vec::new(),
        nullable,
        placeholder: Some("#RRGGBB".to_string()),
        command: None,
    }
}

fn field_action(key: &str, label: &str, command: &str, description: Option<&str>) -> FieldDef {
    FieldDef {
        key: key.to_string(),
        label: label.to_string(),
        kind: FieldKind::Action,
        description: description.map(ToString::to_string),
        requires_running: true,
        default_bool: None,
        default_number: None,
        default_text: None,
        min: None,
        max: None,
        step: None,
        choices: Vec::new(),
        nullable: false,
        placeholder: None,
        command: Some(command.to_string()),
    }
}

/// Declarative settings manifest installed into `%APPDATA%\Noren\manifests\wm.toml`.
pub fn build_wm_manifest() -> ModuleManifest {
    ModuleManifest {
        id: "wm".to_string(),
        name: "Window Manager".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        executable: std::env::current_exe()
            .ok()
            .map(|p| p.to_string_lossy().into_owned()),
        config_format: ConfigFormat::Kv,
        config_file: "noren-wm.conf".to_string(),
        groups: vec![
            FieldGroup {
                id: "layout".to_string(),
                title: "Layout & Behavior".to_string(),
                fields: vec![
                    field_number(
                        "gap",
                        "Gap (px)",
                        16.0,
                        0.0,
                        200.0,
                        1.0,
                        Some("Gap between tiled windows and screen edges."),
                    ),
                    field_number(
                        "anim_ms",
                        "Animation Duration (ms)",
                        160.0,
                        0.0,
                        1000.0,
                        10.0,
                        Some("0 disables animations."),
                    ),
                    field_number(
                        "resize_step",
                        "Column Resize Step (px)",
                        80.0,
                        10.0,
                        400.0,
                        10.0,
                        Some("Step size for Win+[ and Win+]."),
                    ),
                    field_choice(
                        "height_mode",
                        "Height Adjustment Mode",
                        &["preset", "step"],
                        "preset",
                        Some("preset cycles 1/3, 1/2, full; step adjusts incrementally."),
                    ),
                    field_number(
                        "height_step",
                        "Height Step (px)",
                        80.0,
                        10.0,
                        400.0,
                        10.0,
                        Some("Step size when height_mode = step."),
                    ),
                    field_bool(
                        "mouse_drag",
                        "Mouse Drag Swap & Resize",
                        true,
                        Some("Swap windows or resize column divider by dragging."),
                    ),
                    field_bool(
                        "remember_layout",
                        "Remember Layout",
                        true,
                        Some("Persist layout.state across WM restarts while windows remain open."),
                    ),
                    field_bool(
                        "float_auto_minimize",
                        "Auto-Minimize Floating Windows",
                        true,
                        Some("Minimize non-sticky floating windows when focus moves away."),
                    ),
                ],
            },
            FieldGroup {
                id: "appearance".to_string(),
                title: "Borders & Colors".to_string(),
                fields: vec![
                    field_color(
                        "focus_border",
                        "Focused Border Color",
                        true,
                        "off",
                        Some(
                            "Windows 11 DWM border color for the active window (#RRGGBB, accent, or off).",
                        ),
                    ),
                    field_color(
                        "join_border",
                        "Join Target Border Color",
                        true,
                        "off",
                        Some(
                            "Border highlight while targeting a join (#RRGGBB, accent, or off).",
                        ),
                    ),
                    field_color(
                        "overview_color_selected",
                        "Overview Selection Color",
                        true,
                        "accent",
                        Some(
                            "Highlight for the selected card in Overview (#RRGGBB, accent, or off).",
                        ),
                    ),
                    field_color(
                        "overview_color_active",
                        "Overview Active Window Color",
                        true,
                        "accent",
                        Some(
                            "Highlight for the active window & accents (#RRGGBB, accent, or off).",
                        ),
                    ),
                    field_color(
                        "overview_color_view",
                        "Overview Viewport Frame Color",
                        true,
                        "accent",
                        Some("Viewport frame tint in Overview (#RRGGBB, accent, or off)."),
                    ),
                ],
            },
            FieldGroup {
                id: "overview".to_string(),
                title: "Overview".to_string(),
                fields: vec![
                    field_number(
                        "overview_size",
                        "Overview Size (%)",
                        70.0,
                        30.0,
                        100.0,
                        5.0,
                        Some("Maximum size as percentage of work area."),
                    ),
                    field_choice(
                        "overview_minimap",
                        "Minimap Position",
                        &["right", "left", "off"],
                        "right",
                        None,
                    ),
                    field_choice(
                        "overview_label",
                        "Window Card Label",
                        &["title", "app", "reverse"],
                        "title",
                        None,
                    ),
                    field_bool("overview_hint", "Show Key Hints", true, None),
                    field_bool("overview_thumbnails", "Live Window Thumbnails", true, None),
                    field_bool("overview_wrap", "Wrap Arrow Navigation", true, None),
                ],
            },
            FieldGroup {
                id: "osd".to_string(),
                title: "Workspace OSD & Integration".to_string(),
                fields: vec![
                    field_bool(
                        "workspace_osd",
                        "Workspace Switch OSD",
                        true,
                        Some("Show indicator when switching workspaces."),
                    ),
                    field_bool(
                        "workspace_osd_dots",
                        "Workspace Indicator Dots",
                        false,
                        Some("Show pill/dot indicators in WM's built-in OSD."),
                    ),
                    field_bool(
                        "osd_handoff",
                        "Hand Off Workspace OSD to Notch",
                        true,
                        Some(
                            "Send workspace switch OSD to Notch (or another osd.workspace provider) when running; falls back to WM OSD when off or unavailable.",
                        ),
                    ),
                ],
            },
            FieldGroup {
                id: "snap".to_string(),
                title: "Snap Layouts".to_string(),
                fields: vec![
                    field_bool("snap_layouts", "Snap Layouts Menu", true, None),
                    field_bool("snap_assist", "Snap Assist Window List", true, None),
                    field_number(
                        "snap_trigger",
                        "Top-Edge Trigger Height (px)",
                        24.0,
                        2.0,
                        300.0,
                        2.0,
                        None,
                    ),
                ],
            },
            FieldGroup {
                id: "actions".to_string(),
                title: "Live WM Actions".to_string(),
                fields: vec![
                    field_action("reload_action", "Reload Configuration", "reload", None),
                    field_action("overview_action", "Toggle Overview", "overview", None),
                    field_action(
                        "cheatsheet_action",
                        "Toggle Keyboard Cheatsheet",
                        "cheatsheet",
                        None,
                    ),
                ],
            },
        ],
        advanced_editors: vec![
            AdvancedEditor {
                id: "window_rules".to_string(),
                label: "Edit Window Rules (noren-wm.ignore)".to_string(),
                command: "open_rules_editor".to_string(),
            },
            AdvancedEditor {
                id: "key_bindings".to_string(),
                label: "Edit Key Bindings & Snap Presets (noren-wm.conf)".to_string(),
                command: "open_bindings_editor".to_string(),
            },
        ],
    }
}

/// Snapshot payload for `launcher.actions` capability.
pub fn launcher_actions_payload() -> Value {
    json!({
        "module": "wm",
        "actions": [
            {
                "id": "overview",
                "label": "Overview",
                "description": "Toggle workspace overview",
                "cap": CAP_LAUNCHER_ACTIONS,
                "cmd": "overview"
            },
            {
                "id": "cheatsheet",
                "label": "Keyboard Shortcuts",
                "description": "Toggle WM keyboard shortcuts cheatsheet",
                "cap": CAP_LAUNCHER_ACTIONS,
                "cmd": "cheatsheet"
            }
        ]
    })
}

/// Outer viewport insets requested by modules providing `layout.reserved-area` (e.g. Bar).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReservedArea {
    pub top: i32,
    pub bottom: i32,
    pub left: i32,
    pub right: i32,
}

impl ReservedArea {
    pub fn from_json(v: &Value) -> Self {
        let pick = |primary: &str, alt: &str| -> i32 {
            v.get(primary)
                .or_else(|| v.get(alt))
                .and_then(|x| x.as_i64())
                .unwrap_or(0)
                .clamp(0, 400) as i32
        };
        Self {
            top: pick("top", "top_px"),
            bottom: pick("bottom", "bottom_px"),
            left: pick("left", "left_px"),
            right: pick("right", "right_px"),
        }
    }

    pub fn apply_to(self, work: Rect) -> Rect {
        let min_w = 320.min(work.w);
        let min_h = 240.min(work.h);
        let w = (work.w - self.left - self.right).max(min_w);
        let h = (work.h - self.top - self.bottom).max(min_h);
        Rect {
            x: work.x + self.left,
            y: work.y + self.top,
            w,
            h,
        }
    }
}

/// Tracks per-peer `layout.reserved-area` snapshots and computes the combined outer inset.
#[derive(Debug, Default)]
pub struct ReservedAreaTracker {
    by_peer: HashMap<String, ReservedArea>,
}

impl ReservedAreaTracker {
    /// Updates a peer's reserved area. Returns `true` if the combined `effective()` inset changed.
    pub fn set_peer(&mut self, peer: &str, area: ReservedArea) -> bool {
        let before = self.effective();
        if area == ReservedArea::default() {
            self.by_peer.remove(peer);
        } else {
            self.by_peer.insert(peer.to_string(), area);
        }
        self.effective() != before
    }

    /// Removes a peer (e.g. on disconnect/crash). Returns `true` if `effective()` changed.
    pub fn remove_peer(&mut self, peer: &str) -> bool {
        let before = self.effective();
        self.by_peer.remove(peer);
        self.effective() != before
    }

    /// Effective outer inset across all connected providers.
    pub fn effective(&self) -> ReservedArea {
        let mut out = ReservedArea::default();
        for area in self.by_peer.values() {
            out.top = out.top.max(area.top);
            out.bottom = out.bottom.max(area.bottom);
            out.left = out.left.max(area.left);
            out.right = out.right.max(area.right);
        }
        out
    }
}

/// Returns `true` when WM should hand off the workspace switch OSD to an external provider.
pub fn should_handoff_osd(osd_handoff_cfg: bool, has_osd_provider: bool) -> bool {
    osd_handoff_cfg && has_osd_provider
}

fn wm_node_config(manifest: &ModuleManifest, initial_ws_state: Value) -> NodeConfig {
    let mut initial_state = HashMap::new();
    initial_state.insert(CAP_WORKSPACE_STATE.to_string(), initial_ws_state);
    initial_state.insert(CAP_LAUNCHER_ACTIONS.to_string(), launcher_actions_payload());
    initial_state.insert(
        CAP_SETTINGS_SCHEMA.to_string(),
        serde_json::to_value(manifest).unwrap_or(Value::Null),
    );

    NodeConfig {
        module: "wm".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        provides: vec![
            CAP_WORKSPACE_STATE.to_string(),
            CAP_LAUNCHER_ACTIONS.to_string(),
            CAP_SETTINGS_SCHEMA.to_string(),
        ],
        wants: vec![
            CAP_LAYOUT_RESERVED_AREA.to_string(),
            CAP_OSD_WORKSPACE.to_string(),
        ],
        allowlist: Some(
            ALLOWED_COMMANDS
                .iter()
                .map(|c| (*c).to_string())
                .collect(),
        ),
        initial_state,
    }
}

/// Active NIP runtime owned by the WM process and bridged to the Win32 main loop.
pub struct NipRuntime {
    node: NipNode,
    manifest: ModuleManifest,
    queue: Arc<Mutex<VecDeque<NipEvent>>>,
    stop: Arc<AtomicBool>,
    forwarder: Option<JoinHandle<()>>,
    last_ws_state: Option<Value>,
}

impl NipRuntime {
    pub fn start(main_thread_id: u32, initial_ws_state: Value) -> Result<Self, SdkError> {
        let manifest = build_wm_manifest();
        if let Err(e) = install_manifest(&manifest) {
            crate::log_warn!("failed to install wm manifest: {e}");
        }

        let cfg = wm_node_config(&manifest, initial_ws_state.clone());
        let (node, rx) = NipNode::start(cfg)?;

        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let q_bg = Arc::clone(&queue);
        let stop_bg = Arc::clone(&stop);

        let forwarder = thread::spawn(move || {
            while !stop_bg.load(Ordering::Acquire) {
                match rx.recv_timeout(Duration::from_millis(120)) {
                    Ok(ev) => {
                        if let Ok(mut guard) = q_bg.lock() {
                            guard.push_back(ev);
                        }
                        unsafe {
                            let _ =
                                PostThreadMessageW(main_thread_id, WM_NIP_EVENT, WPARAM(0), LPARAM(0));
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
        });

        Ok(Self {
            node,
            manifest,
            queue,
            stop,
            forwarder: Some(forwarder),
            last_ws_state: Some(initial_ws_state),
        })
    }

    pub fn drain_events(&self) -> Vec<NipEvent> {
        let Ok(mut guard) = self.queue.lock() else {
            return Vec::new();
        };
        guard.drain(..).collect()
    }

    pub fn publish_workspace_state(&mut self, state: Value) {
        if self.last_ws_state.as_ref() == Some(&state) {
            return;
        }
        self.last_ws_state = Some(state.clone());
        if let Err(e) = self.node.publish_state(CAP_WORKSPACE_STATE, state) {
            crate::log_warn!("failed to publish workspace.state: {e}");
        }
    }

    pub fn has_osd_provider(&self) -> bool {
        self.node.has_capability(CAP_OSD_WORKSPACE)
    }

    pub fn send_workspace_osd(&self, active: usize, total: usize, dots: bool) -> bool {
        let providers = self.node.providers_of(CAP_OSD_WORKSPACE);
        if providers.is_empty() {
            return false;
        }
        let payload = json!({
            "active": active,
            "active_1based": active + 1,
            "total": total,
            "dots": dots,
        });
        let mut sent = false;
        for peer in providers {
            if self
                .node
                .emit_event(
                    Some(&peer),
                    Some(CAP_OSD_WORKSPACE),
                    "workspace.switch",
                    payload.clone(),
                )
                .is_ok()
            {
                sent = true;
            }
        }
        sent
    }

    pub fn manifest(&self) -> &ModuleManifest {
        &self.manifest
    }

    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.node.shutdown();
        if let Some(handle) = self.forwarder.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for NipRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    cond()
}

/// Automated Phase 2 NIP integration self-test (`--nip-self-test`).
/// Uses isolated `NOREN_CONFIG_DIR` and `NOREN_PIPE_PREFIX`.
pub fn run_nip_self_test() -> Result<(), String> {
    struct EnvGuard {
        dir: std::path::PathBuf,
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            std::env::remove_var("NOREN_CONFIG_DIR");
            std::env::remove_var("NOREN_PIPE_PREFIX");
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("noren_wm_selftest_{pid}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create temp dir failed: {e}"))?;
    let _guard = EnvGuard { dir: dir.clone() };

    let prefix = format!("noren_wm_test_{pid}.");
    std::env::set_var("NOREN_CONFIG_DIR", &dir);
    std::env::set_var("NOREN_PIPE_PREFIX", &prefix);

    let manifest = build_wm_manifest();
    install_manifest(&manifest).map_err(|e| format!("install_manifest failed: {e}"))?;
    let installed = load_installed_manifests();
    if !installed.iter().any(|m| m.id == "wm") {
        return Err("wm manifest not found in load_installed_manifests".into());
    }
    println!("WM_NIP_TEST_MANIFEST_PERSIST=PASS");

    let initial_ws = json!({
        "active": 0,
        "count": 2,
        "paused": false,
        "elevated": crate::log::is_elevated(),
    });
    let (wm_node, wm_rx) = NipNode::start(wm_node_config(&manifest, initial_ws))
        .map_err(|e| format!("wm NipNode start failed: {e}"))?;

    let mut testbed_init = HashMap::new();
    testbed_init.insert(
        CAP_LAYOUT_RESERVED_AREA.to_string(),
        json!({ "top_px": 40 }),
    );
    testbed_init.insert(CAP_OSD_WORKSPACE.to_string(), json!({ "enabled": true }));

    let (tb_node, tb_rx) = NipNode::start(NodeConfig {
        module: "testbed".to_string(),
        version: "0.1.0".to_string(),
        provides: vec![
            CAP_LAYOUT_RESERVED_AREA.to_string(),
            CAP_OSD_WORKSPACE.to_string(),
        ],
        wants: vec![
            CAP_WORKSPACE_STATE.to_string(),
            CAP_LAUNCHER_ACTIONS.to_string(),
        ],
        allowlist: None,
        initial_state: testbed_init,
    })
    .map_err(|e| format!("testbed NipNode start failed: {e}"))?;

    let connected = wait_until(Duration::from_secs(5), || {
        wm_node.is_peer_connected("testbed") && tb_node.is_peer_connected("wm")
    });
    if !connected {
        return Err("wm and testbed did not connect within 5s".into());
    }

    let got_snapshots = wait_until(Duration::from_secs(4), || {
        tb_node.latest_state("wm", CAP_WORKSPACE_STATE).is_some()
            && tb_node.latest_state("wm", CAP_LAUNCHER_ACTIONS).is_some()
            && wm_node
                .latest_state("testbed", CAP_LAYOUT_RESERVED_AREA)
                .is_some()
    });
    if !got_snapshots {
        return Err("initial capability snapshots were not delivered".into());
    }
    let actions = tb_node
        .latest_state("wm", CAP_LAUNCHER_ACTIONS)
        .ok_or("missing launcher.actions")?;
    let actions_str = actions.to_string();
    if !actions_str.contains("overview") || !actions_str.contains("cheatsheet") {
        return Err(format!("launcher.actions missing overview/cheatsheet: {actions_str}"));
    }
    println!("WM_NIP_TEST_DISCOVERY_AND_CAPABILITIES=PASS");

    // Reserved area + fallback
    let mut tracker = ReservedAreaTracker::default();
    let area_val = wm_node
        .latest_state("testbed", CAP_LAYOUT_RESERVED_AREA)
        .ok_or("missing reserved area state")?;
    let changed = tracker.set_peer("testbed", ReservedArea::from_json(&area_val));
    if !changed {
        return Err("ReservedAreaTracker did not register change".into());
    }
    let screen = Rect {
        x: 0,
        y: 0,
        w: 1920,
        h: 1040,
    };
    let adjusted = tracker.effective().apply_to(screen);
    if adjusted
        != (Rect {
            x: 0,
            y: 40,
            w: 1920,
            h: 1000,
        })
    {
        return Err(format!("unexpected adjusted work area: {adjusted:?}"));
    }

    // OSD hand-off vs toggle off
    if !should_handoff_osd(true, wm_node.has_capability(CAP_OSD_WORKSPACE)) {
        return Err("expected should_handoff_osd(true, true) == true".into());
    }
    if should_handoff_osd(false, wm_node.has_capability(CAP_OSD_WORKSPACE)) {
        return Err("expected should_handoff_osd(false, true) == false".into());
    }
    wm_node
        .emit_event(
            Some("testbed"),
            Some(CAP_OSD_WORKSPACE),
            "workspace.switch",
            json!({ "active": 1, "active_1based": 2, "total": 3, "dots": true }),
        )
        .map_err(|e| format!("emit osd event failed: {e}"))?;
    let got_osd = wait_until(Duration::from_secs(3), || {
        while let Ok(ev) = tb_rx.try_recv() {
            if let NipEvent::EventReceived { from, name, .. } = ev {
                if from == "wm" && name == "workspace.switch" {
                    return true;
                }
            }
        }
        false
    });
    if !got_osd {
        return Err("testbed did not receive osd.workspace event from wm".into());
    }

    // Allowlist + config.apply comment-preserving write
    let wm_conf = noren_sdk::paths::module_config_path("wm", "noren-wm.conf");
    std::fs::create_dir_all(wm_conf.parent().unwrap())
        .map_err(|e| format!("create wm dir failed: {e}"))?;
    std::fs::write(
        &wm_conf,
        "# User comment preserved\ngap = 16\nrule = exe:vlc.exe, float\n",
    )
    .map_err(|e| format!("seed conf failed: {e}"))?;

    let wm_stop = Arc::new(AtomicBool::new(false));
    let wm_stop_bg = Arc::clone(&wm_stop);
    let conf_bg = wm_conf.clone();
    let manifest_bg = manifest.clone();
    let responder = thread::spawn(move || {
        while !wm_stop_bg.load(Ordering::Acquire) {
            match wm_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(NipEvent::Command {
                    name, data, reply, ..
                }) => match name.as_str() {
                    "ping" => {
                        let _ = reply.ok(json!({ "ok": true, "module": "wm" }));
                    }
                    "config.apply" => {
                        let mut values = load_kv_file(&conf_bg, &manifest_bg).unwrap_or_default();
                        if let Ok(incoming) = serde_json::from_value::<
                            HashMap<String, noren_sdk::FieldValue>,
                        >(data)
                        {
                            for (k, v) in incoming {
                                values.insert(k, v);
                            }
                        }
                        match save_kv_preserving_comments(&conf_bg, &manifest_bg, &values) {
                            Ok(()) => {
                                let _ = reply.ok(json!({ "saved": true }));
                            }
                            Err(e) => {
                                let _ = reply.err(e.to_string());
                            }
                        }
                    }
                    other => {
                        let _ = reply.err(format!("unexpected test cmd: {other}"));
                    }
                },
                Ok(_) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    tb_node
        .send_cmd(
            "wm",
            CAP_SETTINGS_SCHEMA,
            "ping",
            Value::Null,
            Duration::from_secs(3),
        )
        .map_err(|e| format!("allowlisted ping failed: {e}"))?;

    tb_node
        .send_cmd(
            "wm",
            CAP_SETTINGS_SCHEMA,
            "config.apply",
            json!({ "gap": 24, "osd_handoff": false }),
            Duration::from_secs(3),
        )
        .map_err(|e| format!("config.apply failed: {e}"))?;

    let conf_text =
        std::fs::read_to_string(&wm_conf).map_err(|e| format!("read updated conf failed: {e}"))?;
    if !conf_text.contains("# User comment preserved")
        || !conf_text.contains("rule = exe:vlc.exe, float")
        || !conf_text.contains("gap = 24")
        || !conf_text.contains("osd_handoff = off")
    {
        wm_stop.store(true, Ordering::Release);
        let _ = responder.join();
        return Err(format!("config.apply did not preserve comments/rules:\n{conf_text}"));
    }

    let rejected = tb_node.send_cmd(
        "wm",
        CAP_LAUNCHER_ACTIONS,
        "exec_shell",
        Value::Null,
        Duration::from_secs(3),
    );
    match rejected {
        Err(SdkError::CommandRejected(msg)) if msg.contains("allowlist") => {}
        other => {
            wm_stop.store(true, Ordering::Release);
            let _ = responder.join();
            return Err(format!("expected allowlist rejection, got {other:?}"));
        }
    }
    println!("WM_NIP_TEST_COMMANDS_AND_ALLOWLIST=PASS");

    // Disconnect testbed -> reserved area & OSD provider must fall back
    tb_node.shutdown();
    let disconnected = wait_until(Duration::from_secs(4), || {
        !wm_node.is_peer_connected("testbed")
    });
    if !disconnected {
        wm_stop.store(true, Ordering::Release);
        let _ = responder.join();
        return Err("wm did not detect testbed disconnect".into());
    }
    if wm_node.has_capability(CAP_OSD_WORKSPACE) {
        wm_stop.store(true, Ordering::Release);
        let _ = responder.join();
        return Err("osd.workspace capability still active after disconnect".into());
    }
    let cleared = tracker.remove_peer("testbed");
    if !cleared || tracker.effective().apply_to(screen) != screen {
        wm_stop.store(true, Ordering::Release);
        let _ = responder.join();
        return Err("reserved area did not fall back to full work area on disconnect".into());
    }
    println!("WM_NIP_TEST_RESERVED_AND_OSD_FALLBACK=PASS");

    wm_stop.store(true, Ordering::Release);
    wm_node.shutdown();
    let _ = responder.join();
    println!("WM_NIP_SELF_TEST=ALL_PASS");
    Ok(())
}

/// Live probe (`--probe-wm`) for testing standard-level client ↔ running (or elevated) `noren-wm`.
pub fn run_probe_wm() -> Result<(), String> {
    let mut initial = HashMap::new();
    initial.insert(
        CAP_LAYOUT_RESERVED_AREA.to_string(),
        json!({ "top_px": 0 }),
    );
    initial.insert(CAP_OSD_WORKSPACE.to_string(), json!({ "enabled": true }));

    let (probe, _rx) = NipNode::start(NodeConfig {
        module: "testbed-probe".to_string(),
        version: "0.1.0".to_string(),
        provides: vec![
            CAP_LAYOUT_RESERVED_AREA.to_string(),
            CAP_OSD_WORKSPACE.to_string(),
        ],
        wants: vec![
            CAP_WORKSPACE_STATE.to_string(),
            CAP_LAUNCHER_ACTIONS.to_string(),
        ],
        allowlist: None,
        initial_state: initial,
    })
    .map_err(|e| format!("failed to start testbed-probe: {e}"))?;

    let info = probe.integrity_info();
    println!(
        "WM_PROBE_CLIENT integrity={} elevated={}",
        info.level, info.elevated
    );

    if !wait_until(Duration::from_secs(5), || probe.is_peer_connected("wm")) {
        probe.shutdown();
        return Err("could not connect to running 'wm' peer within 5s".into());
    }

    let wm_peer = probe
        .peers()
        .into_iter()
        .find(|p| p.module == "wm")
        .ok_or("wm peer missing after connect")?;
    println!(
        "WM_PROBE_CONNECTED target=wm pid={} integrity={} elevated={}",
        wm_peer.pid, wm_peer.integrity, wm_peer.elevated
    );

    if !wait_until(Duration::from_secs(3), || {
        probe.latest_state("wm", CAP_WORKSPACE_STATE).is_some()
            && probe.latest_state("wm", CAP_LAUNCHER_ACTIONS).is_some()
    }) {
        probe.shutdown();
        return Err("did not receive workspace.state / launcher.actions from wm".into());
    }

    let ping_reply = probe
        .send_cmd(
            "wm",
            CAP_SETTINGS_SCHEMA,
            "ping",
            Value::Null,
            Duration::from_secs(3),
        )
        .map_err(|e| format!("ping failed: {e}"))?;
    println!("WM_PROBE_PING_OK={ping_reply}");

    let blocked = probe.send_cmd(
        "wm",
        CAP_LAUNCHER_ACTIONS,
        "exec_shell",
        Value::Null,
        Duration::from_secs(3),
    );
    match blocked {
        Err(SdkError::CommandRejected(msg)) if msg.contains("allowlist") => {
            println!("WM_PROBE_ALLOWLIST_BLOCKED_OK={msg}");
        }
        other => {
            probe.shutdown();
            return Err(format!("expected allowlist block, got {other:?}"));
        }
    }

    probe.shutdown();
    println!("WM_LIVE_PROBE=PASS");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn reserved_area_parses_both_key_styles_and_clamps() {
        let a = ReservedArea::from_json(&json!({ "top_px": 40, "bottom": 24, "left_px": -5, "right": 999 }));
        assert_eq!(
            a,
            ReservedArea {
                top: 40,
                bottom: 24,
                left: 0,
                right: 400
            }
        );

        let screen = Rect {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
        };
        assert_eq!(
            a.apply_to(screen),
            Rect {
                x: 0,
                y: 40,
                w: 1520,
                h: 1016
            }
        );
    }

    #[test]
    fn reserved_area_tracker_updates_and_clears_on_disconnect() {
        let mut tracker = ReservedAreaTracker::default();
        assert_eq!(tracker.effective(), ReservedArea::default());
        assert!(tracker.set_peer(
            "bar",
            ReservedArea {
                top: 36,
                ..Default::default()
            }
        ));
        assert!(!tracker.set_peer(
            "bar",
            ReservedArea {
                top: 36,
                ..Default::default()
            }
        ));
        assert!(tracker.set_peer(
            "dock",
            ReservedArea {
                bottom: 48,
                ..Default::default()
            }
        ));
        assert_eq!(
            tracker.effective(),
            ReservedArea {
                top: 36,
                bottom: 48,
                left: 0,
                right: 0
            }
        );
        assert!(tracker.remove_peer("bar"));
        assert_eq!(
            tracker.effective(),
            ReservedArea {
                top: 0,
                bottom: 48,
                left: 0,
                right: 0
            }
        );
        assert!(tracker.remove_peer("dock"));
        assert_eq!(tracker.effective(), ReservedArea::default());
    }

    #[test]
    fn osd_handoff_requires_both_toggle_and_provider() {
        assert!(!should_handoff_osd(false, false));
        assert!(!should_handoff_osd(false, true));
        assert!(!should_handoff_osd(true, false));
        assert!(should_handoff_osd(true, true));
    }

    #[test]
    fn wm_manifest_roundtrips_with_kv_storage_preserving_comments_and_rules() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("noren_wm_manifest_rt_{nanos}"));
        std::fs::create_dir_all(&dir).unwrap();
        let conf = dir.join("noren-wm.conf");
        std::fs::write(
            &conf,
            "# Comment line\ngap = 16\nrule = exe:notepad.exe, float\n#bind.close = win+q\n",
        )
        .unwrap();

        let manifest = build_wm_manifest();
        assert_eq!(manifest.id, "wm");
        assert_eq!(manifest.config_format, ConfigFormat::Kv);
        assert_eq!(manifest.advanced_editors.len(), 2);

        let mut vals = load_kv_file(&conf, &manifest).unwrap();
        vals.insert("gap".to_string(), noren_sdk::FieldValue::Number(20.0));
        vals.insert("osd_handoff".to_string(), noren_sdk::FieldValue::Bool(false));
        vals.insert(
            "focus_border".to_string(),
            noren_sdk::FieldValue::Text("accent".to_string()),
        );
        save_kv_preserving_comments(&conf, &manifest, &vals).unwrap();

        let saved = std::fs::read_to_string(&conf).unwrap();
        assert!(saved.contains("# Comment line"));
        assert!(saved.contains("rule = exe:notepad.exe, float"));
        assert!(saved.contains("#bind.close = win+q"));

        let (parsed, warnings) = crate::config::Config::parse(&saved);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(parsed.gap, 20);
        assert!(!parsed.osd_handoff);
        assert_eq!(parsed.focus_border, Some(crate::config::BorderColor::Accent));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
