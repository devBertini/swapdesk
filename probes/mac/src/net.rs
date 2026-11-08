//! std-only subcommands: `net`, `net-echo`, `wol`, and the agent-to-daemon `ipc` check (Unix only).
//! This file builds on Windows too, so the Windows PC can be the peer for P8 and P10.

use crate::d;
use crate::log::emit;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

/// Connected UDP socket to the peer, one datagram per second, every result logged, never exits early (TN3179 short-lifespan note).
pub fn net(peer: &str, port: u16, seconds: u64) {
    let target = format!("{peer}:{port}");
    let addr: SocketAddr = match target.to_socket_addrs().ok().and_then(|mut a| a.next()) {
        Some(a) => a,
        None => {
            emit("net.resolve", false, &d!("target" => target));
            return;
        }
    };
    let sock = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(e) => {
            emit("net.bind", false, &d!("error" => e.to_string()));
            return;
        }
    };
    // Connecting a UDP socket to a local address is what triggers the Local Network alert (TN3179).
    let connected = sock.connect(addr);
    emit(
        "net.connect",
        connected.is_ok(),
        &d!("peer" => target.clone(), "error" => connected.as_ref().err().map(ToString::to_string).unwrap_or_default()),
    );
    let start = Instant::now();
    let mut first_ok: Option<f64> = None;
    let (mut oks, mut errs_before_first_ok, mut errs) = (0u64, 0u64, 0u64);
    let mut seq = 0u64;
    while start.elapsed() < Duration::from_secs(seconds) {
        let payload = format!("swapdesk-probe {seq}");
        let result = sock.send(payload.as_bytes());
        let t = start.elapsed().as_secs_f64();
        match &result {
            Ok(_) => {
                oks += 1;
                if first_ok.is_none() {
                    first_ok = Some(t);
                }
            }
            Err(_) => {
                errs += 1;
                if first_ok.is_none() {
                    errs_before_first_ok += 1;
                }
            }
        }
        emit(
            "net.send",
            result.is_ok(),
            &d!(
                "seq" => seq,
                "t_s" => t,
                "os_error" => result.as_ref().err().and_then(std::io::Error::raw_os_error).map_or(0, i64::from),
                "error" => result.as_ref().err().map(ToString::to_string).unwrap_or_default()
            ),
        );
        seq += 1;
        std::thread::sleep(Duration::from_secs(1));
    }
    emit(
        "net.summary",
        first_ok.is_some(),
        &d!(
            "first_ok_s" => first_ok.unwrap_or(-1.0),
            "errors_before_first_ok" => errs_before_first_ok,
            "ok_count" => oks,
            "err_count" => errs
        ),
    );
}

/// UDP echo server. `seconds == 0` runs until killed.
pub fn net_echo(port: u16, seconds: u64) {
    let sock = match UdpSocket::bind(("0.0.0.0", port)) {
        Ok(s) => s,
        Err(e) => {
            emit("net_echo.bind", false, &d!("port" => u64::from(port), "error" => e.to_string()));
            return;
        }
    };
    emit("net_echo.bind", true, &d!("port" => u64::from(port)));
    let _ = sock.set_read_timeout(Some(Duration::from_millis(500)));
    let start = Instant::now();
    let mut buf = [0u8; 1500];
    while seconds == 0 || start.elapsed() < Duration::from_secs(seconds) {
        if let Ok((n, from)) = sock.recv_from(&mut buf) {
            emit("net_echo.recv", true, &d!("from" => from.to_string(), "bytes" => n));
            let _ = sock.send_to(&buf[..n], from);
        }
    }
}

/// Magic packet, three times, to the subnet broadcast address.
pub fn wol(mac: &str, broadcast: &str) {
    let bytes: Vec<u8> = mac
        .split([':', '-'])
        .filter_map(|p| u8::from_str_radix(p, 16).ok())
        .collect();
    if bytes.len() != 6 {
        emit("wol.send", false, &d!("error" => "mac must be AA:BB:CC:DD:EE:FF"));
        return;
    }
    let mut packet = vec![0xFFu8; 6];
    for _ in 0..16 {
        packet.extend_from_slice(&bytes);
    }
    let sock = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(e) => {
            emit("wol.send", false, &d!("error" => e.to_string()));
            return;
        }
    };
    let _ = sock.set_broadcast(true);
    for i in 0..3 {
        let r = sock.send_to(&packet, (broadcast, 9));
        emit("wol.send", r.is_ok(), &d!("n" => i as u64, "to" => broadcast, "error" => r.err().map(|e| e.to_string()).unwrap_or_default()));
        std::thread::sleep(Duration::from_millis(300));
    }
}

#[cfg(unix)]
pub const IPC_SOCKET: &str = "/var/run/swapdesk-probe.sock";

/// Daemon side: answers each line with `pong <line>`. Runs forever on its own thread.
#[cfg(unix)]
pub fn ipc_serve() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    let _ = std::fs::remove_file(IPC_SOCKET);
    let listener = match UnixListener::bind(IPC_SOCKET) {
        Ok(l) => l,
        Err(e) => {
            emit("ipc.serve", false, &d!("error" => e.to_string()));
            return;
        }
    };
    // World-writable so the Aqua agent (a normal user) can connect; probe only, never for the product.
    let _ = std::fs::set_permissions(IPC_SOCKET, std::os::unix::fs::PermissionsExt::from_mode(0o666));
    emit("ipc.serve", true, &d!("path" => IPC_SOCKET));
    for stream in listener.incoming().flatten() {
        let mut writer = match stream.try_clone() {
            Ok(w) => w,
            Err(_) => continue,
        };
        let mut line = String::new();
        if BufReader::new(stream).read_line(&mut line).is_ok() {
            emit("ipc.request", true, &d!("line" => line.trim().to_string()));
            let _ = writeln!(writer, "pong {}", line.trim());
        }
    }
}

/// Agent side: one round trip to the daemon.
#[cfg(unix)]
pub fn ipc_client() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    let start = Instant::now();
    let result = (|| -> std::io::Result<String> {
        let mut s = UnixStream::connect(IPC_SOCKET)?;
        s.set_read_timeout(Some(Duration::from_secs(3)))?;
        writeln!(s, "ping")?;
        let mut reply = String::new();
        BufReader::new(s).read_line(&mut reply)?;
        Ok(reply.trim().to_string())
    })();
    match result {
        Ok(reply) => emit("ipc.roundtrip", reply.starts_with("pong"), &d!("reply" => reply, "ms" => start.elapsed().as_millis() as u64)),
        Err(e) => emit("ipc.roundtrip", false, &d!("error" => e.to_string())),
    }
}
