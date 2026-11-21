//! Сглаженное состояние связи для пинга в шапке: короткие всплески потерь не пугают красным.
//! Короткие потери и джиттер — жёлтым (держится несколько секунд после последнего плохого замера), красным —
//! только если потери идут большую часть последних ~12 с.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Замер «с потерями»: доля потерь выше этого.
const LOSS_MIN: f64 = 0.02;
/// Джиттер выше этого — «дёргается».
const JITTER_BAD: f64 = 0.030;
/// Окно, по которому судим «потери надолго».
const WINDOW: Duration = Duration::from_secs(12);
/// Красное — только если история в окне не короче этого (не судим по первым замерам).
const MIN_SPAN: Duration = Duration::from_secs(8);
/// Красное: доля замеров с потерями в окне не меньше этого; уходит с красного, когда доля ниже RED_OFF.
const RED_ON: f64 = 0.6;
const RED_OFF: f64 = 0.3;
/// Жёлтое держится столько после последнего плохого замера.
const HOLD: Duration = Duration::from_secs(6);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LinkState {
    #[default]
    Fine,
    /// Джиттер (несколько замеров подряд).
    Jitter,
    /// Кратковременные потери — жёлтым, без паники.
    Brief,
    /// Потери держатся — красным.
    Loss,
}

#[derive(Default)]
pub struct LinkMeter {
    /// (время, были ли потери) за последние WINDOW.
    samples: VecDeque<(Instant, bool)>,
    last_loss: Option<Instant>,
    jitter_run: u32,
    last_jitter: Option<Instant>,
    red: bool,
}

impl LinkMeter {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Очередной замер статистики (раз в 1–2 с) → состояние для значка.
    pub fn push(&mut self, now: Instant, jitter: Option<f64>, loss: Option<f64>) -> LinkState {
        let lossy = loss.is_some_and(|l| l > LOSS_MIN);
        self.samples.push_back((now, lossy));
        while self.samples.front().is_some_and(|(t, _)| now.duration_since(*t) > WINDOW) {
            self.samples.pop_front();
        }
        if lossy {
            self.last_loss = Some(now);
        }
        if jitter.is_some_and(|j| j > JITTER_BAD) {
            self.jitter_run += 1;
            if self.jitter_run >= 2 {
                self.last_jitter = Some(now);
            }
        } else {
            self.jitter_run = 0;
        }

        let span = self.samples.front().map_or(Duration::ZERO, |(t, _)| now.duration_since(*t));
        let share = self.samples.iter().filter(|(_, l)| *l).count() as f64 / self.samples.len() as f64;
        let was_red = self.red;
        self.red = if self.red { share >= RED_OFF } else { span >= MIN_SPAN && share >= RED_ON };
        if was_red && !self.red {
            // с красного — через жёлтое, а не сразу в норму
            self.last_loss = Some(now);
        }

        let recent = |t: Option<Instant>| t.is_some_and(|t| now.duration_since(t) <= HOLD);
        if self.red {
            LinkState::Loss
        } else if recent(self.last_loss) {
            LinkState::Brief
        } else if recent(self.last_jitter) {
            LinkState::Jitter
        } else {
            LinkState::Fine
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Прогон замеров раз в 2 с; возвращает состояния.
    fn run(m: &mut LinkMeter, t0: Instant, from: u64, samples: &[(Option<f64>, Option<f64>)]) -> Vec<LinkState> {
        samples.iter().enumerate().map(|(i, (j, l))| m.push(t0 + Duration::from_secs(from + 2 * i as u64), *j, *l)).collect()
    }

    const OK: (Option<f64>, Option<f64>) = (Some(0.005), Some(0.0));
    const LOSSY: (Option<f64>, Option<f64>) = (Some(0.005), Some(0.08));
    const JIT: (Option<f64>, Option<f64>) = (Some(0.050), Some(0.0));

    #[test]
    fn short_burst_is_yellow_then_clears() {
        let (mut m, t0) = (LinkMeter::default(), Instant::now());
        let s = run(&mut m, t0, 0, &[OK, OK, OK, OK, OK, (Some(0.005), Some(0.4)), OK, OK, OK, OK, OK]);
        assert_eq!(s[4], LinkState::Fine);
        assert_eq!(s[5], LinkState::Brief, "даже 40 % за один замер — жёлтое");
        assert!(s[5..=8].iter().all(|x| *x == LinkState::Brief), "держится ~6 с: {s:?}");
        assert_eq!(s[9], LinkState::Fine);
        assert!(!s.contains(&LinkState::Loss));
    }

    #[test]
    fn sustained_loss_goes_red_and_back() {
        let (mut m, t0) = (LinkMeter::default(), Instant::now());
        let s = run(&mut m, t0, 0, &[LOSSY; 8]);
        assert_eq!(s[0], LinkState::Brief, "по первым замерам не красим");
        assert_eq!(s[3], LinkState::Brief);
        assert_eq!(s[4], LinkState::Loss, "8 с потерь подряд — красное");
        assert_eq!(s[7], LinkState::Loss);
        let s = run(&mut m, t0, 16, &[OK; 10]);
        assert_eq!(s[0], LinkState::Loss, "гистерезис: один чистый замер не снимает красное");
        let first_yellow = s.iter().position(|x| *x != LinkState::Loss).unwrap();
        assert_eq!(s[first_yellow], LinkState::Brief, "с красного — через жёлтое");
        assert_eq!(*s.last().unwrap(), LinkState::Fine);
    }

    #[test]
    fn intermittent_loss_stays_yellow() {
        let (mut m, t0) = (LinkMeter::default(), Instant::now());
        let s = run(&mut m, t0, 0, &[LOSSY, OK, OK, LOSSY, OK, OK, LOSSY, OK, OK, LOSSY, OK, OK]);
        assert!(s.iter().all(|x| *x == LinkState::Brief), "{s:?}");
    }

    #[test]
    fn small_loss_ignored_and_jitter_needs_two() {
        let (mut m, t0) = (LinkMeter::default(), Instant::now());
        let s = run(&mut m, t0, 0, &[(Some(0.005), Some(0.01)), JIT, OK, JIT, JIT, OK]);
        assert_eq!(&s[..3], &[LinkState::Fine; 3], "1 % потерь и одиночный джиттер — норма");
        assert_eq!(s[4], LinkState::Jitter);
        assert_eq!(s[5], LinkState::Jitter, "джиттер держится");
        assert_eq!(m.push(t0 + Duration::from_secs(20), None, None), LinkState::Fine);
    }

    #[test]
    fn loss_beats_jitter() {
        let (mut m, t0) = (LinkMeter::default(), Instant::now());
        let s = run(&mut m, t0, 0, &[JIT, (Some(0.05), Some(0.1))]);
        assert_eq!(s[1], LinkState::Brief);
    }
}
