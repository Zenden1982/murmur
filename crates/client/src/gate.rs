//! Усиление микрофона (0–200 %) и пороговый шумодав (noise gate) с авточувствительностью.
//!
//! Общие параметры — атомики: их ставит UI, читают колбэк захвата звонка (denoise.rs) и проверка микрофона (mictest.rs).
//! Уровень — в шкале rms×4 (0..1), как у индикатора и audio_level LiveKit.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

static GAIN: AtomicU32 = AtomicU32::new(1.0f32.to_bits());
static GATE_ON: AtomicBool = AtomicBool::new(false);
static AUTO: AtomicBool = AtomicBool::new(true);
static MANUAL: AtomicU32 = AtomicU32::new(DEFAULT_THRESHOLD.to_bits());
static AUTO_THR: AtomicU32 = AtomicU32::new(DEFAULT_THRESHOLD.to_bits());

pub const DEFAULT_THRESHOLD: f32 = 0.1;
/// Сколько порог держится открытым после речи (не обрезать концы слов).
pub const HOLD_MS: f32 = 300.0;
/// Плавность открытия/закрытия (без щелчков).
const RAMP_MS: f32 = 10.0;
/// Мягкий лимитер: выше этой доли шкалы — плавное сжатие, выход не больше 1.0.
const KNEE: f32 = 0.8;
/// Фон растёт не быстрее 1,5 дБ/с (речь не утягивает порог вверх), падает сразу.
const FLOOR_RISE_DB_PER_S: f32 = 1.5;
const AUTO_MIN: f32 = 0.03;
const AUTO_MAX: f32 = 0.5;

/// gain — 0..2; gate_on — режим «Голосом»; auto — авточувствительность; manual — ручной порог 0..1.
pub fn set_params(gain: f32, gate_on: bool, auto: bool, manual: f32) {
    GAIN.store(gain.clamp(0.0, 2.0).to_bits(), Ordering::Relaxed);
    GATE_ON.store(gate_on, Ordering::Relaxed);
    AUTO.store(auto, Ordering::Relaxed);
    MANUAL.store(manual.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
}

pub fn gain() -> f32 {
    f32::from_bits(GAIN.load(Ordering::Relaxed))
}

pub fn gate_on() -> bool {
    GATE_ON.load(Ordering::Relaxed)
}

/// Действующий порог: автоматический или ручной.
pub fn threshold() -> f32 {
    let a = if AUTO.load(Ordering::Relaxed) { &AUTO_THR } else { &MANUAL };
    f32::from_bits(a.load(Ordering::Relaxed))
}

fn soft_limit(x: f32) -> f32 {
    let a = x.abs();
    if a <= KNEE { x } else { x.signum() * (KNEE + (1.0 - KNEE) * ((a - KNEE) / (1.0 - KNEE)).tanh()) }
}

/// Усиление блока (отсчёты в шкале `scale`: 32768 в звонке, 1.0 в проверке) с мягким ограничением пиков.
pub fn apply_gain(block: &mut [f32], scale: f32) {
    let g = gain();
    if (g - 1.0).abs() < 1e-3 {
        return;
    }
    for s in block {
        *s = soft_limit(*s / scale * g) * scale;
    }
}

/// Уровень блока 0..1 (rms×4).
pub fn level(block: &[f32], scale: f32) -> f32 {
    if block.is_empty() {
        return 0.0;
    }
    let rms = (block.iter().map(|s| (s / scale).powi(2)).sum::<f32>() / block.len() as f32).sqrt();
    (rms * 4.0).min(1.0)
}

/// Автопорог по уровню фона.
fn auto_threshold(floor: f32) -> f32 {
    (floor * 2.5 + 0.02).clamp(AUTO_MIN, AUTO_MAX)
}

/// Состояние порога одного потока захвата.
pub struct Gate {
    floor: f32,
    hold_ms: f32,
    env: f32,
    open: bool,
}

impl Gate {
    pub const fn new() -> Self {
        Self { floor: 0.02, hold_ms: 0.0, env: 1.0, open: true }
    }

    /// Порог на блоке (уже после шумодава). Возвращает уровень до порога и «открыт».
    pub fn process(&mut self, block: &mut [f32], scale: f32, rate: u32) -> (f32, bool) {
        let lvl = level(block, scale);
        let ms = block.len() as f32 * 1000.0 / rate.max(1) as f32;
        let rise = 10f32.powf(FLOOR_RISE_DB_PER_S * ms / 1000.0 / 20.0);
        self.floor = lvl.min(self.floor * rise).max(1e-4);
        AUTO_THR.store(auto_threshold(self.floor).to_bits(), Ordering::Relaxed);
        if !gate_on() {
            (self.open, self.env, self.hold_ms) = (true, 1.0, 0.0);
            return (lvl, true);
        }
        if lvl >= threshold() {
            self.open = true;
            self.hold_ms = HOLD_MS;
        } else if self.hold_ms > 0.0 {
            self.hold_ms -= ms;
        } else {
            self.open = false;
        }
        let target = if self.open { 1.0 } else { 0.0 };
        let step = 1.0 / (rate.max(1) as f32 * RAMP_MS / 1000.0);
        for s in block {
            self.env = if self.env < target { (self.env + step).min(target) } else { (self.env - step).max(target) };
            *s *= self.env;
        }
        (lvl, self.open)
    }
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Атомики общие для всех тестов модуля — тесты, меняющие параметры, идут по очереди.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn sine(amp: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| amp * (i as f32 * 0.05).sin()).collect()
    }

    fn block(amp_level: f32) -> Vec<f32> {
        // уровень rms×4 = amp_level → амплитуда синуса = amp_level/4*√2
        sine(amp_level / 4.0 * std::f32::consts::SQRT_2, 480)
    }

    #[test]
    fn gain_200_soft_limited() {
        let _g = LOCK.lock().unwrap();
        set_params(2.0, false, true, 0.1);
        let mut b = sine(0.9, 4800);
        apply_gain(&mut b, 1.0);
        let max = b.iter().fold(0f32, |m, s| m.max(s.abs()));
        assert!(max <= 1.0 && max > 0.9, "{max}");
        let mut q = sine(0.1, 480);
        apply_gain(&mut q, 32768.0);
        assert!((q.iter().fold(0f32, |m, s| m.max(s.abs())) - 0.2).abs() < 0.01);
        set_params(1.0, false, true, 0.1);
    }

    #[test]
    fn gate_closes_below_threshold_after_hold() {
        let _g = LOCK.lock().unwrap();
        set_params(1.0, true, false, 0.2);
        let mut g = Gate::new();
        let (_, open) = g.process(&mut block(0.5), 1.0, 48_000);
        assert!(open);
        // 290 мс тихого — ещё открыт
        for _ in 0..29 {
            assert!(g.process(&mut block(0.05), 1.0, 48_000).1);
        }
        for _ in 0..3 {
            g.process(&mut block(0.05), 1.0, 48_000);
        }
        let mut b = block(0.05);
        let (lvl, open) = g.process(&mut b, 1.0, 48_000);
        assert!(!open && (lvl - 0.05).abs() < 0.01, "уровень до порога: {lvl}");
        assert!(b.iter().all(|s| *s == 0.0));
        set_params(1.0, false, true, 0.1);
    }

    #[test]
    fn gate_off_passes_everything() {
        let _g = LOCK.lock().unwrap();
        set_params(1.0, false, false, 0.9);
        let mut g = Gate::new();
        let orig = block(0.05);
        let mut b = orig.clone();
        for _ in 0..50 {
            b = orig.clone();
            assert!(g.process(&mut b, 1.0, 48_000).1);
        }
        assert_eq!(b, orig);
    }

    #[test]
    fn auto_threshold_tracks_floor() {
        let _g = LOCK.lock().unwrap();
        set_params(1.0, true, true, 0.9);
        let mut g = Gate::new();
        for _ in 0..3000 {
            g.process(&mut block(0.05), 1.0, 48_000); // 30 с постоянного шума
        }
        let t = threshold();
        assert!((0.1..=0.2).contains(&t), "{t}");
        assert!(!g.process(&mut block(0.05), 1.0, 48_000).1, "шум ниже автопорога");
        assert!(g.process(&mut block(0.4), 1.0, 48_000).1, "речь выше");
        for _ in 0..100 {
            g.process(&mut vec![0.0; 480], 1.0, 48_000);
        }
        assert!(threshold() >= AUTO_MIN);
        set_params(1.0, false, true, 0.1);
    }
}
