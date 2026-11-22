//! Обрезка картинки в квадрат (аватар, значок сервера): масштаб колесом, перетаскивание, круглая маска (фаза 9.1).

use eframe::egui::{self, Color32, Rect, RichText, Sense, Stroke, TextureHandle, TextureOptions, Vec2, vec2};
use image::{DynamicImage, RgbaImage};

use crate::kit::{self, ButtonKind};
use crate::theme as t;

/// Сторона окна обрезки (точки).
const VIEW: f32 = 280.0;
/// Сторона результата (px).
pub const OUT: u32 = 256;
const ZOOM_MAX: f32 = 8.0;
/// Картинки больше — уменьшаются при открытии (текстура и память).
const SRC_MAX: u32 = 2048;

/// Положение картинки в окне обрезки: масштаб (1 — короткая сторона во всё окно) и сдвиг от центра (точки окна).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CropState {
    pub zoom: f32,
    pub offset: Vec2,
}

impl Default for CropState {
    fn default() -> Self {
        Self { zoom: 1.0, offset: Vec2::ZERO }
    }
}

/// Квадрат `out`×`out` — то, что видно в окне обрезки стороной `view`.
pub fn crop_square(img: &DynamicImage, st: &CropState, view: f32, out: u32) -> RgbaImage {
    let (x, y, w, h) = crop_rect(img.width() as f32, img.height() as f32, st, vec2(view, view));
    let side = w.min(h);
    img.crop_imm(x, y, side, side).resize_exact(out, out, image::imageops::FilterType::Lanczos3).to_rgba8()
}

/// Форма обрезки: круг 1:1 → 256×256 (аватар) или прямоугольник 5:2 → 960×384 (баннер).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Circle,
    Banner,
}

impl Shape {
    /// Окно обрезки (точки).
    pub fn view(self) -> Vec2 {
        match self {
            Shape::Circle => vec2(VIEW, VIEW),
            Shape::Banner => vec2(440.0, 176.0),
        }
    }
    /// Итог (px) и предел размера файла.
    pub fn out(self) -> (u32, u32, usize) {
        match self {
            Shape::Circle => (proto::limits::AVATAR_SIDE, proto::limits::AVATAR_SIDE, proto::limits::AVATAR_MAX_BYTES),
            Shape::Banner => (proto::limits::USER_BANNER_W, proto::limits::USER_BANNER_H, proto::limits::USER_BANNER_MAX_BYTES),
        }
    }
}

/// Исходник для окна обрезки профиля: байты файла (GIF перечитывается при сборке — кадры не держатся в памяти),
/// первый кадр (уменьшенный до SRC_MAX — для показа) и число кадров (1 — статичная).
pub struct Source {
    pub bytes: std::sync::Arc<Vec<u8>>,
    pub first: DynamicImage,
    pub frames: usize,
}

/// Что собрать после «Готово» (тяжело — в фоне).
#[derive(Clone)]
pub struct RenderJob {
    pub src: std::sync::Arc<Source>,
    pub st: CropState,
    pub shape: Shape,
}

pub fn load_source(bytes: Vec<u8>) -> Result<Source, String> {
    use proto::limits::{GIF_FRAMES_MAX, PROFILE_SOURCE_MAX_BYTES};
    if bytes.len() > PROFILE_SOURCE_MAX_BYTES {
        return Err(format!("Файл больше {} МБ.", PROFILE_SOURCE_MAX_BYTES / 1024 / 1024));
    }
    if image::guess_format(&bytes).ok() == Some(image::ImageFormat::Gif) {
        let bad = |_| "GIF повреждён.".to_string();
        // кадры считаем без распаковки
        let mut o = gif::DecodeOptions::new();
        o.skip_frame_decoding(true);
        let mut d = o.read_info(std::io::Cursor::new(&bytes[..])).map_err(bad)?;
        let mut frames = 0;
        while d.read_next_frame().map_err(bad)?.is_some() {
            frames += 1;
            if frames > GIF_FRAMES_MAX {
                return Err(format!("В GIF больше {GIF_FRAMES_MAX} кадров — возьмите покороче."));
            }
        }
        if frames == 0 || d.width() == 0 || d.height() == 0 {
            return Err("GIF пустой.".into());
        }
        let first = gif_frames(&bytes)?.next().ok_or("GIF пустой.")?.map_err(|_| "GIF повреждён.".to_string())?;
        let first = shrink(DynamicImage::ImageRgba8(first.into_buffer()));
        return Ok(Source { bytes: std::sync::Arc::new(bytes), first, frames });
    }
    let first = load(&bytes)?;
    Ok(Source { bytes: std::sync::Arc::new(bytes), first, frames: 1 })
}

fn gif_frames(bytes: &[u8]) -> Result<image::Frames<'_>, String> {
    use image::AnimationDecoder;
    let dec = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes)).map_err(|_| "GIF повреждён.".to_string())?;
    Ok(dec.into_frames())
}

fn shrink(img: DynamicImage) -> DynamicImage {
    if img.width().max(img.height()) > SRC_MAX { img.resize(SRC_MAX, SRC_MAX, image::imageops::FilterType::Triangle) } else { img }
}

/// Масштаб «картинка закрывает окно» × zoom.
fn cover(w: f32, h: f32, view: Vec2, zoom: f32) -> f32 {
    (view.x / w).max(view.y / h) * zoom
}

/// Масштаб 1..8, сдвиг — не дальше, чем картинка закрывает окно \`view\`.
pub fn clamp_view(st: CropState, w: f32, h: f32, view: Vec2) -> CropState {
    let zoom = st.zoom.clamp(1.0, ZOOM_MAX);
    let s = cover(w, h, view, zoom);
    let mx = ((w * s - view.x) / 2.0).max(0.0);
    let my = ((h * s - view.y) / 2.0).max(0.0);
    CropState { zoom, offset: vec2(st.offset.x.clamp(-mx, mx), st.offset.y.clamp(-my, my)) }
}

/// Прямоугольник обрезки в пикселях оригинала (x, y, w, h): то, что видно в окне \`view\`.
pub fn crop_rect(w: f32, h: f32, st: &CropState, view: Vec2) -> (u32, u32, u32, u32) {
    let st = clamp_view(*st, w, h, view);
    let s = cover(w, h, view, st.zoom);
    let left = (view.x - w * s) / 2.0 + st.offset.x;
    let top = (view.y - h * s) / 2.0 + st.offset.y;
    let (cw, ch) = ((view.x / s).min(w), (view.y / s).min(h));
    let x0 = (-left / s).clamp(0.0, w - cw);
    let y0 = (-top / s).clamp(0.0, h - ch);
    (x0.round() as u32, y0.round() as u32, (cw.round() as u32).max(1), (ch.round() as u32).max(1))
}

fn cut(img: &DynamicImage, st: &CropState, shape: Shape) -> RgbaImage {
    let (ow, oh, _) = shape.out();
    let (x, y, w, h) = crop_rect(img.width() as f32, img.height() as f32, st, shape.view());
    img.crop_imm(x, y, w, h).resize_exact(ow, oh, image::imageops::FilterType::CatmullRom).to_rgba8()
}

/// Собрать итог: каждый \`step\`-й кадр (задержки складываются). PNG для статичной, GIF для анимации.
pub fn render_step(job: &RenderJob, step: usize) -> Result<Vec<u8>, String> {
    let src = &job.src;
    if src.frames <= 1 {
        let mut png = std::io::Cursor::new(Vec::new());
        cut(&src.first, &job.st, job.shape).write_to(&mut png, image::ImageFormat::Png).map_err(|e| e.to_string())?;
        return Ok(png.into_inner());
    }
    let step = step.max(1);
    let mut out = Vec::new();
    {
        let mut enc = image::codecs::gif::GifEncoder::new_with_speed(&mut out, 10);
        enc.set_repeat(image::codecs::gif::Repeat::Infinite).map_err(|e| e.to_string())?;
        let mut pending: Option<(RgbaImage, u32)> = None;
        let flush = |p: Option<(RgbaImage, u32)>, enc: &mut image::codecs::gif::GifEncoder<&mut Vec<u8>>| -> Result<(), String> {
            if let Some((img, ms)) = p {
                let frame = image::Frame::from_parts(img, 0, 0, image::Delay::from_numer_denom_ms(ms, 1));
                enc.encode_frame(frame).map_err(|e| e.to_string())?;
            }
            Ok(())
        };
        for (i, f) in gif_frames(&src.bytes)?.enumerate() {
            let f = f.map_err(|_| "GIF повреждён.".to_string())?;
            let (n, d) = f.delay().numer_denom_ms();
            let ms = n.checked_div(d).unwrap_or(0);
            if i % step == 0 {
                flush(pending.take(), &mut enc)?;
                pending = Some((cut(&DynamicImage::ImageRgba8(f.into_buffer()), &job.st, job.shape), ms));
            } else if let Some(p) = pending.as_mut() {
                p.1 += ms;
            }
        }
        flush(pending.take(), &mut enc)?;
    }
    Ok(out)
}

/// Собрать итог в пределах лимита: шаг 1, если тяжело — 2, потом 4; иначе ошибка.
pub fn render(job: &RenderJob) -> Result<Vec<u8>, String> {
    let (_, _, max) = job.shape.out();
    for step in [1, 2, 4] {
        let out = render_step(job, step)?;
        if out.len() <= max {
            return Ok(out);
        }
        if job.src.frames <= 1 {
            break;
        }
    }
    Err(format!("Слишком тяжёлая картинка (больше {} МБ) — возьмите покороче или поменьше.", max / 1024 / 1024))
}
/// Затемнение квадрата `rect` вне вписанного круга: лучи от центра, между кругом и краем квадрата.
fn circle_mask(rect: Rect, color: Color32) -> egui::Shape {
    const N: u32 = 96; // кратно 8 — углы квадрата попадают на лучи
    let c = rect.center();
    let half = rect.width() / 2.0;
    let mut mesh = egui::Mesh::default();
    for i in 0..=N {
        let a = i as f32 / N as f32 * std::f32::consts::TAU;
        let d = vec2(a.cos(), a.sin());
        let edge = half / d.x.abs().max(d.y.abs());
        mesh.colored_vertex(c + d * half, color);
        mesh.colored_vertex(c + d * edge, color);
        if i > 0 {
            let k = i * 2;
            mesh.add_triangle(k - 2, k - 1, k);
            mesh.add_triangle(k - 1, k + 1, k);
        }
    }
    egui::Shape::mesh(mesh)
}

/// Итог окна обрезки.
pub enum CropResult {
    Open,
    Cancel,
    /// PNG `OUT`×`OUT` (значок сервера).
    Done(Vec<u8>),
    /// Профиль: собрать в фоне (`render`) и отправить.
    Render(RenderJob),
}

/// Окно обрезки: картинка в квадрате, видимая часть — круг.
pub struct CropDialog {
    title: String,
    img: DynamicImage,
    tex: TextureHandle,
    st: CropState,
    shape: Shape,
    /// Профиль (аватар/баннер): итог собирается из исходника, в т.ч. все кадры GIF.
    src: Option<std::sync::Arc<Source>>,
}

/// Байты файла → картинка (уменьшенная до `SRC_MAX`). Тяжело — вызывать не в UI-потоке.
pub fn load(bytes: &[u8]) -> Result<DynamicImage, String> {
    let img = image::load_from_memory(bytes).map_err(|_| "Это не картинка (нужны PNG, JPEG, GIF, WebP).".to_string())?;
    Ok(if img.width().max(img.height()) > SRC_MAX { img.resize(SRC_MAX, SRC_MAX, image::imageops::FilterType::Triangle) } else { img })
}

impl CropDialog {
    pub fn new(ctx: &egui::Context, title: &str, img: DynamicImage) -> Self {
        let rgba = img.to_rgba8();
        let ci = egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw());
        let tex = ctx.load_texture("crop-source", ci, TextureOptions::LINEAR);
        Self { title: title.into(), img, tex, st: CropState::default(), shape: Shape::Circle, src: None }
    }

    /// Аватар (круг) или баннер (5:2) профиля; у GIF в окне — первый кадр.
    pub fn new_profile(ctx: &egui::Context, title: &str, src: Source, shape: Shape) -> Self {
        let mut d = Self::new(ctx, title, src.first.clone());
        d.shape = shape;
        d.src = Some(std::sync::Arc::new(src));
        d
    }

    pub fn ui(&mut self, ctx: &egui::Context) -> CropResult {
        let mut result = CropResult::Open;
        let (w, h) = (self.img.width() as f32, self.img.height() as f32);
        let view = self.shape.view();
        let res = kit::modal(ctx, "crop", &self.title, view.x + 40.0, |ui| {
            ui.vertical_centered(|ui| {
                let (rect, resp) = ui.allocate_exact_size(view, Sense::drag());
                if resp.dragged() {
                    self.st.offset += resp.drag_delta();
                }
                if resp.hovered() {
                    let scroll = ui.input(|i| i.smooth_scroll_delta.y);
                    if scroll != 0.0 {
                        self.st.zoom *= (scroll * 0.003).exp();
                    }
                }
                self.st = clamp_view(self.st, w, h, view);
                let s = cover(w, h, view, self.st.zoom);
                let size = vec2(w * s, h * s);
                let img_rect = Rect::from_center_size(rect.center() + self.st.offset, size);
                let painter = ui.painter_at(rect);
                painter.rect_filled(rect, 0.0, Color32::BLACK);
                painter.image(self.tex.id(), img_rect, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
                if self.shape == Shape::Circle {
                    // затемнить всё вне круга
                    painter.add(circle_mask(rect, Color32::from_black_alpha(150)));
                    painter.circle_stroke(rect.center(), view.x / 2.0, Stroke::new(1.5, Color32::from_white_alpha(180)));
                } else {
                    painter.rect_stroke(rect, 0.0, Stroke::new(1.5, Color32::from_white_alpha(180)), egui::StrokeKind::Inside);
                }
                resp.on_hover_cursor(egui::CursorIcon::Grab);
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(egui_phosphor::regular::MAGNIFYING_GLASS_MINUS).color(t::p().text_muted));
                    kit::slider(ui, &mut self.st.zoom, 1.0..=ZOOM_MAX, view.x - 60.0);
                    ui.label(RichText::new(egui_phosphor::regular::MAGNIFYING_GLASS_PLUS).color(t::p().text_muted));
                });
                ui.label(RichText::new("Перетащите картинку, колесо — масштаб").size(12.5).color(t::p().text_muted));
            });
            ui.add_space(14.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                if kit::button(ui, ButtonKind::Primary, "Готово", None).clicked() {
                    if let Some(src) = &self.src {
                        result = CropResult::Render(RenderJob { src: src.clone(), st: self.st, shape: self.shape });
                        return;
                    }
                    let out = crop_square(&self.img, &self.st, VIEW, OUT);
                    let mut png = std::io::Cursor::new(Vec::new());
                    result = match out.write_to(&mut png, image::ImageFormat::Png) {
                        Ok(()) => CropResult::Done(png.into_inner()),
                        Err(e) => {
                            log::warn!("обрезка: PNG {e}");
                            CropResult::Cancel
                        }
                    };
                }
                if kit::button(ui, ButtonKind::Ghost, "Отмена", None).clicked() {
                    result = CropResult::Cancel;
                }
            });
        });
        if res == kit::ModalResult::Closed {
            return CropResult::Cancel;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::vec2;

    /// 800×300: левая половина красная, правая синяя.
    fn halves() -> DynamicImage {
        DynamicImage::ImageRgba8(RgbaImage::from_fn(800, 300, |x, _| {
            if x < 400 { image::Rgba([255, 0, 0, 255]) } else { image::Rgba([0, 0, 255, 255]) }
        }))
    }

    fn is_red(p: &image::Rgba<u8>) -> bool {
        p[0] > 200 && p[2] < 50
    }

    #[test]
    fn default_is_center_square() {
        let out = crop_square(&halves(), &CropState::default(), 240.0, 256);
        assert!(is_red(out.get_pixel(10, 128)), "слева от центра — красное");
        assert!(!is_red(out.get_pixel(245, 128)), "справа от центра — синее");
    }

    #[test]
    fn offset_to_edge_is_clamped() {
        let st = CropState { zoom: 1.0, offset: vec2(10_000.0, 0.0) };
        let out = crop_square(&halves(), &st, 240.0, 256);
        // сдвиг вправо до упора = виден левый край (0..300) — весь красный
        assert!(is_red(out.get_pixel(10, 128)));
        assert!(is_red(out.get_pixel(245, 128)), "дальше края не уезжает");
    }

    #[test]
    fn zoom_shows_less() {
        let st = CropState { zoom: 4.0, offset: Vec2::ZERO };
        let out = crop_square(&halves(), &st, 240.0, 256);
        // по центру 800×300 при ×4 видно 75 px вокруг x=400: половина красная, половина синяя
        assert!(is_red(out.get_pixel(20, 128)));
        assert!(!is_red(out.get_pixel(236, 128)));
    }

    #[test]
    fn not_an_image() {
        assert!(load(b"hello").is_err());
    }

    fn gif_bytes(w: u16, h: u16, delays: &[u16]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = gif::Encoder::new(&mut out, w, h, &[255, 0, 0, 0, 0, 255]).unwrap();
            enc.set_repeat(gif::Repeat::Infinite).unwrap();
            for (i, &d) in delays.iter().enumerate() {
                let f = gif::Frame { width: w, height: h, delay: d, buffer: std::borrow::Cow::Owned(vec![(i % 2) as u8; w as usize * h as usize]), ..Default::default() };
                enc.write_frame(&f).unwrap();
            }
        }
        out
    }

    fn delays_of(b: &[u8]) -> Vec<u16> {
        let mut d = gif::DecodeOptions::new().read_info(std::io::Cursor::new(b)).unwrap();
        let mut v = Vec::new();
        while let Some(f) = d.read_next_frame().unwrap() {
            v.push(f.delay);
        }
        v
    }

    fn job(bytes: Vec<u8>, shape: Shape) -> RenderJob {
        RenderJob { src: std::sync::Arc::new(load_source(bytes).unwrap()), st: CropState::default(), shape }
    }

    #[test]
    fn gif_three_frames_to_avatar() {
        let out = render(&job(gif_bytes(400, 300, &[5, 10, 15]), Shape::Circle)).unwrap();
        assert!(out.starts_with(b"GIF89a"));
        assert_eq!(imagesize::blob_size(&out).map(|d| (d.width, d.height)).unwrap(), (256, 256));
        assert_eq!(delays_of(&out), vec![5, 10, 15]);
    }

    #[test]
    fn static_to_banner_png() {
        let mut png = std::io::Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(RgbaImage::new(2000, 1000)).write_to(&mut png, image::ImageFormat::Png).unwrap();
        let out = render(&job(png.into_inner(), Shape::Banner)).unwrap();
        assert!(out.starts_with(b"\x89PNG"));
        assert_eq!(imagesize::blob_size(&out).map(|d| (d.width, d.height)).unwrap(), (960, 384));
    }

    #[test]
    fn banner_rect_is_5_to_2() {
        let (_, _, w, h) = crop_rect(2000.0, 1000.0, &CropState::default(), Shape::Banner.view());
        assert!((w as f32 / h as f32 - 2.5).abs() < 0.02, "{w}×{h}");
        assert_eq!(w, 2000, "при масштабе 1 баннер во всю ширину");
    }

    #[test]
    fn step_two_halves_and_doubles_delay() {
        let j = job(gif_bytes(300, 300, &[5, 5, 5, 5]), Shape::Circle);
        assert_eq!(delays_of(&render_step(&j, 2).unwrap()), vec![10, 10]);
    }

    #[test]
    fn source_too_many_frames() {
        let e = load_source(gif_bytes(8, 8, &vec![2; proto::limits::GIF_FRAMES_MAX + 1])).err().unwrap();
        assert!(e.contains("кадр"), "{e}");
    }

    #[test]
    fn source_too_big() {
        let e = load_source(vec![0; proto::limits::PROFILE_SOURCE_MAX_BYTES + 1]).err().unwrap();
        assert!(e.contains("20 МБ"), "{e}");
    }

    #[test]
    fn output_is_256() {
        let out = crop_square(&halves(), &CropState::default(), 240.0, 256);
        assert_eq!(out.dimensions(), (256, 256));
    }
}
