//! Захват окна/экрана на Windows для стрима (фаза 4).
//!
//! - [`sources`] — перечисление мониторов и окон с миниатюрой и иконкой (для пикера).
//! - [`capture`] — захват через Windows Graphics Capture (крейт `windows-capture`),
//!   BGRA → I420 и масштабирование через libyuv (из libwebrtc), в очереди максимум 1 кадр.
//! - [`hw`] — тот же захват с кодированием H.264 на видеокарте (Media Foundation).
//! - [`app_audio`] — звук окна/системы через WASAPI process loopback (фаза 5).

#![cfg(windows)]

// Символ STL 14.44 для предкомпилированного libwebrtc (нужен тестам этого крейта).
use msvc_shim as _;

pub mod app_audio;
pub mod camera;
pub mod capture;
pub mod diag;
pub mod h264qp;
pub mod hw;
pub mod sources;

pub use app_audio::{AppAudio, AudioSink, AudioTarget};
pub use capture::{Capture, CaptureConfig, Event, Sink};
pub use libwebrtc::video_frame::I420Buffer;
pub use sources::{Image, Source, SourceId, list_sources};
