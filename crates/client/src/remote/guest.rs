//! Автомат зрителя (на одного стримера): запрос → разрешение → управление.

use std::time::{Duration, Instant};

use super::proto::{Level, RcMsg, Session};

/// Сколько ждать ответа стримера (у него тайм-аут 30 с).
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(35);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestPhase {
    Idle,
    Requested { level: Level },
    Granted { level: Level, session: Session, controlling: bool },
}

#[derive(Debug)]
pub struct GuestAccess {
    phase: GuestPhase,
    /// Когда отправлен запрос (первый или на управление поверх указки).
    asked_at: Option<Instant>,
    /// Попросили управление кнопкой «Взять управление»: после разрешения сразу взять.
    want_take: bool,
    take_now: bool,
    paused: bool,
    layout: String,
}

impl Default for GuestAccess {
    fn default() -> Self {
        Self { phase: GuestPhase::Idle, asked_at: None, want_take: false, take_now: false, paused: false, layout: String::new() }
    }
}

impl GuestAccess {
    pub fn request(&mut self, level: Level, now: Instant) -> Option<RcMsg> {
        match &self.phase {
            GuestPhase::Granted { level: have, .. } if *have >= level => return None,
            GuestPhase::Granted { .. } => {}
            _ => self.phase = GuestPhase::Requested { level },
        }
        self.asked_at = Some(now);
        Some(RcMsg::Request { level })
    }

    /// Сообщение от стримера. Возвращает текст для уведомления.
    pub fn on_rc(&mut self, msg: &RcMsg, me: &str) -> Option<String> {
        match msg {
            RcMsg::Grant { level, session } => {
                self.asked_at = None;
                self.phase = GuestPhase::Granted { level: *level, session: session.clone(), controlling: false };
                if self.want_take && *level == Level::Control {
                    self.take_now = true;
                }
                self.want_take = false;
                None
            }
            RcMsg::Deny => {
                self.asked_at = None;
                self.want_take = false;
                match self.phase {
                    GuestPhase::Requested { .. } => {
                        self.phase = GuestPhase::Idle;
                        Some("Друг отказал".into())
                    }
                    GuestPhase::Granted { .. } => Some("Друг не разрешил управление".into()),
                    GuestPhase::Idle => None,
                }
            }
            RcMsg::Revoke => {
                let had = !matches!(self.phase, GuestPhase::Idle);
                self.reset();
                had.then(|| "Друг забрал доступ".into())
            }
            RcMsg::State { entries, layout } => {
                self.layout = layout.clone();
                if let GuestPhase::Granted { level, controlling, .. } = &mut self.phase {
                    match entries.iter().find(|e| e.who == me) {
                        Some(e) => {
                            *level = e.level;
                            *controlling = e.controlling;
                            self.paused = e.paused;
                        }
                        None => {
                            self.reset();
                            return Some("Доступ закончился".into());
                        }
                    }
                }
                None
            }
            _ => None,
        }
    }

    fn reset(&mut self) {
        self.phase = GuestPhase::Idle;
        self.asked_at = None;
        self.want_take = false;
        self.take_now = false;
        self.paused = false;
    }

    /// «Взять управление» / «Вернуть указку». Без права управления — запрос стримеру.
    pub fn take(&mut self, on: bool, now: Instant) -> Option<RcMsg> {
        let GuestPhase::Granted { level, session, controlling } = &mut self.phase else { return None };
        if on && *level < Level::Control {
            self.want_take = true;
            self.asked_at = Some(now);
            return Some(RcMsg::Request { level: Level::Control });
        }
        if *controlling == on {
            return None;
        }
        *controlling = on;
        Some(RcMsg::Take { session: session.clone(), on })
    }

    /// После разрешения управления по «Взять управление» — сообщение Take (один раз).
    pub fn pending_take(&mut self) -> Option<RcMsg> {
        if !std::mem::take(&mut self.take_now) {
            return None;
        }
        self.take(true, Instant::now())
    }

    pub fn tick(&mut self, now: Instant) -> Option<String> {
        let at = self.asked_at?;
        if now.duration_since(at) <= ANSWER_TIMEOUT {
            return None;
        }
        self.asked_at = None;
        self.want_take = false;
        if matches!(self.phase, GuestPhase::Requested { .. }) {
            self.phase = GuestPhase::Idle;
        }
        Some("Нет ответа — возможно, у друга старая версия Murmur".into())
    }

    pub fn phase(&self) -> &GuestPhase {
        &self.phase
    }

    pub fn session(&self) -> Option<&Session> {
        match &self.phase {
            GuestPhase::Granted { session, .. } => Some(session),
            _ => None,
        }
    }

    pub fn controlling(&self) -> bool {
        matches!(self.phase, GuestPhase::Granted { controlling: true, .. })
    }

    /// Ждём ответа на просьбу об управлении поверх указки.
    pub fn upgrade_pending(&self) -> bool {
        self.asked_at.is_some() && matches!(self.phase, GuestPhase::Granted { .. })
    }

    /// Стример сейчас сам двигает мышь (или связь пропала).
    pub fn paused(&self) -> bool {
        self.paused && self.controlling()
    }

    pub fn layout(&self) -> &str {
        &self.layout
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::proto::StateEntry;

    fn s(n: u8) -> Session {
        Session(format!("{n:032x}"))
    }
    fn state(who: &str, level: Level, controlling: bool) -> RcMsg {
        RcMsg::State { entries: vec![StateEntry { who: who.into(), level, controlling, paused: false }], layout: "RU".into() }
    }

    #[test]
    fn request_grant_take_release() {
        let t0 = Instant::now();
        let mut g = GuestAccess::default();
        assert_eq!(g.request(Level::Control, t0), Some(RcMsg::Request { level: Level::Control }));
        assert_eq!(g.phase(), &GuestPhase::Requested { level: Level::Control });
        assert_eq!(g.on_rc(&RcMsg::Grant { level: Level::Control, session: s(1) }, "me"), None);
        assert_eq!(g.session(), Some(&s(1)));
        assert!(!g.controlling());
        assert_eq!(g.take(true, t0), Some(RcMsg::Take { session: s(1), on: true }));
        assert!(g.controlling());
        g.on_rc(&state("me", Level::Control, true), "me");
        assert!(g.controlling());
        assert_eq!(g.layout(), "RU");
        assert_eq!(g.take(false, t0), Some(RcMsg::Take { session: s(1), on: false }));
        assert!(!g.controlling());
    }

    #[test]
    fn take_without_control_requests() {
        let t0 = Instant::now();
        let mut g = GuestAccess::default();
        g.request(Level::Pointer, t0);
        g.on_rc(&RcMsg::Grant { level: Level::Pointer, session: s(1) }, "me");
        assert_eq!(g.take(true, t0), Some(RcMsg::Request { level: Level::Control }));
        assert!(!g.controlling());
        assert!(g.upgrade_pending());
        // разрешили — сразу берём управление с новой сессией
        g.on_rc(&RcMsg::Grant { level: Level::Control, session: s(2) }, "me");
        assert_eq!(g.pending_take(), Some(RcMsg::Take { session: s(2), on: true }));
        assert!(g.controlling());
        assert_eq!(g.pending_take(), None);
    }

    #[test]
    fn upgrade_denied_keeps_pointer() {
        let t0 = Instant::now();
        let mut g = GuestAccess::default();
        g.request(Level::Pointer, t0);
        g.on_rc(&RcMsg::Grant { level: Level::Pointer, session: s(1) }, "me");
        g.take(true, t0);
        assert!(g.on_rc(&RcMsg::Deny, "me").is_some());
        assert_eq!(g.session(), Some(&s(1)));
        assert!(!g.upgrade_pending());
    }

    #[test]
    fn revoke_resets() {
        let t0 = Instant::now();
        let mut g = GuestAccess::default();
        g.request(Level::Control, t0);
        g.on_rc(&RcMsg::Grant { level: Level::Control, session: s(1) }, "me");
        g.take(true, t0);
        assert!(g.on_rc(&RcMsg::Revoke, "me").is_some());
        assert_eq!(g.phase(), &GuestPhase::Idle);
        assert!(!g.controlling());
        assert_eq!(g.take(true, t0), None);
    }

    #[test]
    fn deny_resets_request() {
        let t0 = Instant::now();
        let mut g = GuestAccess::default();
        g.request(Level::Pointer, t0);
        assert!(g.on_rc(&RcMsg::Deny, "me").is_some());
        assert_eq!(g.phase(), &GuestPhase::Idle);
    }

    #[test]
    fn state_without_me_resets() {
        let t0 = Instant::now();
        let mut g = GuestAccess::default();
        g.request(Level::Control, t0);
        // пока ждём ответа, чужое состояние не сбрасывает запрос
        g.on_rc(&state("other", Level::Pointer, false), "me");
        assert_eq!(g.phase(), &GuestPhase::Requested { level: Level::Control });
        g.on_rc(&RcMsg::Grant { level: Level::Control, session: s(1) }, "me");
        g.take(true, t0);
        g.on_rc(&state("other", Level::Control, true), "me");
        assert_eq!(g.phase(), &GuestPhase::Idle);
        // стример отдал управление другому: я остаюсь с доступом, но не управляю
        let mut g = GuestAccess::default();
        g.request(Level::Control, t0);
        g.on_rc(&RcMsg::Grant { level: Level::Control, session: s(1) }, "me");
        g.take(true, t0);
        g.on_rc(&state("me", Level::Control, false), "me");
        assert!(!g.controlling());
    }

    #[test]
    fn timeout_35s() {
        let t0 = Instant::now();
        let mut g = GuestAccess::default();
        g.request(Level::Pointer, t0);
        assert_eq!(g.tick(t0 + Duration::from_secs(34)), None);
        assert!(g.tick(t0 + Duration::from_secs(36)).is_some());
        assert_eq!(g.phase(), &GuestPhase::Idle);
    }
}
