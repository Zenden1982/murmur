//! Карточка профиля — одна для всех мест: всплывающая (список участников, звонок, друзья, ЛС) и предпросмотр в настройках.
//! Сверху баннер 120 px (свой, GIF играет всегда; иначе — волны в цвете аватара), аватар, имя, логин, статус, «О себе».

use eframe::egui::{self, Color32, CornerRadius, Rect, RichText, Sense, Stroke, Ui, pos2, vec2};
use proto::PublicUser;

use crate::kit;
use crate::people::People;
use crate::theme as t;

pub const CARD_W: f32 = 300.0;
pub const BANNER_H: f32 = 120.0;

/// Что показать в карточке.
pub struct CardData<'a> {
    pub user: &'a PublicUser,
    /// Ключ своего баннера.
    pub banner: Option<&'a str>,
    pub status: Option<&'a str>,
    pub online: bool,
}

/// Баннер и аватар (с точкой «в сети»), под ними — отступ под имя. Ширина — `CARD_W`.
pub fn head(ui: &mut Ui, people: &People, d: &CardData) {
    let p = t::p();
    let (b, _) = ui.allocate_exact_size(vec2(CARD_W, BANNER_H), Sense::hover());
    let round = CornerRadius { nw: kit::R_CARD, ne: kit::R_CARD, sw: 0, se: 0 };
    paint_banner(ui.painter(), b, round, people, d.user, d.banner, true);
    let av = pos2(b.left() + 50.0, b.bottom() + 4.0);
    ui.painter().circle_filled(av, 38.0, p.surface);
    people.paint_playing(ui.painter(), av, 34.0, d.user, 0.0, false);
    crate::friends::status_dot(ui.painter(), av + vec2(25.0, 25.0), d.online, p.surface);
    ui.add_space(40.0);
}

/// Свой баннер (cover), а пока не загружен или его нет — волны в цвете аватара.
/// play — анимировать GIF (миниатюра в настройках — нет).
pub fn paint_banner(painter: &egui::Painter, rect: Rect, round: CornerRadius, people: &People, user: &PublicUser, key: Option<&str>, play: bool) {
    if let Some(tex) = key.and_then(|k| people.banner(k, play, rect.height() * painter.ctx().pixels_per_point())) {
        let img = tex.size_vec2();
        let scale = (rect.width() / img.x).max(rect.height() / img.y);
        let shown = vec2(rect.width() / (img.x * scale), rect.height() / (img.y * scale));
        let uv = Rect::from_center_size(pos2(0.5, 0.5), shown);
        painter.add(egui::epaint::RectShape::filled(rect, round, Color32::WHITE).with_texture(tex.id(), uv));
        return;
    }
    let name = crate::people::avatar_name(user);
    let (bg, line) = t::avatar_colors(name);
    crate::banner::paint_waves(painter, rect, round, bg, line, (user.id % 5) as f32);
}

/// Имя, логин и плашка статуса (внутри полей карточки).
pub fn identity(ui: &mut Ui, d: &CardData) {
    let p = t::p();
    ui.label(RichText::new(d.user.shown_name()).font(t::xbold(18.0)).color(p.text));
    ui.label(RichText::new(&d.user.name).size(13.0).color(p.text_muted));
    if let Some(st) = d.status.filter(|s| !s.trim().is_empty()) {
        ui.add_space(6.0);
        egui::Frame::new().fill(p.surface_3).corner_radius(CornerRadius::same(kit::R_ROW)).inner_margin(egui::Margin::symmetric(10, 6)).show(ui, |ui| {
            ui.set_width(CARD_W - 32.0 - 20.0);
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(8.0, 14.0), Sense::hover());
                ui.painter().circle_filled(r.center(), 3.5, if d.online { p.status_online } else { p.text_muted });
                crate::emoji::paint::label(ui, st, t::font(13.0), p.text, false);
            });
        });
    }
}

/// «О СЕБЕ».
pub fn about(ui: &mut Ui, about: Option<&str>) {
    let p = t::p();
    if let Some(about) = about.filter(|s| !s.trim().is_empty()) {
        ui.add_space(6.0);
        ui.label(RichText::new("О СЕБЕ").font(t::bold(11.5)).color(p.text_muted));
        ui.add(egui::Label::new(RichText::new(about).size(13.0).color(p.text)).wrap());
    }
}

/// Рамка карточки (фон, обводка, скругление).
pub fn frame() -> egui::Frame {
    let p = t::p();
    egui::Frame::new().fill(p.surface).stroke(Stroke::new(1.0, p.border)).corner_radius(CornerRadius::same(kit::R_CARD))
}

/// Поля внутри карточки под баннером.
pub fn body_frame() -> egui::Frame {
    egui::Frame::new().inner_margin(egui::Margin { left: 16, right: 16, top: 0, bottom: 14 })
}

/// Предпросмотр в настройках: та же карточка, без кнопок.
pub fn preview(ui: &mut Ui, people: &People, d: &CardData, about_text: Option<&str>) {
    frame().show(ui, |ui| {
        ui.set_width(CARD_W);
        head(ui, people, d);
        body_frame().show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            identity(ui, d);
            about(ui, about_text);
        });
    });
}
