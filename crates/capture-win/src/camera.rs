//! Камера (Media Foundation): список устройств и захват кадров → I420 для WebRTC.
//!
//! Видеопроцессор source reader'а сам переводит MJPEG/YUY2/NV12 в RGB32; режим камеры выбираем сами
//! (самый большой не выше 1280×720 с частотой от 15 кадров/с, при равенстве — ближе к 30).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Instant;

use libwebrtc::native::yuv_helper;
use libwebrtc::video_frame::I420Buffer;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize};
use windows::core::{Interface, PWSTR};

use crate::capture::{Event, Sink};

const MF_VERSION: u32 = 0x0002_0070;
const VIDEO: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
/// Предел режима камеры (ширина, высота) и частоты.
pub const MAX_W: u32 = 1280;
pub const MAX_H: u32 = 720;
const MIN_FPS: f32 = 15.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraInfo {
    /// Символьная ссылка устройства (стабильна между запусками).
    pub id: String,
    pub name: String,
}

/// Режим камеры: ширина, высота, кадров/с.
pub type Mode = (u32, u32, f32);

/// Лучший режим: влезает в 1280×720, частота ≥ 15; больше площадь — лучше, затем частота ближе к 30.
pub fn pick_mode(modes: &[Mode]) -> Option<usize> {
    modes
        .iter()
        .enumerate()
        .filter(|(_, (w, h, f))| *w <= MAX_W && *h <= MAX_H && *w >= 2 && *h >= 2 && *f >= MIN_FPS)
        .max_by(|(_, a), (_, b)| {
            let (aa, ba) = (a.0 as u64 * a.1 as u64, b.0 as u64 * b.1 as u64);
            aa.cmp(&ba).then_with(|| (30.0 - b.2).abs().total_cmp(&(30.0 - a.2).abs()))
        })
        .map(|(i, _)| i)
}

/// MF + COM на время работы потока.
struct Mf {
    com: bool,
}
impl Mf {
    fn start() -> Result<Self, String> {
        let com = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        if let Err(e) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) } {
            if com {
                unsafe { CoUninitialize() };
            }
            return Err(format!("Media Foundation: {e}"));
        }
        Ok(Mf { com })
    }
}
impl Drop for Mf {
    fn drop(&mut self) {
        let _ = unsafe { MFShutdown() };
        if self.com {
            unsafe { CoUninitialize() };
        }
    }
}

fn string_attr(a: &IMFActivate, key: &windows::core::GUID) -> Option<String> {
    let (mut p, mut len) = (PWSTR::null(), 0u32);
    unsafe { a.GetAllocatedString(key, &mut p, &mut len) }.ok()?;
    let s = unsafe { p.to_string() }.ok();
    unsafe { CoTaskMemFree(Some(p.0 as *const _)) };
    s
}

fn devices() -> Result<Vec<(CameraInfo, IMFActivate)>, String> {
    unsafe {
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 1).map_err(|e| e.to_string())?;
        let attrs = attrs.ok_or("MFCreateAttributes")?;
        attrs.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID).map_err(|e| e.to_string())?;
        let mut arr: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut n = 0u32;
        MFEnumDeviceSources(&attrs, &mut arr, &mut n).map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for i in 0..n as usize {
            if let Some(a) = (*arr.add(i)).take() {
                let id = string_attr(&a, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_SYMBOLIC_LINK).unwrap_or_default();
                let name = string_attr(&a, &MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME).unwrap_or_else(|| "Камера".into());
                if !id.is_empty() {
                    out.push((CameraInfo { id, name }, a));
                }
            }
        }
        CoTaskMemFree(Some(arr as *const _));
        Ok(out)
    }
}

/// Подключённые камеры. Ошибка Windows → пустой список (в лог).
pub fn list_cameras() -> Vec<CameraInfo> {
    let Ok(_mf) = Mf::start() else { return Vec::new() };
    match devices() {
        Ok(v) => v.into_iter().map(|(i, _)| i).collect(),
        Err(e) => {
            log::warn!("камеры: {e}");
            Vec::new()
        }
    }
}

/// Работающий захват; Drop останавливает поток (камера гаснет).
pub struct Camera {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Camera {
    /// `id` — `CameraInfo::id`; None — первая камера. Кадры и события идут в `sink` из потока захвата.
    pub fn start(id: Option<&str>, sink: Arc<dyn Sink>) -> Result<Camera, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let (st, id) = (stop.clone(), id.map(str::to_string));
        let thread = std::thread::Builder::new()
            .name("camera".into())
            .spawn(move || {
                let res = run(id.as_deref(), &st, &sink, &tx);
                if let Err(e) = res {
                    let _ = tx.send(Err(e.clone()));
                    sink.event(Event::Error(e));
                } else {
                    sink.event(Event::Closed);
                }
            })
            .map_err(|e| e.to_string())?;
        // Ждём «открылась / не открылась», чтобы вернуть ошибку сразу.
        match rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(Ok(())) => Ok(Camera { stop, thread: Some(thread) }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                Err("камера не отвечает".into())
            }
        }
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

const BUSY: &str = "Камера занята другой программой или недоступна — закройте её там и попробуйте снова.";
const DENIED: &str = "Windows не даёт доступ к камере: Параметры → Конфиденциальность → Камера → разрешить классическим приложениям.";

fn open_err(e: windows::core::Error) -> String {
    log::info!("камера: {e}");
    // E_ACCESSDENIED
    if e.code().0 as u32 == 0x8007_0005 { DENIED.into() } else { BUSY.into() }
}

fn run(id: Option<&str>, stop: &AtomicBool, sink: &Arc<dyn Sink>, ready: &std::sync::mpsc::Sender<Result<(), String>>) -> Result<(), String> {
    let _mf = Mf::start()?;
    let list = devices()?;
    let (info, act) = match id {
        Some(id) => list.into_iter().find(|(i, _)| i.id == id),
        None => list.into_iter().next(),
    }
    .ok_or("Камера не найдена — возможно, её отключили.")?;
    log::info!("камера: «{}»", info.name);
    unsafe {
        let source: IMFMediaSource = act.ActivateObject().map_err(open_err)?;
        let result = read_loop(&source, stop, sink, ready);
        let _ = source.Shutdown();
        result
    }
}

unsafe fn read_loop(
    source: &IMFMediaSource,
    stop: &AtomicBool,
    sink: &Arc<dyn Sink>,
    ready: &std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    unsafe {
        let mut attrs: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs, 1).map_err(open_err)?;
        let attrs = attrs.ok_or(BUSY)?;
        attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1).map_err(open_err)?;
        let r = MFCreateSourceReaderFromMediaSource(source, &attrs).map_err(open_err)?;
        r.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false).map_err(open_err)?;
        r.SetStreamSelection(VIDEO, true).map_err(open_err)?;
        // режимы камеры
        let (mut types, mut modes) = (Vec::new(), Vec::new());
        let mut i = 0;
        while let Ok(t) = r.GetNativeMediaType(VIDEO, i) {
            if let Ok(size) = t.GetUINT64(&MF_MT_FRAME_SIZE) {
                let fps = t.GetUINT64(&MF_MT_FRAME_RATE).map(|v| (v >> 32) as f32 / (v as u32).max(1) as f32).unwrap_or(30.0);
                modes.push(((size >> 32) as u32, size as u32, fps));
                types.push(t);
            }
            i += 1;
        }
        if let Some(k) = pick_mode(&modes) {
            r.SetCurrentMediaType(VIDEO, None, &types[k]).map_err(open_err)?;
        }
        let t = MFCreateMediaType().map_err(open_err)?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).map_err(open_err)?;
        t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32).map_err(open_err)?;
        r.SetCurrentMediaType(VIDEO, None, &t).map_err(open_err)?;
        let cur = r.GetCurrentMediaType(VIDEO).map_err(open_err)?;
        let size = cur.GetUINT64(&MF_MT_FRAME_SIZE).map_err(open_err)?;
        let (w, h) = (((size >> 32) as u32) & !1, (size as u32) & !1);
        if w < 2 || h < 2 {
            return Err(BUSY.into());
        }
        let _ = ready.send(Ok(()));
        sink.event(Event::Started { width: w, height: h });
        let t0 = Instant::now();
        let mut bgra = vec![0u8; w as usize * h as usize * 4];
        while !stop.load(Ordering::Relaxed) {
            let (mut flags, mut ts) = (0u32, 0i64);
            let mut sample: Option<IMFSample> = None;
            r.ReadSample(VIDEO, 0, None, Some(&mut flags), Some(&mut ts), Some(&mut sample)).map_err(|e| {
                log::info!("камера ReadSample: {e}");
                "Камера перестала отвечать — возможно, её отключили.".to_string()
            })?;
            if flags & (MF_SOURCE_READERF_ERROR.0 as u32 | MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
                return Err("Камера перестала отвечать — возможно, её отключили.".into());
            }
            let Some(s) = sample else { continue };
            if copy_bgra(&s, w as usize, h as usize, &mut bgra).is_err() {
                continue;
            }
            let mut buf = I420Buffer::new(w, h);
            let (sy, su, sv) = buf.strides();
            let (y, u, v) = buf.data_mut();
            yuv_helper::argb_to_i420(&bgra, w * 4, y, sy, u, su, v, sv, w as i32, h as i32);
            sink.frame(&buf, t0.elapsed().as_micros() as i64);
        }
        Ok(())
    }
}

/// Кадр RGB32 → плотный BGRA сверху вниз (учитывает шаг строки, в т.ч. «снизу вверх»).
unsafe fn copy_bgra(s: &IMFSample, w: usize, h: usize, out: &mut [u8]) -> Result<(), ()> {
    unsafe {
        let buf = s.ConvertToContiguousBuffer().map_err(|_| ())?;
        let row = w * 4;
        let mut copy = |first: *const u8, pitch: isize| {
            for y in 0..h {
                let src = first.offset(y as isize * pitch);
                std::ptr::copy_nonoverlapping(src, out.as_mut_ptr().add(y * row), row);
            }
        };
        if let Ok(b2) = buf.cast::<IMF2DBuffer>() {
            let (mut p, mut pitch) = (std::ptr::null_mut(), 0i32);
            if b2.Lock2D(&mut p, &mut pitch).is_ok() {
                if (pitch.unsigned_abs() as usize) < row {
                    let _ = b2.Unlock2D();
                    return Err(());
                }
                copy(p, pitch as isize);
                let _ = b2.Unlock2D();
                return Ok(());
            }
        }
        let (mut p, mut len) = (std::ptr::null_mut(), 0u32);
        buf.Lock(&mut p, None, Some(&mut len)).map_err(|_| ())?;
        let ok = len as usize >= row * h;
        if ok {
            copy(p, row as isize);
        }
        let _ = buf.Unlock();
        if ok { Ok(()) } else { Err(()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_largest_mode_within_limit() {
        let m = [(640, 480, 30.0), (1920, 1080, 30.0), (1280, 720, 30.0), (1280, 720, 10.0)];
        assert_eq!(pick_mode(&m), Some(2));
    }

    #[test]
    fn prefers_rate_near_30() {
        let m = [(1280, 720, 60.0), (1280, 720, 30.0), (1280, 720, 15.0)];
        assert_eq!(pick_mode(&m), Some(1));
    }

    #[test]
    fn no_mode_when_all_too_slow_or_big() {
        assert_eq!(pick_mode(&[(1920, 1080, 30.0), (640, 480, 5.0)]), None);
    }

    #[test]
    fn listing_cameras_does_not_panic() {
        let _ = list_cameras();
    }
}
