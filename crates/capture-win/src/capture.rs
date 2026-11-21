//! Захват через Windows Graphics Capture → I420 (libyuv) → приёмник (NativeVideoSource).
//!
//! Два потока:
//! - поток WGC (`windows-capture`): копирует кадр в CPU, BGRA → I420 и масштабирует (libyuv),
//!   кладёт в «слот» на 1 кадр — если отправка не успевает, старый кадр перезаписывается (низкая задержка
//!   важнее плавности);
//! - поток отправки: забирает последний кадр и отдаёт приёмнику; если кадров нет (статичная картинка —
//!   WGC шлёт кадры только при изменениях), раз в секунду повторяет последний, чтобы новый зритель
//!   получил ключевой кадр; пока окно свёрнуто — шлёт заглушку.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use libwebrtc::native::yuv_helper;
use libwebrtc::video_frame::{I420Buffer, VideoBuffer};
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::{GraphicsCaptureApi, InternalCaptureControl};
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings, MinimumUpdateIntervalSettings,
    SecondaryWindowSettings, Settings,
};
use windows_capture::window::Window;

use crate::sources::{SourceId, fit};

/// Повтор последнего кадра при статичной картинке.
const REPEAT_EVERY: Duration = Duration::from_millis(1000);
/// Сколько без новых кадров, прежде чем проверять, не перекрыто ли окно.
pub const STALL: Duration = Duration::from_millis(2000);
/// Как часто проверять перекрытие (геометрия окон — дёшево, но не на каждом кадре).
pub const OCCLUSION_CHECK: Duration = Duration::from_millis(500);
const POLL: Duration = Duration::from_millis(100);
const MINIMIZED_CHECK: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureConfig {
    /// Предел разрешения (вписываем с сохранением пропорций); 0 — исходное.
    pub max_width: u32,
    pub max_height: u32,
    pub fps: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Первый кадр: размер отправляемого видео.
    Started { width: u32, height: u32 },
    /// Размер отправляемого видео изменился (окно изменили).
    Resized { width: u32, height: u32 },
    Minimized(bool),
    /// Окно не свёрнуто, но новых кадров нет дольше [`STALL`] и оно целиком закрыто другими окнами: браузеры (и не только)
    /// перестают рисовать перекрытые окна — у зрителей стоп-кадр. false — кадры пошли снова.
    Hidden(bool),
    /// Аппаратный путь: размер источника устойчиво изменился, энкодер (фиксированного размера) надо пересоздать
    /// под новый размер отправляемого видео.
    SizeChanged { width: u32, height: u32 },
    /// Окно закрыто — захват остановлен.
    Closed,
    Error(String),
}

/// Получатель кадров. Вызывается из потока отправки.
pub trait Sink: Send + Sync + 'static {
    fn frame(&self, frame: &I420Buffer, timestamp_us: i64);
    fn event(&self, ev: Event);
}

struct Slot {
    latest: Option<I420Buffer>,
    seq: u64,
}

struct Shared {
    slot: Mutex<Slot>,
    cv: Condvar,
    stop: AtomicBool,
    closed: AtomicBool,
    cfg: CaptureConfig,
    sink: Arc<dyn Sink>,
}

impl Shared {
    fn wake(&self) {
        self.cv.notify_all();
    }
}

/// Монотонные микросекунды (для timestamp кадров).
fn now_us() -> i64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_micros() as i64
}

/// Идущий захват. Останавливается в `stop()` или при уничтожении.
pub struct Capture {
    source: SourceId,
    shared: Arc<Shared>,
    control: Option<CaptureControl<Handler, String>>,
    pump: Option<JoinHandle<()>>,
}

impl Capture {
    pub fn start(source: SourceId, cfg: CaptureConfig, sink: Arc<dyn Sink>) -> Result<Capture, String> {
        if !GraphicsCaptureApi::is_supported().unwrap_or(false) {
            return Err("Захват экрана не поддерживается этой версией Windows (нужна Windows 10 1903+)".into());
        }
        let shared = Arc::new(Shared {
            slot: Mutex::new(Slot { latest: None, seq: 0 }),
            cv: Condvar::new(),
            stop: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            cfg,
            sink,
        });
        let fps = cfg.fps.clamp(1, 120);
        let interval = if GraphicsCaptureApi::is_minimum_update_interval_supported().unwrap_or(false) {
            // Чуть меньше периода: иначе джиттер композитора срезает часть кадров.
            MinimumUpdateIntervalSettings::Custom(Duration::from_micros(1_000_000 / fps as u64 * 9 / 10))
        } else {
            MinimumUpdateIntervalSettings::Default
        };
        let cursor = if GraphicsCaptureApi::is_cursor_settings_supported().unwrap_or(false) {
            CursorCaptureSettings::WithCursor
        } else {
            CursorCaptureSettings::Default
        };
        let border = if GraphicsCaptureApi::is_border_settings_supported().unwrap_or(false) {
            DrawBorderSettings::WithoutBorder
        } else {
            DrawBorderSettings::Default
        };
        let control = match source {
            SourceId::Window(h) => {
                let w = Window::from_raw_hwnd(h as *mut std::ffi::c_void);
                let settings = Settings::new(
                    w,
                    cursor,
                    border,
                    SecondaryWindowSettings::Default,
                    interval,
                    DirtyRegionSettings::Default,
                    ColorFormat::Bgra8,
                    shared.clone(),
                );
                Handler::start_free_threaded(settings)
            }
            SourceId::Monitor(h) => {
                let m = Monitor::from_raw_hmonitor(h as *mut std::ffi::c_void);
                let settings = Settings::new(
                    m,
                    cursor,
                    border,
                    SecondaryWindowSettings::Default,
                    interval,
                    DirtyRegionSettings::Default,
                    ColorFormat::Bgra8,
                    shared.clone(),
                );
                Handler::start_free_threaded(settings)
            }
        }
        .map_err(|e| format!("Не удалось начать захват: {e}"))?;

        let pump = {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("capture-pump".into())
                .spawn(move || pump(shared, source))
                .map_err(|e| e.to_string())?
        };
        log::info!("захват {source:?}: старт, {cfg:?}");
        Ok(Capture { source, shared, control: Some(control), pump: Some(pump) })
    }

    pub fn source(&self) -> SourceId {
        self.source
    }

    pub fn is_finished(&self) -> bool {
        self.shared.closed.load(Ordering::Relaxed) || self.control.as_ref().is_none_or(|c| c.is_finished())
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.wake();
        if let Some(c) = self.control.take()
            && let Err(e) = c.stop() {
                log::warn!("остановка захвата: {e}");
            }
        if let Some(p) = self.pump.take() {
            let _ = p.join();
        }
        log::info!("захват {:?}: стоп", self.source);
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if self.control.is_some() || self.pump.is_some() {
            self.shutdown();
        }
    }
}

// ------------------------------------------------------------------ поток WGC

struct Handler {
    shared: Arc<Shared>,
    min_interval: Duration,
    last: Option<Instant>,
    frames: u64,
    convert_total: Duration,
    stat_at: Instant,
}

impl GraphicsCaptureApiHandler for Handler {
    type Flags = Arc<Shared>;
    type Error = String;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let fps = ctx.flags.cfg.fps.clamp(1, 120);
        Ok(Handler {
            min_interval: Duration::from_micros(1_000_000 / fps as u64 * 85 / 100),
            shared: ctx.flags,
            last: None,
            frames: 0,
            convert_total: Duration::ZERO,
            stat_at: Instant::now(),
        })
    }

    fn on_frame_arrived(&mut self, frame: &mut Frame, control: InternalCaptureControl) -> Result<(), Self::Error> {
        if self.shared.stop.load(Ordering::Relaxed) {
            control.stop();
            return Ok(());
        }
        // Ограничение fps (если ОС не умеет MinUpdateInterval — режем здесь).
        if self.last.is_some_and(|t| t.elapsed() < self.min_interval) {
            return Ok(());
        }
        self.last = Some(Instant::now());
        let t0 = Instant::now();
        let buf = match convert(frame, &self.shared.cfg) {
            Ok(b) => b,
            Err(e) => {
                log::warn!("кадр пропущен: {e}");
                return Ok(());
            }
        };
        self.convert_total += t0.elapsed();
        self.frames += 1;
        if self.stat_at.elapsed() >= Duration::from_secs(10) {
            let secs = self.stat_at.elapsed().as_secs_f64();
            log::info!(
                "захват: {:.1} к/с, {}×{}, конвертация {:.2} мс/кадр",
                self.frames as f64 / secs,
                buf.width(),
                buf.height(),
                self.convert_total.as_secs_f64() * 1000.0 / self.frames.max(1) as f64
            );
            self.frames = 0;
            self.convert_total = Duration::ZERO;
            self.stat_at = Instant::now();
        }
        {
            let mut slot = self.shared.slot.lock().unwrap();
            slot.latest = Some(buf); // старый неотправленный кадр выбрасывается
            slot.seq += 1;
        }
        self.shared.wake();
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        log::info!("захват: источник закрыт");
        self.shared.closed.store(true, Ordering::Relaxed);
        self.shared.wake();
        Ok(())
    }
}

/// Кадр WGC (BGRA, в памяти GPU) → I420 нужного размера.
fn convert(frame: &mut Frame, cfg: &CaptureConfig) -> Result<I420Buffer, String> {
    let mut fb = frame.buffer().map_err(|e| e.to_string())?;
    // I420 — чётные размеры (лишний столбец/строку отрезаем).
    let (w, h) = (fb.width() & !1, fb.height() & !1);
    if w < 2 || h < 2 {
        return Err(format!("пустой кадр {}×{}", fb.width(), fb.height()));
    }
    let pitch = fb.row_pitch();
    let raw = fb.as_raw_buffer();
    let mut full = I420Buffer::new(w, h);
    let (sy, su, sv) = full.strides();
    let (y, u, v) = full.data_mut();
    // libyuv «ARGB» = порядок байт B,G,R,A в памяти — это и есть BGRA8.
    yuv_helper::argb_to_i420(raw, pitch, y, sy, u, su, v, sv, w as i32, h as i32);
    let (tw, th) = target_size(w, h, cfg);
    if (tw, th) == (w, h) { Ok(full) } else { Ok(full.scale(tw as i32, th as i32)) }
}

/// Размер отправляемого видео: вписать в предел пресета, чётные стороны, не увеличивать.
pub fn target_size(w: u32, h: u32, cfg: &CaptureConfig) -> (u32, u32) {
    if cfg.max_width == 0 || cfg.max_height == 0 || (w <= cfg.max_width && h <= cfg.max_height) {
        return (w & !1, h & !1);
    }
    let (tw, th) = fit(w, h, cfg.max_width, cfg.max_height);
    ((tw & !1).max(2), (th & !1).max(2))
}

// ------------------------------------------------------------------ поток отправки

/// Тёмная заглушка (цвет фона клиента) — пока окно свёрнуто.
fn placeholder(w: u32, h: u32) -> I420Buffer {
    let mut b = I420Buffer::new(w.max(2) & !1, h.max(2) & !1);
    let (y, u, v) = b.data_mut();
    y.fill(24);
    u.fill(126);
    v.fill(131);
    b
}

fn pump(shared: Arc<Shared>, source: SourceId) {
    let mut last: Option<I420Buffer> = None;
    let mut last_seq = 0u64;
    let mut last_sent = Instant::now();
    let mut size = (0u32, 0u32);
    let mut minimized = false;
    let mut min_check = Instant::now();
    let mut closed_sent = false;
    let mut last_fresh = Instant::now();
    let mut hidden = false;
    let mut occl_check = Instant::now();
    let (mut diag_at, mut diag_live, mut diag_rep) = (Instant::now(), 0u32, 0u32);
    let mut screen = crate::diag::ScreenWatch::default();
    loop {
        let fresh = {
            let mut slot = shared.slot.lock().unwrap();
            if slot.seq == last_seq && !shared.stop.load(Ordering::Relaxed) && !shared.closed.load(Ordering::Relaxed) {
                slot = shared.cv.wait_timeout(slot, POLL).unwrap().0;
            }
            if slot.seq != last_seq {
                last_seq = slot.seq;
                slot.latest.take()
            } else {
                None
            }
        };
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        if shared.closed.load(Ordering::Relaxed) || (source.is_window() && !source.is_alive()) {
            if !closed_sent {
                closed_sent = true;
                shared.sink.event(Event::Closed);
            }
            // Ждём, пока владелец остановит захват.
            std::thread::sleep(POLL);
            continue;
        }
        if source.is_window() && min_check.elapsed() >= MINIMIZED_CHECK {
            min_check = Instant::now();
            let now_min = source.is_minimized();
            if now_min != minimized {
                minimized = now_min;
                shared.sink.event(Event::Minimized(minimized));
                if minimized && size.0 > 0 {
                    last = Some(placeholder(size.0, size.1));
                    last_sent = Instant::now() - REPEAT_EVERY; // отправить сразу
                }
            }
        }
        if source.is_window() && occl_check.elapsed() >= OCCLUSION_CHECK {
            occl_check = Instant::now();
            let h = !minimized && last_fresh.elapsed() >= STALL && source.is_occluded();
            if h != hidden {
                hidden = h;
                log::info!("захват: окно {}", if h { "перекрыто — новых кадров нет" } else { "снова присылает кадры" });
                shared.sink.event(Event::Hidden(h));
            }
        }
        if crate::diag::on() && diag_at.elapsed() >= Duration::from_secs(1) {
            diag_at = Instant::now();
            let (occ, chg) = match source {
                SourceId::Window(h) => (if source.is_occluded() { "да" } else { "нет" }, screen.changed(h)),
                SourceId::Monitor(_) => ("—", "—"),
            };
            log::info!(
                "ДИАГ захват (процессор): живых {diag_live}, повторов {diag_rep}, без кадров {} мс, свёрнуто {}, перекрыто {occ}, экран под окном меняется {chg}, в фокусе «{}»",
                last_fresh.elapsed().as_millis(),
                if minimized { "да" } else { "нет" },
                crate::diag::foreground_title()
            );
            (diag_live, diag_rep) = (0, 0);
        }
        if let Some(buf) = fresh
            && !minimized {
                let gap = last_fresh.elapsed();
                if crate::diag::on() && size != (0, 0) && gap >= Duration::from_millis(400) {
                    log::info!("ДИАГ захват: живых кадров не было {} мс", gap.as_millis());
                }
                diag_live += 1;
                last_fresh = Instant::now();
                let s = (buf.width(), buf.height());
                if size == (0, 0) {
                    shared.sink.event(Event::Started { width: s.0, height: s.1 });
                } else if s != size {
                    shared.sink.event(Event::Resized { width: s.0, height: s.1 });
                }
                size = s;
                shared.sink.frame(&buf, now_us());
                last = Some(buf);
                last_sent = Instant::now();
                continue;
            }
        if last_sent.elapsed() >= REPEAT_EVERY
            && let Some(buf) = &last {
                shared.sink.frame(buf, now_us());
                diag_rep += 1;
                last_sent = Instant::now();
            }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_size_is_idempotent() {
        // пересоздание энкодера берёт размер из SizeChanged (уже target_size) — второй проход не должен его менять
        let cfgs = [
            CaptureConfig { max_width: 1920, max_height: 1080, fps: 30 },
            CaptureConfig { max_width: 1280, max_height: 720, fps: 30 },
            CaptureConfig { max_width: 0, max_height: 0, fps: 60 },
        ];
        for cfg in &cfgs {
            for (w, h) in [(1920, 1050), (1920, 1054), (2560, 1392), (3440, 1440), (1365, 767), (801, 3001), (17, 17)] {
                let t = target_size(w, h, cfg);
                assert_eq!(target_size(t.0, t.1, cfg), t, "{w}×{h} {cfg:?}");
            }
        }
    }

    #[test]
    fn target_size_fits_and_is_even() {
        let p1080 = CaptureConfig { max_width: 1920, max_height: 1080, fps: 60 };
        assert_eq!(target_size(2560, 1440, &p1080), (1920, 1080));
        assert_eq!(target_size(1280, 720, &p1080), (1280, 720)); // не увеличиваем
        assert_eq!(target_size(3440, 1440, &p1080), (1920, 804)); // ультраширокий
        let native = CaptureConfig { max_width: 0, max_height: 0, fps: 60 };
        assert_eq!(target_size(1365, 767, &native), (1364, 766));
    }

    #[test]
    fn placeholder_is_even() {
        let b = placeholder(1365, 767);
        assert_eq!((b.width(), b.height()), (1364, 766));
    }
}
