mod api;
mod ipc;

use hyper::service::{make_service_fn, service_fn};
use hyper::Server;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;

fn get_addr() -> SocketAddr {
    std::env::var("HIXX_BIND_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8765".to_owned())
        .parse()
        .expect("HIXX_BIND_ADDR must be a valid socket address")
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    print!("🚀 Hixx-Server (Kernel-Native) starting...");
    std::io::stdout().flush().unwrap();

    let ipc = ipc::HixxIPC::new()?;
    print!("✅ IPC initialized, mmap succeeded");
    std::io::stdout().flush().unwrap();

    let ipc_clone = Arc::new(Mutex::new(ipc));

    let addr = get_addr();
    let make_svc = {
        let ipc = Arc::clone(&ipc_clone);
        make_service_fn(move |_| {
            let ipc = Arc::clone(&ipc);
            async move {
                Ok::<_, hyper::Error>(service_fn(move |req| {
                    let ipc = Arc::clone(&ipc);
                    api::handle_request_with_ipc(req, ipc)
                }))
            }
        })
    };

    let server = Server::bind(&addr).serve(make_svc);
    info!("👂 Hixx-Server listening on {}", addr);
    println!("👂 Listening on {}", addr);

    if let Err(e) = server.await {
        eprintln!("Server error: {:?}", e);
    }
    Ok(())
}
