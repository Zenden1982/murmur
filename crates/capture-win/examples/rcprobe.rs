//! Опыт: как аппаратный энкодер держит битрейт и QP на живом окне. 1440p60, старт 2,5 Мбит/с, через 1 с — запрос
//! цели (по умолчанию 12 Мбит/с), как это делает WebRTC. Раз в секунду: кадры, кбит/с, средний QP.
//! cargo run -p capture-win --example rcprobe --release -- <подстрока заголовка окна> [секунд] [цель кбит/с]
//! Режим: AMADEUS_ENC_RC=cbr|pcvbr|uvbr (по умолчанию LowDelayVBR), AMADEUS_ENC_MAXQP=N.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use capture_win::capture::CaptureConfig;
use capture_win::h264qp::QpParser;
use capture_win::hw::{EncodedSink, HwCapture};
use capture_win::{Event, list_sources};

struct Sink {
    frames: AtomicU64,
    bytes: AtomicU64,
    qp_sum: AtomicU64,
    qp_n: AtomicU64,
    parser: Mutex<QpParser>,
    want: AtomicU32,
    sent: AtomicBool,
    /// Запрос ключевого кадра (AMADEUS_PROBE_KEY_EVERY=N — раз в N с; размер и задержка — в строках «ДИАГ ключевой кадр»).
    key: AtomicBool,
}

impl EncodedSink for Sink {
    fn encoded(&self, data: &[u8], _key: bool, _ts: i64, _w: u32, _h: u32) {
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
        if let Some(q) = self.parser.lock().unwrap().frame_qp(data) {
            self.qp_sum.fetch_add(q as u64, Ordering::Relaxed);
            self.qp_n.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn take_keyframe_request(&self) -> bool {
        self.key.swap(false, Ordering::Relaxed)
    }
    fn take_bitrate_request(&self) -> Option<u32> {
        let w = self.want.load(Ordering::Relaxed);
        (w > 0 && !self.sent.swap(true, Ordering::Relaxed)).then_some(w)
    }
    fn event(&self, ev: Event) {
        println!("событие: {ev:?}");
    }
}

fn main() {
    struct L;
    impl log::Log for L {
        fn enabled(&self, m: &log::Metadata) -> bool {
            m.level() <= log::Level::Info
        }
        fn log(&self, r: &log::Record) {
            if r.target().starts_with("capture_win") {
                println!("[{}] {}", r.level(), r.args());
            }
        }
        fn flush(&self) {}
    }
    let _ = log::set_logger(&L).map(|()| log::set_max_level(log::LevelFilter::Info));
    let args: Vec<String> = std::env::args().collect();
    let q = args.get(1).expect("подстрока заголовка окна").to_lowercase();
    let secs: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);
    let target: u32 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(12_000);
    let src = list_sources(16, 9).into_iter().find(|s| s.title.to_lowercase().contains(&q)).expect("окно не найдено");
    println!("окно: {}", src.title);
    let sink = Arc::new(Sink {
        frames: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        qp_sum: AtomicU64::new(0),
        qp_n: AtomicU64::new(0),
        parser: Mutex::new(QpParser::default()),
        want: AtomicU32::new(0),
        sent: AtomicBool::new(false),
        key: AtomicBool::new(false),
    });
    let key_every: u64 = std::env::var("AMADEUS_PROBE_KEY_EVERY").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    let cfg = CaptureConfig { max_width: 2560, max_height: 1440, fps: 60 };
    let cap = HwCapture::start(src.id, cfg, 12_000_000, sink.clone()).expect("старт");
    println!("энкодер «{}», {:?}", cap.encoder_name, cap.size);
    let t0 = Instant::now();
    let mut qps = Vec::new();
    let mut kbs = Vec::new();
    for s in 1..=secs {
        std::thread::sleep(Duration::from_secs(1));
        if s == 1 {
            sink.want.store(target * 1000, Ordering::Relaxed);
        }
        if key_every > 0 && s % key_every == 0 {
            sink.key.store(true, Ordering::Relaxed);
        }
        let f = sink.frames.swap(0, Ordering::Relaxed);
        let kb = sink.bytes.swap(0, Ordering::Relaxed) * 8 / 1000;
        let (qs, qn) = (sink.qp_sum.swap(0, Ordering::Relaxed), sink.qp_n.swap(0, Ordering::Relaxed));
        let qp = if qn > 0 { qs as f64 / qn as f64 } else { f64::NAN };
        println!("{:>4.1} с: {f} к/с, {kb} кбит/с, QP {qp:.1}", t0.elapsed().as_secs_f64());
        if s > 2 {
            qps.push(qp);
            kbs.push(kb);
        }
    }
    let n = qps.len().max(1) as f64;
    println!("ИТОГ (после 2 с): {:.0} кбит/с, QP {:.1}", kbs.iter().sum::<u64>() as f64 / n, qps.iter().sum::<f64>() / n);
    drop(cap);
}
