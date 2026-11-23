//! Голосовой движок: LiveKit-комната + платформенный ADM (WASAPI, AEC/NS/AGC из WebRTC).
//!
//! Работает в tokio-задаче. UI шлёт команды (`Cmd`) и читает общий снимок состояния
//! (`VoiceState`) — UI-поток никогда не ждёт сеть или звук.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use capture_win::{Event as CapEvent, SourceId};
use livekit::options::{AudioEncoding, TrackPublishOptions};
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::video_source::native::NativeVideoSource;
use livekit::prelude::*;
use livekit::webrtc::stats::{OutboundRtpStats, RtcStats};
use livekit::{PlatformAudio, PlayoutDeviceId, RecordingDeviceId};
use proto::Channel;
use tokio::sync::mpsc;

use crate::api::Api;
use crate::config::NoiseMode;
use crate::layers::{self, AutoLayer, Layer, Quality};
use crate::sounds::{self, Sound};
use crate::stream::{self, CaptureEvent, Preset};
use crate::video::VideoHub;
use crate::remote::engine::{Outgoing, RcEngine, RcGuestView, RcHostView, RcPointerView};
use crate::remote::hooks::HookEvent;
use crate::remote::proto::{InputEv, Level};

/// Задержка отпускания рации: хвост слова не обрезается.
const PTT_RELEASE_DELAY: Duration = Duration::from_millis(180);
const TICK: Duration = Duration::from_millis(80);
const STATS_EVERY: Duration = Duration::from_secs(2);
/// Сколько не судить о качестве приёма после смены слоя (подписка на новый трек).
const LAYER_SETTLE: Duration = Duration::from_secs(6);
/// Служебное сообщение стримера зрителям: почему картинка на паузе («minimized» / «hidden» / «live»).
const STREAM_STATE_TOPIC: &str = "murmur.stream-state";
/// Зритель — стримеру: «1» начал смотреть, «0» перестал (для звука и счётчика зрителей).
const WATCH_TOPIC: &str = "murmur.watch";
/// Пока пауза — повторять (для тех, кто зашёл позже).
const STREAM_STATE_RESEND: Duration = Duration::from_secs(3);

/// Почему у стримера стоп-кадр (сообщает сам стример).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamPause {
    /// Окно свёрнуто.
    Minimized,
    /// Окно закрыто другими окнами, и программа перестала его рисовать (браузеры так делают).
    Hidden,
}

impl StreamPause {
    fn wire(p: Option<StreamPause>) -> &'static [u8] {
        match p {
            Some(StreamPause::Minimized) => b"minimized",
            Some(StreamPause::Hidden) => b"hidden",
            None => b"live",
        }
    }
}
/// Паузы между попытками вернуться в канал после разрыва.
const REJOIN_BACKOFF: [u64; 6] = [1, 2, 3, 5, 10, 15];

#[derive(Debug, Clone)]
pub enum Cmd {
    Join(Channel),
    Leave,
    SetSelfMuted(bool),
    SetDeafened(bool),
    SetPttEnabled(bool),
    PttHeld(bool),
    /// identity → громкость (1.0 = 100%, 0 = заглушён).
    SetUserVolume(String, f32),
    /// identity → громкость звука его трансляции (1.0 = 100%).
    SetStreamVolume(String, f32),
    /// Общая громкость звука, 1.0 = 100 %.
    SetOutputVolume(f32),
    SetInputDevice(Option<String>),
    SetOutputDevice(Option<String>),
    SetProcessing { aec: bool, noise: NoiseMode, agc: bool },
    SetSounds(bool),
    RefreshDevices,
    /// Качество трансляции участника: Авто / Исходное / Экономное (слой у стримера).
    SetStreamQuality { identity: String, quality: Quality },
    /// Новый API-клиент (после входа/смены сервера).
    SetApi(Api),
    /// Начать трансляцию или сменить источник на лету (тот же трек, без переподключения).
    /// `audio` — передавать звук окна/системы (фаза 5).
    StartStream { source: SourceId, title: String, width: u32, height: u32, preset: Preset, audio: bool },
    StopStream,
    DismissStreamNotice,
    /// Смотреть / перестать смотреть трансляцию участника (подписка на её видео и звук).
    Watch { identity: String, on: bool },
    /// Где видна трансляция: размер на экране (физические пиксели) для SFU; невидимая — на паузе.
    StreamView { identity: String, visible: bool, width: u32, height: u32 },
    /// Отладочный оверлей F3 открыт/закрыт: открыт — метрики раз в 1 с в `VoiceState.metrics`.
    Metrics(bool),
    /// Вся сырая статистика WebRTC — в лог (один раз, на следующем опросе).
    DumpStats,
    /// Зритель: попросить у стримера указку/управление.
    RcRequest { identity: String, level: Level },
    /// Стример: ответ на запрос (None — отказ).
    RcRespond { identity: String, grant: Option<Level> },
    /// Стример: «Забрать» — весь доступ к своей трансляции.
    RcRevokeAll,
    /// Зритель: взять управление / вернуть указку.
    RcTake { identity: String, on: bool },
    /// Зритель: указка над трансляцией (0..1).
    RcPointer { identity: String, x: f32, y: f32 },
    /// Зритель в режиме управления: кнопка мыши/колесо над трансляцией.
    RcInput { identity: String, ev: InputEv },
    /// Окно Murmur в фокусе (клавиатура управляющего уходит другу только в фокусе).
    RcGuestFocus(bool),
    /// Клавиша рации (не пересылается при управлении).
    RcPttKey(u32),
    /// Событие хуков ввода (внутреннее).
    RcHook(HookEvent),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Conn {
    Idle,
    Connecting,
    Connected,
    /// SDK сам восстанавливает соединение (ICE/сигналинг).
    Reconnecting,
    /// Комната потеряна, заходим заново с новым токеном.
    Rejoining { attempt: usize },
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct Peer {
    pub identity: String,
    pub name: String,
    pub is_local: bool,
    pub speaking: bool,
    pub level: f32,
    pub mic_muted: bool,
    /// Транслирует экран/окно.
    pub streaming: bool,
}

#[derive(Debug, Clone)]
pub struct Device {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct VoiceState {
    pub conn: Conn,
    pub channel: Option<Channel>,
    pub peers: Vec<Peer>,
    /// RTT до LiveKit, секунды.
    pub rtt: Option<f64>,
    /// Джиттер аудио (наибольший из входящего и того, что сервер видит от нас), секунды.
    pub jitter: Option<f64>,
    /// Доля потерянных пакетов аудио 0..1 за последний замер.
    pub loss: Option<f64>,
    /// Сглаженное состояние связи для значка пинга (короткие потери — жёлтым, долгие — красным).
    pub link: crate::link::LinkState,
    pub self_muted: bool,
    pub deafened: bool,
    pub ptt_enabled: bool,
    pub ptt_held: bool,
    /// Микрофон реально передаётся.
    pub transmitting: bool,
    pub inputs: Vec<Device>,
    pub outputs: Vec<Device>,
    pub audio_error: Option<String>,
    pub stream: Option<StreamInfo>,
    /// Почему трансляция остановилась сама (окно закрыто, ошибка).
    pub stream_notice: Option<String>,
    /// Чьи трансляции мы смотрим (identity, по алфавиту).
    pub watching: Vec<String>,
    /// Метрики для оверлея F3 (только пока он открыт и мы в канале).
    pub metrics: Option<crate::metrics::Metrics>,
    /// Стримеры, у которых картинка на паузе, и почему (identity → причина).
    pub stream_paused: HashMap<String, StreamPause>,
    /// Слои трансляций, которые смотрим (identity → выбор, текущий слой).
    pub stream_layers: HashMap<String, LayerInfo>,
    /// Управление своей трансляцией: запросы, кто управляет.
    pub rc_host: RcHostView,
    /// Наш доступ к чужим трансляциям (стример → состояние).
    pub rc_guest: HashMap<String, RcGuestView>,
    /// Указки зрителей поверх чужих трансляций (стример → указки).
    pub rc_pointers: HashMap<String, Vec<RcPointerView>>,
    /// Уведомление об управлении («Друг отказал», «кликни в окно»).
    pub rc_notice: Option<String>,
}

/// Качество просматриваемой трансляции (для интерфейса).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LayerInfo {
    pub quality: Quality,
    /// Стример публикует экономный слой (иначе выбирать нечего).
    pub has_low: bool,
    /// Сейчас показываем экономный слой.
    pub low: bool,
}

#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub source: SourceId,
    pub title: String,
    pub preset: Preset,
    pub minimized: bool,
    /// Окно перекрыто другими и не присылает кадров — у зрителей стоп-кадр.
    pub hidden: bool,
    /// Сколько человек сейчас смотрят.
    pub viewers: usize,
    /// Размер отправляемого видео.
    pub size: Option<(u32, u32)>,
    /// Чем кодируется: «NVIDIA H.264 Encoder MFT (видеокарта)» / «OpenH264 (процессор)».
    pub backend: String,
    /// Из статистики WebRTC: реализация энкодера, к/с, кбит/с, ограничение качества.
    pub encoder: Option<String>,
    pub fps: f64,
    pub kbps: f64,
    pub limitation: Option<String>,
    pub audio: AudioStatus,
}

/// Звук трансляции (для интерфейса).
#[derive(Debug, Clone, PartialEq)]
pub enum AudioStatus {
    Off,
    /// Захватывается: «звук приложения» / «весь звук, кроме Amadeus».
    On(&'static str),
    Failed(String),
}

impl Default for VoiceState {
    fn default() -> Self {
        Self {
            conn: Conn::Idle,
            channel: None,
            peers: Vec::new(),
            rtt: None,
            jitter: None,
            loss: None,
            link: crate::link::LinkState::Fine,
            self_muted: false,
            deafened: false,
            ptt_enabled: false,
            ptt_held: false,
            transmitting: false,
            inputs: Vec::new(),
            outputs: Vec::new(),
            audio_error: None,
            stream: None,
            stream_notice: None,
            watching: Vec::new(),
            metrics: None,
            stream_paused: HashMap::new(),
            stream_layers: HashMap::new(),
            rc_host: RcHostView::default(),
            rc_guest: HashMap::new(),
            rc_pointers: HashMap::new(),
            rc_notice: None,
        }
    }
}

pub type Shared = Arc<Mutex<VoiceState>>;

#[derive(Clone)]
pub struct VoiceHandle {
    tx: mpsc::UnboundedSender<Cmd>,
    pub state: Shared,
}

impl VoiceHandle {
    pub fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }

    pub fn sender(&self) -> mpsc::UnboundedSender<Cmd> {
        self.tx.clone()
    }

    pub fn snapshot(&self) -> VoiceState {
        self.state.lock().unwrap().clone()
    }
}

/// Начальные параметры звука (из настроек).
#[derive(Debug, Clone)]
pub struct AudioPrefs {
    pub input: Option<String>,
    pub output: Option<String>,
    pub aec: bool,
    pub noise: NoiseMode,
    pub agc: bool,
    pub ptt_enabled: bool,
    pub sounds: bool,
    pub volumes: HashMap<String, f32>,
    /// Громкость звука трансляций по identity.
    pub stream_volumes: HashMap<String, f32>,
    /// Общая громкость звука (множитель поверх громкости участников), 1.0 = 100 %.
    pub output_volume: f32,
}

pub fn spawn(
    rt: &tokio::runtime::Handle,
    api: Api,
    prefs: AudioPrefs,
    video: VideoHub,
    repaint: Arc<dyn Fn() + Send + Sync>,
) -> VoiceHandle {
    crate::denoise::install();
    let (tx, rx) = mpsc::unbounded_channel();
    let (cap_tx, cap_rx) = mpsc::unbounded_channel();
    let state: Shared = Arc::new(Mutex::new(VoiceState { ptt_enabled: prefs.ptt_enabled, ..Default::default() }));
    // Хуки ввода живут в своём потоке — их события приходят командами в тот же движок.
    let (hook_tx, mut hook_rx) = mpsc::unbounded_channel::<HookEvent>();
    let fwd = tx.clone();
    rt.spawn(async move {
        while let Some(ev) = hook_rx.recv().await {
            if fwd.send(Cmd::RcHook(ev)).is_err() {
                break;
            }
        }
    });
    // Сообщения управления — одной очередью по порядку (иначе «отпустить» обгоняет «нажать», и стример отбрасывает ввод).
    let (rc_tx, mut rc_rx) = mpsc::unbounded_channel::<(LocalParticipant, livekit::DataPacket)>();
    rt.spawn(async move {
        while let Some((lp, packet)) = rc_rx.recv().await {
            if let Ok(Err(e)) = tokio::time::timeout(IO_TIMEOUT, lp.publish_data(packet)).await {
                log::warn!("управление: отправка: {e}");
            }
        }
    });
    let engine = Engine {
        rc_tx,
        api,
        state: state.clone(),
        repaint,
        audio: None,
        room: None,
        mic: None,
        prefs,
        self_muted: false,
        deafened: false,
        ptt_held: false,
        ptt_release_at: None,
        rejoin: None,
        last_stats: Instant::now() - STATS_EVERY,
        last_peers: HashMap::new(),
        stream: None,
        cap_tx,
        generation: 0,
        last_video_bytes: None,
        video_sent_by_ssrc: HashMap::new(),
        stats_n: 0,
        audio_counts: (0, 0),
        link: Default::default(),
        video,
        watching: HashSet::new(),
        receivers: HashMap::new(),
        recv: HashMap::new(),
        quality: HashMap::new(),
        auto: HashMap::new(),
        recv_prev: HashMap::new(),
        views: HashMap::new(),
        overlay: false,
        counters: Default::default(),
        metrics_at: None,
        dump_stats: false,
        force_relay: std::env::var("AMADEUS_FORCE_RELAY").is_ok_and(|v| v == "1"),
        pause_sent: None,
        viewers: HashSet::new(),
        rc: RcEngine::new(hook_tx),
        rx: None,
        deferred: Default::default(),
    };
    rt.spawn(engine.run(rx, cap_rx));
    VoiceHandle { tx, state }
}

struct Rejoin {
    channel: Channel,
    attempt: usize,
    at: Instant,
}

struct Engine {
    api: Api,
    state: Shared,
    repaint: Arc<dyn Fn() + Send + Sync>,
    audio: Option<PlatformAudio>,
    room: Option<(Room, mpsc::UnboundedReceiver<RoomEvent>)>,
    mic: Option<LocalTrackPublication>,
    prefs: AudioPrefs,
    self_muted: bool,
    deafened: bool,
    ptt_held: bool,
    ptt_release_at: Option<Instant>,
    rejoin: Option<Rejoin>,
    last_stats: Instant,
    /// identity → имя: для звуков входа/выхода и сравнения состава.
    last_peers: HashMap<String, String>,
    stream: Option<Streaming>,
    cap_tx: mpsc::UnboundedSender<CaptureEvent>,
    /// Поколение захвата: растёт при каждом старте/смене источника.
    generation: u64,
    last_video_bytes: Option<(u64, Instant)>,
    /// Байты, отправленные каждым исходящим видеопотоком (ssrc) на прошлом замере — чтобы узнать живой.
    video_sent_by_ssrc: HashMap<u32, u64>,
    stats_n: u64,
    /// Входящее аудио (потеряно, принято) на прошлом замере — для доли потерь.
    audio_counts: (i64, u64),
    /// Сглаживание значка пинга.
    link: crate::link::LinkMeter,
    /// Текстуры просматриваемых трансляций (общие с UI).
    video: VideoHub,
    /// Чьи трансляции смотрим; переживает переподключение, сбрасывается при выходе/смене канала.
    watching: HashSet<String>,
    /// Задачи приёма кадров по identity.
    receivers: HashMap<String, tokio::task::JoinHandle<()>>,
    /// Что принимает приёмник: identity → (sid публикации, id rtc-трека, слой, с какого момента).
    recv: HashMap<String, (TrackSid, String, Layer, Instant)>,
    /// Выбор качества по стримерам (переживает переподключение и выход из канала).
    quality: HashMap<String, Quality>,
    /// Авто-выбор слоя по стримерам.
    auto: HashMap<String, AutoLayer>,
    /// Счётчики входящего видео на прошлом замере: id rtc-трека → (потеряно, принято, заморозок).
    recv_prev: HashMap<String, (i64, u64, u32)>,
    /// Последний вид из UI: (видна, ширина, высота) — повторно применяется после переподписки.
    views: HashMap<String, (bool, u32, u32)>,
    /// Оверлей F3 открыт.
    overlay: bool,
    counters: crate::metrics::Counters,
    /// Время прошлого опроса для метрик (интервал для скоростей).
    metrics_at: Option<Instant>,
    dump_stats: bool,
    /// `AMADEUS_FORCE_RELAY=1`: только через TURN (проверка запасного пути).
    force_relay: bool,
    /// Очередь команд: живёт в движке, чтобы вход в канал мог принимать «Отключиться» (см. `join`).
    rx: Option<mpsc::UnboundedReceiver<Cmd>>,
    /// Команды, принятые во время входа, но ещё не выполненные.
    deferred: std::collections::VecDeque<Cmd>,
    /// Что последним сообщили зрителям о паузе своей трансляции и когда.
    pause_sent: Option<(Option<StreamPause>, Instant)>,
    /// Кто смотрит свою трансляцию (по сообщениям зрителей).
    viewers: HashSet<String>,
    /// Указка и управление трансляцией.
    rc: RcEngine,
    /// Очередь отправки сообщений управления (по порядку).
    rc_tx: mpsc::UnboundedSender<(LocalParticipant, livekit::DataPacket)>,
}

/// Идущая трансляция. Захват и источник живут дольше публикации: при переподключении
/// трек публикуется заново в новую комнату из того же источника.
struct Streaming {
    capture: Option<stream::AnyCapture>,
    source: NativeVideoSource,
    preset: Preset,
    /// Аппаратный путь (pre-encoded H.264).
    hw: bool,
    /// Что транслируем (для пересоздания захвата при смене размера окна).
    source_id: SourceId,
    publication: Option<LocalTrackPublication>,
    /// Экономный слой: источник и его публикация (None — слоя нет).
    low: Option<(NativeVideoSource, Option<LocalTrackPublication>)>,
    /// Звук окна/системы (фаза 5); None — не передаётся.
    audio: Option<StreamSound>,
}

/// Звук трансляции: источник живёт дольше публикации (переподключение), захват меняется со сменой окна.
struct StreamSound {
    capture: Option<capture_win::AppAudio>,
    source: NativeAudioSource,
    publication: Option<LocalTrackPublication>,
}

/// Понятный текст ошибки входа в канал (подробности — в логе).
fn humanize_join_error(e: &anyhow::Error) -> String {
    if let Some(api) = e.downcast_ref::<crate::api::ApiError>() {
        return format!("Не удалось войти в канал: {}", api.message);
    }
    let raw = format!("{e:#}");
    let low = raw.to_lowercase();
    let what = if low.contains("401") || low.contains("invalid api key") || low.contains("unauthorized") {
        "сервер голоса отклонил доступ (ключи LiveKit на бэкенде не совпадают с сервером голоса)"
    } else if low.contains("timeout") || low.contains("timed out") {
        "сервер голоса не отвечает — проверьте интернет"
    } else if low.contains("dns") || low.contains("resolve") || low.contains("connect") {
        "нет связи с сервером голоса"
    } else {
        "ошибка подключения к серверу голоса"
    };
    format!("Не удалось войти в канал: {what}.")
}

async fn next_event(room: &mut Option<(Room, mpsc::UnboundedReceiver<RoomEvent>)>) -> Option<RoomEvent> {
    match room {
        Some((_, rx)) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Ждёт команду; без очереди (ещё не запущен движок) — вечно.
async fn recv_cmd(rx: &mut Option<mpsc::UnboundedReceiver<Cmd>>) -> Option<Cmd> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

/// Снятие трека, отправка данных, статистика: дольше не ждём (обрыв сети иначе вешает «Стоп»/«Отключиться»).
const IO_TIMEOUT: Duration = Duration::from_secs(4);

/// Дольше этого не ждём ответа LiveKit при входе.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

impl Engine {
    async fn run(mut self, rx: mpsc::UnboundedReceiver<Cmd>, mut cap_rx: mpsc::UnboundedReceiver<CaptureEvent>) {
        self.rx = Some(rx);
        self.init_audio();
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            // команды, пришедшие во время входа в канал (см. join), — по порядку, раньше новых
            if let Some(cmd) = self.deferred.pop_front() {
                self.on_cmd(cmd).await;
                continue;
            }
            tokio::select! {
                cmd = recv_cmd(&mut self.rx) => match cmd {
                    Some(cmd) => self.on_cmd(cmd).await,
                    None => break,
                },
                ev = next_event(&mut self.room) => match ev {
                    Some(ev) => self.on_room_event(ev).await,
                    None => {
                        // Канал событий закрыт — комната уничтожена.
                        self.drop_room();
                        self.lost_connection("соединение закрыто");
                    }
                },
                Some(ev) = cap_rx.recv() => self.on_capture_event(ev).await,
                _ = tick.tick() => self.on_tick().await,
            }
        }
        self.stop_stream().await;
        self.leave().await;
    }

    fn set(&self, f: impl FnOnce(&mut VoiceState)) {
        f(&mut self.state.lock().unwrap());
        (self.repaint)();
    }

    // ---------------------------------------------------------------- звук

    fn init_audio(&mut self) {
        match PlatformAudio::new() {
            Ok(audio) => {
                self.audio = Some(audio);
                self.apply_processing();
                // None = системное по умолчанию: ADM и так стартует на нём, переключать не нужно.
                if let Some(inp) = self.prefs.input.clone() {
                    self.apply_input(Some(inp));
                }
                if let Some(out) = self.prefs.output.clone() {
                    self.apply_output(Some(out));
                }
                self.refresh_devices();
            }
            Err(e) => {
                log::error!("PlatformAudio: {e}");
                self.set(|s| s.audio_error = Some(format!("Звуковые устройства недоступны: {e}")));
            }
        }
    }

    /// Список устройств — в отдельном потоке: вызовы ADM ждут поток WebRTC и могут зависнуть, пока драйвер занят
    /// (так вставал весь движок, а с ним и выход из программы).
    fn refresh_devices(&self) {
        static BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let Some(audio) = self.audio.clone() else { return };
        if BUSY.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return; // предыдущее перечисление ещё не вернулось
        }
        let (state, repaint) = (self.state.clone(), self.repaint.clone());
        let spawned = std::thread::Builder::new().name("amadeus-devices".into()).spawn(move || {
            let t0 = Instant::now();
            let inputs: Vec<Device> =
                audio.recording_devices().map(|d| Device { id: d.id.as_str().to_string(), name: d.name }).collect();
            let outputs: Vec<Device> =
                audio.playout_devices().map(|d| Device { id: d.id.as_str().to_string(), name: d.name }).collect();
            log::info!("устройства: {} входов, {} выходов за {} мс", inputs.len(), outputs.len(), t0.elapsed().as_millis());
            {
                let mut s = state.lock().unwrap();
                s.inputs = inputs;
                s.outputs = outputs;
            }
            repaint();
            BUSY.store(false, std::sync::atomic::Ordering::SeqCst);
        });
        if spawned.is_err() {
            BUSY.store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// AudioOptions микрофонного трека WebRTC (патч vendor/webrtc-sys). Фильтр низких частот (<80 Гц, гул) — всегда.
    fn device_audio_flags(&self) -> i32 {
        use webrtc_sys::amadeus as a;
        let mut f = a::HPF;
        if self.prefs.aec {
            f |= a::EC;
        }
        if self.prefs.agc {
            f |= a::AGC;
        }
        if self.prefs.noise == NoiseMode::Standard {
            f |= a::NS;
        }
        f
    }

    /// Обработка микрофона. В SDK микрофонный трек создавался всегда с AEC+NS+AGC WebRTC, а
    /// `configure_audio_processing` переключает только аппаратную обработку, — поэтому флаги ставим патчем.
    fn apply_processing(&self) {
        let flags = self.device_audio_flags();
        webrtc_sys::amadeus::set_device_audio_flags(flags);
        crate::denoise::set_mode(self.prefs.noise);
        log::info!("обработка микрофона: AEC={} AGC={} шумодав={:?}", self.prefs.aec, self.prefs.agc, self.prefs.noise);
    }

    /// Публикует микрофон в комнату (128 кбит/с, без DTX, с RED).
    async fn publish_mic(&mut self, lp: &LocalParticipant) -> Option<LocalTrackPublication> {
        let audio = self.audio.as_ref()?;
        let track = LocalAudioTrack::create_audio_track("microphone", audio.rtc_source());
        let publish = TrackPublishOptions {
            source: TrackSource::Microphone,
            // 128 кбит/с: максимум для голоса (Opus, фуллбэнд 48 кГц) — чище и «ярче» 64; трафик всё равно мал.
            // DTX выключен: на паузах он даёт «рацию»/комфортный шум и режет тихие начала слов.
            audio_encoding: Some(AudioEncoding { max_bitrate: 128_000 }),
            dtx: false,
            red: true,
            ..Default::default()
        };
        match lp.publish_track(LocalTrack::Audio(track), publish).await {
            Ok(p) => {
                // Сразу заглушаем, если звук не должен идти: дальше в connect идут долгие await'ы
                // (публикация трансляции), и до apply_mic микрофон был бы в эфире.
                if !self.mic_should_send() {
                    p.mute();
                }
                Some(p)
            }
            Err(e) => {
                log::error!("публикация микрофона: {e}");
                self.set(|s| s.audio_error = Some(format!("Микрофон не опубликован: {e}")));
                None
            }
        }
    }

    /// Новые флаги обработки WebRTC — новый микрофонный трек (старый снимается).
    async fn republish_mic(&mut self) {
        let Some(lp) = self.room.as_ref().map(|(r, _)| r.local_participant()) else { return };
        if let Some(old) = self.mic.take()
            && let Ok(Err(e)) = tokio::time::timeout(IO_TIMEOUT, lp.unpublish_track(&old.sid())).await
        {
            log::warn!("снятие микрофона: {e}");
        }
        self.mic = self.publish_mic(&lp).await;
        log::info!("микрофон перевыпущен с новой обработкой");
        self.apply_mic();
    }

    fn apply_input(&mut self, id: Option<String>) {
        self.prefs.input = id.clone();
        let Some(audio) = &self.audio else { return };
        let res = match &id {
            // switch_* работает и во время записи (горячая смена устройства).
            Some(guid) => audio.switch_recording_device(&RecordingDeviceId::from_unchecked_guid(guid)),
            None => audio.switch_recording_device_by_index(0),
        };
        if let Err(e) = res {
            log::warn!("микрофон {id:?}: {e}");
        }
    }

    fn apply_output(&mut self, id: Option<String>) {
        self.prefs.output = id.clone();
        let Some(audio) = &self.audio else { return };
        let res = match &id {
            Some(guid) => audio.switch_playout_device(&PlayoutDeviceId::from_unchecked_guid(guid)),
            None => audio.switch_playout_device_by_index(0),
        };
        if let Err(e) = res {
            log::warn!("вывод {id:?}: {e}");
        }
    }

    // ---------------------------------------------------------------- команды

    async fn on_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Join(ch) => {
                self.hang_up_dm(Some(&ch));
                self.rejoin = None;
                self.stop_stream().await;
                self.leave().await;
                self.forget_watching();
                self.join(ch, 0).await;
            }
            Cmd::Leave => {
                self.hang_up_dm(None);
                self.rejoin = None;
                let was_in = self.room.is_some();
                self.stop_stream().await;
                self.leave().await;
                self.forget_watching();
                if was_in {
                    self.play(Sound::LeaveSelf);
                }
                self.set(|s| {
                    s.conn = Conn::Idle;
                    s.channel = None;
                    s.peers.clear();
                    s.rtt = None;
                    s.jitter = None;
                    s.loss = None;
            s.link = crate::link::LinkState::Fine;
                });
            }
            Cmd::SetSelfMuted(m) => {
                self.self_muted = m;
                if !m && self.deafened {
                    // Включить микрофон в «наушниках выкл» = снять и глушение звука (как в Discord).
                    self.deafened = false;
                    self.apply_volumes();
                }
                self.apply_mic();
            }
            Cmd::SetDeafened(d) => {
                self.deafened = d;
                self.apply_volumes();
                self.apply_mic();
            }
            Cmd::SetPttEnabled(on) => {
                self.prefs.ptt_enabled = on;
                self.ptt_held = false;
                self.ptt_release_at = None;
                self.set(|s| s.ptt_enabled = on);
                self.apply_mic();
            }
            Cmd::PttHeld(held) => {
                if held {
                    self.ptt_release_at = None;
                    if !self.ptt_held {
                        self.ptt_held = true;
                        self.apply_mic();
                    }
                } else if self.ptt_held {
                    self.ptt_release_at = Some(Instant::now() + PTT_RELEASE_DELAY);
                }
            }
            Cmd::SetUserVolume(id, v) => {
                self.prefs.volumes.insert(id.clone(), v);
                self.apply_volume_for(&id);
            }
            Cmd::SetStreamVolume(id, v) => {
                self.prefs.stream_volumes.insert(id.clone(), v);
                self.apply_volume_for(&id);
            }
            Cmd::SetOutputVolume(v) => {
                self.prefs.output_volume = v;
                self.apply_volumes();
            }
            Cmd::SetInputDevice(id) => self.apply_input(id),
            Cmd::SetOutputDevice(id) => self.apply_output(id),
            Cmd::SetProcessing { aec, noise, agc } => {
                let before = self.device_audio_flags();
                self.prefs.aec = aec;
                self.prefs.noise = noise;
                self.prefs.agc = agc;
                self.apply_processing();
                // Флаги WebRTC действуют на новый трек: в канале — перевыпустить микрофон.
                if self.device_audio_flags() != before && self.mic.is_some() {
                    self.republish_mic().await;
                }
            }
            Cmd::SetSounds(on) => self.prefs.sounds = on,
            Cmd::RefreshDevices => self.refresh_devices(),
            Cmd::SetApi(api) => self.api = api,
            Cmd::StartStream { source, title, width, height, preset, audio } => {
                self.start_stream(source, title, width, height, preset, audio).await
            }
            Cmd::StopStream => self.stop_stream().await,
            Cmd::DismissStreamNotice => self.set(|s| s.stream_notice = None),
            Cmd::Metrics(on) => {
                self.overlay = on;
                self.counters = Default::default();
                self.metrics_at = None;
                // Сразу опросить на следующем тике.
                self.last_stats = Instant::now() - STATS_EVERY;
                if !on {
                    self.set(|s| s.metrics = None);
                }
            }
            Cmd::DumpStats => self.dump_stats = true,
            Cmd::RcRequest { identity, level } => {
                let o = self.rc.request(&identity, level);
                self.rc_out(o);
            }
            Cmd::RcRespond { identity, grant } => {
                let o = self.rc.respond(&identity, grant, &self.viewers);
                self.rc_out(o);
            }
            Cmd::RcRevokeAll => {
                let o = self.rc.revoke_all(&self.viewers);
                self.rc_out(o);
            }
            Cmd::RcTake { identity, on } => {
                let o = self.rc.take(&identity, on);
                self.rc_out(o);
            }
            Cmd::RcPointer { identity, x, y } => {
                let o = self.rc.pointer(&identity, x, y);
                self.rc_out(o);
            }
            Cmd::RcInput { identity, ev } => {
                let o = self.rc.input(&identity, ev);
                self.rc_out(o);
            }
            Cmd::RcGuestFocus(f) => {
                let o = self.rc.set_focus(f);
                self.rc_out(o);
            }
            Cmd::RcPttKey(vk) => self.rc.set_ptt_key(vk),
            Cmd::RcHook(ev) => {
                let o = self.rc.hook(ev, &self.viewers);
                self.rc_out(o);
            }
            Cmd::Watch { identity, on } => self.set_watching(&identity, on),
            Cmd::SetStreamQuality { identity, quality } => {
                log::info!("трансляция {identity}: качество {}", quality.label());
                self.quality.insert(identity.clone(), quality);
                // «Авто» начинает с исходного и заново копит замеры.
                self.auto.remove(&identity);
                self.sync_subscriptions();
                self.publish_layers();
            }
            Cmd::StreamView { identity, visible, width, height } => {
                self.views.insert(identity.clone(), (visible, width, height));
                self.apply_view(&identity);
            }
        }
    }

    fn play(&self, s: Sound) {
        if self.prefs.sounds {
            sounds::play(s);
        }
    }

    // ---------------------------------------------------------------- комната

    async fn join(&mut self, ch: Channel, attempt: usize) {
        self.set(|s| {
            s.conn = if attempt == 0 { Conn::Connecting } else { Conn::Rejoining { attempt } };
            s.channel = Some(ch.clone());
            s.peers.clear();
            s.rtt = None;
            s.jitter = None;
            s.loss = None;
            s.link = crate::link::LinkState::Fine;
        });
        self.audio_counts = (0, 0);
        self.link.reset();
        // Подключение отменяемо: «Отключиться»/другой канал во время входа не ждут ответа сети.
        let connect = Self::connect_room(self.api.clone(), ch.clone(), self.force_relay);
        tokio::pin!(connect);
        let connected = loop {
            tokio::select! {
                r = &mut connect => break r,
                cmd = recv_cmd(&mut self.rx) => match cmd {
                    Some(Cmd::SetSelfMuted(m)) => {
                        self.self_muted = m;
                        if !m && self.deafened {
                            self.deafened = false;
                            self.apply_volumes();
                        }
                        self.apply_mic();
                    }
                    Some(Cmd::SetDeafened(d)) => {
                        self.deafened = d;
                        self.apply_volumes();
                        self.apply_mic();
                    }
                    Some(cmd @ (Cmd::Join(_) | Cmd::Leave)) => {
                        log::info!("вход в канал {} отменён новой командой", ch.id);
                        self.deferred.push_back(cmd);
                        return;
                    }
                    Some(cmd) => self.deferred.push_back(cmd),
                    None => return,
                },
            }
        };
        let joined = match connected {
            Ok((room, events)) => self.try_join(room, events).await,
            Err(e) => Err(e),
        };
        match joined {
            Ok(()) => {
                self.rejoin = None;
                if attempt == 0 {
                    self.play(Sound::JoinSelf);
                }
                self.set(|s| s.conn = Conn::Connected);
                self.refresh_peers(false);
            }
            Err(e) => {
                log::warn!("вход в канал {} (попытка {attempt}): {e:#}", ch.id);
                self.drop_room();
                // нет прав / канала больше нет — повторять бессмысленно
                let fatal = e
                    .downcast_ref::<crate::api::ApiError>()
                    .is_some_and(|a| a.is_unauthorized() || a.code == "forbidden" || a.code == "not_found");
                if attempt == 0 || fatal {
                    let msg = humanize_join_error(&e);
                    self.set(|s| {
                        s.conn = Conn::Failed(msg);
                        s.peers.clear();
                    });
                } else {
                    self.schedule_rejoin(ch, attempt);
                }
            }
        }
    }

    /// Уходим из звонка в ЛС (выход или переход в другой канал) — сказать серверу сразу: отменить/отклонить звонок,
    /// не дожидаясь вебхука LiveKit.
    fn hang_up_dm(&self, next: Option<&Channel>) {
        let cur = self.state.lock().unwrap().channel.clone();
        if let Some(cur) = cur
            && crate::dms::is_dm(&cur)
            && next.is_none_or(|n| n.id != cur.id)
        {
            let api = self.api.clone();
            tokio::spawn(async move {
                if let Err(e) = api.dm_hang_up(cur.id).await {
                    log::warn!("звонок {}: не удалось положить трубку: {e}", cur.id);
                }
            });
        }
    }

    /// Подключение к комнате (HTTP за токеном + LiveKit). Не трогает движок — его можно отменить, бросив future.
    async fn connect_room(api: crate::api::Api, ch: Channel, force_relay: bool) -> anyhow::Result<(Room, mpsc::UnboundedReceiver<livekit::RoomEvent>)> {
        let join = if crate::dms::is_dm(&ch) { api.dm_call(ch.id).await? } else { api.join(ch.id).await? };
        let mut opts = RoomOptions::default();
        // Подписки — вручную: голос всегда, трансляции (видео + звук) — только по «Смотреть» (канал зрителя не тратится).
        opts.auto_subscribe = false;
        opts.adaptive_stream = true;
        opts.dynacast = true;
        if force_relay {
            log::warn!("AMADEUS_FORCE_RELAY=1: подключение только через TURN");
            opts.rtc_config.ice_transport_type = livekit::webrtc::prelude::IceTransportsType::Relay;
        }
        let connect = Room::connect(&join.livekit_url, &join.livekit_token, opts);
        match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
            Ok(r) => Ok(r?),
            Err(_) => anyhow::bail!("сервер голоса не отвечает"),
        }
    }

    async fn try_join(&mut self, room: Room, events: mpsc::UnboundedReceiver<livekit::RoomEvent>) -> anyhow::Result<()> {
        if self.audio.is_none() {
            self.init_audio();
        } else {
            // Новая комната после выхода — новый движок голоса WebRTC, а он при создании ставит системные
            // устройства (adm_helpers::Init). Выбранные в настройках — заново.
            if let Some(inp) = self.prefs.input.clone() {
                self.apply_input(Some(inp));
            }
            if let Some(out) = self.prefs.output.clone() {
                self.apply_output(Some(out));
            }
        }
        self.apply_processing();
        let mic = self.publish_mic(&room.local_participant()).await;
        // Переподключение во время трансляции: тот же источник — в новую комнату.
        if let Some(st) = &mut self.stream {
            match stream::publish(&room, &st.source, st.preset, st.hw).await {
                Ok(p) => st.publication = Some(p),
                Err(e) => log::error!("повторная публикация трансляции: {e}"),
            }
            if let Some((src, p)) = &mut st.low {
                match stream::publish_low(&room, src).await {
                    Ok(np) => *p = Some(np),
                    Err(e) => log::warn!("повторная публикация экономного слоя: {e}"),
                }
            }
            if let Some(a) = &mut st.audio {
                match stream::publish_audio(&room, &a.source).await {
                    Ok(p) => a.publication = Some(p),
                    Err(e) => log::error!("повторная публикация звука трансляции: {e}"),
                }
            }
        }
        self.room = Some((room, events));
        self.sync_subscriptions();
        self.mic = mic;
        self.last_peers.clear();
        self.apply_mic();
        self.apply_volumes();
        Ok(())
    }

    fn drop_room(&mut self) {
        // кик, бан, обрыв: управление сбросить (зажатое отпустить, хуки снять)
        self.rc.leave_room();
        self.publish_rc();
        self.stop_receivers();
        self.mic = None;
        if let Some(st) = &mut self.stream {
            st.publication = None;
            if let Some((_, p)) = &mut st.low {
                *p = None;
            }
            if let Some(a) = &mut st.audio {
                a.publication = None;
            }
        }
        self.room = None;
    }

    async fn leave(&mut self) {
        log::info!("выход: начало (комната: {})", self.room.is_some());
        self.stop_receivers();
        log::info!("выход: приёмники видео остановлены");
        self.mic = None;
        log::info!("выход: микрофонный трек отпущен");
        if let Some(st) = &mut self.stream {
            st.publication = None;
            if let Some((_, p)) = &mut st.low {
                *p = None;
            }
            if let Some(a) = &mut st.audio {
                a.publication = None;
            }
        }
        // Закрытие комнаты — в фоне и с таймаутом: если нативная часть зависнет, движок и интерфейс не встанут.
        if let Some((room, events)) = self.room.take() {
            drop(events);
            tokio::spawn(async move {
                match tokio::time::timeout(Duration::from_secs(5), room.close()).await {
                    Ok(Ok(())) => log::info!("выход: комната закрыта"),
                    Ok(Err(e)) => log::warn!("закрытие комнаты: {e}"),
                    Err(_) => log::error!("закрытие комнаты: не завершилось за 5 с"),
                }
                drop(room);
                log::info!("выход: объект комнаты освобождён");
            });
        }
        log::info!("выход: закрытие комнаты запущено");
        self.rc.leave_room();
        self.publish_rc();
        self.last_peers.clear();
        self.counters = Default::default();
        self.metrics_at = None;
        self.set(|s| {
            s.metrics = None;
            s.stream_paused.clear();
        });
    }

    fn lost_connection(&mut self, why: &str) {
        let ch = self.state.lock().unwrap().channel.clone();
        log::warn!("связь с комнатой потеряна: {why}");
        match ch {
            Some(ch) => self.schedule_rejoin(ch, 0),
            None => self.set(|s| s.conn = Conn::Idle),
        }
    }

    fn schedule_rejoin(&mut self, channel: Channel, prev_attempt: usize) {
        let attempt = prev_attempt + 1;
        let wait = REJOIN_BACKOFF[(attempt - 1).min(REJOIN_BACKOFF.len() - 1)];
        self.rejoin = Some(Rejoin { channel, attempt, at: Instant::now() + Duration::from_secs(wait) });
        self.set(|s| {
            s.conn = Conn::Rejoining { attempt };
            s.rtt = None;
            s.jitter = None;
            s.loss = None;
            s.link = crate::link::LinkState::Fine;
        });
    }

    async fn on_room_event(&mut self, ev: RoomEvent) {
        match ev {
            RoomEvent::ParticipantConnected(p) => {
                log::info!("+ {} ({})", p.name(), p.identity());
                self.play(Sound::JoinOther);
                self.refresh_peers(false);
                // новичку — сразу, если картинка на паузе
                if self.pause_sent.is_some_and(|(p, _)| p.is_some()) {
                    self.pause_sent = None;
                    self.send_stream_state().await;
                }
            }
            RoomEvent::DataReceived { payload, topic, participant, .. }
                if crate::remote::engine::is_rc_topic(topic.as_deref()) =>
            {
                let (Some(p), Some(msg)) = (participant, crate::remote::engine::parse_inbound(topic.as_deref(), &payload)) else {
                    return;
                };
                let me = self.my_identity();
                let o = self.rc.inbound(p.identity().as_str(), &me, msg, &self.viewers);
                self.rc_out(o);
            }
            RoomEvent::DataReceived { payload, topic, participant, .. }
                if topic.as_deref() == Some(WATCH_TOPIC) =>
            {
                if let Some(p) = participant
                    && self.stream.is_some()
                {
                    let id = p.identity().as_str().to_string();
                    let on = payload.as_slice() == b"1";
                    let changed = if on { self.viewers.insert(id.clone()) } else { self.viewers.remove(&id) };
                    if changed {
                        log::info!("зритель {id}: {}", if on { "смотрит" } else { "перестал" });
                        self.play(if on { Sound::ViewerJoin } else { Sound::ViewerLeave });
                        self.publish_viewers();
                    }
                }
            }
            RoomEvent::DataReceived { payload, topic, participant, .. } => {
                if topic.as_deref() == Some(STREAM_STATE_TOPIC)
                    && let Some(p) = participant
                {
                    let id = p.identity().as_str().to_string();
                    let pause = match payload.as_slice() {
                        b"minimized" => Some(StreamPause::Minimized),
                        b"hidden" => Some(StreamPause::Hidden),
                        _ => None,
                    };
                    self.set(|s| {
                        let prev = match pause {
                            Some(v) => s.stream_paused.insert(id.clone(), v),
                            None => s.stream_paused.remove(&id),
                        };
                        if prev != pause {
                            log::info!("трансляция {id}: {pause:?}");
                        }
                    });
                }
            }
            RoomEvent::ParticipantDisconnected(p) => {
                log::info!("- {} ({})", p.name(), p.identity());
                self.set(|s| {
                    s.stream_paused.remove(p.identity().as_str());
                });
                if self.viewers.remove(p.identity().as_str()) {
                    self.play(Sound::ViewerLeave);
                    self.publish_viewers();
                }
                self.set_watching(p.identity().as_str(), false);
                let o = self.rc.left(p.identity().as_str(), &self.viewers);
                self.rc_out(o);
                self.play(Sound::LeaveOther);
                self.refresh_peers(false);
            }
            RoomEvent::TrackSubscribed { track, publication, participant } => {
                match track {
                    RemoteTrack::Audio(a) => {
                        let v = self.volume_for(participant.identity().as_str(), publication.source());
                        log::info!("подписка на аудио {} {:?} (громкость {v})", participant.identity(), publication.source());
                        a.rtc_track().set_volume(v as f64);
                    }
                    RemoteTrack::Video(v) => {
                        let id = participant.identity().as_str().to_string();
                        let layer = layer_of(&publication);
                        if publication.source() == TrackSource::Screenshare
                            && self.watching.contains(&id)
                            && layer == self.target_layer(&id)
                        {
                            log::info!("смотрим трансляцию {id} ({layer:?})");
                            let rtc = v.rtc_track().id();
                            // Старый приёмник (другой слой) заменяется; его текстура остаётся до первого нового кадра.
                            if let Some(old) = self.receivers.insert(id.clone(), self.video.spawn_receiver(id.clone(), v)) {
                                old.abort();
                            }
                            self.recv.insert(id.clone(), (publication.sid(), rtc, layer, Instant::now()));
                            self.apply_view(&id);
                            self.publish_layers();
                            // Прежний слой больше не нужен.
                            self.sync_subscriptions();
                        } else if !self.wants(&id, &publication) {
                            // Пришло то, что уже не нужно (успели нажать «Перестать смотреть»).
                            publication.set_subscribed(false);
                        }
                    }
                }
                self.refresh_peers(false);
            }
            RoomEvent::TrackPublished { publication, participant } => {
                log::info!("{} опубликовал {:?} «{}»", participant.identity(), publication.source(), publication.name());
                if publication.source() == TrackSource::Screenshare {
                    if layer_of(&publication) == Layer::High {
                        self.play(Sound::StreamStart);
                    } else {
                        // Появился экономный слой: в «Экономном» перейти на него.
                        self.sync_subscriptions();
                        self.publish_layers();
                    }
                }
                self.sync_subscription(participant.identity().as_str(), &publication);
                self.refresh_peers(false);
            }
            RoomEvent::TrackUnpublished { publication, participant } => {
                log::info!("{} снял {:?} «{}»", participant.identity(), publication.source(), publication.name());
                if publication.source() == TrackSource::Screenshare && layer_of(&publication) == Layer::Low {
                    // Экономного слоя больше нет — остаётся исходный.
                    self.sync_subscriptions();
                    self.publish_layers();
                } else if publication.source() == TrackSource::Screenshare {
                    self.play(Sound::StreamStop);
                    // Трансляция закончилась — перестаём «смотреть» (новую стример покажет заново, как в Discord).
                    self.set(|s| {
                        s.stream_paused.remove(participant.identity().as_str());
                    });
                    self.set_watching(participant.identity().as_str(), false);
                }
                self.refresh_peers(false);
            }
            RoomEvent::TrackUnsubscribed { publication, participant, .. } => {
                let id = participant.identity().as_str().to_string();
                // Снят прежний слой после переключения — приёмник уже на новом, не трогаем.
                if publication.source() == TrackSource::Screenshare
                    && self.recv.get(&id).is_some_and(|(sid, ..)| *sid == publication.sid())
                {
                    self.stop_receiver(&id);
                    if self.watching.contains(&id) {
                        self.sync_subscriptions();
                    }
                }
                self.refresh_peers(false);
            }
            RoomEvent::TrackMuted { .. }
            | RoomEvent::TrackUnmuted { .. }
            | RoomEvent::ActiveSpeakersChanged { .. }
            | RoomEvent::ParticipantNameChanged { .. } => self.refresh_peers(false),
            RoomEvent::Reconnecting => self.set(|s| s.conn = Conn::Reconnecting),
            RoomEvent::Reconnected => {
                self.set(|s| s.conn = Conn::Connected);
                self.apply_mic();
                self.apply_volumes();
                self.refresh_peers(false);
            }
            RoomEvent::Disconnected { reason } => {
                log::warn!("комната: Disconnected {reason:?}");
                self.drop_room();
                if reason == livekit::DisconnectReason::ClientInitiated {
                    self.stop_stream().await;
                    self.set(|s| s.conn = Conn::Idle);
                } else if let Some(msg) = final_disconnect(reason) {
                    // выгнали, забанили, «выкинули из голоса», канал удалён — назад не возвращаемся
                    self.stop_stream().await;
                    self.rejoin = None;
                    self.set(|s| {
                        s.conn = Conn::Failed(msg.into());
                        s.peers.clear();
                    });
                } else {
                    self.lost_connection(&format!("{reason:?}"));
                }
            }
            _ => {}
        }
    }

    async fn on_tick(&mut self) {
        if let Some(at) = self.ptt_release_at
            && Instant::now() >= at {
                self.ptt_release_at = None;
                self.ptt_held = false;
                self.apply_mic();
            }
        if let Some(rj) = &self.rejoin
            && Instant::now() >= rj.at {
                let (ch, attempt) = (rj.channel.clone(), rj.attempt);
                self.rejoin = None;
                self.leave().await;
                self.join(ch, attempt).await;
            }
        if self.room.is_some() {
            let target = self.stream.as_ref().map(|s| match s.source_id {
                SourceId::Monitor(h) => crate::remote::inject::Target::Monitor(h),
                SourceId::Window(h) => crate::remote::inject::Target::Window(h),
            });
            let mut o = self.rc.set_target(target, &self.viewers);
            self.rc.set_names(&self.last_peers);
            o.extend(self.rc.tick(&self.viewers));
            self.rc_out(o);
            self.refresh_peers(true);
            if self.pause_sent.is_some_and(|(p, at)| p.is_some() && at.elapsed() >= STREAM_STATE_RESEND) {
                self.send_stream_state().await;
            }
            let every = if self.overlay || capture_win::diag::on() { Duration::from_secs(1) } else { STATS_EVERY };
            if self.last_stats.elapsed() >= every {
                self.last_stats = Instant::now();
                self.update_rtt().await;
            }
        }
    }

    async fn update_rtt(&mut self) {
        let Some((room, _)) = &self.room else { return };
        let Ok(Ok(stats)) = tokio::time::timeout(IO_TIMEOUT, room.get_stats()).await else { return };
        self.stats_n += 1;
        if self.dump_stats {
            self.dump_stats = false;
            log::info!("статистика WebRTC (publisher): {:#?}", stats.publisher_stats);
            log::info!("статистика WebRTC (subscriber): {:#?}", stats.subscriber_stats);
        }
        if self.overlay {
            let now = Instant::now();
            let dt = self.metrics_at.map_or(1.0, |t| now.duration_since(t).as_secs_f64());
            self.metrics_at = Some(now);
            let all: Vec<RtcStats> = stats.publisher_stats.iter().chain(stats.subscriber_stats.iter()).cloned().collect();
            let labels = self.track_labels();
            let mut m = crate::metrics::collect(&all, &labels, &mut self.counters, dt, self.force_relay);
            self.set(|s| {
                // Pre-encoded: libwebrtc видит только «passthrough» — подставляем настоящий энкодер.
                if let Some(info) = &s.stream {
                    for r in m.rows.iter_mut().filter(|r| r.dir == crate::metrics::Dir::Out && r.kind == "video") {
                        r.detail = r.detail.replacen("LiveKit pre-encoded passthrough", &info.backend, 1);
                    }
                }
                s.metrics = Some(m);
            });
        }
        if capture_win::diag::on() {
            let all: Vec<RtcStats> = stats.publisher_stats.iter().chain(stats.subscriber_stats.iter()).cloned().collect();
            let mut prev = crate::metrics::Counters::default();
            let m = crate::metrics::collect(&all, &HashMap::new(), &mut prev, 1.0, self.force_relay);
            log::info!(
                "ДИАГ сеть: путь {:?} → {} {}, RTT {:?} мс, доступно на отправку {:?} кбит/с",
                m.conn.path,
                m.conn.remote,
                m.conn.turn,
                m.conn.rtt_ms.map(|v| v.round()),
                m.conn.out_kbps_avail.map(|v| v.round())
            );
        }
        self.update_video_stats(&stats.publisher_stats);
        // Одна PeerConnection на всё (LiveKit 1.13) — входящие тоже в publisher_stats.
        self.log_inbound_video(&stats.subscriber_stats);
        self.log_inbound_video(&stats.publisher_stats);
        self.log_audio(&stats.publisher_stats);
        let all: Vec<&RtcStats> = stats.publisher_stats.iter().chain(stats.subscriber_stats.iter()).collect();
        self.update_layers(&all);
        let rtt = stats
            .publisher_stats
            .iter()
            .chain(stats.subscriber_stats.iter())
            .filter_map(|s| match s {
                RtcStats::CandidatePair(cp) if cp.candidate_pair.nominated && cp.candidate_pair.current_round_trip_time > 0.0 => {
                    Some(cp.candidate_pair.current_round_trip_time)
                }
                _ => None,
            })
            .reduce(f64::min);
        let (jitter, loss) = self.audio_quality(stats.publisher_stats.iter().chain(stats.subscriber_stats.iter()));
        if rtt.is_some() {
            log::debug!("rtt {rtt:?}, джиттер {jitter:?}, потери {loss:?}");
            self.set(|s| s.rtt = rtt);
        }
        let link = self.link.push(Instant::now(), jitter, loss);
        self.set(|s| {
            s.jitter = jitter;
            s.loss = loss;
            s.link = link;
        });
    }

    /// Джиттер и потери аудио для пинга в заголовке: наш звук глазами сервера (remote-inbound) и входящий звук других.
    fn audio_quality<'a>(&mut self, stats: impl Iterator<Item = &'a RtcStats>) -> (Option<f64>, Option<f64>) {
        let (mut jitter, mut loss) = (None::<f64>, None::<f64>);
        let (mut lost, mut recv) = (0i64, 0u64);
        for s in stats {
            match s {
                RtcStats::RemoteInboundRtp(r) if r.stream.kind == "audio" => {
                    jitter = Some(jitter.unwrap_or(0.0).max(r.received.jitter));
                    loss = Some(loss.unwrap_or(0.0).max(r.remote_inbound.fraction_lost.clamp(0.0, 1.0)));
                }
                RtcStats::InboundRtp(i) if i.stream.kind == "audio" => {
                    jitter = Some(jitter.unwrap_or(0.0).max(i.received.jitter));
                    lost += i.received.packets_lost;
                    recv += i.received.packets_received;
                }
                _ => {}
            }
        }
        let (pl, pr) = std::mem::replace(&mut self.audio_counts, (lost, recv));
        let (dl, dr) = ((lost - pl).max(0) as f64, recv.saturating_sub(pr) as f64);
        if pr > 0 && dl + dr > 0.0 {
            loss = Some(loss.unwrap_or(0.0).max(dl / (dl + dr)));
        }
        (jitter, loss)
    }

    // ---------------------------------------------------------------- трансляция

    async fn start_stream(&mut self, source: SourceId, title: String, width: u32, height: u32, preset: Preset, audio: bool) {
        if self.room.is_none() {
            self.set(|s| s.stream_notice = Some("Не удалось начать трансляцию: нет подключения к голосовому каналу.".into()));
            return;
        }
        self.set(|s| s.stream_notice = None);
        // Смена пресета/источника идущей трансляции — без звуков «конец» + «начало».
        let restarting = self.stream.is_some();
        // Тот же пресет — меняем только источник: трек и публикация остаются (зрители не переподключаются).
        if let Some(mut st) = self.stream.take() {
            if st.preset == preset && st.publication.is_some() {
                if let Some(c) = st.capture.take() {
                    tokio::task::block_in_place(|| c.stop_bounded());
                }
                self.generation += 1;
                self.last_video_bytes = None;
                let low = st.low.as_ref().map(|(s, _)| s.clone());
                let r = tokio::task::block_in_place(|| stream::restart(source, preset, &st.source, low.as_ref(), st.hw, self.generation, &self.cap_tx, None));
                match r {
                    Ok(c) => {
                        st.capture = Some(c);
                        st.source_id = source;
                        self.stream = Some(st);
                        log::info!("трансляция: источник сменён на {source:?}");
                        self.set(|s| {
                            if let Some(info) = &mut s.stream {
                                info.source = source;
                                info.title = title;
                                info.minimized = false;
                                info.hidden = false;
                                info.size = None;
                            }
                        });
                        self.set_stream_audio(source, audio).await;
                        return;
                    }
                    Err(e) => {
                        // Например, аппаратный энкодер не поднялся на новом источнике — начинаем заново.
                        log::warn!("смена источника на лету: {e} — перезапуск трансляции");
                    }
                }
            }
            self.stream = Some(st);
            self.stop_stream_quiet().await;
        }

        self.generation += 1;
        self.last_video_bytes = None;
        let started = match tokio::task::block_in_place(|| stream::start(source, width, height, preset, self.generation, &self.cap_tx)) {
            Ok(s) => s,
            Err(e) => {
                log::error!("трансляция: {e}");
                self.set(|s| s.stream_notice = Some(e));
                return;
            }
        };
        let Some((room, _)) = &self.room else { return };
        let publication = match stream::publish(room, &started.video, preset, started.hw).await {
            Ok(p) => p,
            Err(e) => {
                log::error!("публикация трансляции: {e}");
                tokio::task::block_in_place(|| started.capture.stop_bounded());
                self.set(|s| s.stream_notice = Some(format!("Не удалось начать трансляцию: {e}")));
                return;
            }
        };
        let low_pub = match &started.low {
            Some(l) => match stream::publish_low(room, l).await {
                Ok(p) => Some(p),
                Err(e) => {
                    log::warn!("публикация экономного слоя: {e} — только основной");
                    None
                }
            },
            None => None,
        };
        log::info!("трансляция: {source:?} «{title}», {preset:?}, энкодер {}, экономный слой {}", started.encoder, low_pub.is_some());
        if !restarting {
            self.play(Sound::StreamStart);
        }
        let backend = started.encoder.clone();
        self.stream = Some(Streaming {
            capture: Some(started.capture),
            source: started.video,
            preset,
            hw: started.hw,
            source_id: source,
            publication: Some(publication),
            low: started.low.map(|s| (s, low_pub)),
            audio: None,
        });
        self.set(|s| {
            s.stream = Some(StreamInfo {
                source,
                title,
                preset,
                minimized: false,
                hidden: false,
                viewers: 0,
                size: None,
                backend,
                encoder: None,
                fps: 0.0,
                kbps: 0.0,
                limitation: None,
                audio: AudioStatus::Off,
            })
        });
        self.set_stream_audio(source, audio).await;
        self.refresh_peers(false);
    }

    /// Включает/выключает/перенацеливает звук трансляции. Ошибка звука трансляцию не останавливает.
    async fn set_stream_audio(&mut self, source: SourceId, want: bool) {
        let Some(st) = &mut self.stream else { return };
        let room = self.room.as_ref().map(|(r, _)| r);
        if let Some(c) = st.audio.as_mut().and_then(|a| a.capture.take()) {
            tokio::task::block_in_place(|| c.stop());
        }
        if !want {
            if let Some(a) = st.audio.take()
                && let (Some(p), Some(room)) = (a.publication, room)
                && let Ok(Err(e)) = tokio::time::timeout(IO_TIMEOUT, room.local_participant().unpublish_track(&p.sid())).await
            {
                log::warn!("снятие звука трансляции: {e}");
            }
            self.set(|s| {
                if let Some(i) = &mut s.stream {
                    i.audio = AudioStatus::Off;
                }
            });
            return;
        }
        let a = st.audio.get_or_insert_with(|| StreamSound { capture: None, source: stream::new_audio_source(), publication: None });
        let status = match tokio::task::block_in_place(|| stream::start_audio(source, &a.source)) {
            Ok(c) => {
                a.capture = Some(c);
                if a.publication.is_none()
                    && let Some(room) = room
                {
                    match stream::publish_audio(room, &a.source).await {
                        Ok(p) => a.publication = Some(p),
                        Err(e) => log::error!("публикация звука трансляции: {e}"),
                    }
                }
                AudioStatus::On(if source.is_window() { "звук приложения" } else { "весь звук, кроме Murmur" })
            }
            Err(e) => {
                log::error!("звук трансляции: {e}");
                AudioStatus::Failed(e)
            }
        };
        self.set(|s| {
            if let Some(i) = &mut s.stream {
                i.audio = status;
            }
        });
    }

    async fn stop_stream(&mut self) {
        if self.stream.is_some() {
            self.play(Sound::StreamStop);
        }
        self.stop_stream_quiet().await;
    }

    async fn stop_stream_quiet(&mut self) {
        let Some(mut st) = self.stream.take() else { return };
        if let Some(c) = st.capture.take() {
            tokio::task::block_in_place(|| c.stop_bounded());
        }
        if let Some(mut a) = st.audio.take() {
            if let Some(c) = a.capture.take() {
                tokio::task::block_in_place(|| c.stop());
            }
            if let (Some(p), Some((room, _))) = (a.publication.take(), &self.room)
                && let Ok(Err(e)) = tokio::time::timeout(IO_TIMEOUT, room.local_participant().unpublish_track(&p.sid())).await
            {
                log::warn!("снятие звука трансляции: {e}");
            }
        }
        if let (Some((_, Some(p))), Some((room, _))) = (st.low.take(), &self.room)
            && let Ok(Err(e)) = tokio::time::timeout(IO_TIMEOUT, room.local_participant().unpublish_track(&p.sid())).await
        {
            log::warn!("снятие экономного слоя: {e}");
        }
        if let (Some(p), Some((room, _))) = (st.publication.take(), &self.room)
            && let Ok(Err(e)) = tokio::time::timeout(IO_TIMEOUT, room.local_participant().unpublish_track(&p.sid())).await {
                log::warn!("снятие трансляции: {e}");
            }
        self.generation += 1; // события остановленного захвата больше не нужны
        self.last_video_bytes = None;
        self.pause_sent = None;
        self.viewers.clear();
        log::info!("трансляция остановлена");
        self.set(|s| s.stream = None);
        self.refresh_peers(false);
    }

    /// Аппаратный энкодер — фиксированного размера: окно устойчиво изменилось (развернули, растянули) →
    /// пересоздаём захват под новый размер в тот же опубликованный трек (зрители не переподключаются).
    async fn resize_hw_capture(&mut self, width: u32, height: u32) {
        let Some(st) = self.stream.as_mut() else { return };
        if !st.hw {
            return;
        }
        let (source, preset) = (st.source_id, st.preset);
        if let Some(c) = st.capture.take() {
            tokio::task::block_in_place(|| c.stop_bounded());
        }
        self.generation += 1;
        self.last_video_bytes = None;
        let Some(st) = self.stream.as_mut() else { return };
        let low = st.low.as_ref().map(|(s, _)| s.clone());
        match tokio::task::block_in_place(|| stream::restart(source, preset, &st.source, low.as_ref(), true, self.generation, &self.cap_tx, Some((width, height)))) {
            Ok(c) => {
                st.capture = Some(c);
                log::info!("трансляция: энкодер пересоздан под {width}×{height}");
                self.set(|s| {
                    if let Some(i) = &mut s.stream {
                        i.size = Some((width, height));
                    }
                });
            }
            Err(e) => {
                // Запасной путь: новая трансляция с нуля — stream::start сам уйдёт на процессор, если видеокарта не смогла.
                log::warn!("трансляция: пересоздание энкодера под {width}×{height}: {e} — запуск заново");
                let mut title = None;
                self.set(|s| title = s.stream.as_ref().map(|i| i.title.clone()));
                let audio = self.stream.as_ref().is_some_and(|st| st.audio.is_some());
                self.stop_stream_quiet().await;
                if let Some(title) = title {
                    self.start_stream(source, title, width, height, preset, audio).await;
                }
                if self.stream.is_none() {
                    self.set(|s| s.stream_notice = Some(format!("Трансляция остановлена: {e}")));
                }
            }
        }
    }

    async fn on_capture_event(&mut self, ev: CaptureEvent) {
        if ev.generation != self.generation || self.stream.is_none() {
            return;
        }
        match ev.event {
            CapEvent::Started { width, height } | CapEvent::Resized { width, height } => self.set(|s| {
                if let Some(i) = &mut s.stream {
                    i.size = Some((width, height));
                }
            }),
            CapEvent::SizeChanged { width, height } => self.resize_hw_capture(width, height).await,
            CapEvent::Minimized(m) => {
                self.set(|s| {
                    if let Some(i) = &mut s.stream {
                        i.minimized = m;
                    }
                });
                self.send_stream_state().await;
            }
            CapEvent::Hidden(h) => {
                self.set(|s| {
                    if let Some(i) = &mut s.stream {
                        i.hidden = h;
                    }
                });
                self.send_stream_state().await;
            }
            CapEvent::Closed => {
                self.stop_stream().await;
                self.set(|s| s.stream_notice = Some("Окно закрыто — трансляция остановлена.".into()));
            }
            CapEvent::Error(e) => {
                self.stop_stream().await;
                self.set(|s| s.stream_notice = Some(format!("Трансляция остановлена: {e}")));
            }
        }
    }

    /// Сообщить зрителям, на паузе ли картинка своей трансляции (только при смене; пока пауза — повторять).
    async fn send_stream_state(&mut self) {
        let pause = {
            let s = self.state.lock().unwrap();
            s.stream.as_ref().and_then(|i| {
                if i.minimized {
                    Some(StreamPause::Minimized)
                } else if i.hidden {
                    Some(StreamPause::Hidden)
                } else {
                    None
                }
            })
        };
        let due = match self.pause_sent {
            None => pause.is_some(),
            Some((p, at)) => p != pause || (pause.is_some() && at.elapsed() >= STREAM_STATE_RESEND),
        };
        let Some((room, _)) = &self.room else { return };
        if !due {
            return;
        }
        self.pause_sent = Some((pause, Instant::now()));
        let packet = livekit::DataPacket {
            payload: StreamPause::wire(pause).to_vec(),
            topic: Some(STREAM_STATE_TOPIC.into()),
            reliable: true,
            ..Default::default()
        };
        if let Ok(Err(e)) = tokio::time::timeout(IO_TIMEOUT, room.local_participant().publish_data(packet)).await {
            log::warn!("состояние трансляции зрителям: {e}");
        }
    }

    /// Аудиопотоки в лог (раз в 5 опросов): кодек/каналы/fmtp, байты, потери и «заплатки» у приёмника.
    fn log_audio(&self, stats: &[RtcStats]) {
        if !self.stats_n.is_multiple_of(5) {
            return;
        }
        let codec = |id: &str| {
            stats.iter().find_map(|s| match s {
                RtcStats::Codec(c) if c.rtc.id == id => Some(format!("{}/{}ch [{}]", c.codec.mime_type, c.codec.channels, c.codec.sdp_fmtp_line)),
                _ => None,
            })
        };
        for s in stats {
            match s {
                RtcStats::OutboundRtp(o) if o.stream.kind == "audio" => log::info!(
                    "исходящее аудио ssrc {}: {:?}, отправлено {} пакетов / {} КБ",
                    o.stream.ssrc,
                    codec(&o.stream.codec_id),
                    o.sent.packets_sent,
                    o.sent.bytes_sent / 1024
                ),
                RtcStats::MediaSource(m) if m.source.kind == "audio" => {
                    log::info!("источник аудио {}: уровень {:.3}, энергия {:.2}", m.source.track_identifier, m.audio.audio_level, m.audio.total_audio_energy)
                }
                RtcStats::InboundRtp(i) if i.stream.kind == "audio" => log::info!(
                    "входящее аудио ssrc {}: {:?}, принято {} пакетов / {} КБ, потеряно {}, сэмплов {} (заплаток {}), уровень {:.3}",
                    i.stream.ssrc,
                    codec(&i.stream.codec_id),
                    i.received.packets_received,
                    i.inbound.bytes_received / 1024,
                    i.received.packets_lost,
                    i.inbound.total_samples_received,
                    i.inbound.concealed_samples,
                    i.inbound.audio_level
                ),
                _ => {}
            }
        }
    }

    /// Входящее видео трансляций — в лог (раз в 5 опросов).
    fn log_inbound_video(&self, stats: &[RtcStats]) {
        if capture_win::diag::on() {
            for s in stats {
                if let RtcStats::InboundRtp(i) = s
                    && i.stream.kind == "video"
                {
                    let jb = if i.inbound.jitter_buffer_emitted_count > 0 {
                        i.inbound.jitter_buffer_delay * 1000.0 / i.inbound.jitter_buffer_emitted_count as f64
                    } else {
                        0.0
                    };
                    log::info!(
                        "ДИАГ зритель видео: {}×{} {:.0} к/с, принято кадров {} / декодировано {} / выброшено {}, ключевых {}, потеряно пакетов {} из {}, NACK {}, PLI {}, FIR {}, заморозок {} ({:.1} с), пауз {} ({:.1} с), буфер {:.0} мс",
                        i.inbound.frame_width,
                        i.inbound.frame_height,
                        i.inbound.frames_per_second,
                        i.inbound.frames_received,
                        i.inbound.frames_decoded,
                        i.inbound.frames_dropped,
                        i.inbound.key_frames_decoded,
                        i.received.packets_lost,
                        i.received.packets_received,
                        i.inbound.nack_count,
                        i.inbound.pli_count,
                        i.inbound.fir_count,
                        i.inbound.freeze_count,
                        i.inbound.total_freeze_duration,
                        i.inbound.pause_count,
                        i.inbound.total_pause_duration,
                        jb
                    );
                }
            }
            return;
        }
        if !self.stats_n.is_multiple_of(5) {
            return;
        }
        log::debug!("subscriber stats: {} записей, inbound: {}", stats.len(), stats.iter().filter(|s| matches!(s, RtcStats::InboundRtp(_))).count());
        for s in stats {
            if let RtcStats::InboundRtp(i) = s
                && i.stream.kind == "video" {
                    log::info!(
                        "входящее видео: декодер {:?}, {}×{} {:.0} к/с, кадров {} (ключевых {}, выброшено {}), потеряно пакетов {} из {}, NACK {}, PLI {}, заморозок {}",
                        i.inbound.decoder_implementation,
                        i.inbound.frame_width,
                        i.inbound.frame_height,
                        i.inbound.frames_per_second,
                        i.inbound.frames_decoded,
                        i.inbound.key_frames_decoded,
                        i.inbound.frames_dropped,
                        i.received.packets_lost,
                        i.received.packets_received,
                        i.inbound.nack_count,
                        i.inbound.pli_count,
                        i.inbound.freeze_count
                    );
                }
        }
    }

    /// Статистика исходящего видео (раз в STATS_EVERY).
    fn update_video_stats(&mut self, stats: &[RtcStats]) {
        if self.stream.is_none() {
            return;
        }
        // Исходящих видео может быть несколько: после переподключения (перезапуск SFU) или смены пресета старый поток ещё
        // числится, с застывшими счётчиками и 0×0. Берём тот, что отправил больше всего с прошлого замера.
        let prev = std::mem::take(&mut self.video_sent_by_ssrc);
        let outs: Vec<_> = stats
            .iter()
            .filter_map(|s| match s {
                RtcStats::OutboundRtp(o) if o.stream.kind == "video" => Some(o),
                _ => None,
            })
            .collect();
        for o in &outs {
            self.video_sent_by_ssrc.insert(o.stream.ssrc, o.sent.bytes_sent);
        }
        let Some(o) = pick_active_outbound(&outs, &prev) else {
            return;
        };
        let now = Instant::now();
        // Байты — по всем исходящим видеопотокам (после смены пресета старый поток может ещё числиться).
        let bytes: u64 = stats
            .iter()
            .filter_map(|s| match s {
                RtcStats::OutboundRtp(o) if o.stream.kind == "video" => Some(o.sent.bytes_sent),
                _ => None,
            })
            .sum();
        let kbps = match self.last_video_bytes {
            Some((b, t)) if bytes >= b => (bytes - b) as f64 * 8.0 / 1000.0 / now.duration_since(t).as_secs_f64().max(0.001),
            _ => 0.0,
        };
        self.last_video_bytes = Some((bytes, now));
        let enc = o.outbound.encoder_implementation.clone();
        let fps = o.outbound.frames_per_second;
        let lim = format!("{:?}", o.outbound.quality_limitation_reason);
        // потери — из отчёта SFU именно об этом потоке
        let loss = stats.iter().find_map(|s| match s {
            RtcStats::RemoteInboundRtp(r) if r.stream.kind == "video" && r.stream.ssrc == o.stream.ssrc => Some(r.remote_inbound.fraction_lost),
            _ => None,
        });
        let enc_ms = if o.outbound.frames_encoded > 0 {
            o.outbound.total_encode_time * 1000.0 / o.outbound.frames_encoded as f64
        } else {
            0.0
        };
        if self.stats_n % 5 == 1 || capture_win::diag::on() {
            log::info!(
                "видео: энкодер {enc:?}, {}×{} {fps:.0} к/с, {kbps:.0} кбит/с, цель {:.0} кбит/с, кодирование {enc_ms:.1} мс/кадр, ограничение {lim}, NACK {}, PLI {}, потери у SFU {:?}",
                o.outbound.frame_width,
                o.outbound.frame_height,
                o.outbound.target_bitrate / 1000.0,
                o.outbound.nack_count,
                o.outbound.pli_count,
                loss
            );
        }
        self.set(|s| {
            if let Some(i) = &mut s.stream {
                i.encoder = (!enc.is_empty()).then_some(enc);
                i.fps = fps;
                i.kbps = kbps;
                i.limitation = (lim != "None").then_some(lim);
            }
        });
    }

    // ---------------------------------------------------------------- микрофон/громкость

    fn mic_should_send(&self) -> bool {
        !self.self_muted && !self.deafened && (!self.prefs.ptt_enabled || self.ptt_held)
    }

    fn apply_mic(&mut self) {
        let send = self.mic_should_send();
        if let Some(p) = &self.mic {
            if send && p.is_muted() {
                p.unmute();
            } else if !send && !p.is_muted() {
                p.mute();
            }
        }
        let (muted, deaf, held) = (self.self_muted, self.deafened, self.ptt_held);
        let transmitting = send && self.mic.is_some();
        self.set(|s| {
            s.self_muted = muted;
            s.deafened = deaf;
            s.ptt_held = held;
            s.transmitting = transmitting;
        });
        self.refresh_peers(false);
    }

    /// Громкость дорожки: голос и звук трансляции регулируются отдельно.
    fn volume_for(&self, identity: &str, source: TrackSource) -> f32 {
        if self.deafened {
            return 0.0;
        }
        let map = if source == TrackSource::ScreenshareAudio { &self.prefs.stream_volumes } else { &self.prefs.volumes };
        map.get(identity).copied().unwrap_or(1.0) * self.prefs.output_volume
    }

    fn apply_volume_for(&self, identity: &str) {
        let Some((room, _)) = &self.room else { return };
        for (id, p) in room.remote_participants() {
            if id.as_str() != identity {
                continue;
            }
            for publication in p.track_publications().values() {
                if let Some(RemoteTrack::Audio(a)) = publication.track() {
                    a.rtc_track().set_volume(self.volume_for(identity, publication.source()) as f64);
                }
            }
        }
    }

    fn apply_volumes(&self) {
        let Some((room, _)) = &self.room else { return };
        for (id, p) in room.remote_participants() {
            for publication in p.track_publications().values() {
                if let Some(RemoteTrack::Audio(a)) = publication.track() {
                    a.rtc_track().set_volume(self.volume_for(id.as_str(), publication.source()) as f64);
                }
            }
        }
    }

    // ---------------------------------------------------------------- просмотр трансляций

    /// Нужна ли нам эта публикация: голос — всегда, трансляция (видео и её звук) — только если смотрим.
    fn wants(&self, identity: &str, publication: &RemoteTrackPublication) -> bool {
        match publication.source() {
            // Видео трансляции — только выбранный слой; при переключении прежний держим, пока не придёт новый (без чёрного экрана).
            TrackSource::Screenshare => {
                self.watching.contains(identity)
                    && (layer_of(publication) == self.target_layer(identity)
                        || self.recv.get(identity).is_some_and(|(sid, ..)| *sid == publication.sid()))
            }
            TrackSource::ScreenshareAudio => self.watching.contains(identity),
            _ => publication.kind() == TrackKind::Audio,
        }
    }

    fn sync_subscription(&self, identity: &str, publication: &RemoteTrackPublication) {
        let want = self.wants(identity, publication);
        if want != publication.is_desired() {
            log::debug!("{identity} {:?}: подписка {want}", publication.source());
            publication.set_subscribed(want);
        }
    }

    fn sync_subscriptions(&self) {
        let Some((room, _)) = &self.room else { return };
        for (id, p) in room.remote_participants() {
            for publication in p.track_publications().values() {
                self.sync_subscription(id.as_str(), publication);
            }
        }
    }

    fn set_watching(&mut self, identity: &str, on: bool) {
        let changed = if on { self.watching.insert(identity.to_string()) } else { self.watching.remove(identity) };
        if !changed {
            return;
        }
        log::info!("трансляция {identity}: {}", if on { "смотрим" } else { "не смотрим" });
        if !on {
            let o = self.rc.stop_watching(identity);
            self.rc_out(o);
            self.stop_receiver(identity);
            self.views.remove(identity);
            self.auto.remove(identity);
        }
        self.sync_subscriptions();
        self.publish_watching();
        // Стримеру — для звука «зритель пришёл/ушёл» и счётчика зрителей.
        if let Some((room, _)) = &self.room {
            let lp = room.local_participant();
            let packet = livekit::DataPacket {
                payload: if on { b"1".to_vec() } else { b"0".to_vec() },
                topic: Some(WATCH_TOPIC.into()),
                reliable: true,
                destination_identities: vec![identity.to_string().into()],
            };
            tokio::spawn(async move {
                if let Ok(Err(e)) = tokio::time::timeout(IO_TIMEOUT, lp.publish_data(packet)).await {
                    log::warn!("сообщение стримеру: {e}");
                }
            });
        }
    }

    fn my_identity(&self) -> String {
        self.room.as_ref().map(|(r, _)| r.local_participant().identity().as_str().to_string()).unwrap_or_default()
    }

    /// Отправить сообщения управления и обновить вид в интерфейсе.
    fn rc_out(&self, outs: Vec<Outgoing>) {
        if let Some((room, _)) = &self.room {
            for o in outs {
                let lp = room.local_participant();
                let packet = livekit::DataPacket {
                    payload: o.payload,
                    topic: Some(o.topic.into()),
                    reliable: o.reliable,
                    destination_identities: o.to.into_iter().map(Into::into).collect(),
                };
                let _ = self.rc_tx.send((lp, packet));
            }
        }
        self.publish_rc();
    }

    fn publish_rc(&self) {
        let (host, guest, ptrs) = (self.rc.host_view(), self.rc.guest_views(), self.rc.pointer_views());
        let notice = self.rc.notice.as_ref().map(|(t, _)| t.clone());
        let (changed, new_ask) = {
            let s = self.state.lock().unwrap();
            (
                s.rc_host != host || s.rc_guest != guest || s.rc_pointers != ptrs || s.rc_notice != notice,
                host.pending.len() > s.rc_host.pending.len(),
            )
        };
        if new_ask {
            self.play(Sound::DirectMessage);
        }
        if changed {
            self.set(|s| {
                s.rc_host = host;
                s.rc_guest = guest;
                s.rc_pointers = ptrs;
                s.rc_notice = notice;
            });
        }
    }

    fn publish_viewers(&self) {
        let n = self.viewers.len();
        self.set(|s| {
            if let Some(i) = &mut s.stream {
                i.viewers = n;
            }
        });
    }

    /// Выход из канала / другой канал: ничего не смотрим.
    fn forget_watching(&mut self) {
        self.stop_receivers();
        self.watching.clear();
        self.views.clear();
        self.auto.clear();
        self.recv_prev.clear();
        self.publish_watching();
    }

    fn publish_watching(&self) {
        let mut list: Vec<String> = self.watching.iter().cloned().collect();
        list.sort();
        self.set(|s| s.watching = list);
        self.publish_layers();
    }

    // ---------------------------------------------------------------- слои трансляций (свой simulcast)

    fn remote(&self, identity: &str) -> Option<RemoteParticipant> {
        let (room, _) = self.room.as_ref()?;
        room.remote_participants().into_iter().find(|(id, _)| id.as_str() == identity).map(|(_, p)| p)
    }

    /// Стример публикует экономный слой.
    fn has_low(&self, identity: &str) -> bool {
        self.remote(identity).is_some_and(|p| {
            p.track_publications().values().any(|t| t.source() == TrackSource::Screenshare && layer_of(t) == Layer::Low)
        })
    }

    /// Какой слой показывать: выбор человека, в «Авто» — по качеству приёма; нет экономного — исходный.
    fn target_layer(&self, identity: &str) -> Layer {
        if !self.has_low(identity) {
            return Layer::High;
        }
        match self.quality.get(identity).copied().unwrap_or_default() {
            Quality::High => Layer::High,
            Quality::Low => Layer::Low,
            Quality::Auto => self.auto.get(identity).map_or(Layer::High, AutoLayer::layer),
        }
    }

    fn publish_layers(&self) {
        let map: HashMap<String, LayerInfo> = self
            .watching
            .iter()
            .map(|id| {
                let info = LayerInfo {
                    quality: self.quality.get(id).copied().unwrap_or_default(),
                    has_low: self.has_low(id),
                    low: self.recv.get(id).is_some_and(|(_, _, l, _)| *l == Layer::Low),
                };
                (id.clone(), info)
            })
            .collect();
        self.set(|s| s.stream_layers = map);
    }

    /// Замер приёма просматриваемых трансляций → авто-выбор слоя.
    fn update_layers(&mut self, stats: &[&RtcStats]) {
        let now = Instant::now();
        let paused: HashSet<String> = self.state.lock().unwrap().stream_paused.keys().cloned().collect();
        // Первые секунды после подписки (SFU начинает пересылку с ключевого кадра) — всплеск потерь, не про сеть.
        let recv: Vec<(String, String, bool)> = self
            .recv
            .iter()
            .map(|(id, (_, rtc, _, since))| (id.clone(), rtc.clone(), since.elapsed() < LAYER_SETTLE))
            .collect();
        let mut seen = HashSet::new();
        let mut switched = false;
        for (id, rtc, settling) in recv {
            let Some(i) = stats.iter().find_map(|s| match s {
                RtcStats::InboundRtp(i) if i.stream.kind == "video" && i.inbound.track_identifier == rtc => Some(i),
                _ => None,
            }) else {
                continue;
            };
            let cur = (i.received.packets_lost, i.received.packets_received, i.inbound.freeze_count);
            seen.insert(rtc.clone());
            let Some(prev) = self.recv_prev.insert(rtc, cur) else { continue };
            // Скрыта (на паузе у SFU) или стример свернул окно — кадров нет не из-за сети.
            let visible = self.views.get(&id).is_none_or(|v| v.0);
            let auto = self.quality.get(&id).copied().unwrap_or_default() == Quality::Auto;
            if settling || !visible || paused.contains(&id) || !auto || !self.has_low(&id) {
                continue;
            }
            let s = layers::sample(prev, cur);
            if let Some(to) = self.auto.entry(id.clone()).or_default().update(now, s) {
                log::info!("трансляция {id}: авто → {to:?} (потери {:.1} %, заморозок {})", s.loss * 100.0, s.freezes);
                switched = true;
            }
        }
        self.recv_prev.retain(|k, _| seen.contains(k));
        if switched {
            self.sync_subscriptions();
            self.publish_layers();
        }
    }

    fn stop_receiver(&mut self, identity: &str) {
        if let Some(h) = self.receivers.remove(identity) {
            h.abort();
        }
        self.recv.remove(identity);
        self.video.remove(identity);
    }

    fn stop_receivers(&mut self) {
        for (_, h) in self.receivers.drain() {
            h.abort();
        }
        self.recv.clear();
        self.video.clear();
    }

    /// Размер на экране → SFU (adaptive stream); невидимая трансляция ставится на паузу (SFU не шлёт пакеты).
    fn apply_view(&self, identity: &str) {
        let (Some((room, _)), Some(&(visible, w, h))) = (&self.room, self.views.get(identity)) else { return };
        let Some(p) = room.remote_participants().into_iter().find(|(id, _)| id.as_str() == identity).map(|(_, p)| p) else {
            return;
        };
        for publication in p.track_publications().values() {
            if publication.source() == TrackSource::Screenshare && publication.is_subscribed() {
                if visible && w > 0 && h > 0 {
                    publication.update_video_dimensions(TrackDimension(w, h));
                }
                if publication.is_enabled() != visible {
                    log::info!("трансляция {identity}: {}", if visible { "видна — поток включён" } else { "скрыта — поток на паузе" });
                    publication.set_enabled(visible);
                }
            }
        }
    }

    /// Пересобирает список участников. `quiet` — из таймера (только уровни/говорит).
    /// id rtc-трека → подпись для оверлея: «Имя: голос / экран / звук экрана», свои — «я: …».
    fn track_labels(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        let Some((room, _)) = &self.room else { return out };
        let what = |s: TrackSource| match s {
            TrackSource::Microphone => "голос",
            TrackSource::Screenshare => "экран",
            TrackSource::ScreenshareAudio => "звук экрана",
            TrackSource::Camera => "камера",
            _ => "?",
        };
        for publication in room.local_participant().track_publications().values() {
            let id = match publication.track() {
                Some(LocalTrack::Audio(a)) => a.rtc_track().id(),
                Some(LocalTrack::Video(v)) => v.rtc_track().id(),
                None => continue,
            };
            out.insert(id, format!("я: {}", what(publication.source())));
        }
        for p in room.remote_participants().values() {
            let name = if p.name().is_empty() { p.identity().to_string() } else { p.name().to_string() };
            for publication in p.track_publications().values() {
                let id = match publication.track() {
                    Some(RemoteTrack::Audio(a)) => a.rtc_track().id(),
                    Some(RemoteTrack::Video(v)) => v.rtc_track().id(),
                    None => continue,
                };
                let low = if layer_of(publication) == Layer::Low { " (экономное)" } else { "" };
                out.insert(id, format!("{name}: {}{low}", what(publication.source())));
            }
        }
        out
    }

    fn refresh_peers(&mut self, _quiet: bool) {
        let Some((room, _)) = &self.room else { return };
        let mut peers = Vec::new();
        let lp = room.local_participant();
        peers.push(Peer {
            identity: lp.identity().as_str().to_string(),
            name: lp.name(),
            is_local: true,
            speaking: lp.is_speaking() && self.mic_should_send(),
            level: lp.audio_level(),
            mic_muted: !self.mic_should_send(),
            streaming: self.stream.is_some(),
        });
        let mut remote: Vec<Peer> = room
            .remote_participants()
            .into_values()
            .map(|p| {
                let mic_muted = p
                    .track_publications()
                    .values()
                    .find(|t| t.source() == TrackSource::Microphone)
                    .map(|t| t.is_muted())
                    .unwrap_or(true);
                let streaming = p.track_publications().values().any(|t| t.source() == TrackSource::Screenshare);
                // Без имени (боты `lk load-test`, сторонние клиенты) — показываем identity.
                let name = Some(p.name()).filter(|n| !n.is_empty()).unwrap_or_else(|| p.identity().as_str().to_string());
                Peer {
                    identity: p.identity().as_str().to_string(),
                    name,
                    is_local: false,
                    speaking: p.is_speaking(),
                    level: p.audio_level(),
                    mic_muted,
                    streaming,
                }
            })
            .collect();
        remote.sort_by_key(|a| a.name.to_lowercase());
        peers.extend(remote);
        self.last_peers = peers.iter().map(|p| (p.identity.clone(), p.name.clone())).collect();
        self.set(|s| s.peers = peers);
    }
}

/// Слой трансляции по имени трека: «screen-low» — экономный, остальное — исходный.
fn layer_of(publication: &RemoteTrackPublication) -> Layer {
    if publication.name() == stream::LOW_TRACK { Layer::Low } else { Layer::High }
}

/// Отключение, после которого не переподключаемся (текст для человека); `None` — обрыв связи, вернуться в канал.
fn final_disconnect(reason: livekit::DisconnectReason) -> Option<&'static str> {
    use livekit::DisconnectReason as R;
    match reason {
        R::ParticipantRemoved => Some("Вас отключили от канала."),
        R::RoomDeleted => Some("Канал закрыт."),
        R::DuplicateIdentity => Some("Вы зашли в канал с другого устройства."),
        _ => None,
    }
}

/// Живой исходящий видеопоток: больше всех отправил с прошлого замера; первый замер — с ненулевым размером кадра и
/// наибольшим счётчиком байт. Застывшие потоки (после переподключения) отправляют 0.
fn pick_active_outbound<'a>(outs: &[&'a OutboundRtpStats], prev: &HashMap<u32, u64>) -> Option<&'a OutboundRtpStats> {
    let delta = |o: &OutboundRtpStats| prev.get(&o.stream.ssrc).map(|p| o.sent.bytes_sent.saturating_sub(*p));
    if outs.iter().any(|o| delta(o).is_some()) {
        outs.iter().copied().max_by_key(|o| (delta(o).unwrap_or(0), o.outbound.frame_width > 0))
    } else {
        outs.iter().copied().max_by_key(|o| (o.outbound.frame_width > 0, o.sent.bytes_sent))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(ssrc: u32, bytes: u64, w: u32) -> OutboundRtpStats {
        let mut o = OutboundRtpStats::default();
        o.stream.ssrc = ssrc;
        o.sent.bytes_sent = bytes;
        o.outbound.frame_width = w;
        o
    }

    #[test]
    fn active_outbound_after_reconnect() {
        // старый поток застыл с большим счётчиком, новый растёт
        let (old, new) = (out(1, 9_000_000, 0), out(2, 50_000, 2560));
        let none = HashMap::new();
        assert_eq!(pick_active_outbound(&[&old, &new], &none).unwrap().stream.ssrc, 2, "первый замер: 0×0 не берём");
        let prev = HashMap::from([(1, 9_000_000), (2, 10_000)]);
        assert_eq!(pick_active_outbound(&[&old, &new], &prev).unwrap().stream.ssrc, 2);
        let only = out(3, 100, 1920);
        assert_eq!(pick_active_outbound(&[&only], &prev).unwrap().stream.ssrc, 3, "новый ssrc без прошлого замера");
        assert!(pick_active_outbound(&[], &prev).is_none());
    }

    #[test]
    fn removed_from_room_is_final() {
        use livekit::DisconnectReason as R;
        assert!(final_disconnect(R::ParticipantRemoved).is_some(), "кик/бан/«выкинуть из голоса» — не переподключаться");
        assert!(final_disconnect(R::RoomDeleted).is_some(), "канал удалён — не переподключаться");
        assert!(final_disconnect(R::DuplicateIdentity).is_some());
        assert!(final_disconnect(R::SignalClose).is_none(), "обрыв связи — переподключаемся");
    }
}

