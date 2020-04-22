#[macro_use]
extern crate log;

use anyhow::{Context, Result, Error};
use futures::future::TryFutureExt;
use structopt::StructOpt;
use tokio::{
    net::{TcpListener, TcpStream},
    prelude::*,
    stream::StreamExt,
};
use tokio_proxy_protocol::{ProxyStream, WithProxyInfo};

#[derive(Debug, StructOpt)]
#[structopt(name = "example", about = "An example of StructOpt usage.")]
struct Args {
    // TCP Socket to listen on - addr:port
    #[structopt(short, long, default_value = "127.0.0.1:7776")]
    listen: std::net::SocketAddr,
}

async fn process_proxy_socket(mut socket: ProxyStream<TcpStream>) -> Result<()> {
    debug!("Got connection from {:?}", socket.peer_addr()); // here deref coercion works for us - we can use TcpStream methods
    debug!(
        "Original proxied connection was from {:?} to {:?}",
        socket.original_peer_addr(),
        socket.original_destination_addr()
    );
    let mut buf = [0u8; 1024];
    let mut count = 0;
    while let Ok(n) = socket.read(&mut buf).await {
        count += n;
        if n == 0 {
            break;
        }
    }
    debug!("Read {} bytes", count);
    let msg = format!("Bytes received {}\n", count);
    socket
        .write(msg.as_bytes())
        .await?;
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
                tokio::spawn( async {
                    let mut socket = ProxyStream::<TcpStream>::new(socket);
                    socket.accept().await?;
                    process_proxy_socket(socket).await?;
                    Ok::<_, Error>(())
                       
                }.map_err(|e| error!("Error processing socket {}", e))
                );
            }
            Err(e) => error!("Error accepting socket: {}", e),
        }
    }

    Ok(())
}
