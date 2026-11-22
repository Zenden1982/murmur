//! Список участников сервера справа (232 px) и карточка профиля (поповер ~300 px).
//! Группы: сейчас «В СЕТИ — N» / «НЕ В СЕТИ — N»; модель — заголовок + список + необязательный цвет ника,
//! чтобы позже над ними встали группы ролей («АДМИН — 1») без переделки.

use std::collections::HashMap;
use std::sync::mpsc as std_mpsc;

use eframe::egui::{self, Align2, Color32, CornerRadius, Id, Pos2, RichText, Sense, Stroke, Ui, pos2, vec2};
use egui_phosphor::regular as ph;
use proto::{Channel, Member, UserProfile};

use crate::api::Api;
use crate::kit::{self, ButtonKind};
use crate::model::Model;
use crate::people::People;
use crate::theme as t;

/// Ширина списка участников.
pub const WIDTH: f32 = 232.0;
const CARD_W: f32 = 300.0;

/// Группа списка: заголовок и люди; цвет ника — для будущих ролей.
pub struct Group {
    pub title: String,
    pub color: Option<Color32>,
    pub members: Vec<Member>,
    pub dim: bool,
}

/// Группы участников сервера: в сети / не в сети (по имени).
pub fn groups(model: &Model, people: &People, server: i64) -> Vec<Group> {
    let list = model.members.get(&server).cloned().unwrap_or_default();
    let me = model.me;
    let name = |m: &Member| people.resolve(m.user.clone()).shown_name().to_lowercase();
    let (mut on, mut off): (Vec<Member>, Vec<Member>) = list.into_iter().partition(|m| m.online || Some(m.user.id) == me);
    on.sort_by_key(name);
    off.sort_by_key(name);
    let mut out = Vec::new();
    if !on.is_empty() {
        out.push(Group { title: format!("В СЕТИ — {}", on.len()), color: None, members: on, dim: false });
    }
    if !off.is_empty() {
        out.push(Group { title: format!("НЕ В СЕТИ — {}", off.len()), color: None, members: off, dim: true });
    }
    out
}

/// Что нажали в карточке профиля.
pub enum Act {
    Join(Channel),
    Message(i64),
    AddFriend(String),
}

enum Ev {
    Profile(i64, Result<UserProfile, String>),
}

/// Открытая карточка: чей профиль, где, с какого сервера открыта (роль и баннер).
struct Card {
    user: i64,
    pos: Pos2,
    server: Option<i64>,
    opened: std::time::Instant,
}

pub struct Members {
    rt: tokio::runtime::Handle,
    ctx: egui::Context,
    api: Api,
    tx: std_mpsc::Sender<Ev>,
    rx: std_mpsc::Receiver<Ev>,
    profiles: HashMap<i64, Result<UserProfile, String>>,
    card: Option<Card>,
}

impl Members {
    pub fn new(rt: tokio::runtime::Handle, ctx: egui::Context, api: Api) -> Self {
        let (tx, rx) = std_mpsc::channel();
        Self { rt, ctx, api, tx, rx, profiles: HashMap::new(), card: None }
    }

    pub fn set_api(&mut self, api: Api) {
        self.api = api;
        let (tx, rx) = std_mpsc::channel();
        self.tx = tx;
        self.rx = rx;
        self.profiles.clear();
        self.card = None;
    }

    pub fn poll(&mut self) {
        while let Ok(Ev::Profile(id, res)) = self.rx.try_recv() {
            self.profiles.insert(id, res);
        }
    }

    /// Профиль изменился (событие) — при следующем открытии перечитать.
    pub fn forget(&mut self, id: i64) {
        self.profiles.remove(&id);
    }

    /// Открыть карточку профиля у точки.
    pub fn open_card(&mut self, user: i64, pos: Pos2, server: Option<i64>) {
        self.card = Some(Card { user, pos, server, opened: std::time::Instant::now() });
        let (api, tx, ctx) = (self.api.clone(), self.tx.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            let res = api.user_profile(user).await.map_err(|e| e.message);
            let _ = tx.send(Ev::Profile(user, res));
            ctx.request_repaint();
        });
    }

    /// Список участников справа. Клик по человеку — карточка профиля.
    pub fn panel(&mut self, ui: &mut Ui, model: &Model, people: &People, server: i64, speaking: &[i64]) {
        let p = t::p();
        ui.add_space(12.0);
        for g in groups(model, people, server) {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                ui.label(RichText::new(&g.title).font(t::bold(12.0)).color(p.text_muted));
            });
            ui.add_space(2.0);
            for m in &g.members {
                let u = people.resolve(m.user.clone());
                let w = ui.available_width();
                let (rect, resp) = ui.allocate_exact_size(vec2(w, 46.0), Sense::click());
                let hover = ui.ctx().animate_bool_with_time(resp.id, resp.hovered(), kit::HOVER_S);
                ui.painter().rect_filled(rect, CornerRadius::same(kit::R_ROW), Color32::TRANSPARENT.lerp_to_gamma(p.row_hover, hover));
                let online = m.online || Some(m.user.id) == model.me;
                let av = pos2(rect.left() + 24.0, rect.center().y);
                let glow = if speaking.contains(&u.id) { 0.6 } else { 0.0 };
                people.paint(ui.painter(), av, 16.0, &u, glow, g.dim);
                crate::friends::status_dot(ui.painter(), av + vec2(11.5, 11.5), online, p.surface);
                let x = rect.left() + 48.0;
                let max_w = rect.right() - x - 6.0;
                let name_c = g.color.unwrap_or(if g.dim { p.text_muted } else { p.text });
                let name = crate::banner::truncate_galley(ui.painter(), u.shown_name(), t::semibold(14.0), name_c, max_w);
                let voice = if online { model.voice_of(u.id) } else { None };
                let sub = sub_line(voice.map(|(_, c)| c), m.activity.as_ref(), online);
                let (top, sub_y) = if sub.is_some() { (rect.center().y - 17.0, rect.center().y + 9.0) } else { (rect.center().y - name.size().y / 2.0, 0.0) };
                ui.painter().galley(pos2(x, top), name, name_c);
                if let Some((kind, text)) = sub {
                    let tx = match kind {
                        Sub::Voice => {
                            kit::wave(ui.painter(), pos2(x, sub_y), 16.0, if speaking.contains(&u.id) { 3.0 } else { 1.4 }, ui.input(|i| i.time) as f32 * 14.0, p.speaking, kit::WaveStyle::Solid);
                            x + 20.0
                        }
                        Sub::Game => {
                            ui.painter().text(pos2(x, sub_y), Align2::LEFT_CENTER, ph::GAME_CONTROLLER, t::font(13.0), p.accent_text);
                            x + 17.0
                        }
                        Sub::Text => x,
                    };
                    crate::emoji::paint::line(ui.painter(), pos2(tx, sub_y), &text, t::font(12.0), p.text_muted, rect.right() - tx - 6.0);
                }
                let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                if resp.clicked() {
                    let pos = pos2(rect.left() - CARD_W - 12.0, rect.top());
                    self.open_card(u.id, pos, Some(server));
                }
            }
        }
    }

    /// Карточка профиля (если открыта). Возвращает действие.
    pub fn card_ui(&mut self, ctx: &egui::Context, model: &Model, people: &People, my_voice: Option<i64>, me: i64, is_friend: &dyn Fn(i64) -> bool) -> Option<Act> {
        let card = self.card.as_ref()?;
        let (uid, server, opened) = (card.user, card.server, card.opened);
        let screen = ctx.content_rect();
        let pos = pos2(card.pos.x.clamp(screen.left() + 8.0, screen.right() - CARD_W - 8.0), card.pos.y.clamp(screen.top() + 8.0, (screen.bottom() - 420.0).max(screen.top() + 8.0)));
        let mut act = None;
        let p = t::p();
        let prof = self.profiles.get(&uid);
        let base = people.user(uid).or_else(|| model.user(uid)).cloned().or_else(|| prof.and_then(|r| r.as_ref().ok()).map(|p| p.user.clone()));
        let Some(u) = base else {
            self.card = None;
            return None;
        };
        let u = people.resolve(u);
        let area = egui::Area::new(Id::new("profile-card")).order(egui::Order::Foreground).fixed_pos(pos).show(ctx, |ui| {
            egui::Frame::new()
                .fill(p.surface)
                .stroke(Stroke::new(1.0, p.border))
                .corner_radius(CornerRadius::same(kit::R_CARD))
                .shadow(egui::epaint::Shadow { offset: [0, 6], blur: 24, spread: 0, color: Color32::from_black_alpha(60) })
                .show(ui, |ui| {
                    ui.set_width(CARD_W);
                    let online = prof.and_then(|r| r.as_ref().ok()).map(|p| p.online).unwrap_or(false) || uid == me;
                    let pr_ok = prof.and_then(|r| r.as_ref().ok());
                    let d = crate::profile_card::CardData {
                        user: &u,
                        banner: pr_ok.and_then(|p| p.banner.as_deref()),
                        status: pr_ok.and_then(|p| p.activity.as_ref()).and_then(|a| a.status.as_deref()),
                        online,
                    };
                    crate::profile_card::head(ui, people, &d);
                    crate::profile_card::body_frame().show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 4.0;
                        crate::profile_card::identity(ui, &d);
                        match prof {
                            None => {
                                ui.add_space(4.0);
                                kit::spinner(ui, 16.0);
                            }
                            Some(Err(e)) => {
                                ui.label(RichText::new(e).size(12.5).color(p.danger_text));
                            }
                            Some(Ok(pr)) => {
                                if let Some(a) = &pr.activity
                                    && let Some(g) = a.game.as_deref()
                                {
                                    let since = a.since.map(|s| format!(" · {}", crate::activity::elapsed_text(s, now()))).unwrap_or_default();
                                    ui.label(RichText::new(format!("{}  играет в {g}{since}", ph::GAME_CONTROLLER)).size(13.0).color(p.accent_text));
                                }
                                crate::profile_card::about(ui, pr.about.as_deref());
                            }
                        }
                        // роль на сервере
                        if let Some(s) = server.and_then(|s| model.server(s))
                            && let Some(m) = model.member(s.id, uid)
                            && let Some(role) = s.roles.iter().find(|r| r.id == m.role_id)
                        {
                            ui.add_space(6.0);
                            ui.label(RichText::new("РОЛИ").font(t::bold(11.5)).color(p.text_muted));
                            egui::Frame::new().fill(p.surface_3).corner_radius(CornerRadius::same(kit::R_PILL)).inner_margin(egui::Margin::symmetric(10, 3)).show(ui, |ui| {
                                ui.label(RichText::new(&role.name).size(12.5).color(p.text));
                            });
                        }
                        // в голосе
                        if online && let Some((s, c)) = model.voice_of(uid) {
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(format!("{}  в голосе · {} · {}", ph::SPEAKER_HIGH, c.name, s.name)).size(12.5).color(p.text_muted));
                            });
                            if my_voice != Some(c.id) && uid != me && kit::button_wide(ui, ButtonKind::Primary, "Присоединиться", CARD_W - 32.0, true).clicked() {
                                act = Some(Act::Join(c.clone()));
                            }
                        }
                        if uid != me {
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                if kit::button(ui, ButtonKind::Secondary, "Написать", Some(egui_phosphor::fill::CHAT_CIRCLE)).clicked() {
                                    act = Some(Act::Message(uid));
                                }
                                if !is_friend(uid) && kit::button(ui, ButtonKind::Ghost, "В друзья", Some(egui_phosphor::fill::USER_PLUS)).clicked() {
                                    act = Some(Act::AddFriend(u.name.clone()));
                                }
                            });
                        }
                    });
                });
        });
        let outside = ctx.input(|i| i.pointer.any_pressed()) && !area.response.contains_pointer() && opened.elapsed().as_millis() > 150;
        if outside || ctx.input(|i| i.key_pressed(egui::Key::Escape)) || act.is_some() {
            self.card = None;
        }
        act
    }
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sub {
    Voice,
    Game,
    Text,
}

/// Строка под ником: голос важнее игры, игра — важнее «в сети».
pub fn sub_line(voice: Option<&Channel>, activity: Option<&proto::Activity>, online: bool) -> Option<(Sub, String)> {
    if !online {
        return None;
    }
    if let Some(c) = voice {
        return Some((Sub::Voice, format!("в голосе · {}", c.name)));
    }
    let (game, text) = crate::activity::line(activity?)?;
    Some((if game { Sub::Game } else { Sub::Text }, text))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(name: &str) -> Channel {
        Channel { id: 1, name: name.into(), kind: proto::ChannelKind::Voice, server_id: 1, category_id: None, position: 0, user_limit: None }
    }

    #[test]
    fn voice_beats_game() {
        let a = proto::Activity { game: Some("Dota 2".into()), since: None, status: None };
        assert_eq!(sub_line(Some(&ch("Общий")), Some(&a), true), Some((Sub::Voice, "в голосе · Общий".into())));
        assert_eq!(sub_line(None, Some(&a), true), Some((Sub::Game, "играет в Dota 2".into())));
        assert_eq!(sub_line(None, Some(&a), false), None);
        assert_eq!(sub_line(None, None, true), None);
    }
}
