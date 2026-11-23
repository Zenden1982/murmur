//! Интерфейс указки и управления: слой поверх трансляции у зрителя, запрос и плашка у стримера.

use eframe::egui::{self, Align2, Color32, CornerRadius, Frame, Id, Margin, Pos2, Rect, RichText, Sense, Shape, Stroke, Ui, pos2, vec2};
use egui_phosphor::regular as ph;

use crate::kit::{self, ButtonKind};
use crate::remote::geom::to_norm;
use crate::remote::guest::GuestPhase;
use crate::remote::proto::{Btn, InputEv, Level};
use crate::theme as t;
use crate::voice::{Cmd, VoiceHandle, VoiceState};

use crate::app::ON_VIDEO;

/// Цвет указки участника — яркий и одинаковый в любой теме (у стримера, у зрителей).
pub fn pointer_color(name: &str) -> Color32 {
    const P: [Color32; 6] = [
        Color32::from_rgb(0xF5, 0x9E, 0x0B),
        Color32::from_rgb(0xEC, 0x48, 0x99),
        Color32::from_rgb(0x14, 0xB8, 0xA6),
        Color32::from_rgb(0x63, 0x66, 0xF1),
        Color32::from_rgb(0x22, 0xC5, 0x5E),
        Color32::from_rgb(0xF4, 0x3F, 0x5E),
    ];
    let mut h: u32 = 0x811c_9dc5;
    for b in name.bytes() {
        h = (h ^ b as u32).wrapping_mul(0x0100_0193);
    }
    P[(h % P.len() as u32) as usize]
}

/// Подписи кнопок зрителя по состоянию доступа.
pub fn guest_buttons(phase: &GuestPhase, upgrade_pending: bool) -> Vec<&'static str> {
    match phase {
        GuestPhase::Idle => vec!["Попросить управление"],
        GuestPhase::Requested { .. } => Vec::new(),
        GuestPhase::Granted { controlling: true, .. } => vec!["Вернуть указку"],
        GuestPhase::Granted { .. } if upgrade_pending => Vec::new(),
        GuestPhase::Granted { .. } => vec!["Взять управление"],
    }
}

fn name_of(vs: &VoiceState, id: &str) -> String {
    vs.peers.iter().find(|p| p.identity == id).map(|p| p.name.clone()).unwrap_or_else(|| id.to_string())
}

fn my_identity(vs: &VoiceState) -> Option<&str> {
    vs.peers.iter().find(|p| p.is_local).map(|p| p.identity.as_str())
}

fn chip(painter: &egui::Painter, pos: Pos2, anchor: Align2, text: &str, color: Color32) -> Rect {
    let galley = painter.layout_no_wrap(text.to_string(), t::semibold(13.0), color);
    let size = galley.size() + vec2(18.0, 10.0);
    let r = anchor.anchor_size(pos, size);
    painter.rect_filled(r, CornerRadius::same(255), t::overlay_dark(200));
    painter.galley(r.min + vec2(9.0, 5.0), galley, color);
    r
}

/// Кнопка-пилюля поверх видео.
fn pill(ui: &Ui, center: Pos2, text: &str, id: Id, accent: bool) -> (egui::Response, Rect) {
    let galley = ui.painter().layout_no_wrap(text.to_string(), t::semibold(13.5), ON_VIDEO);
    let rect = Rect::from_center_size(center, galley.size() + vec2(26.0, 14.0));
    let resp = ui.interact(rect, id, Sense::click());
    let hot = resp.hovered();
    let fill = match (accent, hot) {
        (true, false) => t::p().accent_strong,
        (true, true) => t::p().accent_strong_hover,
        (false, false) => t::overlay_dark(210),
        (false, true) => t::p().accent,
    };
    ui.painter().rect_filled(rect, CornerRadius::same(255), fill);
    ui.painter().galley(rect.min + vec2(13.0, 7.0), galley, ON_VIDEO);
    (resp.on_hover_cursor(egui::CursorIcon::PointingHand), rect)
}

/// Стрелка указки (острие в `tip`) и ник.
fn arrow(painter: &egui::Painter, tip: Pos2, color: Color32, name: &str) {
    let pts = [(0.0, 0.0), (0.0, 19.0), (5.0, 14.0), (9.0, 22.0), (12.0, 21.0), (8.0, 13.0), (14.0, 13.0)];
    let poly: Vec<Pos2> = pts.iter().map(|(x, y)| tip + vec2(*x, *y)).collect();
    painter.add(Shape::convex_polygon(poly[..3].iter().copied().chain([poly[6], poly[5]]).collect(), color, Stroke::new(1.0, Color32::from_gray(25))));
    painter.add(Shape::convex_polygon(vec![poly[2], poly[3], poly[4], poly[5]], color, Stroke::new(1.0, Color32::from_gray(25))));
    let galley = painter.layout_no_wrap(name.to_string(), t::semibold(12.0), Color32::WHITE);
    let r = Rect::from_min_size(tip + vec2(14.0, 20.0), galley.size() + vec2(12.0, 6.0));
    painter.rect_filled(r, CornerRadius::same(6), color);
    painter.galley(r.min + vec2(6.0, 3.0), galley, Color32::WHITE);
}

/// Указки других зрителей поверх трансляции `streamer` (свою не рисуем — видна своя мышь).
pub fn draw_pointers(painter: &egui::Painter, vr: Rect, streamer: &str, vs: &VoiceState) {
    let Some(list) = vs.rc_pointers.get(streamer) else { return };
    let me = my_identity(vs);
    for p in list {
        if Some(p.who.as_str()) == me {
            continue;
        }
        let name = name_of(vs, &p.who);
        let tip = vr.min + vec2(p.x * vr.width(), p.y * vr.height());
        arrow(painter, tip, pointer_color(&name), &name);
    }
}

fn btn_of(b: egui::PointerButton) -> Btn {
    match b {
        egui::PointerButton::Primary => Btn::L,
        egui::PointerButton::Secondary => Btn::R,
        egui::PointerButton::Middle => Btn::M,
        egui::PointerButton::Extra1 => Btn::X1,
        egui::PointerButton::Extra2 => Btn::X2,
    }
}

/// Колесо egui → единицы WHEEL_DELTA (120 на щелчок).
fn wheel_units(unit: egui::MouseWheelUnit, d: f32) -> i16 {
    let lines = match unit {
        egui::MouseWheelUnit::Line => d,
        egui::MouseWheelUnit::Point => d / 40.0,
        egui::MouseWheelUnit::Page => d * 3.0,
    };
    (lines * 120.0).round().clamp(-30000.0, 30000.0) as i16
}

/// Слой поверх просматриваемой трансляции: указки, кнопки доступа, указка/управление.
/// Возвращает `true`, если мышь над видео сейчас управляет ПК друга (плитке нельзя реагировать на клики).
pub fn guest_layer(ui: &mut Ui, rect: Rect, vr: Rect, streamer: &str, vs: &VoiceState, voice: &VoiceHandle, hovered: bool) -> bool {
    let painter = ui.painter().clone();
    draw_pointers(&painter, vr, streamer, vs);
    let view = vs.rc_guest.get(streamer);
    let phase = view.map(|v| v.phase.clone()).unwrap_or(GuestPhase::Idle);
    let upgrade = view.is_some_and(|v| v.upgrade_pending);
    let controlling = matches!(phase, GuestPhase::Granted { controlling: true, .. });
    let granted = matches!(phase, GuestPhase::Granted { .. });
    let sid = streamer.to_string();
    let send = |c: Cmd| voice.send(c);

    // состояние — подписью сверху
    let top = pos2(vr.center().x, vr.top() + 14.0);
    match &phase {
        GuestPhase::Requested { .. } => {
            chip(&painter, top, Align2::CENTER_TOP, &format!("{}  Ждём ответа друга…", ph::HOURGLASS), ON_VIDEO);
        }
        GuestPhase::Granted { controlling: true, .. } => {
            let v = view.expect("granted");
            let text = if v.paused {
                format!("{}  Друг двигает мышь — управление на паузе", ph::PAUSE)
            } else {
                let lay = if v.layout.is_empty() { String::new() } else { format!(" · раскладка {}", v.layout) };
                format!("{}  Ты управляешь{lay} · правый Ctrl — вернуть указку", ph::CURSOR_CLICK)
            };
            chip(&painter, top, Align2::CENTER_TOP, &text, ON_VIDEO);
        }
        GuestPhase::Granted { .. } if upgrade => {
            chip(&painter, top, Align2::CENTER_TOP, &format!("{}  Ждём разрешения на управление…", ph::HOURGLASS), ON_VIDEO);
        }
        GuestPhase::Granted { .. } if hovered => {
            chip(&painter, top, Align2::CENTER_TOP, &format!("{}  Указка: друг видит твою мышь", ph::HAND_POINTING), ON_VIDEO);
        }
        _ => {}
    }

    // кнопки — снизу по центру (при наведении или пока управляем)
    let mut blocked: Vec<Rect> = Vec::new();
    let menu_id = Id::new(("rc-menu", streamer));
    let mut menu_open = ui.ctx().data(|d| d.get_temp::<bool>(menu_id).unwrap_or(false));
    if hovered || controlling || menu_open {
        let c = pos2(rect.center().x, rect.bottom() - 30.0);
        for label in guest_buttons(&phase, upgrade) {
            let (icon, accent) = match label {
                "Попросить управление" => (ph::HAND_POINTING, false),
                "Взять управление" => (ph::CURSOR_CLICK, true),
                _ => (ph::ARROW_U_UP_LEFT, false),
            };
            let (resp, r) = pill(ui, c, &format!("{icon}  {label}"), Id::new(("rc-btn", streamer, label)), accent);
            blocked.push(r);
            if resp.clicked() {
                match label {
                    "Попросить управление" => menu_open = !menu_open,
                    "Взять управление" => send(Cmd::RcTake { identity: sid.clone(), on: true }),
                    _ => send(Cmd::RcTake { identity: sid.clone(), on: false }),
                }
            }
        }
        if menu_open && matches!(phase, GuestPhase::Idle) {
            let opts = [("Указка и управление", Level::Control, 92.0), ("Только указка", Level::Pointer, 50.0)];
            for (label, level, dy) in opts {
                let (resp, r) = pill(ui, c - vec2(0.0, dy), label, Id::new(("rc-opt", streamer, label)), level == Level::Control);
                blocked.push(r);
                if resp.clicked() {
                    send(Cmd::RcRequest { identity: sid.clone(), level });
                    menu_open = false;
                }
            }
        } else {
            menu_open = false;
        }
    } else {
        menu_open = false;
    }
    ui.ctx().data_mut(|d| d.insert_temp(menu_id, menu_open));

    if !granted {
        return false;
    }
    // указка (и курсор, если управляем)
    let hover = ui.ctx().pointer_hover_pos().filter(|p| !blocked.iter().any(|r| r.contains(*p)));
    if let Some((x, y)) = hover.and_then(|p| to_norm(vr, p)) {
        send(Cmd::RcPointer { identity: sid.clone(), x, y });
    }
    if !controlling {
        return false;
    }
    let events = ui.input(|i| i.events.clone());
    for e in events {
        match e {
            egui::Event::PointerButton { pos, button, pressed, .. } => {
                if blocked.iter().any(|r| r.contains(pos)) {
                    continue;
                }
                let b = btn_of(button);
                if pressed {
                    if let Some((x, y)) = to_norm(vr, pos) {
                        send(Cmd::RcInput { identity: sid.clone(), ev: InputEv::Button { b, down: true, x, y } });
                    }
                } else {
                    // отпускание — всегда, даже за краем видео (иначе кнопка «залипнет» у друга)
                    let x = ((pos.x - vr.min.x) / vr.width().max(1.0)).clamp(0.0, 1.0);
                    let y = ((pos.y - vr.min.y) / vr.height().max(1.0)).clamp(0.0, 1.0);
                    send(Cmd::RcInput { identity: sid.clone(), ev: InputEv::Button { b, down: false, x, y } });
                }
            }
            egui::Event::MouseWheel { unit, delta, .. } => {
                if let Some((x, y)) = hover.and_then(|p| to_norm(vr, p)) {
                    let (dx, dy) = (wheel_units(unit, delta.x), wheel_units(unit, delta.y));
                    if dx != 0 || dy != 0 {
                        send(Cmd::RcInput { identity: sid.clone(), ev: InputEv::Wheel { dx, dy, x, y } });
                    }
                }
            }
            _ => {}
        }
    }
    hover.is_some_and(|p| vr.contains(p))
}

/// Стример: запрос доступа (поверх всего окна Murmur).
pub fn host_dialog(ctx: &egui::Context, vs: &VoiceState, voice: &VoiceHandle) {
    let Some((who, level)) = vs.rc_host.pending.first().cloned() else { return };
    let name = name_of(vs, &who);
    // кнопки оживают через 0,5 с после смены запроса — чтобы клик, начатый для другого запроса, не ответил этому
    let seen_id = Id::new("rc-ask-seen");
    let now = ctx.input(|i| i.time);
    let shown_at = match ctx.data(|d| d.get_temp::<(String, f64)>(seen_id)) {
        Some((w, at)) if w == who => at,
        _ => {
            ctx.data_mut(|d| d.insert_temp(seen_id, (who.clone(), now)));
            now
        }
    };
    let armed = now - shown_at >= 0.5;
    if !armed {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
    egui::Area::new(Id::new("rc-ask"))
        .order(egui::Order::Foreground)
        .anchor(Align2::CENTER_TOP, vec2(0.0, 84.0))
        .show(ctx, |ui| {
            Frame::new()
                .fill(t::p().surface)
                .stroke(Stroke::new(1.0, t::p().accent))
                .corner_radius(CornerRadius::same(14))
                .inner_margin(Margin::same(18))
                .shadow(egui::epaint::Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(90) })
                .show(ui, |ui| {
                    ui.set_max_width(460.0);
                    let what = if level == Level::Control { "указку и управление" } else { "указку" };
                    ui.label(RichText::new(format!("{}  {name} просит {what}", ph::HAND_POINTING)).font(t::bold(16.0)).color(t::p().text));
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("Указка — второй курсор на твоём экране, ничего не нажимает. Управление — мышь и клавиатура. Забрать в любой момент: Ctrl+Alt+Пробел.")
                            .size(13.0)
                            .color(t::p().text_muted),
                    );
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        let mut answer = None;
                        if kit::button(ui, ButtonKind::Secondary, "Только указка", Some(ph::HAND_POINTING)).clicked() {
                            answer = Some(Some(Level::Pointer));
                        }
                        if kit::button(ui, ButtonKind::Primary, "Указка и управление", Some(ph::CURSOR_CLICK)).clicked() {
                            answer = Some(Some(Level::Control));
                        }
                        if kit::button(ui, ButtonKind::Ghost, "Отказать", None).clicked() {
                            answer = Some(None);
                        }
                        if let Some(grant) = answer.filter(|_| armed) {
                            voice.send(Cmd::RcRespond { identity: who.clone(), grant });
                        }
                    });
                });
        });
}

/// Стример: кто сейчас с указкой/управлением и кнопка «Забрать».
pub fn host_bar(ctx: &egui::Context, vs: &VoiceState, voice: &VoiceHandle) {
    let h = &vs.rc_host;
    if h.pointers.is_empty() {
        return;
    }
    let text = match &h.controller {
        Some(c) => format!("{}  Тобой управляет {}", ph::CURSOR_CLICK, name_of(vs, c)),
        None => format!("{}  Указка: {}", ph::HAND_POINTING, h.pointers.iter().map(|p| name_of(vs, p)).collect::<Vec<_>>().join(", ")),
    };
    egui::Area::new(Id::new("rc-bar"))
        .order(egui::Order::Foreground)
        .anchor(Align2::CENTER_TOP, vec2(130.0, 76.0))
        .show(ctx, |ui| {
            Frame::new()
                .fill(t::p().warn_bg)
                .stroke(Stroke::new(1.0, t::p().warn_border))
                .corner_radius(CornerRadius::same(255))
                .inner_margin(Margin::symmetric(14, 6))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(text).color(t::p().text).size(13.5).strong());
                        if h.paused && h.controller.is_some() {
                            ui.label(RichText::new("· пауза").color(t::p().text_muted).size(13.0));
                        }
                        if let Some(b) = &h.blocked {
                            ui.label(RichText::new(format!("· {b}")).color(t::p().text_muted).size(13.0));
                        }
                        if h.no_overlay {
                            ui.label(RichText::new("· указка на экране недоступна (нужна Windows 10 2004+)").color(t::p().text_muted).size(13.0));
                        }
                        if kit::button(ui, ButtonKind::Danger, "Забрать", None).on_hover_text("Ctrl+Alt+Пробел — с любого окна").clicked() {
                            voice.send(Cmd::RcRevokeAll);
                        }
                    });
                });
        });
}

/// Уведомление об управлении («Друг отказал», «кликни в окно»).
pub fn notice(ctx: &egui::Context, vs: &VoiceState) {
    let Some(n) = &vs.rc_notice else { return };
    egui::Area::new(Id::new("rc-notice"))
        .order(egui::Order::Foreground)
        .anchor(Align2::CENTER_BOTTOM, vec2(130.0, -140.0))
        .interactable(false)
        .show(ctx, |ui| {
            Frame::new()
                .fill(t::overlay_dark(220))
                .corner_radius(CornerRadius::same(255))
                .inner_margin(Margin::symmetric(16, 8))
                .show(ui, |ui| {
                    ui.label(RichText::new(format!("{}  {n}", ph::INFO)).color(ON_VIDEO).size(13.5));
                });
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::guest::GuestPhase;
    use crate::remote::proto::{Level, Session};

    fn s() -> Session {
        Session(format!("{:032x}", 1))
    }

    #[test]
    fn guest_buttons_by_phase() {
        assert_eq!(guest_buttons(&GuestPhase::Idle, false), vec!["Попросить управление"]);
        assert!(guest_buttons(&GuestPhase::Requested { level: Level::Pointer }, false).is_empty());
        let ptr = GuestPhase::Granted { level: Level::Pointer, session: s(), controlling: false };
        assert_eq!(guest_buttons(&ptr, false), vec!["Взять управление"]);
        assert!(guest_buttons(&ptr, true).is_empty());
        let ctl = GuestPhase::Granted { level: Level::Control, session: s(), controlling: true };
        assert_eq!(guest_buttons(&ctl, false), vec!["Вернуть указку"]);
    }

    #[test]
    fn pointer_color_stable_and_bright() {
        let a = pointer_color("Никита");
        assert_eq!(a, pointer_color("Никита"));
        let max = a.r().max(a.g()).max(a.b());
        assert!(max >= 180, "{a:?}");
    }
}
