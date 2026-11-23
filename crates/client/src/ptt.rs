//! Рация (push-to-talk): опрос состояния клавиши через GetAsyncKeyState каждые 10 мс.
//!
//! Не RegisterHotKey/global-hotkey: те «съедают» нажатие у других программ (V перестала бы печататься
//! в игре/чате). Опрос не мешает другим приложениям и понимает кнопки мыши (X1/X2).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc as std_mpsc, Arc};
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;

use crate::voice::Cmd;

#[derive(Clone)]
pub struct Ptt {
    key: Arc<AtomicU32>,
    enabled: Arc<AtomicBool>,
    capture: Arc<AtomicBool>,
}

impl Ptt {
    /// Запускает поток опроса. Захваченные клавиши (режим назначения) приходят в `captured`.
    pub fn start(
        key: u32,
        enabled: bool,
        voice: UnboundedSender<Cmd>,
        captured: std_mpsc::Sender<u32>,
        repaint: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let ptt = Self {
            key: Arc::new(AtomicU32::new(key)),
            enabled: Arc::new(AtomicBool::new(enabled)),
            capture: Arc::new(AtomicBool::new(false)),
        };
        let p = ptt.clone();
        std::thread::Builder::new()
            .name("ptt".into())
            .spawn(move || {
                let mut held = false;
                let mut prev_down = [false; 256];
                loop {
                    std::thread::sleep(Duration::from_millis(10));
                    if p.capture.load(Ordering::Relaxed) {
                        if held {
                            // Клавишу назначают, пока рация зажата: отпускаем, иначе микрофон останется открытым.
                            held = false;
                            if voice.send(Cmd::PttHeld(false)).is_err() {
                                break;
                            }
                        }
                        if let Some(vk) = scan_new_press(&mut prev_down) {
                            p.capture.store(false, Ordering::Relaxed);
                            let _ = captured.send(vk);
                            repaint();
                        }
                        continue;
                    }
                    let want = p.enabled.load(Ordering::Relaxed) && is_down(p.key.load(Ordering::Relaxed));
                    if want != held {
                        held = want;
                        if voice.send(Cmd::PttHeld(held)).is_err() {
                            break;
                        }
                        repaint();
                    }
                }
            })
            .expect("ptt thread");
        ptt
    }

    pub fn set_key(&self, vk: u32) {
        self.key.store(vk, Ordering::Relaxed);
    }

    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
    }

    /// Следующее нажатие (кроме левой кнопки мыши) станет новой клавишей рации; Esc — отмена (vk 0x1B).
    pub fn begin_capture(&self) {
        self.capture.store(true, Ordering::Relaxed);
    }

    pub fn cancel_capture(&self) {
        self.capture.store(false, Ordering::Relaxed);
    }

    pub fn capturing(&self) -> bool {
        self.capture.load(Ordering::Relaxed)
    }
}

/// Пока у нас кто-то управляет (хуки следят) — только настоящие нажатия, иначе удалённо введённая клавиша включит рацию.
fn pick_down(watched: bool, real: bool, async_state: bool) -> bool {
    if watched { real } else { async_state }
}

#[cfg(windows)]
fn is_down(vk: u32) -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    if vk == 0 {
        return false;
    }
    let watched = crate::remote::hooks::host_watch_active();
    pick_down(watched, watched && crate::remote::hooks::real_key_down(vk), unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000 != 0)
}

#[cfg(not(windows))]
fn is_down(_vk: u32) -> bool {
    false
}

/// Ищет клавишу, которая только что нажата (была отпущена на прошлом опросе).
fn scan_new_press(prev: &mut [bool; 256]) -> Option<u32> {
    let mut found = None;
    for vk in 1u32..=0xFE {
        let down = is_down(vk);
        let was = prev[vk as usize];
        prev[vk as usize] = down;
        // 0x01 — левая кнопка мыши (ею нажимают саму кнопку назначения);
        // 0x10..0x12 — общие Shift/Ctrl/Alt: берём левый/правый вариант.
        if down && !was && found.is_none() && !matches!(vk, 0x01 | 0x10 | 0x11 | 0x12) {
            found = Some(vk);
        }
    }
    found
}

/// Человекочитаемое имя клавиши.
pub fn key_name(vk: u32) -> String {
    match vk {
        0x02 => "ПКМ".into(),
        0x04 => "Колесо".into(),
        0x05 => "Мышь 4".into(),
        0x06 => "Мышь 5".into(),
        0x08 => "Backspace".into(),
        0x09 => "Tab".into(),
        0x0D => "Enter".into(),
        0x13 => "Pause".into(),
        0x14 => "Caps Lock".into(),
        0x1B => "Esc".into(),
        0x20 => "Пробел".into(),
        0x2D => "Insert".into(),
        0x2E => "Delete".into(),
        0x24 => "Home".into(),
        0x23 => "End".into(),
        0x21 => "PgUp".into(),
        0x22 => "PgDn".into(),
        0x25 => "←".into(),
        0x26 => "↑".into(),
        0x27 => "→".into(),
        0x28 => "↓".into(),
        0x30..=0x39 | 0x41..=0x5A => char::from_u32(vk).map(String::from).unwrap_or_default(),
        0x60..=0x69 => format!("Num {}", vk - 0x60),
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        0xA0 => "Левый Shift".into(),
        0xA1 => "Правый Shift".into(),
        0xA2 => "Левый Ctrl".into(),
        0xA3 => "Правый Ctrl".into(),
        0xA4 => "Левый Alt".into(),
        0xA5 => "Правый Alt".into(),
        0xC0 => "~".into(),
        0xBA => ";".into(),
        0xBB => "=".into(),
        0xBC => ",".into(),
        0xBD => "-".into(),
        0xBE => ".".into(),
        0xBF => "/".into(),
        0xDB => "[".into(),
        0xDC => "\\".into(),
        0xDD => "]".into(),
        0xDE => "'".into(),
        _ => format!("Клавиша {vk:#04X}"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn remote_keys_dont_press_ptt_while_watched() {
        // у стримера кто-то управляет: клавиши, введённые удалённо (видны GetAsyncKeyState), рацию не жмут
        assert!(!super::pick_down(true, false, true));
        assert!(super::pick_down(true, true, false));
        assert!(super::pick_down(false, false, true));
    }

    #[test]
    fn names() {
        assert_eq!(super::key_name(0x56), "V");
        assert_eq!(super::key_name(0x05), "Мышь 4");
        assert_eq!(super::key_name(0x74), "F5");
    }
}
