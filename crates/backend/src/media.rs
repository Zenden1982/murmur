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
}

impl Kind {
    /// (ширина, высота, байт не больше).
    fn spec(self) -> (u32, u32, usize) {
        match self {
            Kind::Avatar => (limits::AVATAR_SIDE, limits::AVATAR_SIDE, limits::AVATAR_MAX_BYTES),
            Kind::UserBanner => (limits::USER_BANNER_W, limits::USER_BANNER_H, limits::USER_BANNER_MAX_BYTES),
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
    if (dim.width, dim.height) != (w as usize, h as usize) {
        return Err(format!("нужна картинка {w}×{h}"));
    }
    if mime == "image/gif" {
        scan_gif(body, w, h)?;
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
fn scan_gif(body: &[u8], w: u32, h: u32) -> Result<(), String> {
    let bad = |_| "GIF повреждён".to_string();
    let mut d = gif_options(true).read_info(Cursor::new(body)).map_err(bad)?;
    if (d.width() as u32, d.height() as u32) != (w, h) {
        return Err(format!("нужна картинка {w}×{h}"));
    }
    let budget = limits::GIF_FRAMES_MAX as u64 * w as u64 * h as u64;
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
}
