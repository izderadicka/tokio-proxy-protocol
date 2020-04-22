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
        // TODO: name it or original_source_addr - which one is better?
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
            require_proxy_header: true,
            support_v1: true,
            support_v2: true,
        }
    }
}

impl Builder {

    pub fn build<T:AsyncRead>(self, stream: T) -> ProxyStream<T> {
        ProxyStream {
            inner:stream,
            buf: BytesMut::with_capacity(MAX_HEADER_SIZE),
            orig_destination: None,
            orig_source: None,
            pass_proxy_header: self.pass_proxy_header,
            require_proxy_header: self.require_proxy_header,
            support_v1: self.support_v1,
            support_v2: self.support_v2
        }
    }
    
    // pub async fn connect(self, addr: SocketAddr) -> Result<ProxyStream<TcpStream>> {
    //     let stream = TcpStream::connect(addr).await?;
    //     self.wrap(stream).await
    // }

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
pub struct ProxyStream<T> {
    #[pin]
    inner: T,
    buf: BytesMut,
    orig_source: Option<SocketAddr>,
    orig_destination: Option<SocketAddr>,
    pass_proxy_header: bool,
    require_proxy_header: bool,
    support_v1: bool,
    support_v2: bool,

}

impl<T> ProxyStream<T> {
    fn get_pin_mut(self: Pin<&mut Self>) -> Pin<&mut T> {
        self.project().inner
    }
}

impl<T> AsRef<T> for ProxyStream<T> {
    fn as_ref(&self) -> &T {
        &self.inner
    }
}

// with Deref and Deref mut we can get automatic coercion for TcpStream methods

impl std::ops::Deref for ProxyStream<TcpStream> {
    type Target = TcpStream;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl std::ops::DerefMut for ProxyStream<TcpStream> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<T> AsMut<T> for ProxyStream<T> {
    fn as_mut(&mut self) -> &mut T {
        &mut self.inner
    }
}

impl<T> WithProxyInfo for ProxyStream<T> {
    fn original_peer_addr(&self) -> Option<SocketAddr> {
        self.orig_source
    }

    fn original_destination_addr(&self) -> Option<SocketAddr> {
        self.orig_destination
    }
}

impl<T: AsyncRead> ProxyStream<T> {

    pub async fn accept(&mut self) -> Result<()> {
        
        while self.buf.len() < MIN_HEADER_SIZE {
            let r = self.inner.read_buf(&mut self.buf).await?;
            if r == 0 {
                break;
            }
        }

        if self.buf.remaining() < MIN_HEADER_SIZE {
            return if self.require_proxy_header {
                Err(Error::Proxy("Message too short for proxy protocol".into()))
            } else {
                info!("No proxy protocol detected (because of too short message),  just passing the stream");
                Ok(())
            };
        }

        debug!("Buffered initial {} bytes", self.buf.remaining());

        if &self.buf[0..6] == V1_TAG && self.support_v1 {
            debug!("Detected proxy protocol v1 tag");
            let mut codec = ProxyProtocolCodecV1::new_with_pass_header(self.pass_proxy_header);
            loop {
                if let Some(proxy_info) = codec.decode(&mut self.buf)? {
                    self.orig_source = proxy_info.original_source;
                    self.orig_destination = proxy_info.original_destination;
                    return Ok(());
                }

                let r = self.inner.read_buf(&mut self.buf).await?;
                if r == 0 {
                    return Err(Error::Proxy("Incomplete V1 header".into()));
                }
            }
        } else if &self.buf[0..12] == V2_TAG && self.support_v2 {
            debug!("Detected proxy protocol v2 tag");
            unimplemented!("V2 not yet implemented") //TODO: Implement V2 protocol
        } else if self.require_proxy_header {
            error!("Proxy protocol is required");
            Err(Error::Proxy("Proxy protocol is required".into()))
        } else {
            info!("No proxy protocol detected, just passing the stream");
            Ok(())
        }
    }

    pub fn new(stream: T) -> Self {
        Builder::default().build(stream)
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

impl<T: AsyncWrite + Unpin> ProxyStream<T> {
    pub async fn send_proxy_header_v1(&mut self, item: ProxyInfo) -> Result<()> {
        let mut data = BytesMut::new();
        ProxyProtocolCodecV1::new().encode(item, &mut data)?;
        self.inner.write(&data).await?;
        Ok(())
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
        let mut ps = Builder::new().build(message);
        ps.accept().await?;
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
        let mut ps = Builder::new().pass_proxy_header(true).build(message);
        ps.accept().await?;
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

        let mut ps = ProxyStream::new(message.as_bytes());
        ps.accept().await?;
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
        let message: &[u8] = b"MEMAM PROXY HEADER, CHUDACEK JA";
        let mut ps = Builder::new().require_proxy_header(false).build(message);
        ps.accept().await?;
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
        let mut ps = Builder::new()
            .require_proxy_header(true)
            .build(&message[..]);
        assert!(ps.accept().await.is_err());
    }

    #[tokio::test]
    async fn test_too_short_message_fail() {
        env_logger::try_init().ok();
        let message = b"NIC\r\n";
        let mut ps = Builder::new()
            .require_proxy_header(true)
            .build(&message[..]);
        assert!(ps.accept().await.is_err());
    }

    #[tokio::test]
    async fn test_too_short_message_pass() -> Result<()> {
        env_logger::try_init().ok();
        let message = b"NIC\r\n";
        let mut ps = Builder::new()
            .require_proxy_header(false)
            .build(&message[..]);
        ps.accept().await?;
        let mut buf = Vec::new();
        ps.read_to_end(&mut buf).await?;
        assert_eq!(message, &buf[..]);
        Ok(())
    }
}
