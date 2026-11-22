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
