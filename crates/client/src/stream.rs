//! Трансляция окна/экрана: пресеты качества, приёмник кадров для LiveKit, публикация трека.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use capture_win::hw::{EncodedSink, HwCapture};
use capture_win::{AppAudio, AudioSink, AudioTarget, ScreenAudio, Capture, CaptureConfig, Event, I420Buffer, Sink, SourceId, app_audio};
use livekit::options::{AudioEncoding, TrackPublishOptions, VideoCodec, VideoEncoderBackend, VideoEncoding};
use livekit::webrtc::audio_frame::AudioFrame;
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::audio_source::{AudioSourceOptions, RtcAudioSource};
use livekit::prelude::*;
use livekit::webrtc::video_frame::{
    EncodedFrameType, EncodedVideoCodec, EncodedVideoFrame, VideoBuffer, VideoFrame, VideoRotation,
};
use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::webrtc::video_source::{RtcVideoSource, VideoResolution};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Preset {
    P720_30,
    #[default]
    P1080_30,
    P1080_60,
    /// 2K (2560×1440), 60 к/с.
    P1440_60,
    /// Исходное разрешение, 60 к/с.
    Native,
}

impl Preset {
    pub const ALL: [Preset; 5] = [Preset::P720_30, Preset::P1080_30, Preset::P1080_60, Preset::P1440_60, Preset::Native];

    pub fn label(self) -> &'static str {
        match self {
            Preset::P720_30 => "720p · 30",
            Preset::P1080_30 => "1080p · 30",
            Preset::P1080_60 => "1080p · 60",
            Preset::P1440_60 => "2K (1440p) · 60",
            Preset::Native => "Исходное · 60",
        }
    }

    pub fn config(self) -> CaptureConfig {
        let (max_width, max_height, fps) = match self {
            Preset::P720_30 => (1280, 720, 30),
            Preset::P1080_30 => (1920, 1080, 30),
            Preset::P1080_60 => (1920, 1080, 60),
            Preset::P1440_60 => (2560, 1440, 60),
            Preset::Native => (0, 0, 60),
        };
        CaptureConfig { max_width, max_height, fps }
    }

    /// Экономный слой для зрителей со слабым каналом: только когда основной заметно больше 720p30.
    pub fn has_low(self) -> bool {
        !matches!(self, Preset::P720_30)
    }

    /// Потолок битрейта; реальный подстраивает WebRTC (GCC) под канал. Заданный в настройках («Вручную») заменяет пресетный.
    pub fn max_bitrate(self) -> u64 {
        bitrate_or(self.default_bitrate(), BITRATE_BPS.load(Ordering::Relaxed))
    }

    /// Пресетный потолок («Авто»).
    pub fn default_bitrate(self) -> u64 {
        match self {
            Preset::P720_30 => 3_000_000,
            Preset::P1080_30 => 5_000_000,
            Preset::P1080_60 => 8_000_000,
            Preset::P1440_60 => 12_000_000,
            Preset::Native => 12_000_000,
        }
    }
}

/// Что кодирует трансляцию (настройка «Кодировщик»): Авто — видеокарта, а если не вышло — процессор.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum EncoderPref {
    #[default]
    Auto,
    /// Видеокарта; не вышло — процессор (с записью в лог).
    Gpu,
    /// Только процессор (OpenH264): когда видеокарта занята игрой или глючит.
    Cpu,
}

impl EncoderPref {
    pub const ALL: [(EncoderPref, &'static str); 3] = [(EncoderPref::Auto, "Авто"), (EncoderPref::Gpu, "Видеокарта"), (EncoderPref::Cpu, "Процессор")];
}

/// Границы ручного битрейта, Мбит/с.
pub const BITRATE_MIN_MBPS: f32 = 1.0;
pub const BITRATE_MAX_MBPS: f32 = 30.0;

static ENCODER_PREF: AtomicU8 = AtomicU8::new(0);
/// Ручной потолок битрейта, бит/с; 0 — «Авто».
static BITRATE_BPS: AtomicU64 = AtomicU64::new(0);

/// Применить настройки трансляции (вызывать при старте и при изменении; действуют на следующий запуск трансляции).
pub fn set_prefs(pref: EncoderPref, bitrate_mbps: Option<f32>) {
    ENCODER_PREF.store(pref as u8, Ordering::Relaxed);
    BITRATE_BPS.store(bitrate_mbps.map_or(0, |m| (m.clamp(BITRATE_MIN_MBPS, BITRATE_MAX_MBPS) * 1_000_000.0) as u64), Ordering::Relaxed);
}

/// Ручной битрейт (бит/с, 0 — нет) вместо пресетного.
fn bitrate_or(default: u64, manual: u64) -> u64 {
    if manual == 0 { default } else { manual }
}

/// Имена треков трансляции: основной и экономный слой (свой simulcast: аппаратный энкодер выдаёт один готовый поток).
pub const MAIN_TRACK: &str = "screen";
pub const LOW_TRACK: &str = "screen-low";
/// Экономный слой: 720p30, потолок 1,5 Мбит/с.
pub const LOW_CFG: CaptureConfig = CaptureConfig { max_width: 1280, max_height: 720, fps: 30 };
pub const LOW_BITRATE: u64 = 1_500_000;

/// Событие захвата с номером поколения (события старого захвата после смены окна игнорируются).
#[derive(Debug)]
pub struct CaptureEvent {
    pub generation: u64,
    pub event: Event,
}

/// Ссылка на буфер как `AsRef<dyn VideoBuffer>` — без копирования кадра.
struct BufRef<'a>(&'a I420Buffer);

impl AsRef<dyn VideoBuffer> for BufRef<'_> {
    fn as_ref(&self) -> &(dyn VideoBuffer + 'static) {
        self.0
    }
}

/// Приёмник кадров захвата → NativeVideoSource (дальше кодирует WebRTC).
pub struct LkSink {
    pub source: NativeVideoSource,
    pub generation: u64,
    pub events: mpsc::UnboundedSender<CaptureEvent>,
}

impl Sink for LkSink {
    fn frame(&self, frame: &I420Buffer, timestamp_us: i64) {
        let f = VideoFrame { rotation: VideoRotation::VideoRotation0, timestamp_us, frame_metadata: None, buffer: BufRef(frame) };
        self.source.capture_frame(&f);
    }

    fn event(&self, event: Event) {
        let _ = self.events.send(CaptureEvent { generation: self.generation, event });
    }
}

/// Источник видео под размер окна/экрана (подсказка энкодеру; реальные кадры могут отличаться).
pub fn new_source(width: u32, height: u32, preset: Preset) -> NativeVideoSource {
    let cfg = preset.config();
    let (w, h) = capture_win::capture::target_size(width.max(2), height.max(2), &cfg);
    NativeVideoSource::new(
        VideoResolution { width: w, height: h },
        true,
    )
}

pub async fn publish(room: &Room, source: &NativeVideoSource, preset: Preset, hw: bool) -> RoomResult<LocalTrackPublication> {
    let track = LocalVideoTrack::create_video_track(MAIN_TRACK, RtcVideoSource::Native(source.clone()));
    let opts = TrackPublishOptions {
        source: TrackSource::Screenshare,
        video_codec: VideoCodec::H264,
        simulcast: false,
        // Аппаратный путь: H.264 уже закодирован видеокартой, WebRTC только пакетирует.
        video_encoder: if hw { VideoEncoderBackend::PreEncoded } else { VideoEncoderBackend::Auto },
        video_encoding: Some(VideoEncoding { max_bitrate: preset.max_bitrate(), max_framerate: preset.config().fps as f64 }),
        ..Default::default()
    };
    room.local_participant().publish_track(LocalTrack::Video(track), opts).await
}

/// Экономный слой (только аппаратный путь, уже закодирован).
pub async fn publish_low(room: &Room, source: &NativeVideoSource) -> RoomResult<LocalTrackPublication> {
    let track = LocalVideoTrack::create_video_track(LOW_TRACK, RtcVideoSource::Native(source.clone()));
    let opts = TrackPublishOptions {
        source: TrackSource::Screenshare,
        video_codec: VideoCodec::H264,
        simulcast: false,
        video_encoder: VideoEncoderBackend::PreEncoded,
        video_encoding: Some(VideoEncoding { max_bitrate: LOW_BITRATE, max_framerate: LOW_CFG.fps as f64 }),
        ..Default::default()
    };
    room.local_participant().publish_track(LocalTrack::Video(track), opts).await
}

/// Источник экономного слоя под размер окна.
pub fn new_low_source(width: u32, height: u32) -> NativeVideoSource {
    let (w, h) = capture_win::capture::target_size(width.max(2), height.max(2), &LOW_CFG);
    NativeVideoSource::new_encoded(VideoResolution { width: w, height: h })
}

pub fn sink(source: &NativeVideoSource, generation: u64, events: &mpsc::UnboundedSender<CaptureEvent>) -> Arc<dyn Sink> {
    Arc::new(LkSink { source: source.clone(), generation, events: events.clone() })
}

/// Приёмник готового H.264 с видеокарты → NativeVideoSource (pre-encoded).
pub struct LkEncodedSink {
    pub source: NativeVideoSource,
    pub generation: u64,
    pub events: mpsc::UnboundedSender<CaptureEvent>,
}

/// Приёмник экономного слоя: события захвата шлёт основной слой — здесь только ошибки в лог.
struct LowSink(NativeVideoSource);

impl EncodedSink for LowSink {
    fn encoded(&self, data: &[u8], keyframe: bool, timestamp_us: i64, width: u32, height: u32) {
        let f = EncodedVideoFrame {
            codec: EncodedVideoCodec::H264,
            payload: data,
            timestamp_us,
            frame_type: if keyframe { EncodedFrameType::Key } else { EncodedFrameType::Delta },
            resolution: VideoResolution { width, height },
            frame_metadata: None,
        };
        self.0.capture_encoded_frame(&f);
    }

    fn take_keyframe_request(&self) -> bool {
        self.0.take_keyframe_request()
    }

    fn take_bitrate_request(&self) -> Option<u32> {
        self.0.take_rate_control_request().map(|r| r.target_bitrate_bps.min(u32::MAX as u64) as u32)
    }

    fn event(&self, event: Event) {
        if let Event::Error(e) = event {
            log::warn!("экономный слой: {e}");
        }
    }
}

impl EncodedSink for LkEncodedSink {
    fn encoded(&self, data: &[u8], keyframe: bool, timestamp_us: i64, width: u32, height: u32) {
        let f = EncodedVideoFrame {
            codec: EncodedVideoCodec::H264,
            payload: data,
            timestamp_us,
            frame_type: if keyframe { EncodedFrameType::Key } else { EncodedFrameType::Delta },
            resolution: VideoResolution { width, height },
            frame_metadata: None,
        };
        self.source.capture_encoded_frame(&f);
    }

    fn take_keyframe_request(&self) -> bool {
        self.source.take_keyframe_request()
    }

    fn take_bitrate_request(&self) -> Option<u32> {
        self.source.take_rate_control_request().map(|r| r.target_bitrate_bps.min(u32::MAX as u64) as u32)
    }

    fn event(&self, event: Event) {
        let _ = self.events.send(CaptureEvent { generation: self.generation, event });
    }
}

/// Идущий захват: аппаратный (видеокарта кодирует сама) или программный (кадры I420 → OpenH264 в WebRTC).
pub enum AnyCapture {
    Cpu(Capture),
    Hw(HwCapture),
}

impl AnyCapture {
    pub fn stop(self) {
        match self {
            AnyCapture::Cpu(c) => c.stop(),
            AnyCapture::Hw(c) => c.stop(),
        }
    }

    /// Остановить, но не дольше `STOP_LIMIT`: остановка WGC/энкодера в редких случаях зависает (у друга 21.11 — на
    /// пересоздании энкодера), а ждём её в потоке голоса — вместе с ней вставали звонок и выход из программы.
    /// Не успела — остаётся доделываться в фоне.
    pub fn stop_bounded(self) {
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new().name("capture-stop".into()).spawn(move || {
            self.stop();
            let _ = tx.send(());
        });
        match spawned {
            Ok(_) => {
                if rx.recv_timeout(STOP_LIMIT).is_err() {
                    log::warn!("остановка захвата не уложилась в {} с — оставляю её в фоне", STOP_LIMIT.as_secs());
                }
            }
            Err(e) => log::warn!("остановка захвата: поток не создан: {e}"),
        }
    }
}

const STOP_LIMIT: std::time::Duration = std::time::Duration::from_secs(3);

pub struct Started {
    pub capture: AnyCapture,
    pub video: NativeVideoSource,
    /// Экономный слой (аппаратный путь, пресет выше 720p30).
    pub low: Option<NativeVideoSource>,
    pub hw: bool,
    /// Для интерфейса: «NVIDIA H.264 Encoder MFT (видеокарта)» / «OpenH264 (процессор)».
    pub encoder: String,
}

/// `MURMUR_NO_HW=1` — принудительно программное кодирование (диагностика).
pub fn hw_allowed() -> bool {
    std::env::var_os("MURMUR_NO_HW").is_none() && ENCODER_PREF.load(Ordering::Relaxed) != EncoderPref::Cpu as u8
}

/// Новый захват с новым источником видео: сначала видеокарта, если нельзя — процессор.
pub fn start(
    id: SourceId,
    width: u32,
    height: u32,
    preset: Preset,
    generation: u64,
    events: &mpsc::UnboundedSender<CaptureEvent>,
) -> Result<Started, String> {
    if hw_allowed() {
        let cfg = preset.config();
        let (w, h) = capture_win::capture::target_size(width.max(2), height.max(2), &cfg);
        let video = NativeVideoSource::new_encoded(VideoResolution { width: w, height: h });
        let low = (preset.has_low() && low_allowed()).then(|| new_low_source(width, height));
        match start_hw(id, preset, &video, low.as_ref(), generation, events, None) {
            Ok(c) => {
                let encoder = format!("{} (видеокарта)", c.encoder_name);
                return Ok(Started { capture: AnyCapture::Hw(c), video, low, hw: true, encoder });
            }
            Err(e) => log::warn!("аппаратное кодирование недоступно: {e} — кодирует процессор"),
        }
    }
    let video = new_source(width, height, preset);
    let c = Capture::start(id, preset.config(), sink(&video, generation, events))?;
    Ok(Started { capture: AnyCapture::Cpu(c), video, low: None, hw: false, encoder: "OpenH264 (процессор)".into() })
}

/// Экономный слой публикуется по умолчанию (зритель выбирает слой сам); `MURMUR_NO_LOW=1` — выключить.
pub fn low_allowed() -> bool {
    !std::env::var("MURMUR_NO_LOW").is_ok_and(|v| v == "1")
}

fn start_hw(
    id: SourceId,
    preset: Preset,
    video: &NativeVideoSource,
    low: Option<&NativeVideoSource>,
    generation: u64,
    events: &mpsc::UnboundedSender<CaptureEvent>,
    content: Option<(u32, u32)>,
) -> Result<HwCapture, String> {
    let sink = Arc::new(LkEncodedSink { source: video.clone(), generation, events: events.clone() });
    let low = low.map(|l| capture_win::hw::LowLayer { sink: Arc::new(LowSink(l.clone())), cfg: LOW_CFG, bitrate: LOW_BITRATE as u32 });
    HwCapture::start_sized(id, preset.config(), preset.max_bitrate() as u32, sink, low, content)
}

/// Смена источника в уже опубликованный трек (тот же вид кодирования).
#[allow(clippy::too_many_arguments)]
pub fn restart(
    id: SourceId,
    preset: Preset,
    video: &NativeVideoSource,
    low: Option<&NativeVideoSource>,
    hw: bool,
    generation: u64,
    events: &mpsc::UnboundedSender<CaptureEvent>,
    content: Option<(u32, u32)>,
) -> Result<AnyCapture, String> {
    if hw {
        start_hw(id, preset, video, low, generation, events, content).map(AnyCapture::Hw)
    } else {
        Capture::start(id, preset.config(), sink(video, generation, events)).map(AnyCapture::Cpu)
    }
}

// ------------------------------------------------------------------ звук трансляции (фаза 5)

/// Битрейт звука трансляции: Opus стерео, музыка/игры без заметных потерь
/// (с RED в сети выходит примерно вдвое больше — ~256 кбит/с).
pub const AUDIO_BITRATE: u64 = 128_000;

/// Источник звука трансляции: 48 кГц стерео, **без** голосовой обработки (AEC/NS/AGC превращают музыку в кашу),
/// без очереди — кадры по 10 мс уходят сразу в кодер (темп держит поток захвата).
pub fn new_audio_source() -> NativeAudioSource {
    NativeAudioSource::new(
        AudioSourceOptions { echo_cancellation: false, noise_suppression: false, auto_gain_control: false },
        app_audio::SAMPLE_RATE,
        app_audio::CHANNELS,
        0,
    )
}

pub async fn publish_audio(room: &Room, source: &NativeAudioSource) -> RoomResult<LocalTrackPublication> {
    let track = LocalAudioTrack::create_audio_track("screen-audio", RtcAudioSource::Native(source.clone()));
    let opts = TrackPublishOptions {
        source: TrackSource::ScreenshareAudio,
        audio_encoding: Some(AudioEncoding { max_bitrate: AUDIO_BITRATE }),
        // DTX выключен: на тихих местах музыки он «рвёт» звук; RED — защита от потерь.
        dtx: false,
        red: true,
        ..Default::default()
    };
    room.local_participant().publish_track(LocalTrack::Audio(track), opts).await
}

/// Кадры захвата → NativeAudioSource (быстрый путь без очереди: capture_frame завершается сразу).
struct LkAudioSink {
    source: NativeAudioSource,
    errors: std::sync::atomic::AtomicU32,
}

impl AudioSink for LkAudioSink {
    fn frame(&self, data: &[i16]) {
        let f = AudioFrame {
            data: std::borrow::Cow::Borrowed(data),
            sample_rate: app_audio::SAMPLE_RATE,
            num_channels: app_audio::CHANNELS,
            samples_per_channel: app_audio::FRAME as u32,
        };
        let fut = std::pin::pin!(self.source.capture_frame(&f));
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        if let std::task::Poll::Ready(Err(e)) = std::future::Future::poll(fut, &mut cx)
            && self.errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 3
        {
            log::warn!("звук трансляции → WebRTC: {e}");
        }
    }
}

/// Программы, чей звук не идёт в трансляцию экрана (меняется на лету: захват перечитывает раз в секунду).
static AUDIO_EXCLUDE: std::sync::RwLock<Vec<String>> = std::sync::RwLock::new(Vec::new());

/// Имя exe без пути, в нижнем регистре, без пустых и повторов.
pub fn normalize_exclude(list: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in list {
        let s = s.trim();
        let name = s.rsplit(['\\', '/']).next().unwrap_or(s).trim().to_lowercase();
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

pub fn set_audio_exclude(list: Vec<String>) {
    *AUDIO_EXCLUDE.write().unwrap_or_else(|e| e.into_inner()) = normalize_exclude(list);
}

pub fn audio_exclude() -> Vec<String> {
    AUDIO_EXCLUDE.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Идущий захват звука трансляции: окно — одно приложение, экран — всё, кроме Murmur и исключённых.
pub enum StreamAudioCapture {
    App(AppAudio),
    Screen(ScreenAudio),
}

impl StreamAudioCapture {
    pub fn stop(self) {
        match self {
            StreamAudioCapture::App(c) => c.stop(),
            StreamAudioCapture::Screen(c) => c.stop(),
        }
    }
}

/// Запуск захвата звука для источника трансляции (окно → его приложение, экран → всё, кроме Murmur и исключённых).
pub fn start_audio(id: SourceId, source: &NativeAudioSource) -> Result<StreamAudioCapture, String> {
    let sink = Arc::new(LkAudioSink { source: source.clone(), errors: Default::default() });
    if id.is_window() {
        let target = AudioTarget::for_source(id).ok_or("не удалось определить процесс окна")?;
        AppAudio::start(target, sink).map(StreamAudioCapture::App)
    } else {
        ScreenAudio::start(Arc::new(audio_exclude), sink).map(StreamAudioCapture::Screen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclude_list_normalized() {
        assert_eq!(normalize_exclude(vec!["Sonar.EXE".into(), "sonar.exe".into(), " ".into(), "C:\\x\\Voice.exe".into()]), vec!["sonar.exe", "voice.exe"]);
    }

    #[test]
    fn manual_bitrate_replaces_preset_cap() {
        assert_eq!(bitrate_or(5_000_000, 0), 5_000_000);
        assert_eq!(bitrate_or(5_000_000, 9_000_000), 9_000_000);
    }

    #[test]
    fn set_prefs_clamps_manual_bitrate_and_gates_hw() {
        set_prefs(EncoderPref::Cpu, Some(100.0));
        assert_eq!(BITRATE_BPS.load(Ordering::Relaxed), 30_000_000);
        assert!(!hw_allowed());
        set_prefs(EncoderPref::Gpu, Some(0.1));
        assert_eq!(BITRATE_BPS.load(Ordering::Relaxed), 1_000_000);
        set_prefs(EncoderPref::Auto, None);
        assert_eq!(BITRATE_BPS.load(Ordering::Relaxed), 0);
        assert_eq!(Preset::P1080_30.max_bitrate(), 5_000_000);
    }
}
