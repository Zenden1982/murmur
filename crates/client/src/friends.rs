//! Друзья: экран «Друзья» (кнопка вверху колонки серверов) — добавить по нику, заявки карточками, список по тому,
//! где человек (в голосе / в сети / не в сети), справа — «Где сейчас голос».
//! Данные — `GET /friends`; события `friends_changed` (перечитать), `presence` (в сети / нет) и `activity` — в потоке `/events`.

use std::sync::mpsc as std_mpsc;

use eframe::egui::{self, Align, Align2, Color32, CornerRadius, Layout, Rect, RichText, Sense, Stroke, Ui, UiBuilder, pos2, vec2};
use egui_phosphor::regular as ph;
use proto::{Channel, Friend, FriendsList, PublicUser, ServerEvent};

use crate::api::Api;
use crate::kit::{self, ButtonKind};
use crate::model::Model;
use crate::people::People;
use crate::theme as t;

/// Что нажали на экране друзей (выполняет приложение).
pub enum Act {
    /// Войти в голосовой канал (сервера).
    Join(Channel),
    /// Позвать друга в мой голосовой канал.
    Summon { channel_id: i64, user_id: i64 },
    /// Открыть ЛС.
    Message(i64),
    /// Карточка профиля у точки.
    Profile(i64, egui::Pos2),
}

/// Что нужно экрану из приложения.
pub struct ScreenCx<'a> {
    pub model: &'a Model,
    pub people: &'a People,
    /// Мой голосовой канал сервера (не ЛС), если я в нём.
    pub my_voice: Option<&'a Channel>,
    /// Кто сейчас говорит в моём канале (id).
    pub speaking: &'a [i64],
    /// Строка поиска из левой колонки.
    pub filter: &'a str,
    /// «Не в сети» развёрнуто.
    pub offline_open: &'a mut bool,
}

enum Ev {
    List(Result<FriendsList, String>),
    /// Ответ на действие: новый список или ошибка (текст), `added` — это была заявка по нику.
    Done { res: Result<Option<FriendsList>, String>, added: Option<String> },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Online,
    All,
    Requests,
}

pub struct Friends {
    rt: tokio::runtime::Handle,
    ctx: egui::Context,
    api: Api,
    tx: std_mpsc::Sender<Ev>,
    rx: std_mpsc::Receiver<Ev>,
    list: FriendsList,
    loaded: bool,
    /// Список не загрузился (например, сервер ещё без друзей — 404).
    load_error: Option<String>,
    pub open: bool,
    tab: Tab,
    add_name: String,
    /// Итог последнего действия: Ok — подсказка («заявка отправлена»), Err — ошибка.
    status: Option<Result<String, String>>,
    busy: bool,
    /// Кого спросили «удалить из друзей?».
    confirm_remove: Option<i64>,
    /// Нажали «Написать» — открыть ЛС с этим человеком (забирает приложение).
    message_to: Option<i64>,
    /// Заявки, отложенные «Позже» (карточка скрыта до перезапуска; во вкладке «Заявки» видны).
    later: Vec<i64>,
    toggle_offline: bool,
}

impl Friends {
    pub fn new(rt: tokio::runtime::Handle, ctx: egui::Context, api: Api) -> Self {
        let (tx, rx) = std_mpsc::channel();
        Self {
            rt,
            ctx,
            api,
            tx,
            rx,
            list: FriendsList::default(),
            loaded: false,
            load_error: None,
            open: false,
            tab: Tab::Online,
            add_name: String::new(),
            status: None,
            busy: false,
            confirm_remove: None,
            message_to: None,
            later: Vec::new(),
            toggle_offline: false,
        }
    }

    /// Новая сессия / сервер: всё забыть.
    pub fn set_api(&mut self, api: Api) {
        self.api = api;
        let (tx, rx) = std_mpsc::channel();
        self.tx = tx;
        self.rx = rx;
        self.list = FriendsList::default();
        self.loaded = false;
        self.load_error = None;
        self.open = false;
        self.status = None;
        self.busy = false;
    }

    fn spawn(&self, fut: impl std::future::Future<Output = Ev> + Send + 'static) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            let _ = tx.send(fut.await);
            ctx.request_repaint();
        });
    }

    /// Перечитать список (вход, переподключение потока, `friends_changed`).
    pub fn fetch(&self) {
        let api = self.api.clone();
        self.spawn(async move { Ev::List(api.friends().await.map_err(|e| e.message)) });
    }

    /// Событие потока.
    pub fn on_event(&mut self, e: &ServerEvent) {
        match e {
            ServerEvent::Hello | ServerEvent::Resync | ServerEvent::FriendsChanged => self.fetch(),
            ServerEvent::Presence { user_id, online } => {
                if let Some(f) = self.list.friends.iter_mut().find(|f| f.user.id == *user_id) {
                    f.online = *online;
                    if !online {
                        f.activity = None;
                    }
                }
            }
            ServerEvent::Activity { user_id, activity } => {
                if let Some(f) = self.list.friends.iter_mut().find(|f| f.user.id == *user_id) {
                    f.activity = activity.clone();
                }
            }
            ServerEvent::UserUpdated { user } => {
                let fix = |u: &mut PublicUser| {
                    if u.id == user.id {
                        *u = user.clone();
                    }
                };
                self.list.friends.iter_mut().for_each(|f| fix(&mut f.user));
                self.list.incoming.iter_mut().chain(self.list.outgoing.iter_mut()).for_each(|r| fix(&mut r.user));
            }
            _ => {}
        }
    }

    /// Разобрать ответы. Возвращает true, если входящих заявок стало больше (звук).
    pub fn poll(&mut self, people: &mut People) -> bool {
        let before: Vec<i64> = self.list.incoming.iter().map(|r| r.user.id).collect();
        let was_loaded = self.loaded;
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Ev::List(Ok(l)) => {
                    self.load_error = None;
                    self.set_list(l, people);
                }
                Ev::List(Err(e)) => {
                    log::warn!("друзья: {e}");
                    self.load_error = Some(e);
                }
                Ev::Done { res, added } => {
                    self.busy = false;
                    match res {
                        Ok(l) => {
                            if let Some(l) = l {
                                self.set_list(l, people);
                            }
                            if let Some(name) = added {
                                let friend = self.list.friends.iter().any(|f| f.user.name.eq_ignore_ascii_case(&name));
                                self.status = Some(Ok(if friend {
                                    format!("{name} теперь в друзьях")
                                } else {
                                    format!("Заявка для {name} отправлена")
                                }));
                                self.add_name.clear();
                            }
                        }
                        Err(e) => self.status = Some(Err(e)),
                    }
                }
            }
        }
        // при первой загрузке старые заявки — не повод для звука
        was_loaded && self.list.incoming.iter().any(|r| !before.contains(&r.user.id))
    }

    fn set_list(&mut self, l: FriendsList, people: &mut People) {
        for u in l.friends.iter().map(|f| &f.user).chain(l.incoming.iter().map(|r| &r.user)).chain(l.outgoing.iter().map(|r| &r.user)) {
            people.learn(u.clone());
        }
        self.list = l;
        self.loaded = true;
    }

    /// Входящих заявок (бейдж на кнопке).
    pub fn incoming(&self) -> usize {
        self.list.incoming.len()
    }

    /// Нажали «Написать» у друга: его id (один раз).
    pub fn take_message(&mut self) -> Option<i64> {
        self.message_to.take()
    }

    /// Показать ошибку в окне друзей (например, не удалось открыть ЛС).
    pub fn show_error(&mut self, e: String) {
        self.status = Some(Err(e));
        self.open = true;
    }

    /// Друзья (для левой колонки и ЛС).
    pub fn list(&self) -> &[Friend] {
        &self.list.friends
    }

    pub fn is_friend(&self, id: i64) -> bool {
        self.list.friends.iter().any(|f| f.user.id == id)
    }

    /// Отправить заявку человеку (из карточки профиля).
    pub fn add_by_login(&mut self, login: &str) {
        self.add_name = login.to_string();
        self.add();
    }

    /// В сети ли друг; None — не друг.
    pub fn online(&self, id: i64) -> Option<bool> {
        self.list.friends.iter().find(|f| f.user.id == id).map(|f| f.online)
    }

    /// Друзья есть, и никого нет в сети — кот в заголовке спит.
    pub fn all_offline(&self) -> bool {
        !self.list.friends.is_empty() && self.list.friends.iter().all(|f| !f.online)
    }

    fn add(&mut self) {
        let name = self.add_name.trim().to_string();
        if name.is_empty() || self.busy {
            return;
        }
        self.busy = true;
        self.status = None;
        let api = self.api.clone();
        self.spawn(async move {
            let res = api.add_friend(&name).await.map(Some).map_err(|e| match e.code.as_str() {
                "not_found" => format!("Нет пользователя «{name}» на этом сервере"),
                "bad_request" => format!("{name} уже в друзьях"),
                _ => e.message,
            });
            Ev::Done { res, added: Some(name) }
        });
    }

    fn act(&mut self, accept: bool, id: i64) {
        self.busy = true;
        self.status = None;
        let api = self.api.clone();
        self.spawn(async move {
            let res = if accept {
                api.accept_friend(id).await.map(Some)
            } else {
                api.remove_friend(id).await.map(|_| None)
            };
            Ev::Done { res: res.map_err(|e| e.message), added: None }
        });
        if !accept {
            // сразу убрать из списка — сервер всё равно пришлёт friends_changed
            self.list.friends.retain(|f| f.user.id != id);
            self.list.incoming.retain(|r| r.user.id != id);
            self.list.outgoing.retain(|r| r.user.id != id);
        }
    }

    /// Отладка (скриншоты): открыть экран сразу, `requests` / `all` — на этой вкладке.
    #[cfg(debug_assertions)]
    /// Отладка: несколько друзей без сервера (окно выбора для звонка).
    #[cfg(debug_assertions)]
    pub fn debug_fill(&mut self) {
        for (i, (n, on)) in [("Курису", true), ("Дару", true), ("Маюри", false), ("Фэйрис", true)].into_iter().enumerate() {
            let user = PublicUser { id: 10 + i as i64, name: n.to_lowercase(), display_name: Some(n.into()), avatar: None };
            self.list.friends.push(Friend { user, online: on, activity: None });
        }
        self.loaded = true;
    }

    pub fn debug_open(&mut self, tab: &str) {
        self.open = true;
        self.tab = match tab {
            "requests" => Tab::Requests,
            "all" => Tab::All,
            _ => Tab::Online,
        };
    }

    /// Переключатель в шапке: «В сети / Все / Заявки».
    pub fn header_tabs(&mut self, ui: &mut Ui) {
        let l_req = if self.list.incoming.is_empty() { "Заявки".to_string() } else { format!("Заявки · {}", self.list.incoming.len()) };
        kit::segmented(ui, &mut self.tab, &[(Tab::Online, "В сети"), (Tab::All, "Все"), (Tab::Requests, &l_req)]);
    }

    /// Экран «Друзья» (центр + правая колонка).
    pub fn screen(&mut self, ui: &mut Ui, cx: ScreenCx) -> Vec<Act> {
        let mut acts = Vec::new();
        let full = ui.available_rect_before_wrap();
        // правая колонка — справа, на узком окне — снизу
        let side = full.width() >= 860.0;
        let right_w = if side { 280.0 } else { 0.0 };
        let main = Rect::from_min_max(full.min, pos2(full.right() - if side { right_w + 24.0 } else { 0.0 }, full.bottom()));
        ui.scope_builder(UiBuilder::new().max_rect(main), |ui| {
            egui::ScrollArea::vertical().id_salt("friends-main").auto_shrink([false, false]).show(ui, |ui| {
                ui.set_width(ui.available_width());
                self.add_row(ui);
                ui.add_space(14.0);
                self.body(ui, &cx, &mut acts);
                if !side {
                    ui.add_space(18.0);
                    self.voice_column(ui, &cx, &mut acts);
                }
            });
        });
        if side {
            let col = Rect::from_min_max(pos2(full.right() - right_w, full.top()), full.max);
            ui.painter().vline(col.left() - 12.0, col.y_range(), Stroke::new(1.0, t::p().border));
            ui.scope_builder(UiBuilder::new().max_rect(col), |ui| {
                egui::ScrollArea::vertical().id_salt("friends-voice").auto_shrink([false, false]).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    self.voice_column(ui, &cx, &mut acts);
                });
            });
        }
        ui.allocate_rect(full, Sense::hover());
        acts
    }

    /// Строка добавления по нику (сверху, а не отдельной вкладкой).
    fn add_row(&mut self, ui: &mut Ui) {
        let p = t::p();
        ui.label(RichText::new("Добавить в друзья").font(t::bold(15.0)).color(p.text));
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let w = (ui.available_width() - 130.0).min(460.0);
            let r = ui.add_enabled(!self.busy, kit::text_field(&mut self.add_name, w).hint_text("Добавить по нику — например, murka").char_limit(proto::limits::NAME_MAX));
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let ok = !self.add_name.trim().is_empty() && !self.busy;
            if kit::button_ex(ui, ButtonKind::Primary, "Добавить", None, ok).clicked() || (enter && ok) {
                self.add();
            }
        });
        match &self.status {
            Some(Ok(s)) => {
                ui.label(RichText::new(s).size(13.0).color(p.accent_text));
            }
            Some(Err(e)) => {
                ui.label(RichText::new(format!("{}  {e}", ph::WARNING_CIRCLE)).size(13.0).color(p.danger_text));
            }
            None => {}
        }
    }

    fn body(&mut self, ui: &mut Ui, cx: &ScreenCx, acts: &mut Vec<Act>) {
        if !self.loaded {
            match self.load_error.clone() {
                Some(e) => {
                    if kit::cat_state(ui, kit::Cat::Offline, "Друзья недоступны", &e, Some("Повторить")) {
                        self.load_error = None;
                        self.fetch();
                    }
                }
                None => {
                    kit::cat_state(ui, kit::Cat::Loading, "Загружаем…", "Ещё секунду", None);
                }
            }
            return;
        }
        if self.tab == Tab::Requests {
            self.requests(ui, cx.people);
            return;
        }
        // входящие заявки — карточками над списком, пока они есть
        let cards: Vec<PublicUser> = self.list.incoming.iter().filter(|r| !self.later.contains(&r.user.id)).map(|r| r.user.clone()).collect();
        for u in cards {
            self.request_card(ui, cx, &u);
        }
        let f = cx.filter.trim().to_lowercase();
        let matches = |fr: &&Friend| f.is_empty() || fr.user.shown_name().to_lowercase().contains(&f) || fr.user.name.to_lowercase().contains(&f);
        let mut friends: Vec<Friend> = self.list.friends.iter().filter(matches).cloned().collect();
        friends.sort_by_key(|fr| fr.user.shown_name().to_lowercase());
        if self.list.friends.is_empty() {
            ui.add_space(20.0);
            kit::cat_state(ui, kit::Cat::Peek, "Друзей пока нет", "Добавь кого-нибудь по нику", None);
            return;
        }
        let in_voice: Vec<&Friend> = friends.iter().filter(|fr| fr.online && cx.model.voice_of(fr.user.id).is_some()).collect();
        let online: Vec<&Friend> = friends.iter().filter(|fr| fr.online && cx.model.voice_of(fr.user.id).is_none()).collect();
        let offline: Vec<&Friend> = friends.iter().filter(|fr| !fr.online).collect();
        if in_voice.is_empty() && online.is_empty() && self.tab == Tab::Online {
            ui.add_space(20.0);
            if f.is_empty() {
                kit::cat_state(ui, kit::Cat::Sleep, "Все спят", "Позови кого-нибудь", None);
            } else {
                kit::cat_state(ui, kit::Cat::Peek, "Никого не нашли", "Проверь, как написано имя", None);
            }
            return;
        }
        if !in_voice.is_empty() {
            section(ui, &format!("В ГОЛОСЕ — {}", in_voice.len()));
            for fr in in_voice {
                self.friend_row(ui, cx, fr, acts);
            }
        }
        if !online.is_empty() {
            section(ui, &format!("В СЕТИ — {}", online.len()));
            for fr in online {
                self.friend_row(ui, cx, fr, acts);
            }
        }
        if self.tab == Tab::All && !offline.is_empty() {
            ui.add_space(10.0);
            let open = *cx.offline_open;
            let text = format!("Не в сети — {} · {}", offline.len(), if open { "скрыть" } else { "показать" });
            let r = ui.add(egui::Label::new(RichText::new(text).font(t::bold(12.5)).color(t::p().text_muted)).sense(Sense::click()));
            if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                // ScreenCx держит &mut — меняем через указатель-флаг
                self.toggle_offline = true;
            }
            if open {
                for fr in offline {
                    self.friend_row(ui, cx, fr, acts);
                }
            }
        }
    }

    /// Вкладка «Заявки»: входящие и отправленные.
    fn requests(&mut self, ui: &mut Ui, people: &People) {
        if self.list.incoming.is_empty() && self.list.outgoing.is_empty() {
            ui.add_space(20.0);
            kit::cat_state(ui, kit::Cat::Sleep, "Заявок нет", "Новые появятся здесь", None);
            return;
        }
        if !self.list.incoming.is_empty() {
            section(ui, &format!("ВХОДЯЩИЕ — {}", self.list.incoming.len()));
            for r in self.list.incoming.clone() {
                self.request_row(ui, people, &r.user, true);
            }
        }
        if !self.list.outgoing.is_empty() {
            section(ui, &format!("ОТПРАВЛЕННЫЕ — {}", self.list.outgoing.len()));
            for r in self.list.outgoing.clone() {
                self.request_row(ui, people, &r.user, false);
            }
        }
    }

    /// Карточка входящей заявки: «X хочет добавить вас в друзья», общий сервер, «Принять» / «Позже».
    fn request_card(&mut self, ui: &mut Ui, cx: &ScreenCx, u: &PublicUser) {
        let p = t::p();
        let common = cx.model.servers.iter().find(|s| cx.model.is_member(s.id, u.id)).map(|s| s.name.clone());
        egui::Frame::new()
            .fill(p.surface_2)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(CornerRadius::same(kit::R_CARD))
            .inner_margin(egui::Margin::symmetric(14, 12))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(vec2(40.0, 40.0), Sense::hover());
                    cx.people.paint(ui.painter(), rect.center(), 20.0, u, 0.0, false);
                    ui.add_space(4.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.label(RichText::new(format!("{} хочет добавить вас в друзья", u.shown_name())).font(t::semibold(14.5)).color(p.text));
                        let sub = match &common {
                            Some(s) => format!("общий сервер: {s}"),
                            None => format!("логин: {}", u.name),
                        };
                        ui.label(RichText::new(sub).size(12.5).color(p.text_muted));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let busy = self.busy;
                        if kit::button_ex(ui, ButtonKind::Secondary, "Позже", None, !busy).clicked() {
                            self.later.push(u.id);
                        }
                        if kit::button_ex(ui, ButtonKind::Primary, "Принять", None, !busy).clicked() {
                            self.act(true, u.id);
                        }
                    });
                });
            });
        ui.add_space(8.0);
    }

    /// Строка друга: аватар со статусом, ник, под ником — где он (волна + «Канал · Сервер») или чем занят; справа —
    /// «Присоединиться» / «Позвать», «Написать», «Ещё».
    fn friend_row(&mut self, ui: &mut Ui, cx: &ScreenCx, fr: &Friend, acts: &mut Vec<Act>) {
        let p = t::p();
        let u = cx.people.resolve(fr.user.clone());
        let w = ui.available_width();
        let (rect, resp) = ui.allocate_exact_size(vec2(w, 60.0), Sense::click());
        let hover = ui.ctx().animate_bool_with_time(resp.id, resp.hovered() || ui.rect_contains_pointer(rect), kit::HOVER_S);
        ui.painter().rect_filled(rect, CornerRadius::same(kit::R_ROW), Color32::TRANSPARENT.lerp_to_gamma(p.row_hover, hover));
        ui.painter().hline(rect.x_range().shrink(8.0), rect.bottom(), Stroke::new(1.0, p.border));
        let av = pos2(rect.left() + 28.0, rect.center().y);
        let voice = if fr.online { cx.model.voice_of(u.id) } else { None };
        let speaking = cx.speaking.contains(&u.id);
        cx.people.paint(ui.painter(), av, 18.0, &u, if speaking { 0.6 } else { 0.0 }, !fr.online);
        status_dot(ui.painter(), av + vec2(13.0, 13.0), fr.online, p.bg);
        let x = rect.left() + 56.0;
        crate::banner::paint_truncated(ui.painter(), pos2(x, rect.center().y - 9.0), Align2::LEFT_CENTER, u.shown_name(), t::semibold(15.0), if fr.online { p.text } else { p.text_muted }, rect.right() - 370.0 - x);
        let sub_y = rect.center().y + 10.0;
        match (&voice, &fr.activity) {
            (Some((s, c)), _) => {
                let amp = if speaking { 3.5 } else { 1.6 };
                let phase = ui.input(|i| i.time) as f32 * 14.0;
                kit::wave(ui.painter(), pos2(x, sub_y), 22.0, amp, phase, p.speaking, kit::WaveStyle::Solid);
                ui.painter().text(pos2(x + 28.0, sub_y), Align2::LEFT_CENTER, format!("{} · {}", c.name, s.name), t::font(12.5), p.text_muted);
            }
            (None, Some(a)) if fr.online => {
                if let Some((game, text)) = crate::activity::line(a) {
                    let icon = if game { ph::GAME_CONTROLLER } else { ph::CHAT_TEXT };
                    ui.painter().text(pos2(x, sub_y), Align2::LEFT_CENTER, icon, t::font(14.0), p.accent_text);
                    crate::emoji::paint::line(ui.painter(), pos2(x + 20.0, sub_y), &text, t::font(12.5), p.text_muted, rect.right() - 370.0 - x - 20.0);
                }
            }
            _ => {
                ui.painter().text(pos2(x, sub_y), Align2::LEFT_CENTER, if fr.online { "в сети" } else { "не в сети" }, t::font(12.5), p.text_muted);
            }
        }
        // кнопки справа
        let btns = Rect::from_min_max(pos2(rect.right() - 360.0, rect.top()), pos2(rect.right() - 10.0, rect.bottom()));
        ui.scope_builder(UiBuilder::new().max_rect(btns).layout(Layout::right_to_left(Align::Center)), |ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let more = kit::icon_button(ui, ph::DOTS_THREE_VERTICAL, kit::IconState::Normal, "Ещё");
            egui::Popup::menu(&more).width(220.0).show(|ui| {
                if kit::menu_item(ui, egui_phosphor::fill::USER_CIRCLE, "Профиль", false).clicked() {
                    acts.push(Act::Profile(u.id, more.rect.left_bottom()));
                }
                if kit::menu_item(ui, egui_phosphor::fill::USER_MINUS, "Удалить из друзей", true).clicked() {
                    self.confirm_remove = Some(u.id);
                }
            });
            if kit::icon_button(ui, ph::CHAT_CIRCLE, kit::IconState::Normal, "Написать").clicked() {
                acts.push(Act::Message(u.id));
            }
            match &voice {
                Some((_, c)) if cx.my_voice.is_some_and(|m| m.id == c.id) => {
                    ui.label(RichText::new("вы рядом").size(12.5).color(p.text_muted));
                }
                Some((_, c)) => {
                    if kit::button(ui, ButtonKind::Primary, "Присоединиться", None).clicked() {
                        acts.push(Act::Join((*c).clone()));
                    }
                }
                None if fr.online => {
                    let can = cx.my_voice.filter(|m| cx.model.is_member(m.server_id, u.id));
                    let tip = match (cx.my_voice, can) {
                        (None, _) => "Сначала зайдите в голосовой канал".to_string(),
                        (Some(m), None) => format!("{} нет на сервере канала «{}»", u.shown_name(), m.name),
                        (Some(m), Some(_)) => format!("Позвать в «{}»", m.name),
                    };
                    let r = outline_button(ui, ph::SPEAKER_HIGH, "Позвать", can.is_some()).on_hover_text(tip);
                    if r.clicked()
                        && let Some(m) = can
                    {
                        acts.push(Act::Summon { channel_id: m.id, user_id: u.id });
                        self.status = Some(Ok(format!("Позвали {} в «{}»", u.shown_name(), m.name)));
                    }
                }
                None => {}
            }
        });
        if (resp.secondary_clicked() || resp.clicked())
            && let Some(pos) = resp.interact_pointer_pos()
        {
            acts.push(Act::Profile(u.id, pos));
        }
        if self.confirm_remove == Some(u.id) {
            ui.horizontal(|ui| {
                ui.add_space(56.0);
                ui.label(RichText::new(format!("Удалить {} из друзей?", u.shown_name())).size(13.0).color(p.text));
                if kit::button(ui, ButtonKind::Danger, "Удалить", None).clicked() {
                    self.confirm_remove = None;
                    self.act(false, u.id);
                }
                if kit::button(ui, ButtonKind::Ghost, "Нет", None).clicked() {
                    self.confirm_remove = None;
                }
            });
        }
    }

    /// Правая колонка «Где сейчас голос»: карточка на каждый голосовой канал, где есть друзья.
    fn voice_column(&mut self, ui: &mut Ui, cx: &ScreenCx, acts: &mut Vec<Act>) {
        let p = t::p();
        ui.label(RichText::new("Где сейчас голос").font(t::bold(15.0)).color(p.text));
        ui.add_space(8.0);
        let friend_ids: Vec<i64> = self.list.friends.iter().filter(|f| f.online).map(|f| f.user.id).collect();
        let busy: Vec<_> = cx.model.busy_voice().into_iter().filter(|(_, _, ids)| ids.iter().any(|id| friend_ids.contains(id))).collect();
        if busy.is_empty() {
            ui.label(RichText::new("Сейчас никто не в голосе").size(13.0).color(p.text_muted));
            return;
        }
        for (s, c, ids) in busy {
            egui::Frame::new()
                .fill(p.surface_2)
                .stroke(Stroke::new(1.0, p.border))
                .corner_radius(CornerRadius::same(kit::R_CARD))
                .inner_margin(egui::Margin::same(12))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(ph::SPEAKER_HIGH).size(16.0).color(p.text_muted));
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            ui.label(RichText::new(&c.name).font(t::bold(14.5)).color(p.text));
                            ui.label(RichText::new(&s.name).size(12.0).color(p.text_muted));
                        });
                    });
                    ui.add_space(8.0);
                    // аватары: друзья первыми, говорящий — со свечением
                    let mut shown: Vec<i64> = ids.iter().copied().filter(|id| friend_ids.contains(id)).collect();
                    let strangers = ids.len() - shown.len();
                    shown.extend(ids.iter().copied().filter(|id| !friend_ids.contains(id)));
                    let n = shown.len().min(7);
                    let (row, _) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::hover());
                    for (i, id) in shown.iter().take(n).enumerate() {
                        let u = cx.people.user(*id).or_else(|| cx.model.user(*id)).cloned().unwrap_or(PublicUser { id: *id, name: format!("#{id}"), display_name: None, avatar: None });
                        let c = pos2(row.left() + 16.0 + i as f32 * 26.0, row.center().y);
                        ui.painter().circle_filled(c, 17.5, p.surface_2);
                        let glow = if cx.speaking.contains(id) { 0.6 } else { 0.0 };
                        cx.people.paint(ui.painter(), c, 15.0, &u, glow, false);
                    }
                    if shown.len() > n {
                        ui.painter().text(pos2(row.left() + 16.0 + n as f32 * 26.0, row.center().y), Align2::LEFT_CENTER, format!("+{}", shown.len() - n), t::semibold(12.5), p.text_muted);
                    }
                    ui.add_space(4.0);
                    let first = shown.first().and_then(|id| cx.people.user(*id).or_else(|| cx.model.user(*id))).map(|u| u.shown_name().to_string()).unwrap_or_default();
                    let caption = if strangers > 0 { format!("{first} и ещё {} · {strangers} с сервера", ids.len() - 1) } else if ids.len() > 1 { format!("{first} и ещё {}", ids.len() - 1) } else { first };
                    ui.label(RichText::new(caption).size(12.5).color(p.text_muted));
                    ui.add_space(6.0);
                    if cx.my_voice.is_some_and(|m| m.id == c.id) {
                        ui.label(RichText::new("Вы здесь").size(12.5).color(p.accent_text));
                    } else if kit::button_wide(ui, ButtonKind::Primary, "Присоединиться", ui.available_width(), true).clicked() {
                        acts.push(Act::Join(c.clone()));
                    }
                });
            ui.add_space(10.0);
        }
    }

    /// «Не в сети» нажали — переключить (флаг забирает приложение).
    pub fn take_toggle_offline(&mut self) -> bool {
        std::mem::take(&mut self.toggle_offline)
    }

    /// Строка: аватар, имя и логин.
    fn person(ui: &mut Ui, people: &People, u: &PublicUser, sub: &str) {
        let (rect, _) = ui.allocate_exact_size(vec2(40.0, 40.0), Sense::hover());
        let p = t::p();
        people.paint(ui.painter(), rect.center(), 18.0, u, 0.0, false);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.add_space(2.0);
            ui.label(RichText::new(u.shown_name()).font(t::semibold(14.5)).color(p.text));
            let login = if u.display_name.as_deref().is_some_and(|d| !d.is_empty()) { format!("{} · {sub}", u.name) } else { sub.to_string() };
            ui.label(RichText::new(login).size(12.5).color(p.text_muted));
        });
    }

    fn request_row(&mut self, ui: &mut Ui, people: &People, u: &PublicUser, incoming: bool) {
        ui.horizontal(|ui| {
            ui.set_min_height(52.0);
            Self::person(ui, people, u, if incoming { "хочет дружить" } else { "ждём ответа" });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let busy = self.busy;
                if incoming {
                    if kit::icon_button(ui, ph::X, kit::IconState::Normal, "Отклонить").clicked() && !busy {
                        self.act(false, u.id);
                    }
                    if kit::button_ex(ui, ButtonKind::Primary, "Принять", None, !busy).clicked() {
                        self.act(true, u.id);
                    }
                } else if kit::icon_button(ui, ph::X, kit::IconState::Normal, "Отменить заявку").clicked() && !busy {
                    self.act(false, u.id);
                }
            });
        });
    }
}

/// Заголовок группы списка («В СЕТИ — 3»).
fn section(ui: &mut Ui, text: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(text).font(t::bold(12.0)).color(t::p().text_muted));
    ui.add_space(2.0);
}

/// Точка статуса на аватаре.
pub fn status_dot(painter: &egui::Painter, c: egui::Pos2, online: bool, ring: Color32) {
    let p = t::p();
    painter.circle_filled(c, 6.5, ring);
    painter.circle_filled(c, 4.5, if online { p.status_online } else { p.status_offline });
}

/// Контурная кнопка с иконкой (высота 34).
pub fn outline_button(ui: &mut Ui, icon: &str, text: &str, enabled: bool) -> egui::Response {
    let p = t::p();
    let g = ui.painter().layout_no_wrap(text.to_string(), t::bold(13.5), p.text);
    let w = g.size().x + 18.0 + 8.0 + 28.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 34.0), if enabled { Sense::click() } else { Sense::hover() });
    let hover = ui.ctx().animate_bool_with_time(resp.id, enabled && resp.hovered(), kit::HOVER_S);
    let fg = if enabled { p.text } else { p.text_faint.gamma_multiply(0.7) };
    let stroke = if enabled { p.border_strong } else { p.border };
    ui.painter().rect(rect, CornerRadius::same(kit::R_PILL), Color32::TRANSPARENT.lerp_to_gamma(p.row_hover, hover), Stroke::new(1.0, stroke), egui::StrokeKind::Inside);
    ui.painter().text(pos2(rect.left() + 14.0 + 9.0, rect.center().y), Align2::CENTER_CENTER, icon, t::font(16.0), fg);
    ui.painter().galley(pos2(rect.left() + 14.0 + 26.0, rect.center().y - g.size().y / 2.0), g, fg);
    if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

/// Кнопка «Друзья» в колонке серверов (значок людей, бейдж — входящие заявки). true — нажата.
pub fn rail_button(ui: &mut Ui, incoming: usize, open: bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(44.0, 44.0), Sense::click());
    let k = ui.ctx().animate_bool_with_time(resp.id, open || resp.hovered(), kit::HOVER_S * 1.5);
    let p = t::p();
    let r = egui::lerp(22.0..=14.0, k) as u8;
    let bg = if open { p.accent_strong } else { p.surface_3.lerp_to_gamma(p.row_active, k) };
    ui.painter().rect_filled(rect, egui::CornerRadius::same(r), bg);
    let fg = if open { p.on_accent } else { p.accent };
    if open {
        // слева белая полоска, как у выбранного сервера
        let left = ui.max_rect().left();
        let r = egui::Rect::from_center_size(pos2(left + 2.0, rect.center().y), vec2(4.0, 36.0));
        ui.painter().rect_filled(r, CornerRadius::same(2), p.text);
    }
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, egui_phosphor::fill::USERS, t::icon_fill(21.0), fg);
    let resp = resp.on_hover_text("Друзья").on_hover_cursor(egui::CursorIcon::PointingHand);
    if incoming > 0 {
        let c = pos2(resp.rect.right() - 4.0, resp.rect.top() + 4.0);
        kit::count_badge(ui.painter(), c, &crate::unread::badge_text(incoming as u32), p.rail);
    }
    resp.clicked()
}
