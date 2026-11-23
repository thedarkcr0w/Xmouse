#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

use xmouse::app::{Options, XMouseApp};

fn main() -> eframe::Result {
    // The release build has no console window, so also write the log to a file the user can
    // send when a connection fails: %APPDATA%\XMouse\config\xmouse.log (overwritten each run).
    let mut logger = env_logger::Builder::from_env(
        env_logger::Env::default()
            .default_filter_or("info,webrtc=warn,webrtc_ice=info,webrtc_sctp=warn"),
    );
    let log_dir = xmouse::storage::Storage::default_dir();
    let _ = std::fs::create_dir_all(&log_dir);
    if let Ok(file) = std::fs::File::create(log_dir.join("xmouse.log")) {
        logger.target(env_logger::Target::Pipe(Box::new(file)));
    }
    logger.init();
    log::info!("XMouse {} starting", env!("CARGO_PKG_VERSION"));
    // Windows wakes timers every ~15.6 ms by default, so a "120 Hz" input loop really ran at
    // ~64 Hz with uneven gaps. Ask for 1 ms resolution for as long as XMouse runs.
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::Media::timeBeginPeriod(1);
    }
    let args: Vec<String> = std::env::args().collect();
    let opts = Options {
        play: args.iter().any(|a| a == "--play"),
    };

    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("XMouse")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([900.0, 600.0]),
        // Process input as soon as it arrives instead of waiting for the next display refresh.
        glow_options: egui_glow::GlowConfiguration {
            vsync: false,
            ..Default::default()
        },
        // A test copy run with its own config folder keeps its window state there too, so it
        // can't move or resize the real app's window.
        persistence_path: std::env::var_os("XMOUSE_CONFIG_DIR")
            .map(|d| std::path::PathBuf::from(d).join("app.ron")),
        // Always open at the normal size on screen: a remembered position can point off-screen
        // (another monitor, or Windows' -32000 "minimised" spot) and the window seems gone.
        persist_window: false,
        ..Default::default()
    };
    eframe::run_native(
        "XMouse",
        native,
        Box::new(move |cc| Ok(Box::new(XMouseApp::new(cc, opts)))),
    )
}
