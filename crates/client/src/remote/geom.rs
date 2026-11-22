//! Пересчёт координат: картинка трансляции у зрителя ↔ экран стримера ↔ SendInput (0..65535).

use eframe::egui;

/// Прямоугольник в координатах виртуального рабочего стола (пиксели, левый край может быть отрицательным).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenRect {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

/// Точка над картинкой трансляции (`video` — прямоугольник самой картинки, без полос) → 0..1; вне картинки — None.
pub fn to_norm(video: egui::Rect, pos: egui::Pos2) -> Option<(f32, f32)> {
    if !video.contains(pos) || video.width() <= 0.0 || video.height() <= 0.0 {
        return None;
    }
    let x = ((pos.x - video.min.x) / video.width()).clamp(0.0, 1.0);
    let y = ((pos.y - video.min.y) / video.height()).clamp(0.0, 1.0);
    Some((x, y))
}

/// 0..1 → пиксель экрана внутри `target`.
pub fn from_norm(target: ScreenRect, x: f32, y: f32) -> (i32, i32) {
    let span = |len: i32, v: f32| (((len - 1).max(0) as f32) * v.clamp(0.0, 1.0)).round() as i32;
    (target.left + span(target.width, x), target.top + span(target.height, y))
}

/// Пиксель → координаты SendInput с MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK (0..=65535).
pub fn to_absolute(px: i32, py: i32, desk: ScreenRect) -> (i32, i32) {
    let map = |p: i32, start: i32, len: i32| -> i32 {
        let d = (len as i64 - 1).max(1);
        let v = ((p as i64 - start as i64).clamp(0, d) * 65535 + d / 2) / d;
        v as i32
    };
    (map(px, desk.left, desk.width), map(py, desk.top, desk.height))
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{pos2, Rect};

    #[test]
    fn norm_letterbox() {
        let video = Rect::from_min_max(pos2(100.0, 50.0), pos2(300.0, 150.0));
        assert_eq!(to_norm(video, pos2(200.0, 100.0)), Some((0.5, 0.5)));
        assert_eq!(to_norm(video, pos2(100.0, 50.0)), Some((0.0, 0.0)));
        assert_eq!(to_norm(video, pos2(50.0, 100.0)), None);
        assert_eq!(to_norm(video, pos2(200.0, 151.0)), None);
    }

    #[test]
    fn from_norm_corners() {
        let t = ScreenRect { left: -1920, top: 0, width: 1920, height: 1080 };
        assert_eq!(from_norm(t, 0.0, 0.0), (-1920, 0));
        assert_eq!(from_norm(t, 1.0, 1.0), (-1, 1079));
        assert_eq!(from_norm(t, 0.5, 0.5), (-960, 540));
    }

    #[test]
    fn absolute_virtual_desk_negative_left() {
        let desk = ScreenRect { left: -1920, top: 0, width: 3840, height: 1080 };
        assert_eq!(to_absolute(-1920, 0, desk), (0, 0));
        assert_eq!(to_absolute(1919, 1079, desk), (65535, 65535));
        let (x, _) = to_absolute(0, 0, desk);
        assert!((x - 32776).abs() <= 1, "{x}");
    }

    #[test]
    fn norm_roundtrip_center() {
        let video = Rect::from_min_max(pos2(0.0, 0.0), pos2(1280.0, 720.0));
        let (x, y) = to_norm(video, pos2(640.0, 360.0)).unwrap();
        let t = ScreenRect { left: 0, top: 0, width: 2560, height: 1440 };
        assert_eq!(from_norm(t, x, y), (1280, 720));
    }
}
