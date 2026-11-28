//! Проверка захвата звука без клиента:
//! cargo run -p capture-win --example audioprobe --release -- (pid <PID> | except <PID> | all | window <часть заголовка>) [секунд] [out.wav]
//! Печатает уровень по секундам и пишет WAV (48 кГц, стерео, 16 бит).

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use capture_win::app_audio::{self, AppAudio, AudioSink, AudioTarget};
use capture_win::{SourceId, list_sources};

#[derive(Default)]
struct Rec {
    samples: Mutex<Vec<i16>>,
}

impl AudioSink for Rec {
    fn frame(&self, data: &[i16]) {
        assert_eq!(data.len(), app_audio::FRAME * 2);
        self.samples.lock().unwrap().extend_from_slice(data);
    }
}

fn main() {
    env_logger_init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    // apps — программы со звуковыми сессиями; screen <exe,exe|-> — звук экрана без этих программ (ScreenAudio)
    if args.first().map(String::as_str) == Some("apps") {
        for a in capture_win::audio_sessions::audio_apps() {
            println!("{:<40} pid {:>6}  {}", a.exe, a.pid, if a.active { "играет" } else { "" });
        }
        return;
    }
    let screen: Option<Vec<String>> = (args.first().map(String::as_str) == Some("screen"))
        .then(|| args.get(1).filter(|s| *s != "-").map(|s| s.split(',').map(|x| x.trim().to_lowercase()).collect()).unwrap_or_default());
    let target = match args.first().map(String::as_str) {
        Some("screen") => AudioTarget::AllExcept(0),
        Some("pid") => AudioTarget::App(args[1].parse().expect("PID")),
        Some("all") => AudioTarget::for_source(SourceId::Monitor(0)).unwrap(),
        Some("except") => AudioTarget::AllExcept(args[1].parse().expect("PID")),
        Some("window") => {
            let want = args[1].to_lowercase();
            let s = list_sources(64, 36).into_iter().find(|s| s.id.is_window() && s.title.to_lowercase().contains(&want)).expect("окно не найдено");
            println!("окно: {} ({})", s.title, s.app);
            AudioTarget::for_source(s.id).expect("PID окна")
        }
        _ => {
            eprintln!("audioprobe (pid <PID> | all | window <заголовок>) [секунд] [out.wav]");
            return;
        }
    };
    let rest = if matches!(args[0].as_str(), "all") { &args[1..] } else { &args[2..] };
    let secs: u64 = rest.first().and_then(|s| s.parse().ok()).unwrap_or(5);
    let out = rest.get(1).cloned();
    println!("цель: {target:?}, {secs} с");
    let rec = Arc::new(Rec::default());
    let cap: Box<dyn FnOnce()> = match screen {
        Some(ex) => {
            println!("исключено: {ex:?}");
            let c = capture_win::ScreenAudio::start(std::sync::Arc::new(move || ex.clone()), rec.clone()).expect("старт захвата");
            Box::new(move || c.stop())
        }
        None => {
            let c = AppAudio::start(target, rec.clone()).expect("старт захвата");
            Box::new(move || c.stop())
        }
    };
    let mut last = 0;
    for s in 1..=secs {
        std::thread::sleep(Duration::from_secs(1));
        let v = rec.samples.lock().unwrap();
        let chunk = &v[last..];
        let rms = (chunk.iter().map(|&x| (x as f64).powi(2)).sum::<f64>() / chunk.len().max(1) as f64).sqrt();
        let peak = chunk.iter().map(|&x| (x as i32).abs()).max().unwrap_or(0);
        println!("{s:>3} с: кадров {:>4}, rms {rms:>8.1}, пик {peak}", chunk.len() / (app_audio::FRAME * 2));
        last = v.len();
    }
    cap();
    let v = rec.samples.lock().unwrap();
    println!("всего: {:.2} с звука за {secs} с", v.len() as f64 / 2.0 / 48000.0);
    if let Some(p) = out {
        let mut f = std::fs::File::create(&p).unwrap();
        let data_len = (v.len() * 2) as u32;
        f.write_all(b"RIFF").unwrap();
        f.write_all(&(36 + data_len).to_le_bytes()).unwrap();
        f.write_all(b"WAVEfmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap();
        f.write_all(&2u16.to_le_bytes()).unwrap();
        f.write_all(&48000u32.to_le_bytes()).unwrap();
        f.write_all(&(48000u32 * 4).to_le_bytes()).unwrap();
        f.write_all(&4u16.to_le_bytes()).unwrap();
        f.write_all(&16u16.to_le_bytes()).unwrap();
        f.write_all(b"data").unwrap();
        f.write_all(&data_len.to_le_bytes()).unwrap();
        for s in v.iter() {
            f.write_all(&s.to_le_bytes()).unwrap();
        }
        println!("записано: {p}");
    }
}

fn env_logger_init() {
    struct L;
    impl log::Log for L {
        fn enabled(&self, m: &log::Metadata) -> bool {
            m.level() <= log::Level::Info
        }
        fn log(&self, r: &log::Record) {
            println!("[{}] {}", r.level(), r.args());
        }
        fn flush(&self) {}
    }
    let _ = log::set_logger(&L);
    log::set_max_level(log::LevelFilter::Info);
}
