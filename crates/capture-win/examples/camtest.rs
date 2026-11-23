//! Проверка камеры: список устройств и 3 секунды захвата (кадры, размер, к/с).
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use capture_win::{Event, I420Buffer, Sink};
use libwebrtc::video_frame::VideoBuffer;

struct S(AtomicU32);
impl Sink for S {
    fn frame(&self, f: &I420Buffer, _ts: i64) {
        if self.0.fetch_add(1, Ordering::Relaxed) == 0 {
            println!("первый кадр {}x{}", f.width(), f.height());
        }
    }
    fn event(&self, e: Event) {
        println!("событие: {e:?}");
    }
}

fn main() {
    let list = capture_win::camera::list_cameras();
    for c in &list {
        println!("камера: {} [{}]", c.name, c.id);
    }
    if list.is_empty() {
        println!("камер нет");
        return;
    }
    let s = Arc::new(S(AtomicU32::new(0)));
    match capture_win::camera::Camera::start(None, s.clone()) {
        Ok(cam) => {
            std::thread::sleep(Duration::from_secs(3));
            drop(cam);
            println!("кадров за 3 с: {}", s.0.load(Ordering::Relaxed));
        }
        Err(e) => println!("ошибка: {e}"),
    }
}
