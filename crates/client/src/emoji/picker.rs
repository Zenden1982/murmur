//! Панель эмодзи: поиск (по-русски и :кодам:), разделы, недавние.

use eframe::egui::{self, Align2, Color32, CornerRadius, Frame, Id, Key, Margin, Pos2, Rect, RichText, Sense, Stroke, TextEdit, Ui, pos2, vec2};
use egui_phosphor::regular as ph;

use super::{Entry, GROUPS, Group, catalog, entry, search};
use crate::theme as t;

pub const RECENT_MAX: usize = 32;
const COLS: usize = 8;
const CELL: f32 = 38.0;
const W: f32 = COLS as f32 * CELL + 16.0;

impl Group {
    pub fn title(&self) -> &'static str {
        match self {
            Group::Smileys => "Смайлы",
            Group::People => "Люди",
            Group::Animals => "Природа",
            Group::Food => "Еда",
            Group::Travel => "Места",
            Group::Activities => "Занятия",
            Group::Objects => "Предметы",
            Group::Symbols => "Символы",
            Group::Flags => "Флаги",
        }
    }
    pub fn icon(&self) -> &'static str {
        match self {
            Group::Smileys => ph::SMILEY,
            Group::People => ph::PERSON,
            Group::Animals => ph::PAW_PRINT,
            Group::Food => ph::HAMBURGER,
            Group::Travel => ph::AIRPLANE,
            Group::Activities => ph::SOCCER_BALL,
            Group::Objects => ph::LIGHTBULB,
            Group::Symbols => ph::HEART,
            Group::Flags => ph::FLAG,
        }
    }
}

/// Недавние: в начало, без повторов, не больше RECENT_MAX.
pub fn push_recent(list: &mut Vec<String>, e: &str) {
    list.retain(|x| x != e);
    list.insert(0, e.to_string());
    list.truncate(RECENT_MAX);
}

/// Вставить `s` в позицию `char_idx` (в символах; дальше конца — в конец). Возвращает позицию курсора после вставки.
pub fn insert_at(text: &mut String, char_idx: usize, s: &str) -> usize {
    let n = text.chars().count();
    let idx = char_idx.min(n);
    let byte = text.char_indices().nth(idx).map_or(text.len(), |(b, _)| b);
    text.insert_str(byte, s);
    idx + s.chars().count()
}

/// (недавние, строки сетки, начало каждого раздела)
type GridCache = (Vec<String>, Vec<Row>, Vec<(Option<Group>, usize)>);

#[derive(Default)]
pub struct Picker {
    query: String,
    /// Раздел, к которому прокрутить в этом кадре (None внутри — «Недавние»).
    jump: Option<Option<Group>>,
    hovered: Option<&'static str>,
    focused: bool,
    /// Строки сетки и метки разделов — пересчёт, только когда меняются недавние.
    cache: Option<GridCache>,
}

pub enum PickerOut {
    None,
    Picked(String),
    Close,
}

enum Row {
    Header(&'static str),
    Cells(Vec<&'static Entry>),
}

fn rows(recent: &[String]) -> (Vec<Row>, Vec<(Option<Group>, usize)>) {
    let mut out = Vec::new();
    let mut marks = Vec::new();
    let rec: Vec<&'static Entry> = recent.iter().filter_map(|e| entry(e)).collect();
    if !rec.is_empty() {
        marks.push((None, out.len()));
        out.push(Row::Header("Недавние"));
        out.extend(rec.chunks(COLS).map(|c| Row::Cells(c.to_vec())));
    }
    for g in GROUPS {
        let list: Vec<&'static Entry> = catalog().iter().filter(|e| e.group == g).collect();
        marks.push((Some(g), out.len()));
        out.push(Row::Header(g.title()));
        out.extend(list.chunks(COLS).map(|c| Row::Cells(c.to_vec())));
    }
    (out, marks)
}

impl Picker {
    /// Сбросить поиск и фокус (панель открывается заново).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Панель в точке `pos` с опорой `pivot`; `keep` — область, клик по которой не закрывает (кнопка-открывашка).
    pub fn show(&mut self, ctx: &egui::Context, id: Id, pos: Pos2, pivot: Align2, keep: Rect, recent: &[String]) -> PickerOut {
        let mut out = PickerOut::None;
        let area = egui::Area::new(id).order(egui::Order::Foreground).pivot(pivot).fixed_pos(pos).show(ctx, |ui| {
            Frame::new()
                .fill(t::p().surface)
                .stroke(Stroke::new(1.0, t::p().border))
                .corner_radius(CornerRadius::same(12))
                .inner_margin(Margin::same(8))
                .show(ui, |ui| {
                    ui.set_width(W);
                    let edit = ui.add(
                        TextEdit::singleline(&mut self.query)
                            .desired_width(W)
                            .hint_text(RichText::new(format!("{}  Найти эмодзи", ph::MAGNIFYING_GLASS)).color(t::p().text_muted)),
                    );
                    if !self.focused {
                        edit.request_focus();
                        self.focused = true;
                    }
                    if self.query.is_empty() {
                        self.tabs(ui, !recent.is_empty());
                    }
                    ui.add_space(4.0);
                    if let Some(e) = self.grid(ui, recent) {
                        out = PickerOut::Picked(e.to_string());
                    }
                    ui.separator();
                    self.footer(ui);
                });
        });
        let outside = ctx.input(|i| i.pointer.any_click())
            && !area.response.hovered()
            && !ctx.pointer_hover_pos().is_some_and(|p| keep.contains(p) || area.response.rect.contains(p));
        if matches!(out, PickerOut::None) && (outside || ctx.input(|i| i.key_pressed(Key::Escape))) {
            out = PickerOut::Close;
        }
        out
    }

    fn tabs(&mut self, ui: &mut Ui, has_recent: bool) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let mut tab = |ui: &mut Ui, icon: &str, tip: &str, g: Option<Group>| {
                let (r, resp) = ui.allocate_exact_size(vec2(W / 10.0 - 2.0, 26.0), Sense::click());
                let c = if resp.hovered() { t::p().text } else { t::p().text_muted };
                if resp.hovered() {
                    ui.painter().rect_filled(r, CornerRadius::same(6), t::p().row_hover);
                }
                ui.painter().text(r.center(), Align2::CENTER_CENTER, icon, t::font(16.0), c);
                if resp.on_hover_text(tip).clicked() {
                    self.jump = Some(g);
                }
            };
            if has_recent {
                tab(ui, ph::CLOCK_COUNTER_CLOCKWISE, "Недавние", None);
            }
            for g in GROUPS {
                tab(ui, g.icon(), g.title(), Some(g));
            }
        });
    }

    fn cell(&mut self, ui: &mut Ui, e: &'static Entry) -> bool {
        let (r, resp) = ui.allocate_exact_size(vec2(CELL, CELL), Sense::click());
        if resp.hovered() {
            ui.painter().rect_filled(r, CornerRadius::same(6), t::p().row_hover);
            self.hovered = Some(e.emoji);
        }
        if ui.is_rect_visible(r) && !super::paint::paint(ui.painter(), Rect::from_center_size(r.center(), vec2(26.0, 26.0)), e.emoji) {
            ui.painter().text(r.center(), Align2::CENTER_CENTER, e.emoji, t::font(22.0), Color32::WHITE);
        }
        resp.clicked()
    }

    fn grid(&mut self, ui: &mut Ui, recent: &[String]) -> Option<&'static str> {
        let mut picked = None;
        ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
        if !self.query.trim().is_empty() {
            let found = search(&self.query, 240);
            egui::ScrollArea::vertical().id_salt("emoji-search").max_height(300.0).min_scrolled_height(300.0).show(ui, |ui| {
                ui.set_width(W);
                if found.is_empty() {
                    ui.add_space(20.0);
                    ui.vertical_centered(|ui| ui.label(RichText::new("Ничего не нашлось").color(t::p().text_muted)));
                }
                for chunk in found.chunks(COLS) {
                    ui.horizontal(|ui| {
                        for e in chunk {
                            if self.cell(ui, e) {
                                picked = Some(e.emoji);
                            }
                        }
                    });
                }
            });
            if ui.input(|i| i.key_pressed(Key::Enter)) && let Some(e) = found.first() {
                picked = Some(e.emoji);
            }
            return picked;
        }
        if self.cache.as_ref().is_none_or(|(r, _, _)| r.as_slice() != recent) {
            let (rows, marks) = rows(recent);
            self.cache = Some((recent.to_vec(), rows, marks));
        }
        let (_, rows, marks) = self.cache.take().unwrap_or_default();
        let mut sa = egui::ScrollArea::vertical().id_salt("emoji-grid").max_height(300.0).min_scrolled_height(300.0);
        if let Some(g) = self.jump.take() && let Some((_, i)) = marks.iter().find(|(m, _)| *m == g) {
            sa = sa.vertical_scroll_offset(*i as f32 * CELL);
        }
        let n = rows.len();
        sa.show_rows(ui, CELL, n, |ui, range| {
            ui.set_width(W);
            for row in &rows[range] {
                match row {
                    Row::Header(title) => {
                        let (r, _) = ui.allocate_exact_size(vec2(W, CELL), Sense::hover());
                        ui.painter().text(pos2(r.left() + 4.0, r.bottom() - 8.0), Align2::LEFT_BOTTOM, *title, t::semibold(12.5), t::p().text_muted);
                    }
                    Row::Cells(cells) => {
                        ui.horizontal(|ui| {
                            for e in cells {
                                if self.cell(ui, e) {
                                    picked = Some(e.emoji);
                                }
                            }
                        });
                    }
                }
            }
        });
        self.cache = Some((recent.to_vec(), rows, marks));
        picked
    }

    fn footer(&mut self, ui: &mut Ui) {
        let (r, _) = ui.allocate_exact_size(vec2(W, 30.0), Sense::hover());
        let Some(e) = self.hovered.take().and_then(entry) else {
            ui.painter().text(pos2(r.left() + 4.0, r.center().y), Align2::LEFT_CENTER, "Shift+клик — выбрать несколько", t::font(12.0), t::p().text_muted);
            return;
        };
        super::paint::paint(ui.painter(), Rect::from_min_size(pos2(r.left() + 2.0, r.center().y - 13.0), vec2(26.0, 26.0)), e.emoji);
        let code = e.codes.first().map(|c| format!("  :{c}:")).unwrap_or_default();
        ui.painter().text(pos2(r.left() + 36.0, r.center().y), Align2::LEFT_CENTER, format!("{}{code}", e.ru_name), t::font(13.0), t::p().text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_recent_dedup_and_cap() {
        let mut r: Vec<String> = Vec::new();
        push_recent(&mut r, "🔥");
        push_recent(&mut r, "😂");
        push_recent(&mut r, "🔥");
        assert_eq!(r, ["🔥", "😂"]);
        for i in 0..40 {
            push_recent(&mut r, &format!("e{i}"));
        }
        assert_eq!(r.len(), RECENT_MAX);
        assert_eq!(r[0], "e39");
    }

    #[test]
    fn insert_at_cursor() {
        let mut s = String::from("аб");
        assert_eq!(insert_at(&mut s, 1, "🔥"), 2);
        assert_eq!(s, "а🔥б");
        assert_eq!(insert_at(&mut s, 99, "!"), 4);
        assert_eq!(s, "а🔥б!");
    }
}
