//! Строки списков, заголовки групп, значки серверов.

use eframe::egui::{self, Align2, Color32, CornerRadius, Response, Sense, Ui, pos2, vec2};

use super::{HOVER_S, R_ROW};
use crate::theme as t;

/// Строка списка (канал, пункт меню): иконка, текст, справа — число/пометка. Высота 38, на всю ширину.
pub fn row(ui: &mut Ui, icon: &str, text: &str, active: bool, trailing: Option<&str>, enabled: bool) -> Response {
    row_ex(ui, icon, text, active, trailing, enabled, 0)
}

/// Бейдж-счётчик (непрочитанные, заявки): круг/пилюля акцентного цвета с обводкой цвета фона.
pub fn count_badge(painter: &egui::Painter, center: egui::Pos2, text: &str, ring: Color32) {
    let p = t::p();
    let g = painter.layout_no_wrap(text.to_string(), t::xbold(10.5), p.on_accent);
    let w = (g.size().x + 8.0).max(16.0);
    let r = egui::Rect::from_center_size(center, vec2(w, 16.0));
    painter.rect_filled(r.expand(1.5), CornerRadius::same(10), ring);
    painter.rect_filled(r, CornerRadius::same(8), p.accent_strong);
    painter.galley(center - g.size() / 2.0, g, p.on_accent);
}

/// Строка списка с непрочитанными: `unread > 0` — имя ярче и жирнее, справа — бейдж с числом.
pub fn row_ex(ui: &mut Ui, icon: &str, text: &str, active: bool, trailing: Option<&str>, enabled: bool, unread: u32) -> Response {
    let unread = if active { 0 } else { unread };
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 38.0), if enabled { Sense::click() } else { Sense::hover() });
    let hover = ui.ctx().animate_bool_with_time(resp.id, enabled && resp.hovered(), HOVER_S);
    let p = t::p();
    let painter = ui.painter();
    let bg = if active { p.row_active } else { Color32::TRANSPARENT.lerp_to_gamma(p.row_hover, hover) };
    painter.rect_filled(rect, CornerRadius::same(R_ROW), bg);
    let fg = if !enabled {
        p.text_faint
    } else if active || unread > 0 {
        p.text
    } else {
        p.text_muted.lerp_to_gamma(p.text, hover)
    };
    let icon_font = if active { t::icon_fill(19.0) } else { t::font(19.0) };
    let icon_c = if active { p.accent } else { fg };
    painter.text(pos2(rect.left() + 22.0, rect.center().y), Align2::CENTER_CENTER, icon, icon_font, icon_c);
    let font = if active || unread > 0 { t::bold(15.0) } else { t::semibold(15.0) };
    painter.text(pos2(rect.left() + 40.0, rect.center().y), Align2::LEFT_CENTER, text, font, fg);
    if unread > 0 {
        let txt = crate::unread::badge_text(unread);
        count_badge(painter, pos2(rect.right() - 20.0, rect.center().y), &txt, bg);
    } else if let Some(tr) = trailing {
        painter.text(pos2(rect.right() - 12.0, rect.center().y), Align2::RIGHT_CENTER, tr, t::semibold(12.5), p.text_muted);
    }
    if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

/// Заголовок группы в списке («Голосовые каналы»).
pub fn group_label(ui: &mut Ui, text: &str) {
    ui.add_space(6.0);
    ui.label(egui::RichText::new(text).font(t::bold(12.5)).color(t::p().text_muted));
}

/// Что внутри значка сервера.
pub enum ServerMark {
    Letter(String),
    /// Буквы на цвете сервера.
    Colored(String, Color32),
    Image(egui::TextureId),
}

/// Значок сервера 44×44: круг, активный — скруглённый квадрат с акцентом.
pub fn server_icon(ui: &mut Ui, mark: &ServerMark, active: bool, tip: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(44.0, 44.0), Sense::click());
    let k = ui.ctx().animate_bool_with_time(resp.id, active || resp.hovered(), HOVER_S * 1.5);
    let p = t::p();
    let r = egui::lerp(22.0..=14.0, k) as u8;
    let painter = ui.painter();
    match mark {
        ServerMark::Letter(s) => {
            let bg = if active { p.accent_strong } else { p.surface_3.lerp_to_gamma(p.row_active, k) };
            painter.rect_filled(rect, CornerRadius::same(r), bg);
            let fg = if active { p.on_accent } else { p.text };
            painter.text(rect.center(), Align2::CENTER_CENTER, s, t::xbold(17.0), fg);
        }
        ServerMark::Colored(s, c) => {
            painter.rect_filled(rect, CornerRadius::same(r), *c);
            painter.text(rect.center(), Align2::CENTER_CENTER, s, t::xbold(17.0), Color32::WHITE);
        }
        ServerMark::Image(tex) => {
            egui::Image::new(egui::load::SizedTexture::new(*tex, rect.size()))
                .corner_radius(CornerRadius::same(r))
                .paint_at(ui, rect);
        }
    }
    resp.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand)
}
