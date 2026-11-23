//! Своя рамка окна (без стандартной рамки Windows): полоса заголовка в стиле приложения, кнопки «свернуть /
//! развернуть / закрыть» (нарисованы кодом), перетаскивание и Aero Snap (`StartDrag` — системный цикл перемещения),
//! двойной клик — развернуть, ресайз за края (`BeginResize`). Тень и скруглённые углы Win11 — через DWM.

use eframe::egui::{
    self, Color32, CornerRadius, CursorIcon, Id, PointerButton, Pos2, Rect, ResizeDirection, Sense, Stroke, Ui,
    ViewportCommand, pos2, vec2,
};

use crate::theme as t;

pub const HEIGHT: f32 = 34.0;
const BTN_W: f32 = 46.0;
/// Толщина зоны ресайза у края окна (и больше — в углах).
const EDGE: f32 = 5.0;
const CORNER: f32 = 12.0;

/// HWND главного окна (для смены оформления DWM на полном экране).
#[cfg(windows)]
static HWND: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// Цвет тонкой рамки DWM (COLORREF = 0x00BBGGRR) для палитры.
fn border_colorref(p: &t::Palette) -> u32 {
    let c = p.border;
    (c.b() as u32) << 16 | (c.g() as u32) << 8 | c.r() as u32
}

/// Полный экран: без скругления углов и рамки DWM (иначе по краю кадра остаётся светлая линия), обратно — как было.
pub fn set_fullscreen_style(on: bool) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Graphics::Dwm::{
            DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DWMWCP_ROUND,
            DwmSetWindowAttribute,
        };
        let hwnd = HWND.load(std::sync::atomic::Ordering::Relaxed);
        if hwnd == 0 {
            return;
        }
        let hwnd = hwnd as windows_sys::Win32::Foundation::HWND;
        let corner: i32 = if on { DWMWCP_DONOTROUND } else { DWMWCP_ROUND };
        let border: u32 = if on { DWMWA_COLOR_NONE } else { border_colorref(t::p()) };
        unsafe {
            DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE as u32, (&corner as *const i32).cast(), 4);
            DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR as u32, (&border as *const u32).cast(), 4);
        }
    }
    #[cfg(not(windows))]
    let _ = on;
}

/// Альфа «почти непрозрачного» окна: 254 из 255 — глазом не отличить, но трекеры перекрытия Chrome/Firefox такое окно
/// не считают закрывающим (только альфу 255), и браузер под Murmur продолжает рисовать — трансляция окна не замирает.
#[cfg(windows)]
const LAYERED_ALPHA: u8 = 254;
#[cfg(windows)]
static LAYERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Держать окно layered (альфа 254), пока `on`; winit пересобирает стили окна — проверяем каждый кадр (один GetWindowLongW).
pub fn keep_layered(on: bool) {
    #[cfg(windows)]
    {
        use std::sync::atomic::Ordering;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GWL_EXSTYLE, GetWindowLongW, LWA_ALPHA, SetLayeredWindowAttributes, SetWindowLongW, WS_EX_LAYERED,
        };
        let hwnd = HWND.load(Ordering::Relaxed);
        if hwnd == 0 || (!on && !LAYERED.load(Ordering::Relaxed)) {
            return;
        }
        let hwnd = hwnd as windows_sys::Win32::Foundation::HWND;
        unsafe {
            let ex = GetWindowLongW(hwnd, GWL_EXSTYLE);
            let has = ex & WS_EX_LAYERED as i32 != 0;
            if on && !has {
                SetWindowLongW(hwnd, GWL_EXSTYLE, ex | WS_EX_LAYERED as i32);
                SetLayeredWindowAttributes(hwnd, 0, LAYERED_ALPHA, LWA_ALPHA);
                if !LAYERED.swap(true, Ordering::Relaxed) {
                    log::info!("окно: layered (альфа {LAYERED_ALPHA}) — пока идёт трансляция окна");
                }
            } else if !on {
                if has {
                    SetWindowLongW(hwnd, GWL_EXSTYLE, ex & !(WS_EX_LAYERED as i32));
                }
                LAYERED.store(false, Ordering::Relaxed);
                log::info!("окно: обычное");
            }
        }
    }
    #[cfg(not(windows))]
    let _ = on;
}

/// Перекрасить рамку окна под текущую тему (вызывать при смене темы; на полном экране рамки нет).
pub fn apply_border() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Graphics::Dwm::{DWMWA_BORDER_COLOR, DwmSetWindowAttribute};
        let hwnd = HWND.load(std::sync::atomic::Ordering::Relaxed);
        if hwnd == 0 {
            return;
        }
        let border = border_colorref(t::p());
        unsafe {
            DwmSetWindowAttribute(hwnd as windows_sys::Win32::Foundation::HWND, DWMWA_BORDER_COLOR as u32, (&border as *const u32).cast(), 4);
        }
    }
}

/// Скругление углов Win11 и цвет тонкой рамки DWM (на Win10 вызовы просто не действуют).
pub fn style_native_window(cc: &eframe::CreationContext<'_>) {
    #[cfg(windows)]
    {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        use windows_sys::Win32::Graphics::Dwm::{
            DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute,
        };
        let Ok(handle) = cc.window_handle() else { return };
        let RawWindowHandle::Win32(h) = handle.as_raw() else { return };
        HWND.store(h.hwnd.get(), std::sync::atomic::Ordering::Relaxed);
        let hwnd = h.hwnd.get() as windows_sys::Win32::Foundation::HWND;
        let corner: i32 = DWMWCP_ROUND;
        let border = border_colorref(t::p());
        unsafe {
            DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE as u32, (&corner as *const i32).cast(), 4);
            DwmSetWindowAttribute(hwnd, DWMWA_BORDER_COLOR as u32, (&border as *const u32).cast(), 4);
        }
    }
    #[cfg(not(windows))]
    let _ = cc;
}

fn maximized(ctx: &egui::Context) -> bool {
    ctx.input(|i| i.viewport().maximized.unwrap_or(false))
}

fn fullscreen(ctx: &egui::Context) -> bool {
    ctx.input(|i| i.viewport().fullscreen.unwrap_or(false))
}

/// Направление ресайза, если курсор у края окна (развёрнутое/полноэкранное окно не тянется).
fn edge_dir(ctx: &egui::Context) -> Option<ResizeDirection> {
    if maximized(ctx) || fullscreen(ctx) {
        return None;
    }
    let pos = ctx.input(|i| i.pointer.hover_pos())?;
    edge_dir_at(ctx.content_rect(), pos)
}

fn edge_dir_at(r: Rect, p: Pos2) -> Option<ResizeDirection> {
    if !r.expand(1.0).contains(p) {
        return None;
    }
    let near = |d: f32, zone: f32| d < zone;
    let (l, rt, tp, b) = (p.x - r.left(), r.right() - p.x, p.y - r.top(), r.bottom() - p.y);
    use ResizeDirection as D;
    // углы — зона пошире, чтобы попадать было легко
    Some(match () {
        _ if near(l, CORNER) && near(tp, CORNER) => D::NorthWest,
        _ if near(rt, CORNER) && near(tp, CORNER) => D::NorthEast,
        _ if near(l, CORNER) && near(b, CORNER) => D::SouthWest,
        _ if near(rt, CORNER) && near(b, CORNER) => D::SouthEast,
        _ if near(l, EDGE) => D::West,
        _ if near(rt, EDGE) => D::East,
        _ if near(tp, EDGE) => D::North,
        _ if near(b, EDGE) => D::South,
        _ => return None,
    })
}

fn cursor_for(d: ResizeDirection) -> CursorIcon {
    use ResizeDirection as D;
    match d {
        D::North | D::South => CursorIcon::ResizeVertical,
        D::East | D::West => CursorIcon::ResizeHorizontal,
        D::NorthWest | D::SouthEast => CursorIcon::ResizeNwSe,
        D::NorthEast | D::SouthWest => CursorIcon::ResizeNeSw,
    }
}

/// Ресайз за края. Вызывать в конце кадра (курсор ресайза перекрывает курсоры виджетов).
pub fn resize_edges(ctx: &egui::Context) {
    let Some(dir) = edge_dir(ctx) else { return };
    ctx.set_cursor_icon(cursor_for(dir));
    if ctx.input(|i| i.pointer.button_pressed(PointerButton::Primary)) {
        ctx.send_viewport_cmd(ViewportCommand::BeginResize(dir));
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Btn {
    Min,
    Max,
    Close,
}

/// Полоса заголовка — первой панелью кадра (на всю ширину окна). В полноэкранном режиме не показывать.
/// Слева знак + «Murmur», справа — свернуть/развернуть/закрыть (тонкие значки, подложка при наведении; «закрыть» — красная).
/// `badge` — надпись-кнопка слева от кнопок (есть обновление); возвращает true, если по ней кликнули.
pub fn title_bar(ui: &mut Ui, badge: Option<&str>, logo: bool) -> bool {
    let mut badge_clicked = false;
    egui::Panel::top("titlebar")
        .exact_size(HEIGHT)
        .resizable(false)
        .show_separator_line(false)
        .frame(egui::Frame::new().fill(t::p().title_bg))
        .show(ui, |ui| {
            let ctx = ui.ctx().clone();
            let r = ui.max_rect();
            let max = maximized(&ctx);
            let focused = ctx.input(|i| i.viewport().focused.unwrap_or(true)) || forced_focus();
            let close = Rect::from_min_size(pos2(r.right() - BTN_W - 6.0, r.top()), vec2(BTN_W, r.height()));
            let maxb = close.translate(vec2(-BTN_W, 0.0));
            let minb = maxb.translate(vec2(-BTN_W, 0.0));

            // надпись «есть обновление»: «таблетка» акцента
            let mut drag_right = minb.left();
            if let Some(text) = badge {
                let galley = ui.painter().layout_no_wrap(
                    format!("{}  {text}", egui_phosphor::regular::ARROW_CIRCLE_UP),
                    t::bold(12.5),
                    t::p().accent_text,
                );
                let size = vec2(galley.size().x + 20.0, 22.0);
                let rect = Rect::from_min_size(pos2(minb.left() - 12.0 - size.x, r.center().y - size.y / 2.0), size);
                let resp = ui.interact(rect, Id::new("titlebar-badge"), Sense::click());
                let lit = ctx.animate_bool_with_time(Id::new("titlebar-badge-lit"), resp.hovered(), 0.12);
                let p = ui.painter();
                p.rect_filled(rect, CornerRadius::same(255), t::p().accent_soft.lerp_to_gamma(t::p().accent.gamma_multiply(0.3), lit));
                p.galley(pos2(rect.left() + 10.0, rect.center().y - galley.size().y / 2.0), galley, t::p().accent_text);
                if resp.on_hover_text("Скачать и перезапустить").clicked() {
                    badge_clicked = true;
                }
                drag_right = rect.left() - 4.0;
            }

            let drag_rect = Rect::from_min_max(r.min, pos2(drag_right, r.bottom()));
            let drag = ui.interact(drag_rect, Id::new("titlebar-drag"), Sense::click_and_drag());
            if edge_dir(&ctx).is_none() {
                if drag.double_clicked() {
                    ctx.send_viewport_cmd(ViewportCommand::Maximized(!max));
                } else if drag.drag_started_by(PointerButton::Primary) {
                    ctx.send_viewport_cmd(ViewportCommand::StartDrag);
                }
            }

            let p = ui.painter();
            p.hline(r.x_range(), r.bottom() - 0.5, Stroke::new(1.0, t::p().border));
            // лого бренда (знак + «murmur»); окно без фокуса — приглушённо
            const LOGO_H: f32 = 20.0;
            if logo {
                t::paint_lockup_pose(p, pos2(r.left() + 14.0, r.center().y - LOGO_H / 2.0), LOGO_H, focused, cat_pose(&ctx));
            }

            let forced = forced_hover();
            for (rect, kind) in [(minb, Btn::Min), (maxb, Btn::Max), (close, Btn::Close)] {
                let resp = ui.interact(rect, Id::new(("titlebar-btn", kind as u8)), Sense::click());
                let hover = resp.hovered() || forced == Some(kind);
                let mut lit = ctx.animate_bool_with_time(Id::new(("titlebar-btn-lit", kind as u8)), hover, 0.12);
                if resp.is_pointer_button_down_on() {
                    lit *= 0.85;
                }
                let (bg, fg) = btn_colors(t::p(), kind, lit);
                p.rect_filled(rect, CornerRadius::ZERO, bg);
                draw_icon(p, rect.center(), kind, max, fg);
                let tip = match kind {
                    Btn::Min => "Свернуть",
                    Btn::Max if max => "Восстановить",
                    Btn::Max => "Развернуть",
                    Btn::Close => "Закрыть",
                };
                if resp.on_hover_text(tip).clicked() {
                    match kind {
                        Btn::Min => ctx.send_viewport_cmd(ViewportCommand::Minimized(true)),
                        Btn::Max => ctx.send_viewport_cmd(ViewportCommand::Maximized(!max)),
                        Btn::Close => ctx.send_viewport_cmd(ViewportCommand::Close),
                    }
                }
            }
        });
    badge_clicked
}

/// Все друзья не в сети — кот в заголовке спит (выставляет приложение каждый кадр).
static CAT_ASLEEP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_cat_asleep(on: bool) {
    CAT_ASLEEP.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Когда кто-то зашёл в канал — уши кота в заголовке приподнимаются (~300 мс).
static EARS_AT: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

pub fn perk_ears() {
    *EARS_AT.lock().unwrap_or_else(|e| e.into_inner()) = Some(std::time::Instant::now());
}

/// Моргание раз в 4–8 с (~150 мс) и уши. Между событиями — одна отложенная перерисовка к следующему моргания;
/// во время движения — до 30 к/с. «Живой кот» выключен, анимации не «Все» или окно не на виду — статичный знак, без перерисовок.
fn cat_pose(ctx: &egui::Context) -> t::MarkPose {
    use std::time::Duration;
    const BLINK: f64 = 0.15;
    // спит (все друзья не в сети) — статично, без перерисовок; видно и когда окно не в фокусе
    if crate::anim::live_cat_enabled() && CAT_ASLEEP.load(std::sync::atomic::Ordering::Relaxed) {
        return t::MarkPose { sleep: true, ..Default::default() };
    }
    if !crate::anim::live_cat(ctx) {
        return t::MarkPose::default();
    }
    let now = ctx.input(|i| i.time);
    let id = Id::new("title-cat-next-blink");
    let mut next = ctx.data(|d| d.get_temp::<f64>(id)).unwrap_or(now + 4.0);
    let mut pose = t::MarkPose::default();
    let mut wake = next - now;
    if now >= next {
        let k = (now - next) / BLINK;
        if k >= 1.0 {
            // следующее моргание через 4–8 с (псевдослучайно от времени)
            let rnd = ((now * 1000.0) as u64).wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407) >> 40;
            next = now + 4.0 + 4.0 * (rnd % 1000) as f64 / 1000.0;
            ctx.data_mut(|d| d.insert_temp(id, next));
            wake = next - now;
        } else {
            pose.blink = (k * std::f64::consts::PI).sin() as f32;
            wake = 0.033;
        }
    } else {
        ctx.data_mut(|d| d.insert_temp(id, next));
    }
    if let Some(at) = *EARS_AT.lock().unwrap_or_else(|e| e.into_inner()) {
        let el = at.elapsed().as_secs_f64();
        if el < 0.3 {
            pose.ears = 2.0 * (el / 0.3 * std::f64::consts::PI).sin() as f32;
            wake = wake.min(0.033);
        }
    }
    ctx.request_repaint_after(Duration::from_secs_f64(wake.max(0.016)));
    pose
}

/// Отладка вёрстки (только debug): MURMUR_FRAME_HOVER=min|max|close — кнопка как при наведении.
fn forced_hover() -> Option<Btn> {
    #[cfg(debug_assertions)]
    {
        match std::env::var("MURMUR_FRAME_HOVER").ok()?.as_str() {
            "min" => Some(Btn::Min),
            "max" => Some(Btn::Max),
            "close" => Some(Btn::Close),
            _ => None,
        }
    }
    #[cfg(not(debug_assertions))]
    None
}

/// Отладка (только debug): MURMUR_FRAME_FOCUS=1 — рисовать как активное окно (для скриншотов из фона).
fn forced_focus() -> bool {
    cfg!(debug_assertions) && std::env::var_os("MURMUR_FRAME_FOCUS").is_some()
}

/// Цвета кнопки рамки: `lit` 0..1 — наведение. «Закрыть» — красная подложка и белый значок.
fn btn_colors(p: &t::Palette, kind: Btn, lit: f32) -> (Color32, Color32) {
    let lit = lit.clamp(0.0, 1.0);
    let on = if kind == Btn::Close { p.danger } else { p.row_hover };
    let bg = Color32::TRANSPARENT.lerp_to_gamma(on, lit);
    let fg = if kind == Btn::Close { p.text_muted.lerp_to_gamma(Color32::WHITE, lit) } else { p.text_muted.lerp_to_gamma(p.text, lit) };
    if lit >= 1.0 { (on, if kind == Btn::Close { Color32::WHITE } else { p.text }) } else { (bg, fg) }
}

/// Значки 10×10 (тонкая линия).
fn draw_icon(p: &egui::Painter, c: Pos2, kind: Btn, maximized: bool, color: Color32) {
    let s = Stroke::new(1.2, color);
    let c = pos2(c.x.round() + 0.5, c.y.round() + 0.5);
    let k = 5.0 / 3.0; // было 6×6 — теперь 10×10
    match kind {
        Btn::Min => {
            p.hline(c.x - 3.0 * k..=c.x + 3.0 * k, c.y, s);
        }
        Btn::Max if !maximized => {
            p.rect_stroke(Rect::from_center_size(c, vec2(6.0 * k, 6.0 * k)), CornerRadius::same(1), s, egui::StrokeKind::Middle);
        }
        Btn::Max => {
            // «восстановить»: два квадрата, задний виден уголком
            p.rect_stroke(Rect::from_min_size(pos2(c.x - 3.0 * k, c.y - 1.5 * k), vec2(4.5 * k, 4.5 * k)), CornerRadius::same(1), s, egui::StrokeKind::Middle);
            p.line_segment([pos2(c.x - 1.5 * k, c.y - 3.0 * k), pos2(c.x + 3.0 * k, c.y - 3.0 * k)], s);
            p.line_segment([pos2(c.x + 3.0 * k, c.y - 3.0 * k), pos2(c.x + 3.0 * k, c.y + 1.5 * k)], s);
        }
        Btn::Close => {
            p.line_segment([pos2(c.x - 3.0 * k, c.y - 3.0 * k), pos2(c.x + 3.0 * k, c.y + 3.0 * k)], s);
            p.line_segment([pos2(c.x - 3.0 * k, c.y + 3.0 * k), pos2(c.x + 3.0 * k, c.y - 3.0 * k)], s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edges_and_corners() {
        let r = Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0));
        assert_eq!(edge_dir_at(r, pos2(400.0, 300.0)), None);
        assert_eq!(edge_dir_at(r, pos2(2.0, 300.0)), Some(ResizeDirection::West));
        assert_eq!(edge_dir_at(r, pos2(798.0, 300.0)), Some(ResizeDirection::East));
        assert_eq!(edge_dir_at(r, pos2(400.0, 1.0)), Some(ResizeDirection::North));
        assert_eq!(edge_dir_at(r, pos2(400.0, 597.0)), Some(ResizeDirection::South));
        assert_eq!(edge_dir_at(r, pos2(8.0, 8.0)), Some(ResizeDirection::NorthWest));
        assert_eq!(edge_dir_at(r, pos2(795.0, 590.0)), Some(ResizeDirection::SouthEast));
        assert_eq!(edge_dir_at(r, pos2(900.0, 300.0)), None);
    }

    #[test]
    fn button_colors_states() {
        // палитра передаётся явно: другие тесты переключают общую тему параллельно
        for p in [&t::DARK, &t::LIGHT] {
            for k in [Btn::Min, Btn::Max, Btn::Close] {
                let (bg, fg) = btn_colors(p, k, 0.0);
                assert_eq!(bg.a(), 0, "без наведения подложки нет");
                assert_eq!(fg, p.text_muted);
                let (bg, fg) = btn_colors(p, k, 1.0);
                if k == Btn::Close {
                    assert_eq!((bg, fg), (p.danger, Color32::WHITE));
                } else {
                    assert_eq!((bg, fg), (p.row_hover, p.text));
                }
            }
        }
    }

    #[test]
    fn border_follows_palette() {
        let (d, l) = (border_colorref(&t::DARK), border_colorref(&t::LIGHT));
        assert_ne!(d, l, "у светлой темы своя рамка окна");
        let c = t::LIGHT.border;
        assert_eq!(l, (c.b() as u32) << 16 | (c.g() as u32) << 8 | c.r() as u32);
    }
}
