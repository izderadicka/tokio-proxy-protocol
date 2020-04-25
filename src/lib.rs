#[macro_use]
extern crate log;

use bytes::Buf;
use bytes::BytesMut;
use pin_project::pin_project;
use std::convert::TryInto;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::net::{TcpStream, ToSocketAddrs};
use tokio::prelude::*;
use tokio_util::codec::{Decoder, Encoder};

use error::{Error, Result};
use proxy::{ProxyProtocolCodecV1, MAX_HEADER_SIZE, MIN_HEADER_SIZE};

pub mod error;
pub mod proxy;

const V1_TAG: &[u8] = b"PROXY ";
const V2_TAG: &[u8] = b"\x0D\x0A\x0D\x0A\x00\x0D\x0A\x51\x55\x49\x54\x0A";

pub trait WithProxyInfo {
    fn original_peer_addr(&self) -> Option<SocketAddr> {
        // TODO: or original_source_addr - which one is better?
        None
    }

    fn original_destination_addr(&self) -> Option<SocketAddr> {
        None
    }
}

impl WithProxyInfo for TcpStream {}

pub struct Acceptor {
    pass_proxy_header: bool,
    require_proxy_header: bool,
    support_v1: bool,
    support_v2: bool,
}

impl Default for Acceptor {
    fn default() -> Self {
        Acceptor {
            pass_proxy_header: false,
            require_proxy_header: false,
            support_v1: true,
            support_v2: true,
        }
    }
}

impl Acceptor {
    /// Processes proxy protocol header and creates ProxyStream
    /// with appropriate information
    pub async fn accept<T: AsyncRead>(self, mut stream: T) -> Result<ProxyStream<T>> {
        let mut buf = BytesMut::with_capacity(MAX_HEADER_SIZE);
        while buf.len() < MIN_HEADER_SIZE {
            let r = stream.read_buf(&mut buf).await?;
            if r == 0 {
                break;
            }
        }

        if buf.remaining() < MIN_HEADER_SIZE {
            return if self.require_proxy_header {
                Err(Error::Proxy("Message too short for proxy protocol".into()))
            } else {
                info!("No proxy protocol detected (because of too short message),  just passing the stream");
                Ok(ProxyStream {
                    inner: stream,
                    buf,
                    orig_source: None,
                    orig_destination: None,
                })
            };
        }

        debug!("Buffered initial {} bytes", buf.remaining());

        if &buf[0..6] == V1_TAG && self.support_v1 {
            debug!("Detected proxy protocol v1 tag");
            let mut codec = ProxyProtocolCodecV1::new_with_pass_header(self.pass_proxy_header);
            loop {
                if let Some(proxy_info) = codec.decode(&mut buf)? {
                    return Ok(ProxyStream {
                        inner: stream,
                        buf,
                        orig_source: proxy_info.original_source,
                        orig_destination: proxy_info.original_destination,
                    });
                }

                let r = stream.read_buf(&mut buf).await?;
                if r == 0 {
                    return Err(Error::Proxy("Incomplete V1 header".into()));
                }
            }
        } else if &buf[0..12] == V2_TAG && self.support_v2 {
            debug!("Detected proxy protocol v2 tag");
            unimplemented!("V2 not yet implemented") //TODO: Implement V2 protocol
        } else if self.require_proxy_header {
            error!("Proxy protocol is required");
            Err(Error::Proxy("Proxy protocol is required".into()))
        } else {
            info!("No proxy protocol detected, just passing the stream");
            Ok(ProxyStream {
                inner: stream,
                buf,
                orig_source: None,
                orig_destination: None,
            })
        }
    }

    pub fn new() -> Self {
        Acceptor::default()
    }

    pub fn pass_proxy_header(self, pass_proxy_header: bool) -> Self {
        Acceptor {
            pass_proxy_header,
            ..self
        }
    }

    pub fn require_proxy_header(self, require_proxy_header: bool) -> Self {
        Acceptor {
            require_proxy_header,
            ..self
        }
    }

    pub fn support_v1(self, support_v1: bool) -> Self {
        Acceptor { support_v1, ..self }
    }

    pub fn support_v2(self, support_v2: bool) -> Self {
        Acceptor { support_v2, ..self }
    }
}

pub struct Connector {
    use_v2: bool,
}

impl Default for Connector {
    fn default() -> Self {
        Connector { use_v2: false }
    }
}

impl Connector {
    pub fn new() -> Self {
        Connector::default()
    }

    pub fn use_v2(self, use_v2: bool) -> Self {
        Connector { use_v2 }
    }

    /// Creates outgoing connection with appropriate proxy protocol header
    pub async fn connect<A: ToSocketAddrs>(
        &self,
        addr: A,
        original_source: Option<SocketAddr>,
        original_destination: Option<SocketAddr>,
    ) -> Result<TcpStream> {
        let stream = TcpStream::connect(addr).await?;
        self.connect_to(stream, original_source, original_destination)
            .await
    }

    pub async fn connect_to<T: AsyncWrite + Unpin>(
        &self,
        mut dest: T,
        original_source: Option<SocketAddr>,
        original_destination: Option<SocketAddr>,
    ) -> Result<T> {
        let proxy_info = (original_source, original_destination).try_into()?;
        let mut data = BytesMut::new();
        if !self.use_v2 {
            ProxyProtocolCodecV1::new().encode(proxy_info, &mut data)?;
        } else {
            unimplemented!("V2 Protocol is not implemented")
        }
        dest.write(&data).await?;
        Ok(dest)
    }
}

#[pin_project]
pub struct ProxyStream<T> {
    #[pin]
    inner: T,
    buf: BytesMut,
    orig_source: Option<SocketAddr>,
    orig_destination: Option<SocketAddr>,
}

impl<T> ProxyStream<T> {
    fn get_pin_mut(self: Pin<&mut Self>) -> Pin<&mut T> {
        self.project().inner
    }

    /// Returns inner stream, but
    /// only when it is save, e.g. no data in buffer
    pub fn try_into_inner(self) -> Result<T> {
        if self.buf.is_empty() {
            Ok(self.inner)
        } else {
            Err(Error::InvalidState(
                "Cannot return inner steam because buffer is not empty".into(),
            ))
        }
    }
}

impl<T> AsRef<T> for ProxyStream<T> {
    fn as_ref(&self) -> &T {
        &self.inner
    }
}

// with Deref we can get automatic coercion for some TcpStream methods

impl std::ops::Deref for ProxyStream<TcpStream> {
    type Target = TcpStream;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

// actually DerefMut mut can be quite dangerous, because it'll enable to inner stream, while some data are already in buffer
// impl std::ops::DerefMut for ProxyStream<TcpStream> {
//     fn deref_mut(&mut self) -> &mut Self::Target {
//         &mut self.inner
//     }
// }

// same for AsMut - try to not to use it
// impl<T> AsMut<T> for ProxyStream<T> {
//     fn as_mut(&mut self) -> &mut T {
//         &mut self.inner
//     }
// }

impl<T> WithProxyInfo for ProxyStream<T> {
    fn original_peer_addr(&self) -> Option<SocketAddr> {
        self.orig_source
    }

    fn original_destination_addr(&self) -> Option<SocketAddr> {
        self.orig_destination
    }
}

impl<T: AsyncRead> ProxyStream<T> {
    pub async fn new(stream: T) -> Result<Self> {
        Acceptor::default().accept(stream).await
    }
}

impl<T: AsyncRead> AsyncRead for ProxyStream<T> {
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

impl<R: AsyncRead + AsyncWrite> AsyncWrite for ProxyStream<R> {
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
        let mut ps = Acceptor::new().accept(message).await?;
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
    async fn test_v1_header_pass() -> Result<()> {
        env_logger::try_init().ok();
        let message = "PROXY TCP4 192.168.0.1 192.168.0.11 56324 443\r\nHELLO".as_bytes();
        let mut ps = Acceptor::new()
            .pass_proxy_header(true)
            .accept(message)
            .await?;
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
        assert_eq!(message, &buf[..]);
        Ok(())
    }

    #[tokio::test]
    async fn test_v1_unknown_long_message() -> Result<()> {
        env_logger::try_init().ok();
        let mut message = "PROXY UNKNOWN\r\n".to_string();
        const DATA_LENGTH: usize = 1_000_000;
        let data = (b'A'..=b'Z').cycle().take(DATA_LENGTH).map(|c| c as char);
        message.extend(data);

        let mut ps = ProxyStream::new(message.as_bytes()).await?;
        assert!(ps.original_peer_addr().is_none());
        assert!(ps.original_destination_addr().is_none());
        let mut buf = Vec::new();
        ps.read_to_end(&mut buf).await?;
        assert_eq!(DATA_LENGTH, buf.len());
        Ok(())
    }

    #[tokio::test]
    async fn test_no_proxy_header_passed() -> Result<()> {
        env_logger::try_init().ok();
        let message = b"MEMAM PROXY HEADER, CHUDACEK JA";
        let mut ps = ProxyStream::new(&message[..]).await?;
        assert!(ps.original_peer_addr().is_none());
        assert!(ps.original_destination_addr().is_none());
        let mut buf = Vec::new();
        ps.read_to_end(&mut buf).await?;
        assert_eq!(&message[..], &buf[..]);
        Ok(())
    }

    #[tokio::test]
    async fn test_no_proxy_header_rejected() {
        env_logger::try_init().ok();
        let message = b"MEMAM PROXY HEADER, CHUDACEK JA";
        let ps = Acceptor::new()
            .require_proxy_header(true)
            .accept(&message[..])
            .await;
        assert!(ps.is_err());
    }

    #[tokio::test]
    async fn test_too_short_message_fail() {
        env_logger::try_init().ok();
        let message = b"NIC\r\n";
        let ps = Acceptor::new()
            .require_proxy_header(true)
            .accept(&message[..])
            .await;
        assert!(ps.is_err());
    }

    #[tokio::test]
    async fn test_too_short_message_pass() -> Result<()> {
        env_logger::try_init().ok();
        let message = b"NIC\r\n";
        let mut ps = Acceptor::new()
            .require_proxy_header(false)
            .accept(&message[..])
            .await?;
        let mut buf = Vec::new();
        ps.read_to_end(&mut buf).await?;
        assert_eq!(message, &buf[..]);
        Ok(())
    }

    #[tokio::test]
    async fn test_connect() -> Result<()> {
        let mut buf = Vec::new();
        let src = "127.0.0.1:1111".parse().ok();
        let dest = "127.0.0.1:2222".parse().ok();
        let _res = Connector::new().connect_to(&mut buf, src, dest).await?;
        let expected = "PROXY TCP4 127.0.0.1 127.0.0.1 1111 2222\r\n";
        assert_eq!(expected.as_bytes(), &buf[..]);
        Ok(())
    }
}
