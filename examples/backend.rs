#[macro_use]
extern crate log;

use anyhow::{Context, Result};
use futures::future::TryFutureExt;
use structopt::StructOpt;
use tokio::{
    net::{TcpListener, TcpStream},
    prelude::*,
    stream::StreamExt,
};
use tokio_proxy_protocol::{Acceptor, WithProxyInfo};

#[derive(Debug, StructOpt)]
#[structopt(name = "example", about = "An example of StructOpt usage.")]
struct Args {
    // TCP Socket to listen on - addr:port
    #[structopt(short, long, default_value = "127.0.0.1:7775")]
    listen: std::net::SocketAddr,
}

async fn process_socket(socket: TcpStream) -> Result<()> {
    let mut socket = Acceptor::new().accept(socket).await?;
    debug!("Got connection from {:?}", socket.peer_addr()); // here deref coercion works for us - we can use TcpStream methods
    debug!(
        "Original proxied connection was from {:?} to {:?}",
        socket.original_peer_addr(),
        socket.original_destination_addr()
    );
    let mut buf = [0u8; 1024];
    let mut count = 0;
    while let Ok(n) = socket.read(&mut buf).await {
        debug!("Got {} bytes", n);
        count += n;
        if n == 0 {
            break;
        }
    }
    debug!("Read {} bytes", count);
    let msg = format!("Bytes received {}\n", count);
    socket.write(msg.as_bytes()).await?;
    debug!("Connection finished");
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let args = Args::from_args();

    info!("Starting server on localhost port {}", args.listen);
    let mut server = TcpListener::bind(args.listen)
        .await
        .context("Cannot bind server")?;

    while let Some(socket) = server.next().await {
        match socket {
            Ok(socket) => {
                tokio::spawn(
                    process_socket(socket)
                        .map_err(|e| error!("Error while processing socket {}", e)),
                );
            }
            Err(e) => error!("Error accepting socket: {}", e),
        }
    }

    Ok(())
}
