//! A pure-Rust RTMP ingest for tests: accepts publishes, records every media
//! message, and can throttle, stall or drop connections on command.

use super::amf::{decode_all, encode_all, Amf};
use super::rtmp::{self, ChunkReader, MSG_AUDIO, MSG_COMMAND, MSG_VIDEO};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Rec {
    pub at: Instant,
    pub typ: u8,
    pub ts: u32,
    pub key: bool,
    pub seq_header: bool,
    pub len: usize,
}

#[derive(Clone, Debug)]
pub struct ConnLog {
    pub opened: Instant,
    pub published: Option<Instant>,
    pub closed: Option<Instant>,
    pub app: String,
    pub key: String,
    pub fourcc_list: bool,
    pub media: Vec<Rec>,
}

impl ConnLog {
    pub fn video(&self) -> impl Iterator<Item = &Rec> {
        self.media.iter().filter(|r| r.typ == MSG_VIDEO)
    }
}

#[derive(Default)]
pub struct Ctl {
    /// Read rate limit in kbps (0 = unlimited).
    pub throttle_kbps: AtomicU64,
    /// Connection index that stops reading (-1 = none).
    pub stall_conn: AtomicI64,
    /// Bumping this closes every open connection.
    pub drop_gen: AtomicU32,
    pub stop: AtomicBool,
    pub conns: Mutex<Vec<ConnLog>>,
}

impl Ctl {
    pub fn drop_all(&self) {
        self.drop_gen.fetch_add(1, Ordering::SeqCst);
    }

    pub fn conns(&self) -> Vec<ConnLog> {
        self.conns.lock().unwrap().clone()
    }
}

pub struct FakeServer {
    pub port: u16,
    pub ctl: Arc<Ctl>,
}

impl FakeServer {
    pub fn start() -> FakeServer {
        let sock =
            socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
        // A small receive window, so throttling shows up as client backlog fast.
        sock.set_recv_buffer_size(64 << 10).unwrap();
        sock.bind(
            &"127.0.0.1:0"
                .parse::<std::net::SocketAddr>()
                .unwrap()
                .into(),
        )
        .unwrap();
        sock.listen(8).unwrap();
        let listener: TcpListener = sock.into();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let ctl = Arc::new(Ctl {
            stall_conn: AtomicI64::new(-1),
            ..Default::default()
        });
        let c = ctl.clone();
        std::thread::spawn(move || {
            while !c.stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((s, _)) => {
                        let idx = {
                            let mut conns = c.conns.lock().unwrap();
                            conns.push(ConnLog {
                                opened: Instant::now(),
                                published: None,
                                closed: None,
                                app: String::new(),
                                key: String::new(),
                                fourcc_list: false,
                                media: Vec::new(),
                            });
                            conns.len() - 1
                        };
                        let c2 = c.clone();
                        std::thread::spawn(move || {
                            serve(s, idx, &c2);
                            c2.conns.lock().unwrap()[idx].closed = Some(Instant::now());
                        });
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        });
        FakeServer { port, ctl }
    }

    pub fn url(&self, key: &str) -> String {
        format!("rtmp://127.0.0.1:{}/live2/{key}", self.port)
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        self.ctl.stop.store(true, Ordering::SeqCst);
        self.ctl.drop_all();
    }
}

fn send(s: &mut TcpStream, chunk: usize, csid: u32, typ: u8, sid: u32, payload: &[u8]) {
    let mut out = Vec::new();
    rtmp::write_message(&mut out, csid, 0, typ, sid, payload, chunk);
    let _ = s.write_all(&out);
}

fn serve(mut s: TcpStream, idx: usize, ctl: &Ctl) {
    s.set_nonblocking(false).ok();
    s.set_read_timeout(Some(Duration::from_millis(20))).ok();
    let gen = ctl.drop_gen.load(Ordering::SeqCst);
    // Handshake: C0C1 → S0S1S2 → C2.
    let mut c01 = vec![0u8; 1537];
    if read_full(&mut s, &mut c01, ctl, gen).is_err() {
        return;
    }
    let mut reply = vec![3u8];
    reply.extend_from_slice(&[0u8; 1536]);
    reply.extend_from_slice(&c01[1..]);
    if s.write_all(&reply).is_err() {
        return;
    }
    let mut c2 = vec![0u8; 1536];
    if read_full(&mut s, &mut c2, ctl, gen).is_err() {
        return;
    }

    let mut reader = ChunkReader::default();
    let mut chunk_out = 128;
    let mut buf = vec![0u8; 64 << 10];
    let mut window_start = Instant::now();
    let mut window_bytes = 0u64;
    loop {
        if ctl.drop_gen.load(Ordering::SeqCst) != gen {
            let _ = s.shutdown(std::net::Shutdown::Both);
            return;
        }
        if ctl.stall_conn.load(Ordering::SeqCst) == idx as i64 {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        let mut max = buf.len();
        let kbps = ctl.throttle_kbps.load(Ordering::SeqCst);
        if kbps > 0 {
            // Token bucket in 10 ms slices.
            if window_start.elapsed() >= Duration::from_millis(10) {
                window_start = Instant::now();
                window_bytes = 0;
            }
            let budget = (kbps * 125 / 100).saturating_sub(window_bytes) as usize;
            if budget == 0 {
                std::thread::sleep(Duration::from_millis(2));
                continue;
            }
            max = max.min(budget);
        }
        let n = match s.read(&mut buf[..max]) {
            Ok(0) => return,
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(_) => return,
        };
        window_bytes += n as u64;
        reader.feed(&buf[..n]);
        while let Ok(Some(m)) = reader.next() {
            match m.typ {
                MSG_AUDIO | MSG_VIDEO => {
                    let p = &m.payload;
                    let (key, seq) = if m.typ == MSG_VIDEO && !p.is_empty() {
                        let ft = (p[0] >> 4) & 7;
                        let seq = if p[0] & 0x80 != 0 {
                            p[0] & 0x0F == 0
                        } else {
                            p.get(1) == Some(&0)
                        };
                        (ft == 1, seq)
                    } else {
                        (false, p.get(1) == Some(&0))
                    };
                    ctl.conns.lock().unwrap()[idx].media.push(Rec {
                        at: Instant::now(),
                        typ: m.typ,
                        ts: m.ts,
                        key,
                        seq_header: seq,
                        len: p.len(),
                    });
                }
                MSG_COMMAND => {
                    let v = decode_all(&m.payload).unwrap_or_default();
                    let name = v.first().and_then(Amf::as_str).unwrap_or("").to_string();
                    let txn = v.get(1).cloned().unwrap_or(Amf::Number(0.0));
                    match name.as_str() {
                        "connect" => {
                            let o = v.get(2).cloned().unwrap_or(Amf::Null);
                            {
                                let mut conns = ctl.conns.lock().unwrap();
                                let c = &mut conns[idx];
                                c.app = o.get("app").and_then(Amf::as_str).unwrap_or("").into();
                                c.fourcc_list = o.get("fourCcList").is_some();
                            }
                            send(&mut s, chunk_out, 2, 5, 0, &5_000_000u32.to_be_bytes());
                            send(&mut s, chunk_out, 2, 6, 0, &[0, 0x4C, 0x4B, 0x40, 2]);
                            send(&mut s, chunk_out, 2, 1, 0, &4096u32.to_be_bytes());
                            chunk_out = 4096;
                            let res = encode_all(&[
                                Amf::str("_result"),
                                txn,
                                Amf::obj(&[("fmsVer", Amf::str("FMS/3,0,1,123"))]),
                                Amf::obj(&[
                                    ("level", Amf::str("status")),
                                    ("code", Amf::str("NetConnection.Connect.Success")),
                                ]),
                            ]);
                            send(&mut s, chunk_out, 3, MSG_COMMAND, 0, &res);
                        }
                        "createStream" => {
                            let res = encode_all(&[
                                Amf::str("_result"),
                                txn,
                                Amf::Null,
                                Amf::Number(1.0),
                            ]);
                            send(&mut s, chunk_out, 3, MSG_COMMAND, 0, &res);
                        }
                        "publish" => {
                            {
                                let mut conns = ctl.conns.lock().unwrap();
                                conns[idx].published = Some(Instant::now());
                                conns[idx].key =
                                    v.get(3).and_then(Amf::as_str).unwrap_or("").into();
                            }
                            let res = encode_all(&[
                                Amf::str("onStatus"),
                                Amf::Number(0.0),
                                Amf::Null,
                                Amf::obj(&[
                                    ("level", Amf::str("status")),
                                    ("code", Amf::str("NetStream.Publish.Start")),
                                ]),
                            ]);
                            send(&mut s, chunk_out, 5, MSG_COMMAND, 1, &res);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
}

fn read_full(s: &mut TcpStream, buf: &mut [u8], ctl: &Ctl, gen: u32) -> Result<(), ()> {
    let mut got = 0;
    let until = Instant::now() + Duration::from_secs(5);
    while got < buf.len() {
        if Instant::now() > until || ctl.drop_gen.load(Ordering::SeqCst) != gen {
            return Err(());
        }
        match s.read(&mut buf[got..]) {
            Ok(0) => return Err(()),
            Ok(n) => got += n,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return Err(()),
        }
    }
    Ok(())
}
