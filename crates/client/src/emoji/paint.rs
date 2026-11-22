//! Рисование цветных эмодзи (Twemoji): текстуры, строка текста с эмодзи, подпись.

use eframe::egui;
use super::text::{Seg, png, segments};
use egui::{Align2, Color32, Context, FontId, Id, Painter, Pos2, Rect, Response, RichText, Sense, TextureHandle, TextureOptions, Ui, pos2, vec2};

/// Текстура эмодзи; PNG декодируется один раз и живёт в памяти egui.
pub fn texture(ctx: &Context, emoji: &str) -> Option<TextureHandle> {
    let key = Id::new(("emoji-tex", emoji));
    if let Some(t) = ctx.data(|d| d.get_temp::<Option<TextureHandle>>(key)) {
        return t;
    }
    let tex = png(emoji).and_then(|b| image::load_from_memory_with_format(b, image::ImageFormat::Png).ok()).map(|img| {
        let rgba = img.to_rgba8();
        let ci = egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], &rgba);
        ctx.load_texture(format!("emoji-{emoji}"), ci, TextureOptions::LINEAR)
    });
    ctx.data_mut(|d| d.insert_temp(key, tex.clone()));
    tex
}

/// Нарисовать эмодзи в прямоугольнике; false — картинки нет.
pub fn paint(painter: &Painter, rect: Rect, emoji: &str) -> bool {
    let Some(t) = texture(painter.ctx(), emoji) else { return false };
    painter.image(t.id(), rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
    true
}

/// Разметка для поля ввода: эмодзи прозрачные (их место занимает глиф шрифта), сверху рисует `overlay`.
/// `measure` — ширина текста этим шрифтом: место под эмодзи добирается отступом слева до `EDIT_EMOJI_W`.
pub fn edit_job(text: &str, font: FontId, color: Color32, wrap: f32, mut measure: impl FnMut(&str) -> f32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    job.wrap.max_width = wrap;
    let fmt = |c: Color32| egui::TextFormat { font_id: font.clone(), color: c, ..Default::default() };
    for seg in segments(text) {
        match seg {
            Seg::Text(t) => job.append(t, 0.0, fmt(color)),
            Seg::Emoji(e) => {
                // добавка слева (leading_space): правый край места = позиция курсора после эмодзи
                let pad = (EDIT_EMOJI_W - measure(e)).max(0.0);
                job.append(e, pad, fmt(Color32::TRANSPARENT));
            }
        }
    }
    job
}

/// Ширина места под эмодзи в поле ввода (картинка `EDIT_EMOJI_W - 2`).
pub const EDIT_EMOJI_W: f32 = 21.0;

/// Цветные эмодзи поверх галеи поля ввода (разметка из `edit_job`).
pub fn overlay(painter: &Painter, galley: &egui::Galley, galley_pos: Pos2, font: &FontId, size: f32) {
    let text = galley.text();
    let mut chars = 0usize;
    let mut last = 0usize;
    for seg in segments(text) {
        let s = match seg {
            Seg::Text(t) | Seg::Emoji(t) => t,
        };
        // сегменты идут подряд — считаем символы без повторного прохода
        let start_byte = s.as_ptr() as usize - text.as_ptr() as usize;
        chars += text[last..start_byte].chars().count();
        last = start_byte;
        let c0 = chars;
        let n = s.chars().count();
        chars += n;
        last += s.len();
        let Seg::Emoji(e) = seg else { continue };
        // курсор перед эмодзи стоит после добавки слева (см. edit_job): место = [x - добавка, x + ширина глифа]
        let r0 = galley.pos_from_cursor(egui::text::CCursor::new(c0));
        let w = painter.layout_no_wrap(e.to_string(), font.clone(), Color32::WHITE).size().x;
        let pad = (EDIT_EMOJI_W - w).max(0.0);
        let span = Rect::from_x_y_ranges((r0.left() - pad)..=(r0.left() + w), r0.y_range()).translate(galley_pos.to_vec2());
        let img = Rect::from_center_size(span.center(), vec2(size, size));
        if !paint(painter, img, e) {
            painter.text(span.center(), Align2::CENTER_CENTER, e, FontId::proportional(size * 0.8), Color32::WHITE);
        }
    }
}

/// Только эмодзи (1–3) и пробелы — показываем крупно, как в Discord.
#[cfg(test)]
pub fn jumbo(text: &str) -> bool {
    jumbo_segs(&segments(text))
}

fn jumbo_segs(segs: &[Seg<'_>]) -> bool {
    let n = segs.iter().filter(|s| matches!(s, Seg::Emoji(_))).count();
    (1..=3).contains(&n) && segs.iter().all(|s| match s { Seg::Text(t) => t.trim().is_empty(), Seg::Emoji(_) => true })
}

fn hint(e: &str) -> String {
    super::entry(e).map(|x| format!(":{}:", x.codes.last().cloned().unwrap_or_default())).unwrap_or_default()
}

/// Текст с цветными эмодзи, с переносом. Без эмодзи — обычная подпись.
pub fn label(ui: &mut Ui, text: &str, font: FontId, color: Color32, selectable: bool) -> Response {
    let segs = segments(text);
    if !segs.iter().any(|s| matches!(s, Seg::Emoji(_))) {
        return ui.add(egui::Label::new(RichText::new(text).font(font).color(color)).wrap().selectable(selectable));
    }
    let size = if jumbo_segs(&segs) { (font.size * 2.2).round() } else { (font.size * 1.25).round() };
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for seg in segs {
            match seg {
                Seg::Text(t) => {
                    for (i, line) in t.split('\n').enumerate() {
                        if i > 0 {
                            ui.end_row();
                        }
                        if !line.is_empty() {
                            ui.add(egui::Label::new(RichText::new(line).font(font.clone()).color(color)).wrap().selectable(selectable));
                        }
                    }
                }
                Seg::Emoji(e) => {
                    let (r, resp) = ui.allocate_exact_size(vec2(size + 2.0, size), Sense::hover());
                    let img = Rect::from_center_size(r.center(), vec2(size, size));
                    if !paint(ui.painter(), img, e) {
                        ui.painter().text(r.center(), Align2::CENTER_CENTER, e, font.clone(), color);
                    }
                    if resp.hovered() {
                        let h = hint(e);
                        if !h.is_empty() {
                            resp.on_hover_text(h);
                        }
                    }
                }
            }
        }
    })
    .response
}

/// Одна строка через painter (левый край по центру `at`), обрезается многоточием по `max_w`. Возвращает ширину.
pub fn line(painter: &Painter, at: Pos2, text: &str, font: FontId, color: Color32, max_w: f32) -> f32 {
    let size = (font.size * 1.25).round();
    let mut x = at.x;
    for seg in segments(text) {
        let left = at.x + max_w - x;
        if left <= 4.0 {
            break;
        }
        match seg {
            Seg::Text(t) => {
                let g = crate::banner::truncate_galley(painter, t, font.clone(), color, left);
                let w = g.size().x;
                let cut = g.text() != t;
                painter.galley(pos2(x, at.y - g.size().y / 2.0), g, color);
                x += w;
                if cut {
                    break;
                }
            }
            Seg::Emoji(e) => {
                if size + 2.0 > left {
                    break;
                }
                let r = Rect::from_min_size(pos2(x + 1.0, at.y - size / 2.0), vec2(size, size));
                if !paint(painter, r, e) {
                    painter.text(r.center(), Align2::CENTER_CENTER, e, font.clone(), color);
                }
                x += size + 2.0;
            }
        }
    }
    x - at.x
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn jumbo_rules() {
        assert!(jumbo("🔥"));
        assert!(jumbo("🔥 🔥🔥"));
        assert!(!jumbo("🔥🔥🔥🔥"));
        assert!(!jumbo("ок 🔥"));
        assert!(!jumbo(""));
    }

    #[test]
    fn texture_cache_once() {
        let ctx = egui::Context::default();
        let a = texture(&ctx, "🔥").expect("есть картинка");
        let b = texture(&ctx, "🔥").unwrap();
        assert_eq!(a.id(), b.id());
        assert!(texture(&ctx, "x").is_none());
    }
}
