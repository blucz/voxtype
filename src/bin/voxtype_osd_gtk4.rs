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

use std::cell::Cell;
use std::path::PathBuf;
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
use voxtype::osd::preview::{default_preview_path, read_preview, PREVIEW_POLL_MS};
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

/// Number of segments in the vertical peak meter.
const METER_SEGMENTS: usize = 10;

/// dBFS floor for the peak meter (maps to "empty bar").
const METER_FLOOR_DBFS: f32 = -60.0;

/// Preview text point size, and the line box as a multiple of it.
const PREVIEW_FONT_SIZE: f64 = 15.0;
const PREVIEW_LINE_FACTOR: f64 = 1.38;

/// Inner padding of the text/pill panels, in pixels.
const PANEL_PAD_X: f64 = 16.0;
const PANEL_PAD_Y: f64 = 12.0;

/// Corner radius for the text/pill panels.
const PANEL_RADIUS: f64 = 12.0;

/// Surface size used by `visual_mode = "text"` when the user has not
/// chosen one. A 400x48 strip fits a waveform but only one short line of
/// transcript, so text mode substitutes a panel proportioned for a few
/// lines. An explicit width/height in config or on the CLI still wins —
/// see `apply_text_mode_sizing`.
const TEXT_MODE_WIDTH: u32 = 560;
const TEXT_MODE_HEIGHT: u32 = 132;

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
}

impl SharedState {
    fn new(decay_db_per_sec: f32) -> Self {
        Self {
            ring: Mutex::new(FrameRing::new(DEFAULT_RING_DEPTH)),
            peak: Mutex::new(PeakHold::new(decay_db_per_sec)),
            last_seq: Mutex::new(0),
            last_frame_at: Mutex::new(Instant::now() - Duration::from_secs(3600)),
            preview: Mutex::new(None),
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
    drawing_area.set_draw_func(move |_area, cr, w, h| {
        draw(cr, w, h, &palette, &state_for_draw, gain, visual_mode);
    });
    window.set_child(Some(&drawing_area));

    // Click-through: install an empty input region on the window's surface
    // once it's realized. Until then `realize` hasn't allocated a surface.
    {
        let window_ref = window.clone();
        window.connect_realize(move |_| {
            apply_click_through(&window_ref);
        });
    }

    // Redraw timer. We only call queue_draw() when the IPC has produced a
    // newer seq than the last paint, so this is cheap when idle.
    let redraw_state = state.clone();
    let redraw_area = drawing_area.clone();
    let redraw_window = window.clone();
    let last_drawn_seq = Cell::new(0u64);
    // Preview polling state. The file is only consulted in text mode, and
    // only while frames are flowing, so an idle daemon costs nothing.
    let preview_mode = cfg.visual_mode == OsdVisualMode::Text;
    let preview_path = default_preview_path();
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
            session_active.set(latest.is_some());
            if let Ok(mut slot) = redraw_state.preview.lock() {
                if *slot != latest {
                    *slot = latest;
                    preview_dirty.set(true);
                }
            }
        }

        // A live session holds the surface up even with no frames yet.
        let starved = last_at.elapsed().as_secs_f32() > IDLE_TIMEOUT_SECS;
        let idle = starved && !session_active.get();

        if idle {
            if visible.get() {
                tracing::info!("hiding (idle for {:.2}s)", last_at.elapsed().as_secs_f32());
                redraw_window.set_visible(false);
                visible.set(false);
            }
            return glib::ControlFlow::Continue;
        }

        if !visible.get() {
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
    mode: OsdVisualMode,
) {
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
            // Square-cornered fill across the whole surface. Omarchy is
            // crisp-cornered throughout, and rounding this panel while it
            // is pinned to the top edge left slivers of desktop showing at
            // the lower corners, so the notch is drawn as a plain rect.
            cr.set_source_rgba(bg.r as f64, bg.g as f64, bg.b as f64, bg.a as f64);
            cr.set_operator(cairo::Operator::Source);
            cr.paint().ok();
            cr.set_operator(cairo::Operator::Over);

            draw_text_preview(cr, w, h, palette, state);
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

/// VoiceInk-style live transcript panel.
///
/// Shows the tail of the decoded text — the most recent lines — with a
/// state dot on the left. Older visible lines fade slightly so the eye
/// lands on the newest words. Before anything decodes, the panel shows a
/// listening line over the amplitude pill so the user still gets mic
/// feedback.
fn draw_text_preview(cr: &Context, w: f64, h: f64, palette: &Palette, state: &Arc<SharedState>) {
    let fg = palette.foreground;
    let accent = palette.accent;

    cr.select_font_face(
        "sans-serif",
        cairo::FontSlant::Normal,
        cairo::FontWeight::Normal,
    );
    cr.set_font_size(PREVIEW_FONT_SIZE);

    // Status dot, sized by live amplitude so the panel still breathes
    // with the voice without a scrolling waveform.
    let level = current_level(state);
    let dot_cx = PANEL_PAD_X + 5.0;
    let dot_cy = PANEL_PAD_Y + PREVIEW_FONT_SIZE * 0.55;
    let dot_r = 3.5 + level * 2.5;
    cr.set_source_rgba(accent.r as f64, accent.g as f64, accent.b as f64, 0.95);
    cr.arc(dot_cx, dot_cy, dot_r, 0.0, std::f64::consts::PI * 2.0);
    cr.fill().ok();

    let text_x = dot_cx + 6.0 + 10.0;
    let max_w = (w - text_x - PANEL_PAD_X).max(1.0);
    if max_w < 1.0 {
        return;
    }

    let preview = state.preview.lock().ok().and_then(|p| p.clone());
    let text = preview.unwrap_or_default();
    let trimmed = text.trim();

    if trimmed.is_empty() {
        // Nothing decoded yet (or no streaming session). Show a quiet
        // placeholder plus the pill, so the surface is never a blank box.
        cr.set_source_rgba(fg.r as f64, fg.g as f64, fg.b as f64, 0.45);
        cr.move_to(text_x, PANEL_PAD_Y + PREVIEW_FONT_SIZE);
        cr.show_text("Listening…").ok();

        let pill_h = 6.0;
        let pill_y = h - PANEL_PAD_Y - pill_h;
        draw_pill_bar(cr, text_x, pill_y, max_w, pill_h, palette, level);
        return;
    }

    let line_h = PREVIEW_FONT_SIZE * PREVIEW_LINE_FACTOR;
    let avail_h = h - PANEL_PAD_Y * 2.0;
    let max_lines = ((avail_h / line_h).floor() as usize).max(1);

    let lines = wrap_text(cr, trimmed, max_w);
    let start = lines.len().saturating_sub(max_lines);
    let visible = &lines[start..];

    for (i, line) in visible.iter().enumerate() {
        // Ramp older visible lines down toward 0.55 alpha; the newest
        // line always renders at full strength.
        let alpha = if visible.len() > 1 {
            0.55 + 0.45 * (i as f64 / (visible.len() - 1) as f64)
        } else {
            1.0
        };
        cr.set_source_rgba(fg.r as f64, fg.g as f64, fg.b as f64, alpha);
        let baseline = PANEL_PAD_Y + PREVIEW_FONT_SIZE + line_h * i as f64;
        cr.move_to(text_x, baseline);
        cr.show_text(line).ok();

        // Caret after the final line: this text is still being revised.
        if i + 1 == visible.len() {
            let adv = cr.text_extents(line).map(|e| e.x_advance()).unwrap_or(0.0);
            let caret_x = text_x + adv + 3.0;
            if caret_x < w - PANEL_PAD_X {
                cr.set_source_rgba(accent.r as f64, accent.g as f64, accent.b as f64, 0.85);
                cr.rectangle(
                    caret_x,
                    baseline - PREVIEW_FONT_SIZE * 0.78,
                    2.0,
                    PREVIEW_FONT_SIZE * 0.92,
                );
                cr.fill().ok();
            }
        }
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
