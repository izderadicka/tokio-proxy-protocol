#[macro_use]
extern crate log;

use anyhow::{Error, Result};
use std::net::SocketAddr;
use structopt::StructOpt;
use tokio::{
    io,
    net::{TcpListener, TcpStream},
    stream::{StreamExt},
};
use tokio_proxy_protocol::{Builder, WithProxyInfo};
use futures::future::{try_join, select, FutureExt, Either};

#[derive(Debug, StructOpt)]
struct Args {
    #[structopt(short, long, default_value = "127.0.0.1:7776")]
    listen: SocketAddr,

    #[structopt(short, long, default_value = "127.0.0.1:7775")]
    forward: SocketAddr,
}

async fn transfer(inbound: TcpStream, outbound_addr: SocketAddr) -> Result<()> {
    let proxy_stream = Builder::new().accept(inbound).await?;
    match (proxy_stream.local_addr(), proxy_stream.peer_addr()) {
        (Ok(local_addr), Ok(peer_addr)) => info!(
            "Transfering connection accepted on {} from intermediate proxy {}",
            local_addr, peer_addr
        ),
        _ => error!("Cannot get TCPStream addresses"),
    }
    if let (Some(original_source), Some(original_destination)) = (
        proxy_stream.original_peer_addr(),
        proxy_stream.original_destination_addr(),
    ) {
        info!("Connection original source was {} and destination {}", original_source, original_destination);
        let (mut in_read, mut in_write) = io::split(proxy_stream);
        let mut outbound = Builder::new()
            .connect(
                outbound_addr,
                Some(original_source),
                Some(original_destination),
            )
            .await?;

        let (mut out_read, mut out_write) = outbound.split();

        let outgoing = io::copy(&mut in_read, &mut out_write);
        let incoming = io::copy(&mut out_read, &mut in_write);

        try_join(outgoing, incoming).await?;
        Ok(())
        
    } else {
        Err(Error::msg("Proxy info missing".to_string()))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();
    let args = Args::from_args();
    let mut server = TcpListener::bind(args.listen).await?;
    info!("Proxy - listening on {} and forwading to {}", args.listen, args.forward);
    while let Some(socket) = server.next().await {
        match socket {
            Ok(socket) => {
                let t = transfer(socket, args.forward).map(|res| {
                    match res {
                        Ok(()) => info!("Proxied connection finished"),
                        Err(e) => error!("Error forwarding connection: {}", e)
                    }
                    
                });
                tokio::spawn(t);
            }
            Err(e) => error!("Error accepting connection: {}", e),
        };
    }

    Ok(())
}
