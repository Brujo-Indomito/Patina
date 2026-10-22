//! User settings for the settings/tray app, stored in
//! `%APPDATA%\Patina\settings.toml`, plus the "Start with Windows" entry.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegGetValueW, RegOpenKeyExW, RegSetKeyValueW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE,
    REG_SZ, RRF_RT_REG_SZ,
};
use windows_core::HSTRING;

use crate::effects::Effect;
use crate::publisher::{Config, Feed};
use crate::segment;

pub const DEFAULT_BLUR: u32 = 8;
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const RUN_VALUE: &str = "Patina";
/// Command-line flag the autostart entry passes to start hidden in the tray.
pub const MINIMIZED_FLAG: &str = "--minimized";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectKind {
    #[default]
    None,
    Blur,
    Background,
}

/// Which segmentation model to use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelChoice {
    /// MediaPipe if present, else RVM.
    #[default]
    Automatic,
    /// Settings saved before MODNet was dropped load as MediaPipe.
    #[serde(alias = "Modnet")]
    MediaPipe,
    Rvm,
}

impl ModelChoice {
    pub const ALL: [ModelChoice; 3] = [ModelChoice::Automatic, ModelChoice::MediaPipe, ModelChoice::Rvm];

    pub fn label(self) -> &'static str {
        match self {
            ModelChoice::Automatic => "Automatic",
            ModelChoice::MediaPipe => "MediaPipe (fast, Apache-2.0)",
            ModelChoice::Rvm => "RVM (best edges, GPL-3.0)",
        }
    }

    /// The model file this choice resolves to for the given backend, if available.
    pub fn path(self, gpu: bool) -> Option<PathBuf> {
        match self {
            ModelChoice::Automatic => segment::default_model_path(gpu),
            ModelChoice::MediaPipe => segment::find_model(segment::MEDIAPIPE),
            ModelChoice::Rvm => segment::rvm_path(gpu),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Webcam name to use; `None` picks automatically.
    pub camera: Option<String>,
    pub effect: EffectKind,
    pub blur_strength: u32,
    pub background_image: Option<PathBuf>,
    pub model: ModelChoice,
    pub use_gpu: bool,
    pub show_preview: bool,
    pub start_with_windows: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            camera: None,
            effect: EffectKind::None,
            blur_strength: DEFAULT_BLUR,
            background_image: None,
            model: ModelChoice::Automatic,
            use_gpu: true,
            show_preview: true,
            start_with_windows: false,
        }
    }
}

impl Settings {
    pub fn path() -> PathBuf {
        let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
        base.join("Patina").join("settings.toml")
    }

    /// Loads saved settings, falling back to defaults if there are none or
    /// they can't be read.
    pub fn load() -> Self {
        std::fs::read_to_string(Self::path()).ok().and_then(|text| toml::from_str(&text).ok()).unwrap_or_default()
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Self::path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        let text = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
    }

    /// The effect these settings describe, if it can be applied.
    pub fn effect(&self) -> Option<Effect> {
        match self.effect {
            EffectKind::None => None,
            EffectKind::Blur => Some(Effect::Blur { strength: self.blur_strength.clamp(1, 20) }),
            EffectKind::Background => self.background_image.clone().map(|image| Effect::Replace { image }),
        }
    }

    pub fn to_config(&self) -> Config {
        Config {
            feed: Feed::Camera(self.camera.clone()),
            effect: self.effect(),
            model: self.model.path(self.use_gpu),
            allow_gpu: self.use_gpu,
        }
    }
}

/// Whether the "Start with Windows" entry exists.
pub fn start_with_windows_enabled() -> bool {
    let mut size = 0u32;
    let status = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, &HSTRING::from(RUN_KEY), &HSTRING::from(RUN_VALUE), RRF_RT_REG_SZ, None, None, Some(&mut size))
    };
    status == ERROR_SUCCESS
}

/// Adds or removes the per-user autostart entry that launches `exe` hidden in the tray.
pub fn set_start_with_windows(enabled: bool, exe: &Path) -> Result<(), String> {
    if enabled {
        let command = format!("\"{}\" {MINIMIZED_FLAG}", exe.display());
        let data: Vec<u16> = command.encode_utf16().chain([0]).collect();
        let status = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                &HSTRING::from(RUN_KEY),
                &HSTRING::from(RUN_VALUE),
                REG_SZ.0,
                Some(data.as_ptr().cast()),
                (data.len() * 2) as u32,
            )
        };
        status.ok().map_err(|e| format!("cannot add the startup entry: {e}"))
    } else {
        let mut key = HKEY::default();
        let opened = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, &HSTRING::from(RUN_KEY), None, KEY_SET_VALUE, &mut key) };
        if opened != ERROR_SUCCESS {
            return Ok(());
        }
        unsafe {
            let _ = RegDeleteValueW(key, &HSTRING::from(RUN_VALUE));
            let _ = RegCloseKey(key);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_through_toml() {
        let settings = Settings {
            camera: Some("GUSGU".into()),
            effect: EffectKind::Background,
            blur_strength: 12,
            background_image: Some(PathBuf::from("C:\\pictures\\garden.jpg")),
            model: ModelChoice::Rvm,
            use_gpu: false,
            show_preview: false,
            start_with_windows: true,
        };
        let text = toml::to_string_pretty(&settings).unwrap();
        assert_eq!(toml::from_str::<Settings>(&text).unwrap(), settings);
    }

    #[test]
    fn old_modnet_choice_loads_as_mediapipe() {
        let settings: Settings = toml::from_str("model = \"Modnet\"").unwrap();
        assert_eq!(settings.model, ModelChoice::MediaPipe);
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let settings: Settings = toml::from_str("effect = \"Blur\"").unwrap();
        assert_eq!(settings.effect, EffectKind::Blur);
        assert_eq!(settings.blur_strength, DEFAULT_BLUR);
        assert!(settings.use_gpu);
    }

    #[test]
    fn background_without_image_means_no_effect() {
        let settings = Settings { effect: EffectKind::Background, background_image: None, ..Settings::default() };
        assert_eq!(settings.effect(), None);
        let blur = Settings { effect: EffectKind::Blur, blur_strength: 50, ..Settings::default() };
        assert_eq!(blur.effect(), Some(Effect::Blur { strength: 20 }));
    }
}
