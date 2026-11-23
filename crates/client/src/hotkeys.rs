//! Глобальные горячие клавиши (микрофон, звук, выход из голоса, остановка трансляции).
//!
//! Как и рация (ptt.rs): опрос GetAsyncKeyState раз в 10 мс, без RegisterHotKey — нажатие не «съедается»
//! у других программ, понимаются боковые кнопки мыши. Команды идут прямо в голосовой поток, поэтому всё
//! работает и при свёрнутом окне.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

use crate::ptt::{self, is_down};
use crate::voice::Cmd;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Chord {
    pub vk: u32,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Action {
    ToggleMute,
    ToggleDeafen,
    Leave,
    StopStream,
}

impl Action {
    pub const ALL: [Action; 4] = [Action::ToggleMute, Action::ToggleDeafen, Action::Leave, Action::StopStream];

    pub fn label(self) -> &'static str {
        match self {
            Action::ToggleMute => "Микрофон вкл/выкл",
            Action::ToggleDeafen => "Звук вкл/выкл",
            Action::Leave => "Выйти из голосового канала",
            Action::StopStream => "Остановить трансляцию",
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Debug)]
#[serde(default)]
pub struct Bindings {
    pub mute: Option<Chord>,
    pub deafen: Option<Chord>,
    pub leave: Option<Chord>,
    pub stop_stream: Option<Chord>,
}

impl Bindings {
    pub fn get(&self, a: Action) -> Option<Chord> {
        match a {
            Action::ToggleMute => self.mute,
            Action::ToggleDeafen => self.deafen,
            Action::Leave => self.leave,
            Action::StopStream => self.stop_stream,
        }
    }

    pub fn set(&mut self, a: Action, c: Option<Chord>) {
        match a {
            Action::ToggleMute => self.mute = c,
            Action::ToggleDeafen => self.deafen = c,
            Action::Leave => self.leave = c,
            Action::StopStream => self.stop_stream = c,
        }
    }
}

fn is_modifier(vk: u32) -> bool {
    matches!(vk, 0x01 | 0x10..=0x12 | 0xA0..=0xA5)
}

/// Зажаты ли модификаторы (любой из левого/правого).
fn mods(down: &dyn Fn(u32) -> bool) -> (bool, bool, bool) {
    (down(0xA2) || down(0xA3), down(0xA4) || down(0xA5), down(0xA0) || down(0xA1))
}

/// Зажата клавиша сочетания и модификаторы совпадают ровно (Ctrl+Shift+M ≠ Ctrl+M).
pub fn chord_pressed(c: Chord, down: &dyn Fn(u32) -> bool) -> bool {
    down(c.vk) && mods(down) == (c.ctrl, c.alt, c.shift)
}

/// Имя занятого другого действия, если сочетание уже назначено (рация — без модификаторов).
pub fn find_conflict(b: &Bindings, ptt_key: u32, action: Action, c: Chord) -> Option<String> {
    if !c.ctrl && !c.alt && !c.shift && c.vk == ptt_key {
        return Some("Рация".into());
    }
    Action::ALL.into_iter().filter(|a| *a != action).find(|a| b.get(*a) == Some(c)).map(|a| a.label().to_string())
}

/// Записывает сочетание, если оно не занято.
pub fn apply_capture(b: &mut Bindings, ptt_key: u32, action: Action, c: Chord) -> Result<(), String> {
    if let Some(name) = find_conflict(b, ptt_key, action, c) {
        return Err(format!("Уже занято: {name}"));
    }
    b.set(action, Some(c));
    Ok(())
}

pub fn chord_name(c: Chord) -> String {
    let mut s = String::new();
    for (on, n) in [(c.ctrl, "Ctrl+"), (c.alt, "Alt+"), (c.shift, "Shift+")] {
        if on {
            s.push_str(n);
        }
    }
    s + &ptt::key_name(c.vk)
}

/// Фронт нажатия: true только на переходе «отпущено → зажато».
#[derive(Default)]
pub struct Edge(bool);

impl Edge {
    pub fn update(&mut self, now: bool) -> bool {
        let fire = now && !self.0;
        self.0 = now;
        fire
    }
}

#[derive(Default)]
pub struct TickState {
    edges: [Edge; 4],
}

/// Один опрос: какие действия сработали.
pub fn tick(st: &mut TickState, b: &Bindings, down: &dyn Fn(u32) -> bool) -> Vec<Action> {
    let mut out = Vec::new();
    for (i, a) in Action::ALL.into_iter().enumerate() {
        let now = b.get(a).is_some_and(|c| chord_pressed(c, down));
        if st.edges[i].update(now) {
            out.push(a);
        }
    }
    out
}

#[derive(Clone)]
pub struct Hotkeys {
    bindings: Arc<Mutex<Bindings>>,
    capture: Arc<AtomicBool>,
}

impl Hotkeys {
    /// Поток опроса. Захваченное сочетание (режим назначения) приходит в `captured`; Esc без модификаторов — отмена (vk 0x1B).
    pub fn start(
        bindings: Bindings,
        voice: UnboundedSender<Cmd>,
        captured: std_mpsc::Sender<Chord>,
        repaint: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let hk = Self { bindings: Arc::new(Mutex::new(bindings)), capture: Arc::new(AtomicBool::new(false)) };
        let h = hk.clone();
        std::thread::Builder::new()
            .name("hotkeys".into())
            .spawn(move || {
                let mut st = TickState::default();
                let mut prev_down = [false; 256];
                loop {
                    std::thread::sleep(Duration::from_millis(10));
                    if h.capture.load(Ordering::Relaxed) {
                        st = TickState::default();
                        if let Some(vk) = scan_press(&mut prev_down) {
                            let (ctrl, alt, shift) = mods(&is_down);
                            h.capture.store(false, Ordering::Relaxed);
                            let _ = captured.send(Chord { vk, ctrl, alt, shift });
                            repaint();
                        }
                        continue;
                    }
                    prev_down = [false; 256];
                    let b = h.bindings.lock().unwrap().clone();
                    for a in tick(&mut st, &b, &is_down) {
                        let cmd = match a {
                            Action::ToggleMute => Cmd::ToggleMute,
                            Action::ToggleDeafen => Cmd::ToggleDeafen,
                            Action::Leave => Cmd::Leave,
                            Action::StopStream => Cmd::StopStream,
                        };
                        if voice.send(cmd).is_err() {
                            return;
                        }
                        repaint();
                    }
                }
            })
            .expect("hotkeys thread");
        hk
    }

    pub fn set_bindings(&self, b: Bindings) {
        *self.bindings.lock().unwrap() = b;
    }

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

/// Первая только что нажатая не-модификаторная клавиша.
fn scan_press(prev: &mut [bool; 256]) -> Option<u32> {
    let mut found = None;
    for vk in 1u32..=0xFE {
        let down = is_down(vk);
        let was = prev[vk as usize];
        prev[vk as usize] = down;
        if down && !was && found.is_none() && !is_modifier(vk) {
            found = Some(vk);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(vk: u32, ctrl: bool, alt: bool, shift: bool) -> Chord {
        Chord { vk, ctrl, alt, shift }
    }
    fn held(keys: &'static [u32]) -> impl Fn(u32) -> bool {
        move |vk| keys.contains(&vk)
    }

    #[test]
    fn chord_exact_modifiers() {
        let ch = c(0x4D, true, false, false);
        assert!(chord_pressed(ch, &held(&[0x4D, 0xA2])));
        assert!(!chord_pressed(ch, &held(&[0x4D, 0xA2, 0xA0])));
        assert!(!chord_pressed(ch, &held(&[0x4D])));
    }

    #[test]
    fn chord_right_ctrl_counts() {
        assert!(chord_pressed(c(0x4D, true, false, false), &held(&[0x4D, 0xA3])));
    }

    #[test]
    fn conflict_with_ptt() {
        let b = Bindings::default();
        assert_eq!(find_conflict(&b, 0x56, Action::ToggleMute, c(0x56, false, false, false)), Some("Рация".into()));
        assert_eq!(find_conflict(&b, 0x56, Action::ToggleMute, c(0x56, true, false, false)), None);
    }

    #[test]
    fn conflict_with_other_action_and_ignores_self() {
        let mut b = Bindings::default();
        let ch = c(0x77, false, false, false);
        b.set(Action::ToggleDeafen, Some(ch));
        assert_eq!(find_conflict(&b, 0x56, Action::ToggleMute, ch), Some("Звук вкл/выкл".into()));
        assert_eq!(find_conflict(&b, 0x56, Action::ToggleDeafen, ch), None);
    }

    #[test]
    fn apply_capture_rejects_conflict() {
        let mut b = Bindings::default();
        assert!(apply_capture(&mut b, 0x56, Action::Leave, c(0x56, false, false, false)).is_err());
        assert_eq!(b.leave, None);
        assert!(apply_capture(&mut b, 0x56, Action::Leave, c(0x78, false, false, false)).is_ok());
        assert_eq!(b.leave, Some(c(0x78, false, false, false)));
    }

    #[test]
    fn edge_fires_once() {
        let mut e = Edge::default();
        let r: Vec<bool> = [false, true, true, false, true].into_iter().map(|x| e.update(x)).collect();
        assert_eq!(r, [false, true, false, false, true]);
    }

    #[test]
    fn tick_fires_on_press_only() {
        let mut st = TickState::default();
        let mut b = Bindings::default();
        b.mute = Some(c(0x77, false, false, false));
        b.deafen = Some(c(0x78, false, false, false));
        assert_eq!(tick(&mut st, &b, &held(&[0x77])), [Action::ToggleMute]);
        assert!(tick(&mut st, &b, &held(&[0x77])).is_empty());
        assert_eq!(tick(&mut st, &b, &held(&[0x77, 0x78])), [Action::ToggleDeafen]);
    }

    #[test]
    fn tick_no_fire_with_extra_modifier() {
        let mut st = TickState::default();
        let mut b = Bindings::default();
        b.mute = Some(c(0x4D, true, false, false));
        assert!(tick(&mut st, &b, &held(&[0x4D, 0xA2, 0xA4])).is_empty());
    }

    #[test]
    fn bindings_old_config_loads() {
        assert_eq!(serde_json::from_str::<Bindings>("{}").unwrap(), Bindings::default());
    }

    #[test]
    fn chord_name_format() {
        assert_eq!(chord_name(c(0x4D, true, false, true)), "Ctrl+Shift+M");
        assert_eq!(chord_name(c(0x74, false, false, false)), "F5");
    }
}
