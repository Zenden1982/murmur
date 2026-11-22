//! Колонка серверов, меню сервера, диалоги «Создать сервер» / «Вступить по коду» / «Пригласить» (фаза 9.1).
//! Загрузка серверов (список, раскладки, участники, кто в голосе) — тоже здесь; результат кладётся в `Model`.

use std::sync::mpsc as std_mpsc;

use eframe::egui::{self, Align, Align2, CornerRadius, Layout, Rect, RichText, Sense, Stroke, TextureHandle, TextureOptions, Ui, pos2, vec2};
use egui_phosphor::regular as ph;
use proto::{ChannelLayout, Invite, InvitePreview, Member, Server, VoiceRoster, limits, perm};

use crate::api::{self, Api, ApiResult};
use crate::crop::{self, CropDialog, CropResult};
use crate::kit::{self, ButtonKind};
use crate::model::Model;
use crate::people::{self, People};
use crate::theme as t;

/// Ширина колонки серверов.
pub const COLUMN_W: f32 = 72.0;
/// Приглашение из меню: 10 входов, 7 дней.
const INVITE_USES: u32 = 10;
const INVITE_HOURS: u32 = 168;

/// Всё, что нужно знать о сервере, чтобы показать его.
pub struct Loaded {
    pub server: Server,
    pub layout: Option<ChannelLayout>,
    pub members: Vec<Member>,
    pub voice: Vec<VoiceRoster>,
}

/// Проверка кода приглашения. Ошибка: (код не найден/недействителен — не сетевая ошибка, текст).
type PreviewRes = Result<InvitePreview, (bool, String)>;

enum Ev {
    /// `since` — `Model::seq` в момент запроса.
    All { since: u64, res: ApiResult<Vec<Loaded>> },
    One(ApiResult<Loaded>),
    Created(Result<Server, String>),
    Joined(Result<Server, String>),
    Preview { code: String, res: PreviewRes },
    Invite(Result<Invite, String>),
    Left { server: i64, res: Result<(), String> },
    Icon(Result<Option<image::DynamicImage>, String>),
}

/// Что сделал человек в колонке/меню (обрабатывает App).
pub enum Action {
    OpenSettings(i64),
}

/// Итог `poll` для App.
#[derive(Default)]
pub struct PollOut {
    /// Первая загрузка после входа закончилась (можно выбрать сохранённый сервер).
    pub first_load: bool,
    /// Сделать активным (создали / вступили).
    pub select: Option<i64>,
    pub unauthorized: bool,
}

#[derive(PartialEq, Clone, Copy)]
enum AddTab {
    Create,
    Join,
}

struct AddDialog {
    tab: AddTab,
    name: String,
    icon: Option<Vec<u8>>,
    icon_tex: Option<TextureHandle>,
    code: String,
    preview: Option<(String, PreviewRes)>,
    busy: bool,
    error: Option<String>,
}

struct InviteDialog {
    server: i64,
    res: Option<Result<Invite, String>>,
    /// Сервер только что создан — сверху кот «Сервер готов!».
    created: bool,
}

struct LeaveDialog {
    server: i64,
    name: String,
    busy: bool,
    error: Option<String>,
}

pub struct ServersUi {
    rt: tokio::runtime::Handle,
    ctx: egui::Context,
    api: Api,
    tx: std_mpsc::Sender<Ev>,
    rx: std_mpsc::Receiver<Ev>,
    add: Option<AddDialog>,
    invite: Option<InviteDialog>,
    leave: Option<LeaveDialog>,
    loaded_once: bool,
    pub load_error: Option<String>,
    /// Обрезка значка нового сервера.
    crop: Option<CropDialog>,
}

/// Буквы для значка без картинки: первые буквы двух первых слов (или одна буква).
pub fn initials(name: &str) -> String {
    let s: String = name.split_whitespace().take(2).filter_map(|w| w.chars().next()).flat_map(char::to_uppercase).collect();
    if s.is_empty() { "?".into() } else { s }
}

async fn load_one(api: &Api, server: Server) -> ApiResult<Loaded> {
    let id = server.id;
    let (layout, members, voice) = tokio::join!(api.layout(id), api.members(id), api.voice(id));
    Ok(Loaded { server, layout: Some(layout?), members: members?, voice: voice.unwrap_or_default() })
}

/// Выбрать картинку с диска и прочитать (дальше — окно обрезки). Блокирует — вызывать в `spawn_blocking`.
pub fn pick_image(title: &str) -> Result<Option<image::DynamicImage>, String> {
    let Some(path) = rfd::FileDialog::new()
        .set_title(title)
        .set_parent(&crate::desktop::main_window())
        .add_filter("Картинки", &["png", "jpg", "jpeg", "gif", "webp", "bmp"])
        .pick_file()
    else {
        return Ok(None);
    };
    let bytes = std::fs::read(&path).map_err(|e| format!("Не читается файл: {e}"))?;
    crop::load(&bytes).map(Some)
}

/// Картинка профиля (аватар/баннер): исходник целиком (GIF — со всеми кадрами). Блокирует — звать в фоне.
pub fn pick_source(title: &str) -> Result<Option<crop::Source>, String> {
    let Some(path) = rfd::FileDialog::new()
        .set_title(title)
        .set_parent(&crate::desktop::main_window())
        .add_filter("Картинки", &["png", "jpg", "jpeg", "gif", "webp"])
        .pick_file()
    else {
        return Ok(None);
    };
    let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if len > proto::limits::PROFILE_SOURCE_MAX_BYTES as u64 {
        return Err(format!("Файл больше {} МБ.", proto::limits::PROFILE_SOURCE_MAX_BYTES / 1024 / 1024));
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("Не читается файл: {e}"))?;
    crop::load_source(bytes).map(Some)
}

impl ServersUi {
    pub fn new(rt: tokio::runtime::Handle, ctx: egui::Context, api: Api) -> Self {
        let (tx, rx) = std_mpsc::channel();
        Self { rt, ctx, api, tx, rx, add: None, invite: None, leave: None, loaded_once: false, load_error: None, crop: None }
    }

    /// Новая сессия: забыть всё (старые задачи пишут в закрытый канал).
    pub fn set_api(&mut self, api: Api) {
        self.api = api;
        let (tx, rx) = std_mpsc::channel();
        self.tx = tx;
        self.rx = rx;
        self.add = None;
        self.invite = None;
        self.leave = None;
        self.loaded_once = false;
        self.load_error = None;
    }

    fn spawn(&self, fut: impl std::future::Future<Output = Ev> + Send + 'static) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            let _ = tx.send(fut.await);
            ctx.request_repaint();
        });
    }

    pub fn loaded_once(&self) -> bool {
        self.loaded_once
    }

    /// Перечитать все мои серверы (`since` — `Model::seq()` сейчас).
    pub fn reload(&self, since: u64) {
        if !self.api.has_token() {
            return;
        }
        let api = self.api.clone();
        self.spawn(async move {
            let res = async {
                let servers = api.servers().await?;
                let mut out = Vec::with_capacity(servers.len());
                for s in servers {
                    out.push(load_one(&api, s).await?);
                }
                Ok(out)
            }
            .await;
            Ev::All { since, res }
        });
    }

    /// Перечитать один сервер (после вступления, при ошибке перетаскивания).
    pub fn reload_one(&self, server: Server) {
        let api = self.api.clone();
        self.spawn(async move { Ev::One(load_one(&api, server).await) });
    }

    pub fn poll(&mut self, model: &mut Model, people: &mut People) -> PollOut {
        let mut out = PollOut::default();
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Ev::All { since, res: Ok(list) } => {
                    // сервер удалили, пока шёл запрос — его данные не возвращаем
                    let list: Vec<Loaded> = list.into_iter().filter(|l| model.accept_snapshot(since, l.server.id)).collect();
                    model.set_servers(list.iter().map(|l| l.server.clone()).collect());
                    for l in list {
                        put(model, people, l);
                    }
                    self.load_error = None;
                    if !self.loaded_once {
                        self.loaded_once = true;
                        out.first_load = true;
                    }
                }
                Ev::All { res: Err(e), .. } | Ev::One(Err(e)) => {
                    out.unauthorized |= e.is_unauthorized();
                    self.load_error = Some(e.message);
                }
                Ev::One(Ok(l)) => {
                    // только если я всё ещё на сервере (могли выгнать, пока шёл запрос)
                    if model.server(l.server.id).is_some() {
                        put(model, people, l);
                    }
                }
                Ev::Created(ref res) | Ev::Joined(ref res) => {
                    let created = matches!(ev, Ev::Created(_));
                    if let Some(d) = self.add.as_mut() {
                        d.busy = false;
                    }
                    match res.clone() {
                        Ok(server) => {
                            self.add = None;
                            out.select = Some(server.id);
                            model.apply(&proto::ServerEvent::ServerUpserted { server: server.clone() });
                            // свой новый сервер — сразу окно приглашения с котом «Сервер готов!»
                            if created {
                                self.open_invite(server.id);
                                if let Some(d) = self.invite.as_mut() {
                                    d.created = true;
                                }
                            }
                            self.reload_one(server);
                        }
                        Err(e) => {
                            if let Some(d) = self.add.as_mut() {
                                d.error = Some(e);
                            }
                        }
                    }
                }
                Ev::Preview { code, res } => {
                    if let Some(d) = self.add.as_mut() {
                        d.preview = Some((code, res));
                    }
                }
                Ev::Invite(res) => {
                    if let Some(d) = self.invite.as_mut() {
                        d.res = Some(res);
                    }
                }
                Ev::Left { server, res } => match res {
                    Ok(()) => {
                        self.leave = None;
                        model.apply(&proto::ServerEvent::ServerRemoved { server_id: server });
                    }
                    Err(e) => {
                        if let Some(d) = self.leave.as_mut() {
                            d.busy = false;
                            d.error = Some(e);
                        }
                    }
                },
                Ev::Icon(res) => {
                    if let Some(d) = self.add.as_mut() {
                        match res {
                            Ok(Some(img)) => {
                                d.error = None;
                                self.crop = Some(CropDialog::new(&self.ctx, "Значок сервера", img));
                            }
                            Ok(None) => {}
                            Err(e) => d.error = Some(e),
                        }
                    }
                }
            }
        }
        out
    }

    pub fn modal_open(&self) -> bool {
        self.add.is_some() || self.invite.is_some() || self.leave.is_some()
    }

    fn ui_crop(&mut self, ctx: &egui::Context) {
        let Some(c) = self.crop.as_mut() else { return };
        match c.ui(ctx) {
            CropResult::Open => {}
            CropResult::Cancel | CropResult::Render(_) => self.crop = None,
            CropResult::Done(png) => {
                self.crop = None;
                if let Some(d) = self.add.as_mut() {
                    d.icon_tex = people::round_image(&png, 112)
                        .ok()
                        .map(|ci| self.ctx.load_texture("new-server-icon", ci, TextureOptions::LINEAR));
                    d.icon = Some(png);
                }
            }
        }
    }

    fn open_add(&mut self) {
        self.add = Some(AddDialog {
            tab: AddTab::Create,
            name: String::new(),
            icon: None,
            icon_tex: None,
            code: String::new(),
            preview: None,
            busy: false,
            error: None,
        });
    }

    pub fn open_invite(&mut self, server: i64) {
        self.invite = Some(InviteDialog { server, res: None, created: false });
        let api = self.api.clone();
        self.spawn(async move {
            Ev::Invite(api.create_server_invite(server, INVITE_USES, INVITE_HOURS).await.map_err(|e| e.message))
        });
    }

    // ------------------------------------------------------------ колонка

    /// Колонка значков. Возвращает выбранный сервер.
    /// `dm_open` — открыт личный диалог: активный сервер не подсвечивается.
    pub fn column(&mut self, ui: &mut Ui, model: &Model, people: &People, unread: &crate::unread::Unread, dm_open: bool) -> Option<i64> {
        let mut pick = None;
        let left = ui.max_rect().left();
        ui.vertical_centered(|ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            ui.add_space(12.0);
            for s in &model.servers {
                let active = model.active == Some(s.id) && !dm_open;
                let mark = match (s.icon.as_deref().and_then(|k| people.texture(k)), crate::banner::server_color(s)) {
                    (Some(tex), _) => kit::ServerMark::Image(tex.id()),
                    (None, Some(c)) => kit::ServerMark::Colored(initials(&s.name), c),
                    (None, None) => kit::ServerMark::Letter(initials(&s.name)),
                };
                let resp = kit::server_icon(ui, &mark, active, &s.name);
                if active || resp.hovered() || unread.server_has(s.id) {
                    // полоска слева: у активного — высокая, при наведении — короткая, непрочитанное — точка
                    let h = if active { 36.0 } else if resp.hovered() { 18.0 } else { 8.0 };
                    let r = Rect::from_center_size(pos2(left + 2.0, resp.rect.center().y), vec2(4.0, h));
                    ui.painter().rect_filled(r, CornerRadius::same(2), if active { t::p().accent } else { t::p().text_muted });
                }
                if resp.clicked() {
                    pick = Some(s.id);
                }
            }
            if !model.servers.is_empty() {
                let (r, _) = ui.allocate_exact_size(vec2(32.0, 2.0), Sense::hover());
                ui.painter().hline(r.x_range(), r.center().y, Stroke::new(2.0, t::p().border));
            }
            if kit::server_icon(ui, &kit::ServerMark::Letter("+".into()), false, "Создать сервер или вступить по коду").clicked() {
                self.open_add();
            }
        });
        pick
    }

    // ------------------------------------------------------------ шапка сайдбара

    /// Шапка боковой панели: баннер сервера (узор или картинка) с полосой затемнения — иконка, название,
    /// «N участников · M в сети», ▾ меню; под баннером — описание. Без сервера — просто «Murmur».
    pub fn header(&mut self, ui: &mut Ui, model: &Model, people: &People) -> Option<Action> {
        let Some(s) = model.active_server().cloned() else {
            egui::Frame::new().inner_margin(egui::Margin { left: 18, right: 18, top: 16, bottom: 12 }).show(ui, |ui| {
                ui.label(RichText::new("Murmur").font(t::xbold(18.0)).color(t::p().text));
            });
            return None;
        };
        let id = s.id;
        let mut act = None;
        let w = ui.available_width();
        let (rect, resp) = ui.allocate_exact_size(vec2(w, crate::banner::HEIGHT), Sense::click());
        let painter = ui.painter_at(rect);
        crate::banner::paint_background(&painter, rect, &s, people, CornerRadius::ZERO);
        let list = model.members.get(&id);
        let members = list.map(|l| l.len()).unwrap_or(0);
        let online = list.map(|l| l.iter().filter(|m| m.online || Some(m.user.id) == model.me).count()).unwrap_or(0);
        crate::banner::paint_strip(&painter, rect, &s, people, members, online, true);
        if resp.hovered() {
            painter.rect_filled(rect, CornerRadius::ZERO, egui::Color32::from_white_alpha(8));
        }
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        let perms = model.my_perms(id);
        egui::Popup::menu(&resp).width(230.0).show(|ui| {
            if perm::has(perms, perm::INVITE) && kit::menu_item(ui, egui_phosphor::fill::USER_PLUS, "Пригласить", false).clicked() {
                self.open_invite(id);
            }
            if perms & (perm::MANAGE_SERVER | perm::MANAGE_ROLES | perm::BAN) != 0
                && kit::menu_item(ui, egui_phosphor::fill::GEAR_SIX, "Настройки сервера", false).clicked()
            {
                act = Some(Action::OpenSettings(id));
            }
            if !model.is_owner(id) && kit::menu_item(ui, egui_phosphor::fill::SIGN_OUT, "Выйти с сервера", true).clicked() {
                self.leave = Some(LeaveDialog { server: id, name: s.name.clone(), busy: false, error: None });
            }
        });
        if let Some(d) = s.description.as_deref().filter(|d| !d.is_empty()) {
            egui::Frame::new().inner_margin(egui::Margin { left: 14, right: 14, top: 8, bottom: 2 }).show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(d).size(12.0).color(t::p().text_muted)).wrap());
            });
        }
        act
    }

    // ------------------------------------------------------------ диалоги

    pub fn dialogs(&mut self, ctx: &egui::Context) {
        self.ui_add(ctx);
        self.ui_crop(ctx);
        self.ui_invite(ctx);
        self.ui_leave(ctx);
    }

    fn ui_add(&mut self, ctx: &egui::Context) {
        let Some(mut d) = self.add.take() else { return };
        let mut submit = false;
        let mut pick_icon = false;
        let title = if d.tab == AddTab::Create { "Создать сервер" } else { "Вступить на сервер" };
        let res = kit::modal(ctx, "add-server", title, 400.0, |ui| {
            ui.horizontal(|ui| {
                for (tab, label) in [(AddTab::Create, "Создать"), (AddTab::Join, "По коду приглашения")] {
                    let kind = if d.tab == tab { ButtonKind::Primary } else { ButtonKind::Secondary };
                    if kit::button(ui, kind, label, None).clicked() && d.tab != tab {
                        d.tab = tab;
                        d.error = None;
                    }
                }
            });
            ui.add_space(16.0);
            let w = ui.available_width();
            match d.tab {
                AddTab::Create => {
                    ui.horizontal(|ui| {
                        let (rect, resp) = ui.allocate_exact_size(vec2(56.0, 56.0), Sense::click());
                        match &d.icon_tex {
                            Some(tex) => {
                                egui::Image::new(egui::load::SizedTexture::new(tex.id(), rect.size())).paint_at(ui, rect);
                            }
                            None => {
                                ui.painter().circle_filled(rect.center(), 28.0, t::p().surface_3);
                                let letters = if d.name.trim().is_empty() { ph::CAMERA.to_string() } else { initials(&d.name) };
                                ui.painter().text(rect.center(), Align2::CENTER_CENTER, letters, t::xbold(18.0), t::p().text_muted);
                            }
                        }
                        pick_icon |= resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked();
                        ui.vertical(|ui| {
                            ui.label(RichText::new("Значок").font(t::semibold(14.0)));
                            pick_icon |= kit::button(ui, ButtonKind::Ghost, "Выбрать картинку", Some(egui_phosphor::fill::IMAGE)).clicked();
                        });
                    });
                    ui.add_space(12.0);
                    ui.label(RichText::new("Название").font(t::semibold(13.0)).color(t::p().text_muted));
                    let r = ui.add(kit::text_field(&mut d.name, w).hint_text("например, Лаборатория").char_limit(limits::SERVER_NAME_MAX));
                    submit |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                }
                AddTab::Join => {
                    ui.label(RichText::new("Код приглашения").font(t::semibold(13.0)).color(t::p().text_muted));
                    let r = ui.add(kit::text_field(&mut d.code, w).hint_text("XXXX-XXXX-XXXX"));
                    submit |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    let code = api::normalize_invite(&d.code);
                    if code.len() == 12 && d.preview.as_ref().is_none_or(|(c, _)| *c != code) {
                        d.preview = Some((code.clone(), Err((false, "Проверяю код…".into()))));
                        let (api, code) = (self.api.clone(), code.clone());
                        self.spawn(async move {
                            let res = api.invite_preview(&code).await.map_err(|e| (e.code != "network", e.message));
                            Ev::Preview { code, res }
                        });
                    }
                    ui.add_space(6.0);
                    match &d.preview {
                        Some((c, Ok(p))) if *c == code => {
                            ui.label(RichText::new(format!("Вас пригласили на «{}»", p.server_name)).font(t::semibold(14.0)).color(t::p().text));
                        }
                        Some((c, Err((true, _)))) if *c == code => {
                            ui.add_space(8.0);
                            kit::cat_state(ui, kit::Cat::NotFound, "Приглашение не найдено", "Ссылка устарела или в ней опечатка", None);
                        }
                        Some((c, Err((false, e)))) if *c == code => {
                            ui.label(RichText::new(e).size(13.0).color(t::p().text_muted));
                        }
                        _ => {
                            ui.label(RichText::new("Регистр и дефисы не важны.").size(13.0).color(t::p().text_muted));
                        }
                    }
                }
            }
            if let Some(e) = &d.error {
                ui.add_space(8.0);
                ui.label(RichText::new(format!("{}  {e}", ph::WARNING_CIRCLE)).size(13.0).color(t::p().danger_text));
            }
            ui.add_space(18.0);
            ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                let (text, ok) = match d.tab {
                    AddTab::Create => ("Создать", !d.name.trim().is_empty()),
                    AddTab::Join => ("Вступить", api::normalize_invite(&d.code).len() == 12),
                };
                let text = if d.busy { "Подождите…" } else { text };
                if kit::button_ex(ui, ButtonKind::Primary, text, None, ok && !d.busy).clicked() {
                    submit = true;
                }
                submit &= ok;
            });
        });
        if pick_icon && !d.busy {
            self.spawn(async move {
                let r = tokio::task::spawn_blocking(|| pick_image("Значок сервера")).await.map_err(|e| e.to_string());
                Ev::Icon(r.and_then(|r| r))
            });
        }
        if submit && !d.busy {
            d.busy = true;
            d.error = None;
            let api = self.api.clone();
            match d.tab {
                AddTab::Create => {
                    let (name, icon) = (d.name.trim().to_string(), d.icon.clone());
                    self.spawn(async move {
                        let s = match api.create_server(&name).await {
                            Ok(s) => s,
                            Err(e) => return Ev::Created(Err(e.message)),
                        };
                        // сервер уже создан: ошибка значка не повод создавать его ещё раз
                        let s = match icon {
                            Some(png) => api.put_server_icon(s.id, png).await.unwrap_or_else(|e| {
                                log::warn!("значок нового сервера {}: {e}", s.id);
                                s
                            }),
                            None => s,
                        };
                        Ev::Created(Ok(s))
                    });
                }
                AddTab::Join => {
                    let code = api::normalize_invite(&d.code);
                    self.spawn(async move { Ev::Joined(api.join_invite(&code).await.map_err(|e| e.message)) });
                }
            }
        }
        if res == kit::ModalResult::Open {
            self.add = Some(d);
        }
    }

    fn ui_invite(&mut self, ctx: &egui::Context) {
        let Some(d) = &self.invite else { return };
        let res = kit::modal(ctx, "server-invite", "Пригласить друзей", 400.0, |ui| {
            if d.created {
                kit::cat_state(ui, kit::Cat::Success, "Сервер готов!", "Скопируй код и позови друзей", None);
                ui.add_space(14.0);
            }
            match &d.res {
            None => {
                ui.horizontal(|ui| {
                    kit::spinner(ui, 18.0);
                    ui.label(RichText::new("Создаю приглашение…").color(t::p().text_muted));
                });
            }
            Some(Err(e)) => {
                ui.label(RichText::new(e).size(13.0).color(t::p().danger_text));
            }
            Some(Ok(inv)) => {
                ui.label(RichText::new("Отправьте код другу: в Murmur — «+», затем «По коду приглашения», а если аккаунта ещё нет — вкладка «У меня приглашение» при входе.").size(13.5).color(t::p().text_muted));
                ui.add_space(12.0);
                let code = api::format_invite(&api::normalize_invite(&inv.code));
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&code).font(t::xbold(24.0)).color(t::p().accent_text));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if kit::icon_button(ui, egui_phosphor::regular::COPY, kit::IconState::Normal, "Скопировать").clicked() {
                            ui.ctx().copy_text(code.clone());
                        }
                    });
                });
                ui.add_space(6.0);
                ui.label(RichText::new(format!("{INVITE_USES} входов · действует 7 дней")).size(12.5).color(t::p().text_muted));
            }
            }
        });
        let _ = d.server;
        if res == kit::ModalResult::Closed {
            self.invite = None;
        }
    }

    fn ui_leave(&mut self, ctx: &egui::Context) {
        let Some(mut d) = self.leave.take() else { return };
        let mut go = false;
        let mut cancel = false;
        let res = kit::modal(ctx, "leave-server", "Выйти с сервера", 400.0, |ui| {
            ui.label(RichText::new(format!("Выйти с «{}»? Вернуться можно будет только по новому приглашению.", d.name)).size(14.0));
            if let Some(e) = &d.error {
                ui.add_space(8.0);
                ui.label(RichText::new(e).size(13.0).color(t::p().danger_text));
            }
            ui.add_space(18.0);
            ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                go = kit::button_ex(ui, ButtonKind::Danger, if d.busy { "Подождите…" } else { "Выйти" }, None, !d.busy).clicked();
                cancel = kit::button(ui, ButtonKind::Ghost, "Отмена", None).clicked();
            });
        });
        if go {
            d.busy = true;
            d.error = None;
            let (api, server) = (self.api.clone(), d.server);
            self.spawn(async move { Ev::Left { server, res: api.leave(server).await.map_err(|e| e.message) } });
        }
        if res == kit::ModalResult::Open && !cancel {
            self.leave = Some(d);
        }
    }
}

fn put(model: &mut Model, people: &mut People, l: Loaded) {
    let id = l.server.id;
    for m in &l.members {
        people.learn(m.user.clone());
    }
    model.set_members(id, l.members);
    if let Some(layout) = l.layout {
        model.set_layout(id, layout);
    }
    model.set_voice(id, l.voice);
}

#[cfg(test)]
mod tests {
    use super::initials;

    #[test]
    fn initials_from_words() {
        assert_eq!(initials("Steins Gate"), "SG");
        assert_eq!(initials("лаборатория"), "Л");
        assert_eq!(initials("  murmur  друзья  и  ещё"), "MД");
        assert_eq!(initials(""), "?");
    }
}
