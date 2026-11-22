//! Анимированные картинки профиля (GIF-аватары и баннеры): выбор кадра по времени, размеры показа,
//! бюджет памяти (LRU по байтам текстур). Кадры декодируются уже уменьшенными до размера показа.

use std::collections::VecDeque;

use eframe::egui;

/// Бюджет памяти на кадры всех анимаций (RGBA-текстуры).
pub const BUDGET_BYTES: usize = 64 * 1024 * 1024;

/// Кадр на момент `t_ms` (по кругу) и сколько мс до следующего. Задержка 0 — 100 мс (как в браузерах).
pub fn frame_at(delays_ms: &[u32], t_ms: u64) -> (usize, u32) {
    let d = |x: u32| if x == 0 { 100 } else { x };
    let total: u64 = delays_ms.iter().map(|&x| d(x) as u64).sum();
    if total == 0 {
        return (0, 100);
    }
    let mut t = t_ms % total;
    for (i, &x) in delays_ms.iter().enumerate() {
        let x = d(x) as u64;
        if t < x {
            return (i, (x - t) as u32);
        }
        t -= x;
    }
    (0, 100)
}

/// Сторона текстуры аватара: ближайшая сверху из 32/64/128/256.
pub fn bucket(px: f32) -> u32 {
    [32, 64, 128].into_iter().find(|&b| px <= b as f32).unwrap_or(256)
}

/// GIF с больше чем одним кадром (без распаковки кадров).
pub fn is_animated(bytes: &[u8]) -> bool {
    frame_count_upto(bytes, 2) > 1
}

/// Число кадров GIF без распаковки (0 — не GIF).
pub fn frame_count(bytes: &[u8]) -> usize {
    frame_count_upto(bytes, usize::MAX)
}

fn frame_count_upto(bytes: &[u8], limit: usize) -> usize {
    if !(bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        return 0;
    }
    let mut o = gif::DecodeOptions::new();
    o.skip_frame_decoding(true);
    let Ok(mut d) = o.read_info(std::io::Cursor::new(bytes)) else { return 0 };
    let mut n = 0;
    while n < limit && matches!(d.read_next_frame(), Ok(Some(_))) {
        n += 1;
    }
    n
}

/// Все кадры GIF, уменьшенные до w×h (cover — обрезка по центру), для аватара — круглые. Задержки в мс.
#[cfg(test)]
pub fn decode_frames(bytes: &[u8], w: u32, h: u32, round: bool) -> Result<(Vec<egui::ColorImage>, Vec<u32>), String> {
    decode_frames_capped(bytes, w, h, round, usize::MAX)
}

/// Как `decode_frames`, но кадров не больше чем на `max_bytes` (RGBA): лишние прореживаются, задержки складываются.
pub fn decode_frames_capped(bytes: &[u8], w: u32, h: u32, round: bool, max_bytes: usize) -> Result<(Vec<egui::ColorImage>, Vec<u32>), String> {
    use image::AnimationDecoder;
    let per = (w as usize * h as usize * 4).max(1);
    let n = frame_count(bytes).clamp(1, proto::limits::GIF_FRAMES_MAX);
    let step = (n * per).div_ceil(max_bytes.max(per)).max(1);
    let dec = image::codecs::gif::GifDecoder::new(std::io::Cursor::new(bytes)).map_err(|e| e.to_string())?;
    let (mut frames, mut delays) = (Vec::new(), Vec::new());
    for (i, f) in dec.into_frames().take(proto::limits::GIF_FRAMES_MAX).enumerate() {
        let f = f.map_err(|e| e.to_string())?;
        let (num, den) = f.delay().numer_denom_ms();
        let ms = num.checked_div(den).unwrap_or(0);
        if i % step != 0 {
            if let Some(last) = delays.last_mut() {
                *last += ms;
            }
            continue;
        }
        delays.push(ms);
        let img = image::DynamicImage::ImageRgba8(f.into_buffer());
        let mut px = img.resize_to_fill(w, h, image::imageops::FilterType::Triangle).to_rgba8().into_raw();
        if round {
            crate::people::mask_round(&mut px, w);
        }
        frames.push(egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &px));
    }
    if frames.is_empty() {
        return Err("GIF пустой".into());
    }
    Ok((frames, delays))
}

/// Последние показанные — в конце; вытесняются самые давние, пока сумма больше бюджета.
pub struct Lru {
    budget: usize,
    /// (id, байты, номер кадра последнего показа)
    items: VecDeque<(String, usize, u64)>,
    tick: u64,
}

impl Lru {
    pub fn new(budget: usize) -> Self {
        Self { budget, items: VecDeque::new(), tick: 0 }
    }

    /// Отметить показ (каждый вызов — как новый кадр); вернуть вытесненные id.
    pub fn touch(&mut self, id: &str, bytes: usize) -> Vec<String> {
        self.tick += 1;
        let t = self.tick;
        self.touch_at(id, bytes, t)
    }

    /// Отметить показ в кадре `frame`; вытесняются самые давние, но не показанные в этом же кадре
    /// (иначе две анимации на экране вытесняли бы друг друга по кругу).
    pub fn touch_at(&mut self, id: &str, bytes: usize, frame: u64) -> Vec<String> {
        self.items.retain(|(k, _, _)| k != id);
        self.items.push_back((id.to_string(), bytes, frame));
        let mut sum: usize = self.items.iter().map(|(_, b, _)| b).sum();
        let mut out = Vec::new();
        while sum > self.budget {
            // показанные в этом или прошлом кадре — на экране (порядок отрисовки внутри кадра не важен)
            let Some(i) = self.items.iter().position(|(_, _, f)| f + 1 < frame) else { break };
            let (k, b, _) = self.items.remove(i).unwrap();
            sum -= b;
            out.push(k);
        }
        out
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gif3() -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = gif::Encoder::new(&mut out, 100, 100, &[255, 0, 0, 0, 0, 255]).unwrap();
            for d in [5u16, 10, 15] {
                let f = gif::Frame { width: 100, height: 100, delay: d, buffer: std::borrow::Cow::Owned(vec![1; 10_000]), ..Default::default() };
                enc.write_frame(&f).unwrap();
            }
        }
        out
    }

    #[test]
    fn decode_round_avatar_frames() {
        let (frames, delays) = decode_frames(&gif3(), 64, 64, true).unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!(delays, vec![50, 100, 150]);
        assert_eq!(frames[0].size, [64, 64]);
        assert_eq!(frames[0].pixels[0].a(), 0, "углы прозрачные");
    }

    #[test]
    fn decode_banner_frames_cover() {
        let (frames, _) = decode_frames(&gif3(), 320, 128, false).unwrap();
        assert_eq!(frames[2].size, [320, 128]);
        assert_eq!(frames[2].pixels[0].a(), 255);
    }

    #[test]
    fn static_is_not_animated() {
        let mut png = std::io::Cursor::new(Vec::new());
        image::RgbaImage::new(8, 8).write_to(&mut png, image::ImageFormat::Png).unwrap();
        assert!(!is_animated(&png.into_inner()));
        assert!(is_animated(&gif3()));
    }

    #[test]
    fn frame_at_wraps() {
        assert_eq!(frame_at(&[100, 200], 250), (1, 50));
        assert_eq!(frame_at(&[100, 200], 300), (0, 100));
        assert_eq!(frame_at(&[100, 200], 0), (0, 100));
    }

    #[test]
    fn zero_delay_is_100ms() {
        assert_eq!(frame_at(&[0, 0], 150), (1, 50));
    }

    #[test]
    fn bucket_rounds_up() {
        assert_eq!(bucket(30.0), 32);
        assert_eq!(bucket(33.0), 64);
        assert_eq!(bucket(100.0), 128);
        assert_eq!(bucket(300.0), 256);
    }

    #[test]
    fn lru_keeps_items_shown_this_frame() {
        let mut l = Lru::new(100);
        // a и b вместе больше бюджета, оба на экране в одном кадре — никто не вытесняется (иначе — бесконечное перекодирование)
        assert!(l.touch_at("a", 60, 1).is_empty());
        assert!(l.touch_at("b", 60, 1).is_empty());
        assert!(l.touch_at("a", 60, 2).is_empty());
        assert!(l.touch_at("b", 60, 2).is_empty());
        // кадр 4: показан только c — a и b с кадра 2 не показывались, вытесняются
        assert_eq!(l.touch_at("c", 60, 4), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn decode_caps_bytes_by_thinning() {
        // 3 кадра по 64×64×4 = 16 КБ; предел 40 КБ → каждый 2-й кадр, задержки складываются
        let (frames, delays) = decode_frames_capped(&gif3(), 64, 64, true, 40_000).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(delays, vec![150, 150]);
    }

    #[test]
    fn lru_evicts_oldest_over_budget() {
        let mut l = Lru::new(100);
        assert!(l.touch("a", 60).is_empty());
        assert!(l.touch("b", 30).is_empty());
        assert_eq!(l.touch("c", 30), vec!["a".to_string()]);
        assert!(l.touch("b", 30).is_empty());
        assert_eq!(l.touch("d", 50), vec!["c".to_string()]);
    }
}
