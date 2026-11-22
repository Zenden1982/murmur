//! Управление трансляцией в голосовом движке: автоматы + хуки + окно указки + ввод; наружу — сообщения для LiveKit и вид для UI.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;

use super::geom::{from_norm, to_absolute};
use super::guest::{GuestAccess, GuestPhase};
use super::hooks::{HookEvent, Hooks};
use super::host::{HostAccess, HostOut, Verdict};
use super::inject::{self, Blocked, Injector, Target};
use super::keys::Held;
use super::overlay::{Overlay, OverlayPointer};
use super::proto::{self, InMsg, InputEv, Level, PtrMsg, RcMsg, Session, StateEntry};

/// Указка — не чаще (≈60 Гц).
const PTR_EVERY: Duration = Duration::from_millis(16);
/// Управляющий стоит на месте — всё равно напоминает о себе (у стримера пауза через 3 с тишины).
const HEARTBEAT: Duration = Duration::from_millis(1000);
/// Повтор `State` зрителям, пока доступ есть.
const STATE_RESEND: Duration = Duration::from_secs(3);
/// Подсказки управляющему — не чаще.
const NOTICE_EVERY: Duration = Duration::from_secs(1);
/// Сколько показывать уведомление в интерфейсе.
pub const NOTICE_SHOW: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq)]
pub enum RcInbound {
    Rc(RcMsg),
    Ptr(PtrMsg),
    In(InMsg),
}

pub fn is_rc_topic(topic: Option<&str>) -> bool {
    matches!(topic, Some(proto::TOPIC_RC | proto::TOPIC_PTR | proto::TOPIC_IN))
}

pub fn parse_inbound(topic: Option<&str>, payload: &[u8]) -> Option<RcInbound> {
    match topic? {
        proto::TOPIC_RC => proto::decode_rc(payload).map(RcInbound::Rc),
        proto::TOPIC_PTR => proto::decode_ptr(payload).map(RcInbound::Ptr),
        proto::TOPIC_IN => proto::decode_in(payload).map(RcInbound::In),
        _ => None,
    }
}

/// Сообщение в комнату. `to` пуст — всем.
#[derive(Debug, Clone)]
pub struct Outgoing {
    pub topic: &'static str,
    pub payload: Vec<u8>,
    pub reliable: bool,
    pub to: Vec<String>,
}

fn rc_to(to: &str, msg: &RcMsg) -> Outgoing {
    Outgoing { topic: proto::TOPIC_RC, payload: proto::encode(msg), reliable: true, to: vec![to.to_string()] }
}

/// Для интерфейса стримера.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RcHostView {
    /// Запросы: (identity, уровень).
    pub pending: Vec<(String, Level)>,
    /// Кто управляет (identity).
    pub controller: Option<String>,
    /// У кого указка (identity), включая управляющего.
    pub pointers: Vec<String>,
    pub paused: bool,
    /// Последняя причина, почему ввод не выполнен.
    pub blocked: Option<String>,
    /// Окно указки недоступно (старая Windows).
    pub no_overlay: bool,
}

/// Для интерфейса зрителя (на одного стримера).
#[derive(Debug, Clone, PartialEq)]
pub struct RcGuestView {
    pub phase: GuestPhase,
    pub layout: String,
    pub paused: bool,
    pub upgrade_pending: bool,
}

/// Чужая указка поверх трансляции.
#[derive(Debug, Clone, PartialEq)]
pub struct RcPointerView {
    pub who: String,
    pub x: f32,
    pub y: f32,
    pub controlling: bool,
}

pub struct RcEngine {
    host: HostAccess,
    guests: HashMap<String, GuestAccess>,
    host_held: Held,
    /// Что мы зажали у стримера, которым управляем.
    guest_held: Held,
    injector: Box<dyn Injector>,
    hooks: Option<Hooks>,
    hook_tx: UnboundedSender<HookEvent>,
    overlay: Option<Overlay>,
    overlay_failed: bool,
    target: Option<Target>,
    /// Указки зрителей на нашей трансляции: identity → (x, y).
    host_ptrs: HashMap<String, (f32, f32)>,
    /// Последний `State` от стримеров (кого из зрителей рисовать).
    seen_states: HashMap<String, Vec<StateEntry>>,
    /// Указки у чужих трансляций: стример → (кто → x, y).
    seen_ptrs: HashMap<String, HashMap<String, (f32, f32)>>,
    seq: u64,
    /// Последняя указка управляющего у нас (стример): двигаем курсор только при изменении — «пульс» раз в 1 с не отбирает мышь.
    ctl_ptr_last: Option<(f32, f32)>,
    /// Последняя отправленная указка: (когда, стример, x, y).
    ptr_last: Option<(Instant, String, f32, f32)>,
    focus: bool,
    ptt_vk: u32,
    state_sent: Option<Instant>,
    notice_sent: Option<Instant>,
    blocked: Option<String>,
    /// Последнее уведомление для интерфейса.
    pub notice: Option<(String, Instant)>,
    names: HashMap<String, String>,
}

impl RcEngine {
    pub fn new(hook_tx: UnboundedSender<HookEvent>) -> Self {
        Self {
            host: HostAccess::default(),
            guests: HashMap::new(),
            host_held: Held::default(),
            guest_held: Held::default(),
            injector: inject::injector(),
            hooks: None,
            hook_tx,
            overlay: None,
            overlay_failed: false,
            target: None,
            host_ptrs: HashMap::new(),
            seen_states: HashMap::new(),
            seen_ptrs: HashMap::new(),
            seq: 0,
            ctl_ptr_last: None,
            ptr_last: None,
            focus: true,
            ptt_vk: 0,
            state_sent: None,
            notice_sent: None,
            blocked: None,
            notice: None,
            names: HashMap::new(),
        }
    }

    pub fn set_names(&mut self, names: &HashMap<String, String>) {
        if &self.names != names {
            self.names = names.clone();
            self.sync_overlay();
        }
    }

    pub fn set_ptt_key(&mut self, vk: u32) {
        self.ptt_vk = vk;
        self.sync_hooks();
    }

    fn name(&self, id: &str) -> String {
        self.names.get(id).cloned().unwrap_or_else(|| id.to_string())
    }

    fn note(&mut self, text: impl Into<String>) {
        self.notice = Some((text.into(), Instant::now()));
    }

    // ------------------------------------------------------------ стример

    /// Что сейчас транслируем. Смена/конец трансляции — весь доступ сброшен.
    pub fn set_target(&mut self, t: Option<Target>, viewers: &HashSet<String>) -> Vec<Outgoing> {
        if self.target == t {
            return Vec::new();
        }
        let had = self.target.is_some();
        self.target = t;
        if had { self.stream_stopped(viewers) } else { Vec::new() }
    }

    pub fn stream_stopped(&mut self, viewers: &HashSet<String>) -> Vec<Outgoing> {
        let outs = self.host.on_stream_stopped();
        self.run_host(outs, viewers)
    }

    pub fn revoke_all(&mut self, viewers: &HashSet<String>) -> Vec<Outgoing> {
        let outs = self.host.revoke_all();
        self.run_host(outs, viewers)
    }

    pub fn respond(&mut self, who: &str, grant: Option<Level>, viewers: &HashSet<String>) -> Vec<Outgoing> {
        let outs = self.host.respond(who, grant, Session::random(), Instant::now());
        self.run_host(outs, viewers)
    }

    fn state_msg(&self, now: Instant) -> RcMsg {
        RcMsg::State { entries: self.host.state(now), layout: inject::foreground_layout() }
    }

    fn state_targets(&self, viewers: &HashSet<String>) -> Vec<String> {
        let mut to: HashSet<String> = viewers.clone();
        for e in self.host.state(Instant::now()) {
            to.insert(e.who);
        }
        to.into_iter().collect()
    }

    fn run_host(&mut self, outs: Vec<HostOut>, viewers: &HashSet<String>) -> Vec<Outgoing> {
        let mut res = Vec::new();
        let now = Instant::now();
        for o in outs {
            match o {
                HostOut::Send { to, msg } => res.push(rc_to(&to, &msg)),
                HostOut::BroadcastState => {
                    let to = self.state_targets(viewers);
                    if !to.is_empty() {
                        res.push(Outgoing { topic: proto::TOPIC_RC, payload: proto::encode(&self.state_msg(now)), reliable: true, to });
                    }
                    self.state_sent = Some(now);
                }
                HostOut::ReleaseAll => {
                    for ev in self.host_held.release_all() {
                        self.raw_inject(&ev);
                    }
                }
                HostOut::Ask { who, .. } => {
                    let n = self.name(&who);
                    log::info!("управление: запрос от {n}");
                }
                HostOut::Cancelled { .. } => {}
            }
        }
        self.host_ptrs.retain(|w, _| self.host.has_grant(w));
        if self.host.controller().is_none() {
            self.ctl_ptr_last = None;
        }
        self.sync_hooks();
        self.sync_overlay();
        res
    }

    /// Ввод без проверок (только «отпустить»).
    fn raw_inject(&mut self, ev: &InputEv) {
        match *ev {
            InputEv::Button { b, down, .. } => self.injector.button(b, down),
            InputEv::Key { sc, ext, down } => self.injector.key(sc, ext, down),
            InputEv::Wheel { .. } => {}
        }
    }

    fn move_to(&mut self, x: f32, y: f32) -> Result<(i32, i32), Blocked> {
        let t = self.target.ok_or(Blocked::Outside)?;
        // свёрнутое окно (или за пределами экрана) — курсор не трогаем, иначе его утащит в угол
        let rect = inject::visible_rect(&t).ok_or(Blocked::Outside)?;
        let p = from_norm(rect, x, y);
        self.injector.mouse_to(to_absolute(p.0, p.1, inject::virtual_desk()));
        Ok(p)
    }

    fn inject(&mut self, ev: &InputEv) -> Result<(), Blocked> {
        let t = self.target.ok_or(Blocked::Outside)?;
        match *ev {
            InputEv::Button { b, down: true, x, y } => {
                let rect = inject::visible_rect(&t).ok_or(Blocked::Outside)?;
                let p = from_norm(rect, x, y);
                inject::check_point(&t, p.0, p.1)?;
                self.move_to(x, y)?;
                self.injector.button(b, true);
            }
            InputEv::Button { b, down: false, .. } => self.injector.button(b, false),
            InputEv::Wheel { dx, dy, x, y } => {
                let rect = inject::visible_rect(&t).ok_or(Blocked::Outside)?;
                let p = from_norm(rect, x, y);
                inject::check_point(&t, p.0, p.1)?;
                self.move_to(x, y)?;
                self.injector.wheel(dx, dy);
            }
            InputEv::Key { sc, ext, down: true } => {
                if super::keys::host_forbidden_key(sc, ext) {
                    return Ok(());
                }
                inject::check_keys(&t)?;
                self.injector.key(sc, ext, true);
            }
            InputEv::Key { sc, ext, down: false } => self.injector.key(sc, ext, false),
        }
        self.host_held.apply(ev);
        Ok(())
    }

    fn blocked_notice(&mut self, who: &str, b: Blocked) -> Vec<Outgoing> {
        self.blocked = Some(b.text().to_string());
        let now = Instant::now();
        if self.notice_sent.is_some_and(|t| now.duration_since(t) < NOTICE_EVERY) {
            return Vec::new();
        }
        self.notice_sent = Some(now);
        log::info!("управление: ввод от {who} не выполнен: {b:?}");
        vec![rc_to(who, &RcMsg::Notice { text: b.text().to_string() })]
    }

    // ------------------------------------------------------------ входящие

    pub fn inbound(&mut self, from: &str, me: &str, msg: RcInbound, viewers: &HashSet<String>) -> Vec<Outgoing> {
        let now = Instant::now();
        match msg {
            RcInbound::Rc(m @ (RcMsg::Request { .. } | RcMsg::Take { .. })) => {
                if self.target.is_none() {
                    // не транслируем — сразу отказ
                    return if matches!(m, RcMsg::Request { .. }) { vec![rc_to(from, &RcMsg::Deny)] } else { Vec::new() };
                }
                let outs = self.host.on_rc(from, &m, now);
                self.run_host(outs, viewers)
            }
            RcInbound::Rc(RcMsg::Notice { text }) => {
                if self.guests.get(from).is_some_and(|g| g.controlling()) {
                    self.note(text);
                }
                Vec::new()
            }
            RcInbound::Rc(m) => {
                if let RcMsg::State { entries, .. } = &m {
                    self.seen_states.insert(from.to_string(), entries.clone());
                    let ids: HashSet<&str> = entries.iter().map(|e| e.who.as_str()).collect();
                    if let Some(p) = self.seen_ptrs.get_mut(from) {
                        p.retain(|w, _| ids.contains(w.as_str()));
                    }
                }
                let g = self.guests.entry(from.to_string()).or_default();
                let was = g.controlling();
                let text = g.on_rc(&m, me);
                let take = g.pending_take();
                let now_ctl = g.controlling();
                if let Some(t) = text {
                    self.note(t);
                }
                let mut out = Vec::new();
                if let Some(t) = take {
                    out.push(rc_to(from, &t));
                }
                if was && !now_ctl {
                    self.guest_held.release_all();
                }
                self.sync_hooks();
                out
            }
            RcInbound::Ptr(p) => {
                if self.host.accept_ptr(from, &p.s, now) {
                    self.host_ptrs.insert(from.to_string(), (p.x, p.y));
                    if self.host.controller() == Some(from)
                        && !self.host.paused(now)
                        && self.ctl_ptr_last != Some((p.x, p.y))
                    {
                        self.ctl_ptr_last = Some((p.x, p.y));
                        let _ = self.move_to(p.x, p.y);
                    }
                    self.sync_overlay();
                    return Vec::new();
                }
                // чужая указка на чужой трансляции: рисуем, если её стример выдал доступ этому зрителю
                if let Some(streamer) =
                    self.seen_states.iter().find(|(_, e)| e.iter().any(|e| e.who == from)).map(|(s, _)| s.clone())
                {
                    self.seen_ptrs.entry(streamer).or_default().insert(from.to_string(), (p.x, p.y));
                }
                Vec::new()
            }
            RcInbound::In(m) => match self.host.accept_input(from, &m, now) {
                Verdict::Inject => match self.inject(&m.ev) {
                    Ok(()) => {
                        self.blocked = None;
                        Vec::new()
                    }
                    Err(b) => self.blocked_notice(from, b),
                },
                Verdict::Drop(why) => {
                    log::debug!("управление: ввод от {from} отброшен: {why}");
                    Vec::new()
                }
            },
        }
    }

    pub fn left(&mut self, who: &str, viewers: &HashSet<String>) -> Vec<Outgoing> {
        self.guests.remove(who);
        self.seen_states.remove(who);
        self.seen_ptrs.remove(who);
        for p in self.seen_ptrs.values_mut() {
            p.remove(who);
        }
        let outs = self.host.on_left(who);
        let r = self.run_host(outs, viewers);
        self.sync_hooks();
        r
    }

    /// Выход из комнаты: всё забыть (сообщения уже не отправить).
    pub fn leave_room(&mut self) {
        let outs = self.host.revoke_all();
        let _ = self.run_host(outs, &HashSet::new());
        self.guests.clear();
        self.guest_held.release_all();
        self.seen_states.clear();
        self.seen_ptrs.clear();
        self.ptr_last = None;
        self.sync_hooks();
    }

    // ------------------------------------------------------------ зритель

    pub fn request(&mut self, streamer: &str, level: Level) -> Vec<Outgoing> {
        let m = self.guests.entry(streamer.to_string()).or_default().request(level, Instant::now());
        m.map(|m| vec![rc_to(streamer, &m)]).unwrap_or_default()
    }

    pub fn take(&mut self, streamer: &str, on: bool) -> Vec<Outgoing> {
        let mut out = Vec::new();
        // управлять можно только одним: взяли другого — прежнему «вернуть указку»
        if on && let Some(other) = self.controlled().filter(|o| o != streamer) {
            out.extend(self.take(&other, false));
        }
        let Some(g) = self.guests.get_mut(streamer) else { return out };
        let m = g.take(on, Instant::now());
        if !on {
            out.extend(self.release_guest(streamer));
        }
        if let Some(m) = m {
            out.push(rc_to(streamer, &m));
        }
        self.sync_hooks();
        out
    }

    /// Отпустить у стримера всё, что мы зажали (потеря фокуса, возврат к указке).
    fn release_guest(&mut self, streamer: &str) -> Vec<Outgoing> {
        let evs = self.guest_held.release_all();
        let Some(s) = self.guests.get(streamer).and_then(|g| g.session().cloned()) else { return Vec::new() };
        evs.into_iter().map(|ev| self.in_msg(streamer, &s, ev)).collect()
    }

    fn in_msg(&mut self, streamer: &str, s: &Session, ev: InputEv) -> Outgoing {
        self.seq += 1;
        Outgoing {
            topic: proto::TOPIC_IN,
            payload: proto::encode(&InMsg { s: s.clone(), seq: self.seq, ev }),
            reliable: true,
            to: vec![streamer.to_string()],
        }
    }

    pub fn pointer(&mut self, streamer: &str, x: f32, y: f32) -> Vec<Outgoing> {
        let Some(s) = self.guests.get(streamer).and_then(|g| g.session().cloned()) else { return Vec::new() };
        let now = Instant::now();
        if let Some((at, st, px, py)) = &self.ptr_last
            && st == streamer
            && (now.duration_since(*at) < PTR_EVERY || (*px == x && *py == y))
        {
            return Vec::new();
        }
        self.ptr_last = Some((now, streamer.to_string(), x, y));
        // всем в комнате: стример двигает указку/курсор, остальные зрители рисуют её поверх трансляции
        vec![Outgoing { topic: proto::TOPIC_PTR, payload: proto::encode(&PtrMsg { s, x, y }), reliable: false, to: Vec::new() }]
    }

    pub fn input(&mut self, streamer: &str, ev: InputEv) -> Vec<Outgoing> {
        let Some(g) = self.guests.get(streamer) else { return Vec::new() };
        if !g.controlling() {
            return Vec::new();
        }
        let Some(s) = g.session().cloned() else { return Vec::new() };
        self.guest_held.apply(&ev);
        vec![self.in_msg(streamer, &s, ev)]
    }

    /// Перестали смотреть трансляцию — управление ею выключить.
    pub fn stop_watching(&mut self, streamer: &str) -> Vec<Outgoing> {
        if self.guests.get(streamer).is_some_and(|g| g.controlling()) { self.take(streamer, false) } else { Vec::new() }
    }

    fn controlled(&self) -> Option<String> {
        self.guests.iter().find(|(_, g)| g.controlling()).map(|(s, _)| s.clone())
    }

    pub fn set_focus(&mut self, focus: bool) -> Vec<Outgoing> {
        self.focus = focus;
        let mut out = Vec::new();
        if !focus && let Some(st) = self.controlled() {
            out.extend(self.release_guest(&st));
        }
        self.sync_hooks();
        out
    }

    pub fn hook(&mut self, ev: HookEvent, viewers: &HashSet<String>) -> Vec<Outgoing> {
        match ev {
            HookEvent::RealInput => {
                let outs = self.host.on_real_input(Instant::now());
                self.run_host(outs, viewers)
            }
            HookEvent::Panic => {
                log::info!("управление: Ctrl+Alt+Пробел — доступ забран");
                self.revoke_all(viewers)
            }
            HookEvent::GuestKey { sc, ext, down, .. } => match self.controlled() {
                Some(st) => self.input(&st, InputEv::Key { sc, ext, down }),
                None => Vec::new(),
            },
            HookEvent::GuestReturn => match self.controlled() {
                Some(st) => self.take(&st, false),
                None => Vec::new(),
            },
        }
    }

    pub fn tick(&mut self, viewers: &HashSet<String>) -> Vec<Outgoing> {
        let now = Instant::now();
        let outs = self.host.tick(now);
        let mut out = if outs.is_empty() { Vec::new() } else { self.run_host(outs, viewers) };
        if self.host.is_active() && self.state_sent.is_none_or(|t| now.duration_since(t) >= STATE_RESEND) {
            out.extend(self.run_host(vec![HostOut::BroadcastState], viewers));
        }
        let mut notes = Vec::new();
        for g in self.guests.values_mut() {
            if let Some(t) = g.tick(now) {
                notes.push(t);
            }
        }
        for t in notes {
            self.note(t);
        }
        // управляем, но стоим на месте — напомнить о себе, иначе у стримера пауза по тишине
        if let Some(st) = self.controlled()
            && let Some((at, s, x, y)) = self.ptr_last.clone()
            && s == st
            && now.duration_since(at) >= HEARTBEAT
            && let Some(sess) = self.guests.get(&st).and_then(|g| g.session().cloned())
        {
            self.ptr_last = Some((now, s, x, y));
            out.push(Outgoing {
                topic: proto::TOPIC_PTR,
                payload: proto::encode(&PtrMsg { s: sess, x, y }),
                reliable: false,
                to: Vec::new(),
            });
        }
        if self.notice.as_ref().is_some_and(|(_, at)| now.duration_since(*at) > NOTICE_SHOW) {
            self.notice = None;
        }
        out
    }

    // ------------------------------------------------------------ хуки и окно указки

    fn sync_hooks(&mut self) {
        let host = self.host.is_active();
        let guest = self.focus && self.controlled().is_some();
        if !host && !guest {
            self.hooks = None;
            return;
        }
        let h = self.hooks.get_or_insert_with(|| Hooks::start(self.hook_tx.clone()));
        h.set_host_watch(host);
        h.set_guest_capture(guest, self.ptt_vk);
    }

    fn sync_overlay(&mut self) {
        if !self.host.is_active() {
            self.overlay = None;
            self.host_ptrs.clear();
            return;
        }
        if self.overlay.is_none() && !self.overlay_failed {
            self.overlay = Overlay::new();
            self.overlay_failed = self.overlay.is_none();
        }
        let Some(o) = &self.overlay else { return };
        let rect = self.target.and_then(|t| inject::target_rect(&t));
        let list = match rect {
            Some(rect) => self
                .host_ptrs
                .iter()
                .map(|(who, &(x, y))| {
                    let name = self.names.get(who).cloned().unwrap_or_else(|| who.clone());
                    let c = crate::remote_ui::pointer_color(&name);
                    OverlayPointer {
                        pos: from_norm(rect, x, y),
                        color: [c.r(), c.g(), c.b(), 255],
                        name,
                        hidden_arrow: self.host.controller() == Some(who.as_str()),
                    }
                })
                .collect(),
            None => Vec::new(),
        };
        o.set_pointers(list);
    }

    // ------------------------------------------------------------ вид

    pub fn host_view(&self) -> RcHostView {
        let now = Instant::now();
        RcHostView {
            pending: self.host.pending(),
            controller: self.host.controller().map(str::to_string),
            pointers: self.host.state(now).into_iter().map(|e| e.who).collect(),
            paused: self.host.paused(now),
            blocked: self.blocked.clone(),
            no_overlay: self.overlay_failed,
        }
    }

    pub fn guest_views(&self) -> HashMap<String, RcGuestView> {
        self.guests
            .iter()
            .filter(|(_, g)| !matches!(g.phase(), GuestPhase::Idle) || g.upgrade_pending())
            .map(|(s, g)| {
                (
                    s.clone(),
                    RcGuestView {
                        phase: g.phase().clone(),
                        layout: g.layout().to_string(),
                        paused: g.paused(),
                        upgrade_pending: g.upgrade_pending(),
                    },
                )
            })
            .collect()
    }

    pub fn pointer_views(&self) -> HashMap<String, Vec<RcPointerView>> {
        self.seen_ptrs
            .iter()
            .map(|(streamer, ptrs)| {
                let st = self.seen_states.get(streamer);
                let v = ptrs
                    .iter()
                    .map(|(who, &(x, y))| RcPointerView {
                        who: who.clone(),
                        x,
                        y,
                        controlling: st.is_some_and(|e| e.iter().any(|e| &e.who == who && e.controlling)),
                    })
                    .collect();
                (streamer.clone(), v)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::proto::{encode, InputEv, Level, Session};

    #[test]
    fn rc_dispatch_routes_by_topic() {
        let s = Session::random();
        let rc = encode(&RcMsg::Request { level: Level::Pointer });
        assert_eq!(parse_inbound(Some(proto::TOPIC_RC), &rc), Some(RcInbound::Rc(RcMsg::Request { level: Level::Pointer })));
        let p = encode(&PtrMsg { s: s.clone(), x: 0.5, y: 0.5 });
        assert!(matches!(parse_inbound(Some(proto::TOPIC_PTR), &p), Some(RcInbound::Ptr(_))));
        let i = encode(&InMsg { s, seq: 1, ev: InputEv::Key { sc: 1, ext: false, down: true } });
        assert!(matches!(parse_inbound(Some(proto::TOPIC_IN), &i), Some(RcInbound::In(_))));
        // не тот топик / мусор / чужой формат под нашим топиком
        assert_eq!(parse_inbound(Some("murmur.watch"), &rc), None);
        assert_eq!(parse_inbound(None, &rc), None);
        assert_eq!(parse_inbound(Some(proto::TOPIC_RC), b"\xff\x00"), None);
        assert_eq!(parse_inbound(Some(proto::TOPIC_IN), &rc), None);
        assert!(is_rc_topic(Some(proto::TOPIC_PTR)));
        assert!(!is_rc_topic(Some("murmur.watch")));
    }
}
