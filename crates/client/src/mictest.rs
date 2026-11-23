//! Проверка микрофона в настройках: уровень (всегда, пока открыты настройки) и «слышать себя».
//!
//! Отдельный WASAPI-захват через cpal (shared mode, не мешает звонку). Звук проходит через тот же
//! WebRTC APM (шумоподавление, автогромкость), что и в звонке, — слышно то, что услышат друзья
//! (без эхоподавления: проверка рассчитана на наушники).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use livekit::webrtc::native::apm::AudioProcessingModule;

use crate::config::NoiseMode;
use crate::denoise::Processor;

const APM_RATE: u32 = 48_000;
const CHUNK: usize = (APM_RATE / 100) as usize; // 10 мс

/// Линейный ресемплер моно-потока.
struct Resampler {
    step: f64,
    t: f64,
    prev: f32,
}

impl Resampler {
    fn new(from: u32, to: u32) -> Self {
        Self { step: from as f64 / to as f64, t: 0.0, prev: 0.0 }
    }

    fn push(&mut self, x: f32, out: &mut impl FnMut(f32)) {
        while self.t < 1.0 {
            out(self.prev + (x - self.prev) * self.t as f32);
            self.t += self.step;
        }
        self.t -= 1.0;
        self.prev = x;
    }
}

struct SendApm(AudioProcessingModule);
// SAFETY: APM используется только из одного потока (колбэк захвата), между вызовами не разделяется.
unsafe impl Send for SendApm {}

/// Открытые потоки WASAPI. Живут только в рабочем потоке (cpal-потоки не передаются между потоками).
struct Streams {
    _input: cpal::Stream,
    _output: Option<cpal::Stream>,
}

/// Проверка микрофона. Потоки открываются и закрываются в отдельном потоке: на занятом/сломанном устройстве WASAPI
/// это может занимать секунды, а UI-поток ждать не должен. Уничтожение значения останавливает проверку.
pub struct MicTest {
    _stop: std::sync::mpsc::Sender<()>,
    ready: Arc<std::sync::atomic::AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
}

impl MicTest {
    /// Потоки открыты и работают.
    pub fn running(&self) -> bool {
        self.ready.load(Ordering::Relaxed)
    }

    /// Забирает ошибку запуска (один раз).
    pub fn take_error(&self) -> Option<String> {
        self.error.lock().unwrap().take()
    }
}

/// Устройство по имени (имена WASAPI совпадают с именами ADM); None или не найдено — по умолчанию.
fn find(devices: impl Iterator<Item = cpal::Device>, name: Option<&str>) -> Option<cpal::Device> {
    let name = name?;
    devices.into_iter().find(|d| d.description().is_ok_and(|desc| desc.name() == name || name.contains(desc.name()) || desc.name().contains(name)))
}

/// Запускает проверку в фоне, сразу возвращается. `level` — уровень 0..1 (биты f32). `playback` — выводить себя в наушники.
pub fn start(
    input_name: Option<&str>,
    output_name: Option<&str>,
    noise: NoiseMode,
    agc: bool,
    playback: bool,
    level: Arc<AtomicU32>,
) -> MicTest {
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let error = Arc::new(Mutex::new(None));
    let (input_name, output_name) = (input_name.map(str::to_owned), output_name.map(str::to_owned));
    let (ready2, error2) = (ready.clone(), error.clone());
    let spawned = std::thread::Builder::new().name("amadeus-mictest".into()).spawn(move || {
        let t0 = std::time::Instant::now();
        match open(input_name.as_deref(), output_name.as_deref(), noise, agc, playback, level, error2.clone()) {
            Ok(streams) => {
                log::info!("проверка микрофона: запущена за {} мс", t0.elapsed().as_millis());
                ready2.store(true, Ordering::Relaxed);
                let _ = stop_rx.recv(); // вернётся, когда MicTest уничтожен
                ready2.store(false, Ordering::Relaxed);
                let t1 = std::time::Instant::now();
                drop(streams);
                log::info!("проверка микрофона: остановлена за {} мс", t1.elapsed().as_millis());
            }
            Err(e) => *error2.lock().unwrap() = Some(e),
        }
    });
    if let Err(e) = spawned {
        *error.lock().unwrap() = Some(format!("поток не запущен: {e}"));
    }
    MicTest { _stop: stop_tx, ready, error }
}

fn open(
    input_name: Option<&str>,
    output_name: Option<&str>,
    noise: NoiseMode,
    agc: bool,
    playback: bool,
    level: Arc<AtomicU32>,
    error: Arc<Mutex<Option<String>>>,
) -> Result<Streams, String> {
    let host = cpal::default_host();
    let input = find(host.input_devices().map_err(|e| e.to_string())?, input_name)
        .or_else(|| host.default_input_device())
        .ok_or("микрофон не найден")?;
    let in_cfg = input.default_input_config().map_err(|e| format!("микрофон: {e}"))?;
    if in_cfg.sample_format() != cpal::SampleFormat::F32 {
        return Err(format!("неподдерживаемый формат микрофона: {:?}", in_cfg.sample_format()));
    }
    let in_cfg: cpal::StreamConfig = in_cfg.config();
    let in_ch = in_cfg.channels as usize;

    // Выход готовим заранее, чтобы знать его частоту.
    let mut out_stream = None;
    let mut out_rate = APM_RATE;
    let queue: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::new()));
    if playback {
        let output = find(host.output_devices().map_err(|e| e.to_string())?, output_name)
            .or_else(|| host.default_output_device())
            .ok_or("устройство вывода не найдено")?;
        let oc = output.default_output_config().map_err(|e| format!("вывод: {e}"))?;
        if oc.sample_format() != cpal::SampleFormat::F32 {
            return Err(format!("неподдерживаемый формат вывода: {:?}", oc.sample_format()));
        }
        let oc: cpal::StreamConfig = oc.config();
        out_rate = oc.sample_rate;
        let out_ch = oc.channels as usize;
        let q = queue.clone();
        let prime = (out_rate / 1000 * 30) as usize; // 30 мс предбуфер
        let mut primed = false;
        let stream = output
            .build_output_stream::<f32, _, _>(
                oc,
                move |data, _| {
                    let mut q = q.lock().unwrap();
                    if !primed && q.len() >= prime {
                        primed = true;
                    }
                    for frame in data.chunks_mut(out_ch) {
                        let s = if primed { q.pop_front() } else { None };
                        if s.is_none() {
                            primed = false;
                        }
                        frame.fill(s.unwrap_or(0.0));
                    }
                },
                {
                    let error = error.clone();
                    move |e| {
                        log::warn!("проверка микрофона, вывод: {e}");
                        *error.lock().unwrap() = Some(format!("вывод: {e}"));
                    }
                },
                None,
            )
            .map_err(|e| format!("вывод: {e}"))?;
        out_stream = Some(stream);
    }

    let mut apm = SendApm(AudioProcessingModule::new(false, agc, true, noise == NoiseMode::Standard));
    let mut denoiser = Processor::new(noise);
    let mut fbuf = [0f32; CHUNK];
    let mut to48 = Resampler::new(in_cfg.sample_rate, APM_RATE);
    let mut to_out = Resampler::new(APM_RATE, out_rate);
    let mut buf48: Vec<f32> = Vec::with_capacity(CHUNK * 4);
    let mut chunk = vec![0i16; CHUNK];
    let q = queue.clone();
    let max_q = (out_rate / 1000 * 120) as usize; // больше 120 мс — сбрасываем (задержка важнее)
    let keep_q = (out_rate / 1000 * 40) as usize;
    let mut smooth = 0.0f32;
    let mut gate = crate::gate::Gate::new();
    let input_stream = input
        .build_input_stream::<f32, _, _>(
            in_cfg,
            move |data, _| {
                for frame in data.chunks(in_ch) {
                    let mono = frame.iter().sum::<f32>() / in_ch as f32;
                    to48.push(mono, &mut |s| buf48.push(s));
                }
                while buf48.len() >= CHUNK {
                    for (d, s) in chunk.iter_mut().zip(buf48.drain(..CHUNK)) {
                        *d = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                    }
                    let _ = apm.0.process_stream(&mut chunk, APM_RATE as i32, 1);
                    // Как в звонке: громкость микрофона до шумодава, порог — после.
                    for (f, &s) in fbuf.iter_mut().zip(chunk.iter()) {
                        *f = s as f32;
                    }
                    crate::gate::apply_gain(&mut fbuf, i16::MAX as f32);
                    for (s, &f) in chunk.iter_mut().zip(fbuf.iter()) {
                        *s = f.clamp(i16::MIN as f32, i16::MAX as f32) as i16;
                    }
                    // Как в звонке: RNNoise после APM (кадр 10 мс на 48 кГц = кадр RNNoise).
                    if let Some(d) = &mut denoiser {
                        for (f, &s) in fbuf.iter_mut().zip(chunk.iter()) {
                            *f = s as f32;
                        }
                        d.process(&mut fbuf);
                        for (s, &f) in chunk.iter_mut().zip(fbuf.iter()) {
                            *s = f.clamp(i16::MIN as f32, i16::MAX as f32) as i16;
                        }
                    }
                    for (f, &s) in fbuf.iter_mut().zip(chunk.iter()) {
                        *f = s as f32;
                    }
                    // Уровень (rms×4, как у audio_level LiveKit) — до порога, чтобы метку было видно и под ней.
                    let (lvl, _) = gate.process(&mut fbuf, i16::MAX as f32, APM_RATE);
                    for (s, &f) in chunk.iter_mut().zip(fbuf.iter()) {
                        *s = f as i16;
                    }
                    smooth = if lvl > smooth { lvl } else { smooth * 0.85 + lvl * 0.15 };
                    level.store(smooth.to_bits(), Ordering::Relaxed);
                    if playback {
                        let mut q = q.lock().unwrap();
                        for &s in chunk.iter() {
                            to_out.push(s as f32 / i16::MAX as f32, &mut |x| q.push_back(x));
                        }
                        if q.len() > max_q {
                            let drop = q.len() - keep_q;
                            q.drain(..drop);
                        }
                    }
                }
            },
            move |e| {
                log::warn!("проверка микрофона, захват: {e}");
                *error.lock().unwrap() = Some(format!("микрофон: {e}"));
            },
            None,
        )
        .map_err(|e| format!("микрофон: {e}"))?;
    input_stream.play().map_err(|e| format!("микрофон: {e}"))?;
    if let Some(s) = &out_stream {
        s.play().map_err(|e| format!("вывод: {e}"))?;
    }
    Ok(Streams { _input: input_stream, _output: out_stream })
}

#[cfg(test)]
mod tests {
    use super::Resampler;

    #[test]
    fn resampler_ratio() {
        let mut r = Resampler::new(44_100, 48_000);
        let mut n = 0usize;
        for i in 0..44_100 {
            r.push((i as f32 * 0.01).sin(), &mut |_| n += 1);
        }
        assert!((n as i64 - 48_000).abs() <= 2, "{n}");
        let mut r = Resampler::new(48_000, 48_000);
        let mut out = Vec::new();
        for x in [0.1f32, 0.2, 0.3] {
            r.push(x, &mut |s| out.push(s));
        }
        assert_eq!(out.len(), 3);
    }
}
