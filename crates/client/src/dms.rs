//! Личные сообщения (ЛС) и звонки другу: список диалогов, значки в колонке серверов, шапка диалога,
//! окно входящего звонка. История и отправка — общий чат (`chat.rs`) с каналом ЛС; звонок — голосовой движок
//! с «каналом» `call_channel` (комната `dm-{id}`, токен — `POST /dms/{id}/call`).

use std::sync::mpsc as std_mpsc;

use eframe::egui::{self, Align, Align2, CornerRadius, Frame, Layout, Margin, Rect, RichText, Sense, Stroke, Ui, pos2, vec2};
use egui_phosphor::{fill as phf, regular as ph};
use proto::{Channel, ChannelKind, DmChannel, Message, ServerEvent, UnreadChannel};

use crate::api::Api;
use crate::kit::{self, ButtonKind};
use crate::people::People;
use crate::theme as t;
use crate::unread::{Unread, badge_text};

/// «Сервер» каналов ЛС на стороне клиента (у настоящих каналов id сервера > 0).
pub const DM_SERVER: i64 = -1;
/// Значков диалогов в колонке серверов.
const RAIL_MAX: usize = 6;

/// Канал ЛС (чат или звонок), а не канал сервера.
pub fn is_dm(ch: &Channel) -> bool {
    ch.server_id == DM_SERVER
}

fn channel(dm: &DmChannel, kind: ChannelKind) -> Channel {
    Channel { id: dm.id, name: dm.user.shown_name().to_string(), kind, server_id: DM_SERVER, category_id: None, position: 0, user_limit: None }
}

/// «Канал» звонка для голосового движка.
pub fn call_channel(dm: &DmChannel) -> Channel {
    channel(dm, ChannelKind::Voice)
}

/// «Канал» для чата.
pub fn chat_channel(dm: &DmChannel) -> Channel {
    channel(dm, ChannelKind::Text)
}

enum Ev {
    List(Result<Vec<DmChannel>, String>),
    Unread(Result<Vec<UnreadChannel>, String>),
    Opened(Result<DmChannel, String>),
}

pub struct Dms {
    rt: tokio::runtime::Handle,
    ctx: egui::Context,
    api: Api,
    tx: std_mpsc::Sender<Ev>,
    rx: std_mpsc::Receiver<Ev>,
    list: Vec<DmChannel>,
    /// Не удалось открыть диалог (показать человеку).
    pub error: Option<String>,
}

impl Dms {
    pub fn new(rt: tokio::runtime::Handle, ctx: egui::Context, api: Api) -> Self {
        let (tx, rx) = std_mpsc::channel();
        Self { rt, ctx, api, tx, rx, list: Vec::new(), error: None }
    }

    /// Новая сессия: всё забыть.
    pub fn set_api(&mut self, api: Api) {
        self.api = api;
        let (tx, rx) = std_mpsc::channel();
        self.tx = tx;
        self.rx = rx;
        self.list.clear();
        self.error = None;
    }

    fn spawn(&self, fut: impl std::future::Future<Output = Ev> + Send + 'static) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            let _ = tx.send(fut.await);
            ctx.request_repaint();
        });
    }

    /// Перечитать диалоги и непрочитанные (вход, переподключение потока).
    pub fn fetch(&self) {
        self.fetch_list();
        let api = self.api.clone();
        // старый бэкенд (404) — просто без непрочитанных
        self.spawn(async move { Ev::Unread(api.unread().await.map_err(|e| e.message)) });
    }

    pub fn fetch_list(&self) {
        let api = self.api.clone();
        self.spawn(async move { Ev::List(api.dms().await.map_err(|e| e.message)) });
    }

    /// Открыть диалог с человеком; когда сервер ответит, `poll` вернёт его id.
    pub fn open_with(&mut self, user_id: i64) {
        if let Some(d) = self.list.iter().find(|d| d.user.id == user_id) {
            // уже есть — открыть сразу
            let _ = self.tx.send(Ev::Opened(Ok(d.clone())));
            return;
        }
        self.error = None;
        let api = self.api.clone();
        self.spawn(async move {
            Ev::Opened(api.open_dm(user_id).await.map_err(|e| match e.code.as_str() {
                "not_found" => "Диалоги недоступны — нужно обновить бэкенд".to_string(),
                "validation" => e.detail,
                _ => e.message,
            }))
        });
    }

    /// Новое сообщение. true — это сообщение в известном диалоге.
    pub fn on_message(&mut self, m: &Message) -> bool {
        let Some(d) = self.list.iter_mut().find(|d| d.id == m.channel_id) else { return false };
        if d.last_message_id.is_none_or(|l| l < m.id) {
            d.last_message_id = Some(m.id);
            d.last_at = m.created_at;
        }
        self.sort();
        true
    }

    /// Событие потока (кроме сообщений).
    pub fn on_event(&mut self, e: &ServerEvent) {
        match e {
            ServerEvent::Hello | ServerEvent::Resync => self.fetch(),
            ServerEvent::Call { channel_id, call } => match self.list.iter_mut().find(|d| d.id == *channel_id) {
                Some(d) => d.call = call.clone(),
                // звонят в диалоге, которого у меня ещё нет в списке
                None if call.is_some() => self.fetch_list(),
                None => {}
            },
            ServerEvent::UserUpdated { user } => {
                for d in self.list.iter_mut().filter(|d| d.user.id == user.id) {
                    d.user = user.clone();
                }
            }
            _ => {}
        }
    }

    fn sort(&mut self) {
        self.list.sort_by(|a, b| b.last_at.cmp(&a.last_at).then(b.id.cmp(&a.id)));
    }

    /// Разобрать ответы. Возвращает id диалога, который надо открыть.
    pub fn poll(&mut self, unread: &mut Unread, people: &mut People) -> Option<i64> {
        let mut open = None;
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Ev::List(Ok(l)) => {
                    for d in &l {
                        people.learn(d.user.clone());
                    }
                    self.list = l;
                    self.sort();
                }
                Ev::List(Err(e)) => {
                    log::warn!("ЛС: {e}");
                    self.error = Some(format!("Не удалось загрузить список личных сообщений: {e}"));
                }
                Ev::Unread(Ok(u)) => unread.set(u),
                Ev::Unread(Err(e)) => log::warn!("непрочитанные: {e}"),
                Ev::Opened(Ok(d)) => {
                    people.learn(d.user.clone());
                    open = Some(d.id);
                    match self.list.iter_mut().find(|x| x.id == d.id) {
                        Some(x) => *x = d,
                        None => self.list.insert(0, d),
                    }
                    self.sort();
                }
                Ev::Opened(Err(e)) => self.error = Some(e),
            }
        }
        open
    }

    /// Отклонил входящий — убрать звонок сразу, не дожидаясь события.
    pub fn dismiss_call(&mut self, id: i64) {
        if let Some(d) = self.list.iter_mut().find(|d| d.id == id) {
            d.call = None;
        }
    }

    /// Диалоги, свежие сверху.
    pub fn list(&self) -> &[DmChannel] {
        &self.list
    }

    pub fn get(&self, id: i64) -> Option<&DmChannel> {
        self.list.iter().find(|d| d.id == id)
    }

    /// Мне звонят, а я ещё не в этом звонке.
    pub fn incoming(&self, me: i64) -> Option<&DmChannel> {
        self.list.iter().find(|d| d.call.as_ref().is_some_and(|c| c.ringing && c.caller_id != me && !c.participants.contains(&me)))
    }

    /// Значки диалогов под кнопкой «Друзья»: сначала с непрочитанными, звонком и открытый, затем недавние.
    /// Возвращает id нажатого.
    pub fn rail(&self, ui: &mut Ui, people: &People, unread: &Unread, open: Option<i64>) -> Option<i64> {
        let important = |d: &DmChannel| unread.count(d.id) > 0 || d.call.is_some() || open == Some(d.id);
        let mut shown: Vec<&DmChannel> = self.list.iter().filter(|d| important(d)).collect();
        for d in &self.list {
            if shown.len() >= RAIL_MAX {
                break;
            }
            if !important(d) {
                shown.push(d);
            }
        }
        shown.sort_by(|a, b| b.last_at.cmp(&a.last_at).then(b.id.cmp(&a.id)));
        let mut pick = None;
        let left = ui.max_rect().left();
        let p = t::p();
        for d in shown {
            ui.add_space(8.0);
            let (rect, resp) = ui.allocate_exact_size(vec2(44.0, 44.0), Sense::click());
            let active = open == Some(d.id);
            let k = ui.ctx().animate_bool_with_time(resp.id, active || resp.hovered(), kit::HOVER_S * 1.5);
            people.paint(ui.painter(), rect.center(), 22.0 - 1.0 * (1.0 - k), &d.user, 0.0, false);
            if d.call.is_some() {
                ui.painter().circle_stroke(rect.center(), 23.5, Stroke::new(2.5, p.status_online));
                let c = pos2(rect.right() - 5.0, rect.top() + 5.0);
                ui.painter().circle_filled(c, 9.0, p.rail);
                ui.painter().circle_filled(c, 7.5, p.status_online);
                ui.painter().text(c, Align2::CENTER_CENTER, phf::PHONE, t::icon_fill(10.0), p.on_accent);
            }
            let n = unread.count(d.id);
            if n > 0 && !active {
                kit::count_badge(ui.painter(), pos2(rect.right() - 4.0, rect.bottom() - 4.0), &badge_text(n), p.rail);
            }
            if active || resp.hovered() || n > 0 {
                let h = if active { 36.0 } else if resp.hovered() { 18.0 } else { 8.0 };
                let r = Rect::from_center_size(pos2(left + 2.0, rect.center().y), vec2(4.0, h));
                ui.painter().rect_filled(r, CornerRadius::same(2), if active { p.accent } else { p.text_muted });
            }
            let resp = resp.on_hover_text(d.user.shown_name()).on_hover_cursor(egui::CursorIcon::PointingHand);
            if resp.clicked() {
                pick = Some(d.id);
            }
        }
        pick
    }
}

/// Что нажали в шапке диалога.
pub enum HeaderAct {
    /// Позвонить или присоединиться к идущему звонку.
    Call,
}

/// Шапка диалога: аватар, имя, «в сети», справа — звонок. `online`: None — не друг (статус неизвестен).
pub fn header(ui: &mut Ui, people: &People, dm: &DmChannel, online: Option<bool>, me: i64, in_this_call: bool) -> Option<HeaderAct> {
    let p = t::p();
    let mut act = None;
    ui.horizontal_centered(|ui| {
        ui.spacing_mut().item_spacing.x = 12.0;
        let (rect, _) = ui.allocate_exact_size(vec2(32.0, 32.0), Sense::hover());
        people.paint(ui.painter(), rect.center(), 16.0, &dm.user, 0.0, false);
        if let Some(on) = online {
            let c = rect.center() + vec2(11.0, 11.0);
            ui.painter().circle_filled(c, 6.0, p.bg);
            ui.painter().circle_filled(c, 4.0, if on { p.status_online } else { p.status_offline });
        }
        ui.label(RichText::new(dm.user.shown_name()).font(t::xbold(18.0)));
        let sub = match online {
            Some(true) => "в сети".to_string(),
            Some(false) => "не в сети".to_string(),
            None => dm.user.name.clone(),
        };
        ui.label(RichText::new(sub).size(13.5).color(p.text_muted));
        // кнопка звонка — рядом с именем, а не у правого края окна (на широком экране она уезжала далеко)
        ui.add_space(8.0);
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            let call = dm.call.as_ref();
            let others_in = call.is_some_and(|c| c.participants.iter().any(|u| *u != me));
            if in_this_call {
                ui.label(RichText::new(format!("{}  в звонке", phf::PHONE)).font(t::semibold(13.5)).color(p.status_online));
            } else if others_in || call.is_some_and(|c| c.ringing && c.caller_id != me) {
                if kit::button(ui, ButtonKind::Primary, "Присоединиться", Some(phf::PHONE)).clicked() {
                    act = Some(HeaderAct::Call);
                }
            } else if kit::icon_button(ui, ph::PHONE, kit::IconState::Normal, "Позвонить").clicked() {
                act = Some(HeaderAct::Call);
            }
        });
    });
    act
}

/// Окно входящего звонка поверх всего. Some(true) — принять, Some(false) — отклонить.
pub fn incoming_window(ctx: &egui::Context, people: &People, dm: &DmChannel) -> Option<bool> {
    let p = t::p();
    let mut out = None;
    egui::Area::new(egui::Id::new(("incoming-call", dm.id)))
        .order(egui::Order::Foreground)
        .anchor(Align2::CENTER_TOP, vec2(0.0, 112.0))
        .show(ctx, |ui| {
            Frame::new()
                .fill(p.surface)
                .stroke(Stroke::new(1.0, p.border))
                .corner_radius(CornerRadius::same(kit::R_CARD))
                .inner_margin(Margin::symmetric(18, 14))
                .shadow(egui::Shadow { offset: [0, 8], blur: 28, spread: 0, color: egui::Color32::from_black_alpha(70) })
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 12.0;
                        let (rect, _) = ui.allocate_exact_size(vec2(48.0, 48.0), Sense::hover());
                        // мягкая пульсация вокруг аватара, пока звонит
                        let time = ui.input(|i| i.time);
                        let k = ((time * 2.0).sin() * 0.5 + 0.5) as f32;
                        ui.painter().circle_stroke(rect.center(), 25.0 + 3.0 * k, Stroke::new(2.0, p.status_online.gamma_multiply(1.0 - 0.6 * k)));
                        people.paint(ui.painter(), rect.center(), 24.0, &dm.user, 0.0, false);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            ui.add_space(4.0);
                            ui.label(RichText::new(dm.user.shown_name()).font(t::bold(16.0)));
                            ui.label(RichText::new("звонит вам…").size(13.0).color(p.text_muted));
                        });
                        ui.add_space(18.0);
                        if kit::button(ui, ButtonKind::Danger, "Отклонить", Some(phf::PHONE_X)).clicked() {
                            out = Some(false);
                        }
                        if kit::button(ui, ButtonKind::Primary, "Принять", Some(phf::PHONE)).clicked() {
                            out = Some(true);
                        }
                    });
                });
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        });
    out
}
