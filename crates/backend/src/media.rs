//! Картинки профиля (аватар, баннер пользователя): строгая проверка и очистка.
//! Размеры и кадры GIF проверяются по заголовкам ДО распаковки пикселей (защита от «бомб»),
//! затем картинка перекодируется: метаданные (EXIF, tEXt, комментарии GIF, XMP) не переносятся.

use std::io::Cursor;
use std::num::NonZeroU64;

use proto::limits;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Avatar,
    UserBanner,
    /// Иконка сервера: квадрат любой стороны до IMAGE_PROFILE_MAX_SIDE; без GIF.
    ServerIcon,
    /// Баннер сервера: BANNER_W×BANNER_H; без GIF.
    ServerBanner,
}

impl Kind {
    /// (ширина, высота, байт не больше).
    fn spec(self) -> (u32, u32, usize) {
        match self {
            Kind::Avatar => (limits::AVATAR_SIDE, limits::AVATAR_SIDE, limits::AVATAR_MAX_BYTES),
            Kind::UserBanner => (limits::USER_BANNER_W, limits::USER_BANNER_H, limits::USER_BANNER_MAX_BYTES),
            // для иконки w = h = максимальная сторона
            Kind::ServerIcon => (limits::IMAGE_PROFILE_MAX_SIDE, limits::IMAGE_PROFILE_MAX_SIDE, limits::IMAGE_PROFILE_MAX_BYTES),
            Kind::ServerBanner => (limits::BANNER_W, limits::BANNER_H, limits::IMAGE_BANNER_MAX_BYTES),
        }
    }

    fn allows_gif(self) -> bool {
        matches!(self, Kind::Avatar | Kind::UserBanner)
    }

    /// Размер подходит: точный для аватара/баннеров, квадрат до максимума для иконки сервера.
    fn dims_ok(self, (dw, dh): (usize, usize)) -> bool {
        let (w, h, _) = self.spec();
        match self {
            Kind::ServerIcon => dw != 0 && dw == dh && dw <= w as usize,
            _ => (dw, dh) == (w as usize, h as usize),
        }
    }

    fn dims_error(self) -> String {
        let (w, h, _) = self.spec();
        match self {
            Kind::ServerIcon => format!("нужен квадрат до {w}×{w}"),
            _ => format!("нужна картинка {w}×{h}"),
        }
    }
}

/// Память на распаковку одного кадра / статичной картинки.
const DECODE_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Проверить и очистить. Тяжело (декодирование) — звать из `spawn_blocking`. Err — текст для пользователя.
pub fn sanitize(kind: Kind, body: &[u8]) -> Result<Vec<u8>, String> {
    let (w, h, max) = kind.spec();
    if body.len() > max {
        return Err(format!("файл больше {} МБ", max / 1024 / 1024));
    }
    let mime = crate::chat::sniff_image(body).ok_or("поддерживаются PNG, JPEG, WebP и GIF")?;
    let dim = imagesize::blob_size(body).map_err(|_| "картинка повреждена".to_string())?;
    if !kind.dims_ok((dim.width, dim.height)) {
        return Err(kind.dims_error());
    }
    if mime == "image/gif" && !kind.allows_gif() {
        return Err("поддерживаются PNG, JPEG и WebP".into());
    }
    if mime == "image/gif" {
        scan_gif(body, w, h, limits::GIF_FRAMES_MAX as u64 * w as u64 * h as u64)?;
        reencode_gif(body)
    } else {
        reencode_static(body, w, h)
    }
}

fn gif_options(skip: bool) -> gif::DecodeOptions {
    let mut o = gif::DecodeOptions::new();
    o.set_memory_limit(gif::MemoryLimit::Bytes(NonZeroU64::new(DECODE_MAX_BYTES).unwrap()));
    o.skip_frame_decoding(skip);
    o.set_color_output(gif::ColorOutput::Indexed);
    o
}

/// Проход по кадрам без распаковки LZW: число кадров, границы, сумма пикселей.
/// `budget` — максимум суммарных пикселей всех кадров.
fn scan_gif(body: &[u8], w: u32, h: u32, budget: u64) -> Result<(), String> {
    let bad = |_| "GIF повреждён".to_string();
    let mut d = gif_options(true).read_info(Cursor::new(body)).map_err(bad)?;
    if (d.width() as u32, d.height() as u32) != (w, h) {
        return Err("GIF повреждён".into());
    }
    let (mut frames, mut pixels) = (0usize, 0u64);
    while let Some(f) = d.read_next_frame().map_err(bad)? {
        frames += 1;
        if frames > limits::GIF_FRAMES_MAX {
            return Err(format!("в GIF больше {} кадров", limits::GIF_FRAMES_MAX));
        }
        if f.left as u32 + f.width as u32 > w || f.top as u32 + f.height as u32 > h {
            return Err("кадр GIF выходит за края картинки".into());
        }
        pixels += f.width as u64 * f.height as u64;
        if pixels > budget {
            return Err("GIF слишком тяжёлый".into());
        }
    }
    if frames == 0 {
        return Err("GIF пустой".into());
    }
    Ok(())
}

/// Те же кадры (палитры, позиции, задержки, прозрачность) в новый файл; из расширений — только бесконечный повтор.
fn reencode_gif(body: &[u8]) -> Result<Vec<u8>, String> {
    let bad = |_| "GIF повреждён".to_string();
    let mut d = gif_options(false).read_info(Cursor::new(body)).map_err(bad)?;
    let (w, h) = (d.width(), d.height());
    let palette = d.global_palette().map(<[u8]>::to_vec).unwrap_or_default();
    let mut out = Vec::with_capacity(body.len());
    {
        let mut enc = gif::Encoder::new(&mut out, w, h, &palette).map_err(|e| e.to_string())?;
        enc.set_repeat(gif::Repeat::Infinite).map_err(|e| e.to_string())?;
        while let Some(f) = d.read_next_frame().map_err(bad)? {
            enc.write_frame(f).map_err(|e| e.to_string())?;
        }
    }
    Ok(out)
}

/// PNG/JPEG/WebP → PNG (метаданные не переносятся).
fn reencode_static(body: &[u8], w: u32, h: u32) -> Result<Vec<u8>, String> {
    let mut r = image::ImageReader::new(Cursor::new(body)).with_guessed_format().map_err(|_| "картинка повреждена".to_string())?;
    let mut lim = image::Limits::default();
    lim.max_image_width = Some(w);
    lim.max_image_height = Some(h);
    lim.max_alloc = Some(DECODE_MAX_BYTES);
    r.limits(lim);
    let img = r.decode().map_err(|_| "картинка повреждена".to_string())?;
    let mut out = Vec::new();
    img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png).map_err(|e| e.to_string())?;
    Ok(out)
}

/// Картинка чата: бюджет пикселей на кадр (≈25 Мп).
const CHAT_MAX_PIXELS: u64 = 25_000_000;
/// GIF чата: холст × кадры (клиент держит кадры распакованными, RGBA).
const CHAT_GIF_MAX_PIXELS: u64 = 64_000_000;
const JPEG_QUALITY: u8 = 90;

/// Проверить и очистить картинку чата: декод с лимитами, перекодирование без метаданных (EXIF/GPS, tEXt,
/// комментарии GIF, XMP). Возвращает (байты, mime, ширина, высота). Тяжело — звать из `spawn_blocking`.
pub fn sanitize_chat(body: &[u8]) -> Result<(Vec<u8>, &'static str, u32, u32), String> {
    let max = limits::IMAGE_MAX_BYTES;
    if body.len() > max {
        return Err(format!("картинка больше {} МБ", max / 1024 / 1024));
    }
    let mime = crate::chat::sniff_image(body).ok_or("поддерживаются только PNG, JPEG, GIF и WebP")?;
    let dim = imagesize::blob_size(body).map_err(|_| "картинка повреждена".to_string())?;
    let (w, h) = (dim.width as u64, dim.height as u64);
    let side = limits::IMAGE_MAX_SIDE as u64;
    if w == 0 || h == 0 || w > side || h > side {
        return Err(format!("размер картинки — до {side}×{side}"));
    }
    if w * h > CHAT_MAX_PIXELS {
        return Err("картинка слишком большая (больше 25 мегапикселей)".into());
    }
    let (w, h) = (w as u32, h as u32);
    let (out, mime) = if mime == "image/gif" {
        scan_gif(body, w, h, CHAT_GIF_MAX_PIXELS)?;
        if w as u64 * h as u64 * 2 > CHAT_GIF_MAX_PIXELS {
            // хотя бы пара кадров должна помещаться
            return Err("GIF слишком тяжёлый".into());
        }
        (reencode_gif(body)?, "image/gif")
    } else {
        reencode_chat_static(body, mime == "image/jpeg")?
    };
    if out.len() > max {
        return Err(format!("после обработки картинка больше {} МБ", max / 1024 / 1024));
    }
    Ok((out, mime, w, h))
}

/// JPEG → JPEG (качество 90), PNG/WebP → PNG; поворот из EXIF применяется к пикселям, сам EXIF не переносится.
fn reencode_chat_static(body: &[u8], jpeg: bool) -> Result<(Vec<u8>, &'static str), String> {
    use image::ImageDecoder;
    fn bad<E>(_: E) -> String {
        "картинка повреждена".to_string()
    }
    let r = image::ImageReader::new(Cursor::new(body)).with_guessed_format().map_err(bad)?;
    let mut dec = r.into_decoder().map_err(bad)?;
    let orientation = dec.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut lim = image::Limits::default();
    lim.max_image_width = Some(limits::IMAGE_MAX_SIDE);
    lim.max_image_height = Some(limits::IMAGE_MAX_SIDE);
    lim.max_alloc = Some(DECODE_MAX_BYTES * 2);
    dec.set_limits(lim).map_err(bad)?;
    let mut img = image::DynamicImage::from_decoder(dec).map_err(bad)?;
    img.apply_orientation(orientation);
    let mut out = Vec::new();
    if jpeg {
        let rgb = image::DynamicImage::ImageRgb8(img.to_rgb8());
        let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY);
        rgb.write_with_encoder(enc).map_err(|e| e.to_string())?;
        Ok((out, "image/jpeg"))
    } else {
        img.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png).map_err(|e| e.to_string())?;
        Ok((out, "image/png"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageEncoder, RgbaImage};

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x % 256) as u8, (y % 256) as u8, 90, 255]));
        let mut out = Vec::new();
        image::codecs::png::PngEncoder::new(&mut out).write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgba8).unwrap();
        out
    }

    fn jpeg(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 40]));
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut out).write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgb8).unwrap();
        out
    }

    /// GIF: холст w×h, кадры (ширина, высота, задержка в сотых).
    fn gif_frames(w: u16, h: u16, frames: &[(u16, u16, u16)]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = gif::Encoder::new(&mut out, w, h, &[0, 0, 0, 255, 255, 255]).unwrap();
            enc.set_repeat(gif::Repeat::Infinite).unwrap();
            for &(fw, fh, delay) in frames {
                let mut f = gif::Frame { width: fw, height: fh, delay, ..Default::default() };
                f.buffer = std::borrow::Cow::Owned(vec![1u8; fw as usize * fh as usize]);
                enc.write_frame(&f).unwrap();
            }
        }
        out
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut c = 0xFFFF_FFFFu32;
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    fn gif_delays(b: &[u8]) -> Vec<u16> {
        let mut d = gif::DecodeOptions::new().read_info(std::io::Cursor::new(b)).unwrap();
        let mut v = Vec::new();
        while let Some(f) = d.read_next_frame().unwrap() {
            v.push(f.delay);
        }
        v
    }

    #[test]
    fn static_png_ok() {
        let out = sanitize(Kind::Avatar, &png(256, 256)).unwrap();
        assert!(out.starts_with(b"\x89PNG"));
        let d = imagesize::blob_size(&out).unwrap();
        assert_eq!((d.width, d.height), (256, 256));
    }

    #[test]
    fn wrong_size_rejected() {
        assert!(sanitize(Kind::Avatar, &png(255, 256)).is_err());
        assert!(sanitize(Kind::UserBanner, &png(960, 480)).is_err());
        assert!(sanitize(Kind::UserBanner, &png(960, 384)).is_ok());
    }

    #[test]
    fn too_big_rejected() {
        let mut b = png(256, 256);
        b.resize(proto::limits::AVATAR_MAX_BYTES + 1, 0);
        let e = sanitize(Kind::Avatar, &b).unwrap_err();
        assert!(e.contains("МБ"), "{e}");
    }

    #[test]
    fn not_an_image_rejected() {
        assert!(sanitize(Kind::Avatar, b"hello world, not an image at all").is_err());
    }

    #[test]
    fn jpeg_exif_stripped() {
        let j = jpeg(256, 256);
        let mut app1 = vec![0xFF, 0xE1];
        let payload = b"Exif\0\0GPS-55.75N-37.61E";
        app1.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        app1.extend_from_slice(payload);
        let mut with = j[..2].to_vec();
        with.extend_from_slice(&app1);
        with.extend_from_slice(&j[2..]);
        assert!(contains(&with, b"Exif"));
        let out = sanitize(Kind::Avatar, &with).unwrap();
        assert!(out.starts_with(b"\x89PNG"));
        assert!(!contains(&out, b"Exif") && !contains(&out, b"GPS"));
    }

    #[test]
    fn png_text_stripped() {
        let p = png(256, 256);
        // чанк tEXt сразу после IHDR (8 подпись + 25 IHDR)
        let data = b"Comment\0secret";
        let mut chunk = (data.len() as u32).to_be_bytes().to_vec();
        let mut body = b"tEXt".to_vec();
        body.extend_from_slice(data);
        chunk.extend_from_slice(&body);
        chunk.extend_from_slice(&crc32(&body).to_be_bytes());
        let mut with = p[..33].to_vec();
        with.extend_from_slice(&chunk);
        with.extend_from_slice(&p[33..]);
        let out = sanitize(Kind::Avatar, &with).unwrap();
        assert!(!contains(&out, b"secret"));
    }

    #[test]
    fn gif_animated_ok() {
        let g = gif_frames(256, 256, &[(256, 256, 5), (256, 256, 10), (256, 256, 15)]);
        let out = sanitize(Kind::Avatar, &g).unwrap();
        assert!(out.starts_with(b"GIF89a"));
        assert_eq!(gif_delays(&out), vec![5, 10, 15]);
    }

    #[test]
    fn gif_comment_stripped() {
        let mut g = gif_frames(256, 256, &[(256, 256, 5), (256, 256, 5)]);
        let trailer = g.pop().unwrap();
        assert_eq!(trailer, 0x3B);
        g.extend_from_slice(&[0x21, 0xFE, 6]);
        g.extend_from_slice(b"secret");
        g.extend_from_slice(&[0, 0x3B]);
        assert!(contains(&g, b"secret"));
        let out = sanitize(Kind::Avatar, &g).unwrap();
        assert!(!contains(&out, b"secret"));
        assert_eq!(gif_delays(&out).len(), 2);
    }

    #[test]
    fn gif_too_many_frames() {
        let frames = vec![(1u16, 1u16, 2u16); proto::limits::GIF_FRAMES_MAX + 1];
        let e = sanitize(Kind::Avatar, &gif_frames(256, 256, &frames)).unwrap_err();
        assert!(e.contains("кадр"), "{e}");
    }

    #[test]
    fn gif_frame_outside_canvas() {
        let g = gif_frames(256, 256, &[(256, 256, 5), (300, 300, 5)]);
        let e = sanitize(Kind::Avatar, &g).unwrap_err();
        assert!(e.contains("края"), "{e}");
    }

    #[test]
    fn gif_truncated() {
        let g = gif_frames(256, 256, &[(256, 256, 5), (256, 256, 5)]);
        let cut = &g[..g.len() / 2];
        let e = sanitize(Kind::Avatar, cut).unwrap_err();
        assert!(e.contains("повреждён"), "{e}");
    }

    #[test]
    fn chat_jpeg_exif_stripped_stays_jpeg() {
        let j = jpeg(300, 200);
        let mut app1 = vec![0xFF, 0xE1];
        let payload = b"Exif\0\0GPS-55.75N-37.61E";
        app1.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        app1.extend_from_slice(payload);
        let mut with = j[..2].to_vec();
        with.extend_from_slice(&app1);
        with.extend_from_slice(&j[2..]);
        let (out, mime, w, h) = sanitize_chat(&with).unwrap();
        assert_eq!((mime, w, h), ("image/jpeg", 300, 200));
        assert!(!contains(&out, b"Exif") && !contains(&out, b"GPS"));
    }

    #[test]
    fn chat_png_text_stripped() {
        let p = png(64, 64);
        let data = b"Comment\0secret";
        let mut chunk = (data.len() as u32).to_be_bytes().to_vec();
        let mut body = b"tEXt".to_vec();
        body.extend_from_slice(data);
        chunk.extend_from_slice(&body);
        chunk.extend_from_slice(&crc32(&body).to_be_bytes());
        let mut with = p[..33].to_vec();
        with.extend_from_slice(&chunk);
        with.extend_from_slice(&p[33..]);
        let (out, mime, _, _) = sanitize_chat(&with).unwrap();
        assert_eq!(mime, "image/png");
        assert!(!contains(&out, b"secret"));
    }

    #[test]
    fn chat_pixel_budget() {
        // 6000×5000 = 30 Мп > 25 Мп: отказ по заголовку, без декодирования
        let mut p = png(8, 8);
        p[16..20].copy_from_slice(&6000u32.to_be_bytes());
        p[20..24].copy_from_slice(&5000u32.to_be_bytes());
        let e = sanitize_chat(&p).unwrap_err();
        assert!(e.contains("мегапикселей"), "{e}");
    }

    #[test]
    fn chat_gif_animated_and_comment_stripped() {
        let mut g = gif_frames(100, 80, &[(100, 80, 5), (100, 80, 7)]);
        g.pop();
        g.extend_from_slice(&[0x21, 0xFE, 6]);
        g.extend_from_slice(b"secret");
        g.extend_from_slice(&[0, 0x3B]);
        let (out, mime, w, h) = sanitize_chat(&g).unwrap();
        assert_eq!((mime, w, h), ("image/gif", 100, 80));
        assert!(!contains(&out, b"secret"));
        assert_eq!(gif_delays(&out), vec![5, 7]);
    }

    #[test]
    fn chat_gif_too_heavy() {
        // холст 4000×4000 = 16 Мп, 5 кадров → 80 Мп > 64 Мп
        let g = gif_frames(4000, 4000, &vec![(4000, 4000, 2); 5]);
        assert!(sanitize_chat(&g).is_err());
    }

    #[test]
    fn chat_garbage_rejected() {
        assert!(sanitize_chat(b"not an image").is_err());
    }

    #[test]
    fn server_icon_square_any_side_no_gif() {
        assert!(sanitize(Kind::ServerIcon, &png(128, 128)).is_ok());
        assert!(sanitize(Kind::ServerIcon, &png(128, 64)).is_err());
        assert!(sanitize(Kind::ServerIcon, &png(1024, 1024)).is_err());
        let g = gif_frames(64, 64, &[(64, 64, 5), (64, 64, 5)]);
        assert!(sanitize(Kind::ServerIcon, &g).is_err());
    }

    #[test]
    fn server_banner_exact_and_exif_stripped() {
        let j = jpeg(960, 480);
        let mut app1 = vec![0xFF, 0xE1];
        let payload = b"Exif\0\0GPS";
        app1.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        app1.extend_from_slice(payload);
        let mut with = j[..2].to_vec();
        with.extend_from_slice(&app1);
        with.extend_from_slice(&j[2..]);
        let out = sanitize(Kind::ServerBanner, &with).unwrap();
        assert!(!contains(&out, b"Exif"));
        assert!(sanitize(Kind::ServerBanner, &png(960, 384)).is_err());
    }
}
