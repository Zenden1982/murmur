//! Сообщения управления трансляцией по каналу данных LiveKit (JSON, версия в каждом сообщении).

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Запросы, разрешения, состояние (reliable).
pub const TOPIC_RC: &str = "murmur.rc";
/// Положение указки (lossy).
pub const TOPIC_PTR: &str = "murmur.rc.ptr";
/// Ввод управляющего (reliable, по порядку).
pub const TOPIC_IN: &str = "murmur.rc.in";
pub const VERSION: u8 = 1;
pub const MAX_PAYLOAD: usize = 1024;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord, Hash)]
pub enum Level {
    Pointer,
    Control,
}

/// Случайный идентификатор выданного доступа: команды со старой сессией отбрасываются.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct Session(pub String);

impl Session {
    pub fn random() -> Session {
        let mut b = [0u8; 16];
        getrandom::fill(&mut b).expect("getrandom");
        Session(b.iter().map(|x| format!("{x:02x}")).collect())
    }

    fn valid(&self) -> bool {
        self.0.len() == 32 && self.0.bytes().all(|c| c.is_ascii_hexdigit())
    }
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct StateEntry {
    pub who: String,
    pub level: Level,
    pub controlling: bool,
    pub paused: bool,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(tag = "t")]
pub enum RcMsg {
    Request { level: Level },
    Grant { level: Level, session: Session },
    Deny,
    Revoke,
    Take { session: Session, on: bool },
    State { entries: Vec<StateEntry>, layout: String },
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct PtrMsg {
    pub s: Session,
    pub x: f32,
    pub y: f32,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Btn {
    L,
    R,
    M,
    X1,
    X2,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(tag = "t")]
pub enum InputEv {
    Button { b: Btn, down: bool, x: f32, y: f32 },
    Wheel { dx: i16, dy: i16, x: f32, y: f32 },
    Key { sc: u16, ext: bool, down: bool },
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct InMsg {
    pub s: Session,
    pub seq: u64,
    pub ev: InputEv,
}

#[derive(Serialize, Deserialize)]
struct Env<T> {
    v: u8,
    #[serde(flatten)]
    m: T,
}

#[derive(Serialize)]
struct EnvRef<'a, T> {
    v: u8,
    #[serde(flatten)]
    m: &'a T,
}

pub fn encode<T: Serialize>(msg: &T) -> Vec<u8> {
    serde_json::to_vec(&EnvRef { v: VERSION, m: msg }).unwrap_or_default()
}

fn decode<T: DeserializeOwned>(b: &[u8]) -> Option<T> {
    if b.len() > MAX_PAYLOAD {
        return None;
    }
    let e: Env<T> = serde_json::from_slice(b).ok()?;
    (e.v == VERSION).then_some(e.m)
}

fn coord(v: f32) -> bool {
    v.is_finite() && (0.0..=1.0).contains(&v)
}

pub fn decode_rc(b: &[u8]) -> Option<RcMsg> {
    let m: RcMsg = decode(b)?;
    let ok = match &m {
        RcMsg::Grant { session, .. } | RcMsg::Take { session, .. } => session.valid(),
        _ => true,
    };
    ok.then_some(m)
}

pub fn decode_ptr(b: &[u8]) -> Option<PtrMsg> {
    let m: PtrMsg = decode(b)?;
    (m.s.valid() && coord(m.x) && coord(m.y)).then_some(m)
}

pub fn decode_in(b: &[u8]) -> Option<InMsg> {
    let m: InMsg = decode(b)?;
    let ok = match m.ev {
        InputEv::Button { x, y, .. } | InputEv::Wheel { x, y, .. } => coord(x) && coord(y),
        InputEv::Key { .. } => true,
    };
    (m.s.valid() && ok).then_some(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_rc() -> Vec<RcMsg> {
        let s = Session("0123456789abcdef0123456789abcdef".into());
        vec![
            RcMsg::Request { level: Level::Pointer },
            RcMsg::Grant { level: Level::Control, session: s.clone() },
            RcMsg::Deny,
            RcMsg::Revoke,
            RcMsg::Take { session: s, on: true },
            RcMsg::State {
                entries: vec![StateEntry { who: "a".into(), level: Level::Control, controlling: true, paused: false }],
                layout: "RU".into(),
            },
        ]
    }

    #[test]
    fn roundtrip_all_messages() {
        for m in all_rc() {
            assert_eq!(decode_rc(&encode(&m)), Some(m));
        }
        let s = Session::random();
        let p = PtrMsg { s: s.clone(), x: 0.25, y: 1.0 };
        assert_eq!(decode_ptr(&encode(&p)), Some(p));
        for ev in [
            InputEv::Button { b: Btn::X2, down: true, x: 0.0, y: 0.5 },
            InputEv::Wheel { dx: 0, dy: -120, x: 0.1, y: 0.2 },
            InputEv::Key { sc: 0x1E, ext: false, down: true },
        ] {
            let m = InMsg { s: s.clone(), seq: 7, ev };
            assert_eq!(decode_in(&encode(&m)), Some(m));
        }
    }

    #[test]
    fn rejects_wrong_version() {
        assert_eq!(decode_rc(br#"{"v":2,"t":"Deny"}"#), None);
        assert_eq!(decode_rc(br#"{"t":"Deny"}"#), None);
        assert_eq!(decode_rc(br#"{"v":1,"t":"Deny"}"#), Some(RcMsg::Deny));
    }

    #[test]
    fn rejects_oversized() {
        let mut big = br#"{"v":1,"t":"State","entries":[],"layout":""#.to_vec();
        big.resize(1030, b'x');
        big.extend_from_slice(br#""}"#);
        assert!(big.len() > MAX_PAYLOAD);
        assert_eq!(decode_rc(&big), None);
    }

    #[test]
    fn rejects_bad_coords() {
        let s = "0123456789abcdef0123456789abcdef";
        let ok = format!(r#"{{"v":1,"s":"{s}","x":0.5,"y":0.5}}"#);
        assert!(decode_ptr(ok.as_bytes()).is_some());
        for bad in [r#""x":1.5,"y":0.5"#, r#""x":-0.1,"y":0.5"#, r#""x":0.5,"y":1e39"#, r#""x":"NaN","y":0.5"#] {
            let j = format!(r#"{{"v":1,"s":"{s}",{bad}}}"#);
            assert_eq!(decode_ptr(j.as_bytes()), None, "{j}");
        }
        let j = format!(r#"{{"v":1,"s":"{s}","seq":1,"ev":{{"t":"Button","b":"L","down":true,"x":2.0,"y":0.0}}}}"#);
        assert_eq!(decode_in(j.as_bytes()), None);
    }

    #[test]
    fn rejects_bad_session() {
        assert_eq!(decode_ptr(br#"{"v":1,"s":"short","x":0.5,"y":0.5}"#), None);
    }

    #[test]
    fn session_random_is_32_hex_and_differs() {
        let a = Session::random();
        let b = Session::random();
        assert_eq!(a.0.len(), 32);
        assert!(a.0.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
