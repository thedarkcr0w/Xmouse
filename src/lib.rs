//! XMouse: play your own Xbox with mouse and keyboard over Xbox remote play.

pub mod audio;
pub mod auth;
pub mod chime;
pub mod mapper;
pub mod packet;
pub mod plugin;
pub mod profile;
pub mod ps;
pub mod storage;
pub mod stream;
pub mod glpipe;
pub mod pacing;
pub mod testvideo;
pub mod video;
#[cfg(windows)]
pub mod framegen;
#[cfg(windows)]
pub mod hwdecode;
#[cfg(windows)]
pub mod vsr;
pub mod xbox_lan;
pub mod xhome;

/// Pick the TLS crypto backend once (several dependencies enable different ones).
pub fn init_crypto() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub mod app;
pub mod keyboard;
pub mod padview;
pub mod theme;
