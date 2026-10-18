//! RTMP/RTMPS publisher, one per destination.
//!
//! Each destination has its own [`SendQueue`] measured in time (DTS span),
//! so one slow platform never holds up another. On a network failure only
//! the socket is reconnected (first retry after 0.5 s, then 2 s growing
//! ×1.5): the encoder keeps running, the queue is cleared and sending
//! resumes on a fresh IDR requested from the encoder.
//!
//! Own implementation instead of `rml_rtmp`: we need the enhanced-RTMP
//! `fourCcList` in `connect` for HEVC, control over chunk sizes, and a
//! non-blocking read side next to a time-measured send queue; the protocol
//! subset for publishing is small (handshake, chunking, five commands).

use super::amf::{decode_all, encode_all, Amf};
use super::dbr::Meter;
use super::flv::{self, Meta};
use super::{Bytes, Kind, Packet, Priority};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const OUT_CHUNK_SIZE: usize = 4096;
const CSID_CONTROL: u32 = 2;
const CSID_COMMAND: u32 = 3;
const CSID_AUDIO: u32 = 4;
const CSID_VIDEO: u32 = 6;
const CSID_DATA: u32 = 5;

pub const MSG_SET_CHUNK_SIZE: u8 = 1;
pub const MSG_ACK: u8 = 3;
pub const MSG_USER_CONTROL: u8 = 4;
pub const MSG_WIN_ACK_SIZE: u8 = 5;
pub const MSG_SET_PEER_BW: u8 = 6;
pub const MSG_AUDIO: u8 = 8;
pub const MSG_VIDEO: u8 = 9;
pub const MSG_DATA: u8 = 18;
pub const MSG_COMMAND: u8 = 20;

// ---------------- URL ----------------

#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    pub app: String,
    pub key: String,
    pub tc_url: String,
}

/// `rtmp[s]://host[:port]/app/streamkey` (the key may contain `/` or `?`).
pub fn parse_url(url: &str) -> Result<Target, String> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("rtmps://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("rtmp://") {
        (false, r)
    } else {
        return Err("Stream URL must start with rtmp:// or rtmps://".into());
    };
    let (hostport, path) = rest.split_once('/').ok_or("Stream URL has no app")?;
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().map_err(|_| "Bad port in stream URL")?),
        None => (hostport, if tls { 443 } else { 1935 }),
    };
    let (app, key) = path.split_once('/').unwrap_or((path, ""));
    if host.is_empty() || app.is_empty() {
        return Err("Stream URL is incomplete".into());
    }
    Ok(Target {
        tls,
        host: host.into(),
        port,
        app: app.into(),
        key: key.into(),
        tc_url: format!(
            "{}://{host}:{port}/{app}",
            if tls { "rtmps" } else { "rtmp" }
        ),
    })
}

// ---------------- chunk stream ----------------

/// Appends one RTMP message, split into chunks, to `out`.
pub fn write_message(
    out: &mut Vec<u8>,
    csid: u32,
    ts: u32,
    typ: u8,
    stream_id: u32,
    payload: &[u8],
    chunk_size: usize,
) {
    let ext = ts >= 0xFF_FFFF;
    let ts24 = if ext { 0xFF_FFFF } else { ts };
    let basic = |out: &mut Vec<u8>, fmt: u8| {
        if csid < 64 {
            out.push(fmt << 6 | csid as u8);
        } else {
            out.push(fmt << 6 | 1);
            out.extend_from_slice(&((csid - 64) as u16).to_le_bytes());
        }
    };
    basic(out, 0);
    out.extend_from_slice(&ts24.to_be_bytes()[1..]);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes()[1..]);
    out.push(typ);
    out.extend_from_slice(&stream_id.to_le_bytes());
    if ext {
        out.extend_from_slice(&ts.to_be_bytes());
    }
    for (i, chunk) in payload.chunks(chunk_size.max(1)).enumerate() {
        if i > 0 {
            basic(out, 3);
            if ext {
                out.extend_from_slice(&ts.to_be_bytes());
            }
        }
        out.extend_from_slice(chunk);
    }
}

#[derive(Clone, Debug)]
pub struct Message {
    pub typ: u8,
    pub ts: u32,
    pub stream_id: u32,
    pub payload: Vec<u8>,
}

#[derive(Default, Clone)]
struct ChunkState {
    ts: u32,
    delta: u32,
    len: usize,
    typ: u8,
    stream_id: u32,
    ext: bool,
    payload: Vec<u8>,
}

/// Incremental chunk-stream parser: feed bytes, take complete messages.
pub struct ChunkReader {
    buf: Vec<u8>,
    chunk_size: usize,
    streams: HashMap<u32, ChunkState>,
    pub bytes_in: u64,
}

impl Default for ChunkReader {
    fn default() -> Self {
        ChunkReader {
            buf: Vec::new(),
            chunk_size: 128,
            streams: HashMap::new(),
            bytes_in: 0,
        }
    }
}

impl ChunkReader {
    pub fn feed(&mut self, data: &[u8]) {
        self.bytes_in += data.len() as u64;
        self.buf.extend_from_slice(data);
    }

    /// Next complete message, or None if more bytes are needed.
    pub fn next(&mut self) -> Result<Option<Message>, String> {
        loop {
            let Some((used, done)) = self.parse_chunk()? else {
                return Ok(None);
            };
            self.buf.drain(..used);
            if let Some(m) = done {
                if m.typ == MSG_SET_CHUNK_SIZE && m.payload.len() >= 4 {
                    let n = u32::from_be_bytes(m.payload[..4].try_into().unwrap()) & 0x7FFF_FFFF;
                    self.chunk_size = (n as usize).max(1);
                }
                return Ok(Some(m));
            }
        }
    }

    fn parse_chunk(&mut self) -> Result<Option<(usize, Option<Message>)>, String> {
        let b = &self.buf;
        if b.is_empty() {
            return Ok(None);
        }
        let fmt = b[0] >> 6;
        let (csid, mut p) = match b[0] & 0x3F {
            0 if b.len() >= 2 => (64 + b[1] as u32, 2),
            1 if b.len() >= 3 => (64 + b[1] as u32 + 256 * b[2] as u32, 3),
            0 | 1 => return Ok(None),
            c => (c as u32, 1),
        };
        let hdr = [11, 7, 3, 0][fmt as usize];
        if b.len() < p + hdr {
            return Ok(None);
        }
        let u24 = |s: &[u8]| u32::from_be_bytes([0, s[0], s[1], s[2]]);
        let mut st = self.streams.get(&csid).cloned().unwrap_or_default();
        if fmt != 0 && !self.streams.contains_key(&csid) {
            return Err(format!("chunk fmt {fmt} on new chunk stream {csid}"));
        }
        let starting = st.payload.is_empty();
        let mut field = None;
        match fmt {
            0 => {
                field = Some(u24(&b[p..]));
                st.len = u24(&b[p + 3..]) as usize;
                st.typ = b[p + 6];
                st.stream_id = u32::from_le_bytes(b[p + 7..p + 11].try_into().unwrap());
            }
            1 => {
                field = Some(u24(&b[p..]));
                st.len = u24(&b[p + 3..]) as usize;
                st.typ = b[p + 6];
            }
            2 => field = Some(u24(&b[p..])),
            _ => {}
        }
        p += hdr;
        if let Some(f) = field {
            st.ext = f == 0xFF_FFFF;
        }
        let mut value = field.unwrap_or(st.delta);
        if st.ext {
            if b.len() < p + 4 {
                return Ok(None);
            }
            value = u32::from_be_bytes(b[p..p + 4].try_into().unwrap());
            p += 4;
        }
        if starting {
            match fmt {
                0 => st.ts = value,
                1 | 2 => {
                    st.delta = value;
                    st.ts = st.ts.wrapping_add(value);
                }
                _ => st.ts = st.ts.wrapping_add(st.delta),
            }
        }
        let take = (st.len - st.payload.len()).min(self.chunk_size);
        if b.len() < p + take {
            return Ok(None);
        }
        st.payload.extend_from_slice(&b[p..p + take]);
        p += take;
        let done = if st.payload.len() >= st.len {
            let m = Message {
                typ: st.typ,
                ts: st.ts,
                stream_id: st.stream_id,
                payload: std::mem::take(&mut st.payload),
            };
            Some(m)
        } else {
            None
        };
        self.streams.insert(csid, st);
        Ok(Some((p, done)))
    }
}

/// C0+C1 for the simple (non-digest) handshake every ingest accepts.
pub fn handshake_c0c1() -> Vec<u8> {
    let mut v = vec![3u8];
    v.extend_from_slice(&[0u8; 8]);
    let mut x: u32 = 0x9E37_79B9 ^ std::process::id();
    for _ in 0..1528 {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        v.push(x as u8);
    }
    v
}

// ---------------- transport ----------------

enum Transport {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Transport {
    fn tcp(&self) -> &TcpStream {
        match self {
            Transport::Plain(s) => s,
            Transport::Tls(s) => &s.sock,
        }
    }
}

impl Read for Transport {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Transport::Plain(s) => s.read(buf),
            Transport::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Transport::Plain(s) => s.write(buf),
            Transport::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Transport::Plain(s) => s.flush(),
            Transport::Tls(s) => s.flush(),
        }
    }
}

fn tls_config() -> Arc<rustls::ClientConfig> {
    static CFG: std::sync::OnceLock<Arc<rustls::ClientConfig>> = std::sync::OnceLock::new();
    CFG.get_or_init(|| {
        let roots = rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        };
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        Arc::new(
            rustls::ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("TLS versions")
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    })
    .clone()
}

/// Windows TCP statistics (SIO_TCP_INFO) for the status line and DBR.
#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct TcpStats {
    pub rtt_us: u32,
    pub min_rtt_us: u32,
    pub bytes_in_flight: u32,
    pub cwnd: u32,
    pub bytes_retrans: u32,
    pub bytes_out: u64,
    /// The OS's ideal send backlog (bandwidth-delay product estimate).
    pub ideal_backlog: u32,
}

#[cfg(windows)]
mod tcpinfo {
    use super::TcpStats;
    use std::net::TcpStream;
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{TCP_INFO_v0, WSAIoctl, SIO_TCP_INFO};

    const SIO_IDEAL_SEND_BACKLOG_QUERY: u32 = 0x4004_747B;

    pub fn query(s: &TcpStream) -> Option<TcpStats> {
        let sock = s.as_raw_socket() as usize;
        let version: u32 = 0;
        // SAFETY: plain ioctls on a live socket we own, with correctly sized
        // in/out buffers; no overlapped I/O.
        unsafe {
            let mut info: TCP_INFO_v0 = std::mem::zeroed();
            let mut n = 0u32;
            let rc = WSAIoctl(
                sock,
                SIO_TCP_INFO,
                &version as *const u32 as *const _,
                4,
                &mut info as *mut TCP_INFO_v0 as *mut _,
                std::mem::size_of::<TCP_INFO_v0>() as u32,
                &mut n,
                std::ptr::null_mut(),
                None,
            );
            if rc != 0 {
                return None;
            }
            let mut isb = 0u32;
            WSAIoctl(
                sock,
                SIO_IDEAL_SEND_BACKLOG_QUERY,
                std::ptr::null(),
                0,
                &mut isb as *mut u32 as *mut _,
                4,
                &mut n,
                std::ptr::null_mut(),
                None,
            );
            Some(TcpStats {
                rtt_us: info.RttUs,
                min_rtt_us: info.MinRttUs,
                bytes_in_flight: info.BytesInFlight,
                cwnd: info.Cwnd,
                bytes_retrans: info.BytesRetrans,
                bytes_out: info.BytesOut,
                ideal_backlog: isb,
            })
        }
    }
}

#[cfg(not(windows))]
mod tcpinfo {
    pub fn query(_: &std::net::TcpStream) -> Option<super::TcpStats> {
        None
    }
}

/// Like OBS: keep the OS send buffer at its ideal backlog (≈ one RTT of
/// data), so congestion shows up in our queue quickly instead of hiding in
/// a multi-megabyte kernel buffer.
fn tune_send_buffer(s: &TcpStream, fixed: Option<usize>, stats: Option<&TcpStats>) {
    let size = match (fixed, stats) {
        (Some(f), _) => f,
        (None, Some(st)) if st.ideal_backlog > 0 => {
            (st.ideal_backlog as usize).clamp(64 << 10, 8 << 20)
        }
        _ => return,
    };
    socket2::SockRef::from(s).set_send_buffer_size(size).ok();
}

// ---------------- connection ----------------

pub struct Connection {
    t: Transport,
    reader: ChunkReader,
    stream_id: u32,
    out: Vec<u8>,
    pub bytes_out: u64,
}

impl Connection {
    /// TCP (+TLS with SNI) connect, handshake, connect/createStream/publish.
    pub fn open(target: &Target, opts: &SenderOptions, hevc: bool) -> Result<Connection, String> {
        let addrs: Vec<_> = (target.host.as_str(), target.port)
            .to_socket_addrs()
            .map_err(|e| format!("Cannot resolve {}: {e}", target.host))?
            .collect();
        let mut last = String::from("no address");
        let tcp = addrs
            .iter()
            .find_map(
                |a| match TcpStream::connect_timeout(a, opts.connect_timeout) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        last = e.to_string();
                        None
                    }
                },
            )
            .ok_or_else(|| format!("Cannot connect to {}: {last}", target.host))?;
        tcp.set_nodelay(true).ok();
        tcp.set_read_timeout(Some(opts.connect_timeout)).ok();
        tcp.set_write_timeout(Some(opts.stall_timeout)).ok();
        tune_send_buffer(&tcp, opts.send_buffer, tcpinfo::query(&tcp).as_ref());
        let t = if target.tls {
            let name = rustls::pki_types::ServerName::try_from(target.host.clone())
                .map_err(|e| format!("Bad host name: {e}"))?;
            let conn =
                rustls::ClientConnection::new(tls_config(), name).map_err(|e| e.to_string())?;
            Transport::Tls(Box::new(rustls::StreamOwned::new(conn, tcp)))
        } else {
            Transport::Plain(tcp)
        };
        let mut c = Connection {
            t,
            reader: ChunkReader::default(),
            stream_id: 0,
            out: Vec::new(),
            bytes_out: 0,
        };
        c.handshake()?;
        c.publish(target, hevc)?;
        c.t.tcp().set_read_timeout(None).ok();
        Ok(c)
    }

    fn handshake(&mut self) -> Result<(), String> {
        let e = |e: std::io::Error| format!("RTMP handshake failed: {e}");
        self.t.write_all(&handshake_c0c1()).map_err(e)?;
        self.t.flush().map_err(e)?;
        let mut s = vec![0u8; 1 + 1536 * 2];
        self.t.read_exact(&mut s).map_err(e)?;
        self.t.write_all(&s[1..1537]).map_err(e)?; // C2 = S1
        Ok(())
    }

    fn command(&mut self, values: &[Amf], stream_id: u32) -> Result<(), String> {
        let payload = encode_all(values);
        self.send(CSID_COMMAND, 0, MSG_COMMAND, stream_id, &payload)
    }

    fn publish(&mut self, t: &Target, hevc: bool) -> Result<(), String> {
        let mut size = Vec::new();
        write_message(
            &mut size,
            CSID_CONTROL,
            0,
            MSG_SET_CHUNK_SIZE,
            0,
            &(OUT_CHUNK_SIZE as u32).to_be_bytes(),
            128,
        );
        self.write_raw(&size)?;
        let mut props = vec![
            ("app", Amf::str(&t.app)),
            ("type", Amf::str("nonprivate")),
            ("flashVer", Amf::str("FMLE/3.0 (compatible; FMSc/1.0)")),
            ("swfUrl", Amf::str(&t.tc_url)),
            ("tcUrl", Amf::str(&t.tc_url)),
        ];
        if hevc {
            props.push(("fourCcList", Amf::StrictArray(vec![Amf::str("hvc1")])));
        }
        self.command(
            &[Amf::str("connect"), Amf::Number(1.0), Amf::obj(&props)],
            0,
        )?;
        self.wait_result(1.0)?;
        let key = Amf::str(&t.key);
        self.command(
            &[
                Amf::str("releaseStream"),
                Amf::Number(2.0),
                Amf::Null,
                key.clone(),
            ],
            0,
        )?;
        self.command(
            &[
                Amf::str("FCPublish"),
                Amf::Number(3.0),
                Amf::Null,
                key.clone(),
            ],
            0,
        )?;
        self.command(&[Amf::str("createStream"), Amf::Number(4.0), Amf::Null], 0)?;
        let r = self.wait_result(4.0)?;
        self.stream_id = r.get(3).and_then(Amf::as_f64).unwrap_or(1.0) as u32;
        let sid = self.stream_id;
        self.command(
            &[
                Amf::str("publish"),
                Amf::Number(5.0),
                Amf::Null,
                key,
                Amf::str("live"),
            ],
            sid,
        )?;
        loop {
            let v = self.next_command()?;
            if v.first().and_then(Amf::as_str) != Some("onStatus") {
                continue;
            }
            let code = v
                .get(3)
                .and_then(|o| o.get("code"))
                .and_then(Amf::as_str)
                .unwrap_or("");
            if code == "NetStream.Publish.Start" {
                return Ok(());
            }
            if code.contains("Publish") || code.contains("Failed") || code.contains("Rejected") {
                return Err(format!(
                    "Platform refused the stream ({code}). Check the stream key."
                ));
            }
        }
    }

    fn wait_result(&mut self, txn: f64) -> Result<Vec<Amf>, String> {
        loop {
            let v = self.next_command()?;
            let name = v.first().and_then(Amf::as_str).unwrap_or("");
            if v.get(1).and_then(Amf::as_f64) != Some(txn) {
                continue;
            }
            return match name {
                "_result" => Ok(v),
                _ => {
                    let why = v
                        .get(3)
                        .and_then(|o| o.get("description").or(o.get("code")))
                        .and_then(Amf::as_str)
                        .unwrap_or("rejected");
                    Err(format!("Platform refused the connection: {why}"))
                }
            };
        }
    }

    /// Blocking read of the next AMF command (control messages handled).
    fn next_command(&mut self) -> Result<Vec<Amf>, String> {
        loop {
            if let Some(m) = self.reader.next()? {
                if let Some(v) = self.handle(m)? {
                    return Ok(v);
                }
                continue;
            }
            let mut buf = [0u8; 4096];
            let n = self
                .t
                .read(&mut buf)
                .map_err(|e| format!("Connection lost: {e}"))?;
            if n == 0 {
                return Err("Server closed the connection".into());
            }
            self.reader.feed(&buf[..n]);
        }
    }

    /// Answers pings; returns commands.
    fn handle(&mut self, m: Message) -> Result<Option<Vec<Amf>>, String> {
        match m.typ {
            MSG_USER_CONTROL if m.payload.len() >= 6 && m.payload[1] == 6 => {
                let mut pong = vec![0, 7];
                pong.extend_from_slice(&m.payload[2..6]);
                self.send(CSID_CONTROL, 0, MSG_USER_CONTROL, 0, &pong)?;
                Ok(None)
            }
            MSG_COMMAND => Ok(Some(decode_all(&m.payload)?)),
            _ => Ok(None),
        }
    }

    /// Reads whatever the server sent without blocking (acks, pings,
    /// errors). Returns an error if the server closed or rejected us.
    pub fn poll_incoming(&mut self) -> Result<(), String> {
        self.t.tcp().set_nonblocking(true).ok();
        let mut buf = [0u8; 4096];
        let res = loop {
            match self.t.read(&mut buf) {
                Ok(0) => break Err("Server closed the connection".to_string()),
                Ok(n) => self.reader.feed(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break Ok(()),
                Err(e) => break Err(format!("Connection lost: {e}")),
            }
        };
        self.t.tcp().set_nonblocking(false).ok();
        res?;
        while let Some(m) = self.reader.next()? {
            if let Some(v) = self.handle(m)? {
                let code = v
                    .get(3)
                    .and_then(|o| o.get("code"))
                    .and_then(Amf::as_str)
                    .unwrap_or("");
                if code.contains("Failed") || code.contains("Rejected") || code.contains("BadName")
                {
                    return Err(format!("Platform ended the stream ({code})"));
                }
            }
        }
        Ok(())
    }

    fn write_raw(&mut self, data: &[u8]) -> Result<(), String> {
        self.t
            .write_all(data)
            .and_then(|_| self.t.flush())
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
                    "Upload stalled".to_string()
                }
                _ => format!("Connection lost: {e}"),
            })?;
        self.bytes_out += data.len() as u64;
        Ok(())
    }

    fn send(
        &mut self,
        csid: u32,
        ts: u32,
        typ: u8,
        sid: u32,
        payload: &[u8],
    ) -> Result<(), String> {
        let mut out = std::mem::take(&mut self.out);
        out.clear();
        write_message(&mut out, csid, ts, typ, sid, payload, OUT_CHUNK_SIZE);
        let r = self.write_raw(&out);
        self.out = out;
        r
    }

    pub fn send_media(&mut self, audio: bool, ts: u32, body: &[u8]) -> Result<(), String> {
        let (csid, typ) = if audio {
            (CSID_AUDIO, MSG_AUDIO)
        } else {
            (CSID_VIDEO, MSG_VIDEO)
        };
        self.send(csid, ts, typ, self.stream_id, body)
    }

    pub fn send_metadata(&mut self, meta: &Meta) -> Result<(), String> {
        let body = flv::metadata(meta);
        self.send(CSID_DATA, 0, MSG_DATA, self.stream_id, &body)
    }

    /// Polite goodbye; errors are irrelevant at this point.
    pub fn close(mut self, key: &str) {
        let sid = self.stream_id;
        let _ = self.command(
            &[
                Amf::str("FCUnpublish"),
                Amf::Number(6.0),
                Amf::Null,
                Amf::str(key),
            ],
            0,
        );
        let _ = self.command(
            &[
                Amf::str("deleteStream"),
                Amf::Number(7.0),
                Amf::Null,
                Amf::Number(sid as f64),
            ],
            0,
        );
        if let Transport::Tls(s) = &mut self.t {
            s.conn.send_close_notify();
            let _ = s.flush();
        }
        let _ = self.t.tcp().shutdown(std::net::Shutdown::Both);
    }

    pub fn tcp_stats(&self) -> Option<TcpStats> {
        tcpinfo::query(self.t.tcp())
    }

    pub fn retune(&self, opts: &SenderOptions, stats: Option<&TcpStats>) {
        tune_send_buffer(self.t.tcp(), opts.send_buffer, stats);
    }
}

// ---------------- send queue ----------------

#[derive(Default, Clone, Copy, Debug, serde::Serialize)]
pub struct DropStats {
    pub video_frames: u64,
    pub congestion_frames: u64,
}

struct QInner {
    q: VecDeque<Packet>,
    /// Stream (encoder) this queue currently takes packets from.
    src: u32,
    /// Stream to switch to at its next keyframe.
    pending: Option<u32>,
    /// Drop incoming video below this until a frame at/above it arrives.
    min_priority: Priority,
    /// After a (re)connect: drop everything until a keyframe.
    wait_key: bool,
    want_idr: bool,
    drops: DropStats,
    closed: bool,
}

/// A destination's packet queue, measured in time.
pub struct SendQueue {
    inner: Mutex<QInner>,
    cv: Condvar,
    /// Safety cap: beyond this backlog the queue restarts at a keyframe.
    max_backlog_ms: i64,
}

impl SendQueue {
    pub fn new(src: u32, max_backlog_ms: i64) -> Arc<SendQueue> {
        Arc::new(SendQueue {
            inner: Mutex::new(QInner {
                q: VecDeque::new(),
                src,
                pending: None,
                min_priority: Priority::Disposable,
                wait_key: true,
                want_idr: false,
                drops: DropStats::default(),
                closed: false,
            }),
            cv: Condvar::new(),
            max_backlog_ms,
        })
    }

    /// Does this queue still want packets from stream `src`?
    pub fn wants(&self, src: u32) -> bool {
        let g = self.inner.lock().unwrap();
        !g.closed && (g.src == src || g.pending == Some(src))
    }

    pub fn source(&self) -> u32 {
        self.inner.lock().unwrap().src
    }

    /// Switch to another encoder's stream at its next keyframe.
    pub fn switch_to(&self, src: u32) {
        let mut g = self.inner.lock().unwrap();
        g.pending = if g.src == src { None } else { Some(src) };
    }

    pub fn push(&self, p: &Packet) {
        let mut g = self.inner.lock().unwrap();
        if g.closed {
            return;
        }
        if g.pending == Some(p.src) {
            if !p.is_key() {
                return;
            }
            g.src = p.src;
            g.pending = None;
        }
        if p.src != g.src {
            return;
        }
        match p.priority() {
            None => {
                if g.wait_key {
                    return; // resume cleanly: nothing before the keyframe
                }
            }
            Some(pr) => {
                if g.wait_key && pr != Priority::Key {
                    g.drops.video_frames += 1;
                    return;
                }
                g.wait_key = false;
                if pr < g.min_priority {
                    g.drops.video_frames += 1;
                    g.drops.congestion_frames += 1;
                    return;
                }
                g.min_priority = Priority::Disposable;
            }
        }
        g.q.push_back(p.clone());
        if Self::span(&g.q) > self.max_backlog_ms {
            // Hopelessly behind (upload gone but socket alive): restart at
            // the next keyframe rather than streaming minutes-old video.
            g.drops.video_frames += g.q.iter().filter(|p| p.priority().is_some()).count() as u64;
            g.q.clear();
            g.wait_key = true;
            g.want_idr = true;
        }
        drop(g);
        self.cv.notify_one();
    }

    fn span(q: &VecDeque<Packet>) -> i64 {
        match (q.front(), q.back()) {
            (Some(a), Some(b)) => (b.dts_ms - a.dts_ms).max(0),
            _ => 0,
        }
    }

    /// Queue length in milliseconds of media (DTS span).
    pub fn backlog_ms(&self) -> u64 {
        Self::span(&self.inner.lock().unwrap().q) as u64
    }

    /// OBS-style frame dropping: removes queued video below `p` (never
    /// audio, never keyframes) and drops incoming frames below `p` until
    /// one at/above it arrives.
    pub fn drop_below(&self, p: Priority) {
        let mut g = self.inner.lock().unwrap();
        let before = g.q.len();
        g.q.retain(|x| x.priority().is_none_or(|pr| pr >= p));
        let n = (before - g.q.len()) as u64;
        g.drops.video_frames += n;
        g.drops.congestion_frames += n;
        g.min_priority = p;
    }

    pub fn pop(&self, timeout: Duration) -> Option<Packet> {
        let mut g = self.inner.lock().unwrap();
        if g.q.is_empty() && !g.closed {
            g = self.cv.wait_timeout(g, timeout).unwrap().0;
        }
        g.q.pop_front()
    }

    /// New connection: forget the backlog, start at the next keyframe.
    pub fn restart(&self) {
        let mut g = self.inner.lock().unwrap();
        g.q.clear();
        g.wait_key = true;
        g.min_priority = Priority::Disposable;
    }

    /// True once after the queue had to skip ahead (needs an IDR).
    pub fn take_want_idr(&self) -> bool {
        std::mem::take(&mut self.inner.lock().unwrap().want_idr)
    }

    pub fn drops(&self) -> DropStats {
        self.inner.lock().unwrap().drops
    }

    pub fn close(&self) {
        self.inner.lock().unwrap().closed = true;
        self.cv.notify_all();
    }
}

// ---------------- sender ----------------

#[derive(Clone, Debug)]
pub struct SenderOptions {
    pub connect_timeout: Duration,
    /// A write blocked this long means the connection is dead: reconnect.
    pub stall_timeout: Duration,
    pub first_retry: Duration,
    pub retry: Duration,
    pub retry_factor: f64,
    pub max_retry: Duration,
    /// Fixed SO_SNDBUF; None = follow the OS ideal send backlog.
    pub send_buffer: Option<usize>,
}

impl Default for SenderOptions {
    fn default() -> Self {
        SenderOptions {
            connect_timeout: Duration::from_secs(5),
            stall_timeout: Duration::from_secs(5),
            first_retry: Duration::from_millis(500),
            retry: Duration::from_secs(2),
            retry_factor: 1.5,
            max_retry: Duration::from_secs(30),
            send_buffer: None,
        }
    }
}

impl SenderOptions {
    /// Delay before reconnect attempt `n` (0-based, since the last
    /// successful publish): 0.5 s, 2 s, 3 s, 4.5 s, … capped.
    pub fn backoff(&self, n: u32) -> Duration {
        if n == 0 {
            return self.first_retry;
        }
        let d = self.retry.as_secs_f64() * self.retry_factor.powi(n as i32 - 1);
        Duration::from_secs_f64(d.min(self.max_retry.as_secs_f64()))
    }
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct SenderStatus {
    pub state: String,
    pub error: Option<String>,
    pub reconnects: u32,
    #[serde(skip)]
    pub live_since: Option<Instant>,
    /// When the last connection was lost (for reconnect timing).
    #[serde(skip)]
    pub lost_at: Option<Instant>,
    #[serde(skip)]
    pub last_reconnect: Option<Duration>,
    pub tcp: Option<TcpStats>,
}

pub struct Sender {
    pub queue: Arc<SendQueue>,
    pub status: Arc<Mutex<SenderStatus>>,
    pub meter: Arc<Mutex<Meter>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Sender {
    /// `on_ready` is called after every successful publish: the engine
    /// asks this destination's encoder for an IDR there.
    pub fn spawn(
        name: String,
        url: String,
        meta: Meta,
        queue: Arc<SendQueue>,
        opts: SenderOptions,
        window: Duration,
        on_ready: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Sender, String> {
        let target = parse_url(&url)?;
        let status = Arc::new(Mutex::new(SenderStatus {
            state: "connecting".into(),
            ..Default::default()
        }));
        let meter = Arc::new(Mutex::new(Meter::new(window, Instant::now())));
        let stop = Arc::new(AtomicBool::new(false));
        let ctx = SendCtx {
            name: name.clone(),
            target,
            meta,
            queue: queue.clone(),
            opts,
            status: status.clone(),
            meter: meter.clone(),
            stop: stop.clone(),
            on_ready,
        };
        let thread = std::thread::Builder::new()
            .name(format!("rtmp-{name}"))
            .spawn(move || ctx.run())
            .map_err(|e| e.to_string())?;
        Ok(Sender {
            queue,
            status,
            meter,
            stop,
            thread: Some(thread),
        })
    }

    pub fn is_connected(&self) -> bool {
        self.status.lock().unwrap().state == "live"
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.queue.close();
        if let Some(t) = self.thread.take() {
            t.join().ok();
        }
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        self.stop();
    }
}

struct SendCtx {
    name: String,
    target: Target,
    meta: Meta,
    queue: Arc<SendQueue>,
    opts: SenderOptions,
    status: Arc<Mutex<SenderStatus>>,
    meter: Arc<Mutex<Meter>>,
    stop: Arc<AtomicBool>,
    on_ready: Arc<dyn Fn() + Send + Sync>,
}

impl SendCtx {
    fn set(&self, f: impl FnOnce(&mut SenderStatus)) {
        f(&mut self.status.lock().unwrap());
    }

    fn sleep(&self, d: Duration) -> bool {
        let until = Instant::now() + d;
        while Instant::now() < until {
            if self.stop.load(Ordering::SeqCst) {
                return false;
            }
            // Keep the queue from piling up while offline.
            self.queue.restart();
            std::thread::sleep(Duration::from_millis(20).min(until - Instant::now()));
        }
        !self.stop.load(Ordering::SeqCst)
    }

    fn run(self) {
        let mut failures = 0u32;
        let hevc = self.meta.codec == super::VCodec::Hevc;
        while !self.stop.load(Ordering::SeqCst) {
            let conn = Connection::open(&self.target, &self.opts, hevc);
            let mut conn = match conn {
                Ok(c) => c,
                Err(e) => {
                    crate::diag::log("engine", format!("{}: {e}", self.name));
                    self.set(|s| {
                        s.state = "reconnecting".into();
                        s.error = Some(e);
                    });
                    let wait = self.opts.backoff(failures);
                    failures += 1;
                    if !self.sleep(wait) {
                        break;
                    }
                    continue;
                }
            };
            self.queue.restart();
            self.meter.lock().unwrap().reset(Instant::now());
            (self.on_ready)();
            let result = self.stream(&mut conn);
            match result {
                Ok(()) => {
                    conn.close(&self.target.key);
                    break;
                }
                Err(e) => {
                    crate::diag::log("engine", format!("{}: {e} - reconnecting", self.name));
                    self.set(|s| {
                        s.state = "reconnecting".into();
                        s.error = Some(e);
                        s.reconnects += 1;
                        s.live_since = None;
                        s.lost_at = Some(Instant::now());
                    });
                    drop(conn);
                    if !self.sleep(self.opts.backoff(0)) {
                        break;
                    }
                    failures = 1;
                }
            }
        }
        self.set(|s| s.state = "stopped".into());
    }

    /// Sends until stopped (Ok) or the connection fails (Err).
    fn stream(&self, conn: &mut Connection) -> Result<(), String> {
        conn.send_metadata(&self.meta)?;
        let mut offset: Option<i64> = None;
        let (mut vcfg, mut acfg): (Option<Bytes>, Option<Bytes>) = (None, None);
        let mut last_ts = [0u32; 2];
        let mut last_poll = Instant::now();
        let mut live = false;
        loop {
            if self.stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            if last_poll.elapsed() >= Duration::from_millis(200) {
                last_poll = Instant::now();
                conn.poll_incoming()?;
                let st = conn.tcp_stats();
                conn.retune(&self.opts, st.as_ref());
                self.set(|s| s.tcp = st);
            }
            if self.queue.take_want_idr() {
                (self.on_ready)();
            }
            let Some(p) = self.queue.pop(Duration::from_millis(50)) else {
                continue;
            };
            let off = *offset.get_or_insert(p.dts_ms);
            let audio = matches!(p.kind, Kind::Audio);
            // Monotonic per track, as some ingests demand.
            let slot = &mut last_ts[audio as usize];
            let ts = ((p.dts_ms - off).max(0) as u32).max(*slot);
            *slot = ts;
            let before = conn.bytes_out;
            match &p.kind {
                Kind::Audio => {
                    if p.config.is_some() && p.config != acfg {
                        acfg = p.config.clone();
                        conn.send_media(true, ts, &flv::audio_tag(true, acfg.as_deref().unwrap()))?;
                    }
                    if acfg.is_some() {
                        conn.send_media(true, ts, &flv::audio_tag(false, &p.data))?;
                    }
                }
                Kind::Video { codec, cts_ms, .. } => {
                    if p.is_key() && p.config.is_some() && p.config != vcfg {
                        vcfg = p.config.clone();
                        let seq = flv::video_tag(*codec, true, true, 0, vcfg.as_deref().unwrap());
                        conn.send_media(false, ts, &seq)?;
                    }
                    if vcfg.is_some() {
                        let body = flv::video_tag(*codec, p.is_key(), false, *cts_ms, &p.data);
                        conn.send_media(false, ts, &body)?;
                    }
                    if !live && vcfg.is_some() {
                        live = true;
                        self.set(|s| {
                            if let Some(lost) = s.lost_at.take() {
                                s.last_reconnect = Some(lost.elapsed());
                            }
                            s.state = "live".into();
                            s.error = None;
                            s.live_since = Some(Instant::now());
                        });
                    }
                }
            }
            self.meter
                .lock()
                .unwrap()
                .add(Instant::now(), conn.bytes_out - before);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_of_supported_platforms() {
        let t = parse_url("rtmps://a.rtmps.youtube.com:443/live2/abcd-1234").unwrap();
        assert_eq!(
            (t.tls, t.port, t.app.as_str(), t.key.as_str()),
            (true, 443, "live2", "abcd-1234")
        );
        assert_eq!(t.tc_url, "rtmps://a.rtmps.youtube.com:443/live2");
        let fb = parse_url("rtmps://live-api-s.facebook.com:443/rtmp/FB-1-0?s_bl=1&a=b").unwrap();
        assert_eq!(fb.key, "FB-1-0?s_bl=1&a=b");
        let custom = parse_url("rtmp://192.168.1.5/live/test").unwrap();
        assert_eq!((custom.port, custom.tls), (1935, false));
        assert!(parse_url("srt://x").is_err());
    }

    #[test]
    fn chunks_round_trip_with_extended_timestamp() {
        let payload: Vec<u8> = (0..10_000u32).map(|i| i as u8).collect();
        let mut out = Vec::new();
        write_message(
            &mut out,
            CSID_CONTROL,
            0,
            MSG_SET_CHUNK_SIZE,
            0,
            &4096u32.to_be_bytes(),
            128,
        );
        write_message(
            &mut out,
            CSID_VIDEO,
            0x0100_0000,
            MSG_VIDEO,
            1,
            &payload,
            4096,
        );
        write_message(&mut out, 70, 5, MSG_AUDIO, 1, &[1, 2, 3], 4096);
        let mut r = ChunkReader::default();
        // Byte by byte: parsing must cope with any split.
        let mut got = Vec::new();
        for b in out {
            r.feed(&[b]);
            while let Some(m) = r.next().unwrap() {
                got.push(m);
            }
        }
        assert_eq!(got.len(), 3);
        assert_eq!(got[1].ts, 0x0100_0000);
        assert_eq!(got[1].payload, payload);
        assert_eq!(
            (got[2].typ, got[2].ts, got[2].payload.len()),
            (MSG_AUDIO, 5, 3)
        );
    }

    #[test]
    fn backoff_schedule() {
        let o = SenderOptions::default();
        let ms: Vec<u128> = (0..5).map(|n| o.backoff(n).as_millis()).collect();
        assert_eq!(ms, vec![500, 2000, 3000, 4500, 6750]);
        assert_eq!(o.backoff(40), Duration::from_secs(30));
    }
}
