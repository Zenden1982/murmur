//! Панель GIF: поиск и сетка превью. Данные — только через наш сервер (прокси Klipy), сервис клиентов не видит.

use std::collections::HashMap;
use std::sync::mpsc as std_mpsc;

use eframe::egui::{self, Align2, ColorImage, CornerRadius, Frame, Id, Key, Margin, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, TextEdit, TextureHandle, TextureOptions, Ui, vec2};
use egui_phosphor::regular as ph;
use proto::{GifItem, GifPage};

use crate::api::Api;
use crate::theme as t;

const W: f32 = 372.0;
const COLS: usize = 3;

/// Сколько байт кадров анимированных превью держим (RGBA); одно превью — не больше четверти.
const ANIM_BUDGET: usize = 48 * 1024 * 1024;

enum Ev {
    Page { epoch: u64, page: u32, res: Result<GifPage, String> },
    Thumb { url: String, res: Result<ColorImage, String> },
    Anim { key: String, res: Result<(Vec<ColorImage>, Vec<u32>), String> },
}

/// Кадры анимированного превью в размере ячейки.
enum Anim {
    Loading,
    Ready { frames: Vec<TextureHandle>, delays: Vec<u32>, bytes: usize },
    Failed,
}

enum Thumb {
    Loading,
    Ready(TextureHandle),
    Failed,
}

pub enum GifOut {
    None,
    Close,
    Picked(GifItem),
}

pub struct GifPicker {
    rt: tokio::runtime::Handle,
    ctx: egui::Context,
    api: Api,
    tx: std_mpsc::Sender<Ev>,
    rx: std_mpsc::Receiver<Ev>,
    query: String,
    /// Запрос, по которому сейчас показаны результаты.
    searched: String,
    items: Vec<GifItem>,
    has_next: bool,
    page: u32,
    loading: bool,
    error: Option<String>,
    thumbs: HashMap<String, Thumb>,
    /// Анимированные превью видимых ячеек (ключ — адрес@размер).
    anims: HashMap<String, Anim>,
    anim_lru: crate::anim_img::Lru,
    /// Настройка «Анимировать GIF в чате».
    animate: bool,
    /// Номер поиска: ответы на устаревшие запросы отбрасываются.
    epoch: u64,
    started: bool,
    focused: bool,
}

impl GifPicker {
    pub fn new(rt: tokio::runtime::Handle, ctx: egui::Context, api: Api) -> Self {
        let (tx, rx) = std_mpsc::channel();
        Self {
            rt,
            ctx,
            api,
            tx,
            rx,
            query: String::new(),
            searched: String::new(),
            items: Vec::new(),
            has_next: false,
            page: 0,
            loading: false,
            error: None,
            thumbs: HashMap::new(),
            anims: HashMap::new(),
            anim_lru: crate::anim_img::Lru::new(ANIM_BUDGET),
            animate: true,
            epoch: 0,
            started: false,
            focused: false,
        }
    }

    pub fn set_api(&mut self, api: Api) {
        self.api = api;
        self.reset();
    }

    /// Панель открывают заново: чистый запрос, популярное.
    pub fn reset(&mut self) {
        let (tx, rx) = std_mpsc::channel();
        self.tx = tx;
        self.rx = rx;
        self.query.clear();
        self.searched.clear();
        self.items.clear();
        self.has_next = false;
        self.page = 0;
        self.loading = false;
        self.error = None;
        self.thumbs.clear();
        self.anims.clear();
        self.anim_lru.clear();
        self.epoch += 1;
        self.started = false;
        self.focused = false;
    }

    fn fetch(&mut self, page: u32) {
        self.loading = true;
        self.error = None;
        let (api, q, epoch, tx, ctx) = (self.api.clone(), self.searched.clone(), self.epoch, self.tx.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            let res = api.gifs(&q, page).await.map_err(|e| e.message);
            let _ = tx.send(Ev::Page { epoch, page, res });
            ctx.request_repaint();
        });
    }

    fn request_thumb(&mut self, url: &str) {
        if self.thumbs.contains_key(url) {
            return;
        }
        self.thumbs.insert(url.to_string(), Thumb::Loading);
        let (api, url, tx, ctx) = (self.api.clone(), url.to_string(), self.tx.clone(), self.ctx.clone());
        self.rt.spawn(async move {
            let res = match api.gif_media(&url).await {
                Ok(bytes) => tokio::task::spawn_blocking(move || {
                    let img = image::load_from_memory(&bytes).map_err(|e| e.to_string())?.to_rgba8();
                    Ok(ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw()))
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string())),
                Err(e) => Err(e.message),
            };
            let _ = tx.send(Ev::Thumb { url, res });
            ctx.request_repaint();
        });
    }

    pub fn set_animate(&mut self, on: bool) {
        self.animate = on;
    }

    /// Текущий кадр анимированного превью `url` размером `px` (в пикселях); None — пока нет (рисуется неподвижное).
    fn anim_frame(&mut self, url: &str, px: [u32; 2]) -> Option<TextureHandle> {
        if !self.animate || !crate::anim::window_active(&self.ctx) {
            return None;
        }
        let key = format!("{url}@{}x{}", px[0], px[1]);
        match self.anims.get(&key) {
            Some(Anim::Ready { frames, delays, bytes }) => {
                let t_ms = (self.ctx.input(|i| i.time) * 1000.0) as u64;
                let (i, next) = crate::anim_img::frame_at(delays, t_ms);
                let tex = frames.get(i).cloned();
                let nbytes = *bytes;
                self.ctx.request_repaint_after(std::time::Duration::from_millis(next.max(16) as u64));
                let frame = self.ctx.cumulative_frame_nr();
                for gone in self.anim_lru.touch_at(&key, nbytes, frame) {
                    self.anims.remove(&gone);
                }
                tex
            }
            Some(_) => None,
            None => {
                self.anims.insert(key.clone(), Anim::Loading);
                let (api, url, tx, ctx) = (self.api.clone(), url.to_string(), self.tx.clone(), self.ctx.clone());
                self.rt.spawn(async move {
                    let res = match api.gif_media(&url).await {
                        Ok(bytes) => tokio::task::spawn_blocking(move || crate::anim_img::decode_frames_capped(&bytes, px[0], px[1], false, ANIM_BUDGET / 4))
                            .await
                            .unwrap_or_else(|e| Err(e.to_string())),
                        Err(e) => Err(e.message),
                    };
                    let _ = tx.send(Ev::Anim { key, res });
                    ctx.request_repaint();
                });
                None
            }
        }
    }

    fn poll(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Ev::Page { epoch, page, res } if epoch == self.epoch => {
                    self.loading = false;
                    match res {
                        Ok(p) => {
                            if page <= 1 {
                                self.items.clear();
                            }
                            self.page = page;
                            self.has_next = p.has_next && !p.items.is_empty();
                            for i in p.items {
                                if !self.items.iter().any(|x| x.id == i.id) {
                                    self.items.push(i);
                                }
                            }
                        }
                        Err(e) => self.error = Some(e),
                    }
                }
                Ev::Page { .. } => {}
                Ev::Thumb { url, res } => {
                    let th = match res {
                        Ok(ci) => Thumb::Ready(self.ctx.load_texture(format!("gif-{url}"), ci, TextureOptions::LINEAR)),
                        Err(_) => Thumb::Failed,
                    };
                    self.thumbs.insert(url, th);
                }
                Ev::Anim { key, res } => {
                    let a = match res {
                        Ok((frames, delays)) => {
                            let bytes = frames.iter().map(|f| f.pixels.len() * 4).sum();
                            let frames = frames.into_iter().enumerate().map(|(i, f)| self.ctx.load_texture(format!("gifp-{key}-{i}"), f, TextureOptions::LINEAR)).collect();
                            Anim::Ready { frames, delays, bytes }
                        }
                        Err(_) => Anim::Failed,
                    };
                    self.anims.insert(key, a);
                }
            }
        }
    }

    fn search_now(&mut self) {
        self.searched = self.query.split_whitespace().collect::<Vec<_>>().join(" ");
        self.epoch += 1;
        self.items.clear();
        self.page = 0;
        self.has_next = false;
        self.fetch(1);
    }

    /// Панель в точке `pos`; `keep` — кнопка-открывашка (клик по ней не закрывает панель).
    pub fn show(&mut self, ctx: &egui::Context, id: Id, pos: Pos2, pivot: Align2, keep: Rect) -> GifOut {
        self.poll();
        if !self.started {
            self.started = true;
            self.search_now();
        }
        let mut out = GifOut::None;
        let area = egui::Area::new(id).order(egui::Order::Foreground).pivot(pivot).fixed_pos(pos).show(ctx, |ui| {
            Frame::new().fill(t::p().surface).stroke(Stroke::new(1.0, t::p().border)).corner_radius(CornerRadius::same(12)).inner_margin(Margin::same(8)).show(ui, |ui| {
                ui.set_width(W);
                // Запрос уходит на сервер только по Enter или кнопке «Найти» — не на каждую набранную букву.
                let mut go = false;
                ui.horizontal(|ui| {
                    const H: f32 = 34.0;
                    let edit = ui.add_sized(
                        vec2(W - H - ui.spacing().item_spacing.x, H),
                        TextEdit::singleline(&mut self.query)
                            .margin(Margin::symmetric(10, 8))
                            .vertical_align(egui::Align::Center)
                            .hint_text(RichText::new("Найти GIF…").color(t::p().text_muted)),
                    );
                    let btn = ui
                        .add_sized(vec2(H, H), egui::Button::new(RichText::new(ph::MAGNIFYING_GLASS).size(15.0)).corner_radius(CornerRadius::same(8)))
                        .on_hover_text("Найти (Enter)");
                    if !self.focused {
                        edit.request_focus();
                        self.focused = true;
                    }
                    if edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        go = true;
                        edit.request_focus();
                    }
                    if btn.clicked() {
                        go = true;
                    }
                });
                if go {
                    let q = self.query.split_whitespace().collect::<Vec<_>>().join(" ");
                    if q != self.searched || self.error.is_some() {
                        self.search_now();
                    }
                }
                ui.add_space(6.0);
                if let Some(i) = self.grid(ui) {
                    out = GifOut::Picked(i);
                }
                ui.add_space(4.0);
                ui.label(RichText::new("Поиск: KLIPY · запросы идут через ваш сервер").size(11.5).color(t::p().text_muted));
            });
        });
        let outside = ctx.input(|i| i.pointer.any_click()) && !area.response.hovered() && !ctx.pointer_hover_pos().is_some_and(|p| keep.contains(p) || area.response.rect.contains(p));
        if matches!(out, GifOut::None) && (outside || ctx.input(|i| i.key_pressed(Key::Escape))) {
            out = GifOut::Close;
        }
        out
    }

    fn grid(&mut self, ui: &mut Ui) -> Option<GifItem> {
        let mut picked = None;
        let height = 340.0;
        if let Some(e) = &self.error {
            ui.allocate_ui(vec2(W, height), |ui| {
                ui.add_space(12.0);
                ui.label(RichText::new(e).size(13.5).color(t::p().danger_text));
            });
            return None;
        }
        if self.items.is_empty() && !self.loading {
            ui.allocate_ui(vec2(W, height), |ui| {
                ui.add_space(12.0);
                ui.label(RichText::new("Ничего не нашлось").size(13.5).color(t::p().text_muted));
            });
            return None;
        }
        let col_w = (W - 8.0 * (COLS as f32 - 1.0)) / COLS as f32 - 2.0;
        let items = self.items.clone();
        let out = egui::ScrollArea::vertical().id_salt("gif-scroll").max_height(height).min_scrolled_height(height).auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                for c in 0..COLS {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 6.0;
                        for it in items.iter().skip(c).step_by(COLS) {
                            let ratio = if it.width > 0 { it.height as f32 / it.width as f32 } else { 1.0 };
                            let size = vec2(col_w, (col_w * ratio).clamp(40.0, 220.0));
                            let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
                            let visible = ui.is_rect_visible(rect);
                            if visible {
                                self.request_thumb(&it.thumb);
                            }
                            let r = CornerRadius::same(8);
                            // превью играет, когда видно и уже есть неподвижная картинка (она — пока грузятся кадры)
                            let anim = if visible && it.preview != it.thumb && matches!(self.thumbs.get(&it.thumb), Some(Thumb::Ready(_))) {
                                let ppp = ui.ctx().pixels_per_point();
                                self.anim_frame(&it.preview, [(size.x * ppp).round().max(1.0) as u32, (size.y * ppp).round().max(1.0) as u32])
                            } else {
                                None
                            };
                            match (anim, self.thumbs.get(&it.thumb)) {
                                (Some(tex), _) => {
                                    egui::Image::new(&tex).corner_radius(r).fit_to_exact_size(size).paint_at(ui, rect);
                                }
                                (None, Some(Thumb::Ready(tex))) => {
                                    egui::Image::new(tex).corner_radius(r).fit_to_exact_size(size).paint_at(ui, rect);
                                }
                                _ => {
                                    ui.painter().rect_filled(rect, r, t::p().surface_2);
                                }
                            }
                            if resp.hovered() {
                                ui.painter().rect_stroke(rect, r, Stroke::new(2.0, t::p().accent), StrokeKind::Inside);
                            }
                            let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                            if !it.title.is_empty() {
                                resp.clone().on_hover_text(&it.title);
                            }
                            if resp.clicked() {
                                picked = Some(it.clone());
                            }
                        }
                    });
                }
            });
            if self.loading {
                ui.add_space(6.0);
                ui.vertical_centered(|ui| {
                    ui.add(egui::Spinner::new().size(18.0).color(t::p().text_muted));
                });
            }
        });
        let near_end = out.state.offset.y + out.inner_rect.height() >= out.content_size.y - 120.0;
        if near_end && self.has_next && !self.loading && !self.items.is_empty() {
            let next = self.page + 1;
            self.fetch(next);
        }
        picked
    }
}
