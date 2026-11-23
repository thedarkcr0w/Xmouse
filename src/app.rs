//! The native window (egui): sign-in, consoles, play session, keys, mouse, test, settings.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{
    vec2, Color32, CornerRadius, Margin, RichText, Stroke, TextureHandle, TextureOptions, Ui,
    ViewportCommand,
};

use crate::auth::{Auth, DeviceCode};
use crate::glpipe::{Look, Pipeline, Source};
use crate::pacing::{
    nominal_fps, Arrival, Cadence, DupDetector, Kind, Pacer, Show, Signature, Skip, StreamStats,
};
use crate::mapper::{suggest_sensitivity, Mapper, PadFrame};
use crate::padview::pad_view;
use crate::profile::{
    input_label, Action, BindMode, Binding, DeadzoneShape, Layer, MouseSettings, Profile, Stick,
};
use crate::storage::{PlayMode, Settings, Storage};
use crate::stream::{run_session, Phase, Shared, StreamConfig, VideoFrame};
use crate::theme::{self, *};
use crate::xhome::{wake_console, ConsoleInfo, XHome};

const KEY_RELEASE: egui::Key = egui::Key::F8;
const KEY_FINDER: egui::Key = egui::Key::F6;
const KEY_HUD: egui::Key = egui::Key::F7;
const KEY_FULLSCREEN: egui::Key = egui::Key::F11;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Page {
    Home,
    More,
    Consoles,
    PressOnce,
    Profiles,
    Keys,
    Mouse,
    Test,
    Picture,
    Setup,
}

impl Page {
    /// How deep in the app a page is: Play, the pages one step from it, the pages under More.
    fn depth(self) -> i32 {
        match self {
            Page::Home => 0,
            Page::More | Page::Consoles => 1,
            Page::Test => 3,
            _ => 2,
        }
    }
}

/// What the window is showing. A change of view is what the page transition animates.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum View {
    Platform,
    Account,
    SignIn,
    Page(Page),
    /// A session: 0 connecting, 1 ready or paused, 2 in control.
    Session(u8),
}

/// Which way the new view comes in: sideways for going deeper (+1) or back (-1), a small
/// rise (0) for everything else.
fn transition_direction(from: View, to: View) -> f32 {
    match (from, to) {
        (View::Page(a), View::Page(b)) => (b.depth() - a.depth()).signum() as f32,
        _ => 0.0,
    }
}

/// Page changes: remembers the view and the back button's label, and when each last changed.
struct Nav {
    view: Option<View>,
    since: f64,
    dir: f32,
    /// The view changed this frame: its scroll area starts at the top.
    fresh: bool,
    back: Option<&'static str>,
    back_since: f64,
}

impl Nav {
    fn new() -> Self {
        Self {
            view: None,
            since: f64::NEG_INFINITY,
            dir: 0.0,
            fresh: false,
            back: None,
            back_since: f64::NEG_INFINITY,
        }
    }

    /// Say what this frame shows. Returns (how far the view has come in, 0..=1; direction).
    fn show(&mut self, ctx: &egui::Context, view: View) -> (f32, f32) {
        let now = ctx.input(|i| i.time);
        match self.view {
            Some(v) if v == view => {}
            // The first view of a run just appears.
            None => self.view = Some(view),
            Some(old) => {
                self.dir = transition_direction(old, view);
                self.view = Some(view);
                self.since = now;
                self.fresh = true;
            }
        }
        (Self::progress(ctx, now, self.since), self.dir)
    }

    /// How far the back button has faded in since it appeared or its label changed.
    fn back_fade(&mut self, ctx: &egui::Context, back: Option<&'static str>) -> f32 {
        let now = ctx.input(|i| i.time);
        if back != self.back {
            // Only a button that is there fades in; a view without one has nothing to show.
            if back.is_some() && self.view.is_some() && self.since > f64::NEG_INFINITY {
                self.back_since = now;
            }
            self.back = back;
        }
        Self::progress(ctx, now, self.back_since)
    }

    fn progress(ctx: &egui::Context, now: f64, since: f64) -> f32 {
        let t = ((now - since) / motion::SLOW as f64).clamp(0.0, 1.0) as f32;
        if t < 1.0 {
            ctx.request_repaint_after(motion::FRAME);
        }
        motion::ease_out(t)
    }
}

/// Lay out a view's content while it comes in: faded and a few points off its place.
/// Always goes through the same child `Ui`, so widget ids don't change when it has arrived.
fn entering<R>(ui: &mut Ui, (t, dir): (f32, f32), add: impl FnOnce(&mut Ui) -> R) -> R {
    let off = (1.0 - t)
        * if dir == 0.0 {
            vec2(0.0, 10.0)
        } else {
            vec2(18.0 * dir, 0.0)
        };
    let rect = ui.available_rect_before_wrap().translate(off);
    ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
        if t < 1.0 {
            // The fade finishes a little before the slide does.
            ui.set_opacity((t * 1.4).min(1.0));
        }
        add(ui)
    })
    .inner
}

/// Results of background work, written by async tasks and read by the UI.
#[derive(Default)]
struct Bg {
    signed_in: bool,
    auth_busy: bool,
    auth_error: Option<String>,
    device_code: Option<DeviceCode>,
    consoles: Option<Result<Vec<ConsoleInfo>, String>>,
    consoles_loading: bool,
    /// Consoles offered by plugins (listed after the Xbox ones), and which need pairing first.
    plugin_consoles: Vec<ConsoleInfo>,
    needs_pairing: std::collections::HashSet<String>,
    /// Plugins are still being asked for their consoles / have answered at least once.
    plugins_loading: bool,
    plugins_loaded: bool,
    /// Outcome of the last pairing attempt, picked up by the pairing window.
    pair_result: Option<Result<(), String>>,
    /// PSNBridge is being downloaded and installed / the last attempt's error / it finished
    /// and the plugins need scanning again.
    installing: bool,
    install_error: Option<String>,
    install_done: bool,
    toasts: Vec<(String, bool)>,
}

/// The PIN window for pairing a plugin console.
struct Pairing {
    console: ConsoleInfo,
    pin: String,
    busy: bool,
    error: Option<String>,
}

/// Plugin consoles live in the same console list as Xbox ones, with an id that says which
/// plugin owns them, so picking, "last console" and --play work unchanged.
const PLUGIN_PREFIX: &str = "plugin:";

fn plugin_console_key(plugin: &str, console: &str) -> String {
    format!("{PLUGIN_PREFIX}{plugin}:{console}")
}

/// (plugin id, the plugin's own console id) for a console that comes from a plugin.
fn plugin_console_ref(server_id: &str) -> Option<(&str, &str)> {
    server_id.strip_prefix(PLUGIN_PREFIX)?.split_once(':')
}


struct Session {
    paused: bool,
    shared: Arc<Shared>,
    console: ConsoleInfo,
    mode: PlayMode,
    captured: bool,
    show_hud: bool,
    texture: Option<TextureHandle>,
    /// The picture when it comes from the GPU (RTX Video Super Resolution or our own
    /// passes): the egui texture and the part of it holding the picture.
    vsr_tex: Option<(egui::TextureId, egui::Vec2)>,
    /// Picture size in physical pixels in the player window (what the GPU scales to).
    vsr_target: [u32; 2],
    /// Size of the decoded stream.
    video_size: [u32; 2],
    /// The picture's rectangle in the player window, in real pixels (x, y, width, height).
    fg_rect: [i32; 4],
    mods: egui::Modifiers,
    /// When the picture first appeared (the "game ignoring your keys?" tip shows for a while).
    tip_since: Option<Instant>,
    /// The overlay's key list for a layer: (layer it was built for, its rows).
    legend: (Option<String>, Arc<Vec<(String, String)>>),
    /// Frame timing and the state of the picture pipeline for this session.
    pic: PictureState,
}

/// Which way the PC Play picture takes to the screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Route {
    /// As it always was: RTX Video upscaling, or an egui texture scaled by egui.
    Plain,
    /// RTX Video upscaling, then sharpening / colour in our own passes.
    VsrPost,
    /// Everything in our own passes (`glpipe`).
    Pipeline,
}

#[derive(Default)]
struct PictureState {
    /// Zero of the frame clock (arrival times are seconds since then).
    epoch: Option<Instant>,
    detector: DupDetector,
    cadence: Cadence,
    pacer: Pacer,
    stats: StreamStats,
    /// (route, Smooth motion on) the state below belongs to; a change starts it over.
    mode: Option<(Route, bool)>,
    /// The in-between frame is on screen right now (else the real one).
    showing_mid: bool,
    /// The longest a real frame went on screen after its planned time, since the last log
    /// line (seconds).
    late: f64,
    /// RTX Video's last output for the passes after it: (UV size, pixel size), and a
    /// counter that changes with every new one.
    vsr_raw: Option<(egui::Vec2, [u32; 2])>,
    vsr_version: u64,
    /// The player window's centre in screen pixels (to find the display it is on).
    player_center: Option<[i32; 2]>,
    /// Refresh rate of that display, and when it was last looked up.
    display: Option<(f64, Instant)>,
    /// Test runs (XMOUSE_PICTURE_DUMP): pictures written so far, and the last one's version.
    dumped: (u32, u64),
}

/// The picture settings as the pipeline wants them.
fn picture_look(s: &Settings) -> Look {
    Look {
        cleanup: s
            .pc_cleanup
            .then_some(if s.pc_cleanup_strong { 1.0 } else { 0.0 }),
        sharpen: s.pc_sharpen.then_some(s.pc_sharpen_amount),
        colour: s
            .pc_colour
            .then_some([s.pc_vibrance, s.pc_contrast, s.pc_brightness]),
        edge_upscale: s.pc_sharp_upscale,
        smooth: s.pc_smooth_scaling,
    }
}

/// Refresh rate of the display the player window is on (the main one if unknown).
fn display_hz(center: Option<[i32; 2]>) -> f64 {
    // For testing the 60 Hz rules on a faster screen.
    static FORCED: std::sync::OnceLock<Option<f64>> = std::sync::OnceLock::new();
    if let Some(hz) = FORCED.get_or_init(|| {
        std::env::var("XMOUSE_DISPLAY_HZ")
            .ok()
            .and_then(|v| v.parse().ok())
    }) {
        return *hz;
    }
    #[cfg(windows)]
    {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::POINT;
        use windows::Win32::Graphics::Gdi::{
            EnumDisplaySettingsW, GetMonitorInfoW, MonitorFromPoint, DEVMODEW,
            ENUM_CURRENT_SETTINGS, MONITORINFO, MONITORINFOEXW, MONITOR_DEFAULTTONEAREST,
        };
        unsafe {
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            let mut found = false;
            if let Some([x, y]) = center {
                let monitor = MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST);
                found = GetMonitorInfoW(monitor, &mut info as *mut _ as *mut MONITORINFO).as_bool();
            }
            let device = if found {
                PCWSTR(info.szDevice.as_ptr())
            } else {
                PCWSTR::null()
            };
            let mut mode = DEVMODEW {
                dmSize: std::mem::size_of::<DEVMODEW>() as u16,
                ..Default::default()
            };
            if EnumDisplaySettingsW(device, ENUM_CURRENT_SETTINGS, &mut mode).as_bool()
                && mode.dmDisplayFrequency > 1
            {
                return mode.dmDisplayFrequency as f64;
            }
        }
    }
    let _ = center;
    60.0
}

/// Test runs: write an RGBA picture (rows top first) as a 24-bit BMP.
fn write_bmp(path: &std::path::Path, w: u32, h: u32, rgba: &[u8]) {
    let row = (w as usize * 3).div_ceil(4) * 4;
    let mut out = Vec::with_capacity(54 + row * h as usize);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((54 + row * h as usize) as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    // (negative height: rows are stored top first)
    out.extend_from_slice(&(-(h as i32)).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&[0; 24]);
    for line in rgba.chunks_exact(w as usize * 4) {
        let start = out.len();
        for px in line.chunks_exact(4) {
            out.extend_from_slice(&[px[2], px[1], px[0]]);
        }
        out.resize(start + row, 0);
    }
    let _ = std::fs::write(path, out);
}

struct TestPad {
    mapper: Mapper,
    captured: bool,
    mods: egui::Modifiers,
    recording: Option<(Instant, Vec<f32>)>,
    result: String,
}

#[derive(Default)]
struct Editor {
    layer: usize,
    /// Keys page: list instead of the drawn keyboard.
    list_view: bool,
    /// Keys page: key selected on the drawn keyboard.
    key_sel: Option<String>,
    search: String,
    more_mouse: bool,
    selected: Option<usize>,
    listening: Option<(usize, usize, u64)>,
    new_layer: String,
    dirty_since: Option<Instant>,
    test: Option<TestPad>,
}

pub struct XMouseApp {
    rt: tokio::runtime::Runtime,
    auth: Arc<Auth>,
    storage: Storage,
    settings: Settings,
    profiles: Vec<Profile>,
    current: usize,
    page: Page,
    bg: Arc<Mutex<Bg>>,
    session: Option<Session>,
    editor: Editor,
    toasts: Vec<(String, bool, Instant)>,
    /// --play: start the last console in the last mode as soon as consoles are known.
    play_on_start: bool,
    /// Installed console plugins (other platforms), found at startup.
    plugins: Vec<crate::plugin::PluginInfo>,
    /// The PIN window, while pairing a plugin console.
    pairing: Option<Pairing>,
    ctx: egui::Context,
    frame_no: u64,
    /// Page transitions.
    nav: Nav,
    /// When the last frame started (outside a session, frames are paced; see `ui`).
    frame_start: Instant,
    /// Fake consoles for screenshots / UI testing (XMOUSE_DEMO=1).
    demo: bool,
    came_from_keys: bool,
    /// GPU upscaler (RTX Video Super Resolution), created on first use.
    #[cfg(windows)]
    vsr: Option<crate::vsr::Vsr>,
    /// The GPU path failed once: stay on CPU scaling for this run.
    vsr_failed: bool,
    /// Crash guard: (processor generation being watched, good frames since it started).
    vsr_watch: (u64, u32),
    /// The guard file is on disk.
    vsr_guard_armed: bool,
    /// Output size the guard last saw (a change means a new processor setup).
    vsr_guard_target: [u32; 2],
    /// Our own picture passes (clean-up, in-between frames, upscaling, sharpening, colour),
    /// created on first use.
    pipe: Option<Pipeline>,
    /// They failed once on this graphics driver: show the plain picture for this run.
    pipe_failed: bool,
    /// The frame-generation picture surface (FSR / DLSS), while a session uses one.
    #[cfg(windows)]
    fg: Option<crate::framegen::Surface>,
    /// It failed in this session: stay on the normal picture until the next one.
    fg_failed: bool,
    /// Its guard file is on disk.
    fg_guard_armed: bool,
    /// The note about NVIDIA's files is showing, waiting for a yes.
    fg_confirm_dlss: bool,
    /// (pictures in, frames out) at the last log line.
    fg_seen: [u64; 2],
}

pub struct Options {
    /// Start playing right away (last console, last mode), e.g. from a desktop shortcut.
    pub play: bool,
}

impl XMouseApp {
    pub fn new(cc: &eframe::CreationContext<'_>, opts: Options) -> Self {
        crate::init_crypto();
        theme::install(&cc.egui_ctx);
        let storage = Storage::new(Storage::default_dir());
        let auth = Arc::new(Auth::new(storage.token_file()));
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("tokio runtime");
        let demo = std::env::var_os("XMOUSE_DEMO").is_some();
        let signed_in = demo || rt.block_on(auth.is_signed_in());
        let profiles = storage.profiles();
        let mut settings = storage.settings();
        // The graphics driver crashed while RTX Video was upscaling last time: switch it off
        // instead of crashing again on the next fullscreen.
        let mut startup_toasts = vec![];
        // The same for frame generation: its guard file is still there when XMouse or the
        // graphics driver went down in a session's first moments with it.
        if storage.framegen_guard_file().exists() {
            let _ = std::fs::remove_file(storage.framegen_guard_file());
            if settings.pc_fsr_fg || settings.pc_dlss_fg {
                settings.pc_fsr_fg = false;
                settings.pc_dlss_fg = false;
                storage.save_settings(&settings);
                log::warn!("frame generation turned off: the last run crashed with it");
                startup_toasts.push((
                    "Frame generation was switched off because XMouse or the graphics driver crashed with it last time. You can turn it back on under Picture and connection.".to_string(),
                    true,
                    Instant::now(),
                ));
            }
        }
        if storage.vsr_guard_file().exists() {
            let _ = std::fs::remove_file(storage.vsr_guard_file());
            if settings.pc_rtx_vsr {
                settings.pc_rtx_vsr = false;
                storage.save_settings(&settings);
                log::warn!("RTX Video upscaling turned off: the driver crashed during it last run");
                startup_toasts.push((
                    "RTX Video upscaling was switched off because the graphics driver crashed with it last time. You can turn it back on under Picture and connection.".to_string(),
                    true,
                    Instant::now(),
                ));
            }
        }
        let plugins = crate::plugin::discover();
        // Someone already playing on Xbox (signed in, no console plugins) keeps going straight
        // to their Xbox after updating; new users and plugin users pick their console first.
        if settings.console_platform.is_empty() && signed_in && !demo && plugins.is_empty() {
            settings.console_platform = "xbox".into();
            storage.save_settings(&settings);
        }
        let current = profiles
            .iter()
            .position(|p| p.id == settings.last_profile)
            .unwrap_or(0);
        let start_page = match std::env::var("XMOUSE_PAGE").ok().as_deref() {
            Some("profiles") => Page::Profiles,
            Some("more") => Page::More,
            Some("consoles") => Page::Consoles,
            Some("pressonce") => Page::PressOnce,
            Some("keys") => Page::Keys,
            Some("mouse") => Page::Mouse,
            Some("test") => Page::Test,
            Some("picture") => Page::Picture,
            Some("setup") => Page::Setup,
            _ => Page::Home,
        };
        let app = Self {
            rt,
            auth,
            settings,
            profiles,
            current,
            storage,
            page: if demo { start_page } else { Page::Home },
            bg: Arc::new(Mutex::new(Bg {
                signed_in,
                ..Default::default()
            })),
            session: None,
            editor: Editor {
                selected: Some(0),
                ..Default::default()
            },
            toasts: startup_toasts,
            play_on_start: opts.play,
            plugins,
            pairing: None,
            ctx: cc.egui_ctx.clone(),
            frame_no: 0,
            nav: Nav::new(),
            frame_start: Instant::now(),
            demo,
            came_from_keys: false,
            #[cfg(windows)]
            vsr: None,
            vsr_failed: false,
            vsr_watch: (0, 0),
            vsr_guard_armed: false,
            vsr_guard_target: [0, 0],
            pipe: None,
            pipe_failed: false,
            #[cfg(windows)]
            fg: None,
            fg_failed: false,
            fg_guard_armed: false,
            fg_confirm_dlss: false,
            fg_seen: [0; 2],
        };
        app.apply_platform_names();
        app
    }

    fn toast(&mut self, msg: impl Into<String>, error: bool) {
        self.toasts.push((msg.into(), error, Instant::now()));
    }

    fn save_settings(&self) {
        self.storage.save_settings(&self.settings);
    }

    fn profile(&self) -> &Profile {
        &self.profiles[self.current.min(self.profiles.len() - 1)]
    }

    // ------------------------------------------------------------------ background work

    fn sign_in(&self) {
        let (auth, bg, ctx) = (self.auth.clone(), self.bg.clone(), self.ctx.clone());
        {
            let mut b = bg.lock().unwrap();
            b.auth_busy = true;
            b.auth_error = None;
        }
        self.rt.spawn(async move {
            let res = async {
                let code = auth.start_device_code().await?;
                bg.lock().unwrap().device_code = Some(code.clone());
                ctx.request_repaint();
                auth.wait_for_device_code(&code).await?;
                auth.stream_token().await?;
                anyhow::Ok(())
            }
            .await;
            let mut b = bg.lock().unwrap();
            b.auth_busy = false;
            b.device_code = None;
            match res {
                Ok(()) => {
                    b.signed_in = true;
                    b.consoles = None;
                    b.toasts.push(("Signed in".into(), false));
                }
                Err(e) => b.auth_error = Some(format!("{e:#}")),
            }
            ctx.request_repaint();
        });
    }

    fn load_consoles(&self) {
        // Another console type: only its plugin knows the consoles.
        if self.platform_plugin().is_some() {
            self.load_plugin_consoles();
            return;
        }
        if self.demo {
            let c = |id: &str, name: &str, t: &str, p: &str| ConsoleInfo {
                server_id: id.into(),
                name: name.into(),
                console_type: t.into(),
                power_state: p.into(),
                out_of_home: false,
            };
            self.bg.lock().unwrap().consoles = Some(Ok(vec![
                c(
                    "F4000000DEMO01",
                    "Living Room Xbox",
                    "XboxSeriesX",
                    "ConnectedStandby",
                ),
                c("F4000000DEMO02", "Bedroom", "XboxSeriesS", "On"),
            ]));
            return;
        }
        let (auth, bg, ctx) = (self.auth.clone(), self.bg.clone(), self.ctx.clone());
        bg.lock().unwrap().consoles_loading = true;
        self.rt.spawn(async move {
            let res = async {
                let t = auth.stream_token().await?;
                XHome::new(&t.host, &t.token).consoles().await
            }
            .await;
            let mut b = bg.lock().unwrap();
            b.consoles_loading = false;
            b.consoles = Some(res.map_err(|e| format!("{e:#}")));
            ctx.request_repaint();
        });
    }

    /// Ask every installed plugin for its consoles (each runs once, in the background).
    fn load_plugin_consoles(&self) {
        if self.plugins.is_empty() {
            return;
        }
        let (plugins, bg, ctx) = (self.plugins.clone(), self.bg.clone(), self.ctx.clone());
        {
            // One lookup at a time: "Look for consoles again" clicked twice must not start
            // the plugin twice.
            let mut b = bg.lock().unwrap();
            if b.plugins_loading {
                return;
            }
            b.plugins_loading = true;
        }
        self.rt.spawn_blocking(move || {
            let mut list = Vec::new();
            let mut needs_pairing = std::collections::HashSet::new();
            let mut errors = Vec::new();
            for p in &plugins {
                match crate::plugin::list_consoles(p) {
                    Ok(consoles) => {
                        for c in consoles {
                            let key = plugin_console_key(&p.id, &c.id);
                            if c.needs_pairing {
                                needs_pairing.insert(key.clone());
                            }
                            list.push(ConsoleInfo {
                                server_id: key,
                                name: c.name,
                                console_type: if c.model.is_empty() {
                                    p.platform.clone()
                                } else {
                                    c.model
                                },
                                power_state: match c.state {
                                    crate::plugin::ConsoleState::On => "On",
                                    crate::plugin::ConsoleState::Standby => "ConnectedStandby",
                                    crate::plugin::ConsoleState::Off => "Off",
                                    crate::plugin::ConsoleState::Unknown => "Unknown",
                                }
                                .into(),
                                out_of_home: false,
                            });
                        }
                    }
                    Err(e) => {
                        log::warn!("plugin {}: {e:#}", p.id);
                        errors.push(format!("{} plugin: {e:#}", p.name));
                    }
                }
            }
            let mut b = bg.lock().unwrap();
            b.plugin_consoles = list;
            b.needs_pairing = needs_pairing;
            b.plugins_loading = false;
            b.plugins_loaded = true;
            for e in errors {
                b.toasts.push((e, true));
            }
            ctx.request_repaint();
        });
    }

    /// The PIN window for a plugin console that isn't paired yet. Pairing runs in the
    /// background; on success the console starts right away.
    fn pairing_window(&mut self, ctx: &egui::Context) {
        if self.pairing.is_none() {
            return;
        }
        let finished = if self.pairing.as_ref().is_some_and(|p| p.busy) {
            self.bg.lock().unwrap().pair_result.take()
        } else {
            None
        };
        match finished {
            Some(Ok(())) => {
                let p = self.pairing.take().unwrap();
                self.bg.lock().unwrap().needs_pairing.remove(&p.console.server_id);
                self.toast(format!("Paired with {}", p.console.name), false);
                self.load_plugin_consoles();
                let mode = self.settings.last_mode;
                self.start_session(p.console, mode);
                return;
            }
            Some(Err(e)) => {
                if let Some(p) = self.pairing.as_mut() {
                    p.busy = false;
                    p.error = Some(e);
                }
            }
            None => {}
        }
        let mut pair = false;
        let mut cancel = false;
        let p = self.pairing.as_mut().unwrap();
        egui::Window::new("Pair a console")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.set_width(380.0);
                ui.label(
                    RichText::new(format!("Pair XMouse with {}", p.console.name))
                        .font(bold(17.0))
                        .color(INK),
                );
                hint(
                    ui,
                    "Enter the PIN the console shows for remote play pairing. This is needed once.",
                );
                ui.add_space(6.0);
                let busy = p.busy;
                let edit = ui
                    .add_enabled_ui(!busy, |ui| {
                        text_field(
                            ui,
                            egui::TextEdit::singleline(&mut p.pin)
                                .char_limit(16)
                                .hint_text("PIN")
                                .desired_width(200.0),
                        )
                    })
                    .inner;
                if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    pair = true;
                }
                if let Some(e) = &p.error {
                    error_text(ui, e);
                }
                ui.add_space(space::S);
                ui.horizontal(|ui| {
                    if p.busy {
                        // As tall as the buttons it replaces, so the window doesn't jump.
                        ui.set_min_height(53.0);
                        spinner(ui);
                        hint(ui, "Pairing…");
                    } else {
                        if primary(ui, "Pair", 120.0).clicked() {
                            pair = true;
                        }
                        if secondary(ui, "Cancel", 120.0).clicked() {
                            cancel = true;
                        }
                    }
                });
            });
        if cancel {
            self.pairing = None;
            return;
        }
        let p = self.pairing.as_mut().unwrap();
        if pair && !p.busy && !p.pin.trim().is_empty() {
            let Some((pid, cid)) = plugin_console_ref(&p.console.server_id) else {
                self.pairing = None;
                return;
            };
            let Some(info) = self.plugins.iter().find(|x| x.id == pid).cloned() else {
                p.error = Some("The plugin for this console is no longer installed.".into());
                return;
            };
            p.busy = true;
            p.error = None;
            let (cid, pin) = (cid.to_string(), p.pin.trim().to_string());
            // Account IDs the plugin needs (e.g. PSN) travel with the pairing request.
            let mut extra = std::collections::BTreeMap::new();
            if let Some(acc) = self.settings.plugin_accounts.get(&info.id) {
                extra.insert("account_id".to_string(), acc.clone());
            }
            let (bg, ctx) = (self.bg.clone(), self.ctx.clone());
            self.bg.lock().unwrap().pair_result = None;
            self.rt.spawn_blocking(move || {
                let res = crate::plugin::pair(&info, &cid, &pin, &extra)
                    .map_err(|e| format!("{e:#}"));
                bg.lock().unwrap().pair_result = Some(res);
                ctx.request_repaint();
            });
        }
    }

    /// The plugin for the console type you chose; `None` means Xbox.
    fn platform_plugin(&self) -> Option<crate::plugin::PluginInfo> {
        match self.settings.console_platform.as_str() {
            "" | "xbox" => None,
            id => self.plugin(id),
        }
    }

    /// You haven't picked Xbox or a plugin's console yet (or the plugin you picked was removed).
    fn choosing_platform(&self) -> bool {
        match self.settings.console_platform.as_str() {
            "" => true,
            "xbox" => false,
            id => self.plugin(id).is_none(),
        }
    }

    /// Download and install PSNBridge (the PlayStation plugin) in the background.
    fn install_psnbridge(&self) {
        let (bg, ctx) = (self.bg.clone(), self.ctx.clone());
        {
            let mut b = bg.lock().unwrap();
            if b.installing {
                return;
            }
            b.installing = true;
            b.install_error = None;
        }
        self.rt.spawn(async move {
            let res =
                crate::plugin::install::install_psnbridge(&crate::plugin::plugins_dir()).await;
            let mut b = bg.lock().unwrap();
            b.installing = false;
            match res {
                Ok(p) => {
                    log::info!("installed plugin {} {}", p.id, p.version);
                    b.install_done = true;
                    b.toasts.push((format!("{} {} installed", p.name, p.version), false));
                }
                Err(e) => {
                    log::warn!("PSNBridge install: {e}");
                    b.install_error = Some(e);
                }
            }
            ctx.request_repaint();
        });
    }

    /// After an install: pick up the new plugin.
    fn finish_install(&mut self) {
        if !std::mem::take(&mut self.bg.lock().unwrap().install_done) {
            return;
        }
        self.plugins = crate::plugin::discover();
        self.apply_platform_names();
        if self.platform_plugin().is_some() {
            self.load_plugin_consoles();
        }
    }

    /// "Xbox", or the plugin's platform (e.g. "PlayStation").
    fn platform_name(&self) -> String {
        self.platform_plugin()
            .map(|p| p.platform)
            .unwrap_or_else(|| "Xbox".into())
    }

    fn is_playstation(p: &crate::plugin::PluginInfo) -> bool {
        p.platform.to_ascii_lowercase().contains("playstation")
    }

    /// Button names and the controller drawing follow the chosen console type.
    fn apply_platform_names(&self) {
        crate::profile::set_playstation_names(
            self.platform_plugin().is_some_and(|p| Self::is_playstation(&p)),
        );
    }

    /// Switch console type ("xbox", a plugin id, or "" to choose again).
    fn set_platform(&mut self, id: &str) {
        self.settings.console_platform = id.into();
        self.save_settings();
        self.apply_platform_names();
        {
            let mut b = self.bg.lock().unwrap();
            b.plugin_consoles.clear();
            b.plugins_loaded = false;
        }
        self.page = Page::Home;
    }

    /// The consoles for the chosen console type, and whether they're still being looked up.
    fn console_state(&self) -> (Option<Result<Vec<ConsoleInfo>, String>>, bool) {
        let platform = self.platform_plugin();
        let b = self.bg.lock().unwrap();
        match platform {
            None => (b.consoles.clone(), b.consoles_loading),
            Some(p) => {
                let prefix = plugin_console_key(&p.id, "");
                let list: Vec<ConsoleInfo> = b
                    .plugin_consoles
                    .iter()
                    .filter(|c| c.server_id.starts_with(&prefix))
                    .cloned()
                    .collect();
                let state = if b.plugins_loaded { Some(Ok(list)) } else { None };
                (state, b.plugins_loading)
            }
        }
    }

    fn plugin(&self, id: &str) -> Option<crate::plugin::PluginInfo> {
        self.plugins.iter().find(|p| p.id == id).cloned()
    }

    fn wake(&self, id: String, name: String) {
        if let Some((pid, cid)) = plugin_console_ref(&id) {
            let Some(info) = self.plugin(pid) else { return };
            let (cid, bg, ctx) = (cid.to_string(), self.bg.clone(), self.ctx.clone());
            self.rt.spawn_blocking(move || {
                let msg = match crate::plugin::wake(&info, &cid) {
                    Ok(()) => (format!("Wake-up sent to {name}"), false),
                    Err(e) => (format!("Could not wake {name}: {e:#}"), true),
                };
                bg.lock().unwrap().toasts.push(msg);
                ctx.request_repaint();
            });
            return;
        }
        let (auth, bg, ctx) = (self.auth.clone(), self.bg.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            let res = async {
                let (token, uhs) = auth.web_token().await?;
                wake_console(&token, &uhs, &id).await
            }
            .await;
            let msg = match res {
                Ok(()) => (format!("Wake-up sent to {name}"), false),
                Err(e) => (format!("Could not wake {name}: {e:#}"), true),
            };
            bg.lock().unwrap().toasts.push(msg);
            ctx.request_repaint();
        });
    }

    fn start_session(&mut self, console: ConsoleInfo, mode: PlayMode) {
        // A plugin console that isn't paired yet: ask for the PIN first.
        if self.bg.lock().unwrap().needs_pairing.contains(&console.server_id) {
            self.pairing = Some(Pairing {
                console,
                pin: String::new(),
                busy: false,
                error: None,
            });
            return;
        }
        let plugin = match plugin_console_ref(&console.server_id) {
            Some((pid, cid)) => match self.plugin(pid) {
                Some(info) => Some((info, cid.to_string())),
                None => {
                    self.toast(format!("The plugin for {} is no longer installed", console.name), true);
                    return;
                }
            },
            None => None,
        };
        self.flush_profile();
        self.stop_test();
        let profile = self.profile().clone();
        self.settings.last_console = console.server_id.clone();
        self.settings.last_profile = profile.id.clone();
        self.settings.last_mode = mode;
        self.save_settings();

        let shared = Shared::new(Mapper::new(profile));
        shared.gpu_decode.store(
            mode == PlayMode::Pc && self.settings.pc_gpu_decode,
            std::sync::atomic::Ordering::Relaxed,
        );
        let cfg = StreamConfig {
            mode,
            video_bitrate: if mode == PlayMode::Tv {
                self.settings.tv_bitrate
            } else {
                self.settings.pc_bitrate
            },
            high_quality: self.settings.pc_high_quality,
            input_hz: self.settings.input_hz,
            console_name: console.name.clone(),
            console_ip: self.settings.xbox_console_ip.clone(),
            ..Default::default()
        };
        let ctx = self.ctx.clone();
        let repaint: crate::stream::Repaint = Arc::new(move || ctx.request_repaint());
        if let Some((info, console_id)) = plugin {
            // Another platform: the plugin does the network side, everything else is shared.
            let pcfg = crate::plugin::PluginSessionConfig {
                mode,
                bitrate_kbps: cfg.video_bitrate,
                input_hz: cfg.input_hz,
                ..Default::default()
            };
            self.rt.spawn(crate::plugin::run_plugin_session(
                info,
                console_id,
                pcfg,
                shared.clone(),
                repaint,
            ));
        } else if self.demo {
            self.rt
                .spawn(crate::stream::run_demo_session(shared.clone(), repaint));
        } else {
            self.rt.spawn(run_session(
                self.auth.clone(),
                console.server_id.clone(),
                cfg,
                shared.clone(),
                repaint,
            ));
        }
        self.session = Some(Session {
            paused: false,
            shared,
            console,
            mode,
            captured: false,
            show_hud: self.settings.show_hud,
            texture: None,
            vsr_tex: None,
            fg_rect: [0; 4],
            vsr_target: [0, 0],
            video_size: [0, 0],
            tip_since: None,
            legend: (None, Arc::new(Vec::new())),
            mods: egui::Modifiers::default(),
            pic: PictureState::default(),
        });
        // Nothing of the last session's picture may show up in this one.
        if let Some(p) = self.pipe.as_mut() {
            p.reset_history();
        }
    }

    fn end_session(&mut self, ctx: &egui::Context) {
        #[cfg(windows)]
        {
            self.fg = None;
        }
        self.fg_failed = false;
        if self.fg_guard_armed {
            let _ = std::fs::remove_file(self.storage.framegen_guard_file());
            self.fg_guard_armed = false;
        }
        // A session that ended normally didn't crash the driver.
        if self.vsr_guard_armed {
            let _ = std::fs::remove_file(self.storage.vsr_guard_file());
            self.vsr_guard_armed = false;
        }
        self.vsr_guard_target = [0, 0];
        if let Some(s) = self.session.take() {
            s.shared.stop();
            if let Some(e) = s.shared.status().error {
                self.toast(e, true);
            }
        }
        release_capture(ctx);
        ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
        self.bg.lock().unwrap().consoles = None;
    }
}

fn grab_capture(ctx: &egui::Context) {
    let mode = if cfg!(target_os = "macos") {
        egui::CursorGrab::Locked
    } else {
        egui::CursorGrab::Confined
    };
    ctx.send_viewport_cmd(ViewportCommand::CursorGrab(mode));
    ctx.send_viewport_cmd(ViewportCommand::CursorVisible(false));
}

fn grab_capture_of(ctx: &egui::Context, id: egui::ViewportId) {
    let mode = if cfg!(target_os = "macos") {
        egui::CursorGrab::Locked
    } else {
        egui::CursorGrab::Confined
    };
    ctx.send_viewport_cmd_to(id, ViewportCommand::CursorGrab(mode));
    ctx.send_viewport_cmd_to(id, ViewportCommand::CursorVisible(false));
}

fn release_capture(ctx: &egui::Context) {
    ctx.send_viewport_cmd(ViewportCommand::CursorGrab(egui::CursorGrab::None));
    ctx.send_viewport_cmd(ViewportCommand::CursorVisible(true));
}

/// What happened with input this frame.
#[derive(Default)]
struct InputResult {
    hotkeys: Vec<egui::Key>,
    lost_focus: bool,
}

/// Feed this frame's raw input into a mapper. Only while `captured`.
fn feed_input(
    ctx: &egui::Context,
    mapper: &mut Mapper,
    captured: bool,
    mods: &mut egui::Modifiers,
    now: f64,
) -> InputResult {
    let mut out = InputResult::default();
    let (events, cur_mods) = ctx.input(|i| (i.events.clone(), i.modifiers));
    for ev in events {
        match ev {
            egui::Event::Key {
                key,
                physical_key,
                pressed,
                repeat,
                ..
            } => {
                let k = physical_key.unwrap_or(key);
                if matches!(k, KEY_RELEASE | KEY_FINDER | KEY_HUD | KEY_FULLSCREEN) {
                    if pressed && !repeat {
                        out.hotkeys.push(k);
                    }
                    continue;
                }
                if !captured {
                    continue;
                }
                // Key repeats are ignored by the mapper itself (it tracks held keys). egui's own
                // `repeat` flag is keyed by the logical key and can go stale when Shift changes
                // mid-press, which would swallow every later press of that key.
                if pressed {
                    mapper.press(k.name(), now);
                } else {
                    mapper.release(k.name());
                }
            }
            // egui turns Ctrl+X/C/V, Shift+Delete/Insert and Ctrl+Insert into clipboard events
            // with no key press (the release still arrives as a key event). In a game those are
            // just keys: hand the press back to the mapper.
            egui::Event::Cut | egui::Event::Copy | egui::Event::Paste(_) | egui::Event::PasteImage(_)
                if captured =>
            {
                let shift_only = cur_mods.shift && !cur_mods.ctrl;
                let name = match ev {
                    egui::Event::Cut if shift_only => "Delete",
                    egui::Event::Cut => "X",
                    egui::Event::Copy => "C",
                    _ if shift_only => "Insert",
                    _ => "V",
                };
                mapper.press(name, now);
            }
            egui::Event::MouseMoved(d) if captured => mapper.mouse_move(d.x as f64, d.y as f64),
            egui::Event::PointerButton {
                button, pressed, ..
            } if captured => {
                let name = mouse_name(button);
                if pressed {
                    mapper.press(name, now)
                } else {
                    mapper.release(name)
                }
            }
            egui::Event::MouseWheel { delta, .. } if captured && delta.y != 0.0 => {
                mapper.wheel(delta.y > 0.0, now)
            }
            egui::Event::WindowFocused(false) => out.lost_focus = true,
            _ => {}
        }
    }
    if captured {
        for (was, is, name) in [
            (mods.shift, cur_mods.shift, "Shift"),
            (mods.ctrl, cur_mods.ctrl, "Ctrl"),
            (mods.alt, cur_mods.alt, "Alt"),
        ] {
            if is && !was {
                mapper.press(name, now);
            } else if was && !is {
                mapper.release(name);
            }
        }
    }
    *mods = cur_mods;
    out
}

fn mouse_name(b: egui::PointerButton) -> &'static str {
    match b {
        egui::PointerButton::Primary => "Mouse0",
        egui::PointerButton::Middle => "Mouse1",
        egui::PointerButton::Secondary => "Mouse2",
        egui::PointerButton::Extra1 => "Mouse3",
        egui::PointerButton::Extra2 => "Mouse4",
    }
}

/// How long a toast stays: long enough to read it (5 s for a short one, up to 12 s).
fn toast_secs(msg: &str) -> f32 {
    (3.5 + msg.chars().count() as f32 * 0.05).clamp(5.0, 12.0)
}

/// How far a toast has come in and how much of it is left (both 0..=1), from its age and
/// the time it has left.
fn toast_phase(age: f32, left: f32) -> (f32, f32) {
    (
        motion::ease_out(age / motion::SLOW),
        motion::ease_out(left / motion::SLOW),
    )
}

/// How many `item`-wide things fit side by side in `avail` with `gap` between them (at least 1).
fn per_line(avail: f32, item: f32, gap: f32) -> usize {
    (((avail + gap) / (item + gap)).floor() as usize).max(1)
}

fn page_frame() -> egui::Frame {
    egui::Frame::new().fill(PAGE).inner_margin(Margin {
        left: 24,
        right: 24,
        top: 0,
        bottom: 16,
    })
}

impl eframe::App for XMouseApp {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // An edit made in the last half second is still waiting to be saved.
        self.flush_profile();
        // Closing the app during a session is not a graphics driver crash.
        if self.vsr_guard_armed {
            let _ = std::fs::remove_file(self.storage.vsr_guard_file());
            self.vsr_guard_armed = false;
        }
        if let Some(s) = &self.session {
            s.shared.stop();
        }
    }

    fn ui(&mut self, ui: &mut Ui, frame: &mut eframe::Frame) {
        // Outside a session nothing needs more than ~144 frames a second. Vsync is off (for
        // the lowest input delay while playing), so without this a moving mouse, an open
        // menu or the Test page would redraw thousands of times a second and keep a CPU
        // core busy. A session is never paced: there every frame carries input.
        if self.session.is_none() {
            const MIN_FRAME: Duration = Duration::from_micros(6900);
            let since = self.frame_start.elapsed();
            if since < MIN_FRAME {
                std::thread::sleep(MIN_FRAME - since);
            }
        }
        self.frame_start = Instant::now();
        self.frame_no += 1;
        let ctx = ui.ctx().clone();
        let new_toasts: Vec<(String, bool)> = std::mem::take(&mut self.bg.lock().unwrap().toasts);
        for (m, e) in new_toasts {
            self.toast(m, e);
        }
        self.pairing_window(&ctx);
        self.finish_install();

        let signed_in = self.bg.lock().unwrap().signed_in;
        let platform = self.platform_plugin();
        let needs_account = platform.as_ref().is_some_and(|p| {
            Self::is_playstation(p) && !self.settings.plugin_accounts.contains_key(&p.id)
        });
        if self.session.is_some() {
            self.session_ui(ui, &ctx, frame);
        } else if self.choosing_platform() || needs_account {
            // First the console type, then (PlayStation) the account ID pairing needs.
            let view = if needs_account { View::Account } else { View::Platform };
            let enter = self.nav.show(&ctx, view);
            self.nav.back_fade(&ctx, None);
            top_bar(ui, None, None, 1.0);
            status_bar(ui, if needs_account { "PlayStation" } else { "Pick your console" });
            self.page_body(ui, view, enter, |app, ui| {
                if needs_account {
                    app.plugin_account_page(ui)
                } else {
                    app.platform_page(ui)
                }
            });
        } else if platform.is_none() && !signed_in {
            let enter = self.nav.show(&ctx, View::SignIn);
            self.nav.back_fade(&ctx, None);
            top_bar(ui, None, None, 1.0);
            status_bar(ui, "Not signed in");
            self.page_body(ui, View::SignIn, enter, |app, ui| app.sign_in_page(ui));
        } else {
            let back = match self.page {
                Page::Home => None,
                Page::More | Page::Consoles => Some("Play"),
                Page::Test if self.came_from_keys => Some("Keys"),
                _ => Some("More"),
            };
            self.nav.show(&ctx, View::Page(self.page));
            let back_fade = self.nav.back_fade(&ctx, back);
            if top_bar(ui, back, None, back_fade) {
                if self.page == Page::Test {
                    self.stop_test();
                }
                // Leaving the Keys page ends a key pick that was still waiting; it would
                // otherwise take the first key pressed after coming back.
                self.editor.listening = None;
                self.flush_profile();
                self.page = match self.page {
                    Page::More | Page::Consoles => Page::Home,
                    Page::Test if self.came_from_keys => Page::Keys,
                    _ => Page::More,
                };
                self.came_from_keys = false;
            }
            let status = self.status_line();
            status_bar(ui, &status);
            // (Again: a click on the back button just changed the page.)
            let view = View::Page(self.page);
            let enter = self.nav.show(&ctx, view);
            self.page_body(ui, view, enter, |app, ui| match app.page {
                Page::Home => app.home_page(ui),
                Page::More => app.more_page(ui),
                Page::Consoles => app.consoles_page(ui),
                Page::PressOnce => app.press_once_page(ui),
                Page::Profiles => app.profiles_page(ui),
                Page::Keys => app.keys_page(ui, &ctx),
                Page::Mouse => app.mouse_page(ui),
                Page::Test => app.test_page(ui, &ctx),
                Page::Picture => app.picture_page(ui),
                Page::Setup => app.setup_page(ui),
            });
        }
        self.toasts_ui(&ctx);
        self.autosave();
    }
}

impl XMouseApp {
    /// The scrolling body of a page. Every view keeps its own scroll position, starts at
    /// the top when you arrive, and comes in with the page transition.
    fn page_body(
        &mut self,
        ui: &mut Ui,
        view: View,
        enter: (f32, f32),
        add: impl FnOnce(&mut Self, &mut Ui),
    ) {
        let fresh = std::mem::take(&mut self.nav.fresh);
        egui::CentralPanel::default()
            .frame(page_frame())
            .show(ui, |ui| {
                let mut area = egui::ScrollArea::vertical()
                    .id_salt(("page", view))
                    .auto_shrink([false, false]);
                if fresh {
                    area = area.vertical_scroll_offset(0.0);
                }
                area.show(ui, |ui| entering(ui, enter, |ui| add(self, ui)));
            });
    }
}

impl XMouseApp {
    fn status_line(&self) -> String {
        let consoles = match self.console_state().0 {
            None => "Looking for consoles".to_string(),
            Some(Ok(l)) => format!("{} console{}", l.len(), if l.len() == 1 { "" } else { "s" }),
            Some(Err(_)) => "Consoles unavailable".to_string(),
        };
        format!(
            "{} · {consoles} · {} · {} Hz input",
            if self.platform_plugin().is_some() { self.platform_name() } else { "Signed in".into() },
            self.profile().name,
            self.settings.input_hz
        )
    }

    // ------------------------------------------------------------------ toasts / autosave

    /// Toasts stack up from the bottom right. Each one slides up and fades in, stays long
    /// enough to read, then fades out while the ones above settle down into its place.
    /// They are only painted: clicks go through to whatever is underneath.
    fn toasts_ui(&mut self, ctx: &egui::Context) {
        self.toasts
            .retain(|t| t.2.elapsed().as_secs_f32() < toast_secs(&t.0));
        if self.toasts.is_empty() {
            return;
        }
        let screen = ctx.content_rect();
        let mut painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::unique("toasts"),
        ));
        let (pad, gap) = (vec2(16.0, 12.0), 6.0);
        let text_w = (screen.width() - 2.0 * 18.0 - 2.0 * pad.x).clamp(80.0, 380.0);
        let mut bottom = screen.bottom() - 42.0;
        // When the next frame is needed: now while something moves, else when the next
        // toast starts to leave.
        let mut wake = f32::INFINITY;
        for (msg, err, born) in self.toasts.iter().rev() {
            let age = born.elapsed().as_secs_f32();
            let left = toast_secs(msg) - age;
            let (t_in, t_out) = toast_phase(age, left);
            wake = wake.min(if t_in < 1.0 || t_out < 1.0 {
                0.0
            } else {
                left - motion::SLOW
            });
            let shown = t_in * t_out;
            let galley = painter.layout(msg.clone(), medium(14.5), INK, text_w);
            let size = galley.size() + 2.0 * pad;
            let rect = egui::Rect::from_min_size(
                egui::pos2(
                    screen.right() - 18.0 - size.x,
                    bottom - size.y + 12.0 * (1.0 - t_in),
                ),
                size,
            );
            painter.set_opacity(shown);
            painter.add(
                egui::Shadow {
                    offset: [0, 3],
                    blur: 12,
                    spread: 0,
                    color: Color32::from_black_alpha(22),
                }
                .as_shape(rect, CornerRadius::same(radius::BUTTON)),
            );
            painter.rect(
                rect,
                CornerRadius::same(radius::BUTTON),
                KEY,
                Stroke::new(1.5, if *err { SIGNAL } else { EDGE }),
                egui::StrokeKind::Inside,
            );
            painter.galley(rect.min + pad, galley, INK);
            // An arriving toast makes room gradually, a leaving one gives it back the same way.
            bottom -= (size.y + gap) * shown;
        }
        if wake <= 0.0 {
            ctx.request_repaint_after(motion::FRAME);
        } else {
            ctx.request_repaint_after(Duration::from_secs_f32(wake.min(60.0)));
        }
    }

    fn mark_dirty(&mut self) {
        self.editor.dirty_since = Some(Instant::now());
        let p = self.profile().clone();
        if let Some(t) = self.editor.test.as_mut() {
            t.mapper.set_profile(p);
        }
    }

    fn autosave(&mut self) {
        if let Some(t) = self.editor.dirty_since {
            if t.elapsed() > Duration::from_millis(500) {
                self.flush_profile();
            } else {
                self.ctx.request_repaint_after(Duration::from_millis(200));
            }
        }
    }

    fn flush_profile(&mut self) {
        if self.editor.dirty_since.take().is_some() {
            if let Err(e) = self.storage.save_profile(&self.profiles[self.current]) {
                self.toast(format!("Could not save profile: {e}"), true);
            }
        }
    }

    fn select_profile(&mut self, i: usize) {
        self.flush_profile();
        self.current = i.min(self.profiles.len() - 1);
        self.settings.last_profile = self.profiles[self.current].id.clone();
        self.save_settings();
        self.editor = Editor {
            selected: Some(0),
            ..Default::default()
        };
    }

    fn add_profile(&mut self, from: Profile, name: &str) {
        // Reloading the list below would drop an edit still waiting to be saved.
        self.flush_profile();
        let mut p = from;
        p.id = format!(
            "my-{}",
            uuid::Uuid::new_v4()
                .simple()
                .to_string()
                .get(..8)
                .unwrap_or("x")
        );
        p.name = name.to_string();
        p.builtin = false;
        if let Err(e) = self.storage.save_profile(&p) {
            self.toast(format!("Could not save: {e}"), true);
            return;
        }
        self.profiles = self.storage.profiles();
        let i = self.profiles.iter().position(|x| x.id == p.id).unwrap_or(0);
        self.select_profile(i);
    }

    // ------------------------------------------------------------------ sign in

    fn sign_in_page(&mut self, ui: &mut Ui) {
        let (busy, err, code) = {
            let b = self.bg.lock().unwrap();
            (b.auth_busy, b.auth_error.clone(), b.device_code.clone())
        };
        title(
            ui,
            "Sign in",
            "With the Microsoft account that is signed in on your Xbox.",
        );
        let mut copy = false;
        let mut open: Option<String> = None;
        let mut start = false;
        narrow(ui, 560.0, |ui| {
            if let Some(c) = &code {
                section(ui, "On your phone or computer");
                group(ui, |ui| {
                    let url = c
                        .verification_uri
                        .trim_start_matches("https://")
                        .to_string();
                    let r = row(
                        ui,
                        true,
                        &format!("Go to {url}"),
                        "Opens in your browser.",
                        RowOptions {
                            lead: Lead::Key("1".into(), false),
                            ..RowOptions::link()
                        },
                        |_| {},
                    );
                    if r.clicked() {
                        open = Some(c.verification_uri.clone());
                    }
                    row(
                        ui,
                        false,
                        "Enter this code",
                        "",
                        RowOptions {
                            lead: Lead::Key("2".into(), true),
                            ..Default::default()
                        },
                        |ui| {
                            copy =
                                keycap_sized(ui, "Copy", bold(14.0), 28.0, false, true).clicked();
                        },
                    );
                    egui::Frame::new()
                        .inner_margin(Margin {
                            left: 15,
                            right: 15,
                            top: 4,
                            bottom: 18,
                        })
                        .show(ui, |ui| {
                            let n = c.user_code.chars().count() as f32;
                            centered_row(ui, n * 56.0, |ui| {
                                ui.spacing_mut().item_spacing.x = 8.0;
                                for ch in c.user_code.chars() {
                                    keycap_sized(
                                        ui,
                                        &ch.to_string(),
                                        mono(24.0),
                                        48.0,
                                        false,
                                        false,
                                    );
                                }
                            });
                        });
                });
                centered_row(ui, 220.0, |ui| {
                    spinner(ui);
                    hint(ui, "Waiting for you to sign in…");
                });
            } else {
                ui.add_space(space::XS);
                centered_row(ui, 300.0, |ui| {
                    start = busy_button(
                        ui,
                        if busy {
                            "Contacting Microsoft…"
                        } else {
                            "Sign in with Microsoft"
                        },
                        300.0,
                        SIGNAL,
                        SIGNAL_EDGE,
                        busy,
                    )
                    .clicked()
                        && !busy;
                });
                if let Some(e) = &err {
                    ui.add_space(space::S);
                    ui.vertical_centered(|ui| error_text(ui, e));
                }
                ui.add_space(space::L);
            }
            section(ui, "Before you start");
            setup_group(ui);
        });
        if start {
            self.sign_in();
        }
        if let Some(u) = open {
            ui.ctx().open_url(egui::OpenUrl::new_tab(&u));
        }
        if copy {
            if let Some(c) = code {
                ui.ctx().copy_text(c.user_code);
                self.toast("Code copied", false);
            }
        }
    }

    // ------------------------------------------------------------------ home

    fn home_page(&mut self, ui: &mut Ui) {
        let (consoles, loading) = self.console_state();
        if consoles.is_none() && !loading {
            self.load_consoles();
        }
        let list: Vec<ConsoleInfo> = match &consoles {
            Some(Ok(l)) => l.clone(),
            _ => vec![],
        };
        let platform_name = self.platform_name();
        let selected = list
            .iter()
            .find(|c| c.server_id == self.settings.last_console)
            .or(list.first())
            .cloned();
        // --play: one-click start (e.g. a desktop shortcut) on the last console, last mode.
        // A plugin console may only show up once the plugins have answered: wait for them
        // before giving up.
        if self.play_on_start && consoles.is_some() {
            let found = list
                .iter()
                .find(|c| c.server_id == self.settings.last_console)
                .cloned();
            let plugins_loading = self.bg.lock().unwrap().plugins_loading;
            if found.is_some() || !plugins_loading {
                self.play_on_start = false;
            }
            if let Some(c) = found {
                let mode = self.settings.last_mode;
                self.start_session(c, mode);
                return;
            }
        }

        let p = self.profile().clone();
        let mut play = false;
        let mut pick_console: Option<String> = None;
        let mut wake = false;
        let mut refresh = false;
        let mut pick_profile: Option<usize> = None;
        let mut pick_mode: Option<PlayMode> = None;
        let mut go: Option<Page> = None;
        let mode = self.settings.last_mode;

        ui.add_space((ui.available_height() * 0.24).max(30.0));
        ui.vertical_centered(|ui| {
            ui.spacing_mut().item_spacing.y = 14.0;
            // Console line
            match (&consoles, &selected) {
                (None, _) => {
                    // Measured, so spinner and text sit centred together whatever the name is.
                    let text = format!("Looking for your {platform_name}…");
                    let galley = ui.painter().layout_no_wrap(text, body(18.0), MUTED);
                    let w = 18.0 + space::S + galley.size().x;
                    let (rect, _) = ui.allocate_exact_size(
                        vec2(w, galley.size().y.max(18.0)),
                        egui::Sense::hover(),
                    );
                    spinner_at(ui, egui::pos2(rect.left() + 9.0, rect.center().y), 7.0, MUTED);
                    ui.painter().galley(
                        egui::pos2(
                            rect.left() + 18.0 + space::S,
                            rect.center().y - galley.size().y / 2.0,
                        ),
                        galley,
                        MUTED,
                    );
                }
                (Some(Err(e)), _) => {
                    ui.label(
                        RichText::new(format!("Could not reach the {platform_name} service"))
                            .font(bold(18.0))
                            .color(INK),
                    );
                    error_text(ui, e);
                    if small_link(ui, "Try again").clicked() {
                        refresh = true;
                    }
                }
                (Some(Ok(_)), None) => {
                    ui.label(
                        RichText::new("No consoles found")
                            .font(bold(18.0))
                            .color(INK),
                    );
                    if platform_name == "Xbox" {
                        if small_link(ui, "Set up your Xbox").clicked() {
                            go = Some(Page::Setup);
                        }
                    } else {
                        hint(
                            ui,
                            &format!(
                                "Make sure your {platform_name} is on (or resting) with remote play \
                                 turned on, and on the same network as this PC."
                            ),
                        );
                    }
                }
                (Some(Ok(_)), Some(c)) => {
                    let state = match c.power_state.as_str() {
                        "On" => "Ready",
                        "ConnectedStandby" => "Asleep",
                        other => other,
                    };
                    let resp = inline_picker_suffix(
                        ui,
                        "",
                        &format!("{} · {}", short_console(c), c.name),
                        &format!("  ·  {state}"),
                        19.0,
                    );
                    egui::Popup::menu(&resp).show(|ui| {
                        ui.set_min_width(260.0);
                        for c in &list {
                            let label =
                                format!("{} · {}  ({})", short_console(c), c.name, c.power_label());
                            if ui
                                .selectable_label(Some(c) == selected.as_ref(), label)
                                .clicked()
                            {
                                pick_console = Some(c.server_id.clone());
                            }
                        }
                        ui.separator();
                        if c.power_state != "On" && ui.button("Wake this console").clicked() {
                            wake = true;
                        }
                        if ui.button("Look for consoles again").clicked() {
                            refresh = true;
                        }
                    });
                }
            }

            if play_button(ui, "Play", selected.is_some()).clicked() {
                play = true;
            }

            let game = if p.game.is_empty() {
                p.name.clone()
            } else {
                format!("{} · {}", p.name, short_game(&p.game))
            };
            let resp = inline_picker(ui, "Game", &game, 18.0);
            egui::Popup::menu(&resp).show(|ui| {
                ui.set_min_width(260.0);
                for (i, q) in self.profiles.iter().enumerate() {
                    if ui.selectable_label(i == self.current, &q.name).clicked() {
                        pick_profile = Some(i);
                    }
                }
                ui.separator();
                if ui.button("Manage games…").clicked() {
                    go = Some(Page::Profiles);
                }
            });

            let picture = if mode == PlayMode::Tv {
                "On the TV"
            } else {
                "On this PC"
            };
            let resp = inline_picker(ui, "Picture", picture, 18.0);
            egui::Popup::menu(&resp).show(|ui| {
                ui.set_min_width(220.0);
                if ui
                    .selectable_label(mode == PlayMode::Tv, "On the TV (TV Play)")
                    .clicked()
                {
                    pick_mode = Some(PlayMode::Tv);
                }
                if ui
                    .selectable_label(mode == PlayMode::Pc, "On this PC (PC Play)")
                    .clicked()
                {
                    pick_mode = Some(PlayMode::Pc);
                }
            });

            ui.add_space(6.0);
            if small_link(ui, "More").clicked() {
                go = Some(Page::More);
            }
            ui.label(
                RichText::new(if mode == PlayMode::Tv {
                    "The picture is on the TV."
                } else {
                    "The picture opens in its own window on this PC."
                })
                .font(body(17.0))
                .color(MUTED),
            );
            let dz = p.mouse.deadzone;
            if small_link(ui, &format!("Game settings · deadzone {dz:.2}")).clicked() {
                go = Some(Page::Mouse);
            }
        });

        if refresh {
            self.load_consoles();
        }
        if wake {
            if let Some(c) = &selected {
                self.wake(c.server_id.clone(), c.name.clone());
            }
        }
        if let Some(id) = pick_console {
            self.settings.last_console = id;
            self.save_settings();
        }
        if let Some(i) = pick_profile {
            self.select_profile(i);
        }
        if let Some(m) = pick_mode {
            self.settings.last_mode = m;
            self.save_settings();
        }
        if let Some(pg) = go {
            self.page = pg;
        }
        if play {
            if let Some(c) = selected {
                self.start_session(c, self.settings.last_mode);
            }
        }
    }

    /// "Which console do you play on?": Xbox, or the console of an installed plugin.
    fn platform_page(&mut self, ui: &mut Ui) {
        ui.add_space((ui.available_height() * 0.18).max(20.0));
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new("Which console do you play on?")
                    .font(display(34.0))
                    .color(INK),
            );
            hint(ui, "You can change this later under More.");
        });
        ui.add_space(22.0);
        use crate::plugin::install::{PSNBRIDGE_ID, PSNBRIDGE_NAME};
        let mut choice: Option<String> = None;
        let mut install = false;
        let (installing, install_error) = {
            let b = self.bg.lock().unwrap();
            (b.installing, b.install_error.clone())
        };
        // (id, button, line under it, installed). PlayStation is always offered: greyed out
        // with an Install button while PSNBridge is missing.
        let mut cards: Vec<(String, String, String, bool)> = vec![(
            "xbox".into(),
            "Xbox".into(),
            "Sign in with your Microsoft account".into(),
            true,
        )];
        match self.plugin(PSNBRIDGE_ID) {
            Some(p) => cards.push((
                p.id.clone(),
                p.platform.clone(),
                format!("Through the {} plugin ({})", p.name, p.license),
                true,
            )),
            None => cards.push((
                PSNBRIDGE_ID.into(),
                "PlayStation".into(),
                format!("{PSNBRIDGE_NAME} missing"),
                false,
            )),
        }
        for p in self.plugins.iter().filter(|p| p.id != PSNBRIDGE_ID) {
            cards.push((
                p.id.clone(),
                p.platform.clone(),
                format!("Through the {} plugin ({})", p.name, p.license),
                true,
            ));
        }
        // As many cards side by side as fit; more console types wrap onto another line
        // instead of running out of the window.
        for line in cards.chunks(per_line(ui.available_width(), 260.0, space::L)) {
            let w = line.len() as f32 * (260.0 + space::L) - space::L;
            centered_row(ui, w, |ui| {
                ui.spacing_mut().item_spacing.x = space::L;
                for (id, name, sub, installed) in line {
                    ui.vertical(|ui| {
                        ui.set_width(260.0);
                        ui.spacing_mut().item_spacing.y = space::S;
                        if *installed {
                            if primary(ui, name, 260.0).clicked() {
                                choice = Some(id.clone());
                            }
                            hint(ui, sub);
                            return;
                        }
                        // Not installed: a grey button that can't be clicked, and the way to
                        // get it.
                        button3d(ui, name, 260.0, IDLE, EDGE, false).on_hover_text(format!(
                            "Install {PSNBRIDGE_NAME} to play on a PlayStation"
                        ));
                        ui.label(RichText::new(sub).font(bold(14.0)).color(SIGNAL_TEXT));
                        let label = if installing {
                            "Installing…".to_string()
                        } else {
                            format!("Install {PSNBRIDGE_NAME}")
                        };
                        if busy_button(ui, &label, 260.0, KEY, EDGE, installing).clicked() {
                            install = true;
                        }
                        match &install_error {
                            Some(e) => error_text(ui, e),
                            None => hint(ui, "Downloads the free PlayStation plugin (AGPL-3.0)."),
                        }
                    });
                }
            });
            ui.add_space(space::M);
        }
        if install {
            self.install_psnbridge();
        }
        if let Some(id) = choice {
            self.set_platform(&id);
        }
    }

    /// The account ID a PlayStation plugin needs for pairing, asked once.
    fn plugin_account_page(&mut self, ui: &mut Ui) {
        let Some(p) = self.platform_plugin() else { return };
        let key = egui::Id::unique("plugin-account-draft");
        let mut draft = ui.data(|d| d.get_temp::<String>(key)).unwrap_or_default();
        let mut save = false;
        let mut back = false;
        ui.add_space((ui.available_height() * 0.18).max(20.0));
        narrow(ui, 520.0, |ui| {
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new("Your PSN account")
                        .font(display(34.0))
                        .color(INK),
                );
            });
            ui.add_space(space::M);
            hint(
                ui,
                &format!(
                    "Pairing a PlayStation needs your PSN account ID. The {} plugin's instructions \
                     explain where to find it. It stays on this PC.",
                    p.name
                ),
            );
            ui.add_space(space::M);
            let edit = text_field(
                ui,
                egui::TextEdit::singleline(&mut draft)
                    .hint_text("PSN account ID")
                    .char_limit(64)
                    .font(medium(15.0))
                    .margin(vec2(10.0, 8.0))
                    .desired_width(f32::INFINITY),
            );
            if edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                save = true;
            }
            ui.add_space(space::M);
            ui.horizontal(|ui| {
                // Nothing to continue with yet: the button says so instead of ignoring the click.
                let ready = !draft.trim().is_empty();
                if button3d(ui, "Continue", 160.0, SIGNAL, SIGNAL_EDGE, ready).clicked() {
                    save = true;
                }
                if secondary(ui, "Other console", 160.0).clicked() {
                    back = true;
                }
            });
        });
        ui.data_mut(|d| d.insert_temp(key, draft.clone()));
        if save && !draft.trim().is_empty() {
            self.settings
                .plugin_accounts
                .insert(p.id.clone(), draft.trim().to_string());
            self.save_settings();
            ui.data_mut(|d| d.remove_temp::<String>(key));
            self.page = Page::Home;
        }
        if back {
            self.set_platform("");
        }
    }

    fn more_page(&mut self, ui: &mut Ui) {
        let p = self.profile().clone();
        title(ui, "More", &p.name);
        let mut go: Option<Page> = None;
        let mut sign_out = false;
        let mut change_platform = false;
        let mut change_account = false;
        let xbox = self.platform_plugin().is_none();
        let platform_name = self.platform_name();
        let mut install = false;
        let mut open_plugins = false;
        let bridge = self.plugin(crate::plugin::install::PSNBRIDGE_ID);
        let (installing, install_error) = {
            let b = self.bg.lock().unwrap();
            (b.installing, b.install_error.clone())
        };
        narrow(ui, 600.0, |ui| {
            section(ui, "This game");
            group(ui, |ui| {
                for (i, (pg, t, s)) in [
                    (Page::Keys, "Keys", "Which key presses what"),
                    (
                        Page::Mouse,
                        "Sens & aim",
                        "Sensitivity, deadzone and aiming",
                    ),
                    (
                        Page::PressOnce,
                        "Press once, not hold",
                        "The button stays pressed until you press again",
                    ),
                    (Page::Test, "Test", "Try your keys without the console"),
                    (
                        Page::Profiles,
                        "Games",
                        "Pick, copy or import a game profile",
                    ),
                ]
                .into_iter()
                .enumerate()
                {
                    if row(ui, i == 0, t, s, RowOptions::link(), |_| {}).clicked() {
                        go = Some(pg);
                    }
                }
            });
            section(ui, "This computer");
            group(ui, |ui| {
                let mut rows = vec![
                    (
                        Page::Picture,
                        "Picture and connection",
                        "TV Play, PC Play and input",
                    ),
                    (Page::Consoles, "Which console?", "Pick the console to play on"),
                ];
                if xbox {
                    rows.push((Page::Setup, "Xbox setup", "Remote features and power mode"));
                }
                for (i, (pg, t, s)) in rows.into_iter().enumerate() {
                    if row(ui, i == 0, t, s, RowOptions::link(), |_| {}).clicked() {
                        go = Some(pg);
                    }
                }
                if row(
                    ui,
                    false,
                    "Console type",
                    &format!("{platform_name} · change to another console"),
                    RowOptions::link(),
                    |_| {},
                )
                .clicked()
                {
                    change_platform = true;
                }
            });
            section(ui, "Plugins");
            group(ui, |ui| {
                let name = crate::plugin::install::PSNBRIDGE_NAME;
                let sub = if installing {
                    "Installing…".to_string()
                } else if let Some(e) = &install_error {
                    e.clone()
                } else if let Some(p) = &bridge {
                    format!("PlayStation · version {} installed · click to update", p.version)
                } else {
                    "PlayStation · not installed · click to install".to_string()
                };
                // While it installs the row shows the spinner where its arrow was.
                let opts = RowOptions {
                    chevron: !installing,
                    danger: !installing && install_error.is_some(),
                    ..RowOptions::link()
                };
                let clicked = row(ui, true, name, &sub, opts, |ui| {
                    if installing {
                        spinner(ui);
                    }
                })
                .clicked();
                if clicked && !installing {
                    install = true;
                }
                open_plugins = row(
                    ui,
                    false,
                    "Plugins folder",
                    "Where plugins are installed",
                    RowOptions::link(),
                    |_| {},
                )
                .clicked();
            });
            section(ui, "Account");
            group(ui, |ui| {
                if xbox {
                    sign_out = row(
                        ui,
                        true,
                        "Sign out",
                        "Microsoft account",
                        RowOptions {
                            danger: true,
                            ..RowOptions::link()
                        },
                        |_| {},
                    )
                    .clicked();
                } else {
                    change_account = row(
                        ui,
                        true,
                        "PSN account ID",
                        "Change the account used for pairing",
                        RowOptions::link(),
                        |_| {},
                    )
                    .clicked();
                }
            });
        });
        if install {
            self.install_psnbridge();
        }
        if open_plugins {
            let dir = crate::plugin::plugins_dir();
            let _ = std::fs::create_dir_all(&dir);
            if let Err(e) = std::process::Command::new("explorer.exe").arg(&dir).spawn() {
                self.toast(format!("Couldn't open {}: {e}", dir.display()), true);
            }
        }
        if change_platform {
            self.set_platform("");
            return;
        }
        if change_account {
            if let Some(p) = self.platform_plugin() {
                self.settings.plugin_accounts.remove(&p.id);
                self.save_settings();
            }
            return;
        }
        if let Some(pg) = go {
            self.page = pg;
        }
        if sign_out && confirm("Sign out of your Microsoft account?") {
            let auth = self.auth.clone();
            self.rt.block_on(auth.sign_out());
            *self.bg.lock().unwrap() = Bg::default();
            self.page = Page::Home;
        }
    }

    fn consoles_page(&mut self, ui: &mut Ui) {
        let (consoles, loading) = self.console_state();
        if consoles.is_none() && !loading {
            self.load_consoles();
        }
        ui.add_space((ui.available_height() * 0.18).max(20.0));
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new("Which console?")
                    .font(display(34.0))
                    .color(INK),
            );
        });
        ui.add_space(18.0);
        let mut pick: Option<ConsoleInfo> = None;
        let mut wake: Option<ConsoleInfo> = None;
        let mut refresh = false;
        match &consoles {
            Some(Ok(list)) if !list.is_empty() => {
                // Many consoles wrap onto more lines instead of running out of the window.
                for line in list.chunks(per_line(ui.available_width(), 220.0, space::L)) {
                    let w = line.len() as f32 * (220.0 + space::L) - space::L;
                    centered_row(ui, w, |ui| {
                        ui.spacing_mut().item_spacing.x = space::L;
                        for c in line {
                            ui.vertical(|ui| {
                                ui.set_width(220.0);
                                ui.spacing_mut().item_spacing.y = space::S;
                                let r = if c.server_id == self.settings.last_console {
                                    primary(ui, &c.name, 220.0)
                                } else {
                                    secondary(ui, &c.name, 220.0)
                                };
                                if r.clicked() {
                                    pick = Some(c.clone());
                                }
                                hint(ui, &format!("{} · {}", c.type_label(), c.power_label()));
                                if c.power_state != "On" && small_link(ui, "Wake").clicked() {
                                    wake = Some(c.clone());
                                }
                            });
                        }
                    });
                    ui.add_space(space::M);
                }
            }
            Some(Ok(_)) => {
                let msg = format!("No consoles found. Check the {} setup.", self.platform_name());
                ui.vertical_centered(|ui| hint(ui, &msg));
            }
            Some(Err(e)) => {
                ui.vertical_centered(|ui| error_text(ui, e));
            }
            None => {
                ui.vertical_centered(|ui| {
                    spinner(ui);
                });
            }
        }
        ui.add_space(space::M);
        ui.vertical_centered(|ui| {
            if small_link(ui, "Look for consoles again").clicked() {
                refresh = true;
            }
        });
        if refresh {
            self.load_consoles();
        }
        if let Some(c) = wake {
            self.wake(c.server_id.clone(), c.name.clone());
        }
        if let Some(c) = pick {
            self.settings.last_console = c.server_id;
            self.save_settings();
            self.page = Page::Home;
        }
    }

    fn press_once_page(&mut self, ui: &mut Ui) {
        let cur = self.current;
        let p_name = self.profiles[cur].name.clone();
        title(
            ui,
            "Press once, not hold",
            "The button stays pressed until you press again.",
        );
        let mut changed = false;
        narrow(ui, 600.0, |ui| {
            let p = &mut self.profiles[cur];
            for li in 0..p.layers.len() {
                let lname = p.layers[li].name.clone();
                section(ui, &format!("{p_name} · {lname}"));
                group(ui, |ui| {
                    let mut first = true;
                    for b in p.layers[li].bindings.iter_mut() {
                        if b.input.is_empty()
                            || matches!(b.action, Action::None)
                            || b.input.starts_with("Wheel")
                        {
                            continue;
                        }
                        let sub = format!("{} · {}", input_label(&b.input), short_action(b.action));
                        let title = b.title().to_string();
                        control_row(ui, first, &title, &sub, |ui| {
                            let mut on = b.mode == BindMode::Toggle;
                            if switch(ui, &mut on).changed() {
                                b.mode = if on { BindMode::Toggle } else { BindMode::Hold };
                                changed = true;
                            }
                        });
                        first = false;
                    }
                });
            }
        });
        if changed {
            self.mark_dirty();
        }
    }

    // ------------------------------------------------------------------ profiles

    fn profiles_page(&mut self, ui: &mut Ui) {
        title(ui, "Profiles", "Pick the game you are playing.");
        let mut select = None;
        let mut changed = false;
        let mut cmd: Option<&str> = None;
        let cur = self.current;
        let names: Vec<(String, String, bool)> = self
            .profiles
            .iter()
            .map(|p| (p.name.clone(), p.game.clone(), p.builtin))
            .collect();
        let p = &mut self.profiles[cur];
        columns(ui, |ui, col| {
            if col == 0 {
                section(ui, "Which game?");
                group(ui, |ui| {
                    for (i, (name, game, builtin)) in names.iter().enumerate() {
                        let sub = if *builtin {
                            format!("{game} · built in")
                        } else if game.is_empty() {
                            "Custom".into()
                        } else {
                            game.clone()
                        };
                        if row(
                            ui,
                            i == 0,
                            name,
                            &sub,
                            RowOptions {
                                clickable: true,
                                lead: Lead::Radio(i == cur),
                                ..Default::default()
                            },
                            |_| {},
                        )
                        .clicked()
                        {
                            select = Some(i);
                        }
                    }
                });
            } else {
                section(ui, "This profile");
                group(ui, |ui| {
                    control_row(ui, true, "Name", "", |ui| {
                        changed |= text_field(
                            ui,
                            egui::TextEdit::singleline(&mut p.name)
                                .desired_width(220.0)
                                .font(medium(15.0)),
                        )
                        .changed();
                    });
                    control_row(ui, false, "Game", "", |ui| {
                        changed |= text_field(
                            ui,
                            egui::TextEdit::singleline(&mut p.game)
                                .desired_width(220.0)
                                .font(medium(15.0)),
                        )
                        .changed();
                    });
                    if !p.notes.is_empty() {
                        egui::Frame::new()
                            .inner_margin(Margin {
                                left: 15,
                                right: 15,
                                top: 0,
                                bottom: 12,
                            })
                            .show(ui, |ui| hint(ui, &p.notes));
                    }
                });
                section(ui, "Manage");
                group(ui, |ui| {
                    let items: [(&str, &str, &str, bool); 6] = [
                        ("new", "New profile", "Start from the action layout", false),
                        ("dup", "Duplicate", "Copy this profile", false),
                        ("import", "Import…", "Load a profile file", false),
                        ("export", "Export…", "Save this profile to a file", false),
                        ("folder", "Open profiles folder", "", false),
                        (
                            "delete",
                            if p.builtin {
                                "Reset to default"
                            } else {
                                "Delete"
                            },
                            "",
                            true,
                        ),
                    ];
                    for (i, (id, t, s, danger)) in items.into_iter().enumerate() {
                        if row(
                            ui,
                            i == 0,
                            t,
                            s,
                            RowOptions {
                                danger,
                                ..RowOptions::link()
                            },
                            |_| {},
                        )
                        .clicked()
                        {
                            cmd = Some(id);
                        }
                    }
                });
            }
        });
        if changed {
            self.mark_dirty();
        }
        if let Some(i) = select {
            self.select_profile(i);
        }
        match cmd {
            Some("new") => {
                let base = self
                    .profiles
                    .iter()
                    .find(|p| p.id == "generic-third-person")
                    .cloned()
                    .unwrap_or_else(|| self.profiles[0].clone());
                self.add_profile(base, "My profile");
            }
            Some("dup") => {
                self.flush_profile();
                let p = self.profile().clone();
                let name = format!("{} (copy)", p.name);
                self.add_profile(p, &name);
            }
            Some("import") => {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Profile", &["json"])
                    .pick_file()
                {
                    self.flush_profile();
                    match self.storage.import_profile(&path) {
                        Ok(p) => {
                            self.profiles = self.storage.profiles();
                            let i = self.profiles.iter().position(|x| x.id == p.id).unwrap_or(0);
                            self.select_profile(i);
                            self.toast("Profile imported", false);
                        }
                        Err(e) => self.toast(format!("Import failed: {e}"), true),
                    }
                }
            }
            Some("export") => {
                self.flush_profile();
                let p = self.profile().clone();
                if let Some(path) = rfd::FileDialog::new()
                    .set_file_name(format!("{}.json", p.id))
                    .add_filter("Profile", &["json"])
                    .save_file()
                {
                    match serde_json::to_string_pretty(&p)
                        .map_err(anyhow::Error::from)
                        .and_then(|s| std::fs::write(path, s).map_err(anyhow::Error::from))
                    {
                        Ok(()) => self.toast("Profile exported", false),
                        Err(e) => self.toast(format!("Export failed: {e}"), true),
                    }
                }
            }
            Some("folder") => open_folder(&self.storage.profile_dir),
            Some("delete") => {
                let p = self.profile().clone();
                let q = if p.builtin {
                    format!("Reset “{}” to its original settings?", p.name)
                } else {
                    format!("Delete “{}”?", p.name)
                };
                if confirm(&q) {
                    self.editor.dirty_since = None;
                    self.storage.delete_profile(&p.id);
                    self.profiles = self.storage.profiles();
                    let i = self.profiles.iter().position(|x| x.id == p.id).unwrap_or(0);
                    self.select_profile(i);
                }
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------------ keys

    fn keys_page(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let cur = self.current;
        let frame_no = self.frame_no;
        if let Some((li, bi, started)) = self.editor.listening {
            if frame_no > started + 1 {
                if let Some(code) = next_input(ctx) {
                    self.editor.listening = None;
                    if code != "__cancel" {
                        if let Some(b) = self.profiles[cur]
                            .layers
                            .get_mut(li)
                            .and_then(|l| l.bindings.get_mut(bi))
                        {
                            b.input = code;
                        }
                        self.mark_dirty();
                    }
                }
                // Every key, button and wheel event brings a frame by itself; this is only
                // a slow safety net (it used to redraw the page flat out while waiting).
                ctx.request_repaint_after(Duration::from_millis(100));
            } else {
                reset_next_input(ctx);
                ctx.request_repaint();
            }
        }

        if self.editor.layer >= self.profiles[cur].layers.len() {
            self.editor.layer = 0;
        }
        let li = self.editor.layer;
        let layer_names: Vec<String> = self.profiles[cur]
            .layers
            .iter()
            .map(|l| l.name.clone())
            .collect();
        let p_name = self.profiles[cur].name.clone();

        // Header: layer tabs on the left, Picture / List on the right.
        ui.add_space(14.0);
        let mut new_layer_sel: Option<usize> = None;
        let mut new_view: Option<bool> = None;
        let items: Vec<(String, Option<String>)> = layer_names
            .iter()
            .map(|n| (n.clone(), self.switch_key_for(cur, n)))
            .collect();
        ui.horizontal(|ui| {
            if let Some(i) = segmented(ui, "layers", &items, li) {
                new_layer_sel = Some(i);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let views = [("List".to_string(), None), ("Picture".to_string(), None)];
                if let Some(v) = segmented(
                    ui,
                    "view",
                    &views,
                    if self.editor.list_view { 0 } else { 1 },
                ) {
                    new_view = Some(v == 0);
                }
            });
        });
        if let Some(i) = new_layer_sel {
            if i != li {
                self.editor.layer = i;
                self.editor.selected = Some(0);
                self.editor.listening = None;
            }
        }
        if let Some(v) = new_view {
            self.editor.list_view = v;
            // A key pick that was waiting belongs to the list: don't let it grab the next
            // click on the drawn keyboard.
            self.editor.listening = None;
        }
        ui.add_space(10.0);
        if !self.editor.list_view {
            let li = self.editor.layer;
            self.keys_picture(ui, li);
            return;
        }
        title(
            ui,
            "Keys",
            &format!("{p_name} · {}", layer_names[self.editor.layer]),
        );
        let li = self.editor.layer;

        let listening = self.editor.listening;
        let mut changed = false;
        let mut select: Option<usize> = None;
        let mut listen: Option<usize> = None;
        let mut remove: Option<usize> = None;
        let mut add = false;
        let mut add_layer = false;
        let mut remove_layer = false;
        let mut test = false;
        let sel = self.editor.selected;
        let new_layer = &mut self.editor.new_layer;
        let p = &mut self.profiles[cur];
        let base_name = layer_names[0].clone();
        columns(ui, |ui, col| {
            if col == 0 {
                section(ui, "Layout");
                group(ui, |ui| {
                    if p.layers[li].bindings.is_empty() {
                        row(
                            ui,
                            true,
                            "No keys yet",
                            "Add one on the right.",
                            RowOptions::default(),
                            |_| {},
                        );
                    }
                    for (bi, b) in p.layers[li].bindings.iter().enumerate() {
                        let mut sub = if b.label.is_empty() {
                            mode_text(b.mode).to_string()
                        } else {
                            format!("{} · {}", short_action(b.action), mode_text(b.mode))
                        };
                        if let Some(l) = &b.switch_layer {
                            sub = format!("{sub} · then {l}");
                        }
                        let key = if b.input.is_empty() {
                            "—".to_string()
                        } else {
                            input_label(&b.input)
                        };
                        let r = row(
                            ui,
                            bi == 0,
                            &b.title(),
                            &sub,
                            RowOptions {
                                clickable: true,
                                selected: sel == Some(bi),
                                ..Default::default()
                            },
                            |ui| {
                                keycap(ui, &key);
                            },
                        );
                        if r.clicked() {
                            select = Some(bi);
                        }
                    }
                });
                if li > 0 {
                    hint_indented(
                        ui,
                        &format!("Keys not listed here use the “{base_name}” layer."),
                    );
                }
            } else {
                if let Some(bi) = sel.filter(|bi| *bi < p.layers[li].bindings.len()) {
                    let names = layer_names.clone();
                    let b = &mut p.layers[li].bindings[bi];
                    section(ui, "Edit key");
                    group(ui, |ui| {
                        let is_listening = listening
                            .map(|(l, i, _)| l == li && i == bi)
                            .unwrap_or(false);
                        control_row(ui, true, "Name", "What it does in the game.", |ui| {
                            changed |= text_field(
                                ui,
                                egui::TextEdit::singleline(&mut b.label)
                                    .hint_text("e.g. Shoot")
                                    .desired_width(200.0)
                                    .font(medium(15.0)),
                            )
                            .changed();
                        });
                        row(
                            ui,
                            false,
                            "Key",
                            if is_listening {
                                "Press a key, click or scroll."
                            } else {
                                "Click to change"
                            },
                            RowOptions::default(),
                            |ui| {
                                let label = if is_listening {
                                    "…".to_string()
                                } else if b.input.is_empty() {
                                    "Set".into()
                                } else {
                                    input_label(&b.input)
                                };
                                if keycap_sized(ui, &label, mono(14.0), 30.0, is_listening, true)
                                    .clicked()
                                    && !is_listening
                                {
                                    listen = Some(bi);
                                }
                            },
                        );
                        control_row(ui, false, "Controller", "", |ui| {
                            egui::ComboBox::from_id_salt(("act", li, bi))
                                .width(200.0)
                                .selected_text(b.action.label())
                                .show_ui(ui, |ui| {
                                    for a in Action::ALL {
                                        changed |= ui
                                            .selectable_value(&mut b.action, a, a.label())
                                            .changed();
                                    }
                                });
                        });
                        control_row(ui, false, "Mode", mode_hint(b.mode), |ui| {
                            egui::ComboBox::from_id_salt(("mode", li, bi))
                                .width(200.0)
                                .selected_text(b.mode.label())
                                .show_ui(ui, |ui| {
                                    for m in BindMode::ALL {
                                        changed |= ui
                                            .selectable_value(&mut b.mode, m, m.label())
                                            .changed();
                                    }
                                });
                        });
                        control_row(ui, false, "Then switch to", "Change layer on press", |ui| {
                            let current = b.switch_layer.clone().unwrap_or_else(|| "Stay".into());
                            egui::ComboBox::from_id_salt(("layer", li, bi))
                                .width(200.0)
                                .selected_text(current)
                                .show_ui(ui, |ui| {
                                    changed |= ui
                                        .selectable_value(&mut b.switch_layer, None, "Stay")
                                        .changed();
                                    for n in &names {
                                        changed |= ui
                                            .selectable_value(
                                                &mut b.switch_layer,
                                                Some(n.clone()),
                                                n,
                                            )
                                            .changed();
                                    }
                                });
                        });
                        if row(
                            ui,
                            false,
                            "Remove this key",
                            "",
                            RowOptions {
                                danger: true,
                                ..RowOptions::link()
                            },
                            |_| {},
                        )
                        .clicked()
                        {
                            remove = Some(bi);
                        }
                    });
                }
                section(ui, "Set up");
                group(ui, |ui| {
                    add = row(
                        ui,
                        true,
                        "Add key",
                        "Then press the key you want",
                        RowOptions::link(),
                        |_| {},
                    )
                    .clicked();
                    control_row(ui, false, "New layer", "e.g. Vehicle", |ui| {
                        add_layer =
                            keycap_sized(ui, "Add", bold(14.0), 28.0, false, true).clicked();
                        text_field(
                            ui,
                            egui::TextEdit::singleline(new_layer)
                                .hint_text("Name")
                                .desired_width(120.0)
                                .font(medium(15.0)),
                        );
                    });
                    if li > 0 {
                        remove_layer = row(
                            ui,
                            false,
                            "Remove this layer",
                            "",
                            RowOptions {
                                danger: true,
                                ..RowOptions::link()
                            },
                            |_| {},
                        )
                        .clicked();
                    }
                    test = row(
                        ui,
                        false,
                        "Test",
                        "Try your keys without the console",
                        RowOptions::link(),
                        |_| {},
                    )
                    .clicked();
                });
                hint_indented(ui, "While choosing a key: Esc cancels, Shift+Esc binds Esc.");
            }
        });

        let p = &mut self.profiles[cur];
        if let Some(bi) = select {
            self.editor.selected = Some(bi);
        }
        if let Some(bi) = listen {
            self.editor.listening = Some((li, bi, frame_no));
        }
        if let Some(bi) = remove {
            p.layers[li].bindings.remove(bi);
            self.editor.selected = Some(bi.saturating_sub(1));
            changed = true;
        }
        if add {
            p.layers[li].bindings.push(Binding {
                input: String::new(),
                action: Action::A,
                mode: BindMode::Hold,
                switch_layer: None,
                label: String::new(),
            });
            let bi = p.layers[li].bindings.len() - 1;
            self.editor.selected = Some(bi);
            self.editor.listening = Some((li, bi, frame_no));
            changed = true;
        }
        if add_layer {
            let name = self.editor.new_layer.trim().to_string();
            if !name.is_empty() && !p.layers.iter().any(|l| l.name == name) {
                p.layers.push(Layer {
                    name,
                    bindings: vec![],
                });
                self.editor.layer = p.layers.len() - 1;
                self.editor.selected = None;
                changed = true;
            }
            self.editor.new_layer.clear();
        }
        if remove_layer && li > 0 {
            let name = p.layers[li].name.clone();
            p.layers.remove(li);
            for l in &mut p.layers {
                for b in &mut l.bindings {
                    if b.switch_layer.as_deref() == Some(name.as_str()) {
                        b.switch_layer = None;
                    }
                }
            }
            self.editor.layer = 0;
            changed = true;
        }
        if changed {
            self.mark_dirty();
        }
        if test {
            self.page = Page::Test;
            self.came_from_keys = true;
        }
    }

    /// The key (if any) that switches to layer `name`, for the layer tab badge.
    fn switch_key_for(&self, cur: usize, name: &str) -> Option<String> {
        self.profiles[cur]
            .layers
            .iter()
            .flat_map(|l| l.bindings.iter())
            .find(|b| b.switch_layer.as_deref() == Some(name) && !b.input.is_empty())
            .map(|b| input_label(&b.input))
    }

    fn keys_picture(&mut self, ui: &mut Ui, li: usize) {
        let cur = self.current;
        let defs = crate::keyboard::layout();
        // Keyboard and panel side by side when the keyboard stays readable; in a narrow
        // window the panel goes underneath instead of being cut off at the right edge.
        let avail = ui.available_width();
        let stacked = avail - 390.0 - 28.0 < 560.0;
        let (kb_w, panel_w) = if stacked {
            (avail, avail.min(560.0))
        } else {
            (avail - 390.0 - 28.0, 390.0)
        };
        let arrangement = if stacked {
            egui::Layout::top_down(egui::Align::Min)
        } else {
            egui::Layout::left_to_right(egui::Align::Min)
        };
        let mut clicked: Option<String> = None;
        let mut cmd: Option<PanelCmd> = None;
        let sel = self.editor.key_sel.clone();
        {
            let Self {
                profiles, editor, ..
            } = self;
            let p = &profiles[cur];
            let base = &p.layers[0];
            let layer = &p.layers[li];
            let find = |code: &str| -> Option<(Binding, bool)> {
                layer
                    .bindings
                    .iter()
                    .find(|b| b.input == code)
                    .map(|b| (b.clone(), false))
                    .or_else(|| {
                        if li > 0 {
                            base.bindings
                                .iter()
                                .find(|b| b.input == code)
                                .map(|b| (b.clone(), true))
                        } else {
                            None
                        }
                    })
            };
            ui.allocate_ui_with_layout(vec2(avail, 0.0), arrangement, |ui| {
                ui.vertical(|ui| {
                    ui.set_width(kb_w);
                    // White (bound) only when the key really does something in this layer, the
                    // same way the mapper resolves it; a key set to "Nothing" shows as unused.
                    let look = |d: &crate::keyboard::KeyDef| match find(d.code) {
                        Some((b, inherited)) if b.does_something() => crate::keyboard::KeyLook {
                            bound: true,
                            action: if inherited {
                                format!("{} ·", b.title())
                            } else {
                                b.title().to_string()
                            },
                            pad: chip_text(b.action),
                        },
                        _ => crate::keyboard::KeyLook {
                            bound: false,
                            action: String::new(),
                            pad: String::new(),
                        },
                    };
                    if let Some(code) = crate::keyboard::draw(ui, kb_w, defs, look, sel.as_deref())
                    {
                        clicked = Some(code.to_string());
                    }
                    ui.add_space(8.0);
                    // Mouse
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
                        ui.label(RichText::new("MOUSE").font(mono(12.5)).color(LABEL));
                        let moves = if !p.mouse.enabled {
                            "Nothing"
                        } else if p.mouse.stick == Stick::Right {
                            "Look around and aim"
                        } else {
                            "Move the character"
                        };
                        keycap_sized(ui, "Move the mouse", mono(13.0), 26.0, false, false);
                        ui.label(RichText::new(moves).font(bold(15.0)).color(INK));
                        ui.add_space(8.0);
                        for code in [
                            "Mouse0",
                            "Mouse2",
                            "Mouse1",
                            "Mouse3",
                            "Mouse4",
                            "WheelUp",
                            "WheelDown",
                        ] {
                            let found = find(code);
                            let r = keycap_sized(
                                ui,
                                &input_label(code),
                                mono(13.0),
                                26.0,
                                sel.as_deref() == Some(code),
                                true,
                            );
                            match &found {
                                Some((b, _)) if b.does_something() => {
                                    ui.label(RichText::new(b.title()).font(bold(15.0)).color(INK))
                                }
                                // Set to "Nothing" in this layer (e.g. "Unused in vehicles").
                                Some((b, _)) => {
                                    ui.label(RichText::new(b.title()).font(body(15.0)).color(MUTED))
                                }
                                None => {
                                    ui.label(RichText::new("free").font(body(15.0)).color(MUTED))
                                }
                            };
                            if r.clicked() {
                                clicked = Some(code.to_string());
                            }
                            ui.add_space(6.0);
                        }
                    });
                    ui.add_space(6.0);
                    crate::keyboard::legend(ui);
                    let (mut left, mut right) = (0, 0);
                    for d in defs {
                        if d.usable == crate::keyboard::Usable::Yes
                            && find(d.code).is_some_and(|(b, _)| b.does_something())
                        {
                            if d.x < 7.0 {
                                left += 1
                            } else {
                                right += 1
                            }
                        }
                    }
                    let mouse = ["Mouse0", "Mouse1", "Mouse2", "Mouse3", "Mouse4", "WheelUp", "WheelDown"]
                        .iter()
                        .filter(|c| find(c).is_some_and(|(b, _)| b.does_something()))
                        .count();
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(format!(
                            "Left hand: {left} keys · Right hand: {right} keys · Mouse: {mouse}"
                        ))
                        .font(bold(15.5))
                        .color(INK),
                    );
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new("Always available:")
                                .font(bold(14.5))
                                .color(INK),
                        );
                        keycap(ui, "F8");
                        hint(ui, "release mouse");
                        keycap(ui, "F6");
                        hint(ui, "deadzone finder");
                        keycap(ui, "F11");
                        hint(ui, "full screen");
                    });
                });
                ui.add_space(20.0);
                ui.vertical(|ui| {
                    ui.set_width(panel_w);
                    cmd = key_panel(ui, p, li, sel.as_deref(), &mut editor.search);
                });
            });
        }
        if let Some(c) = clicked {
            self.editor.key_sel = Some(c);
            self.editor.search.clear();
        }
        if let (Some(cmd), Some(code)) = (cmd, sel) {
            // In an upper layer a key without a binding falls back to the base layer's.
            let base_binds_it = li > 0
                && self.profiles[cur].layers[0]
                    .bindings
                    .iter()
                    .any(|b| b.input == code && b.action != Action::None);
            let layer = &mut self.profiles[cur].layers[li];
            let existing = layer.bindings.iter().position(|b| b.input == code);
            match cmd {
                PanelCmd::Close => self.editor.key_sel = None,
                PanelCmd::Assign(a) => match existing {
                    Some(i) => {
                        if layer.bindings[i].action != a {
                            layer.bindings[i].action = a;
                            layer.bindings[i].label.clear();
                        }
                    }
                    None => layer.bindings.push(Binding {
                        input: code.clone(),
                        action: a,
                        mode: BindMode::Hold,
                        switch_layer: None,
                        label: String::new(),
                    }),
                },
                // Freeing it there would bring that base binding back, so keep the key
                // bound to "nothing" in this layer instead.
                PanelCmd::Free if base_binds_it => {
                    layer.bindings.retain(|b| b.input != code);
                    layer.bindings.push(Binding {
                        input: code.clone(),
                        action: Action::None,
                        mode: BindMode::Hold,
                        switch_layer: None,
                        label: format!("Unused in {}", layer.name.to_lowercase()),
                    });
                }
                PanelCmd::Free => layer.bindings.retain(|b| b.input != code),
                PanelCmd::Mode(m) => {
                    if let Some(i) = existing {
                        layer.bindings[i].mode = m;
                    }
                }
                PanelCmd::Name(n) => {
                    if let Some(i) = existing {
                        layer.bindings[i].label = n;
                    }
                }
                PanelCmd::Switch(l) => {
                    if let Some(i) = existing {
                        layer.bindings[i].switch_layer = l;
                    }
                }
            }
            self.mark_dirty();
        }
    }

    // ------------------------------------------------------------------ mouse

    fn mouse_page(&mut self, ui: &mut Ui) {
        let cur = self.current;
        let name = self.profiles[cur].name.clone();
        title(ui, "Sens & aim", &name);
        let mut changed = false;
        let mut restore = false;
        let mut toggle_more = false;
        let more = self.editor.more_mouse;
        let p = &mut self.profiles[cur];
        let step = |v: &mut f32, d: i32, by: f32, lo: f32, hi: f32| -> bool {
            if d == 0 {
                return false;
            }
            *v = ((*v + d as f32 * by).clamp(lo, hi) * 100.0).round() / 100.0;
            true
        };
        narrow(ui, 680.0, |ui| {
            group(ui, |ui| {
                let m = &mut p.mouse;
                control_row(ui, true, "Sens in this game", "", |ui| {
                    let d = stepper(ui, &format!("{:.2}", m.sensitivity));
                    changed |= step(&mut m.sensitivity, d, 0.02, 0.05, 2.0);
                });
                control_row(ui, false, "While aiming (ADS)", "", |ui| {
                    let d = stepper(ui, &format!("{:.2} ×", m.aim_multiplier));
                    changed |= step(&mut m.aim_multiplier, d, 0.05, 0.1, 2.0);
                });
                control_row(
                    ui,
                    false,
                    "Deadzone",
                    "Find it in game: F6, then [ and ].",
                    |ui| {
                        let d = stepper(ui, &format!("{:.2}", m.deadzone));
                        changed |= step(&mut m.deadzone, d, 0.01, 0.0, 0.6);
                    },
                );
                control_row(
                    ui,
                    false,
                    "Fine aim",
                    "Higher = slower small movements, more precise.",
                    |ui| {
                        let d = stepper(ui, &format!("{:.2}", m.curve));
                        changed |= step(&mut m.curve, d, 0.05, 0.4, 2.5);
                    },
                );
                control_row(ui, false, "Steady hand", "", |ui| {
                    let levels = [0.0f32, 0.25, 0.5];
                    let cur_i = levels
                        .iter()
                        .enumerate()
                        // total_cmp: a profile file with a broken number must not crash the page.
                        .min_by(|a, b| {
                            (a.1 - m.smoothing)
                                .abs()
                                .total_cmp(&(b.1 - m.smoothing).abs())
                        })
                        .map(|x| x.0)
                        .unwrap_or(0);
                    let items: Vec<(String, Option<String>)> = ["Off", "Light", "Strong"]
                        .iter()
                        .map(|s| (s.to_string(), None))
                        .collect();
                    {
                        if let Some(i) = segmented(ui, "steady", &items, cur_i) {
                            m.smoothing = levels[i];
                            changed = true;
                        }
                    }
                });
                control_row(ui, false, "Invert Y-axis", "", |ui| {
                    changed |= switch(ui, &mut m.invert_y).changed();
                });
                control_row(
                    ui,
                    false,
                    "Mouse moves the character",
                    "For games without a camera. Else leave off.",
                    |ui| {
                        let mut left = m.stick == Stick::Left;
                        if switch(ui, &mut left).changed() {
                            m.stick = if left { Stick::Left } else { Stick::Right };
                            changed = true;
                        }
                    },
                );
            });
            ui.vertical_centered(|ui| {
                hint(
                    ui,
                    "Tip: set the deadzone first, then the sensitivity. Test it under More > Test.",
                );
            });
            ui.add_space(8.0);
            group(ui, |ui| {
                if row(
                    ui,
                    true,
                    "More settings",
                    "",
                    RowOptions {
                        clickable: true,
                        ..Default::default()
                    },
                    |ui| {
                        ui.label(
                            RichText::new(if more { "Hide" } else { "Show" })
                                .font(bold(14.0))
                                .color(INK),
                        );
                    },
                )
                .clicked()
                {
                    toggle_more = true;
                }
                if more {
                    let m = &mut p.mouse;
                    control_row(
                        ui,
                        false,
                        "Mouse controls a stick",
                        "Off for menu-only profiles.",
                        |ui| {
                            changed |= switch(ui, &mut m.enabled).changed();
                        },
                    );
                    control_row(
                        ui,
                        false,
                        "Aiming button",
                        "Which button counts as aiming.",
                        |ui| {
                            egui::ComboBox::from_id_salt("aim")
                                .width(190.0)
                                .selected_text(m.aim_button.label())
                                .show_ui(ui, |ui| {
                                    for a in Action::BUTTONS {
                                        changed |= ui
                                            .selectable_value(&mut m.aim_button, a, a.label())
                                            .changed();
                                    }
                                });
                        },
                    );
                    control_row(
                        ui,
                        false,
                        "Vertical speed",
                        "Relative to horizontal.",
                        |ui| {
                            let d = stepper(ui, &format!("{:.2} ×", m.yx_ratio));
                            changed |= step(&mut m.yx_ratio, d, 0.05, 0.3, 2.0);
                        },
                    );
                    control_row(ui, false, "Deadzone shape", "", |ui| {
                        let items: Vec<(String, Option<String>)> = ["Round", "Square"]
                            .iter()
                            .map(|s| (s.to_string(), None))
                            .collect();
                        {
                            let cur_i = if m.deadzone_shape == DeadzoneShape::Circle {
                                0
                            } else {
                                1
                            };
                            if let Some(i) = segmented(ui, "dzshape", &items, cur_i) {
                                m.deadzone_shape = if i == 0 {
                                    DeadzoneShape::Circle
                                } else {
                                    DeadzoneShape::Square
                                };
                                changed = true;
                            }
                        }
                    });
                    control_row(ui, false, "Gap fill", "For low polling-rate mice.", |ui| {
                        let d = stepper(ui, &format!("{:.0} ms", m.hold_ms));
                        changed |= step(&mut m.hold_ms, d, 5.0, 0.0, 60.0);
                    });
                    control_row(
                        ui,
                        false,
                        "Walk speed",
                        "Left stick while Walk is held.",
                        |ui| {
                            let d = stepper(ui, &format!("{:.0} %", p.walk_magnitude * 100.0));
                            changed |= step(&mut p.walk_magnitude, d, 0.05, 0.1, 1.0);
                        },
                    );
                    restore = row(
                        ui,
                        false,
                        "Restore defaults",
                        "",
                        RowOptions::link(),
                        |_| {},
                    )
                    .clicked();
                }
            });
        });
        if toggle_more {
            self.editor.more_mouse = !more;
        }
        let m = &mut self.profiles[cur].mouse;
        if restore {
            *m = MouseSettings {
                enabled: m.enabled,
                stick: m.stick,
                ..Default::default()
            };
            changed = true;
        }
        if changed {
            self.mark_dirty();
        }
    }

    // ------------------------------------------------------------------ test

    fn stop_test(&mut self) {
        if let Some(t) = &self.editor.test {
            if t.captured {
                release_capture(&self.ctx);
            }
        }
        self.editor.test = None;
    }

    fn test_page(&mut self, ui: &mut Ui, ctx: &egui::Context) {
        let profile = self.profile().clone();
        title(
            ui,
            "Test",
            &format!("{} · nothing is sent to the console", profile.name),
        );
        if self.editor.test.is_none() {
            self.editor.test = Some(TestPad {
                mapper: Mapper::new(profile),
                captured: false,
                mods: Default::default(),
                recording: None,
                result: String::new(),
            });
        }
        let mut new_sens: Option<f32> = None;
        let t = self.editor.test.as_mut().unwrap();
        let now = ctx.input(|i| i.time) * 1000.0;
        let res = feed_input(ctx, &mut t.mapper, t.captured, &mut t.mods, now);
        if t.captured && (res.lost_focus || res.hotkeys.contains(&KEY_RELEASE)) {
            t.captured = false;
            t.mapper.reset();
            release_capture(ctx);
        }
        let frame = t.mapper.tick(now);
        if let Some((started, speeds)) = &mut t.recording {
            if t.mapper.last_tick_moved {
                speeds.push(t.mapper.last_speed);
            }
            if started.elapsed() > Duration::from_secs(5) {
                match suggest_sensitivity(speeds) {
                    Some(s) => {
                        let v = s.sensitivity.clamp(0.05, 2.0);
                        new_sens = Some(v);
                        t.result = format!("Sensitivity set to {v:.2}. Your fast turns ≈ {:.1} counts/ms. Fine-tune it in game.", s.p95);
                    }
                    None => {
                        t.result =
                            "Not enough movement recorded. Try again and move the mouse more."
                                .into()
                    }
                }
                t.recording = None;
            }
        }

        narrow(ui, 640.0, |ui| {
            section(
                ui,
                if t.captured {
                    "Captured"
                } else {
                    "Click the controller to start"
                },
            );
            let resp = group(ui, |ui| {
                egui::Frame::new()
                    .inner_margin(Margin::symmetric(16, 18))
                    .show(ui, |ui| {
                        ui.vertical_centered(|ui| {
                            pad_view(ui, &frame, 420.0);
                            ui.add_space(8.0);
                            let line = if t.captured {
                                format!(
                                    "Layer: {}  ·  F8 releases the mouse",
                                    t.mapper.layer_name()
                                )
                            } else {
                                "Your keys and mouse light up the controller.".to_string()
                            };
                            ui.label(RichText::new(line).font(mono(12.5)).color(LABEL));
                        });
                    })
                    .response
            });
            if t.captured {
                ui.painter().rect_stroke(
                    resp.rect.expand(2.0),
                    CornerRadius::same(17),
                    Stroke::new(2.0, SIGNAL),
                    egui::StrokeKind::Outside,
                );
            }
            let r = ui.interact(
                resp.rect,
                ui.scope_id().with("testpad"),
                egui::Sense::click(),
            );
            if r.clicked() && !t.captured {
                t.captured = true;
                t.mods = ctx.input(|i| i.modifiers);
                grab_capture(ctx);
            }
            ui.add_space(10.0);
            let recording = t.recording.is_some();
            centered_row(ui, 280.0, |ui| {
                let label = match &t.recording {
                    Some((s, _)) => {
                        format!("Recording… {}s", 5u64.saturating_sub(s.elapsed().as_secs()))
                    }
                    None => "Suggest sensitivity".to_string(),
                };
                if button3d(ui, &label, 280.0, SIGNAL, SIGNAL_EDGE, !recording).clicked()
                    && !recording
                {
                    if t.captured {
                        t.recording = Some((Instant::now(), vec![]));
                        t.result.clear();
                    } else {
                        t.result = "Click the controller first to capture your mouse.".into();
                    }
                }
            });
            ui.add_space(6.0);
            ui.vertical_centered(|ui| {
                if recording {
                    hint(
                        ui,
                        "Do your fastest comfortable camera turns, left and right.",
                    );
                } else if !t.result.is_empty() {
                    hint(ui, &t.result);
                }
            });
        });
        if t.captured || t.recording.is_some() {
            ctx.request_repaint();
        }
        if let Some(v) = new_sens {
            self.profiles[self.current].mouse.sensitivity = v;
            self.mark_dirty();
        }
    }

    // ------------------------------------------------------------------ picture / settings

    fn picture_page(&mut self, ui: &mut Ui) {
        title(ui, "Picture and connection", "Only if needed.");
        let mut changed = false;
        // Frame generation: which files are installed, and what the user asked for below.
        #[cfg(windows)]
        let fg_have = {
            use crate::framegen::{dll_dir, Kind};
            [
                dll_dir(Kind::Fsr, &self.storage.dir).is_some(),
                dll_dir(Kind::Dlss, &self.storage.dir).is_some(),
            ]
        };
        #[cfg(windows)]
        let mut fg_install: Option<crate::framegen::Kind> = None;
        let fg_confirm_dlss = &mut self.fg_confirm_dlss;
        let s = &mut self.settings;
        let Settings {
            tv_bitrate,
            pc_bitrate,
            pc_high_quality,
            pc_smooth_scaling,
            pc_rtx_vsr,
            pc_gpu_decode,
            pc_cleanup,
            pc_cleanup_strong,
            pc_sharpen,
            pc_sharpen_amount,
            pc_colour,
            pc_vibrance,
            pc_contrast,
            pc_brightness,
            pc_smooth_motion,
            pc_fsr_fg,
            pc_dlss_fg,
            pc_sharp_upscale,
            input_hz,
            show_hud,
            xbox_console_ip,
            ..
        } = s;
        columns(ui, |ui, col| {
            if col == 0 {
                section(ui, "TV Play");
                group(ui, |ui| {
                    control_row(
                        ui,
                        true,
                        "Background stream",
                        "Keeps the link alive. Your TV picture is not affected.",
                        |ui| {
                            changed |= slider_u32(ui, tv_bitrate, 500..=6000, 100.0, |v| {
                                format!("{v} kbps")
                            });
                        },
                    );
                });
                section(ui, "Input");
                group(ui, |ui| {
                    control_row(
                        ui,
                        true,
                        "Update rate",
                        "How often the controller state is sent.",
                        |ui| {
                            let label = if *input_hz >= 120 { "120 Hz" } else { "60 Hz" };
                            egui::ComboBox::from_id_salt("hz")
                                .width(150.0)
                                .selected_text(label)
                                .show_ui(ui, |ui| {
                                    changed |= ui.selectable_value(input_hz, 60, "60 Hz").changed();
                                    changed |=
                                        ui.selectable_value(input_hz, 120, "120 Hz").changed();
                                });
                        },
                    );
                    control_row(
                        ui,
                        false,
                        "Overlay while playing",
                        "F7 shows or hides it.",
                        |ui| {
                            changed |= switch(ui, show_hud).changed();
                        },
                    );
                });
                section(ui, "Xbox connection");
                group(ui, |ui| {
                    control_row(
                        ui,
                        true,
                        "Xbox IP address",
                        "Leave empty to find it on your network. Type it in (Xbox: Settings > General > Network settings > Advanced settings) if connecting fails.",
                        |ui| {
                            changed |= ui
                                .add(
                                    egui::TextEdit::singleline(xbox_console_ip)
                                        .hint_text("automatic")
                                        .desired_width(150.0),
                                )
                                .changed();
                        },
                    );
                });
                section(ui, "PC Play picture");
                group(ui, |ui| {
                    control_row(
                        ui,
                        true,
                        "Clean up compression",
                        "Softens the blocks, bands and noise of the video stream. Text and edges stay sharp.",
                        |ui| {
                            changed |= switch(ui, pc_cleanup).changed();
                        },
                    );
                    if *pc_cleanup {
                        control_row(ui, false, "Clean-up strength", "", |ui| {
                            egui::ComboBox::from_id_salt("pcclean")
                                .width(150.0)
                                .selected_text(if *pc_cleanup_strong { "Strong" } else { "Light" })
                                .show_ui(ui, |ui| {
                                    changed |= ui
                                        .selectable_value(pc_cleanup_strong, false, "Light")
                                        .changed();
                                    changed |= ui
                                        .selectable_value(pc_cleanup_strong, true, "Strong")
                                        .changed();
                                });
                        });
                    }
                    control_row(
                        ui,
                        false,
                        "Sharpen",
                        "Brings out fine detail without halos around edges.",
                        |ui| {
                            changed |= switch(ui, pc_sharpen).changed();
                        },
                    );
                    if *pc_sharpen {
                        control_row(ui, false, "Sharpness", "", |ui| {
                            changed |= slider_f32(ui, pc_sharpen_amount, 0.0..=1.0, |v| {
                                format!("{:.0}%", v * 100.0)
                            });
                        });
                    }
                    control_row(
                        ui,
                        false,
                        "Colour",
                        "Vibrance lifts dull colours and leaves skin tones alone.",
                        |ui| {
                            changed |= switch(ui, pc_colour).changed();
                        },
                    );
                    if *pc_colour {
                        control_row(ui, false, "Vibrance", "", |ui| {
                            changed |= slider_f32(ui, pc_vibrance, 0.0..=1.0, |v| {
                                format!("{:.0}%", v * 100.0)
                            });
                        });
                        control_row(ui, false, "Contrast", "", |ui| {
                            changed |= slider_f32(ui, pc_contrast, -0.3..=0.3, |v| {
                                format!("{:+.0}%", v * 100.0)
                            });
                        });
                        control_row(ui, false, "Brightness", "", |ui| {
                            changed |= slider_f32(ui, pc_brightness, -0.2..=0.2, |v| {
                                format!("{:+.0}%", v * 100.0)
                            });
                        });
                    }
                    control_row(
                        ui,
                        false,
                        "Smooth motion",
                        "Adds in-between frames so 30 fps games look like 60. Adds about one frame of input lag (≈17 ms in a 30 fps game, ≈8 ms at 60 fps). A 60 fps game needs a 120 Hz screen.",
                        |ui| {
                            changed |= switch(ui, pc_smooth_motion).changed();
                        },
                    );
                    #[cfg(windows)]
                    {
                        use crate::framegen::install::{self, State};
                        use crate::framegen::Kind;
                        let before = (*pc_fsr_fg, *pc_dlss_fg);
                        let nvidia = crate::framegen::nvidia_present();
                        let rows: [(Kind, &mut bool, bool, &str, &str, &str); 2] = [
                            (
                                Kind::Fsr,
                                &mut *pc_fsr_fg,
                                fg_have[0],
                                "Frame generation (FSR)",
                                "AMD FSR adds one in-between frame per game frame (2X: 30 fps looks like 60) on any modern graphics card. Adds about half a game frame of input lag; a 60 fps game needs a 120 Hz screen. Runs while you play; the key overlay is hidden meanwhile.",
                                "Needs AMD's files once (30 MB, from AMD's GitHub page).",
                            ),
                            (
                                Kind::Dlss,
                                &mut *pc_dlss_fg,
                                fg_have[1],
                                "Frame generation (DLSS)",
                                "NVIDIA DLSS adds one in-between frame per game frame (2X). Needs a GeForce RTX 40 or 50 and the player window in front. Adds about half a game frame of input lag; a 60 fps game needs a 120 Hz screen. Runs while you play; the key overlay is hidden meanwhile.",
                                "Needs NVIDIA's files once (a 280 MB download from NVIDIA's GitHub page).",
                            ),
                        ];
                        for (kind, on, have, name, about, needs) in rows {
                            let state = install::state(kind);
                            let unsupported = kind == Kind::Dlss && !nvidia;
                            let confirming = kind == Kind::Dlss && *fg_confirm_dlss && !have;
                            let sub = match &state {
                                State::Working(what) => what.clone(),
                                _ if unsupported => {
                                    "Needs an NVIDIA GeForce RTX 40 or 50 graphics card.".to_string()
                                }
                                _ if confirming => "These files are NVIDIA's own, under NVIDIA's licence (NVIDIA RTX SDKs License), and are downloaded from NVIDIA's GitHub page. They are not part of XMouse. Download them now?".to_string(),
                                State::Failed(why) if !have => {
                                    format!("{about} The download didn't work: {why}.")
                                }
                                _ if !have => format!("{about} {needs}"),
                                _ => about.to_string(),
                            };
                            control_row(ui, false, name, &sub, |ui| {
                                if unsupported {
                                    *on = false;
                                } else if matches!(state, State::Working(_)) {
                                    ui.spinner();
                                } else if have {
                                    changed |= switch(ui, on).changed();
                                } else if confirming {
                                    if secondary(ui, "Cancel", 84.0).clicked() {
                                        *fg_confirm_dlss = false;
                                    }
                                    if primary(ui, "Download", 104.0).clicked() {
                                        *fg_confirm_dlss = false;
                                        fg_install = Some(kind);
                                    }
                                } else if secondary(ui, "Install", 96.0).clicked() {
                                    if kind == Kind::Dlss {
                                        *fg_confirm_dlss = true;
                                    } else {
                                        fg_install = Some(kind);
                                    }
                                }
                            });
                        }
                        // One at a time: the one switched on last stays on.
                        if *pc_fsr_fg && !before.0 {
                            *pc_dlss_fg = false;
                        }
                        if *pc_dlss_fg && !before.1 {
                            *pc_fsr_fg = false;
                        }
                    }
                    control_row(
                        ui,
                        false,
                        "Sharp upscaling",
                        "Enlarges the picture with crisp edges on any graphics card. Used when RTX Video upscaling is off.",
                        |ui| {
                            changed |= switch(ui, pc_sharp_upscale).changed();
                        },
                    );
                });
                hint(
                    ui,
                    "Clean up compression and Smooth motion use XMouse's own upscaling in place of RTX Video. Sharpen and Colour work with both.",
                );
                #[cfg(windows)]
                hint(
                    ui,
                    "Frame generation shows the game at the stream's own resolution and takes the place of Smooth motion. FSR is AMD FidelityFX technology by Advanced Micro Devices, Inc.; DLSS is technology by NVIDIA Corporation. Their files are downloaded from the makers' own pages under the makers' licences.",
                );
            } else {
                section(ui, "PC Play");
                group(ui, |ui| {
                    control_row(
                        ui,
                        true,
                        "Bitrate",
                        "Wired: 20, otherwise 10 Mbit/s.",
                        |ui| {
                            let opts = [
                                (0u32, "Automatic"),
                                (5000, "5 Mbit/s"),
                                (10000, "10 Mbit/s"),
                                (15000, "15 Mbit/s"),
                                (20000, "20 Mbit/s"),
                            ];
                            let label = opts
                                .iter()
                                .find(|o| o.0 == *pc_bitrate)
                                .map(|o| o.1.to_string())
                                .unwrap_or_else(|| format!("{pc_bitrate} kbps"));
                            egui::ComboBox::from_id_salt("pcbr")
                                .width(150.0)
                                .selected_text(label)
                                .show_ui(ui, |ui| {
                                    for (v, l) in opts {
                                        changed |= ui.selectable_value(pc_bitrate, v, l).changed();
                                    }
                                });
                        },
                    );
                    control_row(
                        ui,
                        false,
                        "Video quality",
                        "Compatible if the stream stutters.",
                        |ui| {
                            egui::ComboBox::from_id_salt("pcq")
                                .width(150.0)
                                .selected_text(if *pc_high_quality {
                                    "High"
                                } else {
                                    "Compatible"
                                })
                                .show_ui(ui, |ui| {
                                    changed |= ui
                                        .selectable_value(pc_high_quality, true, "High")
                                        .changed();
                                    changed |= ui
                                        .selectable_value(pc_high_quality, false, "Compatible")
                                        .changed();
                                });
                        },
                    );
                    control_row(
                        ui,
                        false,
                        "RTX Video upscaling",
                        "NVIDIA Super Resolution on the GPU. Needs RTX Video Enhancement → Super Resolution on in the NVIDIA app.",
                        |ui| {
                            egui::ComboBox::from_id_salt("pcvsr")
                                .width(150.0)
                                .selected_text(if *pc_rtx_vsr { "On" } else { "Off" })
                                .show_ui(ui, |ui| {
                                    changed |= ui.selectable_value(pc_rtx_vsr, true, "On").changed();
                                    changed |=
                                        ui.selectable_value(pc_rtx_vsr, false, "Off").changed();
                                });
                        },
                    );
                    control_row(
                        ui,
                        false,
                        "GPU video decoding",
                        "Decode the stream on the graphics card: less input lag and less CPU. Best on NVIDIA RTX with RTX Video upscaling on. Falls back to the CPU if it can't start.",
                        |ui| {
                            egui::ComboBox::from_id_salt("pcgpudec")
                                .width(150.0)
                                .selected_text(if *pc_gpu_decode { "On" } else { "Off" })
                                .show_ui(ui, |ui| {
                                    changed |=
                                        ui.selectable_value(pc_gpu_decode, true, "On").changed();
                                    changed |=
                                        ui.selectable_value(pc_gpu_decode, false, "Off").changed();
                                });
                        },
                    );
                    control_row(
                        ui,
                        false,
                        "Scaling",
                        "How the picture fills the window (when RTX Video is off).",
                        |ui| {
                            egui::ComboBox::from_id_salt("pcs")
                                .width(150.0)
                                .selected_text(if *pc_smooth_scaling {
                                    "Smooth"
                                } else {
                                    "Sharp pixels"
                                })
                                .show_ui(ui, |ui| {
                                    changed |= ui
                                        .selectable_value(pc_smooth_scaling, true, "Smooth")
                                        .changed();
                                    changed |= ui
                                        .selectable_value(pc_smooth_scaling, false, "Sharp pixels")
                                        .changed();
                                });
                        },
                    );
                });
                section(ui, "Hotkeys");
                group(ui, |ui| {
                    for (i, (k, t)) in [
                        ("F8", "Release mouse and keyboard"),
                        ("F6", "Deadzone finder"),
                        ("F7", "Overlay"),
                        ("F11", "Fullscreen"),
                    ]
                    .into_iter()
                    .enumerate()
                    {
                        row(ui, i == 0, t, "", RowOptions::default(), |ui| {
                            keycap(ui, k);
                        });
                    }
                });
            }
        });
        #[cfg(windows)]
        if let Some(kind) = fg_install {
            let ctx = self.ctx.clone();
            crate::framegen::install::start(kind, self.storage.dir.clone(), &self.rt, move || {
                ctx.request_repaint()
            });
        }
        #[cfg(not(windows))]
        let _ = (fg_confirm_dlss, pc_fsr_fg, pc_dlss_fg);
        if changed {
            // Turning RTX Video upscaling (back) on gives it a fresh try next session.
            if self.settings.pc_rtx_vsr {
                self.vsr_failed = false;
            }
            self.save_settings();
        }
    }

    fn setup_page(&mut self, ui: &mut Ui) {
        title(ui, "Xbox setup", "Once, on your console.");
        narrow(ui, 640.0, |ui| {
            section(ui, "On your Xbox");
            setup_group(ui);
        });
    }

    // ------------------------------------------------------------------ session

    fn session_ui(&mut self, ui: &mut Ui, ctx: &egui::Context, frame: &mut eframe::Frame) {
        let mut end = false;
        let mut save_profile: Option<Profile> = None;
        let mut start_capture = false;
        let input_hz = self.settings.input_hz;
        let smooth = self.settings.pc_smooth_scaling;
        let vsr_wanted = self.settings.pc_rtx_vsr;
        let look = picture_look(&self.settings);
        let smooth_motion = self.settings.pc_smooth_motion;
        let s = self.session.as_mut().unwrap();
        let status = s.shared.status();
        let connected = status.phase == Phase::Connected;
        if status.phase == Phase::Ended {
            end = true;
        }

        // PC Play: the picture, mouse capture and hotkeys live in their own window
        // ("XMouse – Xbox"); this window stays behind with the status and Disconnect.
        let pc_window = s.mode == PlayMode::Pc;

        // Input (TV Play: this window; PC Play: read inside the player window below)
        if !pc_window {
            save_profile = session_input(s, ctx);
        }

        // Video (PC Play). Three ways to the screen (see `Route`): as it always was (RTX Video
        // upscales NV12 frames on the GPU, or egui scales RGBA frames), RTX Video followed by
        // our own sharpening / colour passes, or everything in our own passes.
        let pc = s.mode == PlayMode::Pc;
        // Frame generation by FSR or DLSS takes the place of our own in-between frames.
        #[cfg(windows)]
        let fg_wanted = fg_choice(&self.settings, &self.storage.dir).filter(|_| pc && !self.fg_failed);
        #[cfg(not(windows))]
        let fg_wanted: Option<()> = None;
        let smooth_motion = smooth_motion && fg_wanted.is_none();
        let mut fg_error: Option<String> = None;
        let gl = frame.gl().cloned();
        let post = look.sharpen.is_some() || look.colour.is_some();
        // Clean-up and in-between frames work on the stream's own pixels, before any scaling:
        // with those on, the picture goes through our passes and RTX Video is left out.
        let before_scaling = look.cleanup.is_some() || smooth_motion;
        let mut pipe_error: Option<String> = None;
        let mut motion_error = false;
        if pc
            && self.pipe.is_none()
            && !self.pipe_failed
            && (before_scaling || post || look.edge_upscale || fg_wanted.is_some())
        {
            if let Some(gl) = gl.as_ref() {
                match Pipeline::new(gl) {
                    Ok(p) => self.pipe = Some(p),
                    Err(e) => {
                        self.pipe_failed = true;
                        pipe_error = Some(format!("{e:#}"));
                    }
                }
            }
        }
        let pipe_ok = pc && self.pipe.is_some() && gl.is_some();
        #[cfg(windows)]
        let gpu = {
            let wanted = vsr_wanted && pc && !self.vsr_failed && !(before_scaling && pipe_ok);
            if wanted && self.vsr.is_none() {
                match crate::vsr::Vsr::new() {
                    Ok(v) => self.vsr = Some(v),
                    Err(e) => {
                        log::warn!("RTX Video upscaling unavailable: {e:#}");
                        self.vsr_failed = true;
                    }
                }
            }
            wanted && self.vsr.is_some() && frame.gl().is_some()
        };
        #[cfg(not(windows))]
        let gpu = false;
        let route = if pipe_ok && before_scaling {
            Route::Pipeline
        } else if gpu {
            if pipe_ok && post {
                Route::VsrPost
            } else {
                Route::Plain
            }
        } else if pipe_ok && (post || look.edge_upscale) {
            Route::Pipeline
        } else {
            Route::Plain
        };
        // The frame-generation surface takes the decoder's NV12 pictures; without RTX Video
        // our own passes then draw the picture underneath it.
        let route = if fg_wanted.is_some() && !gpu && pipe_ok {
            Route::Pipeline
        } else {
            route
        };
        let fg_kind = fg_wanted.filter(|_| gpu || route == Route::Pipeline);
        s.shared.want_nv12.store(
            gpu || route == Route::Pipeline,
            std::sync::atomic::Ordering::Relaxed,
        );
        if s.pic.mode != Some((route, smooth_motion)) {
            // A switch was flipped (or the session just started): in-between frames start
            // over. The last picture stays on screen until the new route delivers one.
            s.pic.mode = Some((route, smooth_motion));
            s.pic.pacer.reset();
            s.pic.showing_mid = false;
            s.pic.vsr_raw = None;
            if let Some(p) = self.pipe.as_mut() {
                p.reset_history();
            }
            log::info!("picture: {route:?} route, smooth motion {smooth_motion}");
        }
        let frames = s
            .shared
            .video_frames
            .load(std::sync::atomic::Ordering::Relaxed);

        // Every frame counts for the timing (is it a repeat? how fast does the game really
        // run?); only the newest is shown.
        let now_i = Instant::now();
        let epoch = *s.pic.epoch.get_or_insert(now_i);
        let secs = |t: Instant| t.saturating_duration_since(epoch).as_secs_f64();
        let now = secs(now_i);
        let mut batch = if pc {
            s.shared.video.drain()
        } else {
            Vec::new()
        };
        let mut new_pictures: Vec<(f64, Kind)> = Vec::new();
        for f in &batch {
            let sig = match &f.frame {
                VideoFrame::Rgba(img) => {
                    Signature::from_rgba(&img.pixels, img.size[0], img.size[1])
                }
                VideoFrame::Nv12 {
                    width,
                    height,
                    data,
                } => Signature::from_luma(data, *width, *height),
            };
            let kind = s.pic.detector.classify(sig);
            let at = secs(f.at);
            s.pic.cadence.on_frame(at, kind != Kind::Duplicate);
            s.pic.stats.on_frame(at, kind);
            // The frame generators get every new picture, and never a repeat.
            #[cfg(windows)]
            if let (Some(surface), VideoFrame::Nv12 { width, height, data }, true) =
                (self.fg.as_ref(), &f.frame, kind != Kind::Duplicate)
            {
                surface.push(crate::framegen::Frame {
                    width: *width,
                    height: *height,
                    nv12: data.clone(),
                    at: f.at,
                    cut: matches!(kind, Kind::First | Kind::Cut),
                    interval: s.pic.cadence.content_interval().unwrap_or(1.0 / 60.0),
                });
            }            if kind != Kind::Duplicate {
                new_pictures.push((at, kind));
            }
        }
        let newest = batch.pop();
        if let Some(f) = &newest {
            s.video_size = match &f.frame {
                VideoFrame::Rgba(img) => [img.size[0] as u32, img.size[1] as u32],
                VideoFrame::Nv12 { width, height, .. } => [*width as u32, *height as u32],
            };
        }

        // The frame-generation surface: started once the stream's size is known, shown over
        // the picture while you play (mouse captured), gone again on any failure.
        #[cfg(windows)]
        {
            let size = s.video_size;
            if self
                .fg
                .as_ref()
                .is_some_and(|f| Some(f.kind) != fg_kind || (size[0] > 0 && f.size != size))
            {
                self.fg = None;
            }
            let usable = size[0] >= 64 && size[1] >= 64 && size[0] % 2 == 0 && size[1] % 2 == 0;
            if let (Some(kind), true, true) = (fg_kind, self.fg.is_none(), usable) {
                let started = crate::framegen::dll_dir(kind, &self.storage.dir)
                    .zip(crate::framegen::find_own_window("XMouse \u{2013} "))
                    .map(|(dir, hwnd)| {
                        crate::framegen::Surface::start(kind, dir, hwnd, size[0], size[1])
                    });
                match started {
                    Some(Ok(f)) => {
                        // Crash guard: on disk until a few hundred frames went through.
                        let _ = std::fs::write(
                            self.storage.framegen_guard_file(),
                            b"frame generation",
                        );
                        self.fg_guard_armed = true;
                        self.fg_seen = [0; 2];
                        self.fg = Some(f);
                    }
                    Some(Err(e)) => fg_error = Some(format!("{e:#}")),
                    None => {}
                }
            }
            if let Some(f) = self.fg.as_mut() {
                if let Some(e) = f.error() {
                    fg_error = Some(e);
                } else {
                    // XMOUSE_FG_ALWAYS shows it without a captured mouse, for testing.
                    let playing = s.captured || std::env::var_os("XMOUSE_FG_ALWAYS").is_some();
                    let show = playing && connected && f.ready() && s.fg_rect[2] > 16;
                    f.place(s.fg_rect, show);
                    let real = f.shared.real.load(std::sync::atomic::Ordering::Relaxed);
                    if self.fg_guard_armed && real > 300 {
                        let _ = std::fs::remove_file(self.storage.framegen_guard_file());
                        self.fg_guard_armed = false;
                    }
                    // New pictures are handed over from this loop: look often.
                    ctx.request_repaint_after(Duration::from_millis(4));
                }
            }
            if fg_error.is_some() {
                self.fg = None;
                self.fg_failed = true;
                if self.fg_guard_armed {
                    let _ = std::fs::remove_file(self.storage.framegen_guard_file());
                    self.fg_guard_armed = false;
                }
            }
        }
        #[cfg(not(windows))]
        let _ = fg_kind;
        if route == Route::Pipeline {
            if let (Some(p), Some(gl)) = (self.pipe.as_mut(), gl.as_ref()) {
                let hz = match s.pic.display {
                    Some((hz, at)) if at.elapsed() < Duration::from_secs(2) => hz,
                    _ => {
                        let hz = display_hz(s.pic.player_center);
                        s.pic.display = Some((hz, now_i));
                        hz
                    }
                };
                let target = if s.vsr_target[0] > 0 {
                    s.vsr_target
                } else {
                    s.video_size
                };
                let pic = &mut s.pic;
                let shown = (|| -> anyhow::Result<Option<(egui::TextureId, egui::Vec2)>> {
                    if let Some(f) = &newest {
                        if !smooth_motion {
                            p.submit(gl, &f.frame, look.cleanup, false, false)?;
                        } else if let Some(&(at, kind)) = new_pictures.last() {
                            p.submit(gl, &f.frame, look.cleanup, true, true)?;
                            let mut show = pic.pacer.on_unique(Arrival {
                                at,
                                now,
                                interval: pic.cadence.content_interval(),
                                display_hz: hz,
                                cut: kind == Kind::Cut,
                                // (Two new pictures at once: the UI was late and the one
                                // in between was never uploaded.)
                                have_prev: new_pictures.len() == 1
                                    && kind != Kind::First
                                    && p.has_prev(),
                            });
                            if matches!(show, Show::Mid { .. }) {
                                if let Err(e) = p.interpolate(gl) {
                                    log::warn!("Smooth motion can't run on this graphics driver: {e:#}");
                                    motion_error = true;
                                    pic.pacer.reset();
                                    show = Show::Real(Skip::NoHistory);
                                }
                            }
                            pic.stats.on_show(show);
                            pic.showing_mid = matches!(show, Show::Mid { .. });
                        } else {
                            // A repeat. Its pixels are still taken (a "repeat" may hide a
                            // small change), and it is the stream's own tick for a real
                            // frame that waits behind an in-between frame.
                            p.submit(gl, &f.frame, look.cleanup, false, true)?;
                            let due = pic.pacer.pending();
                            if pic.pacer.on_duplicate(secs(f.at)) {
                                pic.showing_mid = false;
                                pic.late = pic.late.max(now - due.unwrap_or(now));
                            }
                        }
                    }
                    let due = pic.pacer.pending();
                    if pic.pacer.poll(now) {
                        pic.showing_mid = false;
                        pic.late = pic.late.max(now - due.unwrap_or(now));
                    }
                    if !p.has_picture() {
                        return Ok(None);
                    }
                    let src = if pic.showing_mid {
                        Source::Mid
                    } else {
                        Source::Current
                    };
                    Ok(Some(p.present(gl, frame, src, target, look)?))
                })();
                match shown {
                    Ok(Some(t)) => {
                        s.vsr_tex = Some(t);
                        dump_picture(p, gl, &mut s.pic, frames);
                    }
                    Ok(None) => {}
                    Err(e) => pipe_error = Some(format!("{e:#}")),
                }
                // No repeat may come to show the waiting real frame (60 fps game): wake up
                // for it.
                if let Some(due) = s.pic.pacer.pending() {
                    ctx.request_repaint_after(Duration::from_secs_f64((due - now).max(0.0)));
                }
            }
        } else {
            match newest.map(|f| f.frame) {
                Some(VideoFrame::Rgba(img)) => {
                    if !gpu {
                        s.vsr_tex = None;
                    }
                    let opts = if smooth {
                        TextureOptions::LINEAR
                    } else {
                        TextureOptions::NEAREST
                    };
                    match &mut s.texture {
                        Some(t) => t.set(img, opts),
                        None => s.texture = Some(ctx.load_texture("video", img, opts)),
                    }
                }
                Some(VideoFrame::Nv12 {
                    width,
                    height,
                    data,
                }) => {
                    #[cfg(windows)]
                    {
                        let mut failed = None;
                        if let (Some(v), Some(gl)) = (self.vsr.as_mut(), gl.as_ref()) {
                            let [tw, th] = if s.vsr_target[0] > 0 {
                                s.vsr_target
                            } else {
                                s.video_size
                            };
                            // Crash guard: a new size means a new processor setup, the moment the
                            // driver crashed before. Mark it on disk until it has run a while.
                            let guard = self.storage.vsr_guard_file();
                            if [tw, th] != self.vsr_guard_target {
                                self.vsr_guard_target = [tw, th];
                                self.vsr_watch.1 = 0;
                                if !self.vsr_guard_armed {
                                    let _ = std::fs::write(&guard, b"upscaling");
                                    self.vsr_guard_armed = true;
                                }
                            }
                            match v.process(gl, frame, width as u32, height as u32, &data, tw, th)
                            {
                                Ok(t) => {
                                    if route == Route::VsrPost {
                                        // Sharpening / colour come after it, below.
                                        s.pic.vsr_raw = Some((t.1, [tw, th]));
                                        s.pic.vsr_version += 1;
                                    } else {
                                        s.vsr_tex = Some(t);
                                    }
                                    if v.generation != self.vsr_watch.0 {
                                        self.vsr_watch = (v.generation, 0);
                                    }
                                    self.vsr_watch.1 += 1;
                                    if self.vsr_guard_armed && self.vsr_watch.1 >= 120 {
                                        let _ = std::fs::remove_file(&guard);
                                        self.vsr_guard_armed = false;
                                    }
                                }
                                Err(e) => failed = Some(e),
                            }
                        }
                        if let Some(e) = failed {
                            log::warn!("RTX Video upscaling failed, using normal scaling: {e:#}");
                            self.vsr = None;
                            self.vsr_failed = true;
                            s.vsr_tex = None;
                            s.shared
                                .want_nv12
                                .store(false, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    #[cfg(not(windows))]
                    let _ = (width, height, data);
                }
                None => {}
            }
        }
        // RTX Video's picture, sharpened / graded by our passes.
        #[cfg(windows)]
        if route == Route::VsrPost {
            if let (Some(p), Some(gl), Some((uv, size))) =
                (self.pipe.as_mut(), gl.as_ref(), s.pic.vsr_raw)
            {
                if let Some(texture) = self.vsr.as_ref().and_then(|v| v.gl_texture()) {
                    let src = Source::External {
                        texture,
                        uv: [uv.x, uv.y],
                        version: s.pic.vsr_version,
                    };
                    match p.present(gl, frame, src, size, look) {
                        Ok(t) => {
                            s.vsr_tex = Some(t);
                            dump_picture(p, gl, &mut s.pic, frames);
                        }
                        Err(e) => pipe_error = Some(format!("{e:#}")),
                    }
                }
            }
        }
        if let Some(e) = &pipe_error {
            log::warn!("picture processing failed, showing the plain picture: {e}");
            self.pipe_failed = true;
            if let (Some(p), Some(gl)) = (self.pipe.take(), gl.as_ref()) {
                p.destroy(gl);
            }
            s.vsr_tex = None;
            s.pic.vsr_raw = None;
        }
        // What the stream really carries, every five seconds: a 60 fps stream of a 30 fps
        // game shows up here as half repeats.
        if let Some((stream, unique)) = s.pic.stats.report(now, 5.0) {
            let repeats = if stream > 0.0 {
                (1.0 - unique / stream).max(0.0) * 100.0
            } else {
                0.0
            };
            let mut line = format!(
                "picture: stream {stream:.1} fps, {unique:.1} new pictures a second ({repeats:.0}% repeats): the game runs at about {} fps",
                nominal_fps(unique)
            );
            if smooth_motion && route == Route::Pipeline {
                line += &format!(
                    " | smooth motion: {} in-between frames",
                    s.pic.stats.generated
                );
                if let Some((why, n)) = s.pic.stats.main_skip() {
                    line += &format!(", {n} pictures shown without one ({why:?})");
                }
                if s.pic.stats.generated > 0 {
                    line += &format!(
                        ", real frames at most {:.1} ms behind schedule",
                        s.pic.late * 1000.0
                    );
                }
                if let Some((hz, _)) = s.pic.display {
                    line += &format!(", display {hz:.0} Hz");
                }
                s.pic.late = 0.0;
            }
            #[cfg(windows)]
            if let Some(f) = &self.fg {
                let o = std::sync::atomic::Ordering::Relaxed;
                let now_seen = [f.shared.real.load(o), f.shared.shown.load(o)];
                line += &format!(
                    " | {} frame generation: {} pictures in, {} frames out since the last line, longest gap between pictures {:.1} ms",
                    f.kind.name(),
                    now_seen[0].saturating_sub(self.fg_seen[0]),
                    now_seen[1].saturating_sub(self.fg_seen[1]),
                    f.shared.worst_gap.swap(0, o) as f64 / 10.0
                );
                self.fg_seen = now_seen;
            }
            if route != Route::Plain {
                if let Some(p) = &self.pipe {
                    let [clean, motion, output] = p.gpu_ms();
                    line += &format!(
                        " | GPU: frame {clean:.2} ms, motion {motion:.2} ms, output {output:.2} ms"
                    );
                }
            }
            log::info!("{line}");
            s.pic.stats.clear_counts();
        }

        let (frame, layer, pname, legend) = {
            let m = s.shared.mapper.lock().unwrap();
            // The key list only changes with the layer (a session can't edit bindings), so
            // it is built once per layer instead of on every frame with the mapper locked.
            if s.legend.0.as_deref() != Some(m.layer_name()) {
                let rows: Vec<(String, String)> = m
                    .layer_bindings()
                    .iter()
                    .filter(|b| b.action != Action::None || b.switch_layer.is_some())
                    .map(|b| (input_label(&b.input), b.title().to_string()))
                    .collect();
                s.legend = (Some(m.layer_name().to_string()), Arc::new(rows));
            }
            (
                m.last_frame,
                m.layer_name().to_string(),
                m.profile().name.clone(),
                s.legend.1.clone(),
            )
        };
        let mode_name = if s.mode == PlayMode::Tv {
            "TV Play"
        } else {
            "PC Play"
        };
        let enter = self.nav.show(
            ctx,
            View::Session(match (connected, s.captured) {
                (false, _) => 0,
                (true, false) => 1,
                (true, true) => 2,
            }),
        );
        {
            let back = if s.captured { None } else { Some("Disconnect") };
            let back_fade = self.nav.back_fade(ctx, back);
            if top_bar(ui, back, Some(format!("{mode_name}  ")), back_fade) {
                end = true;
            }
            let line = if connected {
                format!(
                    "{mode_name} · {} · {pname} · {layer} · {input_hz} Hz · {} frames sent{}",
                    s.console.name,
                    s.shared
                        .frames_sent
                        .load(std::sync::atomic::Ordering::Relaxed),
                    if s.mode == PlayMode::Pc {
                        format!(" · {frames} video frames")
                    } else {
                        String::new()
                    }
                )
            } else {
                format!("{mode_name} · {} · {}", s.console.name, status.detail)
            };
            status_bar(ui, &line);
        }

        // Plugin consoles (other platforms) are called by their own name.
        let plugin_console = plugin_console_ref(&s.console.server_id).is_some();
        let console_word: &str = if !plugin_console {
            "Xbox"
        } else if crate::profile::playstation_names() {
            "PlayStation"
        } else {
            &s.console.name
        };
        // The name of PC Play's own window, as its title bar shows it.
        let window_word: &str = if plugin_console { &s.console.name } else { "Xbox" };
        egui::CentralPanel::default()
            .frame(page_frame())
            .show(ui, |ui| entering(ui, enter, |ui| {
                if !connected {
                    title(ui, "Connecting", &s.console.name);
                    narrow(ui, 600.0, |ui| {
                        group(ui, |ui| {
                            let steps = [
                                (
                                    Phase::Starting,
                                    if plugin_console {
                                        "Ask the console to start a session"
                                    } else {
                                        "Ask Xbox to start a session"
                                    },
                                ),
                                (Phase::WaitingForConsole, "Wait for your console"),
                                (Phase::Connecting, "Connect over your home network"),
                            ];
                            let cur = steps
                                .iter()
                                .position(|(p, _)| *p == status.phase)
                                .unwrap_or(0);
                            for (i, (_, t)) in steps.iter().enumerate() {
                                let sub = if i < cur {
                                    "Done"
                                } else if i == cur {
                                    status.detail.as_str()
                                } else {
                                    ""
                                };
                                row(
                                    ui,
                                    i == 0,
                                    t,
                                    sub,
                                    RowOptions {
                                        lead: Lead::Key((i + 1).to_string(), i == cur),
                                        ..Default::default()
                                    },
                                    |ui| {
                                        if i == cur {
                                            spinner(ui);
                                        }
                                    },
                                );
                            }
                        });
                    });
                    return;
                }
                ui.add_space((ui.available_height() * 0.16).max(20.0));
                ui.vertical_centered(|ui| {
                    ui.spacing_mut().item_spacing.y = 12.0;
                    if s.captured {
                        ui.label(
                            RichText::new(format!("You control the {console_word}"))
                                .font(display(38.0))
                                .color(INK),
                        );
                        ui.label(
                            RichText::new(if s.mode == PlayMode::Tv {
                                "The picture is on the TV. F8 pauses.".to_string()
                            } else {
                                format!(
                                    "The picture is in the XMouse – {window_word} window. F8 pauses."
                                )
                            })
                            .font(body(18.0))
                            .color(MUTED),
                        );
                        ui.add_space(4.0);
                        keycap_sized(ui, "F8  Pause", bold(17.0), 46.0, true, false);
                        ui.label(
                            RichText::new(format!("{pname} · {layer}"))
                                .font(mono(13.0))
                                .color(LABEL),
                        );
                        if s.show_hud {
                            ui.add_space(8.0);
                            pad_view(ui, &frame, 320.0);
                        }
                    } else {
                        ui.label(
                            RichText::new(if s.paused { "Paused" } else { "Ready" })
                                .font(display(38.0))
                                .color(INK),
                        );
                        ui.label(
                            RichText::new(if s.mode == PlayMode::Tv {
                                "Press Play, then look at your TV. F8 pauses at any time."
                                    .to_string()
                            } else {
                                format!(
                                    "Press Play. The picture is in the XMouse – {window_word} window."
                                )
                            })
                            .font(body(18.0))
                            .color(MUTED),
                        );
                        ui.add_space(6.0);
                        if play_button(ui, if s.paused { "Resume" } else { "Play" }, true).clicked()
                        {
                            start_capture = true;
                        }
                        if small_link(ui, "Disconnect").clicked() {
                            end = true;
                        }
                        ui.label(
                            RichText::new(format!("{pname} · {layer}"))
                                .font(mono(13.0))
                                .color(LABEL),
                        );
                    }
                });
            }));

        if !pc_window {
            if s.captured {
                live_frame(ctx);
            }
            finder_overlay(ctx, s);
        } else if !end {
            let r = player_window(ctx, s, connected, &frame, &pname, &layer, &legend);
            end |= r.end;
            start_capture |= r.capture;
            if r.save.is_some() {
                save_profile = r.save;
            }
        }

        if start_capture {
            s.captured = true;
            if pc_window {
                // Bring the picture window forward and capture the mouse there.
                ctx.send_viewport_cmd_to(player_id(), ViewportCommand::Focus);
                grab_capture_of(ctx, player_id());
            } else {
                s.mods = ctx.input(|i| i.modifiers);
                grab_capture(ctx);
                // Keep the window in front while you look at the TV.
                ctx.send_viewport_cmd(ViewportCommand::Fullscreen(true));
            }
        }
        ctx.request_repaint_after(Duration::from_millis(33));

        if let Some(p) = save_profile {
            if let Some(i) = self.profiles.iter().position(|x| x.id == p.id) {
                self.profiles[i] = p.clone();
            }
            match self.storage.save_profile(&p) {
                Ok(()) => self.toast(
                    format!("Deadzone {:.2} saved to “{}”", p.mouse.deadzone, p.name),
                    false,
                ),
                Err(e) => self.toast(format!("Could not save: {e}"), true),
            }
        }
        if let Some(e) = fg_error {
            log::warn!("frame generation off for this session: {e}");
            self.toast(
                format!("Frame generation can't run: {e}. Showing the normal picture."),
                true,
            );
        }
        if pipe_error.is_some() {
            self.toast(
                "The picture settings can't run on this graphics card. Showing the plain picture.",
                true,
            );
        } else if motion_error {
            self.toast("Smooth motion can't run on this graphics card.", true);
        }
        if end {
            self.end_session(ctx);
        }
    }
}

/// Test runs: with XMOUSE_PICTURE_DUMP set to a folder, a few of the pictures the pipeline
/// puts on screen are written there (in-between frames are named so), to look at them
/// without a console or a person at the screen.
fn dump_picture(p: &Pipeline, gl: &eframe::glow::Context, pic: &mut PictureState, frames: u64) {
    static DIR: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    let Some(dir) = DIR.get_or_init(|| std::env::var_os("XMOUSE_PICTURE_DUMP").map(Into::into))
    else {
        return;
    };
    // (Well into the stream, so the frame timing has settled.)
    // (An odd key for an in-between frame, so a real frame that follows it is told apart
    // even when no new frame came in between.)
    let key = (p.version() + pic.vsr_version) * 2 + pic.showing_mid as u64;
    if frames < 240 || pic.dumped.0 >= 6 || pic.dumped.1 == key {
        return;
    }
    // Real and in-between frames in turn (a repeat of the picture on screen is skipped).
    if pic.dumped.0 > 0 && (pic.dumped.1 % 2 == 1) == pic.showing_mid {
        return;
    }
    if let Some((w, h, px)) = p.read_output(gl) {
        let kind = if pic.showing_mid { "mid" } else { "real" };
        write_bmp(&dir.join(format!("{}-{kind}.bmp", pic.dumped.0)), w, h, &px);
        pic.dumped = (pic.dumped.0 + 1, key);
    }
}

/// Test runs: XMOUSE_PICTURE_SIZE=2560x1440 makes the GPU scale to that size whatever the
/// window's size is (to look at upscaling without a large window).
fn forced_picture_size() -> Option<[u32; 2]> {
    static SIZE: std::sync::OnceLock<Option<[u32; 2]>> = std::sync::OnceLock::new();
    *SIZE.get_or_init(|| {
        let v = std::env::var("XMOUSE_PICTURE_SIZE").ok()?;
        let (w, h) = v.split_once('x')?;
        Some([w.parse().ok()?, h.parse().ok()?])
    })
}

/// Which frame generator a session should use: one that is switched on and whose files are
/// installed, DLSS before FSR.
#[cfg(windows)]
fn fg_choice(s: &Settings, config_dir: &std::path::Path) -> Option<crate::framegen::Kind> {
    use crate::framegen::{dll_dir, Kind};
    if s.pc_dlss_fg && crate::framegen::nvidia_present() && dll_dir(Kind::Dlss, config_dir).is_some() {
        Some(Kind::Dlss)
    } else if s.pc_fsr_fg && dll_dir(Kind::Fsr, config_dir).is_some() {
        Some(Kind::Fsr)
    } else {
        None
    }
}

fn player_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("xmouse-player")
}

#[derive(Default)]
struct PlayerResult {
    end: bool,
    capture: bool,
    save: Option<Profile>,
}

/// PC Play's own window: the picture, mouse capture, hotkeys and the HUD.
/// Closing it ends the session.
fn player_window(
    ctx: &egui::Context,
    s: &mut Session,
    connected: bool,
    frame: &PadFrame,
    pname: &str,
    layer: &str,
    legend: &[(String, String)],
) -> PlayerResult {
    let mut out = PlayerResult::default();
    let builder = egui::ViewportBuilder::default()
        // Plugin consoles (other platforms) show their own name instead of "Xbox".
        .with_title(if plugin_console_ref(&s.console.server_id).is_some() {
            format!("XMouse – {}", s.console.name)
        } else {
            "XMouse – Xbox".to_string()
        })
        .with_inner_size([1280.0, 720.0])
        .with_min_inner_size([480.0, 270.0]);
    ctx.show_viewport_immediate(player_id(), builder, |ui, _class| {
        let pctx = ui.ctx().clone();
        if pctx.input(|i| i.viewport().close_requested()) {
            out.end = true;
        }
        out.save = session_input(s, &pctx);
        // The picture's on-screen rectangle; overlays sit inside it, not over the black bars.
        let mut picture = pctx.content_rect();
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(Color32::BLACK))
            .show(ui, |ui| {
                let rect = ui.max_rect();
                let click = ui.interact(rect, ui.scope_id().with("stage"), egui::Sense::click());
                // Where the picture goes (aspect-fit), and its size in real pixels for the
                // GPU upscaler, which renders at exactly that size.
                let [vw, vh] = s.video_size;
                let aspect = if vw > 0 && vh > 0 {
                    vw as f32 / vh as f32
                } else {
                    16.0 / 9.0
                };
                let fit = if rect.width() / rect.height() > aspect {
                    vec2(rect.height() * aspect, rect.height())
                } else {
                    vec2(rect.width(), rect.width() / aspect)
                };
                let pic = egui::Rect::from_center_size(rect.center(), fit);
                picture = pic;
                let ppp = ui.ctx().pixels_per_point();
                s.fg_rect = [
                    (pic.min.x * ppp).round() as i32,
                    (pic.min.y * ppp).round() as i32,
                    (pic.width() * ppp).round() as i32,
                    (pic.height() * ppp).round() as i32,
                ];
                s.vsr_target = forced_picture_size().unwrap_or([
                    (fit.x * ppp).round().max(16.0) as u32,
                    (fit.y * ppp).round().max(16.0) as u32,
                ]);
                // Where this window is on the desktop, to know its display's refresh rate.
                s.pic.player_center = ui.ctx().input(|i| {
                    let v = i.viewport();
                    let scale = v.native_pixels_per_point.unwrap_or(1.0);
                    v.outer_rect.map(|r| {
                        let c = r.center() * scale;
                        [c.x as i32, c.y as i32]
                    })
                });
                if let Some((id, uv)) = s.vsr_tex {
                    ui.painter().image(
                        id,
                        pic,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), uv.to_pos2()),
                        Color32::WHITE,
                    );
                } else if let Some(tex) = &s.texture {
                    let size = tex.size_vec2();
                    let scale = (rect.width() / size.x).min(rect.height() / size.y);
                    let r = egui::Rect::from_center_size(rect.center(), size * scale);
                    ui.painter().image(
                        tex.id(),
                        r,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        Color32::WHITE,
                    );
                } else {
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        if connected {
                            "Waiting for the picture…"
                        } else if plugin_console_ref(&s.console.server_id).is_some() {
                            "Connecting to your console…"
                        } else {
                            "Connecting to your Xbox…"
                        },
                        body(18.0),
                        Color32::from_gray(170),
                    );
                }
                if connected && !s.captured && click.clicked() {
                    out.capture = true;
                }
            });
        if s.captured {
            live_frame(&pctx);
        }
        session_overlays(&pctx, s, connected, frame, pname, layer, picture);
        if s.show_hud && connected {
            controls_legend(&pctx, layer, legend, picture);
        }
        game_tip(&pctx, s, connected, picture);
        finder_overlay(&pctx, s);
        pctx.request_repaint_after(Duration::from_millis(16));
    });
    out
}

/// Read this window's input into the mapper and act on the hotkeys (F8, F7, F11, F6).
/// Returns a profile to save when the deadzone finder stored a value.
fn session_input(s: &mut Session, ctx: &egui::Context) -> Option<Profile> {
    let mut save_profile = None;
    let now = s.shared.now_ms();
    let res = {
        let mut m = s.shared.mapper.lock().unwrap();
        feed_input(ctx, &mut m, s.captured, &mut s.mods, now)
    };
    let release = |s: &mut Session| {
        s.captured = false;
        s.paused = true;
        s.shared.mapper.lock().unwrap().reset();
        // The deadzone finder pushes the stick on its own: stop it with the rest.
        *s.shared.finder.lock().unwrap() = None;
        release_capture(ctx);
        ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
    };
    if res.lost_focus && s.captured {
        release(s);
    }
    for k in res.hotkeys {
        match k {
            KEY_RELEASE if s.captured => release(s),
            KEY_HUD => s.show_hud = !s.show_hud,
            KEY_FULLSCREEN => {
                let fs = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
                ctx.send_viewport_cmd(ViewportCommand::Fullscreen(!fs));
            }
            KEY_FINDER => {
                let mut f = s.shared.finder.lock().unwrap();
                let mut m = s.shared.mapper.lock().unwrap();
                if let Some(v) = f.take() {
                    let mut p = m.profile().clone();
                    p.mouse.deadzone = (v * 100.0).round() / 100.0;
                    m.set_profile(p.clone());
                    save_profile = Some(p);
                } else if s.captured {
                    // Only while you're in control: it moves the camera on the console.
                    *f = Some((m.profile().mouse.deadzone - 0.08).max(0.05));
                }
            }
            _ => {}
        }
    }
    if s.shared.finder.lock().unwrap().is_some() {
        let step = ctx.input(|i| {
            i.events
                .iter()
                .map(|e| match e {
                    egui::Event::Key {
                        key: egui::Key::CloseBracket,
                        pressed: true,
                        ..
                    } => 0.01,
                    egui::Event::Key {
                        key: egui::Key::OpenBracket,
                        pressed: true,
                        ..
                    } => -0.01,
                    _ => 0.0,
                })
                .sum::<f32>()
        });
        if step != 0.0 {
            if let Some(v) = s.shared.finder.lock().unwrap().as_mut() {
                *v = (*v + step).clamp(0.0, 0.6);
            }
        }
    }
    save_profile
}

/// The player window's HUD and "click to capture" card.
fn session_overlays(
    ctx: &egui::Context,
    s: &Session,
    connected: bool,
    frame: &PadFrame,
    pname: &str,
    layer: &str,
    picture: egui::Rect,
) {
    if s.captured && s.show_hud {
        egui::Area::new(egui::Id::unique("hud"))
            .pivot(egui::Align2::LEFT_BOTTOM)
            .fixed_pos(picture.left_bottom() + vec2(16.0, -16.0))
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(KEY)
                    .stroke(Stroke::new(1.0, EDGE))
                    .corner_radius(CornerRadius::same(14))
                    .inner_margin(Margin::same(10))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(format!("{pname} · {layer}"))
                                .font(bold(13.0))
                                .color(INK),
                        );
                        pad_view(ui, &frame, 200.0);
                        ui.label(
                            RichText::new("F8 release · F6 deadzone · F7 hide")
                                .font(mono(11.0))
                                .color(LABEL),
                        );
                    });
            });
    }
    if connected && !s.captured {
        egui::Area::new(egui::Id::unique("capture-card"))
            .anchor(egui::Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(KEY)
                    .stroke(Stroke::new(1.0, EDGE))
                    .corner_radius(CornerRadius::same(16))
                    .inner_margin(Margin::same(20))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new("Click to capture mouse and keyboard")
                                .font(bold(17.0))
                                .color(INK),
                        );
                        hint(ui, "F8 gives you the mouse back.");
                    });
            });
    }
}

/// Faint list of the active layer's keys on the right edge of the picture (F7 hides it).
fn controls_legend(
    ctx: &egui::Context,
    layer: &str,
    rows: &[(String, String)],
    picture: egui::Rect,
) {
    if rows.is_empty() {
        return;
    }
    egui::Area::new(egui::Id::unique("controls-legend"))
        .pivot(egui::Align2::RIGHT_CENTER)
        .fixed_pos(egui::pos2(picture.right() - 12.0, picture.center().y))
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(Color32::from_black_alpha(80))
                .corner_radius(CornerRadius::same(10))
                .inner_margin(Margin::same(10))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(layer)
                            .font(bold(12.0))
                            .color(Color32::from_white_alpha(150)),
                    );
                    ui.add_space(2.0);
                    egui::Grid::new("controls-legend-grid")
                        .num_columns(2)
                        .spacing(vec2(10.0, 1.0))
                        .show(ui, |ui| {
                            for (key, what) in rows {
                                ui.label(
                                    RichText::new(key)
                                        .font(mono(11.0))
                                        .color(Color32::from_white_alpha(140)),
                                );
                                ui.label(
                                    RichText::new(what)
                                        .font(body(11.5))
                                        .color(Color32::from_white_alpha(110)),
                                );
                                ui.end_row();
                            }
                        });
                    ui.add_space(2.0);
                    ui.label(
                        RichText::new("F7 hides")
                            .font(mono(10.0))
                            .color(Color32::from_white_alpha(70)),
                    );
                });
        });
}

/// How long the "game ignoring your keys?" tip stays up after the picture appears.
const TIP_SECS: f32 = 10.0;

/// A short tip at the top of the picture for the first seconds of a session: a game started
/// with another controller ignores XMouse's until you hand it over from the Xbox guide.
fn game_tip(ctx: &egui::Context, s: &mut Session, connected: bool, picture: egui::Rect) {
    if !connected || s.shared.video_frames.load(std::sync::atomic::Ordering::Relaxed) == 0 {
        return;
    }
    if s.tip_since.is_none() {
        // A soft notification chime as the tip appears.
        crate::chime::play_notification();
    }
    let age = s.tip_since.get_or_insert_with(Instant::now).elapsed().as_secs_f32();
    if age > TIP_SECS {
        return;
    }
    // Ease in quickly (settling down into place), ease out over the last 1.5 s.
    let fade_in = motion::ease_out(age / 0.4);
    let fade = fade_in.min(1.0 - motion::ease_out(1.0 - (TIP_SECS - age) / 1.5));
    // Name the keys this profile uses for the console's home button and A / Cross.
    let key_for = |action: Action| -> Option<String> {
        let m = s.shared.mapper.lock().unwrap();
        m.profile()
            .layers
            .first()
            .and_then(|l| l.bindings.iter().find(|b| b.action == action && !b.input.is_empty()))
            .map(|b| input_label(&b.input))
    };
    // "Xbox button" / "A", or "PS button" / "Cross" on a PlayStation.
    let (home_name, a_name) = (Action::Nexus.label(), Action::A.label());
    let xbox = key_for(Action::Nexus)
        .map(|k| format!("{k} ({home_name})"))
        .unwrap_or_else(|| format!("the {home_name}"));
    let a = key_for(Action::A)
        .map(|k| format!("{k} ({a_name})"))
        .unwrap_or_else(|| a_name.into());
    let white = |alpha: f32| Color32::from_white_alpha((alpha * fade * 255.0) as u8);
    egui::Area::new(egui::Id::unique("game-tip"))
        .pivot(egui::Align2::CENTER_TOP)
        .fixed_pos(egui::pos2(
            picture.center().x,
            picture.top() + 24.0 - 8.0 * (1.0 - fade_in),
        ))
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(Color32::from_black_alpha((170.0 * fade) as u8))
                .corner_radius(CornerRadius::same(12))
                .inner_margin(Margin::symmetric(18, 12))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new("Game ignoring your keys?")
                            .font(bold(16.0))
                            .color(white(0.95)),
                    );
                    ui.label(
                        RichText::new(format!(
                            "Press {xbox}, pick the game's tile and press {a} to play it with XMouse."
                        ))
                        .font(body(14.5))
                        .color(white(0.8)),
                    );
                });
        });
}

/// The deadzone finder card (F6), in whichever window takes the input.
fn finder_overlay(ctx: &egui::Context, s: &Session) {
    if let Some(v) = *s.shared.finder.lock().unwrap() {
        egui::Area::new(egui::Id::unique("finder"))
            .anchor(egui::Align2::RIGHT_TOP, vec2(-18.0, 76.0))
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(KEY)
                    .stroke(Stroke::new(1.5, SIGNAL))
                    .corner_radius(CornerRadius::same(16))
                    .inner_margin(Margin::same(16))
                    .show(ui, |ui| {
                        ui.set_width(300.0);
                        ui.label(RichText::new("Deadzone finder").font(bold(16.0)).color(INK));
                        ui.label(
                            RichText::new(format!("Right stick: {v:.2}"))
                                .font(mono(14.0))
                                .color(INK),
                        );
                        hint(ui, "Watch the camera on your TV. ] raises, [ lowers.");
                        hint(
                            ui,
                            "Stop where the camera just starts turning, then press F6 to save.",
                        );
                    });
            });
    }
}

fn short_console(c: &ConsoleInfo) -> String {
    if plugin_console_ref(&c.server_id).is_some() {
        // A plugin's console: its own model name (e.g. "PS5").
        return c.console_type.clone();
    }
    match c.console_type.as_str() {
        "XboxSeriesX" => "Series X",
        "XboxSeriesS" => "Series S",
        "XboxOneX" => "One X",
        "XboxOneS" => "One S",
        _ => "Xbox",
    }
    .to_string()
}

fn short_game(g: &str) -> String {
    let words: Vec<&str> = g.split_whitespace().collect();
    if words.len() > 4 {
        words[..4].join(" ") + "…"
    } else {
        g.to_string()
    }
}

/// Short controller name for list subtitles (e.g. "RT").
fn short_action(a: Action) -> &'static str {
    if crate::profile::playstation_names() {
        // Action::label already gives the short PlayStation names (L1, R2, Options…).
        return match a {
            Action::LeftThumb => "L3",
            Action::RightThumb => "R3",
            other => other.label(),
        };
    }
    match a {
        Action::LeftShoulder => "LB",
        Action::RightShoulder => "RB",
        Action::LeftTrigger => "LT",
        Action::RightTrigger => "RT",
        Action::LeftThumb => "L3",
        Action::RightThumb => "R3",
        Action::Nexus => "Xbox button",
        other => other.label(),
    }
}

fn mode_text(m: BindMode) -> &'static str {
    match m {
        BindMode::Hold => "hold",
        BindMode::Toggle => "press once, not hold",
        BindMode::Tap => "tap",
        BindMode::Turbo => "repeat while held",
    }
}

fn mode_hint(m: BindMode) -> &'static str {
    match m {
        BindMode::Hold => "Pressed while you hold the key.",
        BindMode::Toggle => "Stays pressed until you press again.",
        BindMode::Tap => "A short press.",
        BindMode::Turbo => "Presses again and again while held.",
    }
}

fn setup_group(ui: &mut Ui) {
    group(ui, |ui| {
        let steps = [
            (
                "Turn on remote features",
                "Settings > Devices & connections > Remote features.",
            ),
            ("Power mode: Sleep", "So XMouse can wake the console."),
            (
                "Allow connections",
                "Xbox app preferences: from any device, or from signed-in profiles.",
            ),
            (
                "Same home network",
                "Wired Ethernet gives the lowest delay.",
            ),
        ];
        for (i, (t, s)) in steps.iter().enumerate() {
            row(
                ui,
                i == 0,
                t,
                s,
                RowOptions {
                    lead: Lead::Key((i + 1).to_string(), false),
                    ..Default::default()
                },
                |_| {},
            );
        }
    });
}

fn confirm(question: &str) -> bool {
    rfd::MessageDialog::new()
        .set_title("XMouse")
        .set_description(question)
        .set_buttons(rfd::MessageButtons::YesNo)
        .show()
        == rfd::MessageDialogResult::Yes
}

/// The next key / mouse button / wheel notch / modifier pressed this frame, as a binding code.
/// A modifier on its own is bound when it's let go, so Shift+Esc can still mean "bind Esc".
fn next_input(ctx: &egui::Context) -> Option<String> {
    enum Mods {
        None,
        Alone(&'static str),
        Several,
    }
    let held_id = next_input_held_id();
    let (found, mods) = next_input_event(ctx);
    if found.is_some() {
        ctx.data_mut(|d| d.remove_temp::<String>(held_id));
        return found;
    }
    let mods = match mods {
        (true, false, false) => Mods::Alone("Shift"),
        (false, true, false) => Mods::Alone("Ctrl"),
        (false, false, true) => Mods::Alone("Alt"),
        (false, false, false) => Mods::None,
        _ => Mods::Several,
    };
    match mods {
        Mods::Alone(m) => {
            ctx.data_mut(|d| d.insert_temp(held_id, m.to_string()));
            None
        }
        Mods::Several => {
            ctx.data_mut(|d| d.remove_temp::<String>(held_id));
            None
        }
        // Released with nothing else pressed in between: that modifier is the binding.
        Mods::None => ctx.data_mut(|d| d.remove_temp::<String>(held_id)),
    }
}

fn next_input_held_id() -> egui::Id {
    egui::Id::unique("next-input-held-modifier")
}

/// Forget a modifier remembered by an earlier key pick (call when a new pick starts).
fn reset_next_input(ctx: &egui::Context) {
    ctx.data_mut(|d| d.remove_temp::<String>(next_input_held_id()));
}

/// This frame's first key / button / wheel input as a binding code, and which of
/// (Shift, Ctrl, Alt) are held.
fn next_input_event(ctx: &egui::Context) -> (Option<String>, (bool, bool, bool)) {
    let m = ctx.input(|i| i.modifiers);
    let mods = (m.shift, m.ctrl, m.alt);
    let found = ctx.input(|i| {
        for e in &i.events {
            match e {
                egui::Event::Key {
                    key,
                    physical_key,
                    pressed: true,
                    modifiers,
                    ..
                } => {
                    let k = physical_key.unwrap_or(*key);
                    if k == egui::Key::Escape && !modifiers.shift {
                        return Some("__cancel".to_string());
                    }
                    // Prefer the generic modifier names so left and right both work.
                    return Some(
                        match k.name() {
                            "ShiftLeft" | "ShiftRight" => "Shift",
                            "ControlLeft" | "ControlRight" => "Ctrl",
                            "AltLeft" | "AltRight" => "Alt",
                            other => other,
                        }
                        .to_string(),
                    );
                }
                egui::Event::PointerButton {
                    button,
                    pressed: true,
                    ..
                } => return Some(mouse_name(*button).to_string()),
                egui::Event::MouseWheel { delta, .. } if delta.y != 0.0 => {
                    return Some(
                        if delta.y > 0.0 {
                            "WheelUp"
                        } else {
                            "WheelDown"
                        }
                        .to_string(),
                    )
                }
                _ => {}
            }
        }
        None
    });
    (found, mods)
}

fn open_folder(path: &std::path::Path) {
    let _ = std::fs::create_dir_all(path);
    #[cfg(target_os = "windows")]
    let cmd = "explorer";
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let cmd = "xdg-open";
    let _ = std::process::Command::new(cmd).arg(path).spawn();
}

enum PanelCmd {
    Close,
    Assign(Action),
    Free,
    Mode(BindMode),
    Name(String),
    Switch(Option<String>),
}

/// Controller name on keycaps and chips (ASCII so every font has it).
fn chip_text(a: Action) -> String {
    match a {
        Action::LeftStickUp => "LS up".into(),
        Action::LeftStickDown => "LS dn".into(),
        Action::LeftStickLeft => "LS lt".into(),
        Action::LeftStickRight => "LS rt".into(),
        Action::RightStickUp => "RS up".into(),
        Action::RightStickDown => "RS dn".into(),
        Action::RightStickLeft => "RS lt".into(),
        Action::RightStickRight => "RS rt".into(),
        Action::DPadUp => "D up".into(),
        Action::DPadDown => "D dn".into(),
        Action::DPadLeft => "D lt".into(),
        Action::DPadRight => "D rt".into(),
        Action::Nexus if crate::profile::playstation_names() => "PS".into(),
        Action::Nexus => "Xbox".into(),
        Action::A if crate::profile::playstation_names() => "×".into(),
        Action::B if crate::profile::playstation_names() => "O".into(),
        Action::X if crate::profile::playstation_names() => "Sq".into(),
        Action::Y if crate::profile::playstation_names() => "Tri".into(),
        Action::Walk => "Walk".into(),
        Action::None => "".into(),
        other => short_action(other).to_string(),
    }
}

/// Right-hand panel of the drawn keyboard: "W: What should it do?"
fn key_panel(
    ui: &mut Ui,
    p: &Profile,
    li: usize,
    code: Option<&str>,
    search: &mut String,
) -> Option<PanelCmd> {
    let mut cmd = None;
    let Some(code) = code else {
        group(ui, |ui| {
            egui::Frame::new().inner_margin(Margin::same(18)).show(ui, |ui| {
                ui.label(RichText::new("Click a key").font(display(22.0)).color(INK));
                ui.add_space(4.0);
                hint(ui, "Pick any key or mouse button on the left and choose what it does on the controller.");
                ui.add_space(6.0);
                hint(ui, "Black keys belong to XMouse. Dashed keys can't be used.");
            });
        });
        return None;
    };
    let layer = &p.layers[li];
    let binding = layer.bindings.iter().find(|b| b.input == code).cloned();
    let inherited = if binding.is_none() && li > 0 {
        p.layers[0]
            .bindings
            .iter()
            .find(|b| b.input == code)
            .cloned()
    } else {
        None
    };
    let label = input_label(code);

    group(ui, |ui| {
        egui::Frame::new()
            .inner_margin(Margin {
                left: 18,
                right: 14,
                top: 16,
                bottom: 10,
            })
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("{label}: What should it do?"))
                            .font(display(20.0))
                            .color(INK),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                        if small_link(ui, "Close").clicked() {
                            cmd = Some(PanelCmd::Close);
                        }
                    });
                });
                let now = match (&binding, &inherited) {
                    (Some(b), _) => format!("Now: {} · {}", b.title(), short_action(b.action)),
                    (None, Some(b)) => format!("Now: {} (from “{}”)", b.title(), p.layers[0].name),
                    _ => "Now: free".to_string(),
                };
                ui.label(RichText::new(now).font(body(15.0)).color(MUTED));
                ui.add_space(8.0);
                text_field(
                    ui,
                    egui::TextEdit::singleline(search)
                        .hint_text("Search actions")
                        .desired_width(f32::INFINITY)
                        .font(medium(15.0))
                        .margin(vec2(10.0, 8.0)),
                );
            });
        divider(ui);
        egui::Frame::new()
            .inner_margin(Margin {
                left: 14,
                right: 14,
                top: 8,
                bottom: 4,
            })
            .show(ui, |ui| {
                ui.label(RichText::new("CONTROLLER").font(mono(12.0)).color(LABEL));
            });
        let q = search.to_lowercase();
        egui::ScrollArea::vertical()
            .id_salt("actions")
            .max_height(300.0)
            .show(ui, |ui| {
                let mut first = true;
                for a in Action::ALL {
                    if a == Action::None {
                        continue;
                    }
                    let name = a.label();
                    if !q.is_empty()
                        && !name.to_lowercase().contains(&q)
                        && !chip_text(a).to_lowercase().contains(&q)
                    {
                        continue;
                    }
                    let selected = binding.as_ref().map(|b| b.action == a).unwrap_or(false);
                    let r = row(
                        ui,
                        first,
                        name,
                        "",
                        RowOptions {
                            clickable: true,
                            selected,
                            ..Default::default()
                        },
                        |ui| {
                            pad_chip(ui, &chip_text(a));
                        },
                    );
                    if r.clicked() {
                        cmd = Some(PanelCmd::Assign(a));
                    }
                    first = false;
                }
            });
    });

    if let Some(b) = &binding {
        group(ui, |ui| {
            let mut name = b.label.clone();
            control_row(ui, true, "Name", "What it does in the game.", |ui| {
                if text_field(
                    ui,
                    egui::TextEdit::singleline(&mut name)
                        .hint_text("e.g. Reload")
                        .desired_width(170.0)
                        .font(medium(15.0)),
                )
                .changed()
                {
                    cmd = Some(PanelCmd::Name(name.clone()));
                }
            });
            let modes = [
                BindMode::Hold,
                BindMode::Toggle,
                BindMode::Tap,
                BindMode::Turbo,
            ];
            let items: Vec<(String, Option<String>)> = ["Hold", "Once", "Tap", "Repeat"]
                .iter()
                .map(|s| (s.to_string(), None))
                .collect();
            control_row(ui, false, "Press", mode_hint(b.mode), |ui| {
                let cur = modes.iter().position(|m| *m == b.mode).unwrap_or(0);
                // right-to-left layout: wrap in a left-to-right child
                {
                    if let Some(i) = segmented(ui, "mode", &items, cur) {
                        cmd = Some(PanelCmd::Mode(modes[i]));
                    }
                }
            });
            control_row(
                ui,
                false,
                "Then switch to",
                "Change layer on press.",
                |ui| {
                    let current = b.switch_layer.clone().unwrap_or_else(|| "Stay".into());
                    egui::ComboBox::from_id_salt("kp-switch")
                        .width(150.0)
                        .selected_text(current)
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_label(b.switch_layer.is_none(), "Stay")
                                .clicked()
                            {
                                cmd = Some(PanelCmd::Switch(None));
                            }
                            for l in &p.layers {
                                if ui
                                    .selectable_label(
                                        b.switch_layer.as_deref() == Some(l.name.as_str()),
                                        &l.name,
                                    )
                                    .clicked()
                                {
                                    cmd = Some(PanelCmd::Switch(Some(l.name.clone())));
                                }
                            }
                        });
                },
            );
            if row(
                ui,
                false,
                "Free this key",
                "",
                RowOptions {
                    danger: true,
                    ..RowOptions::link()
                },
                |_| {},
            )
            .clicked()
            {
                cmd = Some(PanelCmd::Free);
            }
        });
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_transitions_go_the_way_you_navigate() {
        let page = View::Page;
        assert_eq!(transition_direction(page(Page::Home), page(Page::More)), 1.0);
        assert_eq!(transition_direction(page(Page::More), page(Page::Keys)), 1.0);
        assert_eq!(transition_direction(page(Page::Keys), page(Page::Test)), 1.0);
        assert_eq!(transition_direction(page(Page::Test), page(Page::Keys)), -1.0);
        assert_eq!(transition_direction(page(Page::Keys), page(Page::More)), -1.0);
        assert_eq!(transition_direction(page(Page::Consoles), page(Page::Home)), -1.0);
        // Same depth, or not between pages at all: a plain rise.
        assert_eq!(transition_direction(page(Page::Keys), page(Page::Mouse)), 0.0);
        assert_eq!(transition_direction(View::Platform, page(Page::Home)), 0.0);
        assert_eq!(transition_direction(page(Page::Home), View::Session(0)), 0.0);
    }

    #[test]
    fn nav_animates_changes_but_not_the_first_view() {
        let ctx = egui::Context::default();
        let mut nav = Nav::new();
        let frame = |nav: &mut Nav, time: f64, view: View| {
            let mut out = (0.0, 0.0);
            let input = egui::RawInput {
                time: Some(time),
                ..Default::default()
            };
            let _ = ctx.run_ui(input, |ui| out = nav.show(ui.ctx(), view));
            out
        };
        assert_eq!(frame(&mut nav, 0.0, View::Page(Page::Home)).0, 1.0, "no fade at launch");
        assert!(!nav.fresh);
        let (t, dir) = frame(&mut nav, 1.0, View::Page(Page::More));
        assert_eq!((t, dir), (0.0, 1.0), "a new page starts invisible, coming from the right");
        assert!(nav.fresh, "and scrolled to its top");
        let (mid, _) = frame(&mut nav, 1.0 + motion::SLOW as f64 / 2.0, View::Page(Page::More));
        assert!(mid > 0.5 && mid < 1.0, "eased: more than half way at half time, got {mid}");
        assert_eq!(frame(&mut nav, 2.0, View::Page(Page::More)).0, 1.0);
        assert_eq!(frame(&mut nav, 3.0, View::Page(Page::Home)), (0.0, -1.0));
    }

    #[test]
    fn toasts_stay_long_enough_to_read_and_ease_in_and_out() {
        assert_eq!(toast_secs("Signed in"), 5.0);
        assert!(toast_secs(&"x".repeat(120)) > 9.0);
        assert_eq!(toast_secs(&"x".repeat(2000)), 12.0);
        assert_eq!(toast_phase(0.0, 5.0), (0.0, 1.0));
        assert_eq!(toast_phase(1.0, 4.0), (1.0, 1.0));
        assert_eq!(toast_phase(5.0, 0.0), (1.0, 0.0));
        let (t_in, _) = toast_phase(motion::SLOW / 2.0, 4.0);
        assert!(t_in > 0.5 && t_in < 1.0);
    }

    #[test]
    fn cards_wrap_instead_of_overflowing() {
        assert_eq!(per_line(852.0, 260.0, 16.0), 3);
        assert_eq!(per_line(1232.0, 220.0, 16.0), 5);
        assert_eq!(per_line(100.0, 260.0, 16.0), 1, "never zero");
    }
}
