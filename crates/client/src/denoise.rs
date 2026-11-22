//! Нейросетевой шумодав в цепочке микрофона: DeepFilterNet3 (по умолчанию) или RNNoise (лёгкий).
//!
//! Звонок: колбэк захвата WebRTC (патч vendor/webrtc-sys) получает каждый 10-мс блок микрофона на 48 кГц после
//! эхоподавления — шумодав чистит его прямо там, до кодера Opus. Полоса не режется (фуллбэнд 48 кГц), в отличие
//! от шумоподавления WebRTC, которое делает голос «глухим».
//! - DeepFilterNet3 (крейт deep-filter-rt, модель внутри): лучшее качество, давит и шипение; ~2,5 % ядра, +30 мс.
//! - RNNoise (крейт nnnoiseless): дешевле и +10 мс, но белое шипение почти не трогает.
//!
//! Проверка микрофона (mictest.rs) использует тот же [`Processor`].

use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use deep_filter_rt::tract::DfTract;
use ndarray::Array2;
use nnnoiseless::DenoiseState;

use crate::config::NoiseMode;

/// Обе сети работают кадрами по 10 мс на 48 кГц.
pub const FRAME: usize = DenoiseState::FRAME_SIZE;
pub const RATE: i32 = 48_000;
const SCALE: f32 = 32_768.0;

/// Режим звонка (NoiseMode как число) и готовый процессор под него.
static MODE: AtomicU8 = AtomicU8::new(0);
static CALL: Mutex<Option<CallState>> = Mutex::new(None);
static SEEN: AtomicBool = AtomicBool::new(false);
static WRONG_FORMAT: AtomicBool = AtomicBool::new(false);

/// Свой уровень микрофона в звонке — прямо из колбэка захвата (каждые 10 мс), без ожидания «говорящих» от сервера.
static LEVEL: AtomicU32 = AtomicU32::new(0);
/// Когда последний раз был блок громче порога (мс от [`epoch`]) и когда вообще был блок.
static LOUD_AT: AtomicU64 = AtomicU64::new(0);
static BLOCK_AT: AtomicU64 = AtomicU64::new(0);
static SPEAKING: AtomicBool = AtomicBool::new(false);
static REPAINT: OnceLock<eframe::egui::Context> = OnceLock::new();
/// Порог речи по уровню (шкала как у audio_level LiveKit / проверки микрофона: rms × 4) и «хвост» после речи.
const SPEAK_LEVEL: f32 = 0.06;
const SPEAK_HOLD_MS: u64 = 300;

fn epoch() -> &'static Instant {
    static E: OnceLock<Instant> = OnceLock::new();
    E.get_or_init(Instant::now)
}

fn now_ms() -> u64 {
    epoch().elapsed().as_millis() as u64
}

/// Контекст UI: колбэк захвата будит отрисовку, когда начинаешь говорить (иначе волна ждала бы следующего кадра).
pub fn set_repaint(ctx: eframe::egui::Context) {
    let _ = REPAINT.set(ctx);
}

/// Свой уровень (0..1) и «говорю» — по последним блокам микрофона. Блоков нет дольше 200 мс (микрофон выключен,
/// не в звонке) — тишина.
pub fn local_voice() -> (f32, bool) {
    let now = now_ms();
    if now.saturating_sub(BLOCK_AT.load(Ordering::Relaxed)) > 200 {
        return (0.0, false);
    }
    let level = f32::from_bits(LEVEL.load(Ordering::Relaxed));
    (level, now.saturating_sub(LOUD_AT.load(Ordering::Relaxed)) <= SPEAK_HOLD_MS)
}

/// Порог (noise gate) звонка: только аудиопоток захвата.
static GATE: Mutex<crate::gate::Gate> = Mutex::new(crate::gate::Gate::new());

/// Уровень блока до порога (0..1) → сглаженный уровень; «говорю» — порог открыт (в режиме рации — громче SPEAK_LEVEL).
fn track_level(lvl: f32, open: bool) {
    let loud = if crate::gate::gate_on() { open } else { lvl >= SPEAK_LEVEL };
    let prev = f32::from_bits(LEVEL.load(Ordering::Relaxed));
    let smooth = if lvl > prev { lvl } else { prev * 0.85 + lvl * 0.15 };
    LEVEL.store(smooth.to_bits(), Ordering::Relaxed);
    let now = now_ms();
    BLOCK_AT.store(now, Ordering::Relaxed);
    if loud {
        LOUD_AT.store(now, Ordering::Relaxed);
        if !SPEAKING.swap(true, Ordering::Relaxed)
            && let Some(ctx) = REPAINT.get()
        {
            ctx.request_repaint();
        }
    } else if now.saturating_sub(LOUD_AT.load(Ordering::Relaxed)) > SPEAK_HOLD_MS {
        SPEAKING.store(false, Ordering::Relaxed);
    }
}

fn code(m: NoiseMode) -> u8 {
    match m {
        NoiseMode::Off => 0,
        NoiseMode::Standard => 1,
        NoiseMode::Light => 2,
        NoiseMode::Neural => 3,
    }
}

/// Шумодав одного моно-канала: кадр [`FRAME`] отсчётов в шкале int16 (±32768), на месте.
pub enum Processor {
    Rnn { state: Box<DenoiseState<'static>>, out: Box<[f32; FRAME]> },
    Dfn { df: Box<DfTract>, inp: Array2<f32>, out: Array2<f32>, failed: bool },
}

impl Processor {
    /// None — режим без нейросети. DeepFilterNet грузится ~0,1–0,5 с: создавать не на аудиопотоке.
    pub fn new(mode: NoiseMode) -> Option<Self> {
        match mode {
            NoiseMode::Light => Some(Processor::Rnn { state: DenoiseState::new(), out: Box::new([0.0; FRAME]) }),
            NoiseMode::Neural => {
                let df = DfTract::default();
                if df.hop_size != FRAME || df.sr != RATE as usize {
                    log::error!("DeepFilterNet: неожиданный формат модели {} Гц / {}", df.sr, df.hop_size);
                    return None;
                }
                let ch = df.ch;
                Some(Processor::Dfn {
                    df: Box::new(df),
                    inp: Array2::zeros((ch, FRAME)),
                    out: Array2::zeros((ch, FRAME)),
                    failed: false,
                })
            }
            NoiseMode::Off | NoiseMode::Standard => None,
        }
    }

    pub fn process(&mut self, frame: &mut [f32]) {
        if frame.len() != FRAME {
            return;
        }
        match self {
            Processor::Rnn { state, out } => {
                state.process_frame(&mut out[..], frame);
                frame.copy_from_slice(&out[..]);
            }
            Processor::Dfn { df, inp, out, failed } => {
                for row in inp.rows_mut() {
                    for (d, &s) in row.into_iter().zip(frame.iter()) {
                        *d = s / SCALE;
                    }
                }
                match df.process(inp.view(), out.view_mut()) {
                    Ok(_) => {
                        for (d, &s) in frame.iter_mut().zip(out.row(0).iter()) {
                            *d = s * SCALE;
                        }
                    }
                    Err(e) => {
                        if !*failed {
                            *failed = true;
                            log::error!("DeepFilterNet: {e:#}");
                        }
                    }
                }
            }
        }
    }
}

struct CallState {
    mode: u8,
    proc: Processor,
    frames: u32,
    total: Duration,
    worst: Duration,
    since: Instant,
}

/// Подключает колбэк к WebRTC (один раз при старте движка). Пока шумодав не нужен, колбэк ничего не делает.
pub fn install() {
    webrtc_sys::amadeus::set_capture_hook(Some(capture_hook));
}

/// Режим шумодава в звонке (действует сразу, без переподключения). Сеть создаётся в фоне; пока она не готова,
/// звук идёт как есть.
pub fn set_mode(mode: NoiseMode) {
    let c = code(mode);
    if MODE.swap(c, Ordering::SeqCst) == c {
        return;
    }
    log::info!("шумодав в звонке: {mode:?}");
    if let Ok(mut s) = CALL.lock()
        && s.as_ref().is_some_and(|s| s.mode != c)
    {
        *s = None;
    }
    if !matches!(mode, NoiseMode::Light | NoiseMode::Neural) {
        return;
    }
    let _ = std::thread::Builder::new().name("amadeus-denoise-init".into()).spawn(move || {
        let t0 = Instant::now();
        let Some(proc) = Processor::new(mode) else { return };
        log::info!("шумодав {mode:?} готов за {} мс", t0.elapsed().as_millis());
        if MODE.load(Ordering::SeqCst) != c {
            return; // режим успели сменить
        }
        if let Ok(mut s) = CALL.lock() {
            *s = Some(CallState { mode: c, proc, frames: 0, total: Duration::ZERO, worst: Duration::ZERO, since: Instant::now() });
        }
    });
}

/// Флаги FTZ/DAZ на время обработки: денормалы в сети стоят в десятки раз дороже (так делают аудиохосты).
struct DenormalGuard(#[cfg(target_arch = "x86_64")] u32);

impl DenormalGuard {
    #[allow(deprecated)]
    fn new() -> Self {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            use std::arch::x86_64::{_mm_getcsr, _mm_setcsr};
            let old = _mm_getcsr();
            _mm_setcsr(old | 0x8040);
            DenormalGuard(old)
        }
        #[cfg(not(target_arch = "x86_64"))]
        DenormalGuard()
    }
}

impl Drop for DenormalGuard {
    #[allow(deprecated)]
    fn drop(&mut self) {
        #[cfg(target_arch = "x86_64")]
        unsafe {
            std::arch::x86_64::_mm_setcsr(self.0)
        }
    }
}

unsafe extern "C" fn capture_hook(channels: *const *mut f32, num_channels: usize, frames: usize, rate: i32) {
    if !SEEN.swap(true, Ordering::Relaxed) {
        log::info!("захват микрофона: {rate} Гц, {num_channels} кан., {frames} отсчётов в блоке");
    }
    if channels.is_null() || num_channels == 0 {
        return;
    }
    // SAFETY: WebRTC передаёт num_channels указателей на буферы по frames float, живых на время вызова.
    let ptrs = unsafe { std::slice::from_raw_parts(channels, num_channels) };
    if ptrs.iter().any(|p| p.is_null()) {
        return;
    }
    // Громкость микрофона — до шумодава.
    for &p in ptrs {
        crate::gate::apply_gain(unsafe { std::slice::from_raw_parts_mut(p, frames) }, SCALE);
    }
    unsafe { process_block(channels, num_channels, frames, rate) };
    // Порог — после шумодава; уровень для индикатора — до порога.
    let first = unsafe { std::slice::from_raw_parts_mut(ptrs[0], frames) };
    let Ok(mut gate) = GATE.try_lock() else { return };
    let (lvl, open) = gate.process(first, SCALE, rate.max(1) as u32);
    drop(gate);
    for &p in &ptrs[1..] {
        unsafe { std::slice::from_raw_parts_mut(p, frames) }.copy_from_slice(first);
    }
    track_level(lvl, open);
}

unsafe fn process_block(channels: *const *mut f32, num_channels: usize, frames: usize, rate: i32) {
    let mode = MODE.load(Ordering::Relaxed);
    if mode < 2 {
        return;
    }
    if rate != RATE || frames != FRAME {
        if !WRONG_FORMAT.swap(true, Ordering::Relaxed) {
            log::warn!("шумодав: неожиданный формат {rate} Гц / {frames} — пропускаю");
        }
        return;
    }
    // Аудиопоток WebRTC: без паник наружу и без ожидания блокировки.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Ok(mut guard) = CALL.try_lock() else { return };
        let Some(st) = guard.as_mut().filter(|s| s.mode == mode) else { return };
        // SAFETY: WebRTC передаёт num_channels указателей на буферы по frames float, живых на время вызова.
        let ptrs = unsafe { std::slice::from_raw_parts(channels, num_channels) };
        if ptrs.iter().any(|p| p.is_null()) {
            return;
        }
        let first = unsafe { std::slice::from_raw_parts_mut(ptrs[0], frames) };
        let t0 = Instant::now();
        {
            let _ftz = DenormalGuard::new();
            st.proc.process(first);
        }
        let dt = t0.elapsed();
        // Микрофон — моно; если каналов больше, остальным — тот же очищенный сигнал.
        for &p in &ptrs[1..] {
            unsafe { std::slice::from_raw_parts_mut(p, frames) }.copy_from_slice(first);
        }
        st.frames += 1;
        st.total += dt;
        st.worst = st.worst.max(dt);
        if st.since.elapsed() >= Duration::from_secs(60) {
            log::info!(
                "шумодав: {} кадров, в среднем {} мкс, худший {} мкс (бюджет 10000)",
                st.frames,
                (st.total / st.frames.max(1)).as_micros(),
                st.worst.as_micros()
            );
            (st.frames, st.total, st.worst, st.since) = (0, Duration::ZERO, Duration::ZERO, Instant::now());
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Простой ГПСЧ (детерминированный шум без внешних крейтов).
    fn noise(seed: &mut u32) -> f32 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 17;
        *seed ^= *seed << 5;
        (*seed as f32 / u32::MAX as f32) * 2.0 - 1.0
    }

    fn energy(x: &[f32]) -> f32 {
        x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32
    }

    /// Ослабление (дБ) шума: `lp` — сглаживание (0 — белый шум, ближе к 1 — «гул»).
    fn attenuation(mode: NoiseMode, amp: f32, lp: f32) -> f32 {
        let mut p = Processor::new(mode).unwrap();
        let mut seed = 0x1234_5678;
        let (mut e_in, mut e_out, mut y) = (0.0, 0.0, 0.0f32);
        for i in 0..400 {
            let mut f: Vec<f32> = (0..FRAME)
                .map(|_| {
                    y = lp * y + (1.0 - lp) * noise(&mut seed) * amp;
                    y
                })
                .collect();
            let before = energy(&f);
            p.process(&mut f);
            if i >= 150 {
                e_in += before;
                e_out += energy(&f);
            }
        }
        10.0 * (e_out / e_in).log10()
    }

    #[test]
    fn rnnoise_suppresses_hum() {
        let db = attenuation(NoiseMode::Light, 12_000.0, 0.9);
        assert!(db < -10.0, "{db} dB");
    }

    #[test]
    fn deepfilter_suppresses_hiss_and_hum() {
        let hiss = attenuation(NoiseMode::Neural, 1_000.0, 0.0);
        let hum = attenuation(NoiseMode::Neural, 12_000.0, 0.9);
        println!("DeepFilterNet: шипение {hiss:.1} дБ, гул {hum:.1} дБ");
        assert!(hiss < -10.0, "шипение {hiss} dB");
        assert!(hum < -10.0, "гул {hum} dB");
    }

    fn read_wav(path: &str) -> Vec<f32> {
        let b = std::fs::read(path).unwrap();
        let pos = b.windows(4).position(|w| w == b"data").unwrap() + 8;
        b[pos..].as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes(*c) as f32).collect()
    }

    fn write_wav(path: &str, x: &[f32]) {
        let mut b = Vec::new();
        let n = (x.len() * 2) as u32;
        b.extend_from_slice(b"RIFF");
        b.extend_from_slice(&(36 + n).to_le_bytes());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend_from_slice(&16u32.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes());
        b.extend_from_slice(&48_000u32.to_le_bytes());
        b.extend_from_slice(&96_000u32.to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&16u16.to_le_bytes());
        b.extend_from_slice(b"data");
        b.extend_from_slice(&n.to_le_bytes());
        for &s in x {
            b.extend_from_slice(&(s.clamp(-32768.0, 32767.0) as i16).to_le_bytes());
        }
        std::fs::write(path, b).unwrap();
    }

    /// Энергия «верха»: вторая разность (+12 дБ/окт) — грубая мера высоких частот.
    fn hi_energy(x: &[f32]) -> f32 {
        x.windows(3).map(|w| (w[2] - 2.0 * w[1] + w[0]).powi(2)).sum::<f32>() / x.len() as f32
    }

    fn db(a: f32, b: f32) -> f32 {
        10.0 * (a / b).log10()
    }

    /// Сравнение на настоящей речи: AMADEUS_SPEECH_WAV=путь к моно 48 кГц 16 бит; рядом пишет *_out_*.wav.
    #[test]
    #[ignore]
    fn speech_report() {
        let Ok(path) = std::env::var("AMADEUS_SPEECH_WAV") else { return };
        // 2 с тишины в начале: там будет только шум — мера подавления.
        let mut clean = vec![0.0f32; 96_000];
        clean.extend(read_wav(&path));
        let mut seed = 0x9e37_79b9;
        let rms = (energy(&clean[96_000..])).sqrt();
        // Шипение (белый шум) на 15 дБ тише речи + «гул».
        let mut y = 0.0f32;
        let noisy: Vec<f32> = clean
            .iter()
            .map(|&s| {
                y = 0.95 * y + 0.05 * noise(&mut seed);
                s + noise(&mut seed) * rms * 0.18 * 1.7 + y * rms * 2.0
            })
            .collect();
        let base = path.trim_end_matches(".wav");
        write_wav(&format!("{base}_noisy.wav"), &noisy);
        // Кадры с речью — по огибающей чистого сигнала.
        let speech: Vec<bool> = clean.chunks(FRAME).map(|c| energy(c) > rms * rms * 0.3).collect();
        for (name, mode) in [("webrtc-ns", NoiseMode::Standard), ("rnnoise", NoiseMode::Light), ("deepfilter", NoiseMode::Neural)] {
            for (input, tag) in [(&clean, "clean"), (&noisy, "noisy")] {
                let mut p = Processor::new(mode);
                let mut apm = (mode == NoiseMode::Standard)
                    .then(|| livekit::webrtc::native::apm::AudioProcessingModule::new(false, false, false, true));
                let mut out = Vec::with_capacity(input.len());
                let t0 = Instant::now();
                for c in input.as_chunks::<FRAME>().0 {
                    let mut f = c.to_vec();
                    if let Some(p) = &mut p {
                        p.process(&mut f);
                    }
                    if let Some(apm) = &mut apm {
                        let mut i: Vec<i16> = f.iter().map(|&s| s.clamp(-32768.0, 32767.0) as i16).collect();
                        apm.process_stream(&mut i, 48_000, 1).unwrap();
                        f = i.iter().map(|&s| s as f32).collect();
                    }
                    out.extend_from_slice(&f);
                }
                let per = t0.elapsed() / (input.len() / FRAME) as u32;
                write_wav(&format!("{base}_out_{name}_{tag}.wav"), &out);
                let (mut sp_in, mut sp_out, mut si_in, mut si_out) = (0.0, 0.0, 0.0, 0.0);
                // Задержка сетей до 30 мс — сравниваем по кадрам со сдвигом 3 кадра (с запасом в обе стороны).
                let lead = 50 * FRAME..190 * FRAME; // 0,5–1,9 с: только шум
                let noise_db = db(energy(&out[lead.clone()]).max(1e-6), energy(&input[lead]).max(1e-6));
                for (i, &is_sp) in speech.iter().enumerate().skip(200) {
                    let a = &input[i * FRAME..((i + 1) * FRAME).min(input.len())];
                    let b = &out[(i * FRAME).min(out.len())..((i + 1) * FRAME).min(out.len())];
                    if b.is_empty() {
                        break;
                    }
                    if is_sp {
                        sp_in += energy(a);
                        sp_out += energy(b);
                    } else {
                        si_in += energy(a);
                        si_out += energy(b);
                    }
                }
                println!(
                    "{name:10} {tag:5}: чистый шум {noise_db:+.1} дБ, речь {:+.1} дБ, паузы {:+.1} дБ, верх/всё {:+.1} дБ, {} мкс/кадр",
                    db(sp_out, sp_in),
                    db(si_out, si_in),
                    db(hi_energy(&out) / energy(&out), hi_energy(input) / energy(input)),
                    per.as_micros()
                );
            }
        }
    }

    #[test]
    fn wrong_size_is_untouched() {
        let mut p = Processor::new(NoiseMode::Light).unwrap();
        let mut f = vec![100.0f32; 160];
        p.process(&mut f);
        assert!(f.iter().all(|&s| s == 100.0));
    }
}
