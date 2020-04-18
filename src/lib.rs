#[macro_use]
extern crate log;

use bytes::Buf;
use bytes::BytesMut;
use pin_project::pin_project;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::net::TcpStream;
use tokio::prelude::*;
use tokio_util::codec::{Decoder, Encoder};

use error::{Error, Result};
use proxy::{ProxyInfo, ProxyProtocolCodecV1, MAX_HEADER_SIZE, MIN_HEADER_SIZE};

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

pub struct Builder {
    pass_proxy_header: bool,
    require_proxy_header: bool,
    support_v1: bool,
    support_v2: bool,
}

impl Default for Builder {
    fn default() -> Self {
        Builder {
            pass_proxy_header: false,
            require_proxy_header: false,
            support_v1: true,
            support_v2: true,
        }
    }
}

impl Builder {
    pub async fn wrap<T: AsyncRead>(self, mut stream: T) -> Result<ProxiedStream<T>> {
        let mut buf = BytesMut::with_capacity(MAX_HEADER_SIZE);
        while buf.len() < MIN_HEADER_SIZE {
            let r = stream.read_buf(&mut buf).await?;
            if r == 0 {
                break;
            }
        }
        // TODO:  protocol specification speaks that header always must come as whole in first packet
        //        but could we rely on it also here in tokio? If yes implementation could be easier
        if buf.remaining() < MIN_HEADER_SIZE {
            return if self.require_proxy_header {
                Err(Error::Proxy("Message too short for proxy protocol".into()))
            } else {
                info!("No proxy protocol detected (because of too short message),  just passing the stream");
                Ok(ProxiedStream {
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
                    return Ok(ProxiedStream {
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
            Ok(ProxiedStream {
                inner: stream,
                buf,
                orig_source: None,
                orig_destination: None,
            })
        }
    }

    pub async fn connect(self, addr: SocketAddr) -> Result<ProxiedStream<TcpStream>> {
        let stream = TcpStream::connect(addr).await?;
        self.wrap(stream).await
    }

    pub fn new() -> Self {
        Builder::default()
    }

    pub fn pass_proxy_header(self, pass_proxy_header: bool) -> Self {
        Builder {
            pass_proxy_header,
            ..self
        }
    }

    pub fn require_proxy_header(self, require_proxy_header: bool) -> Self {
        Builder {
            require_proxy_header,
            ..self
        }
    }

    pub fn support_v1(self, support_v1: bool) -> Self {
        Builder { support_v1, ..self }
    }

    pub fn support_v2(self, support_v2: bool) -> Self {
        Builder { support_v2, ..self }
    }
}

#[pin_project]
pub struct ProxiedStream<T> {
    #[pin]
    inner: T,
    buf: BytesMut,
    orig_source: Option<SocketAddr>,
    orig_destination: Option<SocketAddr>,
}

impl<T> ProxiedStream<T> {
    fn get_pin_mut(self: Pin<&mut Self>) -> Pin<&mut T> {
        self.project().inner
    }
}

impl<T> AsRef<T> for ProxiedStream<T> {
    fn as_ref(&self) -> &T {
        &self.inner
    }
}

// with Deref and Deref mut we can get automatic coercion for TcpStream methods

impl std::ops::Deref for ProxiedStream<TcpStream> {
    type Target = TcpStream;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl std::ops::DerefMut for ProxiedStream<TcpStream> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<T> AsMut<T> for ProxiedStream<T> {
    fn as_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T> WithProxyInfo for ProxiedStream<T> {
    fn original_peer_addr(&self) -> Option<SocketAddr> {
        self.orig_source
    }

    fn original_destination_addr(&self) -> Option<SocketAddr> {
        self.orig_destination
    }
}

impl<T: AsyncRead> ProxiedStream<T> {
    pub async fn new(stream: T) -> Result<Self> {
        Builder::default().wrap(stream).await
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

impl<T: AsyncWrite + Unpin> ProxiedStream<T> {
    pub async fn send_proxy_header_v1(&mut self, item: ProxyInfo) -> Result<()> {
        let mut data = BytesMut::new();
        ProxyProtocolCodecV1::new().encode(item, &mut data)?;
        self.inner.write(&data).await?;
        Ok(())
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
        let mut ps = Builder::new().wrap(message).await?;
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
        let mut ps = Builder::new().pass_proxy_header(true).wrap(message).await?;
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

        let mut ps = ProxiedStream::new(message.as_bytes()).await?;
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
        let mut ps = ProxiedStream::new(&message[..]).await?;
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
        let ps = Builder::new()
            .require_proxy_header(true)
            .wrap(&message[..])
            .await;
        assert!(ps.is_err());
    }

    #[tokio::test]
    async fn test_too_short_message_fail() {
        env_logger::try_init().ok();
        let message = b"NIC\r\n";
        let ps = Builder::new()
            .require_proxy_header(true)
            .wrap(&message[..])
            .await;
        assert!(ps.is_err());
    }

    #[tokio::test]
    async fn test_too_short_message_pass() -> Result<()> {
        env_logger::try_init().ok();
        let message = b"NIC\r\n";
        let mut ps = Builder::new()
            .require_proxy_header(false)
            .wrap(&message[..])
            .await?;
        let mut buf = Vec::new();
        ps.read_to_end(&mut buf).await?;
        assert_eq!(message, &buf[..]);
        Ok(())
    }
}
