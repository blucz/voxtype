//! `voxtype-osd-gtk4` — GTK4 + gtk4-layer-shell on-screen mic visualizer
//! for the Voxtype daemon.
//!
//! Renders a click-through, layer-shell-anchored window containing the
//! scrolling waveform plus a segmented peak meter. Audio frames arrive on
//! the daemon's audio Unix socket via [`voxtype::osd::ipc::run_ipc_loop`],
//! decoded into [`AudioFrame`]s by a tokio runtime on a worker thread, and
//! pushed into a shared [`FrameRing`] + [`PeakHold`]. The GTK side polls a
//! ~60 Hz `glib::timeout_add_local` callback that redraws the
//! `DrawingArea` whenever new frames have arrived.
//!
//! When the IPC socket is silent for `idle_timeout_secs` (Idle proxy) the
//! window is hidden so the binary does no rendering work and consumes
//! effectively zero CPU. It reappears when frames resume.
//!
//! Run with `RUST_LOG=debug` for verbose logs.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cairo::{Context, RectangleInt, Region};
use clap::Parser;
use gtk4::glib;
use gtk4::prelude::*;
use gtk4::{Application, ApplicationWindow, DrawingArea};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use voxtype::audio::levels::{AudioFrame, FRAME_HZ};
use voxtype::config::Config as VoxtypeConfig;
use voxtype::osd::config::{OsdConfig, OsdPosition, OsdVisualMode};
use voxtype::osd::ipc::{resolve_socket_path, run_ipc_loop, FrameRing, DEFAULT_RING_DEPTH};
use voxtype::osd::preview::{
    default_hint_path, default_preview_path, read_hint, read_preview, PREVIEW_POLL_MS,
};
use voxtype::osd::theme::ThemeWatcher;
use voxtype::osd::visual::{peak_meter_fraction, project_envelope, MeterZone, Palette, PeakHold};

/// Load the `[osd]` section from the voxtype config file, falling back to
/// `OsdConfig::default()` on any error (file missing, unreadable, parse
/// failure, or `[osd]` section absent).
///
/// We deliberately ignore parse errors instead of returning them: the OSD
/// is a side car, and a malformed config shouldn't prevent it from running
/// with sensible defaults — the user will see the daemon complain about
/// the same file separately.
fn load_osd_config_from_file(explicit: Option<&std::path::Path>) -> OsdConfig {
    let path = explicit
        .map(std::path::Path::to_path_buf)
        .or_else(VoxtypeConfig::default_path);
    let Some(path) = path else {
        return OsdConfig::default();
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return OsdConfig::default(),
    };

    #[derive(serde::Deserialize, Default)]
    struct PartialConfig {
        #[serde(default)]
        osd: Option<OsdConfig>,
    }

    match toml::from_str::<PartialConfig>(&content) {
        Ok(p) => p.osd.unwrap_or_default(),
        Err(_) => OsdConfig::default(),
    }
}

/// Application id for the GTK4 frontend.
const APP_ID: &str = "io.voxtype.OsdGtk4";

/// Render tick period (~60 Hz). The redraw is gated on whether new frames
/// have arrived since the last paint, so this is a cheap upper bound.
const RENDER_TICK_MS: u32 = 16;

/// How long we wait without frames before treating the daemon as idle and
/// hiding the surface. Matches the BRIEF's "Idle: surface destroyed" rule.
const IDLE_TIMEOUT_SECS: f32 = 0.15;

/// True while the daemon has marked the in-flight recording as OSD-suppressed
/// (`voxtype record start --no-osd`).
///
/// Frames stop flowing for a suppressed recording too, so the idle timeout
/// alone would hide the surface. This is checked anyway so suppression is
/// immediate rather than waiting out IDLE_TIMEOUT_SECS, and so the behaviour
/// matches the Quickshell frontend exactly.
fn osd_suppressed() -> bool {
    let base = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| format!("/run/user/{}", unsafe { libc::getuid() }));
    std::path::Path::new(&base)
        .join("voxtype/osd_suppressed")
        .exists()
}

/// Number of segments in the vertical peak meter.
const METER_SEGMENTS: usize = 10;

/// dBFS floor for the peak meter (maps to "empty bar").
const METER_FLOOR_DBFS: f32 = -60.0;

/// Preview text point size, and the line box as a multiple of it.
const PREVIEW_FONT_SIZE: f64 = 15.0;
const PREVIEW_LINE_FACTOR: f64 = 1.38;

/// Inner padding of the text/pill panels, in pixels.
const PANEL_PAD_X: f64 = 16.0;

/// Corner radius for the text/pill panels.
const PANEL_RADIUS: f64 = 12.0;

/// Surface size used by `visual_mode = "text"` when the user has not
/// chosen one. A 400x48 strip fits a waveform but only one short line of
/// transcript, so text mode substitutes a panel proportioned for a few
/// lines. An explicit width/height in config or on the CLI still wins —
/// see `apply_text_mode_sizing`.
const TEXT_MODE_WIDTH: u32 = 480;
const TEXT_MODE_HEIGHT: u32 = 82;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "voxtype-osd-gtk4",
    version,
    about = "Voxtype on-screen mic visualizer (GTK4 + gtk4-layer-shell)"
)]
struct Args {
    /// Path to the voxtype config file. Defaults to
    /// `~/.config/voxtype/config.toml`. Only the `[osd]` section is read.
    #[arg(long, env = "VOXTYPE_CONFIG")]
    config: Option<PathBuf>,

    /// Path to the audio-frame Unix socket. Defaults to
    /// `$XDG_RUNTIME_DIR/voxtype/audio.sock`.
    #[arg(long, env = "VOXTYPE_OSD_SOCKET")]
    socket: Option<PathBuf>,

    /// Seconds to wait between reconnect attempts when the daemon is down.
    #[arg(long, default_value = "1.0", env = "VOXTYPE_OSD_RECONNECT_SECS")]
    reconnect_secs: f32,

    /// Print one debug line per N frames received (0 = quiet).
    #[arg(long, default_value = "0", env = "VOXTYPE_OSD_LOG_EVERY")]
    log_every: u32,

    /// Held-peak decay rate in dB/sec.
    #[arg(long, default_value = "6.0", env = "VOXTYPE_OSD_PEAK_DECAY")]
    peak_decay_db_per_sec: f32,

    /// Surface width in physical pixels.
    #[arg(long, env = "VOXTYPE_OSD_WIDTH")]
    width_px: Option<u32>,

    /// Surface height in physical pixels.
    #[arg(long, env = "VOXTYPE_OSD_HEIGHT")]
    height_px: Option<u32>,

    /// Margin from the screen edge in physical pixels.
    #[arg(long, env = "VOXTYPE_OSD_MARGIN")]
    margin_px: Option<u32>,

    /// Visual gain applied to audio samples before drawing the waveform.
    /// Higher = waveform fills more of the vertical for quiet inputs.
    /// Reduce for hot mics (e.g. 4.0); raise for quiet sources (e.g. 14.0).
    #[arg(long, env = "VOXTYPE_OSD_GAIN")]
    waveform_gain: Option<f32>,
}

/// Small user-owned layout state, independent of the dictation session.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct PanelLayout {
    left_fraction: f64,
    top_px: i32,
    collapsed: bool,
}
impl Default for PanelLayout {
    fn default() -> Self {
        Self {
            left_fraction: 0.5,
            top_px: 32,
            collapsed: false,
        }
    }
}
fn layout_path() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(std::env::temp_dir)
        .join("voxtype/osd-layout.json")
}
fn load_layout() -> PanelLayout {
    std::fs::read(layout_path())
        .ok()
        .and_then(|s| serde_json::from_slice(&s).ok())
        .unwrap_or_default()
}
fn save_layout(layout: &PanelLayout) {
    let path = layout_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(bytes) = serde_json::to_vec(layout) {
        let staged = path.with_extension("tmp");
        if std::fs::write(&staged, bytes).is_ok() {
            let _ = std::fs::rename(staged, path);
        }
    }
}
fn panel_bounds(window: &ApplicationWindow) -> (i32, i32) {
    let monitor = window.monitor().or_else(|| {
        window
            .surface()
            .and_then(|s| s.display().monitor_at_surface(&s))
    });
    monitor
        .map(|m| (m.geometry().width(), m.geometry().height()))
        .unwrap_or((1920, 1080))
}
fn panel_geometry(
    layout: &PanelLayout,
    screen: (i32, i32),
    expanded: (i32, i32),
) -> (i32, i32, i32, i32) {
    let (sw, sh) = screen;
    let (w, h) = if layout.collapsed {
        (240, 34)
    } else {
        expanded
    };
    let w = w.min((sw - 8).max(1));
    let h = h.min((sh - 8).max(1));
    let fraction = if layout.left_fraction.is_finite() {
        layout.left_fraction.clamp(0.0, 1.0)
    } else {
        0.5
    };
    let x = ((sw - w).max(0) as f64 * fraction).round() as i32;
    let y = layout.top_px.clamp(0, (sh - h).max(0));
    (x, y, w, h)
}
fn place_panel(
    window: &ApplicationWindow,
    area: &DrawingArea,
    layout: &PanelLayout,
    expanded: (i32, i32),
) {
    let (x, y, w, h) = panel_geometry(layout, panel_bounds(window), expanded);
    window.set_anchor(Edge::Top, true);
    window.set_anchor(Edge::Bottom, false);
    window.set_anchor(Edge::Left, true);
    window.set_anchor(Edge::Right, false);
    window.set_exclusive_zone(-1);
    window.set_margin(Edge::Left, x);
    window.set_margin(Edge::Right, 0);
    window.set_margin(Edge::Top, y);
    window.set_margin(Edge::Bottom, 0);
    area.set_content_width(w);
    area.set_content_height(h);
    window.set_default_size(w, h);
    area.queue_draw();
}
fn panel_input_region(window: &ApplicationWindow, width: i32) {
    if let Some(surface) = window.surface() {
        // Only the grip and collapse button receive clicks. Text and audio
        // feedback remain transparent to the application underneath.
        let region = Region::create_rectangle(&RectangleInt::new(0, 0, 28, 28));
        let _ = region.union_rectangle(&RectangleInt::new((width - 32).max(0), 0, 32, 28));
        surface.set_input_region(Some(&region));
    }
}
/// Absolute cursor coordinates do not change when the layer surface moves.
/// GTK drag offsets are surface-relative and feed our own movement back into
/// the next event, producing oscillation/half-speed movement on Wayland.
fn hypr_cursor_position() -> Option<(f64, f64)> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    let socket = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?)
        .join("hypr")
        .join(std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?)
        .join(".socket.sock");
    let mut connection = UnixStream::connect(socket).ok()?;
    connection
        .set_read_timeout(Some(Duration::from_millis(5)))
        .ok()?;
    connection
        .set_write_timeout(Some(Duration::from_millis(5)))
        .ok()?;
    connection.write_all(b"j/cursorpos").ok()?;
    let mut reply = Vec::new();
    let mut block = [0u8; 128];
    while reply.len() < 512 {
        let n = connection.read(&mut block).ok()?;
        if n == 0 {
            break;
        }
        reply.extend_from_slice(&block[..n]);
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&reply) {
            return Some((value.get("x")?.as_f64()?, value.get("y")?.as_f64()?));
        }
    }
    None
}

#[derive(Clone, Copy)]
struct PanelDrag {
    panel: (i32, i32),
    cursor: (f64, f64),
}

fn panel_controls(
    window: &ApplicationWindow,
    area: &DrawingArea,
    layout: Rc<RefCell<PanelLayout>>,
    expanded: (i32, i32),
) {
    panel_controls_with_cursor(
        window,
        area,
        layout,
        expanded,
        Rc::new(hypr_cursor_position),
    );
}

fn panel_controls_with_cursor(
    window: &ApplicationWindow,
    area: &DrawingArea,
    layout: Rc<RefCell<PanelLayout>>,
    expanded: (i32, i32),
    cursor_position: Rc<dyn Fn() -> Option<(f64, f64)>>,
) {
    place_panel(window, area, &layout.borrow(), expanded);
    let w = window.clone();
    area.connect_resize(move |_, width, _| panel_input_region(&w, width));
    let click = gtk4::GestureClick::new();
    let (w, a, l) = (window.clone(), area.clone(), layout.clone());
    click.connect_released(move |_, _, x, y| {
        if y <= 28.0 && x >= f64::from(a.width() - 32) {
            let mut l = l.borrow_mut();
            l.collapsed = !l.collapsed;
            place_panel(&w, &a, &l, expanded);
            save_layout(&l);
        }
    });
    area.add_controller(click);
    let drag = gtk4::GestureDrag::new();
    let origin = Rc::new(Cell::new(None::<PanelDrag>));
    let read_cursor = cursor_position.clone();
    let (w, l, o) = (window.clone(), layout.clone(), origin.clone());
    drag.connect_drag_begin(move |g, x, y| {
        if x > 28.0 || y > 28.0 {
            g.set_state(gtk4::EventSequenceState::Denied);
            return;
        }
        let Some(cursor) = read_cursor() else {
            g.set_state(gtk4::EventSequenceState::Denied);
            return;
        };
        let (x, y, _, _) = panel_geometry(&l.borrow(), panel_bounds(&w), expanded);
        o.set(Some(PanelDrag {
            panel: (x, y),
            cursor,
        }));
    });
    let (w, a, l, o) = (window.clone(), area.clone(), layout.clone(), origin);
    drag.connect_drag_update(move |_, _surface_dx, _surface_dy| {
        let Some(anchor) = o.get() else {
            return;
        };
        let Some(cursor) = cursor_position() else {
            return;
        };
        let dx = cursor.0 - anchor.cursor.0;
        let dy = cursor.1 - anchor.cursor.1;
        let screen = panel_bounds(&w);
        let (_, _, width, _) = panel_geometry(&l.borrow(), screen, expanded);
        let (x, y) = anchor.panel;
        let mut l = l.borrow_mut();
        l.left_fraction =
            ((f64::from(x) + dx) / f64::from((screen.0 - width).max(1))).clamp(0.0, 1.0);
        l.top_px = (f64::from(y) + dy).round() as i32;
        place_panel(&w, &a, &l, expanded);
    });
    drag.connect_drag_end(move |_, _, _| save_layout(&layout.borrow()));
    area.add_controller(drag);
}

/// State shared between the IPC worker and the GTK redraw timer.
struct SharedState {
    ring: Mutex<FrameRing>,
    peak: Mutex<PeakHold>,
    last_seq: Mutex<u64>,
    last_frame_at: Mutex<Instant>,
    /// Latest transcript published by the daemon. `None` means no
    /// streaming session is live (so text mode shows its listening
    /// state); `Some("")` means a session is open but nothing has
    /// decoded yet.
    preview: Mutex<Option<String>>,
    /// Transient one-line message overlaid on the transcript panel, e.g.
    /// the armed-cancel prompt. `None` when nothing is armed.
    hint: Mutex<Option<String>>,
    /// When the current session started, for the elapsed clock. A long
    /// dictation has no other way to tell you it is still running.
    session_started: Mutex<Option<Instant>>,
}

impl SharedState {
    fn new(decay_db_per_sec: f32) -> Self {
        Self {
            ring: Mutex::new(FrameRing::new(DEFAULT_RING_DEPTH)),
            peak: Mutex::new(PeakHold::new(decay_db_per_sec)),
            last_seq: Mutex::new(0),
            last_frame_at: Mutex::new(Instant::now() - Duration::from_secs(3600)),
            preview: Mutex::new(None),
            hint: Mutex::new(None),
            session_started: Mutex::new(None),
        }
    }
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let socket_path = resolve_socket_path(args.socket.clone());

    // Layer config: defaults < config file [osd] < CLI/env overrides.
    let mut osd_cfg = load_osd_config_from_file(args.config.as_deref());
    if let Some(w) = args.width_px {
        osd_cfg.width_px = w;
    }
    if let Some(h) = args.height_px {
        osd_cfg.height_px = h;
    }
    if let Some(m) = args.margin_px {
        osd_cfg.margin_px = m;
    }
    if let Some(g) = args.waveform_gain {
        osd_cfg.waveform_gain = g;
    }
    apply_text_mode_sizing(
        &mut osd_cfg,
        args.width_px.is_some(),
        args.height_px.is_some(),
    );

    // peak_decay_db_per_sec has a clap default value, so this always
    // overrides whatever the file said. That's intentional: if the user
    // passes the flag, honor it; if they don't, the clap default kicks in.
    osd_cfg.peak_decay_db_per_sec = args.peak_decay_db_per_sec;

    tracing::info!(
        "voxtype-osd-gtk4 starting; socket={:?} size={}x{} margin={} pos={:?}",
        socket_path,
        osd_cfg.width_px,
        osd_cfg.height_px,
        osd_cfg.margin_px,
        osd_cfg.position,
    );

    let theme = ThemeWatcher::new();
    let palette = theme.palette();

    let state = Arc::new(SharedState::new(osd_cfg.peak_decay_db_per_sec));

    // Spawn the tokio IPC worker on a side thread.
    spawn_ipc_worker(
        state.clone(),
        socket_path,
        args.reconnect_secs,
        args.log_every,
    );

    // GTK application owns the main thread.
    let app = Application::builder().application_id(APP_ID).build();

    let cfg = osd_cfg.clone();
    let state_for_activate = state.clone();
    app.connect_activate(move |app| {
        build_window(app, &cfg, palette, state_for_activate.clone());
    });

    // GTK's run() consumes argv; we've already parsed via clap, so feed
    // it an empty vector to keep it from re-parsing.
    let exit = app.run_with_args::<&str>(&[]);
    let code: u8 = exit.into();
    if code != 0 {
        anyhow::bail!("GTK application exited with status {}", code);
    }
    Ok(())
}

/// Give `visual_mode = "text"` a surface tall enough to be worth reading.
///
/// The shared defaults (400x48) are sized for a waveform strip. Text mode
/// on that surface shows a single clipped line, which is useless, so when
/// the user has expressed no size preference we substitute panel
/// proportions. "No preference" means the value still equals the shared
/// default *and* no CLI flag was passed — an explicit `width_px = 400` in
/// config is indistinguishable from the default here, which is the one
/// wrinkle of this heuristic and why the override flags are consulted too.
fn apply_text_mode_sizing(cfg: &mut OsdConfig, width_from_cli: bool, height_from_cli: bool) {
    if cfg.visual_mode != OsdVisualMode::Text {
        return;
    }
    let defaults = OsdConfig::default();
    if !width_from_cli && cfg.width_px == defaults.width_px {
        cfg.width_px = TEXT_MODE_WIDTH;
    }
    if !height_from_cli && cfg.height_px == defaults.height_px {
        cfg.height_px = TEXT_MODE_HEIGHT;
    }
}

/// Spawn the tokio runtime + IPC loop on a dedicated thread.
fn spawn_ipc_worker(
    state: Arc<SharedState>,
    socket_path: PathBuf,
    reconnect_secs: f32,
    log_every: u32,
) {
    std::thread::Builder::new()
        .name("voxtype-osd-ipc".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!("Failed to build tokio runtime: {e}");
                    return;
                }
            };

            let dt_per_frame = 1.0 / FRAME_HZ as f32;
            let mut total: u64 = 0;
            let mut last_log = Instant::now();

            let on_frame = move |frame: AudioFrame| {
                if let Ok(mut r) = state.ring.lock() {
                    r.push(frame);
                }
                if let Ok(mut p) = state.peak.lock() {
                    p.update(frame.peak_dbfs, dt_per_frame);
                }
                if let Ok(mut s) = state.last_seq.lock() {
                    *s = s.wrapping_add(1);
                }
                if let Ok(mut t) = state.last_frame_at.lock() {
                    *t = Instant::now();
                }

                total += 1;
                if log_every > 0 && total.is_multiple_of(u64::from(log_every)) {
                    let elapsed = last_log.elapsed().as_secs_f32();
                    let rate = if elapsed > 0.0 {
                        log_every as f32 / elapsed
                    } else {
                        0.0
                    };
                    tracing::debug!(
                        target: "osd::frames",
                        frontend = "gtk4",
                        seq = frame.seq,
                        peak_dbfs = frame.peak_dbfs,
                        min = frame.min,
                        max = frame.max,
                        rate_hz = rate,
                        "frame batch"
                    );
                    last_log = Instant::now();
                }
            };

            rt.block_on(run_ipc_loop(socket_path, reconnect_secs, on_frame));
        })
        .expect("spawn ipc worker thread");
}

/// Best-effort monitor height in physical pixels for translating the
/// fractional `top_margin` config into a layer-shell pixel offset.
///
/// Tries the GDK display's currently-focused monitor first (which is the
/// monitor the user is most likely looking at, and the one swayosd targets
/// via `--monitor`). If that fails — display unavailable, no monitors
/// enumerated — returns None and the caller falls back to a conservative
/// default.
fn focused_monitor_height_px() -> Option<i32> {
    use gtk4::gdk;
    let display = gdk::Display::default()?;
    let monitors = display.monitors();
    // `monitors` is a GListModel; pull the first item with a non-zero
    // height. On multi-monitor setups this picks whichever the compositor
    // ordered first, which lines up with the layer-shell default in
    // practice.
    for i in 0..monitors.n_items() {
        if let Some(obj) = monitors.item(i) {
            if let Ok(monitor) = obj.downcast::<gdk::Monitor>() {
                let h = monitor.geometry().height();
                if h > 0 {
                    return Some(h);
                }
            }
        }
    }
    None
}

/// Build the GTK window, attach layer-shell config, mount the DrawingArea,
/// and start the redraw tick.
fn build_window(app: &Application, cfg: &OsdConfig, palette: Palette, state: Arc<SharedState>) {
    let window = ApplicationWindow::builder()
        .application(app)
        .default_width(cfg.width_px as i32)
        .default_height(cfg.height_px as i32)
        .resizable(false)
        .decorated(false)
        .build();

    window.add_css_class("voxtype-osd");
    let css = gtk4::CssProvider::new();
    css.load_from_data("window.voxtype-osd { background: transparent; box-shadow: none; }");
    gtk4::style_context_add_provider_for_display(
        &gtk4::prelude::WidgetExt::display(&window),
        &css,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    // Layer-shell setup: top layer, no keyboard, anchored per config.
    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    window.set_keyboard_mode(KeyboardMode::None);
    window.set_namespace(Some("voxtype-osd"));

    // Centered positions anchor to a single horizontal edge and leave
    // Left/Right unanchored, which makes the layer shell center the
    // surface horizontally.
    //
    // `bottom-center` keeps swayosd-style *fractional* placement: it
    // anchors to Edge::Top at `top_margin × monitor_height` so the panel
    // lands in the same band as volume/brightness/media-key feedback.
    //
    // `top-center` must anchor to Edge::Top with the absolute `margin_px`
    // instead. Routing it through the fractional path (as every release
    // through 0.7.5 did) offsets it by 0.85 × screen height, putting it
    // at the *bottom* of the display — visually identical to
    // `bottom-center`, which is what made the setting look broken.
    // Set by the arms below when the surface is anchored to an edge with no
    // margin, i.e. it physically touches that edge of the display.
    // Every arm below assigns flush_top; only the corner arm can be flush
    // against the bottom, so that one keeps a default.
    let flush_top;
    let mut flush_bottom = false;

    match cfg.position {
        OsdPosition::BottomCenter => {
            // Resolve monitor height to translate the fractional offset
            // into pixels. Falls back to a conservative 1080 if the
            // display can't be queried (extremely rare on Wayland-only
            // systems where layer-shell is supported at all).
            let monitor_height = focused_monitor_height_px().unwrap_or(1080);
            let top_px = (cfg.top_margin.clamp(0.0, 1.0) * monitor_height as f32) as i32;

            window.set_anchor(Edge::Top, true);
            window.set_anchor(Edge::Bottom, false);
            window.set_anchor(Edge::Left, false);
            window.set_anchor(Edge::Right, false);
            window.set_margin(Edge::Top, top_px);
            flush_top = top_px == 0;
        }
        OsdPosition::TopCenter => {
            window.set_anchor(Edge::Top, true);
            window.set_anchor(Edge::Bottom, false);
            window.set_anchor(Edge::Left, false);
            window.set_anchor(Edge::Right, false);
            window.set_margin(Edge::Top, cfg.margin_px as i32);
            flush_top = cfg.margin_px == 0;
        }
        _ => {
            // Corner positions: legacy anchor + uniform pixel margin.
            let (anchor_top, anchor_bottom, anchor_left, anchor_right) = match cfg.position {
                OsdPosition::BottomLeft => (false, true, true, false),
                OsdPosition::BottomRight => (false, true, false, true),
                OsdPosition::TopLeft => (true, false, true, false),
                OsdPosition::TopRight => (true, false, false, true),
                // Centered variants are handled by the arms above.
                OsdPosition::BottomCenter | OsdPosition::TopCenter => unreachable!(),
            };
            window.set_anchor(Edge::Top, anchor_top);
            window.set_anchor(Edge::Bottom, anchor_bottom);
            window.set_anchor(Edge::Left, anchor_left);
            window.set_anchor(Edge::Right, anchor_right);

            let m = cfg.margin_px as i32;
            flush_top = anchor_top && m == 0;
            flush_bottom = anchor_bottom && m == 0;
            if anchor_top {
                window.set_margin(Edge::Top, m);
            }
            if anchor_bottom {
                window.set_margin(Edge::Bottom, m);
            }
            if anchor_left {
                window.set_margin(Edge::Left, m);
            }
            if anchor_right {
                window.set_margin(Edge::Right, m);
            }
        }
    }

    // Exclusive zone. We never reserve space for ourselves — the OSD floats
    // over windows — but the two non-reserving values differ in a way that
    // matters here:
    //
    //   0  don't reserve, but stay clear of everyone else's reserved strips
    //  -1  don't reserve, and ignore theirs too
    //
    // At 0 a top-anchored surface with margin 0 lands *below* the Omarchy
    // bar, because the compositor honours the bar's exclusive zone — which
    // looks like the margin was ignored. A flush panel is meant to read as
    // part of the bezel, so it has to overlap the bar: -1 when flush, 0
    // otherwise so ordinary margins still measure from the usable area.
    let flush = flush_top || flush_bottom;
    window.set_exclusive_zone(if flush { -1 } else { 0 });

    // The drawing area fills the window.
    let drawing_area = DrawingArea::new();
    drawing_area.set_content_width(cfg.width_px as i32);
    drawing_area.set_content_height(cfg.height_px as i32);
    let state_for_draw = state.clone();
    let gain = cfg.waveform_gain as f64;
    let visual_mode = cfg.visual_mode;
    let panel_layout = Rc::new(RefCell::new(load_layout()));
    let draw_layout = panel_layout.clone();
    drawing_area.set_draw_func(move |_area, cr, w, h| {
        draw(
            cr,
            w,
            h,
            &palette,
            &state_for_draw,
            gain,
            (visual_mode, draw_layout.borrow().collapsed),
        );
    });
    window.set_child(Some(&drawing_area));

    // Click-through: install an empty input region on the window's surface
    // once it's realized. Until then `realize` hasn't allocated a surface.
    {
        let window_ref = window.clone();
        window.connect_realize(move |_| {
            if visual_mode == OsdVisualMode::Text {
                panel_input_region(&window_ref, window_ref.width());
            } else {
                apply_click_through(&window_ref);
            }
        });
    }

    if visual_mode == OsdVisualMode::Text {
        panel_controls(
            &window,
            &drawing_area,
            panel_layout.clone(),
            (cfg.width_px as i32, cfg.height_px as i32),
        );
    }

    // Redraw timer. We only call queue_draw() when the IPC has produced a
    // newer seq than the last paint, so this is cheap when idle.
    let redraw_state = state.clone();
    let redraw_area = drawing_area.clone();
    let redraw_window = window.clone();
    let last_drawn_seq = Cell::new(0u64);
    let show_layout = panel_layout.clone();
    let expanded_size = (cfg.width_px as i32, cfg.height_px as i32);
    let last_bounds = Cell::new(panel_bounds(&window));
    let bounds_checked = Cell::new(Instant::now());
    // Preview polling state. The file is only consulted in text mode, and
    // only while frames are flowing, so an idle daemon costs nothing.
    let preview_mode = cfg.visual_mode == OsdVisualMode::Text;
    let preview_path = default_preview_path();
    let hint_path = default_hint_path();
    let last_preview_poll = Cell::new(Instant::now() - Duration::from_secs(3600));
    let preview_dirty = Cell::new(false);
    // True while the daemon reports a live streaming session (preview file
    // present). Only meaningful in text mode; waveform mode keeps the
    // original frames-only gating.
    let session_active = Cell::new(false);
    // Tracks GTK visibility. Starts true because `window.present()` below maps
    // the surface, the first tick's idle check then hides it.
    let visible = Cell::new(true);

    glib::timeout_add_local(Duration::from_millis(RENDER_TICK_MS as u64), move || {
        if preview_mode && bounds_checked.get().elapsed() >= Duration::from_millis(500) {
            bounds_checked.set(Instant::now());
            let bounds = panel_bounds(&redraw_window);
            if bounds != last_bounds.replace(bounds) {
                place_panel(
                    &redraw_window,
                    &redraw_area,
                    &show_layout.borrow(),
                    expanded_size,
                );
            }
        }
        let cur_seq = redraw_state.last_seq.lock().map(|s| *s).unwrap_or(0);
        let last_at = redraw_state
            .last_frame_at
            .lock()
            .map(|t| *t)
            .unwrap_or_else(|_| Instant::now() - Duration::from_secs(3600));
        // Poll the preview file before the idle gate. Its presence marks an
        // active session, and the daemon publishes it the moment the hotkey
        // fires — before the first audio frame, since opening the capture
        // device costs a few hundred milliseconds. Gating the surface on
        // frames alone put the panel on screen ~0.6 s after the keypress.
        if preview_mode
            && last_preview_poll.get().elapsed() >= Duration::from_millis(PREVIEW_POLL_MS)
        {
            last_preview_poll.set(Instant::now());
            let latest = read_preview(&preview_path);
            let was_active = session_active.get();
            session_active.set(latest.is_some());
            // Stamp the session clock on the edges only, so the elapsed
            // readout survives the transcript going quiet mid-sentence.
            if was_active != session_active.get() {
                if let Ok(mut started) = redraw_state.session_started.lock() {
                    *started = session_active.get().then(Instant::now);
                }
                preview_dirty.set(true);
            }
            if let Ok(mut slot) = redraw_state.preview.lock() {
                if *slot != latest {
                    *slot = latest;
                    preview_dirty.set(true);
                }
            }
            // Hints expire on their own deadline, so this poll is also
            // what takes an elapsed one off screen.
            let hint = read_hint(&hint_path);
            if let Ok(mut slot) = redraw_state.hint.lock() {
                if *slot != hint {
                    *slot = hint;
                    preview_dirty.set(true);
                }
            }
        }

        // A live session holds the surface up even with no frames yet.
        let starved = last_at.elapsed().as_secs_f32() > IDLE_TIMEOUT_SECS;
        let idle = (starved && !session_active.get()) || osd_suppressed();

        if idle {
            if visible.get() {
                tracing::info!("hiding (idle for {:.2}s)", last_at.elapsed().as_secs_f32());
                redraw_window.set_visible(false);
                visible.set(false);
            }
            return glib::ControlFlow::Continue;
        }

        if !visible.get() {
            if preview_mode {
                place_panel(
                    &redraw_window,
                    &redraw_area,
                    &show_layout.borrow(),
                    expanded_size,
                );
            }
            tracing::info!(
                "showing (frame seq={}, last_at={:.3}s ago)",
                cur_seq,
                last_at.elapsed().as_secs_f32()
            );
            redraw_window.set_visible(true);
            visible.set(true);
        }

        // Decay the held peak even when no new frame arrived this tick.
        if let Ok(mut p) = redraw_state.peak.lock() {
            let dt = (RENDER_TICK_MS as f32) / 1000.0;
            // We pass the most recent peak from the ring as the "current"
            // value so a stale update doesn't snap the held value back up.
            let cur_peak = redraw_state
                .ring
                .lock()
                .ok()
                .and_then(|r| r.latest())
                .map(|f| f.peak_dbfs)
                .unwrap_or(-120.0);
            // Only decay; the IPC callback already snapped up on each
            // received frame. Calling update here with a non-louder peak
            // keeps the linear decay running at render rate.
            if cur_peak <= p.held_dbfs {
                p.update(cur_peak, dt);
            }
        }

        // Repaint on a new audio frame *or* on new transcript text. The
        // seq check alone would be enough while the mic is live, but the
        // preview also changes during the post-mic drain, when frames are
        // synthetic silence at a fixed seq cadence.
        if cur_seq != last_drawn_seq.get() || preview_dirty.get() {
            redraw_area.queue_draw();
            last_drawn_seq.set(cur_seq);
            preview_dirty.set(false);
        }
        glib::ControlFlow::Continue
    });

    // Map the layer-shell surface once. The redraw timer will hide it
    // immediately on its first tick (no frames yet → idle), and toggle
    // visibility from there. Mapping once at startup keeps Hyprland's
    // layer-shell state machine happy across hide/show cycles.
    window.present();
}

/// Set an empty input region on the GdkSurface so clicks pass through.
fn apply_click_through(window: &ApplicationWindow) {
    let Some(surface) = window.surface() else {
        tracing::warn!("Window has no surface yet; click-through not applied");
        return;
    };
    let empty = Region::create_rectangle(&RectangleInt::new(0, 0, 0, 0));
    surface.set_input_region(Some(&empty));
}

/// Render the waveform + peak meter into the given Cairo context.
fn draw(
    cr: &Context,
    width: i32,
    height: i32,
    palette: &Palette,
    state: &Arc<SharedState>,
    gain: f64,
    presentation: (OsdVisualMode, bool),
) {
    let (mode, collapsed) = presentation;
    let w = width as f64;
    let h = height as f64;
    if w <= 0.0 || h <= 0.0 {
        return;
    }

    let bg = palette.background;
    match mode {
        OsdVisualMode::Waveform => {
            // Square-cornered fill across the whole surface, as shipped.
            cr.set_source_rgba(bg.r as f64, bg.g as f64, bg.b as f64, bg.a as f64);
            cr.set_operator(cairo::Operator::Source);
            cr.paint().ok();
            cr.set_operator(cairo::Operator::Over);

            // Layout: waveform area on the left (~92% width), gap (1%),
            // then peak meter on the right (~7% width).
            let meter_width = (w * 0.07).max(8.0);
            let gap = (w * 0.01).max(2.0);
            let wave_width = (w - meter_width - gap).max(0.0);

            draw_waveform(cr, 0.0, 0.0, wave_width, h, palette, state, gain);
            draw_peak_meter(cr, wave_width + gap, 0.0, meter_width, h, palette, state);
        }
        OsdVisualMode::Text => {
            cr.set_operator(cairo::Operator::Source);
            cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);
            cr.paint().ok();
            cr.set_operator(cairo::Operator::Over);
            rounded_rect(cr, 0.0, 0.0, w, h, PANEL_RADIUS);
            cr.set_source_rgba(bg.r as f64, bg.g as f64, bg.b as f64, bg.a as f64);
            cr.fill().ok();
            rounded_rect(cr, 0.5, 0.5, w - 1.0, h - 1.0, PANEL_RADIUS);
            cr.set_source_rgba(
                palette.foreground.r as f64,
                palette.foreground.g as f64,
                palette.foreground.b as f64,
                0.16,
            );
            cr.set_line_width(1.0);
            cr.stroke().ok();
            draw_text_preview(cr, w, h, palette, state, collapsed);
        }
        OsdVisualMode::Pill => {
            // A pill keeps its capsule silhouette — that is the whole
            // point of the mode. Clear to fully transparent first so the
            // corners read against whatever is behind the surface.
            cr.set_source_rgba(0.0, 0.0, 0.0, 0.0);
            cr.set_operator(cairo::Operator::Source);
            cr.paint().ok();
            cr.set_operator(cairo::Operator::Over);

            rounded_rect(cr, 0.0, 0.0, w, h, PANEL_RADIUS);
            cr.set_source_rgba(bg.r as f64, bg.g as f64, bg.b as f64, bg.a as f64);
            cr.fill().ok();

            // Keep every later stroke inside the rounded silhouette.
            rounded_rect(cr, 0.0, 0.0, w, h, PANEL_RADIUS);
            cr.clip();

            draw_pill(cr, 0.0, 0.0, w, h, palette, state);
        }
    }
}

/// Append a rounded-rectangle sub-path to the current path.
fn rounded_rect(cr: &Context, x: f64, y: f64, w: f64, h: f64, radius: f64) {
    use std::f64::consts::{FRAC_PI_2, PI};
    let r = radius.min(w / 2.0).min(h / 2.0).max(0.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, FRAC_PI_2, PI);
    cr.arc(x + r, y + r, r, PI, PI + FRAC_PI_2);
    cr.close_path();
}

/// Current meter fill, 0.0..=1.0, from the held peak.
fn current_level(state: &Arc<SharedState>) -> f64 {
    let held = state.peak.lock().map(|p| p.held_dbfs).unwrap_or(-120.0);
    peak_meter_fraction(held, METER_FLOOR_DBFS) as f64
}

/// Break `text` into lines that fit `max_w` at the context's current font.
///
/// Greedy word wrap. A single word wider than the whole line — rare from
/// ASR but not impossible — is hard-split by characters rather than
/// allowed to run off the panel.
fn wrap_text(cr: &Context, text: &str, max_w: f64) -> Vec<String> {
    let advance = |s: &str| cr.text_extents(s).map(|e| e.x_advance()).unwrap_or(0.0);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();

    for word in text.split_whitespace() {
        let candidate = if cur.is_empty() {
            word.to_string()
        } else {
            format!("{} {}", cur, word)
        };
        if advance(&candidate) <= max_w {
            cur = candidate;
            continue;
        }
        if !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
        }
        // `word` alone on a fresh line: split it only if it still
        // overflows on its own.
        if advance(word) <= max_w {
            cur = word.to_string();
        } else {
            let mut piece = String::new();
            for ch in word.chars() {
                let mut trial = piece.clone();
                trial.push(ch);
                if !piece.is_empty() && advance(&trial) > max_w {
                    lines.push(std::mem::take(&mut piece));
                }
                piece.push(ch);
            }
            cur = piece;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Upper bound on transcript characters handed to [`wrap_text`].
///
/// Only the last few lines are ever drawn, but wrapping measures every
/// word through cairo — so wrapping the whole transcript makes each frame
/// cost O(everything said so far). At 60 fps a half-hour dictation turns
/// that into a busy loop. 4000 characters is an order of magnitude more
/// than the tallest panel can show, and bounds the per-frame work.
const WRAP_TAIL_CHARS: usize = 4000;

/// The trailing slice of `text` worth wrapping: at most `max_chars`
/// characters, cut at a word boundary so the first visible line does not
/// start mid-word.
fn wrap_tail(text: &str, max_chars: usize) -> &str {
    if text.chars().count() <= max_chars {
        return text;
    }
    // Byte offset of the character `max_chars` back from the end.
    let cut = text
        .char_indices()
        .nth(text.chars().count() - max_chars)
        .map(|(i, _)| i)
        .unwrap_or(0);
    let tail = &text[cut..];
    match tail.find(char::is_whitespace) {
        Some(i) => tail[i..].trim_start(),
        None => tail,
    }
}

/// Elapsed time as `M:SS`, or `H:MM:SS` once past an hour.
fn format_elapsed(d: Duration) -> String {
    let total = d.as_secs();
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{}:{:02}:{:02}", h, m, s)
    } else {
        format!("{}:{:02}", m, s)
    }
}

/// VoiceInk-style live transcript panel.
///
/// Shows the tail of the decoded text — the most recent lines — with a
/// state dot and an elapsed clock in the left gutter. Older visible lines
/// fade slightly so the eye lands on the newest words. Before anything
/// decodes, the panel shows a listening line over the amplitude pill so
/// the user still gets mic feedback. An armed hint, when present, takes
/// the bottom line.
fn draw_text_preview(
    cr: &Context,
    w: f64,
    h: f64,
    palette: &Palette,
    state: &Arc<SharedState>,
    collapsed: bool,
) {
    let fg = palette.foreground;
    let accent = palette.accent;
    cr.select_font_face(
        "sans-serif",
        cairo::FontSlant::Normal,
        cairo::FontWeight::Normal,
    );
    cr.set_source_rgba(fg.r as f64, fg.g as f64, fg.b as f64, 0.65);
    for x in [10.0, 15.0] {
        for y in [10.0, 15.0, 20.0] {
            cr.arc(x, y, 1.0, 0.0, std::f64::consts::TAU);
            cr.fill().ok();
        }
    }
    cr.set_font_size(12.0);
    cr.move_to(32.0, 20.0);
    let cancel_hint = state.hint.lock().ok().and_then(|h| h.clone());
    let armed_collapsed = collapsed && cancel_hint.is_some();
    if armed_collapsed {
        cr.show_text("Ctrl+Esc again").ok();
    } else if let Some(started) = state.session_started.lock().ok().and_then(|t| *t) {
        cr.show_text(&format_elapsed(started.elapsed())).ok();
    }
    let level = state
        .ring
        .lock()
        .ok()
        .and_then(|r| r.latest())
        .map(|f| peak_meter_fraction(f.peak_dbfs, METER_FLOOR_DBFS) as f64)
        .unwrap_or(0.0);
    cr.set_source_rgba(accent.r as f64, accent.g as f64, accent.b as f64, 0.95);
    for (i, scale) in [0.45, 0.8, 1.0, 0.8, 0.45].iter().enumerate() {
        let bar_h = 3.0 + 13.0 * level * scale;
        rounded_rect(
            cr,
            w - 77.0 + i as f64 * 6.0,
            16.0 - bar_h / 2.0,
            3.0,
            bar_h,
            1.5,
        );
        cr.fill().ok();
    }
    cr.set_source_rgba(fg.r as f64, fg.g as f64, fg.b as f64, 0.8);
    cr.set_line_width(1.5);
    cr.move_to(w - 23.0, 15.0);
    cr.line_to(w - 13.0, 15.0);
    if collapsed {
        cr.move_to(w - 18.0, 10.0);
        cr.line_to(w - 18.0, 20.0);
    }
    cr.stroke().ok();
    if collapsed {
        return;
    }
    cr.set_font_size(PREVIEW_FONT_SIZE);
    let preview = state
        .preview
        .lock()
        .ok()
        .and_then(|p| p.clone())
        .unwrap_or_default();
    let text = if preview.trim().is_empty() {
        "Listening…"
    } else {
        preview.trim()
    };
    let lines = wrap_text(
        cr,
        wrap_tail(text, WRAP_TAIL_CHARS),
        (w - 2.0 * PANEL_PAD_X).max(1.0),
    );
    let visible = &lines[lines.len().saturating_sub(2)..];
    let hint = state.hint.lock().ok().and_then(|h| h.clone());
    for (i, line) in visible.iter().enumerate() {
        if hint.is_some() && i == 1 {
            break;
        }
        cr.set_source_rgba(
            fg.r as f64,
            fg.g as f64,
            fg.b as f64,
            if i + 1 == visible.len() { 1.0 } else { 0.65 },
        );
        cr.move_to(
            PANEL_PAD_X,
            45.0 + i as f64 * PREVIEW_FONT_SIZE * PREVIEW_LINE_FACTOR,
        );
        cr.show_text(line).ok();
    }
    if let Some(hint) = hint {
        cr.set_font_size(12.0);
        cr.set_source_rgba(accent.r as f64, accent.g as f64, accent.b as f64, 1.0);
        cr.move_to(PANEL_PAD_X, h - 12.0);
        cr.show_text(&hint).ok();
    }
}

/// Thin amplitude capsule filling the panel — the waveform-free fallback
/// for engines that cannot stream a transcript.
fn draw_pill(
    cr: &Context,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    palette: &Palette,
    state: &Arc<SharedState>,
) {
    let pill_h = (h * 0.22).clamp(5.0, 12.0);
    let pill_w = (w - PANEL_PAD_X * 2.0).max(1.0);
    let pill_x = x + PANEL_PAD_X;
    let pill_y = y + (h - pill_h) / 2.0;
    draw_pill_bar(
        cr,
        pill_x,
        pill_y,
        pill_w,
        pill_h,
        palette,
        current_level(state),
    );
}

/// Draw one capsule: a dim track with an accent-filled leading portion.
fn draw_pill_bar(cr: &Context, x: f64, y: f64, w: f64, h: f64, palette: &Palette, level: f64) {
    let fg = palette.foreground;
    let accent = palette.accent;
    let r = h / 2.0;

    rounded_rect(cr, x, y, w, h, r);
    cr.set_source_rgba(fg.r as f64, fg.g as f64, fg.b as f64, 0.16);
    cr.fill().ok();

    // A hair of fill always shows so the capsule reads as "live" rather
    // than "broken" during a pause in speech.
    let fill_w = (w * level.clamp(0.0, 1.0)).max(h.min(w));
    cr.save().ok();
    rounded_rect(cr, x, y, w, h, r);
    cr.clip();
    rounded_rect(cr, x, y, fill_w, h, r);
    cr.set_source_rgba(accent.r as f64, accent.g as f64, accent.b as f64, 0.95);
    cr.fill().ok();
    cr.restore().ok();
}

fn draw_waveform(
    cr: &Context,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    palette: &Palette,
    state: &Arc<SharedState>,
    gain: f64,
) {
    if w < 1.0 {
        return;
    }
    let n_columns = w.floor() as usize;
    if n_columns == 0 {
        return;
    }

    // Collect frames as a Vec snapshot under the lock, then drop.
    let frames: Vec<AudioFrame> = match state.ring.lock() {
        Ok(r) => r.iter().collect(),
        Err(_) => return,
    };
    let cols = project_envelope(&frames, n_columns);

    let mid = y + h * 0.5;
    let half = h * 0.5;

    // Mirrored envelope filled polygon. We trace the top edge left-to-right
    // following `max`, then the bottom edge right-to-left following `min`.
    cr.set_source_rgba(
        palette.accent.r as f64,
        palette.accent.g as f64,
        palette.accent.b as f64,
        palette.accent.a as f64,
    );

    cr.new_path();
    // Top edge.
    for (i, col) in cols.iter().enumerate() {
        let px = x + i as f64 + 0.5;
        let py = mid - sample_to_pixels(col.max, half, gain);
        if i == 0 {
            cr.move_to(px, py);
        } else {
            cr.line_to(px, py);
        }
    }
    // Bottom edge, right-to-left.
    for (i, col) in cols.iter().enumerate().rev() {
        let px = x + i as f64 + 0.5;
        let py = mid - sample_to_pixels(col.min, half, gain);
        cr.line_to(px, py);
    }
    cr.close_path();
    cr.fill().ok();

    // Subtle centerline.
    cr.set_source_rgba(
        palette.foreground.r as f64,
        palette.foreground.g as f64,
        palette.foreground.b as f64,
        0.15,
    );
    cr.set_line_width(1.0);
    cr.move_to(x, mid);
    cr.line_to(x + w, mid);
    cr.stroke().ok();
}

fn sample_to_pixels(sample: f32, half_height: f64, gain: f64) -> f64 {
    // Apply visual gain, then clamp to -1.0..=1.0, then scale to half_height.
    let s = (sample as f64 * gain).clamp(-1.0, 1.0);
    s * half_height
}

fn draw_peak_meter(
    cr: &Context,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    palette: &Palette,
    state: &Arc<SharedState>,
) {
    if w < 1.0 || h < 1.0 {
        return;
    }

    let (latest_peak, held_peak) = {
        let latest = state
            .ring
            .lock()
            .ok()
            .and_then(|r| r.latest())
            .map(|f| f.peak_dbfs)
            .unwrap_or(f32::NEG_INFINITY);
        let held = state
            .peak
            .lock()
            .map(|p| p.held_dbfs)
            .unwrap_or(f32::NEG_INFINITY);
        (latest, held)
    };

    let fill_frac = peak_meter_fraction(latest_peak, METER_FLOOR_DBFS) as f64;

    let segments = METER_SEGMENTS;
    let gap = 1.5_f64;
    let total_gap = gap * (segments as f64 - 1.0);
    let seg_h = ((h - total_gap) / segments as f64).max(1.0);

    for i in 0..segments {
        // Segment 0 is the bottom of the bar.
        let frac_top = (i as f64 + 1.0) / segments as f64;
        let lit = fill_frac >= (i as f64 + 0.5) / segments as f64;
        // dBFS at the *top* of this segment for color zone classification.
        let seg_top_db = METER_FLOOR_DBFS + (frac_top as f32) * (-METER_FLOOR_DBFS);
        let zone = MeterZone::from_dbfs(seg_top_db);
        let zone_color = zone.color(palette);

        let py = y + h - (i as f64 + 1.0) * seg_h - i as f64 * gap;

        if lit {
            cr.set_source_rgba(
                zone_color.r as f64,
                zone_color.g as f64,
                zone_color.b as f64,
                zone_color.a as f64,
            );
        } else {
            cr.set_source_rgba(
                zone_color.r as f64,
                zone_color.g as f64,
                zone_color.b as f64,
                0.18,
            );
        }
        cr.rectangle(x, py, w, seg_h);
        cr.fill().ok();
    }

    // Held-peak tick (1.5 px line at the held position).
    if held_peak.is_finite() && held_peak > METER_FLOOR_DBFS {
        let held_frac = peak_meter_fraction(held_peak, METER_FLOOR_DBFS) as f64;
        let py = y + h - held_frac * h;
        cr.set_source_rgba(
            palette.foreground.r as f64,
            palette.foreground.g as f64,
            palette.foreground.b as f64,
            0.95,
        );
        cr.set_line_width(1.5);
        cr.move_to(x, py);
        cr.line_to(x + w, py);
        cr.stroke().ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires a Wayland session; use an isolated XDG_STATE_HOME"]
    fn gtk_collapse_and_drag_controls_update_layout() {
        gtk4::init().unwrap();
        let app = Application::builder()
            .application_id("io.voxtype.PanelControlsTest")
            .flags(gtk4::gio::ApplicationFlags::NON_UNIQUE)
            .build();
        app.register(None::<&gtk4::gio::Cancellable>).unwrap();
        let window = ApplicationWindow::builder().application(&app).build();
        window.init_layer_shell();
        let area = DrawingArea::new();
        window.set_child(Some(&area));
        let layout = Rc::new(RefCell::new(PanelLayout::default()));
        let cursor = Rc::new(Cell::new((1000.0, 400.0)));
        let simulated_cursor = cursor.clone();
        panel_controls_with_cursor(
            &window,
            &area,
            layout.clone(),
            (480, 82),
            Rc::new(move || Some(simulated_cursor.get())),
        );
        area.allocate(480, 82, -1, None);
        let controllers = area.observe_controllers();
        for i in 0..controllers.n_items() {
            if let Some(click) = controllers.item(i).and_downcast::<gtk4::GestureClick>() {
                click.emit_by_name::<()>("released", &[&1i32, &462.0f64, &15.0f64]);
            }
        }
        assert!(layout.borrow().collapsed);
        for i in 0..controllers.n_items() {
            if let Some(drag) = controllers.item(i).and_downcast::<gtk4::GestureDrag>() {
                drag.emit_by_name::<()>("drag-begin", &[&12.0f64, &12.0f64]);
                cursor.set((1100.0, 480.0));
                drag.emit_by_name::<()>("drag-update", &[&100.0f64, &80.0f64]);
                assert_eq!(layout.borrow().top_px, 112);
                let first_x = layout.borrow().left_fraction;
                // The panel moved beneath the pointer, changing local
                // coordinates: the global cursor still moves another 20px.
                cursor.set((1120.0, 500.0));
                drag.emit_by_name::<()>("drag-update", &[&20.0f64, &20.0f64]);
                assert_eq!(layout.borrow().top_px, 132);
                assert!(layout.borrow().left_fraction > first_x);
                // Moving the surface with the mouse stationary must not
                // move the panel back or keep chasing stale local offsets.
                let stable_x = layout.borrow().left_fraction;
                drag.emit_by_name::<()>("drag-update", &[&0.0f64, &0.0f64]);
                assert_eq!(layout.borrow().top_px, 132);
                assert_eq!(layout.borrow().left_fraction, stable_x);
                drag.emit_by_name::<()>("drag-end", &[&0.0f64, &0.0f64]);
            }
        }
        assert_eq!(layout.borrow().top_px, 132);
        assert!(layout.borrow().left_fraction > 0.5);
        assert!(load_layout().collapsed);
        assert_eq!(load_layout().top_px, 132);
        window.close();
    }

    #[test]
    fn dragged_layout_stays_visible_after_undocking() {
        let layout = PanelLayout {
            left_fraction: 0.98,
            top_px: 1800,
            collapsed: false,
        };
        let (x, y, w, h) = panel_geometry(&layout, (1280, 800), (480, 82));
        assert!(x >= 0 && y >= 0 && x + w <= 1280 && y + h <= 800);
        let layout = PanelLayout {
            collapsed: true,
            ..layout
        };
        let (_, _, w, h) = panel_geometry(&layout, (1280, 800), (480, 82));
        assert_eq!((w, h), (240, 34));
    }

    #[test]
    fn compact_panel_renders_transparent_corners_and_plain_transcript() {
        let state = Arc::new(SharedState::new(6.0));
        *state.preview.lock().unwrap()=Some("This is a longer dictation preview. Only the two most recent lines stay visible while you use the rest of the desktop. <b>Speech remains plain text.</b>".into());
        *state.session_started.lock().unwrap() = Some(Instant::now() - Duration::from_secs(3725));
        state.ring.lock().unwrap().push(AudioFrame {
            seq: 1,
            min: -0.3,
            max: 0.3,
            peak_dbfs: -12.0,
        });
        for (name, w, h, collapsed) in [("expanded", 480, 82, false), ("collapsed", 240, 34, true)]
        {
            let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).unwrap();
            let cr = Context::new(&surface).unwrap();
            draw(
                &cr,
                w,
                h,
                &Palette::default(),
                &state,
                10.0,
                (OsdVisualMode::Text, collapsed),
            );
            drop(cr);
            surface.flush();
            let data = surface.data().unwrap();
            assert_eq!(data[3], 0, "corner must be transparent");
            if let Ok(dir) = std::env::var("VOXTYPE_TEST_RENDER_DIR") {
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(PathBuf::from(dir).join(format!("{name}.argb")), &*data).unwrap();
            }
        }
    }

    #[test]
    fn wrap_tail_returns_short_text_untouched() {
        assert_eq!(wrap_tail("hello world", 4000), "hello world");
    }

    #[test]
    fn wrap_tail_cuts_at_a_word_boundary() {
        // 40 characters, cut to at most 12: the slice starts at a word,
        // never mid-word.
        let text = "alpha bravo charlie delta echo foxtrot";
        let tail = wrap_tail(text, 12);
        assert!(text.ends_with(tail), "{tail:?} must be a suffix");
        assert!(tail.chars().count() <= 12);
        assert!(!tail.starts_with(char::is_whitespace));
        assert!(text.split_whitespace().any(|w| tail.starts_with(w)));
    }

    #[test]
    fn wrap_tail_survives_multibyte_text() {
        let text = "ααα βββ γγγ δδδ εεε";
        let tail = wrap_tail(text, 7);
        assert!(text.ends_with(tail));
        assert!(tail.chars().count() <= 7);
    }

    #[test]
    fn wrap_tail_falls_back_to_a_hard_cut_without_whitespace() {
        let text = "abcdefghijklmnop";
        assert_eq!(wrap_tail(text, 4).chars().count(), 4);
    }

    #[test]
    fn elapsed_formats_minutes_then_hours() {
        assert_eq!(format_elapsed(Duration::from_secs(0)), "0:00");
        assert_eq!(format_elapsed(Duration::from_secs(9)), "0:09");
        assert_eq!(format_elapsed(Duration::from_secs(75)), "1:15");
        assert_eq!(format_elapsed(Duration::from_secs(1800)), "30:00");
        assert_eq!(format_elapsed(Duration::from_secs(3725)), "1:02:05");
    }
}
