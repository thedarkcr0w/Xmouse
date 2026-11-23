//! Settings and profiles on disk.

use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::profile::{builtin_profiles, Profile};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PlayMode {
    #[default]
    Tv,
    Pc,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// Video bitrate (kbps) in TV Play – the TV picture is unaffected.
    pub tv_bitrate: u32,
    /// Video bitrate (kbps) in PC Play, 0 = automatic.
    pub pc_bitrate: u32,
    /// Prefer the H.264 High/Main profile in PC Play.
    pub pc_high_quality: bool,
    /// Smooth (bilinear) scaling of the PC Play picture.
    pub pc_smooth_scaling: bool,
    /// Upscale the PC Play picture with NVIDIA RTX Video Super Resolution (GPU path).
    pub pc_rtx_vsr: bool,
    /// Decode the PC Play video on the graphics card instead of the CPU.
    pub pc_gpu_decode: bool,
    /// Soften compression blocks, banding and noise in the PC Play picture.
    pub pc_cleanup: bool,
    /// Clean-up strength: stronger instead of light.
    pub pc_cleanup_strong: bool,
    /// Sharpen the PC Play picture, and how much (0–1).
    pub pc_sharpen: bool,
    pub pc_sharpen_amount: f32,
    /// Colour adjustments: vibrance (0–1), contrast and brightness (0 = unchanged).
    pub pc_colour: bool,
    pub pc_vibrance: f32,
    pub pc_contrast: f32,
    pub pc_brightness: f32,
    /// Add in-between frames (frame generation).
    pub pc_smooth_motion: bool,
    /// Frame generation by AMD FSR / NVIDIA DLSS (their files are installed on request).
    pub pc_fsr_fg: bool,
    pub pc_dlss_fg: bool,
    /// Enlarge the picture with XMouse's own edge-aware scaler when RTX Video isn't used.
    pub pc_sharp_upscale: bool,
    pub input_hz: u32,
    pub show_hud: bool,
    pub last_console: String,
    pub last_profile: String,
    pub last_mode: PlayMode,
    /// Which kind of console you play: "xbox", a plugin's id (e.g. "psnbridge"), or empty
    /// when a plugin is installed and you haven't chosen yet.
    pub console_platform: String,
    /// Account IDs that plugins need for pairing (e.g. a PSN account ID), by plugin id.
    pub plugin_accounts: std::collections::BTreeMap<String, String>,
    /// The Xbox's address on the home network (e.g. 192.168.1.20). Empty = find it automatically.
    pub xbox_console_ip: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            tv_bitrate: 1000,
            pc_bitrate: 0,
            pc_high_quality: true,
            pc_smooth_scaling: true,
            pc_rtx_vsr: true,
            pc_gpu_decode: false,
            pc_cleanup: false,
            pc_cleanup_strong: false,
            pc_sharpen: false,
            pc_sharpen_amount: 0.5,
            pc_colour: false,
            pc_vibrance: 0.0,
            pc_contrast: 0.0,
            pc_brightness: 0.0,
            pc_smooth_motion: false,
            pc_fsr_fg: false,
            pc_dlss_fg: false,
            pc_sharp_upscale: false,
            input_hz: 120,
            show_hud: true,
            last_console: String::new(),
            last_profile: "gta-v".into(),
            last_mode: PlayMode::Tv,
            console_platform: String::new(),
            plugin_accounts: Default::default(),
            xbox_console_ip: String::new(),
        }
    }
}

pub struct Storage {
    pub dir: PathBuf,
    pub profile_dir: PathBuf,
}

impl Storage {
    pub fn new(dir: PathBuf) -> Self {
        let profile_dir = dir.join("profiles");
        let _ = std::fs::create_dir_all(&profile_dir);
        Self { dir, profile_dir }
    }

    /// `%APPDATA%\XMouse\config` (or the folder in `XMOUSE_CONFIG_DIR`, for testing).
    pub fn default_dir() -> PathBuf {
        if let Some(dir) = std::env::var_os("XMOUSE_CONFIG_DIR") {
            return PathBuf::from(dir);
        }
        directories::ProjectDirs::from("", "", "XMouse")
            .map(|d| d.config_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".xmouse"))
    }

    pub fn token_file(&self) -> PathBuf {
        self.dir.join("xbox-tokens.json")
    }

    /// Present while the GPU upscaler is in its first moments of a new setup. If XMouse finds it
    /// at the next start, the graphics driver crashed during upscaling.
    pub fn vsr_guard_file(&self) -> PathBuf {
        self.dir.join("rtx-video-guard")
    }

    /// Present while frame generation is in its first moments of a session. If XMouse finds it
    /// at the next start, the graphics driver crashed with it.
    pub fn framegen_guard_file(&self) -> PathBuf {
        self.dir.join("framegen-guard")
    }

    pub fn settings(&self) -> Settings {
        // Tolerate a UTF-8 byte-order mark (Windows editors and PowerShell 5.1 add one); without
        // this a hand-edited file silently fell back to defaults (TV Play, first profile).
        std::fs::read_to_string(self.dir.join("settings.json"))
            .ok()
            .and_then(|s| serde_json::from_str(s.trim_start_matches('\u{feff}')).ok())
            .unwrap_or_default()
    }

    pub fn save_settings(&self, s: &Settings) {
        if let Ok(j) = serde_json::to_string_pretty(s) {
            let _ = std::fs::write(self.dir.join("settings.json"), j);
        }
    }

    fn file(&self, id: &str) -> PathBuf {
        let safe: String = id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        self.profile_dir.join(format!("{safe}.json"))
    }

    /// Built-ins (or your saved edits of them) first, then your own profiles.
    pub fn profiles(&self) -> Vec<Profile> {
        let mut saved: Vec<Profile> = std::fs::read_dir(&self.profile_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .filter_map(|s| serde_json::from_str::<Profile>(s.trim_start_matches('\u{feff}')).ok())
            .map(|p| p.normalize())
            .collect();
        let mut out = Vec::new();
        for b in builtin_profiles() {
            if let Some(i) = saved.iter().position(|p| p.id == b.id) {
                let mut p = saved.remove(i);
                p.builtin = true;
                out.push(p);
            } else {
                out.push(b);
            }
        }
        saved.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        out.extend(saved);
        out
    }

    pub fn save_profile(&self, p: &Profile) -> Result<()> {
        std::fs::write(self.file(&p.id), serde_json::to_string_pretty(p)?)?;
        Ok(())
    }

    /// Delete a custom profile, or reset a built-in one.
    pub fn delete_profile(&self, id: &str) {
        let _ = std::fs::remove_file(self.file(id));
    }

    pub fn import_profile(&self, path: &std::path::Path) -> Result<Profile> {
        let text = std::fs::read_to_string(path)?;
        let mut p: Profile = serde_json::from_str(text.trim_start_matches('\u{feff}'))?;
        p.builtin = false;
        if self.profiles().iter().any(|x| x.id == p.id) {
            p.id = format!(
                "{}-{}",
                p.id,
                uuid::Uuid::new_v4()
                    .simple()
                    .to_string()
                    .get(..6)
                    .unwrap_or("copy")
            );
        }
        let p = p.normalize();
        self.save_profile(&p)?;
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_roundtrip_and_reset() {
        let dir = std::env::temp_dir().join(format!("xmouse-test-{}", uuid::Uuid::new_v4()));
        let s = Storage::new(dir.clone());
        let mut list = s.profiles();
        assert_eq!(list.len(), builtin_profiles().len());
        list[0].mouse.sensitivity = 1.23;
        s.save_profile(&list[0]).unwrap();
        let mut custom = list[1].clone();
        custom.id = "mine".into();
        custom.builtin = false;
        s.save_profile(&custom).unwrap();
        let again = s.profiles();
        assert_eq!(again[0].mouse.sensitivity, 1.23);
        assert!(again[0].builtin);
        assert!(again.iter().any(|p| p.id == "mine" && !p.builtin));
        s.delete_profile(&list[0].id);
        assert_eq!(
            s.profiles()[0].mouse.sensitivity,
            builtin_profiles()[0].mouse.sensitivity
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn picture_switches_default_to_off_and_old_settings_files_still_load() {
        let d = Settings::default();
        assert!(!d.pc_cleanup && !d.pc_cleanup_strong && !d.pc_sharpen && !d.pc_colour);
        assert!(!d.pc_smooth_motion && !d.pc_sharp_upscale);
        assert_eq!((d.pc_vibrance, d.pc_contrast, d.pc_brightness), (0.0, 0.0, 0.0));
        // A settings file written before these existed: its values are kept, the new ones
        // come in switched off.
        let dir = std::env::temp_dir().join(format!("xmouse-test-{}", uuid::Uuid::new_v4()));
        let s = Storage::new(dir.clone());
        std::fs::write(
            dir.join("settings.json"),
            r#"{"tvBitrate":1500,"pcBitrate":20000,"pcHighQuality":true,"pcSmoothScaling":false,
                "pcRtxVsr":false,"pcGpuDecode":true,"inputHz":60,"showHud":false,
                "lastConsole":"X","lastProfile":"gta-v","lastMode":"pc"}"#,
        )
        .unwrap();
        let old = s.settings();
        assert_eq!((old.tv_bitrate, old.pc_bitrate, old.input_hz), (1500, 20000, 60));
        assert!(!old.pc_smooth_scaling && old.pc_gpu_decode && old.last_mode == PlayMode::Pc);
        assert!(!old.pc_cleanup && !old.pc_sharpen && !old.pc_colour && !old.pc_smooth_motion);
        assert_eq!(old.pc_sharpen_amount, d.pc_sharpen_amount);
        // And they survive a save and load.
        let mut new = old.clone();
        new.pc_smooth_motion = true;
        new.pc_vibrance = 0.35;
        s.save_settings(&new);
        assert_eq!(s.settings(), new);
        assert!(std::fs::read_to_string(dir.join("settings.json"))
            .unwrap()
            .contains("\"pcSmoothMotion\": true"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn settings_with_byte_order_mark_still_load() {
        let dir = std::env::temp_dir().join(format!("xmouse-test-{}", uuid::Uuid::new_v4()));
        let s = Storage::new(dir.clone());
        let mut set = Settings::default();
        set.last_profile = "gta-vi".into();
        s.save_settings(&set);
        let path = dir.join("settings.json");
        let json = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("\u{feff}{json}")).unwrap();
        assert_eq!(s.settings().last_profile, "gta-vi");
        let _ = std::fs::remove_dir_all(dir);
    }
}
