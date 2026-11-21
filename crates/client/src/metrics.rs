//! Метрики для отладочного оверлея (F3): из статистики WebRTC (`get_stats()`) — путь соединения, RTT
//! и строка на каждый поток (скорость, потери, джиттер, кадры, энкодер/декодер).
//!
//! Чистые функции: скорости и потери считаются по разнице счётчиков между опросами (`Counters`).

use std::collections::HashMap;

use livekit::webrtc::stats::{IceCandidateType, IceServerTransportProtocol, RtcStats};

/// Как идут пакеты до SFU.
#[derive(Debug, Clone, PartialEq)]
pub enum Path {
    /// Напрямую (host/srflx/prflx), `proto` — udp/tcp.
    Direct { proto: String },
    /// Через TURN, `proto` — udp/tcp/tls (до TURN-сервера).
    Relay { proto: String },
    Unknown,
}

impl std::fmt::Display for Path {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Path::Direct { proto } => write!(f, "напрямую {}", proto.to_uppercase()),
            Path::Relay { proto } => write!(f, "TURN {}", proto.to_uppercase()),
            Path::Unknown => write!(f, "—"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Conn {
    pub path: Path,
    /// Адрес удалённого кандидата (SFU или TURN), `ip:port`.
    pub remote: String,
    /// Через какой TURN (url локального relay-кандидата), иначе пусто.
    pub turn: String,
    pub rtt_ms: Option<f64>,
    /// Оценка доступного исходящего канала (кбит/с).
    pub out_kbps_avail: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Out,
    In,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StreamRow {
    pub dir: Dir,
    /// "audio" | "video"
    pub kind: &'static str,
    pub label: String,
    pub kbps: f64,
    /// Входящие — за интервал между опросами; исходящие — по отчёту SFU (RTCP).
    pub loss_pct: Option<f64>,
    pub jitter_ms: Option<f64>,
    pub codec: String,
    pub size: Option<(u32, u32)>,
    pub fps: Option<f64>,
    /// Энкодер/декодер, ограничение качества, NACK/PLI, заморозки, «заплатки» звука.
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Metrics {
    pub conn: Conn,
    pub rows: Vec<StreamRow>,
    pub forced_relay: bool,
}

/// Счётчики прошлого опроса по id статистики: (байты, пакетов принято, пакетов потеряно, кадров декодировано).
#[derive(Debug, Default)]
pub struct Counters(HashMap<String, (u64, u64, i64, u64)>);

/// `labels`: id rtc-трека (`track_identifier`) → подпись («Вася: голос», «я: микрофон»).
pub fn collect(stats: &[RtcStats], labels: &HashMap<String, String>, prev: &mut Counters, dt_s: f64, forced_relay: bool) -> Metrics {
    let dt = dt_s.max(0.001);
    let mut next = HashMap::new();
    let label = |id: &str| labels.get(id).cloned().unwrap_or_else(|| "?".into());
    let codec = |id: &str| {
        stats
            .iter()
            .find_map(|s| match s {
                RtcStats::Codec(c) if c.rtc.id == id => Some(c.codec.mime_type.split('/').next_back().unwrap_or_default().to_string()),
                _ => None,
            })
            .unwrap_or_default()
    };
    let kbps = |id: &str, bytes: u64| match prev.0.get(id) {
        Some(&(b, ..)) if bytes >= b => (bytes - b) as f64 * 8.0 / 1000.0 / dt,
        _ => 0.0,
    };

    let mut rows = Vec::new();
    for s in stats {
        match s {
            RtcStats::OutboundRtp(o) => {
                let id = &o.rtc.id;
                let bytes = o.sent.bytes_sent;
                let track = stats.iter().find_map(|m| match m {
                    RtcStats::MediaSource(ms) if ms.rtc.id == o.outbound.media_source_id => Some(ms.source.track_identifier.as_str()),
                    _ => None,
                });
                let remote = stats.iter().find_map(|r| match r {
                    RtcStats::RemoteInboundRtp(r) if r.remote_inbound.local_id == *id => Some(r),
                    _ => None,
                });
                let video = o.stream.kind == "video";
                let detail = if video {
                    format!(
                        "{} · лимит {:?} · цель {:.0}k · NACK {} PLI {}",
                        o.outbound.encoder_implementation,
                        o.outbound.quality_limitation_reason,
                        o.outbound.target_bitrate / 1000.0,
                        o.outbound.nack_count,
                        o.outbound.pli_count
                    )
                } else {
                    String::new()
                };
                rows.push(StreamRow {
                    dir: Dir::Out,
                    kind: if video { "video" } else { "audio" },
                    label: track.map(label).unwrap_or_else(|| "?".into()),
                    kbps: kbps(id, bytes),
                    loss_pct: remote.map(|r| r.remote_inbound.fraction_lost * 100.0),
                    jitter_ms: remote.map(|r| r.received.jitter * 1000.0),
                    codec: codec(&o.stream.codec_id),
                    size: (video && o.outbound.frame_width > 0).then_some((o.outbound.frame_width, o.outbound.frame_height)),
                    fps: video.then_some(o.outbound.frames_per_second),
                    detail,
                });
                next.insert(id.clone(), (bytes, 0, 0, 0));
            }
            RtcStats::InboundRtp(i) => {
                let id = &i.rtc.id;
                let bytes = i.inbound.bytes_received;
                let (recv, lost) = (i.received.packets_received, i.received.packets_lost);
                let loss_pct = match prev.0.get(id) {
                    Some(&(_, r0, l0, _)) if recv >= r0 && lost >= l0 => {
                        let (dr, dl) = ((recv - r0) as f64, (lost - l0) as f64);
                        (dr + dl > 0.0).then(|| dl / (dr + dl) * 100.0)
                    }
                    _ => None,
                };
                let video = i.stream.kind == "video";
                // framesPerSecond у входящего libwebrtc здесь не заполняет — считаем по приросту декодированных кадров.
                let frames = u64::from(i.inbound.frames_decoded);
                let fps = match prev.0.get(id) {
                    _ if i.inbound.frames_per_second > 0.0 => i.inbound.frames_per_second,
                    Some(&(.., f0)) if frames >= f0 => (frames - f0) as f64 / dt,
                    _ => 0.0,
                };
                let detail = if video {
                    format!(
                        "{} · заморозок {} · выброшено {} · NACK {} PLI {}",
                        i.inbound.decoder_implementation, i.inbound.freeze_count, i.inbound.frames_dropped, i.inbound.nack_count, i.inbound.pli_count
                    )
                } else if i.inbound.total_samples_received > 0 {
                    format!("заплатки {:.1}%", i.inbound.concealed_samples as f64 * 100.0 / i.inbound.total_samples_received as f64)
                } else {
                    String::new()
                };
                rows.push(StreamRow {
                    dir: Dir::In,
                    kind: if video { "video" } else { "audio" },
                    label: label(&i.inbound.track_identifier),
                    kbps: kbps(id, bytes),
                    loss_pct,
                    jitter_ms: Some(i.received.jitter * 1000.0),
                    codec: codec(&i.stream.codec_id),
                    size: (video && i.inbound.frame_width > 0).then_some((i.inbound.frame_width, i.inbound.frame_height)),
                    fps: video.then_some(fps),
                    detail,
                });
                next.insert(id.clone(), (bytes, recv, lost, frames));
            }
            _ => {}
        }
    }
    rows.sort_by(|a, b| (a.dir == Dir::In, a.kind, &a.label).cmp(&(b.dir == Dir::In, b.kind, &b.label)));
    prev.0 = next;
    Metrics { conn: conn(stats), rows, forced_relay }
}

/// Выбранная пара кандидатов: по `Transport.selected_candidate_pair_id`, иначе первая `nominated`.
fn conn(stats: &[RtcStats]) -> Conn {
    let selected: Vec<&str> = stats
        .iter()
        .filter_map(|s| match s {
            RtcStats::Transport(t) if !t.transport.selected_candidate_pair_id.is_empty() => Some(t.transport.selected_candidate_pair_id.as_str()),
            _ => None,
        })
        .collect();
    let pair = stats
        .iter()
        .find_map(|s| match s {
            RtcStats::CandidatePair(p) if selected.contains(&p.rtc.id.as_str()) => Some(p),
            _ => None,
        })
        .or_else(|| {
            stats.iter().find_map(|s| match s {
                RtcStats::CandidatePair(p) if p.candidate_pair.nominated => Some(p),
                _ => None,
            })
        });
    let Some(pair) = pair else {
        return Conn { path: Path::Unknown, remote: String::new(), turn: String::new(), rtt_ms: None, out_kbps_avail: None };
    };
    let cp = &pair.candidate_pair;
    let local = stats.iter().find_map(|s| match s {
        RtcStats::LocalCandidate(c) if c.rtc.id == cp.local_candidate_id => Some(&c.local_candidate),
        _ => None,
    });
    let remote = stats.iter().find_map(|s| match s {
        RtcStats::RemoteCandidate(c) if c.rtc.id == cp.remote_candidate_id => Some(&c.remote_candidate),
        _ => None,
    });
    let path = match local {
        Some(l) if l.candidate_type == Some(IceCandidateType::Relay) => Path::Relay {
            proto: match l.relay_protocol {
                Some(IceServerTransportProtocol::Udp) => "udp",
                Some(IceServerTransportProtocol::Tcp) => "tcp",
                Some(IceServerTransportProtocol::Tls) => "tls",
                None => "?",
            }
            .into(),
        },
        Some(l) => Path::Direct { proto: l.protocol.clone() },
        None => Path::Unknown,
    };
    let turn = match (&path, local) {
        (Path::Relay { .. }, Some(l)) => l.url.clone(),
        _ => String::new(),
    };
    Conn {
        path,
        remote: remote.map(|r| format!("{}:{}", r.address, r.port)).unwrap_or_default(),
        turn,
        rtt_ms: (cp.current_round_trip_time > 0.0).then_some(cp.current_round_trip_time * 1000.0),
        out_kbps_avail: (cp.available_outgoing_bitrate > 0.0).then_some(cp.available_outgoing_bitrate / 1000.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stat(v: serde_json::Value) -> RtcStats {
        serde_json::from_value(v).unwrap()
    }

    fn pair_stats(cand_type: &str, relay: Option<&str>) -> Vec<RtcStats> {
        let mut local = json!({"type":"local-candidate","id":"L1","timestamp":0,"address":"10.0.0.2","port":5000,"protocol":"udp","candidateType":cand_type});
        if let Some(r) = relay {
            local["relayProtocol"] = json!(r);
            local["url"] = json!("turns:turn.murmurchat.ru:443?transport=tcp");
        }
        vec![
            stat(json!({"type":"transport","id":"T1","timestamp":0,"selectedCandidatePairId":"P1"})),
            stat(json!({"type":"candidate-pair","id":"P0","timestamp":0,"localCandidateId":"X","remoteCandidateId":"X","nominated":false})),
            stat(json!({"type":"candidate-pair","id":"P1","timestamp":0,"localCandidateId":"L1","remoteCandidateId":"R1","nominated":true,
                "currentRoundTripTime":0.042,"availableOutgoingBitrate":3_000_000.0})),
            stat(local),
            stat(json!({"type":"remote-candidate","id":"R1","timestamp":0,"address":"77.222.35.128","port":443,"protocol":"udp","candidateType":"host"})),
        ]
    }

    fn run(stats: &[RtcStats], prev: &mut Counters) -> Metrics {
        let labels = HashMap::from([("TR_a".to_string(), "Вася: голос".to_string())]);
        collect(stats, &labels, prev, 1.0, false)
    }

    #[test]
    fn relay_tls_path() {
        let m = run(&pair_stats("relay", Some("tls")), &mut Counters::default());
        assert_eq!(m.conn.path, Path::Relay { proto: "tls".into() });
        assert_eq!(m.conn.path.to_string(), "TURN TLS");
        assert_eq!(m.conn.remote, "77.222.35.128:443");
        assert!((m.conn.rtt_ms.unwrap() - 42.0).abs() < 1e-9);
        assert_eq!(m.conn.out_kbps_avail, Some(3000.0));
        assert_eq!(m.conn.turn, "turns:turn.murmurchat.ru:443?transport=tcp");
    }

    #[test]
    fn direct_udp_path() {
        let m = run(&pair_stats("host", None), &mut Counters::default());
        assert_eq!(m.conn.path, Path::Direct { proto: "udp".into() });
        assert_eq!(m.conn.path.to_string(), "напрямую UDP");
        assert_eq!(m.conn.turn, "");
    }

    #[test]
    fn no_pair_unknown() {
        let m = run(&[], &mut Counters::default());
        assert_eq!(m.conn.path, Path::Unknown);
        assert_eq!(m.conn.rtt_ms, None);
        assert!(m.rows.is_empty());
    }

    fn inbound(bytes: u64, recv: u64, lost: i64, track: &str) -> RtcStats {
        stat(json!({"type":"inbound-rtp","id":"IA","timestamp":0,"kind":"audio","codecId":"C1","trackIdentifier":track,
            "bytesReceived":bytes,"packetsReceived":recv,"packetsLost":lost,"jitter":0.005,
            "totalSamplesReceived":1000,"concealedSamples":20}))
    }

    #[test]
    fn inbound_rate_and_loss() {
        let codec = stat(json!({"type":"codec","id":"C1","timestamp":0,"mimeType":"audio/opus","channels":2}));
        let mut prev = Counters::default();
        let m = run(&[inbound(1000, 100, 0, "TR_a"), codec.clone()], &mut prev);
        assert_eq!(m.rows[0].kbps, 0.0);
        assert_eq!(m.rows[0].loss_pct, None);
        let m = run(&[inbound(126_000, 190, 10, "TR_a"), codec], &mut prev);
        let r = &m.rows[0];
        assert_eq!((r.dir, r.kind, r.label.as_str(), r.codec.as_str()), (Dir::In, "audio", "Вася: голос", "opus"));
        assert!((r.kbps - 1000.0).abs() < 1e-9);
        assert!((r.loss_pct.unwrap() - 10.0).abs() < 1e-9);
        assert!((r.jitter_ms.unwrap() - 5.0).abs() < 1e-9);
        assert_eq!(r.detail, "заплатки 2.0%");
    }

    #[test]
    fn counter_reset_gives_zero() {
        let mut prev = Counters::default();
        run(&[inbound(500_000, 1000, 5, "TR_a")], &mut prev);
        let m = run(&[inbound(1000, 10, 0, "TR_a")], &mut prev);
        assert_eq!(m.rows[0].kbps, 0.0);
        assert_eq!(m.rows[0].loss_pct, None);
    }

    #[test]
    fn inbound_video_fps_from_decoded_frames() {
        let v = |frames: u32| stat(json!({"type":"inbound-rtp","id":"IV","timestamp":0,"kind":"video","trackIdentifier":"TR_a",
            "framesDecoded":frames,"frameWidth":1920,"frameHeight":1080}));
        let mut prev = Counters::default();
        run(&[v(100)], &mut prev);
        let m = collect(&[v(160)], &HashMap::new(), &mut prev, 2.0, false);
        assert_eq!(m.rows[0].fps, Some(30.0));
        assert_eq!(m.rows[0].size, Some((1920, 1080)));
    }

    #[test]
    fn unlabeled_stream_shown_as_question() {
        let m = run(&[inbound(1, 1, 0, "TR_gone")], &mut Counters::default());
        assert_eq!(m.rows.len(), 1);
        assert_eq!(m.rows[0].label, "?");
    }

    #[test]
    fn outbound_video_with_remote_loss() {
        let stats = [
            stat(json!({"type":"media-source","id":"S1","timestamp":0,"kind":"video","trackIdentifier":"TR_me"})),
            stat(json!({"type":"outbound-rtp","id":"O1","timestamp":0,"kind":"video","mediaSourceId":"S1","bytesSent":250_000,
                "frameWidth":1920,"frameHeight":1080,"framesPerSecond":30.0,"encoderImplementation":"NVENC","qualityLimitationReason":"bandwidth"})),
            stat(json!({"type":"remote-inbound-rtp","id":"RI1","timestamp":0,"kind":"video","localId":"O1","fractionLost":0.03,"jitter":0.002})),
        ];
        let labels = HashMap::from([("TR_me".to_string(), "я: экран".to_string())]);
        let m = collect(&stats, &labels, &mut Counters::default(), 1.0, true);
        let r = &m.rows[0];
        assert_eq!((r.dir, r.kind, r.label.as_str()), (Dir::Out, "video", "я: экран"));
        assert_eq!(r.size, Some((1920, 1080)));
        assert!((r.loss_pct.unwrap() - 3.0).abs() < 1e-9);
        assert!(r.detail.starts_with("NVENC · лимит Bandwidth"));
        assert!(m.forced_relay);
    }
}
