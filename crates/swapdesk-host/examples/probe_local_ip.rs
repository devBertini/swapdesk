//! ACCOUNTS.md 5.5 probe: does `Incoming::local_ip()` give the arrival address on a socket bound to 0.0.0.0?
//! Development only: `cargo run -p swapdesk-host --example probe_local_ip`.

fn main() {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let identity = swapdesk_trust::Identity::generate(0).unwrap();
        let trust =
            swapdesk_trust::TrustHandle::open(std::sync::Arc::new(swapdesk_fakes::MemorySecretStore::new())).unwrap();
        let server_sock = std::net::UdpSocket::bind(("0.0.0.0", 0)).unwrap();
        let port = server_sock.local_addr().unwrap().port();
        let server = swapdesk_trust::open_session_endpoint(server_sock, &identity, trust).unwrap();
        let client_id = swapdesk_trust::Identity::generate(0).unwrap();
        let client =
            swapdesk_trust::endpoint_from_socket(std::net::UdpSocket::bind(("127.0.0.1", 0)).unwrap(), None).unwrap();
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let spki = *identity.spki();
        tokio::spawn(async move {
            let _ = swapdesk_trust::connect_session(&client, &client_id, addr, spki).await;
        });
        let incoming = tokio::time::timeout(std::time::Duration::from_secs(5), server.accept()).await.unwrap().unwrap();
        println!("PROBE local_ip on 0.0.0.0:{port} = {:?}", incoming.local_ip());
    });
}
