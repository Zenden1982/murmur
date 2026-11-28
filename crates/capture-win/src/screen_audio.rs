//! Звук трансляции экрана с исключёнными программами.
//!
//! Поток-планировщик раз в секунду: звуковые сессии → [`plan_targets`] → список целей в смеситель (перечисление процессов
//! и сессий — десятки мс, поэтому не в потоке смесителя). Смеситель запускает/останавливает захваты по разнице
//! (старт и остановка — в отдельных потоках) и каждые 10 мс по часам: по кадру из каждой очереди → сумма → приёмник.
//! Пустой список исключений даёт одну цель «всё, кроме Murmur»: её кадры идут прямо в приёмник, без очереди и часов
//! смесителя — как прежний захват.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::System::Threading::GetCurrentProcessId;

use crate::app_audio::{AppAudio, AudioSink, AudioTarget, CHANNELS, FRAME, process_list};
use crate::audio_mix::{MixQueue, mix_into};
use crate::audio_plan::plan_targets;
use crate::audio_sessions::session_pids;

const TICK: Duration = Duration::from_millis(10);
const REPLAN: Duration = Duration::from_millis(1000);
const RETRY: Duration = Duration::from_secs(5);
/// Отстали от часов сильнее — не догоняем пачкой кадров, а начинаем отсчёт заново.
const MAX_LAG: Duration = Duration::from_millis(50);
/// Сколько кадров накопить в очереди источника, прежде чем брать из неё (сглаживает рывки потока захвата).
const PRIME: usize = 2;

pub type ExcludedFn = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

/// Идущий захват звука экрана; остановка — [`ScreenAudio::stop`] (или drop).
pub struct ScreenAudio {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// Кадры захвата «всё, кроме Murmur» — сразу в приёмник.
struct Direct(Arc<dyn AudioSink>);

impl AudioSink for Direct {
    fn frame(&self, data: &[i16]) {
        self.0.frame(data);
    }
}

struct Running {
    cap: AppAudio,
    /// None — прямой путь в приёмник (цель AllExcept).
    queue: Option<Arc<MixQueue>>,
    primed: bool,
}

type Started = (AudioTarget, Option<Arc<MixQueue>>, Result<AppAudio, String>);

fn targets(self_pid: u32, excluded: &ExcludedFn) -> Vec<AudioTarget> {
    let ex = excluded();
    if ex.is_empty() {
        return vec![AudioTarget::AllExcept(self_pid)];
    }
    plan_targets(&session_pids(), &process_list(), self_pid, &ex)
}

/// Захват цели: AllExcept — прямо в приёмник, приложение — в свою очередь смесителя.
fn start_target(t: AudioTarget, sink: &Arc<dyn AudioSink>) -> Started {
    match t {
        AudioTarget::AllExcept(_) => (t, None, AppAudio::start(t, Arc::new(Direct(sink.clone())))),
        AudioTarget::App(_) => {
            let q = Arc::new(MixQueue::default());
            let res = AppAudio::start(t, q.clone());
            (t, Some(q), res)
        }
    }
}

/// Остановка захвата не держит вызывающий поток (ждёт поток захвата).
fn stop_detached(cap: AppAudio) {
    std::thread::spawn(move || cap.stop());
}

impl ScreenAudio {
    /// Ошибка — если при старте не запустился ни один нужный захват.
    pub fn start(excluded: ExcludedFn, sink: Arc<dyn AudioSink>) -> Result<ScreenAudio, String> {
        let self_pid = unsafe { GetCurrentProcessId() };
        let want = targets(self_pid, &excluded);
        // стартуем параллельно: каждая цель — до 5 с
        let handles: Vec<_> = want
            .iter()
            .map(|&t| {
                let sink = sink.clone();
                std::thread::spawn(move || start_target(t, &sink))
            })
            .collect();
        let mut running = HashMap::new();
        let mut first_err = None;
        for h in handles {
            let Ok((t, queue, res)) = h.join() else { continue };
            match res {
                Ok(cap) => {
                    running.insert(t, Running { cap, queue, primed: false });
                }
                Err(e) => {
                    log::warn!("звук трансляции: {t:?}: {e}");
                    first_err.get_or_insert(e);
                }
            }
        }
        if running.is_empty()
            && let Some(e) = first_err
        {
            return Err(e);
        }
        log::info!("звук трансляции экрана: {} захват(ов), исключено {:?}", running.len(), excluded());
        let stop = Arc::new(AtomicBool::new(false));

        let (plan_tx, plan_rx) = mpsc::channel::<Vec<AudioTarget>>();
        let st = stop.clone();
        std::thread::Builder::new()
            .name("screen-audio-plan".into())
            .spawn(move || {
                while !st.load(Ordering::Relaxed) {
                    std::thread::sleep(REPLAN);
                    if st.load(Ordering::Relaxed) || plan_tx.send(targets(self_pid, &excluded)).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| format!("поток звука: {e}"))?;

        let st = stop.clone();
        let thread = std::thread::Builder::new()
            .name("screen-audio".into())
            .spawn(move || run(sink, running, plan_rx, st))
            .map_err(|e| format!("поток звука: {e}"))?;
        Ok(ScreenAudio { stop, thread: Some(thread) })
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

impl Drop for ScreenAudio {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run(sink: Arc<dyn AudioSink>, mut running: HashMap<AudioTarget, Running>, plans: mpsc::Receiver<Vec<AudioTarget>>, stop: Arc<AtomicBool>) {
    let (tx, rx) = mpsc::channel::<Started>();
    let mut starting: Vec<AudioTarget> = Vec::new();
    let mut failed: HashMap<AudioTarget, Instant> = HashMap::new();
    let mut want: Vec<AudioTarget> = running.keys().copied().collect();
    let mut out = vec![0i16; FRAME * CHANNELS as usize];
    let mut t0 = Instant::now();
    let mut n: u32 = 0;
    while !stop.load(Ordering::Relaxed) {
        // ровный темп 10 мс от t0
        n += 1;
        let due = t0 + TICK * n;
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        } else if now - due > MAX_LAG {
            t0 = now;
            n = 0;
        }

        // запущенные в фоне захваты
        while let Ok((t, queue, res)) = rx.try_recv() {
            starting.retain(|x| *x != t);
            match res {
                Ok(cap) if want.contains(&t) => {
                    log::info!("звук трансляции: + {t:?}");
                    running.insert(t, Running { cap, queue, primed: false });
                }
                Ok(cap) => stop_detached(cap),
                Err(e) => {
                    log::warn!("звук трансляции: {t:?}: {e}");
                    failed.insert(t, Instant::now() + RETRY);
                }
            }
        }

        // смешать по кадру от каждой очереди; если очередей нет (только прямой путь или тишина без целей) —
        // при прямом пути кадры уже идут сами, без целей — шлём тишину, чтобы трек не замерзал
        let direct = running.values().any(|r| r.queue.is_none());
        let queued = running.values().any(|r| r.queue.is_some());
        if queued || !direct {
            let mut frames = Vec::with_capacity(running.len());
            for r in running.values_mut() {
                let Some(q) = &r.queue else { continue };
                let len = q.len();
                if len == 0 {
                    r.primed = false;
                } else if len >= PRIME {
                    r.primed = true;
                }
                if r.primed
                    && let Some(f) = q.pop()
                {
                    frames.push(f);
                }
            }
            // на переходе «прямой путь → очереди» прямой захват ещё жив: не дублируем его тишиной
            if !direct || !frames.is_empty() {
                mix_into(&mut out, &frames);
                sink.frame(&out);
            }
        }

        // новый план от планировщика (берём последний)
        let mut fresh = None;
        while let Ok(p) = plans.try_recv() {
            fresh = Some(p);
        }
        if let Some(p) = fresh {
            want = p;
            let gone: Vec<AudioTarget> = running.keys().filter(|t| !want.contains(t)).copied().collect();
            for t in gone {
                if let Some(r) = running.remove(&t) {
                    log::info!("звук трансляции: − {t:?}");
                    stop_detached(r.cap);
                }
            }
            failed.retain(|_, until| Instant::now() < *until);
            for t in &want {
                if running.contains_key(t) || starting.contains(t) || failed.contains_key(t) {
                    continue;
                }
                starting.push(*t);
                let (t, tx, sink) = (*t, tx.clone(), sink.clone());
                std::thread::spawn(move || {
                    // получатель уже закрыт (стрим остановлен) — захват остановится при drop здесь же
                    let _ = tx.send(start_target(t, &sink));
                });
            }
        }
    }
    // захваты, которые ещё стартуют, остановятся сами (send в закрытый канал вернёт их и они упадут в drop)
    let caps: Vec<_> = running.drain().map(|(_, r)| std::thread::spawn(move || r.cap.stop())).collect();
    for c in caps {
        let _ = c.join();
    }
    log::info!("звук трансляции экрана: остановлен");
}
