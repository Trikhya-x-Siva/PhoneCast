//! Engine tests against the fake RTMP server, with a fake encoder that
//! produces correctly shaped H.264 packets in real time (no GPU needed).

use super::encoder::{BitrateGate, EncodeSpec, EncoderStats, Source, Stream, VideoEncoder};
use super::fake_server::FakeServer;
use super::rtmp::{SendQueue, SenderOptions};
use super::*;
use std::sync::atomic::{AtomicBool, AtomicU32};

/// Opens fake encoders; counts every encoder session ever opened.
struct FakeSource {
    epoch: Instant,
    opened: AtomicU32,
    encoders: Mutex<Vec<Arc<FakeEncoder>>>,
}

struct FakeEncoder {
    spec: EncodeSpec,
    gate: Mutex<BitrateGate>,
    idr: AtomicBool,
    stats: Mutex<EncoderStats>,
    stop: AtomicBool,
}

impl FakeSource {
    fn new() -> Arc<FakeSource> {
        Arc::new(FakeSource {
            epoch: Instant::now(),
            opened: AtomicU32::new(0),
            encoders: Mutex::new(Vec::new()),
        })
    }
}

const AVCC: &[u8] = &[
    1, 0x64, 0, 0x28, 0xFF, 0xE1, 0, 4, 0x67, 0x64, 0, 0x28, 1, 0, 2, 0x68, 0xEB,
];

impl Source for FakeSource {
    fn backend(&self) -> &'static str {
        "fake"
    }

    fn live_bitrate(&self) -> bool {
        true
    }

    fn open(&self, spec: EncodeSpec, out: Arc<Stream>) -> Result<Arc<dyn VideoEncoder>, String> {
        self.opened.fetch_add(1, Ordering::SeqCst);
        let enc = Arc::new(FakeEncoder {
            spec,
            gate: Mutex::new(BitrateGate::new(spec.kbps, Duration::from_secs(1))),
            idr: AtomicBool::new(false),
            stats: Mutex::new(EncoderStats {
                sessions: 1,
                kbps: spec.kbps,
                ..Default::default()
            }),
            stop: AtomicBool::new(false),
        });
        let e = enc.clone();
        let epoch = self.epoch;
        std::thread::spawn(move || e.run(epoch, out));
        self.encoders.lock().unwrap().push(enc.clone());
        Ok(enc)
    }

    fn push(&self, _m: &Media) {}

    fn stop(&self) {
        for e in self.encoders.lock().unwrap().iter() {
            e.close();
        }
    }
}

impl FakeEncoder {
    fn run(&self, epoch: Instant, out: Arc<Stream>) {
        let fps = self.spec.key.fps as u64;
        let gop = self.spec.gop() as u64;
        let start = Instant::now();
        let mut since_key = gop; // first frame is a keyframe
        let mut audio_ms = 0i64;
        let base = epoch.elapsed().as_millis() as i64;
        for n in 0u64.. {
            if self.stop.load(Ordering::SeqCst) {
                return;
            }
            let due = start + Duration::from_micros(n * 1_000_000 / fps);
            if let Some(d) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(d);
            }
            let mut forced = self.idr.swap(false, Ordering::SeqCst);
            if let Some(k) = self.gate.lock().unwrap().poll(Instant::now()) {
                let mut s = self.stats.lock().unwrap();
                s.reconfigs += 1;
                s.kbps = k;
                forced = true; // NVENC emits an IDR on reconfigure
            }
            let key = forced || since_key >= gop;
            since_key = if key { 1 } else { since_key + 1 };
            let kbps = self.stats.lock().unwrap().kbps as u64;
            let size = (kbps * 1000 / 8 / fps) as usize;
            let (hdr, priority) = match (key, since_key % 3) {
                (true, _) => (0x65, Priority::Key),
                (false, 1) => (0x41, Priority::Ref),
                _ => (0x01, Priority::Disposable),
            };
            let mut data = ((size - 4) as u32).to_be_bytes().to_vec();
            data.push(hdr);
            data.resize(size, 0xAB);
            let dts = base + (n * 1000 / fps) as i64;
            {
                let mut s = self.stats.lock().unwrap();
                s.frames += 1;
                if key && n > 0 {
                    s.idrs += 1;
                }
            }
            out.publish(Packet {
                src: 0,
                dts_ms: dts,
                kind: Kind::Video {
                    codec: VCodec::H264,
                    priority,
                    cts_ms: 0,
                },
                data: data.into(),
                config: key.then(|| AVCC.into()),
            });
            while base + audio_ms <= dts {
                out.publish(Packet {
                    src: 0,
                    dts_ms: base + audio_ms,
                    kind: Kind::Audio,
                    data: vec![0x21; 420].into(),
                    config: Some(Arc::from(&[0x11u8, 0x90][..])),
                });
                audio_ms += 21;
            }
        }
    }
}

impl VideoEncoder for FakeEncoder {
    fn spec(&self) -> EncodeSpec {
        self.spec
    }

    fn set_bitrate(&self, kbps: u32) -> bool {
        self.gate.lock().unwrap().request(kbps);
        true
    }

    fn request_idr(&self) {
        self.idr.store(true, Ordering::SeqCst);
    }

    fn stats(&self) -> EncoderStats {
        *self.stats.lock().unwrap()
    }

    fn close(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn dest(id: &str, url: String, kbps: u32, main: bool) -> DestConfig {
    DestConfig {
        id: id.into(),
        name: id.into(),
        url,
        codec: VCodec::H264,
        w: 1920,
        h: 1080,
        fps: 60,
        kbps,
        framing: Framing::Blur,
        main,
    }
}

fn opts() -> EngineOptions {
    EngineOptions {
        sender: SenderOptions {
            // Deterministic and small, so throttling shows up quickly.
            send_buffer: Some(32 << 10),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Polls `f` until true or the timeout passes.
fn wait_for(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + timeout;
    while Instant::now() < until {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    f()
}

fn live(e: &Engine, i: usize) -> bool {
    e.status()[i].state == "live"
}

#[test]
fn bitrate_change_never_restarts_encoder() {
    let server = FakeServer::start();
    server.ctl.throttle_kbps.store(1500, Ordering::SeqCst);
    let src = FakeSource::new();
    let e = Engine::start(
        src.clone(),
        vec![dest("yt", server.url("k"), 4000, true)],
        opts(),
    )
    .unwrap();
    // The first cut can overshoot (socket buffers filling count as sent);
    // later cuts converge on the throttled rate.
    let lowered = wait_for(Duration::from_secs(10), || {
        let s = &e.status()[0];
        s.target_kbps <= 1500 && s.encoder.kbps <= 1500
    });
    let frames =
        |c: &super::fake_server::Ctl| c.conns()[0].video().filter(|r| !r.seq_header).count();
    // At the lower bitrate the link keeps up again: once the old backlog
    // has drained, video flows (not just keyframes).
    std::thread::sleep(Duration::from_secs(1));
    let at_cut = frames(&server.ctl);
    std::thread::sleep(Duration::from_secs(2));
    let after = frames(&server.ctl) - at_cut;
    let s = e.status()[0].clone();
    e.stop();
    assert!(lowered, "DBR never lowered the bitrate: {s:?}");
    assert!(after >= 60, "only {after} frames in 2 s after the cut");
    assert!(
        s.target_kbps <= 1500,
        "cut to the measured throughput: {}",
        s.target_kbps
    );
    assert_eq!(src.opened.load(Ordering::SeqCst), 1, "no second encoder");
    assert_eq!(s.encoder.sessions, 1, "encoder never re-opened");
    assert!(s.encoder.idrs >= s.encoder.reconfigs, "IDR on every change");
    let conns = server.ctl.conns();
    assert_eq!(conns.len(), 1, "connection kept while lowering bitrate");
}

#[test]
fn reconnect_within_2s_resumes_on_idr() {
    let server = FakeServer::start();
    let src = FakeSource::new();
    let e = Engine::start(
        src.clone(),
        vec![dest("yt", server.url("k"), 2000, true)],
        opts(),
    )
    .unwrap();
    assert!(wait_for(Duration::from_secs(5), || live(&e, 0)));
    std::thread::sleep(Duration::from_secs(1));
    let dropped_at = Instant::now();
    server.ctl.drop_all();
    let back = wait_for(Duration::from_secs(5), || {
        let c = server.ctl.conns();
        c.len() >= 2 && c[1].video().any(|r| !r.seq_header)
    });
    let s = e.status()[0].clone();
    e.stop();
    assert!(back, "no reconnect: {s:?}");
    let conns = server.ctl.conns();
    let c = &conns[1];
    let first_video = c.video().find(|r| !r.seq_header).unwrap();
    let resumed = first_video.at.duration_since(dropped_at);
    assert!(
        resumed < Duration::from_secs(2),
        "video back after {resumed:?}"
    );
    assert_eq!(c.app, "live2");
    assert_eq!(c.key, "k");
    // Sequence header first, then a keyframe; no audio before the keyframe.
    assert!(c.media[0].typ == rtmp::MSG_VIDEO && c.media[0].seq_header);
    assert!(first_video.key, "resumed on a keyframe");
    let before_key = c
        .media
        .iter()
        .take_while(|r| !(r.typ == rtmp::MSG_VIDEO && r.key && !r.seq_header));
    assert!(before_key.filter(|r| r.typ == rtmp::MSG_AUDIO).count() == 0);
    // Timestamps restart near zero on the new connection.
    assert!(first_video.ts < 100, "ts {}", first_video.ts);
    assert_eq!(s.reconnects, 1);
    assert_eq!(src.opened.load(Ordering::SeqCst), 1, "encoder kept running");
    assert_eq!(s.encoder.sessions, 1);
}

#[test]
fn stalled_upload_reconnects_without_encoder_restart() {
    let server = FakeServer::start();
    let src = FakeSource::new();
    let mut o = opts();
    o.sender.stall_timeout = Duration::from_secs(1);
    let e = Engine::start(
        src.clone(),
        vec![dest("tw", server.url("k"), 3000, true)],
        o,
    )
    .unwrap();
    assert!(wait_for(Duration::from_secs(5), || live(&e, 0)));
    // The link goes silent: no RST, the server just stops reading.
    server.ctl.stall_conn.store(0, Ordering::SeqCst);
    let back = wait_for(Duration::from_secs(8), || {
        let c = server.ctl.conns();
        c.len() >= 2 && c[1].video().any(|r| r.key && !r.seq_header)
    });
    let s = e.status()[0].clone();
    e.stop();
    assert!(back, "stalled link was not replaced: {s:?}");
    assert_eq!(src.opened.load(Ordering::SeqCst), 1);
    // While stalled, the queue dropped frames instead of growing forever.
    assert!(s.dropped.congestion_frames > 0, "{s:?}");
}

#[test]
fn shared_encoder_splits_for_secondary_and_main_keeps_quality() {
    let main = FakeServer::start();
    let second = FakeServer::start();
    second.ctl.throttle_kbps.store(1500, Ordering::SeqCst);
    let src = FakeSource::new();
    let e = Engine::start(
        src.clone(),
        vec![
            dest("yt", main.url("a"), 4000, true),
            dest("kick", second.url("b"), 4000, false),
        ],
        opts(),
    )
    .unwrap();
    assert_eq!(
        src.opened.load(Ordering::SeqCst),
        1,
        "same preset → one encoder"
    );
    let split = wait_for(Duration::from_secs(10), || {
        let s = e.status();
        src.opened.load(Ordering::SeqCst) == 2 && s[1].target_kbps < 4000
    });
    std::thread::sleep(Duration::from_millis(1500));
    let s = e.status();
    let encs = e.encoders();
    e.stop();
    assert!(split, "secondary never moved to its own encoder: {s:?}");
    assert_eq!(s[0].target_kbps, 4000, "main keeps full quality");
    assert_eq!(s[0].encoder.reconfigs, 0, "main's encoder untouched");
    assert_eq!(s[0].encoder.sessions, 1);
    assert!(encs.iter().any(|(sp, _)| sp.kbps == 4000));
    assert_eq!(main.ctl.conns().len(), 1);
    assert_eq!(
        second.ctl.conns().len(),
        1,
        "secondary switched encoders without reconnecting"
    );
}

#[test]
fn queue_drops_never_touch_audio_or_keyframes() {
    let q = SendQueue::new(1, 10_000);
    let pkt = |dts: i64, p: Option<Priority>| Packet {
        src: 1,
        dts_ms: dts,
        kind: match p {
            Some(priority) => Kind::Video {
                codec: VCodec::H264,
                priority,
                cts_ms: 0,
            },
            None => Kind::Audio,
        },
        data: vec![0].into(),
        config: None,
    };
    // Before the first keyframe nothing is queued (clean start).
    q.push(&pkt(0, Some(Priority::Ref)));
    q.push(&pkt(0, None));
    assert_eq!(q.backlog_ms(), 0);
    let pattern = [
        Priority::Key,
        Priority::Ref,
        Priority::Disposable,
        Priority::Disposable,
    ];
    for i in 0..60 {
        q.push(&pkt(i * 16, Some(pattern[i as usize % 4])));
        q.push(&pkt(i * 16 + 8, None));
    }
    assert_eq!(q.backlog_ms(), 59 * 16 + 8);
    let count = |q: &SendQueue| {
        let mut v = Vec::new();
        while let Some(p) = q.pop(Duration::ZERO) {
            v.push(p.priority());
        }
        v
    };
    q.drop_below(Priority::Ref);
    // Incoming B-frames are dropped until a P/I-frame arrives.
    q.push(&pkt(1000, Some(Priority::Disposable)));
    q.push(&pkt(1016, Some(Priority::Ref)));
    q.push(&pkt(1032, Some(Priority::Disposable)));
    let left = count(&q);
    assert_eq!(
        left.iter().filter(|p| p.is_none()).count(),
        60,
        "all audio kept"
    );
    assert_eq!(
        left.iter().filter(|p| *p == &Some(Priority::Key)).count(),
        15
    );
    assert_eq!(
        left.iter()
            .filter(|p| *p == &Some(Priority::Disposable))
            .count(),
        1
    );
    assert_eq!(q.drops().congestion_frames, 31);

    for i in 0..8 {
        q.push(&pkt(2000 + i * 16, Some(pattern[i as usize % 4])));
    }
    q.drop_below(Priority::Key);
    q.push(&pkt(3000, Some(Priority::Ref)));
    let left = count(&q);
    assert_eq!(left, vec![Some(Priority::Key), Some(Priority::Key)]);
}
