use crate::error::{Error, Result};
use bytes::{Buf, BufMut, BytesMut};
use std::convert::TryFrom;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::str::FromStr;
use tokio::{prelude::*, stream::StreamExt};
use tokio_util::codec::{Decoder, Encoder, Framed, FramedParts};

pub mod v2;

pub(crate) const MAX_HEADER_SIZE: usize = 536;
pub(crate) const MIN_HEADER_SIZE: usize = 15;

/// Type of transport
#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub enum SocketType {
    /// TCP/IP V4
    Ipv4,
    /// TCP/IP V6
    Ipv6,
    /// Transport protocol in unknown
    Unknown,
}

/// Contains information from PROXY protocol
#[derive(PartialEq, Eq, Debug, Clone)]
pub struct ProxyInfo {
    /// Type of transport
    pub socket_type: SocketType,
    /// Original source address passed in PROXY protocol
    pub original_source: Option<SocketAddr>,
    /// Original destination address passed in PROXY protocol
    pub original_destination: Option<SocketAddr>,
}

impl TryFrom<(Option<SocketAddr>, Option<SocketAddr>)> for ProxyInfo {
    type Error = Error;
    fn try_from(addrs: (Option<SocketAddr>, Option<SocketAddr>)) -> Result<Self> {
        match (addrs.0, addrs.1) {
            (s @ Some(SocketAddr::V4(_)), d @ Some(SocketAddr::V4(_))) => Ok(ProxyInfo {
                socket_type: SocketType::Ipv4,
                original_source: s,
                original_destination: d,
            }),

            (s @ Some(SocketAddr::V6(_)), d @ Some(SocketAddr::V6(_))) => Ok(ProxyInfo {
                socket_type: SocketType::Ipv6,
                original_source: s,
                original_destination: d,
            }),

            (None, None) => Ok(ProxyInfo {
                socket_type: SocketType::Unknown,
                original_source: None,
                original_destination: None,
            }),

            _ => Err(Error::Proxy(
                "Inconsistent source and destination addresses".into(),
            )),
        }
    }
}

/// Encoder and Decoder for PROXY protocol v1
pub struct ProxyProtocolCodecV1 {
    next_pos: usize,
}

impl Default for ProxyProtocolCodecV1 {
    fn default() -> Self {
        ProxyProtocolCodecV1::new()
    }
}

impl ProxyProtocolCodecV1 {
    pub fn new() -> Self {
        ProxyProtocolCodecV1 { next_pos: 0 }
    }
}

fn parse_addresses<T>(parts: &[&str]) -> Result<(SocketAddr, SocketAddr)>
where
    T: FromStr,
    std::net::IpAddr: From<T>,
    Error: From<<T as FromStr>::Err>,
{
    let orig_sender_addr: T = parts[2].parse()?;
    let orig_sender_port: u16 = parts[4].parse::<u16>()?;
    let orig_recipient_addr: T = parts[3].parse()?;
    let orig_recipient_port: u16 = parts[5].parse::<u16>()?;

    Ok((
        (orig_sender_addr, orig_sender_port).into(),
        (orig_recipient_addr, orig_recipient_port).into(),
    ))
}

impl Decoder for ProxyProtocolCodecV1 {
    type Item = ProxyInfo;
    type Error = Error;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>> {
        if let Some(eol_pos) = buf[self.next_pos..].windows(2).position(|w| w == b"\r\n") {
            let eol_pos = eol_pos + self.next_pos;
            let header = std::str::from_utf8(&buf[..eol_pos])?;

            debug!("Proxy header is {}", header);
            let parts: Vec<_> = header.split(' ').collect();
            if parts[0] != "PROXY" {
                return Err(Error::Proxy("Protocol tag is wrong".into()));
            }
            if parts.len() < 2 {
                return Err(Error::Proxy("At least two parts are needed".into()));
            }

            let res = match parts[1] {
                "UNKNOWN" => Ok(Some(ProxyInfo {
                    socket_type: SocketType::Unknown,
                    original_source: None,
                    original_destination: None,
                })),
                "TCP4" if parts.len() == 6 => {
                    let (original_source, original_destination) =
                        parse_addresses::<Ipv4Addr>(&parts)?;
                    if !original_source.is_ipv4() && !original_destination.is_ipv4() {
                        return Err(Error::Proxy("Invalid address version - expected V4".into()));
                    }
                    Ok(Some(ProxyInfo {
                        socket_type: SocketType::Ipv4,
                        original_source: Some(original_source),
                        original_destination: Some(original_destination),
                    }))
                }
                "TCP6" if parts.len() == 6 => {
                    let (original_source, original_destination) =
                        parse_addresses::<Ipv6Addr>(&parts)?;
                    if !original_source.is_ipv6() && !original_destination.is_ipv6() {
                        return Err(Error::Proxy("Invalid address version - expected V6".into()));
                    }
                    Ok(Some(ProxyInfo {
                        socket_type: SocketType::Ipv6,
                        original_source: Some(original_source),
                        original_destination: Some(original_destination),
                    }))
                }
                _ => Err(Error::Proxy(format!("Invalid proxy header v1: {}", header))),
            };
            buf.advance(eol_pos + 2);
            res
        } else if buf.len() < MAX_HEADER_SIZE {
            self.next_pos = if buf.is_empty() { 0 } else { buf.len() - 1 };
            Ok(None)
        } else {
            Err(Error::Proxy("Proxy header v1 does not contain EOL".into()))
        }
    }
}

impl Encoder<ProxyInfo> for ProxyProtocolCodecV1 {
    type Error = Error;
    fn encode(&mut self, item: ProxyInfo, header: &mut BytesMut) -> Result<()> {
        header.put(&b"PROXY "[..]);

        let proto = match item {
            ProxyInfo {
                socket_type: SocketType::Ipv4,
                ..
            } => "TCP4",
            ProxyInfo {
                socket_type: SocketType::Ipv6,
                ..
            } => "TCP6",
            ProxyInfo {
                socket_type: SocketType::Unknown,
                ..
            } => {
                header.put(&b"UNKNOWN\r\n"[..]);
                return Ok(());
            }
        };
        header.put(
            format!(
                "{} {} {} {} {}\r\n",
                proto,
                item.original_source.expect("IP missing").ip(),
                item.original_destination.expect("IP missing").ip(),
                item.original_source.expect("Port missing").port(),
                item.original_destination.expect("Port missing").port()
            )
            .as_bytes(),
        );

        Ok(())
    }
}

/// Helper function to accept stream with PROXY protocol header v1
///
/// Consumes header and returns appropriate `ProxyInfo` and rest of data as `FramedParts`,
/// which can be used to easily create new Framed struct (with different codec)
pub async fn accept_v1_framed<T>(
    stream: T,
) -> Result<(ProxyInfo, FramedParts<T, ProxyProtocolCodecV1>)>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let mut framed = Framed::new(stream, ProxyProtocolCodecV1::new());
    let proxy_info = framed
        .next()
        .await
        .ok_or_else(|| Error::Proxy("Proxy header is missing".into()))??;
    let parts = framed.into_parts();
    Ok((proxy_info, parts))
}

/// Helper function to accept stream with PROXY protocol header v2
///
/// Consumes header and returns appropriate `ProxyInfo` and rest of data as `FramedParts`,
/// which can be used to easily create new Framed struct (with different codec)
pub async fn accept_v2_framed<T>(
    stream: T,
) -> Result<(ProxyInfo, FramedParts<T, v2::ProxyProtocolCodecV2>)>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    let mut framed = Framed::new(stream, v2::ProxyProtocolCodecV2::new());
    let proxy_info = framed
        .next()
        .await
        .ok_or_else(|| Error::Proxy("Proxy header is missing".into()))??;
    let parts = framed.into_parts();
    Ok((proxy_info, parts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::{BufMut, BytesMut};

    #[test]
    fn test_header_v1_in_small_pieces() {
        let data = [
            "PROX",
            "Y TCP4",
            " 192.168",
            ".0.1 ",
            "19",
            "2.168.0.1",
            "1 563",
            "24 443\r",
            "\nUsak",
        ];
        let data: Vec<_> = data.iter().map(|s| s.as_bytes()).collect();
        let mut d = ProxyProtocolCodecV1::new();
        let mut buf = BytesMut::new();
        for &piece in &data[..data.len() - 1] {
            buf.put(piece);
            let r = d.decode(&mut buf).unwrap();
            assert!(r.is_none())
        }
        // put there last piece

        buf.put(*data.last().unwrap());
        let r = d
            .decode(&mut buf)
            .expect("Buffer should should be ok")
            .expect("and contain full header");
        assert_eq!(SocketType::Ipv4, r.socket_type);
        assert_eq!(
            "192.168.0.1:56324".parse::<SocketAddr>().unwrap(),
            r.original_source.unwrap()
        );
        assert_eq!(
            "192.168.0.11:443".parse::<SocketAddr>().unwrap(),
            r.original_destination.unwrap()
        );
        assert_eq!(b"Usak", &buf[..]);
    }

    #[test]
    fn test_long_v1_header_without_eol() {
        let data = (b'a'..b'z').cycle().take(600).collect::<Vec<_>>();
        let mut buf = BytesMut::from(&data[..]);
        let mut d = ProxyProtocolCodecV1::new();
        let r = d.decode(&mut buf);
        assert!(r.is_err());
        if let Err(Error::Proxy(m)) = r {
            assert!(
                m.contains("does not contain EOL"),
                "error is  about missing EOL"
            )
        } else {
            panic!("Wrong error")
        }
    }

    #[test]
    fn test_v1_header_creation() {
        let header_bytes = "PROXY TCP4 192.168.0.1 192.168.0.11 56324 443\r\n".as_bytes();
        let header_info = ProxyInfo {
            socket_type: SocketType::Ipv4,
            original_source: "192.168.0.1:56324".parse().ok(),
            original_destination: "192.168.0.11:443".parse().ok(),
        };

        let mut buf = BytesMut::new();
        let mut e = ProxyProtocolCodecV1::new();
        e.encode(header_info, &mut buf).unwrap();

        assert_eq!(header_bytes, &buf[..]);
    }

    #[test]
    fn test_v1_header_creation_for_ipv6() {
        let header_bytes = b"PROXY TCP6 ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff aaaa:aaaa:aaaa:aaaa:aaaa:aaaa:aaaa:aaaa 65535 65534\r\n";
        let header_info = ProxyInfo {
            socket_type: SocketType::Ipv6,
            original_source: "[ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff]:65535"
                .parse()
                .ok(),
            original_destination: "[aaaa:aaaa:aaaa:aaaa:aaaa:aaaa:aaaa:aaaa]:65534"
                .parse()
                .ok(),
        };

        let mut buf = BytesMut::new();
        let mut e = ProxyProtocolCodecV1::new();
        e.encode(header_info, &mut buf).unwrap();

        assert_eq!(&header_bytes[..], &buf[..]);
    }

    #[test]
    fn test_v1_header_decode_tcp6() {
        let header_bytes = b"PROXY TCP6 ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff aaaa:aaaa:aaaa:aaaa:aaaa:aaaa:aaaa:aaaa 65535 65534\r\nHello";
        let mut d = ProxyProtocolCodecV1::new();
        let mut buf = BytesMut::new();
        buf.put(&header_bytes[..]);
        let header_info = d
            .decode(&mut buf)
            .expect("parsed_ok")
            .expect("contains full header");
        let original_source: Option<SocketAddr> = "[ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff]:65535"
            .parse()
            .ok();
        let original_destination: Option<SocketAddr> =
            "[aaaa:aaaa:aaaa:aaaa:aaaa:aaaa:aaaa:aaaa]:65534"
                .parse()
                .ok();
        assert_eq!(SocketType::Ipv6, header_info.socket_type);
        assert_eq!(original_source, header_info.original_source);
        assert_eq!(original_destination, header_info.original_destination);
    }

    #[tokio::test]
    async fn test_accept_v1() {
        use std::io::Cursor;
        let message = Cursor::new(
            "PROXY TCP4 192.168.0.1 192.168.0.11 56324 443\r\nHello"
                .as_bytes()
                .to_vec(),
        );
        let (info, parts) = accept_v1_framed(message)
            .await
            .expect("Error parsing header");

        assert_eq!(SocketType::Ipv4, info.socket_type);
        assert_eq!(info.original_source, "192.168.0.1:56324".parse().ok());
        assert_eq!(info.original_destination, "192.168.0.11:443".parse().ok());

        assert_eq!(b"Hello", &parts.read_buf[..])
    }

    #[tokio::test]
    async fn test_accept_v1_incomplete_header() {
        use std::io::Cursor;
        let message = Cursor::new("PROXY TCP4 192.168.0.1 192.168.".as_bytes().to_vec());
        let res = accept_v1_framed(message).await;

        assert!(res.is_err());

        if let Err(Error::Io(e)) = res {
            println!("ERROR: {:?}", e);
        } else {
            panic!("Invalid error")
        }
    }

    #[tokio::test]
    async fn test_accept_v1_malformed() {
        use std::io::Cursor;
        let message = Cursor::new(
            "PROXY TCP4 192.168.0.1 192.168.\r\nHello"
                .as_bytes()
                .to_vec(),
        );
        let res = accept_v1_framed(message).await;

        assert!(res.is_err());

        if let Err(Error::Proxy(e)) = res {
            println!("ERROR: {}", e);
        } else {
            panic!("Invalid error")
        }
    }
}
