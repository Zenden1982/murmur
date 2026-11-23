//! Список каналов: категории (сворачиваются), «кто в голосе» под голосовыми, лимит, перетаскивание и меню (фаза 9.1).

use std::sync::mpsc as std_mpsc;

use eframe::egui::{self, Align, Align2, CornerRadius, Layout, Rect, RichText, Sense, Stroke, Ui, pos2, vec2};
use egui_phosphor::regular as ph;
use proto::{Category, Channel, ChannelKind, ChannelLayout, ChannelPlace, CreateChannelRequest, LayoutRequest, UpdateChannelRequest, limits, perm};

use crate::api::Api;
use crate::kit::{self, ButtonKind};
use crate::model::Model;
use crate::people::People;
use crate::theme as t;
use crate::voice::Peer;

/// Куда бросили канал.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropTarget {
    /// Перед этим каналом (в его категории).
    Before(i64),
    /// В конец категории (`None` — каналы без категории).
    EndOf(Option<i64>),
}

/// Новый порядок после переноса канала `dragged` (раскладка — как её показывает `Model::sorted_layout`).
pub fn apply_drop(sorted: &[(Option<Category>, Vec<Channel>)], dragged: i64, target: DropTarget) -> LayoutRequest {
    // группы: (категория, каналы по порядку)
    let mut groups: Vec<(Option<i64>, Vec<i64>)> =
        sorted.iter().map(|(c, chs)| (c.as_ref().map(|c| c.id), chs.iter().map(|c| c.id).collect())).collect();
    if !groups.iter().any(|(c, _)| c.is_none()) {
        groups.insert(0, (None, Vec::new()));
    }
    let exists = groups.iter().any(|(_, ids)| ids.contains(&dragged));
    if exists && target != DropTarget::Before(dragged) {
        for (_, ids) in groups.iter_mut() {
            ids.retain(|&id| id != dragged);
        }
        match target {
            DropTarget::Before(anchor) => {
                if let Some((_, ids)) = groups.iter_mut().find(|(_, ids)| ids.contains(&anchor)) {
                    let i = ids.iter().position(|&id| id == anchor).unwrap_or(ids.len());
                    ids.insert(i, dragged);
                }
            }
            DropTarget::EndOf(cat) => {
                if let Some((_, ids)) = groups.iter_mut().find(|(c, _)| *c == cat) {
                    ids.push(dragged);
                }
            }
        }
    }
    LayoutRequest {
        categories: groups.iter().filter_map(|(c, _)| *c).collect(),
        channels: groups
            .iter()
            .flat_map(|(cat, ids)| ids.iter().map(move |&id| ChannelPlace { id, category_id: *cat }))
            .collect(),
    }
}

/// Раскладка, как её вернёт сервер после `put_layout` (чтобы показать сразу, до ответа).
pub fn layout_after(old: &ChannelLayout, req: &LayoutRequest) -> ChannelLayout {
    let mut out = old.clone();
    for c in out.categories.iter_mut() {
        if let Some(i) = req.categories.iter().position(|&id| id == c.id) {
            c.position = i as i32;
        }
    }
    for ch in out.channels.iter_mut() {
        if let Some(i) = req.channels.iter().position(|p| p.id == ch.id) {
            ch.position = i as i32;
            ch.category_id = req.channels[i].category_id;
        }
    }
    out
}


/// Что выбрал человек в списке.
pub enum Pick {
    Voice(Channel),
    Text(i64),
    /// ЛКМ по участнику голосового канала — карточка профиля.
    Profile(i64, egui::Pos2),
    /// ПКМ по участнику моего звонка — громкость.
    PeerVolume(String, egui::Pos2),
}

/// Что нужно списку от окна.
pub struct ListCtx<'a> {
    pub model: &'a Model,
    pub people: &'a People,
    /// Канал, в котором я сейчас (голос), и живые участники (с индикатором речи).
    pub my_voice: Option<(i64, &'a [Peer])>,
    pub open_text: Option<i64>,
    /// Непрочитанные сообщения.
    pub unread: &'a crate::unread::Unread,
    /// Свёрнутые категории (настройки).
    pub collapsed: &'a mut Vec<i64>,
}

enum Ev {
    Done(Result<(), String>),
    /// Перетаскивание не удалось — вернуть раскладку сервера.
    LayoutFailed { server: i64, error: String, layout: Option<ChannelLayout> },
}

enum Dialog {
    CreateChannel { server: i64, category: Option<i64>, name: String, kind: ChannelKind, limit: String },
    CreateCategory { server: i64, name: String },
    RenameChannel { id: i64, name: String },
    RenameCategory { id: i64, name: String },
    Limit { id: i64, limit: String },
    DeleteChannel { id: i64, name: String },
    DeleteCategory { id: i64, name: String },
}

pub struct ChannelsUi {
    rt: tokio::runtime::Handle,
    ctx: egui::Context,
    api: Api,
    tx: std_mpsc::Sender<Ev>,
    rx: std_mpsc::Receiver<Ev>,
    dialog: Option<Dialog>,
    busy: bool,
    error: Option<String>,
}

/// Перенос, который надо показать сразу и отправить на сервер.
pub struct Moved {
    pub server: i64,
    pub layout: ChannelLayout,
}

impl ChannelsUi {
    pub fn new(rt: tokio::runtime::Handle, ctx: egui::Context, api: Api) -> Self {
        let (tx, rx) = std_mpsc::channel();
        Self { rt, ctx, api, tx, rx, dialog: None, busy: false, error: None }
    }

    pub fn set_api(&mut self, api: Api) {
        self.api = api;
        let (tx, rx) = std_mpsc::channel();
        self.tx = tx;
        self.rx = rx;
        self.dialog = None;
        self.busy = false;
        self.error = None;
    }

    pub fn modal_open(&self) -> bool {
        self.dialog.is_some()
    }

    fn spawn(&self, fut: impl std::future::Future<Output = Ev> + Send + 'static) {
        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            let _ = tx.send(fut.await);
            ctx.request_repaint();
        });
    }

    /// Ответы сервера. Изменения раскладки приходят событием `layout_changed`, здесь — только ошибки и откаты.
    pub fn poll(&mut self, model: &mut Model) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Ev::Done(Ok(())) => {
                    self.busy = false;
                    self.dialog = None;
                    self.error = None;
                }
                Ev::Done(Err(e)) => {
                    self.busy = false;
                    self.error = Some(e);
                }
                Ev::LayoutFailed { server, error, layout } => {
                    log::warn!("порядок каналов не сохранён: {error}");
                    if let Some(l) = layout {
                        model.set_layout(server, l);
                    }
                }
            }
        }
    }

    /// Отправить новый порядок; при ошибке — вернуть раскладку сервера.
    pub fn send_layout(&self, server: i64, req: LayoutRequest) {
        let api = self.api.clone();
        self.spawn(async move {
            match api.put_layout(server, &req).await {
                Ok(_) => Ev::Done(Ok(())),
                Err(e) => Ev::LayoutFailed { server, error: e.message, layout: api.layout(server).await.ok() },
            }
        });
    }

    // ------------------------------------------------------------ список

    /// Список каналов активного сервера. Возвращает выбор и (если перетащили) новую раскладку.
    pub fn list(&mut self, ui: &mut Ui, cx: ListCtx) -> (Option<Pick>, Option<Moved>) {
        let mut pick = None;
        let mut moved = None;
        let Some(server) = cx.model.active else { return (None, None) };
        let manage = cx.model.has_perm(server, perm::MANAGE_CHANNELS);
        let sorted = cx.model.sorted_layout(server);
        let mut drop: Option<(i64, DropTarget)> = None;
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.add_space(6.0);
        if manage {
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                ui.label(RichText::new("Каналы").font(t::bold(12.5)).color(t::p().text_muted));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let r = kit::icon_button(ui, ph::PLUS, kit::IconState::Normal, "Создать канал или категорию");
                    egui::Popup::menu(&r).width(220.0).show(|ui| {
                        if kit::menu_item(ui, egui_phosphor::fill::HASH, "Создать канал", false).clicked() {
                            self.open(Dialog::CreateChannel { server, category: None, name: String::new(), kind: ChannelKind::Voice, limit: String::new() });
                        }
                        if kit::menu_item(ui, egui_phosphor::fill::FOLDER_SIMPLE_PLUS, "Создать категорию", false).clicked() {
                            self.open(Dialog::CreateCategory { server, name: String::new() });
                        }
                    });
                });
            });
        }
        if sorted.is_empty() {
            ui.horizontal(|ui| {
                ui.add_space(18.0);
                ui.label(RichText::new(if manage { "Каналов пока нет — «+» выше" } else { "Каналов пока нет" }).size(13.0).color(t::p().text_muted));
            });
        }
        for (cat, chans) in &sorted {
            if let Some(c) = cat {
                ui.add_space(8.0);
                let collapsed = cx.collapsed.contains(&c.id);
                let resp = self.category_row(ui, c, collapsed, manage, server);
                if resp.clicked() {
                    if collapsed {
                        cx.collapsed.retain(|&id| id != c.id);
                    } else {
                        cx.collapsed.push(c.id);
                    }
                }
                if manage && resp.dnd_hover_payload::<i64>().is_some() {
                    insert_line(ui, resp.rect.bottom() + 1.0, resp.rect.x_range());
                }
                if let Some(id) = resp.dnd_release_payload::<i64>() {
                    drop = Some((*id, DropTarget::EndOf(Some(c.id))));
                }
                if collapsed {
                    // свёрнута — показать только канал, где я сейчас, и открытый текстовый
                    for ch in chans.iter().filter(|ch| cx.my_voice.is_some_and(|(id, _)| id == ch.id) || cx.open_text == Some(ch.id)) {
                        self.channel_row(ui, ch, &cx, manage, &mut pick, &mut drop);
                    }
                    continue;
                }
            }
            for ch in chans {
                self.channel_row(ui, ch, &cx, manage, &mut pick, &mut drop);
            }
            // зона «в конец группы» (видна только при перетаскивании)
            if manage && egui::DragAndDrop::has_payload_of_type::<i64>(ui.ctx()) {
                let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 10.0), Sense::hover());
                if resp.dnd_hover_payload::<i64>().is_some() {
                    insert_line(ui, rect.top() + 2.0, rect.x_range());
                }
                if let Some(id) = resp.dnd_release_payload::<i64>() {
                    drop = Some((*id, DropTarget::EndOf(cat.as_ref().map(|c| c.id))));
                }
            }
        }
        // подпись у курсора при перетаскивании
        if let Some(id) = egui::DragAndDrop::payload::<i64>(ui.ctx())
            && let Some(pos) = ui.ctx().pointer_interact_pos()
            && let Some(ch) = sorted.iter().flat_map(|(_, c)| c).find(|c| c.id == *id)
        {
            egui::Area::new(egui::Id::new("drag-channel")).order(egui::Order::Tooltip).fixed_pos(pos + vec2(12.0, 4.0)).show(ui.ctx(), |ui| {
                egui::Frame::new().fill(t::p().surface).stroke(Stroke::new(1.0, t::p().border)).corner_radius(CornerRadius::same(8)).inner_margin(egui::Margin::symmetric(10, 6)).show(ui, |ui| {
                    ui.label(RichText::new(&ch.name).font(t::semibold(14.0)));
                });
            });
        }
        if let Some((id, target)) = drop {
            let req = apply_drop(&sorted, id, target);
            if let Some(old) = cx.model.layouts.get(&server) {
                let changed = req.channels.iter().enumerate().any(|(i, p)| {
                    old.channels.iter().find(|c| c.id == p.id).is_none_or(|c| c.category_id != p.category_id)
                        || sorted.iter().flat_map(|(_, c)| c).nth(i).is_none_or(|c| c.id != p.id)
                });
                if changed {
                    moved = Some(Moved { server, layout: layout_after(old, &req) });
                    self.send_layout(server, req);
                }
            }
        }
        (pick, moved)
    }

    fn category_row(&mut self, ui: &mut Ui, c: &Category, collapsed: bool, manage: bool, server: i64) -> egui::Response {
        let w = ui.available_width();
        let (rect, resp) = ui.allocate_exact_size(vec2(w, 24.0), Sense::click());
        let hover = resp.hovered();
        let color = if hover { t::p().text } else { t::p().text_muted };
        let caret = if collapsed { ph::CARET_RIGHT } else { ph::CARET_DOWN };
        let painter = ui.painter();
        painter.text(pos2(rect.left() + 8.0, rect.center().y), Align2::CENTER_CENTER, caret, egui::FontId::proportional(12.0), color);
        crate::banner::paint_truncated(painter, pos2(rect.left() + 18.0, rect.center().y), Align2::LEFT_CENTER, &c.name.to_uppercase(), t::bold(12.0), color, rect.width() - 18.0 - if manage { 32.0 } else { 8.0 });
        if manage {
            let plus = Rect::from_center_size(pos2(rect.right() - 12.0, rect.center().y), vec2(20.0, 20.0));
            let pr = ui.interact(plus, ui.id().with(("cat-plus", c.id)), Sense::click());
            if hover || pr.hovered() {
                ui.painter().text(plus.center(), Align2::CENTER_CENTER, ph::PLUS, egui::FontId::proportional(15.0), if pr.hovered() { t::p().text } else { t::p().text_muted });
            }
            if pr.on_hover_text("Создать канал").clicked() {
                self.open(Dialog::CreateChannel { server, category: Some(c.id), name: String::new(), kind: ChannelKind::Voice, limit: String::new() });
            }
            resp.context_menu(|ui| {
                if kit::menu_item(ui, egui_phosphor::fill::PLUS_CIRCLE, "Создать канал", false).clicked() {
                    self.open(Dialog::CreateChannel { server, category: Some(c.id), name: String::new(), kind: ChannelKind::Voice, limit: String::new() });
                }
                if kit::menu_item(ui, egui_phosphor::fill::PENCIL_SIMPLE, "Переименовать", false).clicked() {
                    self.open(Dialog::RenameCategory { id: c.id, name: c.name.clone() });
                }
                if kit::menu_item(ui, egui_phosphor::fill::TRASH, "Удалить категорию", true).clicked() {
                    self.open(Dialog::DeleteCategory { id: c.id, name: c.name.clone() });
                }
            });
        }
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    }

    fn channel_row(&mut self, ui: &mut Ui, ch: &Channel, cx: &ListCtx, manage: bool, pick: &mut Option<Pick>, drop: &mut Option<(i64, DropTarget)>) {
        let voice = ch.kind == ChannelKind::Voice;
        let here = cx.my_voice.is_some_and(|(id, _)| id == ch.id);
        let roster = cx.model.in_voice(ch.id);
        let count = if here { cx.my_voice.map(|(_, p)| p.len()).unwrap_or(0) } else { roster.len() };
        let full = voice && !here && ch.user_limit.is_some_and(|l| count as u32 >= l);
        let trailing = match (voice, ch.user_limit) {
            (true, Some(l)) => Some(format!("{count}/{l}")),
            (true, None) if count > 0 => Some(count.to_string()),
            _ => None,
        };
        let (icon, active) = if voice { (ph::SPEAKER_HIGH, here) } else { (ph::HASH, cx.open_text == Some(ch.id)) };
        let unread = if voice { 0 } else { cx.unread.count(ch.id) };
        let mut resp = kit::row_ex(ui, icon, &ch.name, active, trailing.as_deref(), !full, unread);
        // выбранный канал — полоска цвета сервера, непрочитанный — короткая белая
        let left = resp.rect.left() - 6.0;
        if active {
            let c = cx.model.server(ch.server_id).and_then(crate::banner::server_color).unwrap_or(t::p().accent);
            let r = egui::Rect::from_center_size(egui::pos2(left, resp.rect.center().y), vec2(4.0, 24.0));
            ui.painter().rect_filled(r, CornerRadius::same(2), c);
        } else if unread > 0 {
            let r = egui::Rect::from_center_size(egui::pos2(left, resp.rect.center().y), vec2(4.0, 10.0));
            ui.painter().rect_filled(r, CornerRadius::same(2), t::p().text);
        }
        if full {
            resp = resp.on_hover_text("Канал заполнен");
        }
        if manage {
            resp = resp.interact(Sense::click_and_drag());
            resp.dnd_set_drag_payload(ch.id);
            if let Some(p) = resp.dnd_hover_payload::<i64>()
                && *p != ch.id
            {
                insert_line(ui, resp.rect.top() - 1.0, resp.rect.x_range());
            }
            if let Some(id) = resp.dnd_release_payload::<i64>() {
                *drop = Some((*id, DropTarget::Before(ch.id)));
            }
            resp.context_menu(|ui| {
                if kit::menu_item(ui, egui_phosphor::fill::PENCIL_SIMPLE, "Переименовать", false).clicked() {
                    self.open(Dialog::RenameChannel { id: ch.id, name: ch.name.clone() });
                }
                if voice && kit::menu_item(ui, egui_phosphor::fill::USERS, "Лимит участников", false).clicked() {
                    self.open(Dialog::Limit { id: ch.id, limit: ch.user_limit.map(|l| l.to_string()).unwrap_or_default() });
                }
                if kit::menu_item(ui, egui_phosphor::fill::TRASH, "Удалить канал", true).clicked() {
                    self.open(Dialog::DeleteChannel { id: ch.id, name: ch.name.clone() });
                }
            });
        }
        if resp.clicked() && !full {
            *pick = Some(if voice { Pick::Voice(ch.clone()) } else { Pick::Text(ch.id) });
        }
        if !voice {
            return;
        }
        match cx.my_voice {
            Some((id, peers)) if id == ch.id => {
                for p in peers {
                    let r = crate::app::member_row(ui, p, cx.people);
                    let at = r.interact_pointer_pos().unwrap_or(r.rect.center());
                    if r.clicked()
                        && let Ok(uid) = p.identity.parse::<i64>()
                    {
                        *pick = Some(Pick::Profile(uid, at));
                    } else if r.secondary_clicked() && !p.is_local {
                        *pick = Some(Pick::PeerVolume(p.identity.clone(), at));
                    }
                }
            }
            _ => {
                for &uid in roster {
                    let r = roster_row(ui, cx, uid);
                    if r.clicked() || r.secondary_clicked() {
                        *pick = Some(Pick::Profile(uid, r.interact_pointer_pos().unwrap_or(r.rect.center())));
                    }
                }
            }
        }
    }

    fn open(&mut self, d: Dialog) {
        self.error = None;
        self.busy = false;
        self.dialog = Some(d);
    }

    // ------------------------------------------------------------ диалоги

    pub fn dialogs(&mut self, ctx: &egui::Context) {
        let Some(mut d) = self.dialog.take() else { return };
        let mut submit = false;
        let mut cancel = false;
        let (title, danger) = match &d {
            Dialog::CreateChannel { .. } => ("Новый канал", false),
            Dialog::CreateCategory { .. } => ("Новая категория", false),
            Dialog::RenameChannel { .. } => ("Переименовать канал", false),
            Dialog::RenameCategory { .. } => ("Переименовать категорию", false),
            Dialog::Limit { .. } => ("Лимит участников", false),
            Dialog::DeleteChannel { .. } => ("Удалить канал", true),
            Dialog::DeleteCategory { .. } => ("Удалить категорию", true),
        };
        let busy = self.busy;
        let error = self.error.clone();
        let res = kit::modal(ctx, "channel-dialog", title, 380.0, |ui| {
            let w = ui.available_width();
            let field = |ui: &mut Ui, label: &str, text: &mut String, hint: &str, max: usize| -> bool {
                ui.label(RichText::new(label).font(t::semibold(13.0)).color(t::p().text_muted));
                let r = ui.add(kit::text_field(text, w).hint_text(hint).char_limit(max));
                r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
            };
            match &mut d {
                Dialog::CreateChannel { name, kind, limit, .. } => {
                    ui.horizontal(|ui| {
                        for (k, label, icon) in [(ChannelKind::Voice, "Голосовой", ph::SPEAKER_HIGH), (ChannelKind::Text, "Текстовый", ph::HASH)] {
                            let b = if *kind == k { ButtonKind::Primary } else { ButtonKind::Secondary };
                            if kit::button(ui, b, label, Some(icon)).clicked() {
                                *kind = k;
                            }
                        }
                    });
                    ui.add_space(12.0);
                    submit |= field(ui, "Название", name, "например, Игры", limits::CHANNEL_NAME_MAX);
                    if *kind == ChannelKind::Voice {
                        ui.add_space(10.0);
                        ui.label(RichText::new("Лимит участников (пусто — без лимита)").font(t::semibold(13.0)).color(t::p().text_muted));
                        ui.add(kit::text_field(limit, 80.0).hint_text("∞").char_limit(2));
                    }
                }
                Dialog::CreateCategory { name, .. } => submit |= field(ui, "Название", name, "например, Игры", limits::CATEGORY_NAME_MAX),
                Dialog::RenameChannel { name, .. } => submit |= field(ui, "Название", name, "", limits::CHANNEL_NAME_MAX),
                Dialog::RenameCategory { name, .. } => submit |= field(ui, "Название", name, "", limits::CATEGORY_NAME_MAX),
                Dialog::Limit { limit, .. } => {
                    submit |= field(ui, &format!("Сколько человек (1–{}; пусто — без лимита)", limits::USER_LIMIT_MAX), limit, "∞", 2);
                }
                Dialog::DeleteChannel { name, .. } => {
                    ui.label(RichText::new(format!("Удалить «{name}»? Сообщения и вложения канала удалятся насовсем.")).size(14.0));
                }
                Dialog::DeleteCategory { name, .. } => {
                    ui.label(RichText::new(format!("Удалить категорию «{name}»? Каналы останутся — без категории.")).size(14.0));
                }
            }
            if let Some(e) = &error {
                ui.add_space(8.0);
                ui.label(RichText::new(format!("{}  {e}", ph::WARNING_CIRCLE)).size(13.0).color(t::p().danger_text));
            }
            ui.add_space(18.0);
            ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                let (text, kind) = if danger { ("Удалить", ButtonKind::Danger) } else { ("Сохранить", ButtonKind::Primary) };
                let text = if busy { "Подождите…" } else { text };
                submit |= kit::button_ex(ui, kind, text, None, !busy).clicked();
                cancel = kit::button(ui, ButtonKind::Ghost, "Отмена", None).clicked();
            });
        });
        if submit && !self.busy {
            match self.submit(&d) {
                Ok(()) => self.busy = true,
                Err(e) => self.error = Some(e),
            }
        }
        if res == kit::ModalResult::Open && !cancel {
            self.dialog = Some(d);
        }
    }

    fn submit(&self, d: &Dialog) -> Result<(), String> {
        let api = self.api.clone();
        let name_of = |s: &str| -> Result<String, String> {
            let s = s.trim();
            if s.is_empty() { Err("Введите название.".into()) } else { Ok(s.to_string()) }
        };
        let limit_of = |s: &str| -> Result<Option<u32>, String> {
            let s = s.trim();
            if s.is_empty() {
                return Ok(None);
            }
            match s.parse::<u32>() {
                Ok(n) if (1..=limits::USER_LIMIT_MAX).contains(&n) => Ok(Some(n)),
                _ => Err(format!("Лимит — число от 1 до {}.", limits::USER_LIMIT_MAX)),
            }
        };
        let done = |r: Result<(), crate::api::ApiError>| Ev::Done(r.map_err(|e| e.message));
        match d {
            Dialog::CreateChannel { server, category, name, kind, limit } => {
                let req = CreateChannelRequest {
                    name: name_of(name)?,
                    kind: *kind,
                    category_id: *category,
                    user_limit: if *kind == ChannelKind::Voice { limit_of(limit)? } else { None },
                };
                let server = *server;
                self.spawn(async move { done(api.create_channel(server, &req).await.map(|_| ())) });
            }
            Dialog::CreateCategory { server, name } => {
                let (server, name) = (*server, name_of(name)?);
                self.spawn(async move { done(api.create_category(server, &name).await.map(|_| ())) });
            }
            Dialog::RenameChannel { id, name } => {
                let (id, req) = (*id, UpdateChannelRequest { name: Some(name_of(name)?), user_limit: None });
                self.spawn(async move { done(api.update_channel(id, &req).await.map(|_| ())) });
            }
            Dialog::RenameCategory { id, name } => {
                let (id, name) = (*id, name_of(name)?);
                self.spawn(async move { done(api.rename_category(id, &name).await.map(|_| ())) });
            }
            Dialog::Limit { id, limit } => {
                let (id, req) = (*id, UpdateChannelRequest { name: None, user_limit: Some(limit_of(limit)?) });
                self.spawn(async move { done(api.update_channel(id, &req).await.map(|_| ())) });
            }
            Dialog::DeleteChannel { id, .. } => {
                let id = *id;
                self.spawn(async move { done(api.delete_channel(id).await) });
            }
            Dialog::DeleteCategory { id, .. } => {
                let id = *id;
                self.spawn(async move { done(api.delete_category(id).await) });
            }
        }
        Ok(())
    }
}

/// Человек в голосовом канале (я там не сижу): аватар 20 px и имя.
/// ЛКМ/ПКМ — карточка профиля (громкость — только у тех, кто в моём звонке).
fn roster_row(ui: &mut Ui, cx: &ListCtx, uid: i64) -> egui::Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 28.0), Sense::click());
    let c = pos2(rect.left() + 40.0 + 10.0, rect.center().y);
    let u = cx.people.user(uid).or_else(|| cx.model.user(uid)).cloned().unwrap_or_else(|| proto::PublicUser {
        id: uid,
        name: format!("#{uid}"),
        display_name: None,
        avatar: None,
    });
    cx.people.paint(ui.painter(), c, 10.0, &u, 0.0, false);
    ui.painter().text(pos2(c.x + 18.0, c.y), Align2::LEFT_CENTER, u.shown_name(), t::semibold(13.5), t::p().text_muted);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Линия «вставить сюда» при перетаскивании.
fn insert_line(ui: &Ui, y: f32, x: egui::Rangef) {
    ui.painter().hline(x.shrink(6.0), y, Stroke::new(2.0, t::p().accent));
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::ChannelKind;

    fn ch(id: i64, cat: Option<i64>) -> Channel {
        Channel { id, name: format!("c{id}"), kind: ChannelKind::Text, server_id: 1, category_id: cat, position: 0, user_limit: None }
    }

    fn cat(id: i64) -> Option<Category> {
        Some(Category { id, name: format!("k{id}"), position: 0 })
    }

    /// Без категории: 1, 2; категория 10: 3, 4; категория 20: пусто.
    fn sorted() -> Vec<(Option<Category>, Vec<Channel>)> {
        vec![(None, vec![ch(1, None), ch(2, None)]), (cat(10), vec![ch(3, Some(10)), ch(4, Some(10))]), (cat(20), vec![])]
    }

    fn places(req: &LayoutRequest) -> Vec<(i64, Option<i64>)> {
        req.channels.iter().map(|p| (p.id, p.category_id)).collect()
    }

    #[test]
    fn drop_into_other_category_end() {
        let req = apply_drop(&sorted(), 1, DropTarget::EndOf(Some(20)));
        assert_eq!(req.categories, vec![10, 20]);
        assert_eq!(places(&req), vec![(2, None), (3, Some(10)), (4, Some(10)), (1, Some(20))]);
    }

    #[test]
    fn drop_at_start() {
        let req = apply_drop(&sorted(), 4, DropTarget::Before(1));
        assert_eq!(places(&req), vec![(4, None), (1, None), (2, None), (3, Some(10))]);
    }

    #[test]
    fn drop_at_end_of_loose() {
        let req = apply_drop(&sorted(), 3, DropTarget::EndOf(None));
        assert_eq!(places(&req), vec![(1, None), (2, None), (3, None), (4, Some(10))]);
    }

    #[test]
    fn drop_before_inside_same_category() {
        let req = apply_drop(&sorted(), 4, DropTarget::Before(3));
        assert_eq!(places(&req), vec![(1, None), (2, None), (4, Some(10)), (3, Some(10))]);
    }

    #[test]
    fn drop_on_itself_keeps_order() {
        let req = apply_drop(&sorted(), 3, DropTarget::Before(3));
        assert_eq!(places(&req), vec![(1, None), (2, None), (3, Some(10)), (4, Some(10))]);
    }

    #[test]
    fn layout_after_sets_positions_and_categories() {
        let old = ChannelLayout {
            categories: vec![Category { id: 10, name: "k".into(), position: 0 }, Category { id: 20, name: "m".into(), position: 1 }],
            channels: vec![ch(1, None), ch(3, Some(10))],
        };
        let req = LayoutRequest {
            categories: vec![20, 10],
            channels: vec![ChannelPlace { id: 3, category_id: None }, ChannelPlace { id: 1, category_id: Some(20) }],
        };
        let l = layout_after(&old, &req);
        let c3 = l.channels.iter().find(|c| c.id == 3).unwrap();
        let c1 = l.channels.iter().find(|c| c.id == 1).unwrap();
        assert_eq!((c3.category_id, c1.category_id), (None, Some(20)));
        assert!(c3.position < c1.position);
        let k20 = l.categories.iter().find(|c| c.id == 20).unwrap();
        let k10 = l.categories.iter().find(|c| c.id == 10).unwrap();
        assert!(k20.position < k10.position);
    }
}
