//! Просмотр трансляций (фаза 6): входящее видео → RGBA (libyuv) в фоне → текстура egui.
//!
//! На каждую просматриваемую трансляцию — своя tokio-задача: `NativeVideoStream` с очередью в один кадр
//! (только последний, без накопления задержки), конвертация I420 → RGBA прямо в пиксели `ColorImage`,
//! загрузка в ту же текстуру (`set_partial` — без пересоздания GPU-текстуры). UI берёт готовую текстуру.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, ColorImage, TextureHandle, TextureId, TextureOptions};
use futures_util::StreamExt;
use livekit::prelude::RemoteVideoTrack;
use livekit::webrtc::video_frame::native::VideoFrameBufferExt;
use livekit::webrtc::video_frame::{VideoBuffer, VideoFormatType};
use livekit::webrtc::video_stream::native::NativeVideoStream;

/// Кадр трансляции для отрисовки.
#[derive(Clone, Copy)]
pub struct Frame {
    pub texture: TextureId,
    /// Размер видео в пикселях.
    pub size: [usize; 2],
    pub fps: f32,
    /// Давно нет новых кадров (стример свернул окно, сеть): показать подсказку.
    pub stale: bool,
}

struct Slot {
    texture: TextureHandle,
    size: [usize; 2],
    fps: f32,
    last: Instant,
}

/// Общий склад текстур трансляций: пишут задачи приёма, читает UI.
#[derive(Clone)]
pub struct VideoHub {
    ctx: egui::Context,
    slots: Arc<Mutex<HashMap<String, Slot>>>,
}

impl VideoHub {
    pub fn new(ctx: &egui::Context) -> Self {
        Self { ctx: ctx.clone(), slots: Default::default() }
    }

    pub fn frame(&self, identity: &str) -> Option<Frame> {
        let slots = self.slots.lock().unwrap();
        let s = slots.get(identity)?;
        Some(Frame { texture: s.texture.id(), size: s.size, fps: s.fps, stale: s.last.elapsed() > Duration::from_secs(3) })
    }

    pub fn remove(&self, identity: &str) {
        self.slots.lock().unwrap().remove(identity);
    }

    pub fn clear(&self) {
        self.slots.lock().unwrap().clear();
    }

    /// Кладёт кадр: тот же размер — обновление существующей текстуры, иначе — новая.
    fn put(&self, identity: &str, image: ColorImage, fps: f32) {
        let size = image.size;
        let mut slots = self.slots.lock().unwrap();
        match slots.get_mut(identity) {
            Some(s) if s.size == size => {
                s.texture.set_partial([0, 0], image, TextureOptions::LINEAR);
                s.fps = fps;
                s.last = Instant::now();
            }
            Some(s) => {
                s.texture.set(image, TextureOptions::LINEAR);
                s.size = size;
                s.fps = fps;
                s.last = Instant::now();
            }
            None => {
                let texture = self.ctx.load_texture(format!("stream-{identity}"), image, TextureOptions::LINEAR);
                slots.insert(identity.to_string(), Slot { texture, size, fps, last: Instant::now() });
            }
        }
        drop(slots);
        self.ctx.request_repaint();
    }

    /// Кладёт локальный кадр (превью своей камеры).
    pub fn put_i420(&self, key: &str, frame: &capture_win::I420Buffer) {
        if let Some(image) = to_image(frame) {
            self.put(key, image, 0.0);
        }
    }

    /// Задача приёма кадров одной трансляции (остановка — `abort()` хэндла).
    pub fn spawn_receiver(&self, identity: String, track: RemoteVideoTrack) -> tokio::task::JoinHandle<()> {
        let hub = self.clone();
        tokio::spawn(async move {
            // Очередь по умолчанию — один кадр: если UI/конвертация не успевают, старый кадр заменяется новым.
            let mut stream = NativeVideoStream::new(track.rtc_track());
            let mut fps = Fps::default();
            let mut warned = false;
            let mut last_at: Option<Instant> = None;
            while let Some(frame) = stream.next().await {
                if capture_win::diag::on() {
                    let now = Instant::now();
                    if let Some(t) = last_at
                        && now.duration_since(t) >= Duration::from_millis(400)
                    {
                        log::info!("ДИАГ зритель {identity}: кадров не было {} мс", now.duration_since(t).as_millis());
                    }
                    last_at = Some(now);
                }
                let image = tokio::task::block_in_place(|| to_image(frame.buffer.as_ref()));
                match image {
                    Some(image) => hub.put(&identity, image, fps.tick()),
                    None if !warned => {
                        warned = true;
                        log::warn!("трансляция {identity}: неподдерживаемый буфер {:?}", frame.buffer.buffer_type());
                    }
                    None => {}
                }
            }
            log::info!("трансляция {identity}: поток видео закончился");
        })
    }

    /// Отладка вёрстки (MURMUR_DEMO): синтетическая «картинка трансляции».
    #[cfg(debug_assertions)]
    pub fn put_demo(&self, identity: &str, t: f32) {
        // MURMUR_DEMO_SIZE=640x348 — другой размер кадра (проверка полей при несовпадении пропорций).
        let (w, h) = std::env::var("MURMUR_DEMO_SIZE")
            .ok()
            .and_then(|s| s.split_once('x').and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?))))
            .unwrap_or((640usize, 360usize));
        let mut px = vec![Color32::BLACK; w * h];
        for y in 0..h {
            for x in 0..w {
                let fx = x as f32 / w as f32;
                let fy = y as f32 / h as f32;
                let r = (40.0 + 120.0 * fx) as u8;
                let g = (30.0 + 60.0 * (fy + 0.2 * (t + fx * 6.0).sin()).clamp(0.0, 1.0)) as u8;
                let b = (60.0 + 140.0 * (1.0 - fy)) as u8;
                px[y * w + x] = Color32::from_rgb(r, g, b);
            }
        }
        self.put(identity, ColorImage::new([w, h], px), 30.0);
    }
}

/// I420 → RGBA (порядок байт R,G,B,A = libyuv «ABGR»), сразу в пиксели egui (`Color32` непрозрачный = RGBA).
fn to_image(buffer: &dyn VideoBuffer) -> Option<ColorImage> {
    let i420 = buffer.as_i420()?;
    let (w, h) = (i420.width() as usize, i420.height() as usize);
    if w == 0 || h == 0 {
        return None;
    }
    // calloc: без поэлементного заполнения (кадр всё равно целиком перезаписывается).
    let mut pixels: Vec<Color32> = bytemuck::zeroed_vec(w * h);
    let dst: &mut [u8] = bytemuck::cast_slice_mut(&mut pixels);
    i420.to_argb(VideoFormatType::ABGR, dst, (w * 4) as u32, w as i32, h as i32);
    Some(ColorImage::new([w, h], pixels))
}

/// Скользящая оценка к/с по интервалам между кадрами.
#[derive(Default)]
struct Fps {
    window_start: Option<Instant>,
    n: u32,
    value: f32,
}

impl Fps {
    fn tick(&mut self) -> f32 {
        let now = Instant::now();
        let start = *self.window_start.get_or_insert(now);
        self.n += 1;
        let dt = now.duration_since(start).as_secs_f32();
        if dt >= 1.0 {
            self.value = self.n as f32 / dt;
            self.n = 0;
            self.window_start = Some(now);
        }
        self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use livekit::webrtc::video_frame::I420Buffer;

    #[test]
    fn i420_to_rgba_colors() {
        // Чистый белый и «чёрный» в BT.601 limited: Y=235/16, U=V=128.
        let mut buf = I420Buffer::new(4, 2);
        {
            let (y, u, v) = buf.data_mut();
            y.fill(235);
            y[0] = 16;
            u.fill(128);
            v.fill(128);
        }
        let img = to_image(&buf).expect("i420");
        assert_eq!(img.size, [4, 2]);
        let p = img.pixels[1];
        assert!(p.r() > 245 && p.g() > 245 && p.b() > 245 && p.a() == 255, "{p:?}");
        let k = img.pixels[0];
        assert!(k.r() < 10 && k.g() < 10 && k.b() < 10 && k.a() == 255, "{k:?}");
    }

    #[test]
    fn i420_red_is_red() {
        // Красный BT.601 limited: Y=81, U=90, V=240 → порядок каналов не перепутан (R, а не B).
        let mut buf = I420Buffer::new(2, 2);
        {
            let (y, u, v) = buf.data_mut();
            y.fill(81);
            u.fill(90);
            v.fill(240);
        }
        let p = to_image(&buf).unwrap().pixels[0];
        assert!(p.r() > 200 && p.b() < 60, "{p:?}");
    }

    #[test]
    fn fps_estimate() {
        let mut f = Fps::default();
        let t0 = Instant::now();
        f.window_start = Some(t0 - Duration::from_millis(1000));
        f.n = 29;
        assert!((f.tick() - 30.0).abs() < 1.5);
    }
}
