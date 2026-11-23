//! Баннер сервера: готовые узоры («Волны», «Ночь», «Закат» — рисуются кодом) или своя картинка 960×480,
//! снизу — полоса затемнения с иконкой, названием и счётчиками. Плюс цвет сервера.

use eframe::egui::{self, Align2, Color32, CornerRadius, Painter, Pos2, Rect, Shape, Stroke, pos2, vec2};
use proto::{Server, banner};

use crate::people::People;
use crate::theme as t;

/// Высота баннера в шапке боковой панели.
pub const HEIGHT: f32 = 124.0;
/// Высота полосы затемнения.
pub const STRIP: f32 = 46.0;

/// Цвета узора: (фон, линии).
pub fn pattern_colors(name: &str) -> (Color32, Color32) {
    match name {
        banner::NIGHT => (Color32::from_rgb(0x3B, 0x1F, 0x7A), Color32::from_rgb(0xC4, 0xB5, 0xFD)),
        banner::SUNSET => (Color32::from_rgb(0xB4, 0x53, 0x09), Color32::from_rgb(0xFD, 0xE6, 0x8A)),
        _ => (Color32::from_rgb(0x0F, 0x76, 0x6E), Color32::from_rgb(0x5E, 0xEA, 0xD4)),
    }
}

/// Название узора по-русски.
pub fn pattern_label(name: &str) -> &'static str {
    match name {
        banner::NIGHT => "Ночь",
        banner::SUNSET => "Закат",
        _ => "Волны",
    }
}

/// "#RRGGBB" → цвет.
pub fn parse_hex(s: &str) -> Option<Color32> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    Some(Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

/// Цвет сервера (фон иконки, полоска выбранного канала); None — фирменный.
pub fn server_color(s: &Server) -> Option<Color32> {
    s.color.as_deref().and_then(parse_hex)
}

/// Узор: тонкие волны поверх заливки. `seed` сдвигает фазу (у разных серверов разные волны).
pub fn paint_pattern(painter: &Painter, rect: Rect, name: &str, rounding: CornerRadius, seed: f32) {
    let (bg, line) = pattern_colors(name);
    paint_waves(painter, rect, rounding, bg, line, seed);
}

/// Волны своими цветами (карточка профиля без своего баннера — в цвете аватара).
pub fn paint_waves(painter: &Painter, rect: Rect, rounding: CornerRadius, bg: Color32, line: Color32, seed: f32) {
    painter.rect_filled(rect, rounding, bg);
    let clip = painter.with_clip_rect(rect.shrink(0.5));
    let n = 7;
    for i in 0..n {
        let k = i as f32 / (n - 1) as f32;
        let y0 = rect.top() + rect.height() * (0.08 + 0.84 * k);
        let amp = 5.0 + 7.0 * (1.0 - (k - 0.5).abs() * 2.0);
        let freq = std::f32::consts::TAU / (rect.width() * (0.55 + 0.1 * (i % 3) as f32));
        let phase = seed + i as f32 * 0.9;
        let steps = 48;
        let pts: Vec<Pos2> = (0..=steps)
            .map(|j| {
                let x = rect.left() + rect.width() * j as f32 / steps as f32;
                pos2(x, y0 + amp * (x * freq + phase).sin())
            })
            .collect();
        let a = (40.0 + 90.0 * (1.0 - (k - 0.5).abs() * 2.0)) as u8;
        clip.add(Shape::line(pts, Stroke::new(1.3, Color32::from_rgba_unmultiplied(line.r(), line.g(), line.b(), a))));
    }
}

/// Фон баннера сервера: своя картинка (если загружена), иначе узор.
pub fn paint_background(painter: &Painter, rect: Rect, s: &Server, people: &People, rounding: CornerRadius) {
    if let Some(tex) = s.banner_image.as_deref().and_then(|k| people.picture(k)) {
        // cover: картинка 2:1 обрезается по меньшей стороне
        let img = tex.size_vec2();
        let scale = (rect.width() / img.x).max(rect.height() / img.y);
        let shown = vec2(rect.width() / (img.x * scale), rect.height() / (img.y * scale));
        let uv = Rect::from_center_size(pos2(0.5, 0.5), shown);
        painter.add(egui::epaint::RectShape::filled(rect, rounding, Color32::WHITE).with_texture(tex.id(), uv));
        return;
    }
    let seed = (s.id % 7) as f32 * 0.7;
    paint_pattern(painter, rect, s.banner.as_deref().unwrap_or(banner::WAVES), rounding, seed);
}

/// Значок сервера в скруглённом квадрате: картинка или буквы на цвете сервера.
pub fn paint_icon(painter: &Painter, rect: Rect, s: &Server, people: &People) {
    let r = CornerRadius::same((rect.width() * 0.3) as u8);
    match s.icon.as_deref().and_then(|k| people.texture(k)) {
        Some(tex) => {
            painter.add(egui::epaint::RectShape::filled(rect, r, Color32::WHITE).with_texture(tex.id(), Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0))));
        }
        None => {
            painter.rect_filled(rect, r, server_color(s).unwrap_or(t::p().accent_strong));
            painter.text(rect.center(), Align2::CENTER_CENTER, crate::servers_ui::initials(&s.name), t::xbold(rect.height() * 0.42), Color32::WHITE);
        }
    }
}

/// Полоса затемнения снизу баннера: иконка, название, «N участников · M в сети», стрелка меню.
pub fn paint_strip(painter: &Painter, banner: Rect, s: &Server, people: &People, members: usize, online: usize, caret: bool) {
    let strip = Rect::from_min_max(pos2(banner.left(), banner.bottom() - STRIP), banner.max);
    painter.rect_filled(strip, CornerRadius::ZERO, Color32::from_rgba_unmultiplied(10, 14, 16, 140));
    let icon = Rect::from_center_size(pos2(strip.left() + 14.0 + 15.0, strip.center().y), vec2(30.0, 30.0));
    paint_icon(painter, icon, s, people);
    let x = icon.right() + 10.0;
    let max_w = strip.right() - x - if caret { 34.0 } else { 12.0 };
    let name = truncate_galley(painter, &s.name, t::xbold(15.0), Color32::WHITE, max_w);
    painter.galley(pos2(x, strip.center().y - 17.0), name, Color32::WHITE);
    let sub = format!("{} · {} в сети", t::plural(members, "участник", "участника", "участников"), online);
    painter.text(pos2(x, strip.center().y + 3.0), Align2::LEFT_TOP, sub, t::font(12.0), Color32::from_rgba_unmultiplied(255, 255, 255, 200));
    if caret {
        painter.text(pos2(strip.right() - 16.0, strip.center().y), Align2::CENTER_CENTER, egui_phosphor::regular::CARET_DOWN, t::font(16.0), Color32::WHITE);
    }
}

/// Строка в одну линию с многоточием, если не влезает.
pub fn truncate_galley(painter: &Painter, text: &str, font: egui::FontId, color: Color32, max_w: f32) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), font, color);
    job.wrap = egui::text::TextWrapping { max_width: max_w, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    painter.layout_job(job)
}

/// Текст с якорем `anchor` в точке `pos`, обрезанный многоточием до `max_w`. Возвращает занятую ширину.
pub fn paint_truncated(painter: &Painter, pos: egui::Pos2, anchor: egui::Align2, text: &str, font: egui::FontId, color: Color32, max_w: f32) -> f32 {
    let g = truncate_galley(painter, text, font, color, max_w.max(8.0));
    let r = anchor.anchor_size(pos, g.size());
    let w = g.size().x;
    painter.galley(r.min, g, color);
    w
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex() {
        assert_eq!(parse_hex("#14B8A6"), Some(Color32::from_rgb(0x14, 0xB8, 0xA6)));
        assert_eq!(parse_hex("14B8A6"), None);
        assert_eq!(parse_hex("#12"), None);
        for c in proto::SERVER_COLORS {
            assert!(parse_hex(c).is_some(), "{c}");
        }
    }
}
