#[macro_use]
extern crate log;

use anyhow::{Context, Result};
use tokio::{net::{TcpListener, TcpStream}, prelude::*, stream::StreamExt, };
use tokio_proxy_protocol::ProxiedStream;
use futures::future::TryFutureExt;

async fn process_socket(mut socket: TcpStream) -> Result<()>{
    debug!("Got connection from {:?}", socket.peer_addr());
    let mut socket = ProxiedStream::new(socket).await?;
    debug!("Original proxied connection was from {:?} to {:?}", socket.original_peer_addr(), socket.original_destination_addr());
    let mut buf = [0u8; 1024];
    let mut count = 0;
    while let Ok(n) = socket.read(&mut buf).await {
        count += n;
        if n == 0 {
            break;
        }
    }
    debug!("Read {} bytes", count);
    socket
        .write(format!("Bytes received {}\n", count).as_bytes())
        .await?;
    debug!("Connection finished");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let port: u16 = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "7776".into())
        .parse()
        .context("Invalid port argument")?;
    info!("Starting server on localhost port {}", port);
    let mut server = TcpListener::bind(("127.0.0.1", port))
        .await
        .context("Cannot bind server")?;

    while let Some(socket) = server.next().await {
        match socket {
            Ok(socket) => {
                tokio::spawn(process_socket(socket).map_err(|e| error!("Error while processing socket {}", e)));
            }
            Err(e) => error!("Error accepting socket: {}", e),
        }
    }

    Ok(())
}
