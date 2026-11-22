//! Что зажато у стримера по командам управляющего (чтобы всё отпустить), и какие клавиши не пересылать.

use std::collections::{BTreeMap, BTreeSet};

use super::proto::{Btn, InputEv};

const VK_TAB: u32 = 0x09;
const VK_SPACE: u32 = 0x20;
const VK_LWIN: u32 = 0x5B;
const VK_RWIN: u32 = 0x5C;
const VK_RCONTROL: u32 = 0xA3;

fn btn_key(b: Btn) -> u8 {
    b as u8
}

fn btn_from(k: u8) -> Btn {
    match k {
        0 => Btn::L,
        1 => Btn::R,
        2 => Btn::M,
        3 => Btn::X1,
        _ => Btn::X2,
    }
}

/// Нажатые по командам управляющего клавиши (скан-код, расширенная) и кнопки мыши (с местом нажатия).
#[derive(Default, Debug)]
pub struct Held {
    keys: BTreeSet<(u16, bool)>,
    buttons: BTreeMap<u8, (f32, f32)>,
}

impl Held {
    pub fn apply(&mut self, ev: &InputEv) {
        match *ev {
            InputEv::Key { sc, ext, down } => {
                if down {
                    self.keys.insert((sc, ext));
                } else {
                    self.keys.remove(&(sc, ext));
                }
            }
            InputEv::Button { b, down, x, y } => {
                if down {
                    self.buttons.insert(btn_key(b), (x, y));
                } else {
                    self.buttons.remove(&btn_key(b));
                }
            }
            InputEv::Wheel { .. } => {}
        }
    }

    pub fn any(&self) -> bool {
        !self.keys.is_empty() || !self.buttons.is_empty()
    }

    /// События «отпустить» для всего нажатого; после вызова ничего не зажато.
    pub fn release_all(&mut self) -> Vec<InputEv> {
        let mut out: Vec<InputEv> =
            std::mem::take(&mut self.keys).into_iter().map(|(sc, ext)| InputEv::Key { sc, ext, down: false }).collect();
        out.extend(
            std::mem::take(&mut self.buttons)
                .into_iter()
                .map(|(b, (x, y))| InputEv::Button { b: btn_from(b), down: false, x, y }),
        );
        out
    }
}

/// Куда идёт клавиша управляющего в режиме управления.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyRoute {
    /// Переслать стримеру (и не отдавать своей системе).
    Forward,
    /// Оставить у себя (Alt+Tab, Win, рация, Ctrl+Alt+Пробел).
    Local,
    /// Правый Ctrl — вернуться к указке.
    ReturnToPointer,
}

pub fn route_key(vk: u32, ctrl: bool, alt: bool, ptt_vk: u32) -> KeyRoute {
    if vk == VK_RCONTROL {
        return KeyRoute::ReturnToPointer;
    }
    if vk == VK_LWIN || vk == VK_RWIN || (vk == VK_TAB && alt) || vk == ptt_vk || is_panic_combo(vk, ctrl, alt) {
        return KeyRoute::Local;
    }
    KeyRoute::Forward
}

/// Ctrl+Alt+Пробел — у стримера «забрать всё».
pub fn is_panic_combo(vk: u32, ctrl: bool, alt: bool) -> bool {
    vk == VK_SPACE && ctrl && alt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn held_release_all_releases_keys_and_buttons() {
        let mut h = Held::default();
        h.apply(&InputEv::Key { sc: 30, ext: false, down: true });
        h.apply(&InputEv::Button { b: Btn::L, down: true, x: 0.2, y: 0.3 });
        let r = h.release_all();
        assert_eq!(r.len(), 2);
        assert!(r.contains(&InputEv::Key { sc: 30, ext: false, down: false }));
        assert!(r.iter().any(|e| matches!(e, InputEv::Button { b: Btn::L, down: false, .. })));
        assert!(h.release_all().is_empty());
    }

    #[test]
    fn held_up_removes() {
        let mut h = Held::default();
        h.apply(&InputEv::Key { sc: 30, ext: true, down: true });
        h.apply(&InputEv::Key { sc: 30, ext: true, down: false });
        h.apply(&InputEv::Button { b: Btn::R, down: true, x: 0.0, y: 0.0 });
        h.apply(&InputEv::Button { b: Btn::R, down: false, x: 0.0, y: 0.0 });
        assert!(h.release_all().is_empty());
        assert!(!h.any());
    }

    #[test]
    fn route_exemptions() {
        const PTT: u32 = 0x56; // V
        assert!(matches!(route_key(0xA3, false, false, PTT), KeyRoute::ReturnToPointer));
        assert!(matches!(route_key(0x5B, false, false, PTT), KeyRoute::Local));
        assert!(matches!(route_key(0x5C, false, false, PTT), KeyRoute::Local));
        assert!(matches!(route_key(0x09, false, true, PTT), KeyRoute::Local));
        assert!(matches!(route_key(PTT, false, false, PTT), KeyRoute::Local));
        assert!(matches!(route_key(0x20, true, true, PTT), KeyRoute::Local));
        assert!(matches!(route_key(0x41, false, false, PTT), KeyRoute::Forward));
        assert!(matches!(route_key(0x09, false, false, PTT), KeyRoute::Forward));
        assert!(matches!(route_key(0x20, true, false, PTT), KeyRoute::Forward));
        // рация на кнопке мыши (0x05) не мешает клавиатуре
        assert!(matches!(route_key(0x41, false, false, 0x05), KeyRoute::Forward));
    }

    #[test]
    fn panic_combo() {
        assert!(is_panic_combo(0x20, true, true));
        assert!(!is_panic_combo(0x20, true, false));
        assert!(!is_panic_combo(0x41, true, true));
    }
}
