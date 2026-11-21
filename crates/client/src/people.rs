//! Люди: отображаемые имена и аватары (фаза 9.1, минимум). Кэш пользователей (участники серверов + события
//! `user_updated`/`member_upserted`) и текстуры аватаров (`GET /images/{key}`, круглые, в памяти).

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::mpsc as std_mpsc;

use eframe::egui::{self, ColorImage, Pos2, TextureHandle, TextureOptions};
use proto::{Author, PublicUser, User};

use crate::api::Api;

/// Сторона текстуры аватара (пикселей): хватает на плитку комнаты (до ~92 логических точек при 150 %).
const TEX_SIDE: u32 = 160;

enum Tex {
    Loading,
    Ready(TextureHandle),
    Failed,
}

enum Ev {
    Image { key: String, res: Result<ColorImage, String> },
}

pub struct People {
    rt: tokio::runtime::Handle,
    ctx: egui::Context,
    api: Api,
    tx: std_mpsc::Sender<Ev>,
    rx: std_mpsc::Receiver<Ev>,
    users: HashMap<i64, PublicUser>,
    tex: RefCell<HashMap<String, Tex>>,
}

pub fn public(u: &User) -> PublicUser {
    PublicUser { id: u.id, name: u.name.clone(), display_name: u.display_name.clone(), avatar: u.avatar.clone() }
}

pub fn from_author(a: &Author) -> PublicUser {
    PublicUser { id: a.id, name: a.name.clone(), display_name: a.display_name.clone(), avatar: a.avatar.clone() }
}

impl People {
    pub fn new(rt: tokio::runtime::Handle, ctx: egui::Context, api: Api) -> Self {
        let (tx, rx) = std_mpsc::channel();
        Self { rt, ctx, api, tx, rx, users: HashMap::new(), tex: RefCell::new(HashMap::new()) }
    }

    /// Новый сервер/сессия: всё забыть (старые задачи пишут в закрытый канал).
    pub fn set_api(&mut self, api: Api) {
        self.api = api;
        let (tx, rx) = std_mpsc::channel();
        self.tx = tx;
        self.rx = rx;
        self.users.clear();
        self.tex.borrow_mut().clear();
    }

    pub fn learn(&mut self, u: PublicUser) {
        self.users.insert(u.id, u);
    }

    pub fn poll(&mut self) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Ev::Image { key, res } => {
                    let tex = match res {
                        Ok(ci) => Tex::Ready(self.ctx.load_texture(format!("avatar-{key}"), ci, TextureOptions::LINEAR)),
                        Err(e) => {
                            log::warn!("аватар {key}: {e}");
                            Tex::Failed
                        }
                    };
                    self.tex.borrow_mut().insert(key, tex);
                }
            }
        }
    }

    pub fn user(&self, id: i64) -> Option<&PublicUser> {
        self.users.get(&id)
    }

    /// Человек по identity LiveKit (= id пользователя).
    pub fn by_identity(&self, identity: &str) -> Option<&PublicUser> {
        identity.parse::<i64>().ok().and_then(|id| self.user(id))
    }

    /// Свежие данные из кэша, иначе — как пришло (автор сообщения).
    pub fn resolve(&self, fallback: PublicUser) -> PublicUser {
        self.users.get(&fallback.id).cloned().unwrap_or(fallback)
    }

    /// Текстура аватара; первый запрос — загрузка в фоне.
    pub fn texture(&self, key: &str) -> Option<TextureHandle> {
        let mut map = self.tex.borrow_mut();
        match map.get(key) {
            Some(Tex::Ready(t)) => return Some(t.clone()),
            Some(_) => return None,
            None => {}
        }
        map.insert(key.to_string(), Tex::Loading);
        let (api, tx, ctx, key) = (self.api.clone(), self.tx.clone(), self.ctx.clone(), key.to_string());
        self.rt.spawn(async move {
            let res = match api.image(&key).await {
                Ok(bytes) => tokio::task::spawn_blocking(move || round_image(&bytes, TEX_SIDE))
                    .await
                    .unwrap_or_else(|e| Err(e.to_string())),
                Err(e) => Err(e.message),
            };
            let _ = tx.send(Ev::Image { key, res });
            ctx.request_repaint();
        });
        None
    }

    /// Прямоугольная картинка (баннер сервера) по ключу, без скругления; первый запрос — загрузка в фоне.
    pub fn picture(&self, key: &str) -> Option<TextureHandle> {
        let id = format!("rect:{key}");
        let mut map = self.tex.borrow_mut();
        match map.get(&id) {
            Some(Tex::Ready(t)) => return Some(t.clone()),
            Some(_) => return None,
            None => {}
        }
        map.insert(id.clone(), Tex::Loading);
        let (api, tx, ctx, key) = (self.api.clone(), self.tx.clone(), self.ctx.clone(), key.to_string());
        self.rt.spawn(async move {
            let res = match api.image(&key).await {
                Ok(bytes) => tokio::task::spawn_blocking(move || rect_image(&bytes, 960)).await.unwrap_or_else(|e| Err(e.to_string())),
                Err(e) => Err(e.message),
            };
            let _ = tx.send(Ev::Image { key: id, res });
            ctx.request_repaint();
        });
        None
    }

    /// Текстура аватара человека, если уже загружена.
    pub fn texture_of(&self, u: &PublicUser) -> Option<egui::TextureId> {
        u.avatar.as_deref().and_then(|k| self.texture(k)).map(|t| t.id())
    }

    /// Аватар: картинка в круге, а пока её нет (или нет вовсе) — буква на цветном круге.
    pub fn paint(&self, painter: &egui::Painter, center: Pos2, r: f32, u: &PublicUser, glow: f32, dim: bool) {
        let tex = u.avatar.as_deref().and_then(|k| self.texture(k));
        paint_avatar(painter, center, r, avatar_name(u), tex.as_ref(), glow, dim);
    }
}

/// Имя, по которому рисуется аватар (буква и цвет) — везде одно: отображаемое, иначе логин.
pub fn avatar_name(u: &PublicUser) -> &str {
    u.shown_name()
}

pub fn paint_avatar(painter: &egui::Painter, center: Pos2, r: f32, name: &str, tex: Option<&TextureHandle>, glow: f32, dim: bool) {
    let who = crate::kit::AvatarInput { name, texture: tex.map(|t| t.id()) };
    crate::kit::avatar(painter, center, r, &who, glow, crate::kit::PeerBadge::None, dim);
}

/// Картинка как есть, не шире `max_w`.
pub fn rect_image(bytes: &[u8], max_w: u32) -> Result<ColorImage, String> {
    let img = image::load_from_memory(bytes).map_err(|e| format!("не картинка: {e}"))?;
    let img = if img.width() > max_w { img.resize(max_w, u32::MAX, image::imageops::FilterType::Lanczos3) } else { img };
    let rgba = img.to_rgba8();
    Ok(ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw()))
}

/// Квадрат из середины картинки.
fn center_square(img: image::DynamicImage) -> image::DynamicImage {
    let (w, h) = (img.width(), img.height());
    let s = w.min(h);
    img.crop_imm((w - s) / 2, (h - s) / 2, s, s)
}

/// Картинка → круг `side`×`side` с мягким краем (прозрачные углы).
pub fn round_image(bytes: &[u8], side: u32) -> Result<ColorImage, String> {
    let img = image::load_from_memory(bytes).map_err(|e| format!("не картинка: {e}"))?;
    let sq = center_square(img).resize_exact(side, side, image::imageops::FilterType::Lanczos3).to_rgba8();
    let c = side as f32 / 2.0;
    let mut px = sq.into_raw();
    for y in 0..side {
        for x in 0..side {
            let d = ((x as f32 + 0.5 - c).powi(2) + (y as f32 + 0.5 - c).powi(2)).sqrt();
            let a = (c - d + 0.5).clamp(0.0, 1.0);
            let i = ((y * side + x) * 4 + 3) as usize;
            px[i] = (px[i] as f32 * a) as u8;
        }
    }
    Ok(ColorImage::from_rgba_unmultiplied([side as usize, side as usize], &px))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_fn(w, h, |x, _| if x < w / 2 { image::Rgba([255, 0, 0, 255]) } else { image::Rgba([0, 0, 255, 255]) });
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn round_has_transparent_corners() {
        let ci = round_image(&png(100, 100), 64).unwrap();
        assert_eq!(ci.size, [64, 64]);
        assert_eq!(ci.pixels[0].a(), 0);
        assert_eq!(ci.pixels[32 * 64 + 32].a(), 255);
    }

}

#[cfg(test)]
mod avatar_name_tests {
    use super::avatar_name;

    #[test]
    fn avatar_letter_from_shown_name() {
        let u = proto::PublicUser { id: 1, name: "kurigohan".into(), display_name: Some("Окабе".into()), avatar: None };
        assert_eq!(avatar_name(&u), "Окабе", "буква и цвет — по отображаемому имени, как в списке");
        let u = proto::PublicUser { display_name: None, ..u };
        assert_eq!(avatar_name(&u), "kurigohan");
    }
}
