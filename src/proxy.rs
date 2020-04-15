use crate::error::{Error, Result};
use bytes::{Buf, BufMut, BytesMut};
use std::net::{Ipv4Addr, SocketAddr};
use tokio_util::codec::{Decoder, Encoder};

pub const MAX_HEADER_SIZE: usize = 536;
pub const MIN_HEADER_SIZE: usize = 15;

#[derive(PartialEq, Eq, Debug, Clone)]
pub enum SocketType {
    Ipv4,
    Ipv6,
    Unknown,
}

#[derive(PartialEq, Eq, Debug, Clone)]
pub struct ProxyInfo {
    pub socket_type: SocketType,
    pub original_source: Option<SocketAddr>,
    pub original_destination: Option<SocketAddr>,
}

pub struct ProxyProtocolCodecV1 {
    next_pos: usize,
    pass_header: bool,
}

impl Default for ProxyProtocolCodecV1 {
    fn default() -> Self {
        ProxyProtocolCodecV1::new()
    }
}

impl ProxyProtocolCodecV1 {
    pub fn new() -> Self {
        ProxyProtocolCodecV1 {
            next_pos: 0,
            pass_header: false,
        }
    }

    pub fn new_with_pass_header(pass_header: bool) -> Self {
        ProxyProtocolCodecV1 {
            next_pos: 0,
            pass_header,
        }
    }
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
                    let orig_sender_addr: Ipv4Addr = parts[2].parse()?;
                    let orig_sender_port: u16 = parts[4].parse()?;
                    let orig_recipient_addr: Ipv4Addr = parts[3].parse()?;
                    let orig_recipient_port: u16 = parts[5].parse()?;

                    Ok(Some(ProxyInfo {
                        socket_type: SocketType::Ipv4,
                        original_source: Some((orig_sender_addr, orig_sender_port).into()),
                        original_destination: Some(
                            (orig_recipient_addr, orig_recipient_port).into(),
                        ),
                    }))
                }
                "TCP6" if parts.len() == 6 => unimplemented!(),
                _ => Err(Error::Proxy(format!("Invalid proxy header v1: {}", header))),
            };

            if !self.pass_header {
                buf.advance(eol_pos + 2);
            }

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
}
