#[macro_use]
extern crate log;

use anyhow::{Error, Result};
use futures::future::{select, Either, FutureExt};
use std::net::Shutdown;
use std::net::SocketAddr;
use structopt::StructOpt;
use tokio::{
    io,
    net::{TcpListener, TcpStream},
    stream::StreamExt,
};
use tokio_proxy_protocol::{Acceptor, Connector, WithProxyInfo};

#[derive(Debug, StructOpt)]
struct Args {
    #[structopt(short, long, default_value = "127.0.0.1:7776")]
    listen: SocketAddr,

    #[structopt(short, long, default_value = "127.0.0.1:7775")]
    forward: SocketAddr,
}

async fn transfer(inbound: TcpStream, outbound_addr: SocketAddr) -> Result<()> {
    let proxy_stream = Acceptor::new().accept(inbound).await?;
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
        info!(
            "Connection original source was {} and destination {}",
            original_source, original_destination
        );
        let (mut in_read, mut in_write) = io::split(proxy_stream);
        let mut outbound = Connector::new()
            .connect(
                outbound_addr,
                Some(original_source),
                Some(original_destination),
            )
            .await?;
        //let outbound_ptr = &mut outbound as *mut TcpStream as u64;
        let (mut out_read, mut out_write) = outbound.split();

        let outgoing = copy::copy(&mut in_read, &mut out_write);
        let incoming = copy::copy(&mut out_read, &mut in_write);

        match select(outgoing, incoming).await {
            Either::Left((Ok(n), incoming)) => {
                debug!("Outgoing connection closed, copied {} bytes", n);
                // unsafe {
                //     // BUG: this is something completely unsafe
                //     let ob: &mut TcpStream = &mut *(outbound_ptr as *mut TcpStream);
                //     ob.shutdown(Shutdown::Write)?;
                // }

                incoming.reader_ref().as_ref().shutdown(Shutdown::Write)?;

                let read = incoming.await?;
                debug!("Incomming connection closed, copied {} bytes", read);
            }
            Either::Right((Ok(_), _)) => {
                error!("Incomming connection closed before outgoing");
            }
            Either::Left((Err(e), _)) => {
                error!("Error in outgoing connection: {}", e);
            }
            Either::Right((Err(e), _)) => {
                error!("Error in incomming connection: {}", e);
            }
        }
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
    info!(
        "Proxy - listening on {} and forwading to {}",
        args.listen, args.forward
    );
    while let Some(socket) = server.next().await {
        match socket {
            Ok(socket) => {
                let t = transfer(socket, args.forward).map(|res| match res {
                    Ok(()) => info!("Proxied connection finished"),
                    Err(e) => error!("Error forwarding connection: {}", e),
                });
                tokio::spawn(t);
            }
            Err(e) => error!("Error accepting connection: {}", e),
        };
    }

    Ok(())
}

// TODO: As we need to shutdown upstream connection after copy
// we need access to TCP stream, but it's mutably borrowed in Copy struct
// So only solution I've found is to create own modification of Copy
// which gives access to inner reference in Copy
// Or is there better solution?
mod copy {
    use futures::ready;
    use std::future::Future;
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite};

    #[derive(Debug)]
    #[must_use = "futures do nothing unless you `.await` or poll them"]
    pub struct Copy<'a, R: ?Sized, W: ?Sized> {
        reader: &'a mut R,
        read_done: bool,
        writer: &'a mut W,
        pos: usize,
        cap: usize,
        amt: u64,
        buf: Box<[u8]>,
    }

    pub fn copy<'a, R, W>(reader: &'a mut R, writer: &'a mut W) -> Copy<'a, R, W>
    where
        R: AsyncRead + Unpin + ?Sized,
        W: AsyncWrite + Unpin + ?Sized,
    {
        Copy {
            reader,
            read_done: false,
            writer,
            amt: 0,
            pos: 0,
            cap: 0,
            buf: Box::new([0; 2048]),
        }
    }

    impl<R, W> Copy<'_, R, W> {
        pub fn reader_ref(&self) -> &R {
            self.reader
        }

        #[allow(dead_code)]
        pub fn writer_ref(&self) -> &W {
            self.writer
        }
    }

    impl<R, W> Future for Copy<'_, R, W>
    where
        R: AsyncRead + Unpin + ?Sized,
        W: AsyncWrite + Unpin + ?Sized,
    {
        type Output = io::Result<u64>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
            loop {
                // If our buffer is empty, then we need to read some data to
                // continue.
                if self.pos == self.cap && !self.read_done {
                    let me = &mut *self;
                    let n = ready!(Pin::new(&mut *me.reader).poll_read(cx, &mut me.buf))?;
                    if n == 0 {
                        self.read_done = true;
                    } else {
                        self.pos = 0;
                        self.cap = n;
                    }
                }

                // If our buffer has some data, let's write it out!
                while self.pos < self.cap {
                    let me = &mut *self;
                    let i =
                        ready!(Pin::new(&mut *me.writer).poll_write(cx, &me.buf[me.pos..me.cap]))?;
                    if i == 0 {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "write zero byte into writer",
                        )));
                    } else {
                        self.pos += i;
                        self.amt += i as u64;
                    }
                }

                // If we've written all the data and we've seen EOF, flush out the
                // data and finish the transfer.
                if self.pos == self.cap && self.read_done {
                    let me = &mut *self;
                    ready!(Pin::new(&mut *me.writer).poll_flush(cx))?;
                    return Poll::Ready(Ok(self.amt));
                }
            }
        }
    }
}
