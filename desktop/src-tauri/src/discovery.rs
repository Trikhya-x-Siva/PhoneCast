//! Wi-Fi auto-discovery: the phone broadcasts "PCST-DISCOVER" on UDP port
//! 27185 and every PhoneCast PC on the network answers "PCST-HERE <name>",
//! so nobody has to type an IP address. See PROTOCOL.md.

use std::net::UdpSocket;
use std::time::Duration;

pub const DISCOVERY_PORT: u16 = 27185;

pub fn spawn() {
    std::thread::Builder::new()
        .name("discovery".into())
        .spawn(|| loop {
            if let Err(e) = serve() {
                crate::diag::log("wifi", format!("discovery responder stopped: {e}"));
            }
            std::thread::sleep(Duration::from_secs(5));
        })
        .ok();
}

fn serve() -> std::io::Result<()> {
    let sock = UdpSocket::bind(("0.0.0.0", DISCOVERY_PORT))?;
    sock.set_broadcast(true)?;
    let name = hostname();
    let reply = format!("PCST-HERE {name}");
    let mut buf = [0u8; 256];
    loop {
        let (n, from) = sock.recv_from(&mut buf)?;
        if buf[..n].starts_with(b"PCST-DISCOVER") {
            sock.send_to(reply.as_bytes(), from)?;
        }
    }
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "PhoneCast PC".into())
}
