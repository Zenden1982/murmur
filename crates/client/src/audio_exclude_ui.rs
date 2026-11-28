//! Список «Не включать в звук трансляции»: программы со звуком (обновляется в фоне) и исключённые.
//! Один виджет для настроек и для всплывающего окна в окне запуска стрима.

use std::sync::{Arc, Mutex};

use capture_win::audio_sessions::AudioApp;
use eframe::egui::{self, RichText, Ui};
use egui_phosphor::regular as ph;

use crate::kit;
use crate::theme as t;

const REFRESH_SECS: f64 = 2.0;

/// Кэш программ со звуком + выбор .exe в фоне.
#[derive(Default)]
pub struct AudioApps {
    list: Arc<Mutex<Option<Vec<AudioApp>>>>,
    busy: Arc<Mutex<bool>>,
    last: Option<f64>,
    picked: Arc<Mutex<Option<String>>>,
    /// Показать и молчащие программы (по умолчанию — только исключённые и играющие).
    show_all: bool,
}

impl AudioApps {
    /// Обновить, если пора (вызывать каждый кадр, пока список виден).
    fn tick(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        ctx.request_repaint_after(std::time::Duration::from_secs_f64(REFRESH_SECS));
        if self.last.is_some_and(|l| now - l < REFRESH_SECS) || *self.busy.lock().unwrap_or_else(|e| e.into_inner()) {
            return;
        }
        self.last = Some(now);
        *self.busy.lock().unwrap_or_else(|e| e.into_inner()) = true;
        let (list, busy, ctx) = (self.list.clone(), self.busy.clone(), ctx.clone());
        std::thread::Builder::new()
            .name("audio-apps".into())
            .spawn(move || {
                let v = capture_win::audio_sessions::audio_apps();
                *list.lock().unwrap_or_else(|e| e.into_inner()) = Some(v);
                *busy.lock().unwrap_or_else(|e| e.into_inner()) = false;
                ctx.request_repaint();
            })
            .ok();
    }

    fn pick_exe(&self, ctx: &egui::Context) {
        let (picked, ctx) = (self.picked.clone(), ctx.clone());
        std::thread::spawn(move || {
            if let Some(p) = rfd::FileDialog::new().set_title("Программа, чей звук не нужен в трансляции").add_filter("Программы", &["exe"]).pick_file()
                && let Some(name) = p.file_name()
            {
                *picked.lock().unwrap_or_else(|e| e.into_inner()) = Some(name.to_string_lossy().into_owned());
                ctx.request_repaint();
            }
        });
    }
}

fn programs(n: usize) -> &'static str {
    match (n % 10, n % 100) {
        (1, r) if r != 11 => "программа",
        (2..=4, r) if !(12..=14).contains(&r) => "программы",
        _ => "программ",
    }
}

/// Подпись ссылки в окне запуска стрима.
pub fn link_text(n: usize) -> String {
    match n {
        0 => "Исключить программы…".into(),
        1 => format!("Без 1 программы {}", ph::CARET_DOWN),
        n => format!("Без {n} {} {}", if programs(n) == "программа" { "программы" } else { "программ" }, ph::CARET_DOWN),
    }
}

/// Строки: сначала исключённые, затем остальные запущенные со звуком. true — список изменился.
pub fn exclude_list(ui: &mut Ui, apps: &mut AudioApps, list: &mut Vec<String>, width: f32) -> bool {
    apps.tick(ui.ctx());
    let mut changed = false;
    if let Some(name) = apps.picked.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let name = name.to_lowercase();
        if !list.contains(&name) {
            list.push(name);
            changed = true;
        }
    }
    let running = apps.list.lock().unwrap_or_else(|e| e.into_inner()).clone();
    // (имя для показа, ключ, играет ли)
    let mut rows: Vec<(String, String, bool)> = Vec::new();
    for ex in list.iter() {
        let live = running.as_ref().and_then(|v| v.iter().find(|a| a.exe.eq_ignore_ascii_case(ex)));
        rows.push((live.map_or(ex.clone(), |a| a.exe.clone()), ex.clone(), live.is_some_and(|a| a.active)));
    }
    // свой exe (другая копия Murmur) не показываем — Murmur и так никогда не попадает в звук трансляции
    let own = std::env::current_exe().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
    let mut quiet = 0;
    for a in running.iter().flatten() {
        let key = a.exe.to_lowercase();
        if list.contains(&key) || own.as_deref() == Some(key.as_str()) || key == "murmur.exe" {
            continue;
        }
        if a.active || apps.show_all {
            rows.push((a.exe.clone(), key, a.active));
        } else {
            quiet += 1;
        }
    }
    if rows.is_empty() && quiet == 0 {
        let text = if running.is_none() { "Ищем программы со звуком…" } else { "Сейчас ни одна программа не выводит звук" };
        ui.label(RichText::new(text).size(12.5).color(t::p().text_muted));
    }
    let max_h = if width < 400.0 { 240.0 } else { f32::INFINITY };
    egui::ScrollArea::vertical().id_salt(("audio-exclude", width as i32)).max_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
    for (shown, key, active) in rows {
        let mut on = list.contains(&key);
        ui.allocate_ui_with_layout(egui::vec2(width, 30.0), egui::Layout::left_to_right(egui::Align::Center), |ui| {
            ui.label(RichText::new(ph::APP_WINDOW).size(16.0).color(t::p().text_muted));
            ui.label(RichText::new(shown.trim_end_matches(".exe").trim_end_matches(".EXE")).size(13.5));
            if active {
                ui.label(RichText::new(format!("{} играет", ph::SPEAKER_HIGH)).size(11.5).color(t::p().text_muted));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if kit::switch(ui, &mut on).on_hover_text("Включено — звук этой программы не попадёт в трансляцию экрана").changed() {
                    if on {
                        list.push(key.clone());
                    } else {
                        list.retain(|x| *x != key);
                    }
                    changed = true;
                }
            });
        });
    }
    });
    ui.add_space(2.0);
    if quiet > 0 || apps.show_all {
        let text = if apps.show_all { "Только играющие".to_string() } else { format!("Ещё {quiet} {} со звуком", programs(quiet)) };
        if ui
            .add(egui::Label::new(RichText::new(text).size(12.5).color(t::p().text_muted)).sense(egui::Sense::click()))
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .clicked()
        {
            apps.show_all = !apps.show_all;
        }
    }
    if ui
        .add(egui::Label::new(RichText::new(format!("{} Добавить программу…", ph::PLUS)).size(13.0).color(t::p().accent_hover)).sense(egui::Sense::click()))
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text("Выбрать .exe — даже если программа сейчас не запущена")
        .clicked()
    {
        apps.pick_exe(ui.ctx());
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_text_counts() {
        assert_eq!(link_text(0), "Исключить программы…");
        assert_eq!(link_text(1), format!("Без 1 программы {}", ph::CARET_DOWN));
        assert_eq!(link_text(3), format!("Без 3 программ {}", ph::CARET_DOWN));
        assert_eq!(link_text(21), format!("Без 21 программы {}", ph::CARET_DOWN));
    }

    #[test]
    fn plural() {
        assert_eq!([1, 2, 5, 11, 12, 21, 22, 25].map(programs), ["программа", "программы", "программ", "программ", "программ", "программа", "программы", "программ"]);
    }
}
