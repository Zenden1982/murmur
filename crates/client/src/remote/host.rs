//! Автомат доступа у стримера: кто с указкой, кто управляет, пауза «друг главнее», таймауты.
//! Чистая логика: время передаётся снаружи, сообщения наружу — `HostOut`.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::proto::{InMsg, Level, RcMsg, Session, StateEntry};

/// Запрос без ответа стримера.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// «Друг главнее»: пауза после настоящего ввода стримера.
pub const REAL_INPUT_PAUSE: Duration = Duration::from_secs(1);
/// Нет пакетов от управляющего — пауза.
pub const SILENCE_PAUSE: Duration = Duration::from_secs(3);
/// Нет пакетов от управляющего — доступ сброшен.
pub const SILENCE_REVOKE: Duration = Duration::from_secs(15);
/// Не больше событий ввода в секунду.
pub const MAX_EVENTS_PER_SEC: u32 = 500;

#[derive(Debug, Clone, PartialEq)]
pub enum HostOut {
    /// Сообщение одному зрителю (reliable).
    Send { to: String, msg: RcMsg },
    /// Разослать `State` зрителям трансляции.
    BroadcastState,
    /// Отпустить всё, что зажато по командам управляющего.
    ReleaseAll,
    /// Показать стримеру запрос.
    Ask { who: String, level: Level },
    /// Запрос снят (тайм-аут, ушёл).
    Cancelled { who: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Inject,
    Drop(&'static str),
}

#[derive(Debug)]
struct Grant {
    level: Level,
    session: Session,
}

#[derive(Debug, Default)]
pub struct HostAccess {
    grants: BTreeMap<String, Grant>,
    pending: BTreeMap<String, (Level, Instant)>,
    controller: Option<String>,
    /// Последний пакет от управляющего.
    last_seen: Option<Instant>,
    last_seq: u64,
    paused_until: Option<Instant>,
    silence_paused: bool,
    /// Какое «на паузе» последним разослано зрителям.
    announced_paused: bool,
    rate: Option<(Instant, u32)>,
}

impl HostAccess {
    pub fn on_rc(&mut self, from: &str, msg: &RcMsg, now: Instant) -> Vec<HostOut> {
        match msg {
            RcMsg::Request { level } => {
                if self.grants.get(from).is_some_and(|g| g.level >= *level) {
                    return Vec::new();
                }
                self.ask(from, *level, now)
            }
            RcMsg::Take { session, on } => {
                let Some(g) = self.grants.get(from) else { return Vec::new() };
                if &g.session != session {
                    return Vec::new();
                }
                if !*on {
                    if self.controller.as_deref() == Some(from) {
                        return self.drop_controller();
                    }
                    return Vec::new();
                }
                if g.level < Level::Control {
                    return self.ask(from, Level::Control, now);
                }
                if self.controller.as_deref() == Some(from) {
                    return Vec::new();
                }
                let mut out = Vec::new();
                if self.controller.is_some() {
                    out.push(HostOut::ReleaseAll);
                }
                self.controller = Some(from.to_string());
                self.last_seen = Some(now);
                self.last_seq = 0;
                self.silence_paused = false;
                self.rate = None;
                out.push(HostOut::BroadcastState);
                out
            }
            _ => Vec::new(),
        }
    }

    fn ask(&mut self, who: &str, level: Level, now: Instant) -> Vec<HostOut> {
        let e = self.pending.entry(who.to_string()).or_insert((level, now));
        e.0 = e.0.max(level);
        vec![HostOut::Ask { who: who.to_string(), level: e.0 }]
    }

    fn drop_controller(&mut self) -> Vec<HostOut> {
        self.controller = None;
        self.last_seen = None;
        self.silence_paused = false;
        self.paused_until = None;
        vec![HostOut::ReleaseAll, HostOut::BroadcastState]
    }

    /// Ответ стримера на запрос. Новое разрешение — новая сессия.
    pub fn respond(&mut self, who: &str, grant: Option<Level>, session: Session, _now: Instant) -> Vec<HostOut> {
        let was_pending = self.pending.remove(who).is_some();
        let Some(level) = grant else {
            return if was_pending { vec![HostOut::Send { to: who.to_string(), msg: RcMsg::Deny }] } else { Vec::new() };
        };
        let mut out = Vec::new();
        if self.controller.as_deref() == Some(who) && level < Level::Control {
            out.extend(self.drop_controller());
        }
        self.grants.insert(who.to_string(), Grant { level, session: session.clone() });
        out.push(HostOut::Send { to: who.to_string(), msg: RcMsg::Grant { level, session } });
        out.push(HostOut::BroadcastState);
        out
    }

    /// «Забрать» / Ctrl+Alt+Пробел / конец трансляции: всё сбросить.
    pub fn revoke_all(&mut self) -> Vec<HostOut> {
        let mut out = Vec::new();
        if self.controller.is_some() {
            out.extend(self.drop_controller());
        }
        for who in std::mem::take(&mut self.grants).into_keys() {
            out.push(HostOut::Send { to: who, msg: RcMsg::Revoke });
        }
        for who in std::mem::take(&mut self.pending).into_keys() {
            out.push(HostOut::Send { to: who.clone(), msg: RcMsg::Deny });
            out.push(HostOut::Cancelled { who });
        }
        if !out.iter().any(|o| matches!(o, HostOut::BroadcastState)) {
            out.push(HostOut::BroadcastState);
        }
        out
    }

    pub fn on_stream_stopped(&mut self) -> Vec<HostOut> {
        self.revoke_all()
    }

    pub fn on_left(&mut self, who: &str) -> Vec<HostOut> {
        let mut out = Vec::new();
        if self.controller.as_deref() == Some(who) {
            out.extend(self.drop_controller());
        }
        if self.grants.remove(who).is_some() && !out.iter().any(|o| matches!(o, HostOut::BroadcastState)) {
            out.push(HostOut::BroadcastState);
        }
        if self.pending.remove(who).is_some() {
            out.push(HostOut::Cancelled { who: who.to_string() });
        }
        out
    }

    /// Настоящий ввод стримера: управление замирает на секунду, всё зажатое отпускается.
    pub fn on_real_input(&mut self, now: Instant) -> Vec<HostOut> {
        if self.controller.is_none() {
            return Vec::new();
        }
        let was = self.paused(now);
        self.paused_until = Some(now + REAL_INPUT_PAUSE);
        if was {
            return Vec::new();
        }
        self.announced_paused = true;
        vec![HostOut::ReleaseAll, HostOut::BroadcastState]
    }

    pub fn tick(&mut self, now: Instant) -> Vec<HostOut> {
        let mut out = Vec::new();
        let expired: Vec<String> =
            self.pending.iter().filter(|(_, (_, at))| now.duration_since(*at) > REQUEST_TIMEOUT).map(|(w, _)| w.clone()).collect();
        for who in expired {
            self.pending.remove(&who);
            out.push(HostOut::Send { to: who.clone(), msg: RcMsg::Deny });
            out.push(HostOut::Cancelled { who });
        }
        if let (Some(who), Some(seen)) = (self.controller.clone(), self.last_seen) {
            let quiet = now.duration_since(seen);
            if quiet > SILENCE_REVOKE {
                out.extend(self.drop_controller());
                self.grants.remove(&who);
                out.push(HostOut::Send { to: who, msg: RcMsg::Revoke });
            } else if quiet > SILENCE_PAUSE && !self.silence_paused {
                self.silence_paused = true;
                out.push(HostOut::ReleaseAll);
            }
        }
        let paused = self.paused(now);
        if paused != self.announced_paused {
            self.announced_paused = paused;
            if !out.iter().any(|o| matches!(o, HostOut::BroadcastState)) {
                out.push(HostOut::BroadcastState);
            }
        }
        out
    }

    fn touch(&mut self, from: &str, now: Instant) {
        if self.controller.as_deref() == Some(from) {
            self.last_seen = Some(now);
            self.silence_paused = false;
        }
    }

    /// Положение указки: только от того, у кого есть доступ, с его текущей сессией.
    pub fn accept_ptr(&mut self, from: &str, s: &Session, now: Instant) -> bool {
        let ok = self.grants.get(from).is_some_and(|g| &g.session == s);
        if ok {
            self.touch(from, now);
        }
        ok
    }

    pub fn accept_input(&mut self, from: &str, m: &InMsg, now: Instant) -> Verdict {
        if self.controller.as_deref() != Some(from) {
            return Verdict::Drop("не управляет");
        }
        if self.grants.get(from).is_none_or(|g| g.session != m.s) {
            return Verdict::Drop("чужая сессия");
        }
        self.touch(from, now);
        if self.paused(now) {
            return Verdict::Drop("пауза");
        }
        if m.seq <= self.last_seq {
            return Verdict::Drop("повтор");
        }
        let (start, n) = match self.rate {
            Some((start, n)) if now.duration_since(start) < Duration::from_secs(1) => (start, n),
            _ => (now, 0),
        };
        if n >= MAX_EVENTS_PER_SEC {
            self.rate = Some((start, n));
            return Verdict::Drop("слишком часто");
        }
        self.rate = Some((start, n + 1));
        self.last_seq = m.seq;
        Verdict::Inject
    }

    pub fn paused(&self, now: Instant) -> bool {
        self.controller.is_some() && (self.silence_paused || self.paused_until.is_some_and(|t| now < t))
    }

    pub fn state(&self, now: Instant) -> Vec<StateEntry> {
        let paused = self.paused(now);
        self.grants
            .iter()
            .map(|(who, g)| {
                let controlling = self.controller.as_deref() == Some(who.as_str());
                StateEntry { who: who.clone(), level: g.level, controlling, paused: controlling && paused }
            })
            .collect()
    }

    /// Запросы в порядке поступления.
    pub fn pending(&self) -> Vec<(String, Level)> {
        let mut v: Vec<(&String, &(Level, Instant))> = self.pending.iter().collect();
        v.sort_by_key(|(_, (_, at))| *at);
        v.into_iter().map(|(w, (l, _))| (w.clone(), *l)).collect()
    }

    pub fn controller(&self) -> Option<&str> {
        self.controller.as_deref()
    }

    /// Есть хоть один доступ (указка или управление).
    pub fn is_active(&self) -> bool {
        !self.grants.is_empty()
    }

    pub fn has_grant(&self, who: &str) -> bool {
        self.grants.contains_key(who)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::proto::InputEv;

    fn s(n: u8) -> Session {
        Session(format!("{n:032x}"))
    }
    fn ms(t0: Instant, v: u64) -> Instant {
        t0 + Duration::from_millis(v)
    }
    fn key(sess: &Session, seq: u64) -> InMsg {
        InMsg { s: sess.clone(), seq, ev: InputEv::Key { sc: 30, ext: false, down: true } }
    }
    fn sent(out: &[HostOut], to: &str) -> Vec<RcMsg> {
        out.iter()
            .filter_map(|o| match o {
                HostOut::Send { to: t, msg } if t == to => Some(msg.clone()),
                _ => None,
            })
            .collect()
    }
    fn has_release(out: &[HostOut]) -> bool {
        out.iter().any(|o| matches!(o, HostOut::ReleaseAll))
    }
    /// a получает Control и берёт управление.
    fn controlling(h: &mut HostAccess, t0: Instant, who: &str, sess: &Session) {
        h.on_rc(who, &RcMsg::Request { level: Level::Control }, t0);
        h.respond(who, Some(Level::Control), sess.clone(), t0);
        h.on_rc(who, &RcMsg::Take { session: sess.clone(), on: true }, t0);
        assert_eq!(h.controller(), Some(who));
    }

    #[test]
    fn request_then_pointer_grant() {
        let t0 = Instant::now();
        let mut h = HostAccess::default();
        let out = h.on_rc("a", &RcMsg::Request { level: Level::Pointer }, t0);
        assert!(out.iter().any(|o| matches!(o, HostOut::Ask { who, level: Level::Pointer } if who == "a")));
        assert_eq!(h.pending(), vec![("a".to_string(), Level::Pointer)]);
        let out = h.respond("a", Some(Level::Pointer), s(1), t0);
        assert_eq!(sent(&out, "a"), vec![RcMsg::Grant { level: Level::Pointer, session: s(1) }]);
        assert!(out.iter().any(|o| matches!(o, HostOut::BroadcastState)));
        assert!(h.pending().is_empty());
        assert!(h.accept_ptr("a", &s(1), t0));
        assert!(!h.accept_ptr("a", &s(2), t0));
        assert!(!h.accept_ptr("b", &s(1), t0));
        assert_eq!(h.state(t0), vec![StateEntry { who: "a".into(), level: Level::Pointer, controlling: false, paused: false }]);
        // указка не даёт ввода
        assert!(matches!(h.accept_input("a", &key(&s(1), 1), t0), Verdict::Drop(_)));
    }

    #[test]
    fn deny_sends_deny() {
        let t0 = Instant::now();
        let mut h = HostAccess::default();
        h.on_rc("a", &RcMsg::Request { level: Level::Control }, t0);
        let out = h.respond("a", None, s(1), t0);
        assert_eq!(sent(&out, "a"), vec![RcMsg::Deny]);
        assert!(!h.is_active());
        assert!(h.pending().is_empty());
    }

    #[test]
    fn request_timeout_30s() {
        let t0 = Instant::now();
        let mut h = HostAccess::default();
        h.on_rc("a", &RcMsg::Request { level: Level::Pointer }, t0);
        assert!(h.tick(ms(t0, 29_000)).is_empty());
        let out = h.tick(ms(t0, 30_001));
        assert_eq!(sent(&out, "a"), vec![RcMsg::Deny]);
        assert!(out.iter().any(|o| matches!(o, HostOut::Cancelled { who } if who == "a")));
        assert!(h.pending().is_empty());
    }

    #[test]
    fn take_without_control_asks() {
        let t0 = Instant::now();
        let mut h = HostAccess::default();
        h.on_rc("a", &RcMsg::Request { level: Level::Pointer }, t0);
        h.respond("a", Some(Level::Pointer), s(1), t0);
        let out = h.on_rc("a", &RcMsg::Take { session: s(1), on: true }, t0);
        assert!(out.iter().any(|o| matches!(o, HostOut::Ask { who, level: Level::Control } if who == "a")));
        assert_eq!(h.controller(), None);
        // разрешили управление — новая сессия, старая больше не годится
        h.respond("a", Some(Level::Control), s(2), t0);
        assert!(!h.accept_ptr("a", &s(1), t0));
        h.on_rc("a", &RcMsg::Take { session: s(2), on: true }, t0);
        assert_eq!(h.controller(), Some("a"));
        assert!(matches!(h.accept_input("a", &key(&s(2), 1), t0), Verdict::Inject));
    }

    #[test]
    fn take_with_wrong_session_ignored() {
        let t0 = Instant::now();
        let mut h = HostAccess::default();
        h.on_rc("a", &RcMsg::Request { level: Level::Control }, t0);
        h.respond("a", Some(Level::Control), s(1), t0);
        assert!(h.on_rc("a", &RcMsg::Take { session: s(9), on: true }, t0).is_empty());
        assert_eq!(h.controller(), None);
    }

    #[test]
    fn second_controller_replaces_first() {
        let t0 = Instant::now();
        let mut h = HostAccess::default();
        controlling(&mut h, t0, "a", &s(1));
        h.on_rc("b", &RcMsg::Request { level: Level::Control }, t0);
        h.respond("b", Some(Level::Control), s(2), t0);
        assert_eq!(h.controller(), Some("a"));
        let out = h.on_rc("b", &RcMsg::Take { session: s(2), on: true }, t0);
        assert!(has_release(&out));
        assert_eq!(h.controller(), Some("b"));
        assert!(matches!(h.accept_input("a", &key(&s(1), 5), t0), Verdict::Drop(_)));
        assert!(matches!(h.accept_input("b", &key(&s(2), 1), t0), Verdict::Inject));
        let st = h.state(t0);
        assert_eq!(st.len(), 2);
        assert!(st.iter().any(|e| e.who == "a" && !e.controlling && e.level == Level::Control));
        assert!(h.accept_ptr("a", &s(1), t0), "первый остаётся с указкой");
    }

    #[test]
    fn take_off_releases() {
        let t0 = Instant::now();
        let mut h = HostAccess::default();
        controlling(&mut h, t0, "a", &s(1));
        let out = h.on_rc("a", &RcMsg::Take { session: s(1), on: false }, t0);
        assert!(has_release(&out));
        assert_eq!(h.controller(), None);
        assert!(h.accept_ptr("a", &s(1), t0));
    }

    #[test]
    fn stale_session_dropped() {
        let t0 = Instant::now();
        let mut h = HostAccess::default();
        controlling(&mut h, t0, "a", &s(1));
        h.revoke_all();
        controlling(&mut h, t0, "a", &s(2));
        assert!(matches!(h.accept_input("a", &key(&s(1), 100), t0), Verdict::Drop(_)));
        assert!(matches!(h.accept_input("a", &key(&s(2), 1), t0), Verdict::Inject));
    }

    #[test]
    fn real_input_pauses_1s_and_releases() {
        let t0 = Instant::now();
        let mut h = HostAccess::default();
        controlling(&mut h, t0, "a", &s(1));
        let out = h.on_real_input(t0);
        assert!(has_release(&out));
        assert!(out.iter().any(|o| matches!(o, HostOut::BroadcastState)));
        assert!(h.paused(ms(t0, 500)));
        assert!(matches!(h.accept_input("a", &key(&s(1), 1), ms(t0, 500)), Verdict::Drop(_)));
        // повторный настоящий ввод во время паузы — продлевает, без лишних ReleaseAll
        let out = h.on_real_input(ms(t0, 800));
        assert!(!has_release(&out));
        assert!(h.paused(ms(t0, 1700)));
        assert!(!h.paused(ms(t0, 1801)));
        let out = h.tick(ms(t0, 1801));
        assert!(out.iter().any(|o| matches!(o, HostOut::BroadcastState)), "конец паузы объявляется");
        assert!(matches!(h.accept_input("a", &key(&s(1), 2), ms(t0, 1802)), Verdict::Inject));
        // без управляющего настоящий ввод ничего не делает
        let mut h2 = HostAccess::default();
        assert!(h2.on_real_input(t0).is_empty());
    }

    #[test]
    fn silence_3s_pauses_15s_revokes() {
        let t0 = Instant::now();
        let mut h = HostAccess::default();
        controlling(&mut h, t0, "a", &s(1));
        assert!(h.tick(ms(t0, 2900)).is_empty());
        let out = h.tick(ms(t0, 3100));
        assert!(has_release(&out));
        assert!(h.paused(ms(t0, 3100)));
        // связь вернулась
        assert!(h.accept_ptr("a", &s(1), ms(t0, 4000)));
        assert!(!h.paused(ms(t0, 4000)));
        let out = h.tick(ms(t0, 19_100));
        assert_eq!(sent(&out, "a"), vec![RcMsg::Revoke]);
        assert_eq!(h.controller(), None);
        assert!(!h.is_active());
    }

    #[test]
    fn stream_stopped_revokes_all() {
        let t0 = Instant::now();
        let mut h = HostAccess::default();
        controlling(&mut h, t0, "a", &s(1));
        h.on_rc("b", &RcMsg::Request { level: Level::Pointer }, t0);
        h.respond("b", Some(Level::Pointer), s(2), t0);
        h.on_rc("c", &RcMsg::Request { level: Level::Pointer }, t0);
        let out = h.on_stream_stopped();
        assert!(has_release(&out));
        assert_eq!(sent(&out, "a"), vec![RcMsg::Revoke]);
        assert_eq!(sent(&out, "b"), vec![RcMsg::Revoke]);
        assert_eq!(sent(&out, "c"), vec![RcMsg::Deny]);
        assert!(!h.is_active());
        assert!(h.pending().is_empty());
        assert!(matches!(h.accept_input("a", &key(&s(1), 9), t0), Verdict::Drop(_)));
    }

    #[test]
    fn left_participant_revoked() {
        let t0 = Instant::now();
        let mut h = HostAccess::default();
        controlling(&mut h, t0, "a", &s(1));
        let out = h.on_left("a");
        assert!(has_release(&out));
        assert!(!h.is_active());
        h.on_rc("b", &RcMsg::Request { level: Level::Pointer }, t0);
        let out = h.on_left("b");
        assert!(out.iter().any(|o| matches!(o, HostOut::Cancelled { who } if who == "b")));
    }

    #[test]
    fn rate_limit_500_per_sec() {
        let t0 = Instant::now();
        let mut h = HostAccess::default();
        controlling(&mut h, t0, "a", &s(1));
        let mut ok = 0;
        for i in 1..=600u64 {
            if matches!(h.accept_input("a", &key(&s(1), i), ms(t0, i)), Verdict::Inject) {
                ok += 1;
            }
        }
        assert_eq!(ok, 500);
        assert!(matches!(h.accept_input("a", &key(&s(1), 601), ms(t0, 1100)), Verdict::Inject));
    }

    #[test]
    fn seq_must_increase() {
        let t0 = Instant::now();
        let mut h = HostAccess::default();
        controlling(&mut h, t0, "a", &s(1));
        assert!(matches!(h.accept_input("a", &key(&s(1), 5), t0), Verdict::Inject));
        assert!(matches!(h.accept_input("a", &key(&s(1), 5), t0), Verdict::Drop(_)));
        assert!(matches!(h.accept_input("a", &key(&s(1), 3), t0), Verdict::Drop(_)));
        assert!(matches!(h.accept_input("a", &key(&s(1), 6), t0), Verdict::Inject));
    }

    #[test]
    fn outsider_input_dropped() {
        let t0 = Instant::now();
        let mut h = HostAccess::default();
        controlling(&mut h, t0, "a", &s(1));
        assert!(matches!(h.accept_input("x", &key(&s(1), 1), t0), Verdict::Drop(_)));
        assert!(h.on_rc("x", &RcMsg::Take { session: s(1), on: true }, t0).is_empty());
        assert_eq!(h.controller(), Some("a"));
        // чужие Grant/State/Revoke от зрителя игнорируются
        assert!(h.on_rc("x", &RcMsg::Revoke, t0).is_empty());
        assert_eq!(h.controller(), Some("a"));
    }

    #[test]
    fn pending_in_arrival_order() {
        let t0 = Instant::now();
        let mut h = HostAccess::default();
        h.on_rc("zed", &RcMsg::Request { level: Level::Pointer }, t0);
        h.on_rc("abe", &RcMsg::Request { level: Level::Pointer }, ms(t0, 10));
        assert_eq!(h.pending()[0].0, "zed", "первым — кто раньше попросил, не по алфавиту");
    }

    #[test]
    fn downgrade_controller_to_pointer_releases() {
        let t0 = Instant::now();
        let mut h = HostAccess::default();
        controlling(&mut h, t0, "a", &s(1));
        let out = h.respond("a", Some(Level::Pointer), s(2), t0);
        assert!(has_release(&out));
        assert_eq!(h.controller(), None);
    }
}
