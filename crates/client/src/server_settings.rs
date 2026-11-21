//! Настройки сервера: слева меню (Профиль сервера, Участники, Приглашения, Баны, Роли и Модерация — «скоро»),
//! справа — раздел. «Профиль сервера»: название, значок, баннер (узор или своя картинка 960×480), цвет, описание и живой
//! предпросмотр (шапка боковой панели + карточка приглашения).

use std::sync::mpsc as std_mpsc;

use eframe::egui::{self, Align, Align2, Color32, CornerRadius, Layout, RichText, Sense, Stroke, TextureHandle, TextureOptions, Ui, vec2};
use egui_phosphor::regular as ph;
use proto::{Ban, Invite, Role, limits, perm};

use crate::api::{self, Api};
use crate::kit::{self, ButtonKind};
use crate::model::Model;
use crate::people::{self, People};
use crate::theme as t;

/// Что можно сделать с участником (по моим правам и иерархии).
#[derive(Debug, Default, PartialEq)]
pub struct MemberActions {
    /// Роли, которые можно назначить (пусто — пункта «Роль» нет).
    pub roles: Vec<Role>,
    pub disconnect: bool,
    pub kick: bool,
    pub ban: bool,
}

pub fn member_actions(model: &Model, server: i64, user: i64) -> MemberActions {
    if !model.can_act_on(server, user) {
        return MemberActions::default();
    }
    let has = |p| model.has_perm(server, p);
    let roles = if has(perm::MANAGE_ROLES) {
        model
            .server(server)
            .map(|s| s.roles.iter().filter(|r| model.can_assign(server, r.id)).cloned().collect())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    MemberActions { roles, disconnect: has(perm::DISCONNECT), kick: has(perm::KICK), ban: has(perm::BAN) }
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Overview,
    Members,
    Bans,
    Invites,
}

enum Confirm {
    Kick { user: i64, name: String },
    Ban { user: i64, name: String, reason: String },
    Transfer { user: i64, name: String },
    Delete { typed: String },
}

enum Ev {
    /// Действие выполнено (`refresh` — перечитать баны/приглашения).
    Done { res: Result<(), String>, refresh: bool },
    Bans(Result<Vec<Ban>, String>),
    Invites(Result<Vec<Invite>, String>),
    Icon(Result<Option<image::DynamicImage>, String>),
    /// Своя картинка баннера: готовые байты 960×480 (или ошибка).
    Banner(Result<Option<Vec<u8>>, String>),
}

pub struct ServerSettings {
    rt: tokio::runtime::Handle,
    ctx: egui::Context,
    api: Api,
    tx: std_mpsc::Sender<Ev>,
    rx: std_mpsc::Receiver<Ev>,
    server: Option<i64>,
    tab: Tab,
    name: String,
    bans: Option<Result<Vec<Ban>, String>>,
    invites: Option<Result<Vec<Invite>, String>>,
    confirm: Option<Confirm>,
    busy: bool,
    error: Option<String>,
    icon_preview: Option<TextureHandle>,
    crop: Option<crate::crop::CropDialog>,
    /// Поля профиля (правка до «Сохранить»).
    description: String,
    banner: Option<String>,
    color: Option<String>,
}

impl ServerSettings {
    pub fn new(rt: tokio::runtime::Handle, ctx: egui::Context, api: Api) -> Self {
        let (tx, rx) = std_mpsc::channel();
        Self {
            rt,
            ctx,
            api,
            tx,
            rx,
            server: None,
            tab: Tab::Overview,
            name: String::new(),
            bans: None,
            invites: None,
            confirm: None,
            busy: false,
            error: None,
            icon_preview: None,
            crop: None,
            description: String::new(),
            banner: None,
            color: None,
        }
    }

    pub fn set_api(&mut self, api: Api) {
        self.api = api;
        let (tx, rx) = std_mpsc::channel();
        self.tx = tx;
        self.rx = rx;
        self.server = None;
    }

    pub fn is_open(&self) -> bool {
        self.server.is_some()
    }

    pub fn open(&mut self, model: &Model, server: i64) {
        self.server = Some(server);
        let s = model.server(server);
        self.name = s.map(|s| s.name.clone()).unwrap_or_default();
        self.description = s.and_then(|s| s.description.clone()).unwrap_or_default();
        self.banner = s.and_then(|s| s.banner.clone());
        self.color = s.and_then(|s| s.color.clone());
        self.tab = if model.has_perm(server, perm::MANAGE_SERVER) { Tab::Overview } else { Tab::Members };
        self.bans = None;
        self.invites = None;
        self.confirm = None;
        self.busy = false;
        self.error = None;
        self.icon_preview = None;
    }

    fn spawn(&self, fut: impl std::future::Future<Output = Ev> + Send + 'static) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            let _ = tx.send(fut.await);
            ctx.request_repaint();
        });
    }

    /// Действие с ответом без данных: модель обновят события сервера.
    fn act<F>(&mut self, refresh: bool, fut: F)
    where
        F: std::future::Future<Output = Result<(), api::ApiError>> + Send + 'static,
    {
        self.busy = true;
        self.error = None;
        self.spawn(async move { Ev::Done { res: fut.await.map_err(|e| e.message), refresh } });
    }

    fn load_bans(&self, server: i64) {
        let api = self.api.clone();
        self.spawn(async move { Ev::Bans(api.bans(server).await.map_err(|e| e.message)) });
    }

    fn load_invites(&self, server: i64) {
        let api = self.api.clone();
        self.spawn(async move { Ev::Invites(api.server_invites(server).await.map_err(|e| e.message)) });
    }

    pub fn poll(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Ev::Done { res, refresh } => {
                    self.busy = false;
                    match res {
                        Ok(()) => {
                            self.confirm = None;
                            if refresh && let Some(s) = self.server {
                                match self.tab {
                                    Tab::Bans => self.load_bans(s),
                                    Tab::Invites => self.load_invites(s),
                                    _ => {}
                                }
                            }
                        }
                        Err(e) => self.error = Some(e),
                    }
                }
                Ev::Bans(r) => self.bans = Some(r),
                Ev::Invites(r) => self.invites = Some(r),
                Ev::Banner(r) => match r {
                    Ok(Some(bytes)) => {
                        if let Some(s) = self.server {
                            let api = self.api.clone();
                            self.act(false, async move { api.put_server_banner(s, bytes).await.map(|_| ()) });
                        }
                    }
                    Ok(None) => {}
                    Err(e) => self.error = Some(e),
                },
                Ev::Icon(r) => match r {
                    Ok(Some(img)) => self.crop = Some(crate::crop::CropDialog::new(&self.ctx, "Значок сервера", img)),
                    Ok(None) => {}
                    Err(e) => self.error = Some(e),
                },
            }
        }
    }

    pub fn ui(&mut self, ctx: &egui::Context, model: &Model, people: &People) {
        let Some(server) = self.server else { return };
        // сервер пропал (удалили, выгнали) — закрыть
        let Some(s) = model.server(server).cloned() else {
            self.server = None;
            return;
        };
        let perms = model.my_perms(server);
        let manage = perm::has(perms, perm::MANAGE_SERVER) || model.is_owner(server);
        let mut tabs: Vec<(Option<Tab>, &str, &str)> = Vec::new();
        if manage {
            tabs.push((Some(Tab::Overview), ph::PAINT_BRUSH, "Профиль сервера"));
        }
        tabs.push((Some(Tab::Members), ph::USERS, "Участники"));
        tabs.push((Some(Tab::Invites), ph::USER_PLUS, "Приглашения"));
        if perm::has(perms, perm::BAN) {
            tabs.push((Some(Tab::Bans), ph::PROHIBIT, "Баны"));
        }
        tabs.push((None, ph::SHIELD_CHECK, "Роли · скоро"));
        tabs.push((None, ph::GAVEL, "Модерация · скоро"));
        if !tabs.iter().any(|(t, _, _)| *t == Some(self.tab)) {
            self.tab = Tab::Members;
        }
        let width = (ctx.content_rect().width() - 60.0).clamp(640.0, 900.0);
        let res = kit::modal(ctx, "server-settings", &format!("Сервер «{}»", s.name), width, |ui| {
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(190.0);
                    ui.spacing_mut().item_spacing.y = 2.0;
                    for (tab, icon, label) in &tabs {
                        let active = *tab == Some(self.tab);
                        if kit::row(ui, icon, label, active, None, tab.is_some()).clicked()
                            && let Some(tab) = tab
                            && !active
                        {
                            self.tab = *tab;
                            self.error = None;
                            self.confirm = None;
                            match tab {
                                Tab::Bans => self.load_bans(server),
                                Tab::Invites => self.load_invites(server),
                                _ => {}
                            }
                        }
                    }
                });
                ui.add_space(14.0);
                let (sep, _) = ui.allocate_exact_size(vec2(1.0, 460.0), Sense::hover());
                ui.painter().vline(sep.center().x, sep.y_range(), Stroke::new(1.0, t::p().border));
                ui.add_space(14.0);
                ui.vertical(|ui| {
                    egui::ScrollArea::vertical().max_height(460.0).auto_shrink([false, true]).show(ui, |ui| match self.tab {
                        Tab::Overview => self.ui_overview(ui, model, people, &s),
                        Tab::Members => self.ui_members(ui, model, people, server),
                        Tab::Bans => self.ui_bans(ui, people, server),
                        Tab::Invites => self.ui_invites(ui, model, server),
                    });
                    if let Some(e) = &self.error {
                        ui.add_space(8.0);
                        ui.label(RichText::new(format!("{}  {e}", ph::WARNING_CIRCLE)).size(13.0).color(t::p().danger_text));
                    }
                });
            });
        });
        self.ui_confirm(ctx, server);
        if let Some(c) = self.crop.as_mut() {
            match c.ui(ctx) {
                crate::crop::CropResult::Open => {}
                crate::crop::CropResult::Cancel => self.crop = None,
                crate::crop::CropResult::Done(png) => {
                    self.crop = None;
                    self.icon_preview = people::round_image(&png, 112)
                        .ok()
                        .map(|ci| self.ctx.load_texture("server-icon-preview", ci, TextureOptions::LINEAR));
                    let api = self.api.clone();
                    self.act(false, async move { api.put_server_icon(server, png).await.map(|_| ()) });
                }
            }
        }
        if res == kit::ModalResult::Closed && self.confirm.is_none() && self.crop.is_none() {
            self.server = None;
        }
    }

    /// Профиль сервера: поля слева, предпросмотр справа.
    fn ui_overview(&mut self, ui: &mut Ui, model: &Model, people: &People, s: &proto::Server) {
        let manage = model.has_perm(s.id, perm::MANAGE_SERVER) || model.is_owner(s.id);
        let full_w = ui.available_width();
        let preview_w = 280.0;
        let side = full_w > 600.0;
        let w = if side { full_w - preview_w - 24.0 } else { full_w };
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(w);
                if manage {
                    self.profile_fields(ui, people, s, w);
                }
                if model.is_owner(s.id) {
                    ui.add_space(22.0);
                    label(ui, "Опасная зона");
                    ui.label(RichText::new("Передать владение — в «Участниках»: меню у человека, затем «Сделать владельцем».").size(13.0).color(t::p().text_muted));
                    ui.add_space(8.0);
                    if kit::button_ex(ui, ButtonKind::Danger, "Удалить сервер", Some(egui_phosphor::fill::TRASH), !self.busy).clicked() {
                        self.confirm = Some(Confirm::Delete { typed: String::new() });
                    }
                }
            });
            if side {
                ui.add_space(24.0);
                ui.vertical(|ui| {
                    ui.set_width(preview_w);
                    self.preview(ui, model, people, s, preview_w);
                });
            }
        });
    }

    /// Сервер с несохранёнными правками (для предпросмотра).
    fn edited(&self, s: &proto::Server) -> proto::Server {
        let mut e = s.clone();
        if !self.name.trim().is_empty() {
            e.name = self.name.trim().to_string();
        }
        e.description = Some(self.description.trim().to_string()).filter(|d| !d.is_empty());
        e.banner = self.banner.clone();
        e.color = self.color.clone();
        e
    }

    fn profile_fields(&mut self, ui: &mut Ui, people: &People, s: &proto::Server, w: f32) {
        let p = t::p();
        label(ui, "Название");
        ui.add(kit::text_field(&mut self.name, w).char_limit(limits::SERVER_NAME_MAX));
        ui.add_space(12.0);
        label(ui, "Иконка");
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(vec2(56.0, 56.0), Sense::hover());
            match self.icon_preview.as_ref().map(|t| t.id()) {
                Some(id) => {
                    egui::Image::new(egui::load::SizedTexture::new(id, rect.size())).corner_radius(CornerRadius::same(16)).paint_at(ui, rect);
                }
                None => crate::banner::paint_icon(ui.painter(), rect, &self.edited(s), people),
            }
            ui.add_space(8.0);
            ui.vertical(|ui| {
                if kit::button_ex(ui, ButtonKind::Secondary, "Загрузить…", Some(egui_phosphor::fill::IMAGE), !self.busy).clicked() {
                    self.spawn(async move {
                        let r = tokio::task::spawn_blocking(|| crate::servers_ui::pick_image("Значок сервера")).await.map_err(|e| e.to_string());
                        Ev::Icon(r.and_then(|r| r))
                    });
                }
                if s.icon.is_some() && kit::button_ex(ui, ButtonKind::Ghost, "Убрать", None, !self.busy).clicked() {
                    self.icon_preview = None;
                    let (api, id) = (self.api.clone(), s.id);
                    self.act(false, async move { api.delete_server_icon(id).await.map(|_| ()) });
                }
            });
        });
        ui.label(RichText::new("квадрат от 256 px").size(12.0).color(p.text_muted));
        ui.add_space(12.0);
        label(ui, "Баннер");
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            let current = self.banner.clone().unwrap_or_else(|| proto::banner::WAVES.to_string());
            for name in proto::banner::ALL {
                let (rect, resp) = ui.allocate_exact_size(vec2(92.0, 52.0), Sense::click());
                crate::banner::paint_pattern(ui.painter(), rect, name, CornerRadius::same(10), 0.4);
                ui.painter().text(rect.left_bottom() + vec2(8.0, -8.0), Align2::LEFT_BOTTOM, crate::banner::pattern_label(name), t::bold(12.0), Color32::WHITE);
                let selected = s.banner_image.is_none() && current == name;
                if selected || resp.hovered() {
                    let c = if selected { p.accent } else { p.border_strong };
                    ui.painter().rect_stroke(rect.expand(2.0), CornerRadius::same(12), Stroke::new(2.0, c), egui::StrokeKind::Outside);
                }
                if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    self.banner = (name != proto::banner::WAVES).then(|| name.to_string());
                    if s.banner_image.is_some() {
                        let (api, id) = (self.api.clone(), s.id);
                        self.act(false, async move { api.delete_server_banner(id).await.map(|_| ()) });
                    }
                }
            }
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if kit::button_ex(ui, ButtonKind::Secondary, "Своё изображение…", Some(egui_phosphor::fill::IMAGE), !self.busy).clicked() {
                self.spawn(async move {
                    let r = tokio::task::spawn_blocking(|| {
                        let img = crate::servers_ui::pick_image("Баннер сервера")?;
                        img.map(|i| banner_bytes(&i)).transpose()
                    })
                    .await
                    .map_err(|e| e.to_string());
                    Ev::Banner(r.and_then(|r| r))
                });
            }
            if s.banner_image.is_some() && kit::button_ex(ui, ButtonKind::Ghost, "Убрать картинку", None, !self.busy).clicked() {
                let (api, id) = (self.api.clone(), s.id);
                self.act(false, async move { api.delete_server_banner(id).await.map(|_| ()) });
            }
        });
        ui.label(RichText::new("960×480; другая картинка обрежется по центру. Полоса затемнения под названием ложится сама.").size(12.0).color(p.text_muted));
        ui.add_space(12.0);
        label(ui, "Цвет сервера");
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            let mut pick = |ui: &mut Ui, value: Option<&str>, fill: Color32, tip: &str| {
                let (rect, resp) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::click());
                ui.painter().circle_filled(rect.center(), 13.0, fill);
                let selected = self.color.as_deref() == value;
                if selected {
                    ui.painter().circle_stroke(rect.center(), 15.5, Stroke::new(2.0, p.text));
                }
                if resp.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    self.color = value.map(str::to_string);
                }
            };
            pick(ui, None, p.accent_strong, "Фирменный");
            for c in proto::SERVER_COLORS {
                pick(ui, Some(c), crate::banner::parse_hex(c).unwrap_or(p.accent), c);
            }
        });
        ui.label(RichText::new("Фон иконки и полоска выбранного канала. Кнопки и волны остаются фирменными.").size(12.0).color(p.text_muted));
        ui.add_space(12.0);
        label(ui, "Описание");
        ui.add(kit::text_area(&mut self.description, w).hint_text("видно под баннером и в приглашении").char_limit(limits::SERVER_DESCRIPTION_MAX).desired_rows(2));
        ui.label(RichText::new(format!("{} / {}", self.description.chars().count(), limits::SERVER_DESCRIPTION_MAX)).size(11.5).color(p.text_faint));
        ui.add_space(12.0);
        let req = proto::UpdateServerRequest {
            name: Some(self.name.trim().to_string()).filter(|n| !n.is_empty() && *n != s.name),
            description: Some(self.description.trim().to_string()).filter(|d| d.as_str() != s.description.as_deref().unwrap_or("")),
            banner: (self.banner != s.banner).then(|| self.banner.clone().unwrap_or_default()),
            color: (self.color != s.color).then(|| self.color.clone().unwrap_or_default()),
        };
        let changed = req.name.is_some() || req.description.is_some() || req.banner.is_some() || req.color.is_some();
        if kit::button_ex(ui, ButtonKind::Primary, "Сохранить", None, changed && !self.busy).clicked() {
            let (api, id) = (self.api.clone(), s.id);
            self.act(false, async move { api.update_server_profile(id, req).await.map(|_| ()) });
        }
    }

    /// Живой предпросмотр: шапка боковой панели и карточка приглашения.
    fn preview(&self, ui: &mut Ui, model: &Model, people: &People, s: &proto::Server, w: f32) {
        let p = t::p();
        let e = self.edited(s);
        let list = model.members.get(&s.id);
        let members = list.map(|l| l.len()).unwrap_or(0);
        let online = list.map(|l| l.iter().filter(|m| m.online || Some(m.user.id) == model.me).count()).unwrap_or(0);
        label(ui, "Предпросмотр");
        let (rect, _) = ui.allocate_exact_size(vec2(w, crate::banner::HEIGHT), Sense::hover());
        let round = CornerRadius { nw: 12, ne: 12, sw: 0, se: 0 };
        crate::banner::paint_background(ui.painter(), rect, &e, people, round);
        crate::banner::paint_strip(&ui.painter_at(rect), rect, &e, people, members, online, true);
        egui::Frame::new()
            .fill(p.surface)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(CornerRadius { nw: 0, ne: 0, sw: 12, se: 12 })
            .inner_margin(egui::Margin::symmetric(12, 8))
            .show(ui, |ui| {
                ui.set_width(w - 24.0);
                match e.description.as_deref() {
                    Some(d) => ui.add(egui::Label::new(RichText::new(d).size(12.0).color(p.text_muted)).wrap()),
                    None => ui.label(RichText::new("описания нет").size(12.0).color(p.text_faint)),
                };
                ui.add_space(4.0);
                for name in ["общий", "General"] {
                    ui.label(RichText::new(format!("{}  {name}", if name == "общий" { ph::HASH } else { ph::SPEAKER_HIGH })).size(13.0).color(p.text_muted));
                }
            });
        ui.add_space(16.0);
        label(ui, "Карточка приглашения");
        egui::Frame::new()
            .fill(p.surface_2)
            .stroke(Stroke::new(1.0, p.border))
            .corner_radius(CornerRadius::same(kit::R_CARD))
            .inner_margin(egui::Margin::same(14))
            .show(ui, |ui| {
                ui.set_width(w - 28.0);
                ui.label(RichText::new("ВАС ПРИГЛАШАЮТ НА СЕРВЕР").font(t::bold(11.0)).color(p.text_muted));
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(vec2(48.0, 48.0), Sense::hover());
                    crate::banner::paint_icon(ui.painter(), r, &e, people);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 0.0;
                        ui.label(RichText::new(&e.name).font(t::xbold(15.0)).color(p.text));
                        ui.label(RichText::new(format!("{online} в сети · {}", t::plural(members, "участник", "участника", "участников"))).size(12.0).color(p.text_muted));
                    });
                });
                if let Some(d) = e.description.as_deref() {
                    ui.add_space(6.0);
                    ui.add(egui::Label::new(RichText::new(d).size(12.0).color(p.text_muted)).wrap());
                }
                ui.add_space(8.0);
                let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 36.0), Sense::hover());
                ui.painter().rect_filled(r, CornerRadius::same(kit::R_PILL), p.accent_strong);
                ui.painter().text(r.center(), Align2::CENTER_CENTER, "Войти", t::bold(14.0), p.on_accent);
            });
    }

    fn ui_members(&mut self, ui: &mut Ui, model: &Model, people: &People, server: i64) {
        let Some(s) = model.server(server) else { return };
        let mut list: Vec<_> = model.members.get(&server).cloned().unwrap_or_default();
        let pos = |rid: i64| s.roles.iter().find(|r| r.id == rid).map(|r| r.position).unwrap_or(0);
        list.sort_by_key(|m| (-pos(m.role_id), people.resolve(m.user.clone()).shown_name().to_lowercase()));
        let in_voice: Vec<i64> = model.channels(server).iter().flat_map(|c| model.in_voice(c.id).to_vec()).collect();
        ui.label(RichText::new(format!("{} участников", list.len())).size(13.0).color(t::p().text_muted));
        ui.add_space(6.0);
        for m in list {
            let u = people.resolve(m.user.clone());
            let role = s.roles.iter().find(|r| r.id == m.role_id).map(|r| r.name.clone()).unwrap_or_default();
            let acts = member_actions(model, server, u.id);
            let owner_here = model.is_owner(server) && Some(u.id) != model.me;
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(vec2(36.0, 36.0), Sense::hover());
                people.paint(ui.painter(), rect.center(), 16.0, &u, 0.0, false);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    let me = if Some(u.id) == model.me { "  (вы)" } else { "" };
                    ui.label(RichText::new(format!("{}{me}", u.shown_name())).font(t::semibold(14.0)));
                    ui.label(RichText::new(role).size(12.5).color(t::p().text_muted));
                });
                let any = !acts.roles.is_empty() || acts.kick || acts.ban || acts.disconnect || owner_here;
                if !any {
                    return;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let r = kit::icon_button(ui, ph::DOTS_THREE, kit::IconState::Normal, "Действия");
                    egui::Popup::menu(&r).width(240.0).show(|ui| {
                        for role in &acts.roles {
                            let cur = role.id == m.role_id;
                            let text = format!("Роль: {}{}", role.name, if cur { " (сейчас)" } else { "" });
                            if kit::menu_item(ui, egui_phosphor::fill::SHIELD, &text, false).clicked() && !cur {
                                let (api, uid, rid) = (self.api.clone(), u.id, role.id);
                                self.act(false, async move { api.set_role(server, uid, rid).await.map(|_| ()) });
                            }
                        }
                        if acts.disconnect && in_voice.contains(&u.id) && kit::menu_item(ui, egui_phosphor::fill::PHONE_X, "Выкинуть из голоса", false).clicked() {
                            let (api, uid) = (self.api.clone(), u.id);
                            self.act(false, async move { api.disconnect(server, uid).await });
                        }
                        if owner_here && kit::menu_item(ui, egui_phosphor::fill::CROWN, "Сделать владельцем", false).clicked() {
                            self.confirm = Some(Confirm::Transfer { user: u.id, name: u.shown_name().to_string() });
                        }
                        if acts.kick && kit::menu_item(ui, egui_phosphor::fill::SIGN_OUT, "Выгнать", true).clicked() {
                            self.confirm = Some(Confirm::Kick { user: u.id, name: u.shown_name().to_string() });
                        }
                        if acts.ban && kit::menu_item(ui, egui_phosphor::fill::PROHIBIT, "Забанить", true).clicked() {
                            self.confirm = Some(Confirm::Ban { user: u.id, name: u.shown_name().to_string(), reason: String::new() });
                        }
                    });
                });
            });
        }
    }

    fn ui_bans(&mut self, ui: &mut Ui, people: &People, server: i64) {
        match &self.bans {
            None => {
                kit::spinner(ui, 18.0);
            }
            Some(Err(e)) => {
                ui.label(RichText::new(e).size(13.0).color(t::p().danger_text));
            }
            Some(Ok(list)) if list.is_empty() => {
                ui.label(RichText::new("Банов нет.").color(t::p().text_muted));
            }
            Some(Ok(list)) => {
                let list = list.clone();
                for b in list {
                    ui.horizontal(|ui| {
                        let (rect, _) = ui.allocate_exact_size(vec2(36.0, 36.0), Sense::hover());
                        people.paint(ui.painter(), rect.center(), 16.0, &b.user, 0.0, false);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 0.0;
                            ui.label(RichText::new(b.user.shown_name()).font(t::semibold(14.0)));
                            let reason = b.reason.as_deref().unwrap_or("без причины");
                            ui.label(RichText::new(reason).size(12.5).color(t::p().text_muted));
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if kit::button_ex(ui, ButtonKind::Secondary, "Разбанить", None, !self.busy).clicked() {
                                let (api, uid) = (self.api.clone(), b.user.id);
                                self.act(true, async move { api.unban(server, uid).await });
                            }
                        });
                    });
                }
            }
        }
    }

    fn ui_invites(&mut self, ui: &mut Ui, model: &Model, server: i64) {
        let all = model.has_perm(server, perm::MANAGE_SERVER);
        ui.label(RichText::new(if all { "Все приглашения сервера." } else { "Ваши приглашения." }).size(13.0).color(t::p().text_muted));
        ui.add_space(6.0);
        match &self.invites {
            None => {
                kit::spinner(ui, 18.0);
            }
            Some(Err(e)) => {
                ui.label(RichText::new(e).size(13.0).color(t::p().danger_text));
            }
            Some(Ok(list)) => {
                let now = chrono::Utc::now().timestamp();
                let live: Vec<Invite> = list
                    .iter()
                    .filter(|i| !i.revoked && i.uses_left > 0 && i.expires_at.is_none_or(|e| e > now))
                    .cloned()
                    .collect();
                if live.is_empty() {
                    ui.label(RichText::new("Действующих приглашений нет.").color(t::p().text_muted));
                }
                for inv in live {
                    ui.horizontal(|ui| {
                        let code = api::format_invite(&api::normalize_invite(&inv.code));
                        ui.label(RichText::new(&code).font(t::bold(16.0)).color(t::p().accent_text));
                        let who = model.user(inv.created_by).map(|u| u.shown_name().to_string()).unwrap_or_default();
                        let left = match inv.expires_at {
                            Some(e) => format!("{} ч", ((e - now) / 3600).max(1)),
                            None => "бессрочно".into(),
                        };
                        ui.label(RichText::new(format!("· {} вх. · {left} · {who}", inv.uses_left)).size(12.5).color(t::p().text_muted));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if kit::button_ex(ui, ButtonKind::Ghost, "Отозвать", None, !self.busy).clicked() {
                                let (api, c) = (self.api.clone(), inv.code.clone());
                                self.act(true, async move { api.revoke_invite(&c).await });
                            }
                            if kit::icon_button(ui, ph::COPY, kit::IconState::Normal, "Скопировать").clicked() {
                                ui.ctx().copy_text(code.clone());
                            }
                        });
                    });
                }
            }
        }
    }

    fn ui_confirm(&mut self, ctx: &egui::Context, server: i64) {
        let Some(mut c) = self.confirm.take() else { return };
        let (title, button) = match &c {
            Confirm::Kick { .. } => ("Выгнать участника", "Выгнать"),
            Confirm::Ban { .. } => ("Забанить участника", "Забанить"),
            Confirm::Transfer { .. } => ("Передать владение", "Передать"),
            Confirm::Delete { .. } => ("Удалить сервер", "Удалить навсегда"),
        };
        let mut go = false;
        let mut cancel = false;
        let busy = self.busy;
        let error = self.error.clone();
        let res = kit::modal(ctx, "server-confirm", title, 400.0, |ui| {
            match &mut c {
                Confirm::Kick { name, .. } => {
                    ui.label(RichText::new(format!("Выгнать {name}? Вернуться можно по новому приглашению.")).size(14.0));
                }
                Confirm::Ban { name, reason, .. } => {
                    ui.label(RichText::new(format!("Забанить {name}? Он пропадёт с сервера и не сможет вернуться по приглашению.")).size(14.0));
                    ui.add_space(10.0);
                    label(ui, "Причина (необязательно)");
                    ui.add(kit::text_field(reason, ui.available_width()).char_limit(200));
                }
                Confirm::Transfer { name, .. } => {
                    ui.label(RichText::new(format!("Сделать {name} владельцем? Вы станете админом.")).size(14.0));
                }
                Confirm::Delete { typed } => {
                    ui.label(RichText::new("Сервер, каналы, сообщения и файлы удалятся насовсем. Чтобы подтвердить, введите УДАЛИТЬ.").size(14.0));
                    ui.add_space(10.0);
                    ui.add(kit::text_field(typed, ui.available_width()).hint_text("УДАЛИТЬ"));
                }
            }
            if let Some(e) = &error {
                ui.add_space(8.0);
                ui.label(RichText::new(e).size(13.0).color(t::p().danger_text));
            }
            ui.add_space(18.0);
            ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                let ok = match &c {
                    Confirm::Delete { typed } => typed.trim().eq_ignore_ascii_case("УДАЛИТЬ") || typed.trim().to_lowercase() == "удалить",
                    _ => true,
                };
                go = kit::button_ex(ui, ButtonKind::Danger, if busy { "Подождите…" } else { button }, None, ok && !busy).clicked();
                cancel = kit::button(ui, ButtonKind::Ghost, "Отмена", None).clicked();
            });
        });
        if go {
            let api = self.api.clone();
            match &c {
                Confirm::Kick { user, .. } => {
                    let u = *user;
                    self.act(false, async move { api.kick(server, u).await });
                }
                Confirm::Ban { user, reason, .. } => {
                    let (u, r) = (*user, Some(reason.trim().to_string()).filter(|r| !r.is_empty()));
                    self.act(false, async move { api.ban(server, u, r).await });
                }
                Confirm::Transfer { user, .. } => {
                    let u = *user;
                    self.act(false, async move { api.transfer(server, u).await.map(|_| ()) });
                }
                Confirm::Delete { .. } => self.act(false, async move { api.delete_server(server).await }),
            }
        }
        if res == kit::ModalResult::Open && !cancel {
            self.confirm = Some(c);
        } else {
            self.error = None;
        }
    }
}

/// Картинка → баннер 960×480 (обрезка по центру, «cover»), PNG; если больше 2 МБ — JPEG.
pub fn banner_bytes(img: &image::DynamicImage) -> Result<Vec<u8>, String> {
    let (w, h) = (limits::BANNER_W, limits::BANNER_H);
    let out = img.resize_to_fill(w, h, image::imageops::FilterType::Lanczos3);
    let mut png = std::io::Cursor::new(Vec::new());
    out.write_to(&mut png, image::ImageFormat::Png).map_err(|e| e.to_string())?;
    let png = png.into_inner();
    if png.len() <= limits::IMAGE_BANNER_MAX_BYTES {
        return Ok(png);
    }
    let mut jpg = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(out.to_rgb8()).write_to(&mut jpg, image::ImageFormat::Jpeg).map_err(|e| e.to_string())?;
    Ok(jpg.into_inner())
}

fn label(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).font(t::semibold(13.0)).color(t::p().text_muted));
}


#[cfg(test)]
mod tests {
    use super::*;
    use proto::{Member, PublicUser, Server};

    fn model(my_role: i64) -> Model {
        let roles = vec![
            Role { id: 10, name: "Владелец".into(), position: 3, permissions: perm::ALL },
            Role { id: 11, name: "Админ".into(), position: 2, permissions: perm::ALL },
            Role { id: 12, name: "Модератор".into(), position: 1, permissions: perm::INVITE | perm::KICK | perm::BAN | perm::DISCONNECT },
            Role { id: 13, name: "Участник".into(), position: 0, permissions: perm::INVITE },
        ];
        let mut m = Model::with_me(5);
        m.set_servers(vec![Server { id: 1, name: "s".into(), icon: None, owner_id: 9, roles, my_role_id: my_role, ..Default::default() }]);
        let u = |id: i64| PublicUser { id, name: format!("u{id}"), display_name: None, avatar: None };
        m.set_members(1, vec![
            Member { user: u(5), role_id: my_role, online: false, activity: None },
            Member { user: u(6), role_id: 11, online: false, activity: None },
            Member { user: u(7), role_id: 13, online: false, activity: None },
            Member { user: u(9), role_id: 10, online: false, activity: None },
        ]);
        m
    }

    #[test]
    fn moderator_cannot_touch_admin() {
        let m = model(12);
        assert_eq!(member_actions(&m, 1, 6), MemberActions::default());
    }

    #[test]
    fn moderator_on_member_no_roles() {
        let m = model(12);
        let a = member_actions(&m, 1, 7);
        assert!(a.roles.is_empty(), "у модератора нет MANAGE_ROLES");
        assert!(a.kick && a.ban && a.disconnect);
    }

    #[test]
    fn admin_assigns_only_lower_roles() {
        let m = model(11);
        let a = member_actions(&m, 1, 7);
        assert_eq!(a.roles.iter().map(|r| r.id).collect::<Vec<_>>(), vec![12, 13]);
        assert_eq!(member_actions(&m, 1, 9), MemberActions::default(), "владельца не трогаем");
    }

    #[test]
    fn nothing_on_myself() {
        let m = model(11);
        assert_eq!(member_actions(&m, 1, 5), MemberActions::default());
    }
}
