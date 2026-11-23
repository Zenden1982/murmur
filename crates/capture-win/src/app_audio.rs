//! Звук трансляции (фаза 5): WASAPI process loopback.
//!
//! - Окно → дерево процессов его приложения (корневой процесс с тем же exe: у Chrome/Edge/Firefox/Electron
//!   звук играет дочерний процесс), режим INCLUDE.
//! - Экран → весь звук системы, кроме нашего клиента (режим EXCLUDE для своего PID): голоса друзей,
//!   которые играет Murmur, в трансляцию не попадают — у них не будет эха самих себя.
//!
//! Формат: 48 кГц, стерео, float32 (Windows сама конвертирует), событийный захват в своём потоке
//! с приоритетом «Pro Audio». Выход — кадры по 10 мс (480 × 2, i16). Если приложение молчит, WASAPI
//! пакетов не даёт — тогда по часам досылается тишина, чтобы трек не «замерзал».
//! Требуется Windows 10 2004 (сборка 19041) и новее.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, LPARAM, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient,
    PROCESS_LOOPBACK_MODE, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
    VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX,
};
use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{BLOB, COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW, GetCurrentProcessId, SetEvent, WaitForSingleObject,
};
use windows::Win32::System::Variant::VT_BLOB;
use windows::Win32::UI::WindowsAndMessaging::{EnumChildWindows, GetWindowThreadProcessId};
use windows::core::{BOOL, HRESULT, Interface, Ref, implement, w};

use crate::sources::SourceId;

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;
/// Сэмплов на канал в кадре 10 мс.
pub const FRAME: usize = (SAMPLE_RATE / 100) as usize;
const FRAME_LEN: usize = FRAME * CHANNELS as usize;
const FRAME_DUR: Duration = Duration::from_millis(10);
/// Нет пакетов дольше этого — досылаем тишину.
const GAP_FRAMES: u64 = 5;

/// Что захватываем.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioTarget {
    /// Дерево процессов приложения (корневой PID).
    App(u32),
    /// Весь звук, кроме дерева процессов с этим PID (наш клиент).
    AllExcept(u32),
}

impl AudioTarget {
    /// Цель для источника трансляции: окно — его приложение, экран — всё, кроме нас.
    pub fn for_source(id: SourceId) -> Option<AudioTarget> {
        match id {
            SourceId::Window(h) => window_app_pid(h).map(AudioTarget::App),
            SourceId::Monitor(_) => Some(AudioTarget::AllExcept(unsafe { GetCurrentProcessId() })),
        }
    }
}

/// Приёмник звука: кадры по 10 мс, 48 кГц, стерео, i16 вперемешку (L R L R …).
pub trait AudioSink: Send + Sync + 'static {
    fn frame(&self, data: &[i16]);
}

/// Идущий захват звука; остановка — [`AppAudio::stop`] (или drop).
pub struct AppAudio {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pub target: AudioTarget,
}

impl AppAudio {
    /// Запускает захват. Ошибка — если ОС не умеет process loopback или процесс недоступен.
    pub fn start(target: AudioTarget, sink: Arc<dyn AudioSink>) -> Result<AppAudio, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let st = stop.clone();
        let thread = std::thread::Builder::new()
            .name("app-audio".into())
            .spawn(move || audio_thread(target, sink, st, tx))
            .map_err(|e| format!("поток звука: {e}"))?;
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(AppAudio { stop, thread: Some(thread), target }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                Err("захват звука не запустился за 5 с".into())
            }
        }
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for AppAudio {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ------------------------------------------------------------------ нарезка и темп

/// f32 [-1, 1] → i16 с насыщением.
pub fn f32_to_i16(x: f32) -> i16 {
    (x.clamp(-1.0, 1.0) * 32767.0).round() as i16
}

/// Копит сэмплы и отдаёт ровные кадры по 10 мс.
#[derive(Default)]
pub struct Chunker {
    buf: Vec<i16>,
}

impl Chunker {
    /// Сэмплы вперемешку (L R …); возвращает число отданных кадров.
    pub fn push_f32(&mut self, samples: &[f32], out: &mut dyn FnMut(&[i16])) -> u64 {
        let mut n = 0;
        for &s in samples {
            self.buf.push(f32_to_i16(s));
            if self.buf.len() == FRAME_LEN {
                out(&self.buf);
                self.buf.clear();
                n += 1;
            }
        }
        n
    }

    /// Тишина: `frames` сэмплов на канал.
    pub fn push_silence(&mut self, frames: usize, out: &mut dyn FnMut(&[i16])) -> u64 {
        let mut n = 0;
        let mut left = frames * CHANNELS as usize;
        while left > 0 {
            let take = left.min(FRAME_LEN - self.buf.len());
            self.buf.resize(self.buf.len() + take, 0);
            left -= take;
            if self.buf.len() == FRAME_LEN {
                out(&self.buf);
                self.buf.clear();
                n += 1;
            }
        }
        n
    }

    /// Недобранный кадр дополняется тишиной и отдаётся (перед досылкой тишины по таймеру).
    pub fn flush_padded(&mut self, out: &mut dyn FnMut(&[i16])) -> u64 {
        if self.buf.is_empty() {
            return 0;
        }
        self.buf.resize(FRAME_LEN, 0);
        out(&self.buf);
        self.buf.clear();
        1
    }
}

/// Темп: сколько кадров должно было уйти к этому моменту. Пока идут данные — часы подстраиваются
/// под устройство (без дрейфа и вставок); пропали дольше `GAP_FRAMES` — досылаем тишину.
pub struct Pacer {
    t0: Instant,
    sent: u64,
}

impl Pacer {
    pub fn new(now: Instant) -> Self {
        Pacer { t0: now, sent: 0 }
    }

    /// Ушли настоящие кадры.
    pub fn on_data(&mut self, frames: u64, now: Instant) {
        self.sent += frames;
        self.t0 = now.checked_sub(FRAME_DUR * self.sent as u32).unwrap_or(now);
    }

    /// Сколько кадров тишины дослать сейчас (0 — не надо).
    pub fn silence_due(&mut self, now: Instant) -> u64 {
        let expected = (now.saturating_duration_since(self.t0).as_micros() / FRAME_DUR.as_micros()) as u64;
        if expected >= self.sent + GAP_FRAMES {
            let n = expected - self.sent;
            self.sent = expected;
            n
        } else {
            0
        }
    }
}

// ------------------------------------------------------------------ процессы

#[derive(Debug, Clone)]
pub struct ProcInfo {
    pub pid: u32,
    pub parent: u32,
    pub exe: String,
}

/// Корневой процесс приложения: поднимаемся к родителю, пока у него тот же exe.
pub fn app_root(pid: u32, procs: &[ProcInfo]) -> u32 {
    let find = |p: u32| procs.iter().find(|x| x.pid == p);
    let mut cur = pid;
    for _ in 0..64 {
        let Some(me) = find(cur) else { break };
        match find(me.parent) {
            Some(p) if p.pid != cur && p.exe.eq_ignore_ascii_case(&me.exe) => cur = p.pid,
            _ => break,
        }
    }
    cur
}

fn process_list() -> Vec<ProcInfo> {
    let mut out = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return out };
        let mut e = PROCESSENTRY32W { dwSize: size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            out.push(ProcInfo { pid: e.th32ProcessID, parent: e.th32ParentProcessID, exe: String::from_utf16_lossy(&e.szExeFile[..len]) });
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    out
}

fn window_pid(hwnd: HWND) -> u32 {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    pid
}

/// PID приложения окна (корень дерева процессов с тем же exe).
pub fn window_app_pid(hwnd: isize) -> Option<u32> {
    let h = HWND(hwnd as _);
    let mut pid = window_pid(h);
    if pid == 0 {
        return None;
    }
    let procs = process_list();
    // UWP: окно принадлежит ApplicationFrameHost, само приложение — в дочернем окне другого процесса.
    if procs.iter().any(|p| p.pid == pid && p.exe.eq_ignore_ascii_case("ApplicationFrameHost.exe")) {
        struct Find {
            host: u32,
            found: u32,
        }
        unsafe extern "system" fn cb(child: HWND, lp: LPARAM) -> BOOL {
            let f = unsafe { &mut *(lp.0 as *mut Find) };
            let p = window_pid(child);
            if p != 0 && p != f.host {
                f.found = p;
                return BOOL(0);
            }
            BOOL(1)
        }
        let mut f = Find { host: pid, found: 0 };
        unsafe {
            let _ = EnumChildWindows(Some(h), Some(cb), LPARAM(&mut f as *mut Find as isize));
        }
        if f.found != 0 {
            pid = f.found;
        }
    }
    Some(app_root(pid, &procs))
}

// ------------------------------------------------------------------ WASAPI

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated(HANDLE);

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(&self, _op: Ref<IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
        unsafe { SetEvent(self.0) }
    }
}

struct Event(HANDLE);

impl Event {
    fn new() -> Result<Event, String> {
        unsafe { CreateEventW(None, false, false, None) }.map(Event).map_err(|e| format!("CreateEvent: {e}"))
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn activate(target: AudioTarget) -> Result<IAudioClient, String> {
    let (pid, mode): (u32, PROCESS_LOOPBACK_MODE) = match target {
        AudioTarget::App(p) => (p, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE),
        AudioTarget::AllExcept(p) => (p, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE),
    };
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS { TargetProcessId: pid, ProcessLoopbackMode: mode },
        },
    };
    // PROPVARIANT(VT_BLOB) указывает на params на стеке — не очищать (PropVariantClear освободил бы чужую память).
    let mut pv = std::mem::ManuallyDrop::new(PROPVARIANT::default());
    unsafe {
        let inner = &mut *pv.Anonymous.Anonymous;
        inner.vt = VT_BLOB;
        inner.Anonymous.blob = BLOB {
            cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            pBlobData: &params as *const _ as *mut u8,
        };
    }
    let done = Event::new()?;
    let handler: IActivateAudioInterfaceCompletionHandler = Activated(done.0).into();
    unsafe {
        let op = ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&*pv as *const PROPVARIANT), &handler)
            .map_err(|e| format!("ActivateAudioInterfaceAsync: {e}"))?;
        if WaitForSingleObject(done.0, 5000) != WAIT_OBJECT_0 {
            // Обработчик ещё может сигналить событие позже: дескриптор не закрываем (иначе SetEvent попадёт
            // в закрытый или переиспользованный дескриптор). Утечка одного события безопаснее.
            std::mem::forget(done);
            return Err("активация захвата звука не завершилась за 5 с".into());
        }
        let mut hr = HRESULT(0);
        let mut unk = None;
        op.GetActivateResult(&mut hr, &mut unk).map_err(|e| format!("GetActivateResult: {e}"))?;
        hr.ok().map_err(|e| format!("активация захвата звука: {e}"))?;
        unk.ok_or("активация захвата звука: пустой интерфейс")?.cast::<IAudioClient>().map_err(|e| format!("IAudioClient: {e}"))
    }
}

struct Session {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: Event,
}

fn open(target: AudioTarget) -> Result<Session, String> {
    let client = activate(target)?;
    let block = (CHANNELS * 4) as u16;
    let fmt = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
        nChannels: CHANNELS as u16,
        nSamplesPerSec: SAMPLE_RATE,
        nAvgBytesPerSec: SAMPLE_RATE * block as u32,
        nBlockAlign: block,
        wBitsPerSample: 32,
        cbSize: 0,
    };
    let flags = AUDCLNT_STREAMFLAGS_LOOPBACK
        | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
        | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
        | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
    unsafe {
        // Буфер 100 мс — запас на случай задержки потока; задержку не добавляет (читаем по событию).
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 1_000_000, 0, &fmt, None).map_err(|e| format!("IAudioClient::Initialize: {e}"))?;
        let event = Event::new()?;
        client.SetEventHandle(event.0).map_err(|e| format!("SetEventHandle: {e}"))?;
        let capture: IAudioCaptureClient = client.GetService().map_err(|e| format!("IAudioCaptureClient: {e}"))?;
        client.Start().map_err(|e| format!("IAudioClient::Start: {e}"))?;
        Ok(Session { client, capture, event })
    }
}

/// Забирает всё, что накопилось; возвращает число отданных кадров.
fn drain(s: &Session, chunker: &mut Chunker, out: &mut dyn FnMut(&[i16])) -> Result<u64, String> {
    let mut n = 0;
    unsafe {
        loop {
            let size = s.capture.GetNextPacketSize().map_err(|e| format!("GetNextPacketSize: {e}"))?;
            if size == 0 {
                break;
            }
            let mut data = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            s.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None).map_err(|e| format!("GetBuffer: {e}"))?;
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                n += chunker.push_silence(frames as usize, out);
            } else {
                let samples = std::slice::from_raw_parts(data as *const f32, frames as usize * CHANNELS as usize);
                n += chunker.push_f32(samples, out);
            }
            s.capture.ReleaseBuffer(frames).map_err(|e| format!("ReleaseBuffer: {e}"))?;
        }
    }
    Ok(n)
}

fn audio_thread(target: AudioTarget, sink: Arc<dyn AudioSink>, stop: Arc<AtomicBool>, ready: std::sync::mpsc::Sender<Result<(), String>>) {
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let mut task_index = 0u32;
    let mm = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) }.ok();
    let mut out = |f: &[i16]| sink.frame(f);

    let mut session = match open(target) {
        Ok(s) => {
            let _ = ready.send(Ok(()));
            Some(s)
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    log::info!("звук трансляции: захват {target:?}");
    let mut chunker = Chunker::default();
    let mut pacer = Pacer::new(Instant::now());
    let mut retry_at: Option<Instant> = None;
    let mut errors = 0u32;
    while !stop.load(Ordering::Relaxed) {
        match &session {
            Some(s) => {
                unsafe { WaitForSingleObject(s.event.0, 10) };
                match drain(s, &mut chunker, &mut out) {
                    Ok(n) if n > 0 => pacer.on_data(n, Instant::now()),
                    Ok(_) => {}
                    Err(e) => {
                        // Например, AUDCLNT_E_DEVICE_INVALIDATED; переоткрываем, пока досылаем тишину.
                        log::warn!("звук трансляции: {e} — переоткрываю");
                        unsafe {
                            let _ = s.client.Stop();
                        }
                        session = None;
                        errors += 1;
                        retry_at = Some(Instant::now() + Duration::from_millis(500 * errors.min(10) as u64));
                    }
                }
            }
            None => {
                std::thread::sleep(FRAME_DUR);
                if retry_at.is_some_and(|t| Instant::now() >= t) {
                    match open(target) {
                        Ok(s) => {
                            log::info!("звук трансляции: захват восстановлен");
                            session = Some(s);
                            retry_at = None;
                        }
                        Err(e) => {
                            errors += 1;
                            log::warn!("звук трансляции: {e}");
                            retry_at = Some(Instant::now() + Duration::from_millis(500 * errors.min(10) as u64));
                        }
                    }
                }
            }
        }
        let silence = pacer.silence_due(Instant::now());
        if silence > 0 {
            let padded = chunker.flush_padded(&mut out);
            let left = silence.saturating_sub(padded);
            if left > 0 {
                chunker.push_silence(left as usize * FRAME, &mut out);
            }
        }
    }
    if let Some(s) = session {
        unsafe {
            let _ = s.client.Stop();
        }
    }
    if let Some(h) = mm {
        let _ = unsafe { AvRevertMmThreadCharacteristics(h) };
    }
    log::info!("звук трансляции: остановлен");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_saturates() {
        assert_eq!(f32_to_i16(0.0), 0);
        assert_eq!(f32_to_i16(1.0), 32767);
        assert_eq!(f32_to_i16(-1.0), -32767);
        assert_eq!(f32_to_i16(2.5), 32767);
        assert_eq!(f32_to_i16(-7.0), -32767);
        assert_eq!(f32_to_i16(0.5), 16384);
        assert_eq!(f32_to_i16(f32::NAN), 0);
    }

    #[test]
    fn chunker_emits_exact_10ms_frames() {
        let mut c = Chunker::default();
        let mut frames: Vec<Vec<i16>> = Vec::new();
        // 3 пакета по 7 мс (336 сэмплов на канал) = 21 мс → 2 кадра, 1 мс в остатке.
        for _ in 0..3 {
            let pkt = vec![0.25f32; 336 * 2];
            c.push_f32(&pkt, &mut |f| frames.push(f.to_vec()));
        }
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().all(|f| f.len() == FRAME_LEN && f.iter().all(|&s| s == 8192)));
        // Остаток дополняется тишиной.
        assert_eq!(c.flush_padded(&mut |f| frames.push(f.to_vec())), 1);
        let last = frames.last().unwrap();
        assert_eq!(last.len(), FRAME_LEN);
        assert_eq!(last[0], 8192);
        assert_eq!(last[48 * 2 - 1], 8192);
        assert_eq!(last[48 * 2], 0);
        assert_eq!(c.flush_padded(&mut |_| panic!()), 0);
    }

    #[test]
    fn chunker_keeps_channel_order() {
        let mut c = Chunker::default();
        let mut got = Vec::new();
        let pkt: Vec<f32> = (0..FRAME).flat_map(|_| [1.0f32, -1.0]).collect();
        c.push_f32(&pkt, &mut |f| got.extend_from_slice(f));
        assert_eq!(got.len(), FRAME_LEN);
        assert!(got.chunks(2).all(|lr| lr[0] == 32767 && lr[1] == -32767));
    }

    #[test]
    fn silence_frames() {
        let mut c = Chunker::default();
        let mut n = 0;
        assert_eq!(c.push_silence(FRAME * 3 + 10, &mut |f| {
            assert!(f.iter().all(|&s| s == 0));
            n += 1;
        }), 3);
        assert_eq!(n, 3);
    }

    #[test]
    fn pacer_fills_only_real_gaps() {
        let t = Instant::now();
        let mut p = Pacer::new(t);
        // Данные идут ровно — тишины нет.
        for i in 1..=100u32 {
            p.on_data(1, t + FRAME_DUR * i);
            assert_eq!(p.silence_due(t + FRAME_DUR * i + Duration::from_millis(5)), 0);
        }
        // Пауза 30 мс — ещё не дыра.
        assert_eq!(p.silence_due(t + FRAME_DUR * 103), 0);
        // 60 мс без данных — досылаем 6 кадров, затем по одному по мере хода часов (после порога).
        assert_eq!(p.silence_due(t + FRAME_DUR * 106), 6);
        assert_eq!(p.silence_due(t + FRAME_DUR * 107), 0);
        assert_eq!(p.silence_due(t + FRAME_DUR * 111), 5);
    }

    #[test]
    fn root_walks_same_exe_only() {
        let procs = vec![
            ProcInfo { pid: 1, parent: 0, exe: "explorer.exe".into() },
            ProcInfo { pid: 10, parent: 1, exe: "chrome.exe".into() },
            ProcInfo { pid: 11, parent: 10, exe: "chrome.exe".into() },
            ProcInfo { pid: 12, parent: 11, exe: "Chrome.EXE".into() },
            ProcInfo { pid: 20, parent: 1, exe: "game.exe".into() },
            // Родитель с переиспользованным PID / петля.
            ProcInfo { pid: 30, parent: 30, exe: "x.exe".into() },
        ];
        assert_eq!(app_root(12, &procs), 10);
        assert_eq!(app_root(10, &procs), 10);
        assert_eq!(app_root(20, &procs), 20);
        assert_eq!(app_root(30, &procs), 30);
        assert_eq!(app_root(99, &procs), 99);
    }
}
