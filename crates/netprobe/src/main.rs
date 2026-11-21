//! murmur-netprobe — замер UDP-потерь до сервера Murmur на скоростях трансляции.
//!
//! Клиент (по умолчанию): murmur-netprobe [--server host:port] [--label имя] [--rates 2,5,10] [--secs 10]
//! Сервер: murmur-netprobe serve --port 7990 --out results.log
//!
//! Трафик похож на трансляцию: 60 кадров/с, пакеты по 1200 байт, раз в 2 с «смена сцены» (кадр ×8),
//! отправка через «пейсер» со скоростью 2,5× цели (как в WebRTC). Меряем в обе стороны.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAGIC: &[u8; 4] = b"MNP1";
const T_HELLO: u8 = 1;
const T_HELLO_ACK: u8 = 2;
const T_DATA: u8 = 3;
const T_UP_END: u8 = 4;
const T_UP_REPORT: u8 = 5;
const T_DOWN_REQ: u8 = 6;
const T_DOWN_END: u8 = 7;
const T_REPORT: u8 = 8;
const T_PING: u8 = 9;
const T_PONG: u8 = 10;
const T_KEEP: u8 = 11;
const T_REPORT_ACK: u8 = 12;

const PKT: usize = 1200;
const FPS: u64 = 60;
const SPIKE_EVERY: u64 = 120; // кадров (2 с)
const SPIKE_MUL: u64 = 8;
const PACING: f64 = 2.5;
const DEFAULT_SERVER: &str = "77.222.35.128:7990";

// Ограничения сервера (защита от злоупотреблений).
const MAX_RATE_KBPS: u32 = 25_000;
const MAX_DUR_MS: u32 = 15_000;
const MAX_SESSIONS: usize = 8;
const MAX_DOWNS_PER_SESSION: u32 = 12;
const MAX_ACTIVE_DOWNS: usize = 3;
const SESSION_TTL: Duration = Duration::from_secs(600);
const MAX_RECS: usize = 200_000;

// ---------- кодирование ----------

struct W(Vec<u8>);
impl W {
    fn new(t: u8) -> Self {
        let mut v = Vec::with_capacity(64);
        v.extend_from_slice(MAGIC);
        v.push(t);
        W(v)
    }
    fn u8(mut self, x: u8) -> Self {
        self.0.push(x);
        self
    }
    fn u16(mut self, x: u16) -> Self {
        self.0.extend_from_slice(&x.to_le_bytes());
        self
    }
    fn u32(mut self, x: u32) -> Self {
        self.0.extend_from_slice(&x.to_le_bytes());
        self
    }
    fn u64(mut self, x: u64) -> Self {
        self.0.extend_from_slice(&x.to_le_bytes());
        self
    }
    fn bytes(mut self, b: &[u8]) -> Self {
        self.0.extend_from_slice(b);
        self
    }
    fn pad(mut self, n: usize) -> Self {
        if self.0.len() < n {
            self.0.resize(n, 0);
        }
        self
    }
}

struct R<'a>(&'a [u8]);
impl<'a> R<'a> {
    fn parse(b: &'a [u8]) -> Option<(u8, R<'a>)> {
        if b.len() < 5 || &b[..4] != MAGIC {
            return None;
        }
        Some((b[4], R(&b[5..])))
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_le_bytes(b.try_into().unwrap()))
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    }
    fn u64(&mut self) -> Option<u64> {
        self.take(8).map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    }
    fn rest(&mut self) -> &'a [u8] {
        let r = self.0;
        self.0 = &[];
        r
    }
}

fn rnd() -> u64 {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() as u64;
    let mut x = t ^ ((std::process::id() as u64) << 32) ^ 0x9E37_79B9_7F4A_7C15;
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51afd7ed558ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ceb9fe1a85ec53);
    x ^ (x >> 33)
}

fn unix_s() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

// ---------- расписание «видео» ----------

/// (момент отправки в мкс от начала, пакет «смены сцены»)
fn schedule(rate_kbps: u32, dur_ms: u32) -> Vec<(u64, bool)> {
    let frame_us = 1_000_000 / FPS;
    let frame_bytes = (rate_kbps as u64 * 1000 / 8) / FPS;
    let gap_us = ((PKT as f64 * 8.0 * 1e6) / (rate_kbps as f64 * 1000.0 * PACING)).max(1.0) as u64;
    let frames = dur_ms as u64 * FPS / 1000;
    let mut out = Vec::new();
    let mut next_free = 0u64;
    for f in 0..frames {
        let start = f * frame_us;
        let spike = f % SPIKE_EVERY == SPIKE_EVERY / 2;
        let bytes = if spike { frame_bytes * SPIKE_MUL } else { frame_bytes };
        let pkts = bytes.div_ceil(PKT as u64).max(1);
        for _ in 0..pkts {
            let due = start.max(next_free);
            next_free = due + gap_us;
            out.push((due, spike));
        }
    }
    out
}

fn send_schedule(
    sock: &UdpSocket,
    to: SocketAddr,
    session: u64,
    test: u8,
    sched: &[(u64, bool)],
    stop: &dyn Fn() -> bool,
) -> (u32, u32) {
    let mut buf = vec![0u8; PKT];
    buf[..4].copy_from_slice(MAGIC);
    buf[4] = T_DATA;
    buf[5..13].copy_from_slice(&session.to_le_bytes());
    buf[13] = test;
    let t0 = Instant::now();
    let mut sent = 0u32;
    let mut errs = 0u32;
    for (seq, &(due, spike)) in sched.iter().enumerate() {
        loop {
            let now = t0.elapsed().as_micros() as u64;
            if now >= due {
                break;
            }
            let wait = due - now;
            if wait > 1500 {
                std::thread::sleep(Duration::from_micros(wait - 1000));
            } else if wait > 300 {
                std::thread::sleep(Duration::from_micros(200));
            } else {
                std::hint::spin_loop();
            }
        }
        if seq % 64 == 0 && stop() {
            break;
        }
        buf[14] = spike as u8;
        buf[15..19].copy_from_slice(&(seq as u32).to_le_bytes());
        let now = t0.elapsed().as_micros() as u64;
        buf[19..27].copy_from_slice(&now.to_le_bytes());
        if sock.send_to(&buf, to).is_err() {
            errs += 1;
        }
        sent += 1;
    }
    (sent, errs)
}

// ---------- статистика ----------

/// (seq, send_us, recv_us)
type Rec = (u32, u64, u64);

struct Stats {
    sent: u32,
    lost: u32,
    events: u32,
    max_run: u32,
    spike_sent: u32,
    spike_lost: u32,
    reorder: u32,
    dup: u32,
    worst_sec: f64,
    d50: f64,
    d95: f64,
    d99: f64,
    dmax: f64,
}

fn stats(sched: &[(u64, bool)], sent: u32, recs: &[Rec]) -> Stats {
    let n = sent as usize;
    let mut got = vec![false; n];
    let mut dup = 0;
    let mut reorder = 0;
    let mut max_seen: i64 = -1;
    let mut ds: Vec<i64> = Vec::with_capacity(recs.len());
    for &(seq, s, r) in recs {
        let i = seq as usize;
        if i >= n {
            continue;
        }
        if got[i] {
            dup += 1;
            continue;
        }
        got[i] = true;
        if (seq as i64) < max_seen {
            reorder += 1;
        }
        max_seen = max_seen.max(seq as i64);
        ds.push(r as i64 - s as i64);
    }
    let mut lost = 0;
    let mut events = 0;
    let mut run = 0;
    let mut max_run = 0;
    let mut spike_sent = 0;
    let mut spike_lost = 0;
    let mut sec: HashMap<u64, (u32, u32)> = HashMap::new();
    for (i, &g) in got.iter().enumerate() {
        let (due, spike) = sched.get(i).copied().unwrap_or((0, false));
        let e = sec.entry(due / 1_000_000).or_default();
        e.0 += 1;
        if spike {
            spike_sent += 1;
        }
        if g {
            run = 0;
        } else {
            lost += 1;
            e.1 += 1;
            if spike {
                spike_lost += 1;
            }
            if run == 0 {
                events += 1;
            }
            run += 1;
            max_run = max_run.max(run);
        }
    }
    let worst_sec = sec
        .values()
        .filter(|(s, _)| *s > 0)
        .map(|(s, l)| *l as f64 * 100.0 / *s as f64)
        .fold(0.0, f64::max);
    let (mut d50, mut d95, mut d99, mut dmax) = (0.0, 0.0, 0.0, 0.0);
    if !ds.is_empty() {
        let min = *ds.iter().min().unwrap();
        let mut v: Vec<f64> = ds.iter().map(|d| (d - min) as f64 / 1000.0).collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p = |q: f64| v[((v.len() - 1) as f64 * q) as usize];
        d50 = p(0.5);
        d95 = p(0.95);
        d99 = p(0.99);
        dmax = *v.last().unwrap();
    }
    Stats { sent, lost, events, max_run, spike_sent, spike_lost, reorder, dup, worst_sec, d50, d95, d99, dmax }
}

fn fmt_stats(st: &Stats) -> String {
    let pct = |a: u32, b: u32| if b == 0 { 0.0 } else { a as f64 * 100.0 / b as f64 };
    format!(
        "потери {:.2} % ({} из {}), событий {}, макс. подряд {}, в «сменах сцены» {:.2} % ({} из {}), худшая секунда {:.1} %, \
         разброс задержки p50 {:.1} / p95 {:.1} / p99 {:.1} / макс {:.1} мс, перестановок {}, дублей {}",
        pct(st.lost, st.sent),
        st.lost,
        st.sent,
        st.events,
        st.max_run,
        pct(st.spike_lost, st.spike_sent),
        st.spike_lost,
        st.spike_sent,
        st.worst_sec,
        st.d50,
        st.d95,
        st.d99,
        st.dmax,
        st.reorder,
        st.dup
    )
}

// ---------- сервер ----------

struct Session {
    addr: SocketAddr,
    cookie: u64,
    label: String,
    last: Instant,
    alive_ms: Arc<AtomicU64>,
    up: HashMap<u8, Vec<Rec>>,
    up_done: HashMap<u8, String>,
    downs: u32,
    down_started: HashSet<u8>,
    reports: HashSet<u16>,
}

fn serve(port: u16, out: &str) -> std::io::Result<()> {
    let sock = Arc::new(UdpSocket::bind(("0.0.0.0", port))?);
    let log = Arc::new(Mutex::new(std::fs::OpenOptions::new().create(true).append(true).open(out)?));
    let logln = {
        let log = log.clone();
        move |s: &str| {
            let line = format!("{}\t{}\n", unix_s(), s);
            print!("{line}");
            if let Ok(mut f) = log.lock() {
                let _ = f.write_all(line.as_bytes());
                let _ = f.flush();
            }
        }
    };
    logln(&format!("сервер: порт {port}"));
    let t0 = Instant::now();
    let mut sessions: HashMap<u64, Session> = HashMap::new();
    let active_downs = Arc::new(AtomicUsize::new(0));
    let mut buf = vec![0u8; 2048];
    loop {
        let (n, from) = match sock.recv_from(&mut buf) {
            Ok(x) => x,
            Err(_) => continue,
        };
        let now_us = t0.elapsed().as_micros() as u64;
        let Some((t, mut r)) = R::parse(&buf[..n]) else { continue };
        let Some(sid) = r.u64() else { continue };
        sessions.retain(|_, s| s.last.elapsed() < SESSION_TTL);

        if t == T_HELLO {
            if n < PKT {
                continue; // HELLO полноразмерный — ответ меньше запроса, усиления нет
            }
            if !sessions.contains_key(&sid) {
                if sessions.len() >= MAX_SESSIONS {
                    continue;
                }
                let llen = r.u8().unwrap_or(0) as usize;
                let label = r
                    .take(llen.min(64))
                    .map(|b| String::from_utf8_lossy(b).replace(['\t', '\n', '\r'], " "))
                    .unwrap_or_default();
                logln(&format!("{label}\t{from}\tсессия {sid:016x}: начало"));
                sessions.insert(
                    sid,
                    Session {
                        addr: from,
                        cookie: rnd(),
                        label,
                        last: Instant::now(),
                        alive_ms: Arc::new(AtomicU64::new(t0.elapsed().as_millis() as u64)),
                        up: HashMap::new(),
                        up_done: HashMap::new(),
                        downs: 0,
                        down_started: HashSet::new(),
                        reports: HashSet::new(),
                    },
                );
            }
            let s = &sessions[&sid];
            if s.addr == from {
                let _ = sock.send_to(&W::new(T_HELLO_ACK).u64(sid).u64(s.cookie).0, from);
            }
            continue;
        }

        let Some(s) = sessions.get_mut(&sid) else { continue };
        if s.addr != from {
            continue;
        }
        s.last = Instant::now();
        s.alive_ms.store(t0.elapsed().as_millis() as u64, Ordering::Relaxed);

        match t {
            T_DATA => {
                let (Some(test), Some(_spike), Some(seq), Some(su)) = (r.u8(), r.u8(), r.u32(), r.u64()) else {
                    continue;
                };
                let v = s.up.entry(test).or_default();
                if v.len() < MAX_RECS {
                    v.push((seq, su, now_us));
                }
            }
            T_PING => {
                let mut out = buf[..n].to_vec();
                out[4] = T_PONG;
                let _ = sock.send_to(&out, from);
            }
            T_KEEP => {}
            _ => {
                let Some(cookie) = r.u64() else { continue };
                if cookie != s.cookie {
                    continue;
                }
                match t {
                    T_UP_END => {
                        let (Some(test), Some(rate), Some(dur), Some(sent)) = (r.u8(), r.u32(), r.u32(), r.u32())
                        else {
                            continue;
                        };
                        if !s.up_done.contains_key(&test) {
                            let sched = schedule(rate.min(MAX_RATE_KBPS), dur.min(MAX_DUR_MS));
                            let recs = s.up.remove(&test).unwrap_or_default();
                            let st = stats(&sched, sent.min(sched.len() as u32), &recs);
                            let line = format!("↑ {} Мбит/с: {}", rate as f64 / 1000.0, fmt_stats(&st));
                            logln(&format!("{}\t{}\tсервер видит {}", s.label, s.addr, line));
                            s.up_done.insert(test, line);
                        }
                        let line = &s.up_done[&test];
                        let _ =
                            sock.send_to(&W::new(T_UP_REPORT).u64(sid).u8(test).bytes(line.as_bytes()).0, from);
                    }
                    T_DOWN_REQ => {
                        let (Some(test), Some(rate), Some(dur)) = (r.u8(), r.u32(), r.u32()) else { continue };
                        if s.down_started.contains(&test)
                            || s.downs >= MAX_DOWNS_PER_SESSION
                            || active_downs.load(Ordering::Relaxed) >= MAX_ACTIVE_DOWNS
                        {
                            continue;
                        }
                        s.down_started.insert(test);
                        s.downs += 1;
                        let rate = rate.min(MAX_RATE_KBPS);
                        let dur = dur.min(MAX_DUR_MS);
                        let sock = sock.clone();
                        let to = s.addr;
                        let alive = s.alive_ms.clone();
                        let active = active_downs.clone();
                        active.fetch_add(1, Ordering::Relaxed);
                        std::thread::spawn(move || {
                            let sched = schedule(rate, dur);
                            let stop = || {
                                let now = t0.elapsed().as_millis() as u64;
                                now.saturating_sub(alive.load(Ordering::Relaxed)) > 2000
                            };
                            let (sent, _) = send_schedule(&sock, to, sid, test, &sched, &stop);
                            for _ in 0..6 {
                                let _ = sock.send_to(&W::new(T_DOWN_END).u64(sid).u8(test).u32(sent).0, to);
                                std::thread::sleep(Duration::from_millis(50));
                            }
                            active.fetch_sub(1, Ordering::Relaxed);
                        });
                    }
                    T_REPORT => {
                        let (Some(idx), Some(_total)) = (r.u16(), r.u16()) else { continue };
                        let text = String::from_utf8_lossy(r.rest()).replace(['\t', '\n', '\r'], " ");
                        if s.reports.insert(idx) {
                            logln(&format!("{}\t{}\tклиент: {}", s.label, s.addr, text));
                        }
                        let _ = sock.send_to(&W::new(T_REPORT_ACK).u64(sid).u16(idx).0, from);
                    }
                    _ => {}
                }
            }
        }
    }
}

// ---------- клиент ----------

#[derive(Default)]
struct ClientRx {
    cookie: Option<u64>,
    pongs: HashMap<u32, u64>, // seq -> rtt мкс
    down: HashMap<u8, Vec<Rec>>,
    down_end: HashMap<u8, u32>,
    up_report: HashMap<u8, String>,
    acks: HashSet<u16>,
}

fn say(s: String, lines: &mut Vec<String>) {
    println!("{s}");
    lines.push(s);
}

fn client(server: &str, label: &str, rates: &[u32], secs: u32) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let addr = match server.to_socket_addrs().ok().and_then(|mut a| a.find(|x| x.is_ipv4())) {
        Some(a) => a,
        None => {
            say(format!("не удалось разобрать адрес сервера {server}"), &mut lines);
            return lines;
        }
    };
    let sock = Arc::new(UdpSocket::bind("0.0.0.0:0").expect("bind"));
    sock.set_read_timeout(Some(Duration::from_millis(200))).ok();
    let sid = rnd();
    let st = Arc::new(Mutex::new(ClientRx::default()));
    let t0 = Instant::now();
    let done = Arc::new(AtomicBool::new(false));
    {
        let sock = sock.clone();
        let st = st.clone();
        let done = done.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 2048];
            while !done.load(Ordering::Relaxed) {
                let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
                if from != addr {
                    continue;
                }
                let now_us = t0.elapsed().as_micros() as u64;
                let Some((t, mut r)) = R::parse(&buf[..n]) else { continue };
                if r.u64() != Some(sid) {
                    continue;
                }
                let mut g = st.lock().unwrap();
                match t {
                    T_HELLO_ACK => g.cookie = r.u64(),
                    T_PONG => {
                        if let (Some(seq), Some(su)) = (r.u32(), r.u64()) {
                            g.pongs.entry(seq).or_insert(now_us.saturating_sub(su));
                        }
                    }
                    T_DATA => {
                        if let (Some(test), Some(_), Some(seq), Some(su)) = (r.u8(), r.u8(), r.u32(), r.u64()) {
                            let v = g.down.entry(test).or_default();
                            if v.len() < MAX_RECS {
                                v.push((seq, su, now_us));
                            }
                        }
                    }
                    T_DOWN_END => {
                        if let (Some(test), Some(sent)) = (r.u8(), r.u32()) {
                            g.down_end.insert(test, sent);
                        }
                    }
                    T_UP_REPORT => {
                        if let Some(test) = r.u8() {
                            g.up_report.insert(test, String::from_utf8_lossy(r.rest()).into_owned());
                        }
                    }
                    T_REPORT_ACK => {
                        if let Some(i) = r.u16() {
                            g.acks.insert(i);
                        }
                    }
                    _ => {}
                }
            }
        });
    }

    say(format!("murmur-netprobe {} → {addr}, метка «{label}»", env!("CARGO_PKG_VERSION")), &mut lines);
    // 1. знакомство
    let mut cookie = None;
    let mut ll = label.as_bytes().to_vec();
    ll.truncate(64);
    let hello = W::new(T_HELLO).u64(sid).u8(ll.len() as u8).bytes(&ll).pad(PKT).0;
    for _ in 0..15 {
        let _ = sock.send_to(&hello, addr);
        std::thread::sleep(Duration::from_millis(200));
        cookie = st.lock().unwrap().cookie;
        if cookie.is_some() {
            break;
        }
    }
    let Some(cookie) = cookie else {
        say("сервер не отвечает (порт закрыт или UDP режется по пути)".into(), &mut lines);
        done.store(true, Ordering::Relaxed);
        return lines;
    };

    // 2. пинг: 250 пакетов по 20 мс (5 с), 200 байт
    let n_ping = 250u32;
    for seq in 0..n_ping {
        let su = t0.elapsed().as_micros() as u64;
        let p = W::new(T_PING).u64(sid).u32(seq).u64(su).pad(200).0;
        let _ = sock.send_to(&p, addr);
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(800));
    {
        let g = st.lock().unwrap();
        let mut rtts: Vec<f64> = g.pongs.values().map(|u| *u as f64 / 1000.0).collect();
        rtts.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let lost = n_ping - rtts.len() as u32;
        let line = if rtts.is_empty() {
            "пинг: ответов нет".to_string()
        } else {
            let p = |q: f64| rtts[((rtts.len() - 1) as f64 * q) as usize];
            format!(
                "пинг (50 пакетов/с): потери {:.1} % ({lost} из {n_ping}), RTT мин {:.1} / p50 {:.1} / p95 {:.1} / макс {:.1} мс",
                lost as f64 * 100.0 / n_ping as f64,
                rtts[0],
                p(0.5),
                p(0.95),
                rtts[rtts.len() - 1]
            )
        };
        say(line, &mut lines);
    }

    // 3. «трансляции» в обе стороны
    let dur_ms = secs * 1000;
    let mut test: u8 = 0;
    for &rate in rates {
        let mbit = rate as f64 / 1000.0;
        // ↑ к серверу
        test += 1;
        println!("… отправка {mbit} Мбит/с, {secs} с");
        let sched = schedule(rate, dur_ms);
        let (sent, errs) = send_schedule(&sock, addr, sid, test, &sched, &|| false);
        std::thread::sleep(Duration::from_millis(500));
        let mut rep = None;
        for _ in 0..20 {
            let p = W::new(T_UP_END).u64(sid).u64(cookie).u8(test).u32(rate).u32(dur_ms).u32(sent).0;
            let _ = sock.send_to(&p, addr);
            std::thread::sleep(Duration::from_millis(200));
            rep = st.lock().unwrap().up_report.get(&test).cloned();
            if rep.is_some() {
                break;
            }
        }
        let mut l = rep.unwrap_or_else(|| format!("↑ {mbit} Мбит/с: отчёт сервера не пришёл"));
        if errs > 0 {
            l.push_str(&format!(" (ошибок отправки у себя: {errs})"));
        }
        say(l, &mut lines);

        // ↓ от сервера
        test += 1;
        println!("… приём {mbit} Мбит/с, {secs} с");
        let req = W::new(T_DOWN_REQ).u64(sid).u64(cookie).u8(test).u32(rate).u32(dur_ms).0;
        let start = Instant::now();
        let deadline = Duration::from_millis(dur_ms as u64 + 6000);
        let mut end = None;
        while start.elapsed() < deadline {
            let (has_data, e) = {
                let g = st.lock().unwrap();
                (g.down.contains_key(&test), g.down_end.get(&test).copied())
            };
            if let Some(e) = e {
                end = Some(e);
                break;
            }
            if !has_data && start.elapsed() < Duration::from_secs(3) {
                let _ = sock.send_to(&req, addr);
            }
            let _ = sock.send_to(&W::new(T_KEEP).u64(sid).0, addr);
            std::thread::sleep(Duration::from_millis(200));
        }
        std::thread::sleep(Duration::from_millis(300));
        let recs = st.lock().unwrap().down.remove(&test).unwrap_or_default();
        let line = match end {
            Some(sent) => {
                let sched = schedule(rate, dur_ms);
                format!("↓ {mbit} Мбит/с: {}", fmt_stats(&stats(&sched, sent, &recs)))
            }
            None if recs.is_empty() => format!("↓ {mbit} Мбит/с: данные не пришли"),
            None => format!("↓ {mbit} Мбит/с: конец не пришёл, принято пакетов {}", recs.len()),
        };
        say(line, &mut lines);
    }

    // 4. отчёт на сервер
    let total = lines.len() as u16;
    for (i, l) in lines.iter().enumerate() {
        let mut text = l.as_bytes().to_vec();
        text.truncate(1100);
        let p = W::new(T_REPORT).u64(sid).u64(cookie).u16(i as u16).u16(total).bytes(&text).0;
        for _ in 0..10 {
            let _ = sock.send_to(&p, addr);
            std::thread::sleep(Duration::from_millis(100));
            if st.lock().unwrap().acks.contains(&(i as u16)) {
                break;
            }
        }
    }
    let acked = st.lock().unwrap().acks.len();
    if acked == lines.len() {
        println!("отчёт отправлен на сервер");
    } else {
        println!("отчёт на сервер дошёл частично ({acked} из {})", lines.len());
    }
    done.store(true, Ordering::Relaxed);
    lines
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok().map(|s| s.trim().to_string()))
        .unwrap_or_else(|| "?".into())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let get = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    if args.first().map(|s| s.as_str()) == Some("serve") {
        let port = get("--port").and_then(|p| p.parse().ok()).unwrap_or(7990);
        let out = get("--out").unwrap_or_else(|| "netprobe-results.log".into());
        if let Err(e) = serve(port, &out) {
            eprintln!("сервер: {e}");
            std::process::exit(1);
        }
        return;
    }
    let server = get("--server").unwrap_or_else(|| DEFAULT_SERVER.into());
    let label = get("--label").unwrap_or_else(hostname);
    let rates: Vec<u32> = get("--rates")
        .unwrap_or_else(|| "2,5,10".into())
        .split(',')
        .filter_map(|x| x.trim().parse::<f64>().ok())
        .map(|m| (m * 1000.0) as u32)
        .filter(|r| *r > 0)
        .collect();
    let secs = get("--secs").and_then(|s| s.parse().ok()).unwrap_or(10u32).clamp(3, 15);
    println!(
        "Замер связи до сервера Murmur. Займёт около {} с — не закрывайте окно.\n",
        8 + rates.len() as u32 * (secs * 2 + 3)
    );
    let lines = client(&server, &label, &rates, secs);
    let name = format!(
        "netprobe-{}-{}.txt",
        label.replace(|c: char| !c.is_alphanumeric() && c != '-', "_"),
        unix_s()
    );
    let path = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(&name)))
        .unwrap_or_else(|| name.clone().into());
    if std::fs::write(&path, lines.join("\n") + "\n").is_ok() {
        println!("\nрезультат сохранён: {}", path.display());
    }
    if cfg!(windows) && !args.iter().any(|a| a == "--no-wait") {
        println!("\nНажмите Enter, чтобы закрыть окно.");
        let mut s = String::new();
        let _ = std::io::stdin().read_line(&mut s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_rate_close_to_target() {
        let s = schedule(5000, 10_000);
        let bytes = s.len() as f64 * PKT as f64;
        let mbit = bytes * 8.0 / 10.0 / 1e6;
        assert!(mbit > 4.9 && mbit < 5.8, "{mbit}");
        assert!(s.windows(2).all(|w| w[0].0 <= w[1].0));
        assert!(s.iter().any(|x| x.1));
    }

    #[test]
    fn stats_counts_losses_and_runs() {
        let sched = schedule(2000, 3000);
        let n = sched.len() as u32;
        let recs: Vec<Rec> = (0..n).filter(|i| !(10..13).contains(i) && *i != 50).map(|i| (i, 0, 0)).collect();
        let st = stats(&sched, n, &recs);
        assert_eq!(st.lost, 4);
        assert_eq!(st.events, 2);
        assert_eq!(st.max_run, 3);
    }

    #[test]
    fn loopback_end_to_end() {
        let port = 47990;
        std::thread::spawn(move || {
            let out = std::env::temp_dir().join("netprobe-test.log");
            let _ = serve(port, out.to_str().unwrap());
        });
        std::thread::sleep(Duration::from_millis(200));
        let lines = client(&format!("127.0.0.1:{port}"), "тест", &[2000], 3);
        let all = lines.join("\n");
        assert!(all.contains("пинг"), "{all}");
        assert!(all.contains("↑ 2 Мбит/с: потери"), "{all}");
        assert!(all.contains("↓ 2 Мбит/с: потери"), "{all}");
    }
}
