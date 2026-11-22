//! Настройки клиента: %APPDATA%\\Amadeus\\settings.json.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::stream::Preset;

/// Адрес сервера по умолчанию (переопределяется при сборке: `AMADEUS_SERVER=https://… cargo build`).
pub const DEFAULT_SERVER: &str = match option_env!("AMADEUS_SERVER") {
    Some(s) => s,
    None => "https://api.murmurchat.ru",
};

/// Прежние адреса сервера по умолчанию: сохранённый в settings.json такой адрес при загрузке меняется на [`DEFAULT_SERVER`]
/// (переезд на murmurchat.ru, 2026-11-22).
const OLD_DEFAULT_SERVERS: &[&str] = &["https://api.steins-gate.ru"];

/// Клавиша рации по умолчанию — V (виртуальный код Windows).
pub const DEFAULT_PTT_KEY: u32 = 0x56;

/// Тема оформления.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemePref {
    /// Как в Windows (по умолчанию).
    #[default]
    System,
    Dark,
    Light,
}

#[cfg_attr(not(debug_assertions), allow(dead_code))] // галерея (только отладка)
impl ThemePref {
    pub const ALL: [ThemePref; 3] = [ThemePref::System, ThemePref::Dark, ThemePref::Light];

    pub fn label(self) -> &'static str {
        match self {
            ThemePref::System => "Как в Windows",
            ThemePref::Dark => "Тёмная",
            ThemePref::Light => "Светлая",
        }
    }
}

/// Анимации интерфейса.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnimPref {
    #[default]
    All,
    /// Только волна речи и анимация подключения.
    Important,
    /// Всё статично.
    Off,
}

/// Как показывать, что человек говорит.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpeechIndicator {
    #[default]
    Wave,
    /// Обводка вокруг аватара, без волны.
    Ring,
}

/// Пинг в заголовке канала.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PingDisplay {
    #[default]
    WaveAndMs,
    MsOnly,
    Hidden,
}

/// Шумоподавление микрофона.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoiseMode {
    /// Без шумоподавления (студийный микрофон, тихая комната).
    Off,
    /// Шумоподавление WebRTC — режет верх, голос «глуше».
    Standard,
    /// Лёгкая нейросеть RNNoise: +10 мс, дёшево, но белое шипение почти не убирает.
    #[serde(rename = "rnnoise")]
    Light,
    /// Нейросеть DeepFilterNet3 (по умолчанию): убирает и шум, и шипение, полоса 48 кГц сохраняется; +30 мс.
    #[default]
    Neural,
}

impl NoiseMode {
    pub const ALL: [NoiseMode; 4] = [NoiseMode::Neural, NoiseMode::Light, NoiseMode::Standard, NoiseMode::Off];

    pub fn label(self) -> &'static str {
        match self {
            NoiseMode::Off => "Выключено",
            NoiseMode::Standard => "Обычное (WebRTC)",
            NoiseMode::Light => "Нейросеть, лёгкая (RNNoise)",
            NoiseMode::Neural => "Нейросеть (DeepFilterNet)",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub server_url: String,
    /// Сессионный токен (лежит в профиле пользователя, как у браузера).
    pub session_token: Option<String>,
    pub last_name: String,
    /// GUID устройства WASAPI; None — системное по умолчанию.
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    pub ptt_enabled: bool,
    pub ptt_key: u32,
    pub echo_cancellation: bool,
    /// Шумоподавление микрофона (старое поле noise_suppression: bool больше не читается).
    pub noise: NoiseMode,
    pub auto_gain: bool,
    pub sounds: bool,
    /// Громкость звуков интерфейса, 0–100.
    pub sounds_volume: u8,
    /// Громкость участников по id пользователя (identity), 1.0 = 100%.
    pub user_volume: HashMap<String, f32>,
    /// Заглушённые для себя участники.
    pub user_muted: HashMap<String, bool>,
    /// Качество трансляции по умолчанию (последнее выбранное).
    pub stream_preset: Preset,
    /// Передавать звук окна/системы в трансляции.
    pub stream_audio: bool,
    /// Громкость звука трансляций участников по identity (отдельно от голоса).
    pub stream_volume: HashMap<String, f32>,
    /// «Закрыть» прячет окно в трей (выход — меню значка в трее).
    pub close_to_tray: bool,
    /// Запускать вместе с Windows (только у установленной программы).
    pub autostart: bool,
    /// Тема оформления.
    pub theme: ThemePref,
    /// Анимации: None — по системной настройке Windows («Показывать анимацию» выкл. → «Выкл.», иначе «Все»).
    pub animations: Option<AnimPref>,
    /// Останавливать анимации, когда окно свёрнуто или не в фокусе.
    pub pause_anim_in_background: bool,
    pub speech_indicator: SpeechIndicator,
    pub ping_display: PingDisplay,
    /// Компактные плитки участников (≈200×68).
    pub compact_tiles: bool,
    /// Живой кот в заголовке окна (моргает, шевелит ушами).
    pub live_cat: bool,
    /// Последний открытый сервер.
    pub last_server: Option<i64>,
    /// Свёрнутые категории каналов (id).
    pub collapsed_categories: Vec<i64>,
    /// Показывать друзьям и соседям по серверам, во что я играю.
    pub share_activity: bool,
    /// «Не выходить» на экране входа: сохранять сеанс между запусками.
    pub stay_signed_in: bool,
    /// Свернуть «Не в сети» в списке друзей.
    pub friends_offline_open: bool,
    /// Показан список участников справа (на сервере).
    pub members_open: bool,
    /// Недавние эмодзи (панель эмодзи), новые — первыми.
    pub recent_emoji: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            server_url: DEFAULT_SERVER.to_string(),
            session_token: None,
            last_name: String::new(),
            input_device: None,
            output_device: None,
            ptt_enabled: false,
            ptt_key: DEFAULT_PTT_KEY,
            echo_cancellation: true,
            noise: NoiseMode::default(),
            auto_gain: true,
            sounds: true,
            sounds_volume: 60,
            user_volume: HashMap::new(),
            user_muted: HashMap::new(),
            stream_preset: Preset::default(),
            stream_audio: true,
            stream_volume: HashMap::new(),
            close_to_tray: true,
            autostart: true,
            theme: ThemePref::default(),
            animations: None,
            pause_anim_in_background: true,
            speech_indicator: SpeechIndicator::default(),
            ping_display: PingDisplay::default(),
            compact_tiles: false,
            live_cat: true,
            last_server: None,
            collapsed_categories: Vec::new(),
            share_activity: true,
            stay_signed_in: true,
            friends_offline_open: false,
            members_open: true,
            recent_emoji: Vec::new(),
        }
    }
}

/// Каталог настроек: `AMADEUS_CONFIG_DIR` (например, второй клиент на том же ПК) или %APPDATA%\\Amadeus.
fn path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("AMADEUS_CONFIG_DIR") {
        return Some(PathBuf::from(dir).join("settings.json"));
    }
    directories::ProjectDirs::from("", "", "Amadeus").map(|d| d.config_dir().join("settings.json"))
}

/// Файл лога рядом с настройками (при старте больше 2 МБ — начинается заново).
pub fn log_path() -> Option<PathBuf> {
    path().map(|p| p.with_file_name("amadeus.log"))
}

impl Settings {
    pub fn load() -> Self {
        let Some(p) = path() else { return Self::default() };
        match std::fs::read(&p) {
            Ok(bytes) => {
                let mut s: Self = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                    log::warn!("settings.json повреждён ({e}), беру настройки по умолчанию");
                    Self::default()
                });
                if s.migrate_server() {
                    log::info!("адрес сервера заменён на {}", s.server_url);
                    s.save();
                }
                s
            }
            Err(_) => Self::default(),
        }
    }

    /// Старый адрес сервера по умолчанию → новый. true — если поменяли.
    fn migrate_server(&mut self) -> bool {
        let cur = self.server_url.trim().trim_end_matches('/');
        if OLD_DEFAULT_SERVERS.iter().any(|o| o.eq_ignore_ascii_case(cur)) && cur != DEFAULT_SERVER {
            self.server_url = DEFAULT_SERVER.to_string();
            return true;
        }
        false
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = p.with_extension("json.tmp");
        match serde_json::to_vec_pretty(self) {
            Ok(bytes) => {
                if std::fs::write(&tmp, bytes).is_ok() {
                    let _ = std::fs::rename(&tmp, &p);
                }
            }
            Err(e) => log::error!("не удалось сохранить настройки: {e}"),
        }
    }

    /// Анимации с учётом системной настройки Windows (если пользователь не выбирал сам).
    pub fn animations_effective(&self) -> AnimPref {
        self.animations.unwrap_or_else(|| if crate::anim::system_animations_enabled() { AnimPref::All } else { AnimPref::Off })
    }

    pub fn volume_of(&self, identity: &str) -> f32 {
        self.user_volume.get(identity).copied().unwrap_or(1.0)
    }

    pub fn stream_volume_of(&self, identity: &str) -> f32 {
        self.stream_volume.get(identity).copied().unwrap_or(1.0)
    }

    pub fn is_user_muted(&self, identity: &str) -> bool {
        self.user_muted.get(identity).copied().unwrap_or(false)
    }

    /// Итоговая громкость участника для WebRTC (с учётом «заглушить для себя»).
    pub fn effective_volume(&self, identity: &str) -> f32 {
        if self.is_user_muted(identity) { 0.0 } else { self.volume_of(identity) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_settings_default_theme() {
        let s: Settings = serde_json::from_str(r#"{"noise_suppression":true}"#).unwrap();
        assert_eq!(s.theme, ThemePref::System);
    }

    #[test]
    fn old_settings_default_appearance() {
        let s: Settings = serde_json::from_str(r#"{"sounds":true}"#).unwrap();
        assert_eq!(s.animations, None);
        assert!(s.pause_anim_in_background && s.live_cat && !s.compact_tiles);
        assert_eq!((s.speech_indicator, s.ping_display, s.sounds_volume), (SpeechIndicator::Wave, PingDisplay::WaveAndMs, 60));
    }

    #[test]
    fn old_server_migrates() {
        let mut s: Settings = serde_json::from_str(r#"{"server_url":"https://api.steins-gate.ru/"}"#).unwrap();
        assert!(s.migrate_server());
        assert_eq!(s.server_url, DEFAULT_SERVER);
        let mut own: Settings = serde_json::from_str(r#"{"server_url":"https://chat.example.com"}"#).unwrap();
        assert!(!own.migrate_server());
        assert_eq!(own.server_url, "https://chat.example.com");
    }
}
