//! Короткие звуки: вход/выход из голоса, начало/конец трансляции, зрители своей трансляции. Синтезируются при старте (WAV в памяти) и играются через
//! PlaySound на системном устройстве вывода по умолчанию.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};

/// Громкость звуков интерфейса, 0–100 (настройка «Звуки интерфейса»).
static VOLUME: AtomicU8 = AtomicU8::new(60);

pub fn set_volume(v: u8) {
    VOLUME.store(v.min(100), Ordering::Relaxed);
}

#[derive(Debug, Clone, Copy)]
pub enum Sound {
    JoinSelf,
    LeaveSelf,
    JoinOther,
    LeaveOther,
    /// Кто-то (или вы) начал трансляцию.
    StreamStart,
    /// Трансляция закончилась.
    StreamStop,
    /// Вашу трансляцию начали смотреть.
    ViewerJoin,
    /// Вашу трансляцию перестали смотреть.
    ViewerLeave,
    /// Новое личное сообщение.
    DirectMessage,
    /// Входящий звонок (повторяется, пока звонит).
    Ring,
    /// Гудок у звонящего (повторяется, пока не ответили).
    RingBack,
    /// Горячая клавиша: микрофон выключен / включён.
    MuteOn,
    MuteOff,
    /// Горячая клавиша: звук выключен / включён.
    DeafenOn,
    DeafenOff,
}

const RATE: u32 = 44_100;

/// Тон с мягкой огибающей (без щелчков): частота → длительность в мс.
fn tone(out: &mut Vec<i16>, freq: f32, ms: u32, amp: f32) {
    let n = RATE * ms / 1000;
    let attack = (RATE / 200) as f32; // 5 мс
    for i in 0..n {
        let t = i as f32 / RATE as f32;
        let env_in = (i as f32 / attack).min(1.0);
        let env_out = ((n - i) as f32 / (n as f32 * 0.6)).min(1.0);
        let env = env_in * env_out * env_out;
        // основной тон + немного второй гармоники — «тёплый» звук лампы
        let s = (t * freq * std::f32::consts::TAU).sin() * 0.85 + (t * freq * 2.0 * std::f32::consts::TAU).sin() * 0.15;
        out.push((s * env * amp * i16::MAX as f32) as i16);
    }
}

fn wav(samples: &[i16]) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut v = Vec::with_capacity(44 + data_len as usize);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data_len).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes()); // PCM
    v.extend_from_slice(&1u16.to_le_bytes()); // mono
    v.extend_from_slice(&RATE.to_le_bytes());
    v.extend_from_slice(&(RATE * 2).to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

fn build(notes: &[(f32, u32)], amp: f32) -> Vec<u8> {
    let mut s = Vec::new();
    for &(f, ms) in notes {
        tone(&mut s, f, ms, amp);
    }
    wav(&s)
}

struct Bank {
    join_self: Vec<u8>,
    leave_self: Vec<u8>,
    join_other: Vec<u8>,
    leave_other: Vec<u8>,
    stream_start: Vec<u8>,
    stream_stop: Vec<u8>,
    viewer_join: Vec<u8>,
    viewer_leave: Vec<u8>,
    direct_message: Vec<u8>,
    ring: Vec<u8>,
    ring_back: Vec<u8>,
    mute_on: Vec<u8>,
    mute_off: Vec<u8>,
    deafen_on: Vec<u8>,
    deafen_off: Vec<u8>,
}

/// Банк звуков для громкости `step` (шаг 5 %; 60 % = прежняя громкость). PlaySound с SND_ASYNC читает WAV, пока играет,
/// поэтому банки не освобождаются: не больше 21 штуки по ~0.2 МБ.
fn bank() -> &'static Bank {
    static BANKS: Mutex<Vec<(u8, &'static Bank)>> = Mutex::new(Vec::new());
    let step = (VOLUME.load(Ordering::Relaxed).min(100) + 2) / 5 * 5;
    let mut banks = BANKS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, b)) = banks.iter().find(|(s, _)| *s == step) {
        return b;
    }
    let k = step as f32 / 60.0;
    let b: &'static Bank = Box::leak(Box::new(Bank {
        join_self: build(&[(523.25, 90), (783.99, 140)], 0.22 * k),
        leave_self: build(&[(783.99, 90), (523.25, 140)], 0.22 * k),
        join_other: build(&[(659.25, 70), (880.0, 110)], 0.16 * k),
        leave_other: build(&[(880.0, 70), (659.25, 110)], 0.16 * k),
        // трансляция — три ноты (ре–соль–до), выше и «ярче» голосовых
        stream_start: build(&[(587.33, 60), (783.99, 60), (1046.5, 150)], 0.18 * k),
        stream_stop: build(&[(1046.5, 60), (783.99, 60), (587.33, 150)], 0.18 * k),
        // зритель — короткий тихий «блик»
        viewer_join: build(&[(1174.66, 50), (1567.98, 90)], 0.11 * k),
        viewer_leave: build(&[(1567.98, 50), (1174.66, 90)], 0.11 * k),
        // ЛС — две мягкие ноты вниз (ми–до), не путать с «бликом» зрителя
        direct_message: build(&[(1318.51, 70), (1046.5, 130)], 0.14 * k),
        // звонок — трель «до–ми–до–ми–соль», заметнее остальных звуков
        ring: build(&[(1046.5, 110), (1318.51, 110), (1046.5, 110), (1318.51, 110), (1567.98, 260)], 0.2 * k),
        // гудок у звонящего — один тихий долгий тон
        ring_back: build(&[(440.0, 900)], 0.08 * k),
        // микрофон: один короткий тон — вниз (выкл) и вверх (вкл); звук: два тона
        mute_on: build(&[(659.25, 60), (440.0, 110)], 0.14 * k),
        mute_off: build(&[(440.0, 60), (659.25, 110)], 0.14 * k),
        deafen_on: build(&[(587.33, 55), (466.16, 55), (349.23, 120)], 0.14 * k),
        deafen_off: build(&[(349.23, 55), (466.16, 55), (587.33, 120)], 0.14 * k),
    }));
    banks.push((step, b));
    b
}

pub fn play(s: Sound) {
    if VOLUME.load(Ordering::Relaxed) == 0 {
        return;
    }
    let b = bank();
    let data: &'static [u8] = match s {
        Sound::JoinSelf => &b.join_self,
        Sound::LeaveSelf => &b.leave_self,
        Sound::JoinOther => &b.join_other,
        Sound::LeaveOther => &b.leave_other,
        Sound::StreamStart => &b.stream_start,
        Sound::StreamStop => &b.stream_stop,
        Sound::ViewerJoin => &b.viewer_join,
        Sound::ViewerLeave => &b.viewer_leave,
        Sound::DirectMessage => &b.direct_message,
        Sound::Ring => &b.ring,
        Sound::RingBack => &b.ring_back,
        Sound::MuteOn => &b.mute_on,
        Sound::MuteOff => &b.mute_off,
        Sound::DeafenOn => &b.deafen_on,
        Sound::DeafenOff => &b.deafen_off,
    };
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
        // SND_MEMORY: первый аргумент — указатель на WAV в памяти (живёт всю программу).
        PlaySoundW(data.as_ptr() as *const u16, std::ptr::null_mut(), SND_MEMORY | SND_ASYNC | SND_NODEFAULT);
    }
    #[cfg(not(windows))]
    let _ = data;
}

#[cfg(test)]
mod tests {
    #[test]
    fn wav_header_is_consistent() {
        let w = super::build(&[(440.0, 10)], 0.5);
        assert_eq!(&w[0..4], b"RIFF");
        let riff = u32::from_le_bytes(w[4..8].try_into().unwrap()) as usize;
        assert_eq!(riff + 8, w.len());
        let data = u32::from_le_bytes(w[40..44].try_into().unwrap()) as usize;
        assert_eq!(data + 44, w.len());
    }
}
