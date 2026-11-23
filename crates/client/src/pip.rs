//! Окно в окне: какой стрим показывать в мини-плеере и когда плеер нужен. Чистая логика без egui — тесты ниже.

use std::collections::HashSet;

/// Не переключать показанный стрим чаще этого (секунды), чтобы картинка не мигала от каждой реплики.
pub const SWITCH_MIN: f64 = 3.5;

/// Участник звонка, как его видит выбор стрима.
#[derive(Debug, Clone, Copy)]
pub struct PeerInfo<'a> {
    pub identity: &'a str,
    pub speaking: bool,
    pub streaming: bool,
}

#[derive(Debug, Default)]
pub struct Pip {
    shown: Option<String>,
    /// Когда (секунды) показанный стрим выбран.
    since: f64,
    pinned: Option<String>,
    dismissed: bool,
    /// Включено вручную кнопкой на плитке: показывать, даже если стрим виден на сцене.
    forced: bool,
}

impl Pip {
    /// Какой из просматриваемых стримов показывать сейчас (None — никакой не смотрим).
    pub fn pick(&mut self, now: f64, watching: &[String], peers: &[PeerInfo]) -> Option<String> {
        if self.pinned.as_ref().is_some_and(|p| !watching.contains(p)) {
            self.pinned = None;
        }
        if let Some(p) = &self.pinned {
            if self.shown.as_ref() != Some(p) {
                self.shown = Some(p.clone());
                self.since = now;
            }
            return self.shown.clone();
        }
        if watching.is_empty() {
            self.shown = None;
            return None;
        }
        let speaker = peers.iter().find(|p| p.speaking && p.streaming && watching.contains(&p.identity.to_string())).map(|p| p.identity.to_string());
        let current_ok = self.shown.as_ref().is_some_and(|s| watching.contains(s));
        if !current_ok {
            // показанный стрим закончился (или показа ещё не было) — выбрать сразу, без выдержки
            self.shown = speaker.or_else(|| watching.first().cloned());
            self.since = now;
        } else if let Some(c) = speaker
            && self.shown.as_ref() != Some(&c)
            && now - self.since >= SWITCH_MIN
        {
            self.shown = Some(c);
            self.since = now;
        }
        self.shown.clone()
    }

    /// Закрепить показанный стрим (None — снять).
    pub fn pin(&mut self, identity: Option<String>) {
        self.pinned = identity;
    }

    pub fn pinned(&self) -> Option<&str> {
        self.pinned.as_deref()
    }

    /// Крестик на плеере: не показывать до `reset`.
    pub fn dismiss(&mut self) {
        self.dismissed = true;
        self.forced = false;
    }

    /// Начали смотреть новый стрим или заново вошли в звонок.
    pub fn reset(&mut self) {
        self.dismissed = false;
    }

    /// Кнопка «Окно в окне» на плитке: показать плеер сейчас.
    pub fn force(&mut self) {
        self.forced = true;
        self.dismissed = false;
    }

    /// Нужен ли плеер: выбранный стрим не нарисован на сцене (`drawn`), плеер не закрыт крестиком; вручную — всегда.
    pub fn wanted(&self, picked: &str, drawn: &HashSet<String>) -> bool {
        !self.dismissed && (self.forced || !drawn.contains(picked))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }
    fn p<'a>(identity: &'a str, speaking: bool, streaming: bool) -> PeerInfo<'a> {
        PeerInfo { identity, speaking, streaming }
    }

    #[test]
    fn one_stream_always_shown() {
        let mut pip = Pip::default();
        assert_eq!(pip.pick(0.0, &w(&["a"]), &[p("a", false, true), p("b", true, false)]), Some("a".into()));
        assert_eq!(pip.pick(100.0, &w(&["a"]), &[p("b", true, false)]), Some("a".into()));
    }

    #[test]
    fn nothing_watched_nothing_shown() {
        let mut pip = Pip::default();
        assert_eq!(pip.pick(0.0, &w(&[]), &[]), None);
    }

    #[test]
    fn speaker_with_stream_wins() {
        let mut pip = Pip::default();
        let peers = [p("a", false, true), p("b", true, true)];
        assert_eq!(pip.pick(0.0, &w(&["a", "b"]), &peers), Some("b".into()));
    }

    #[test]
    fn no_switch_before_min_interval() {
        let mut pip = Pip::default();
        let watching = w(&["a", "b"]);
        assert_eq!(pip.pick(0.0, &watching, &[p("a", true, true), p("b", false, true)]), Some("a".into()));
        let b_speaks = [p("a", false, true), p("b", true, true)];
        assert_eq!(pip.pick(SWITCH_MIN - 0.1, &watching, &b_speaks), Some("a".into()));
        assert_eq!(pip.pick(SWITCH_MIN + 0.1, &watching, &b_speaks), Some("b".into()));
    }

    #[test]
    fn speaker_without_stream_keeps_last() {
        let mut pip = Pip::default();
        let watching = w(&["a", "b"]);
        assert_eq!(pip.pick(0.0, &watching, &[p("b", true, true)]), Some("b".into()));
        assert_eq!(pip.pick(10.0, &watching, &[p("a", false, true), p("c", true, false)]), Some("b".into()));
    }

    #[test]
    fn nobody_spoke_takes_first_watched() {
        let mut pip = Pip::default();
        assert_eq!(pip.pick(0.0, &w(&["x", "y"]), &[p("x", false, true), p("y", false, true)]), Some("x".into()));
    }

    #[test]
    fn ended_stream_switches_immediately() {
        let mut pip = Pip::default();
        assert_eq!(pip.pick(0.0, &w(&["a", "b"]), &[]), Some("a".into()));
        // a закончился через 0,5 с — выдержка не нужна
        assert_eq!(pip.pick(0.5, &w(&["b"]), &[]), Some("b".into()));
        assert_eq!(pip.pick(0.6, &w(&[]), &[]), None);
    }

    #[test]
    fn two_speakers_do_not_flicker() {
        let mut pip = Pip::default();
        let watching = w(&["a", "b"]);
        let both = [p("a", true, true), p("b", true, true)];
        let first = pip.pick(0.0, &watching, &both);
        // оба говорят — выбор остаётся тем же, пока не пройдёт выдержка, и дальше не прыгает без причины
        for i in 1..100 {
            let cur = pip.pick(i as f64 * 0.1, &watching, &both);
            assert!(cur.is_some());
        }
        assert_eq!(first, Some("a".into()));
        assert_eq!(pip.pick(10.0, &watching, &both), Some("a".into()));
    }

    #[test]
    fn pin_overrides_and_drops_when_gone() {
        let mut pip = Pip::default();
        let watching = w(&["a", "b"]);
        pip.pin(Some("a".into()));
        assert_eq!(pip.pick(0.0, &watching, &[p("b", true, true)]), Some("a".into()));
        assert_eq!(pip.pick(20.0, &watching, &[p("b", true, true)]), Some("a".into()));
        // закреплённый пропал — закрепление снято, работает обычное правило
        assert_eq!(pip.pick(21.0, &w(&["b"]), &[p("b", true, true)]), Some("b".into()));
        assert_eq!(pip.pinned(), None);
    }

    #[test]
    fn dismissed_hides_until_reset() {
        let mut pip = Pip::default();
        let drawn = HashSet::new();
        assert!(pip.wanted("a", &drawn));
        pip.dismiss();
        assert!(!pip.wanted("a", &drawn));
        pip.reset();
        assert!(pip.wanted("a", &drawn));
    }

    #[test]
    fn wanted_false_when_drawn_on_scene() {
        let pip = Pip::default();
        let drawn: HashSet<String> = ["a".to_string()].into();
        assert!(!pip.wanted("a", &drawn));
        assert!(pip.wanted("b", &drawn));
    }

    #[test]
    fn forced_shows_even_when_drawn_and_dismiss_cancels_it() {
        let mut pip = Pip::default();
        let drawn: HashSet<String> = ["a".to_string()].into();
        pip.force();
        assert!(pip.wanted("a", &drawn));
        pip.dismiss();
        assert!(!pip.wanted("a", &drawn));
    }
}
