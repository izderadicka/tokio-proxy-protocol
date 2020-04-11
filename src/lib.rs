#[macro_use]
extern crate log;

use bytes::Buf;
use bytes::BytesMut;
use pin_project::pin_project;
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::prelude::*;

#[pin_project]
pub struct ProxiedStream<T> {
    #[pin]
    inner: T,
    buf: BytesMut,
    orig_source: Option<SocketAddr>,
    orig_destination: Option<SocketAddr>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Proxy protocol error: {0}")]
    Proxy(String),

    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    #[error("Invalid encoding of proxy header: {0}")]
    Utf8(#[from] std::str::Utf8Error),

    #[error("Invalid address in proxy header: {0}")]
    IPAddress(#[from] std::net::AddrParseError),

    #[error("Invalid port in proxy header: {0}")]
    Port(#[from] std::num::ParseIntError),
}

pub type Result<T> = std::result::Result<T, Error>;

const MAX_HEADER_SIZE: usize = 536;
const MIN_HEADER_SIZE: usize = 15;

fn parse_proxy_header_v1(buf: &mut BytesMut) -> Result<(Option<SocketAddr>, Option<SocketAddr>)> {
    let eol_pos = buf
        .windows(2)
        .enumerate()
        .find(|(_, w)| w == b"\r\n")
        .ok_or(Error::Proxy("Missing EOL in proxy v1 protocol".into()))?
        .0;

    let header = buf.split_to(eol_pos + 2);
    let header = std::str::from_utf8(&header[..header.len() - 2])?;
    debug!("Proxy header is {}", header);
    let parts: Vec<_> = header.split(' ').collect();
    if parts.len() < 2 {
        return Err(Error::Proxy("At least two parts are needed".into()));
    }
    match parts[1] {
        "UNKNOWN" => Ok((None, None)),
        "TCP4" if parts.len() == 6 => {
            let orig_sender_addr: Ipv4Addr = parts[2].parse()?;
            let orig_sender_port: u16 = parts[4].parse()?;
            let orig_recipient_addr: Ipv4Addr = parts[3].parse()?;
            let orig_recipient_port: u16 = parts[5].parse()?;

            Ok((
                Some((orig_sender_addr, orig_sender_port).into()),
                Some((orig_recipient_addr, orig_recipient_port).into()),
            ))
        }
        "TCP6" if parts.len() == 6 => unimplemented!(),
        _ => Err(Error::Proxy(format!("Invalid proxy header v1: {}", header))),
    }
}

impl<T: AsyncRead> ProxiedStream<T> {
    pub async fn new(mut stream: T) -> Result<Self> {
        let mut buf = BytesMut::with_capacity(MAX_HEADER_SIZE);
        while buf.len() < MAX_HEADER_SIZE {
            let r = stream.read_buf(&mut buf).await?;
            if r == 0 {
                break;
            }
        }

        if buf.remaining() < MIN_HEADER_SIZE {
            return Err(Error::Proxy("Message too short for proxy protocol".into()));
        }
        debug!("Buffered initial {} bytes", buf.remaining());

        if &buf[0..6] == b"PROXY " {
            debug!("Detected proxy protocol v1 tag");
            let (orig_source, orig_destination) = parse_proxy_header_v1(&mut buf)?;

            Ok(ProxiedStream {
                inner: stream,
                buf,
                orig_source,
                orig_destination,
            })
        } else {
            Err(Error::Proxy(format!("Only v1 proxy protocol supported - header start{:?}", &buf[0..6])))
        }
    }

    fn get_pin_mut(self: Pin<&mut Self>) -> Pin<&mut T> {
        self.project().inner
    }

    pub fn original_peer_addr(&self) -> Option<SocketAddr> {
        self.orig_source
    }

    pub fn original_destination_addr(&self) -> Option<SocketAddr> {
        self.orig_destination
    }
}

impl<T: AsyncRead> AsyncRead for ProxiedStream<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        ctx: &mut Context,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.project();
        if this.buf.is_empty() {
            this.inner.poll_read(ctx, buf)
        } else {
            // send remaining data from buffer
            let to_copy = this.buf.remaining().min(buf.len());
            this.buf.copy_to_slice(&mut buf[0..to_copy]);

            //there is still space in output buffer
            // let's try if we have some bytes to add there
            if to_copy < buf.len() {
                let added = match this.inner.poll_read(ctx, &mut buf[to_copy..]) {
                    Poll::Ready(Ok(n)) => n,
                    Poll::Ready(Err(e)) => return Err(e).into(),
                    Poll::Pending => 0,
                };
                Poll::Ready(Ok(to_copy + added))
            } else {
                Poll::Ready(Ok(to_copy))
            }
        }
    }
}

impl<R: AsyncRead + AsyncWrite> AsyncWrite for ProxiedStream<R> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.get_pin_mut().poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_pin_mut().poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_pin_mut().poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[tokio::test]
    async fn test_v1_tcp4() -> Result<()> {
        env_logger::try_init().ok();
        let message = "PROXY TCP4 192.168.0.1 192.168.0.11 56324 443\r\nHELLO".as_bytes();
        let mut ps = ProxiedStream::new(message).await?;
        assert_eq!(
            "192.168.0.1:56324".parse::<SocketAddr>().unwrap(),
            ps.original_peer_addr().unwrap()
        );
        assert_eq!(
            "192.168.0.11:443".parse::<SocketAddr>().unwrap(),
            ps.original_destination_addr().unwrap()
        );
        let mut buf = Vec::new();
        ps.read_to_end(&mut buf).await?;
        assert_eq!(b"HELLO", &buf[..]);
        Ok(())
    }

    #[tokio::test]
    async fn test_v1_unknown_long_message() -> Result<()> {
        env_logger::try_init().ok();
        let mut message = "PROXY UNKNOWN\r\n".to_string();
        const DATA_LENGTH: usize = 1_000_000;
        let data = (b'A'..=b'Z').cycle().take(DATA_LENGTH).map(|c| c as char);
        message.extend(data);

        let mut ps = ProxiedStream::new(message.as_bytes()).await?;
        assert!(ps.original_peer_addr().is_none());
        assert!(ps.original_destination_addr().is_none());
        let mut buf = Vec::new();
        ps.read_to_end(&mut buf).await?;
        assert_eq!(DATA_LENGTH, buf.len());
        Ok(())
    }
}
