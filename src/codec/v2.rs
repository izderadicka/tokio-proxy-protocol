use super::{ProxyInfo, SocketType};
use crate::error::Error;
use bytes::BytesMut;
use proto::*;
use std::net::SocketAddr;
use tokio_util::codec::{Decoder, Encoder};

pub mod proto;

pub const SIGNATURE: &[u8] = b"\x0D\x0A\x0D\x0A\x00\x0D\x0A\x51\x55\x49\x54\x0A";

pub struct ProxyProtocolCodecV2 {
    socket_type: Option<SocketType>,
    remains: usize,
}

impl Default for ProxyProtocolCodecV2 {
    fn default() -> Self {
        ProxyProtocolCodecV2 {
            socket_type: None,
            remains: 0,
        }
    }
}

impl ProxyProtocolCodecV2 {
    pub fn new() -> Self {
        Default::default()
    }
}

impl Decoder for ProxyProtocolCodecV2 {
    type Item = ProxyInfo;
    type Error = Error;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        loop {
            match self.socket_type {
                Some(t) => {
                    if self.remains > 0 && buf.len() < self.remains {
                        return Ok(None);
                    } else {
                        let mut data_buf = buf.split_to(self.remains);
                        let info = match t {
                            SocketType::Ipv4 => {
                                let addresses = Ip4Addresses::deserialize(&mut data_buf)?;
                                let (src, dst) = addresses.into();
                                ProxyInfo {
                                    socket_type: t,
                                    original_source: Some(SocketAddr::V4(src)),
                                    original_destination: Some(SocketAddr::V4(dst)),
                                }
                            }
                            SocketType::Ipv6 => {
                                let addresses = Ip6Addresses::deserialize(&mut data_buf)?;
                                let (src, dst) = addresses.into();
                                ProxyInfo {
                                    socket_type: t,
                                    original_source: Some(SocketAddr::V6(src)),
                                    original_destination: Some(SocketAddr::V6(dst)),
                                }
                            }
                            SocketType::Unknown => ProxyInfo {
                                socket_type: t,
                                original_source: None,
                                original_destination: None,
                            },
                        };
                        self.socket_type = None;
                        self.remains = 0;
                        return Ok(Some(info));
                    }
                }
                None => {
                    if buf.len() < SIZE_HEADER as usize {
                        return Ok(None);
                    } else {
                        let header = Header::deserialize(buf)?;
                        self.remains = header.len as usize;
                        match header.protocol {
                            PROTOCOL_TCP_IP4 => self.socket_type = Some(SocketType::Ipv4),
                            PROTOCOL_TCP_IP6 => self.socket_type = Some(SocketType::Ipv6),
                            p => {
                                warn!("Yet unsupported protocol, code {}", p);
                                self.socket_type = Some(SocketType::Unknown);
                            }
                        }
                    }
                }
            }
        }
    }
}

impl Encoder<ProxyInfo> for ProxyProtocolCodecV2 {
    type Error = Error;
    fn encode(&mut self, item: ProxyInfo, buf: &mut BytesMut) -> Result<(), Self::Error> {
        let header = Header::new(item.socket_type);
        header.serialize(buf);
        match item.socket_type {
            SocketType::Ipv4 => {
                if let (Some(SocketAddr::V4(src)), Some(SocketAddr::V4(dst))) =
                    (item.original_source, item.original_destination)
                {
                    let addresses: Ip4Addresses = (src, dst).into();
                    addresses.serialize(buf);
                } else {
                    return Err(Error::Proxy("Both V4 addresses must be present".into()));
                }
            }

            SocketType::Ipv6 => {
                if let (Some(SocketAddr::V6(src)), Some(SocketAddr::V6(dst))) =
                    (item.original_source, item.original_destination)
                {
                    let addresses: Ip6Addresses = (src, dst).into();
                    addresses.serialize(buf);
                } else {
                    return Err(Error::Proxy("Both V4 addresses must be present".into()));
                }
            }
            SocketType::Unknown => (),
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::BufMut;

    fn test_msg_ip4(msg: &str) -> BytesMut {
        let mut output = BytesMut::with_capacity(16 + 12 + msg.len());
        output.extend_from_slice(SIGNATURE);
        output.put_u8(0x21);
        output.put_u8(0x11);
        output.extend(&[0, 12]);
        output.extend(&[127, 0, 0, 1]);
        output.extend(&[127, 0, 0, 2]);
        output.extend(&[0, 80]);
        output.extend(&[1, 187]);
        output.extend(msg.as_bytes());
        output
    }

    #[test]
    fn test_v2_proxy_decode() {
        let mut buf = test_msg_ip4("Hello");
        let mut codec = ProxyProtocolCodecV2::new();
        let info = codec
            .decode(&mut buf)
            .expect("decoding ok")
            .expect("has enough data");
        let src_addr: SocketAddr = "127.0.0.1:80".parse().unwrap();
        let dst_addr: SocketAddr = "127.0.0.2:443".parse().unwrap();
        assert_eq!(Some(src_addr), info.original_source);
        assert_eq!(Some(dst_addr), info.original_destination);
        assert_eq!(5, buf.len());
    }

    #[test]
    fn test_v2_encode() {
        let src_addr: SocketAddr = "127.0.0.1:80".parse().unwrap();
        let dst_addr: SocketAddr = "127.0.0.2:443".parse().unwrap();
        let info = ProxyInfo {
            socket_type: SocketType::Ipv4,
            original_source: Some(src_addr),
            original_destination: Some(dst_addr),
        };
        let mut buf = BytesMut::new();
        let mut codec = ProxyProtocolCodecV2::new();
        codec.encode(info.clone(), &mut buf).expect("encoding ok");
        let info2 = codec
            .decode(&mut buf)
            .expect("decoding ok")
            .expect("has enough data");
        assert_eq!(info, info2);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_v2_ip6_encode_decode() {
        let src_addr: SocketAddr = "[ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff]:80"
            .parse()
            .unwrap();
        let dst_addr: SocketAddr = "[aaaa:aaaa:aaaa:aaaa:aaaa:aaaa:aaaa:aaaa]:443"
            .parse()
            .unwrap();
        let info = ProxyInfo {
            socket_type: SocketType::Ipv6,
            original_source: Some(src_addr),
            original_destination: Some(dst_addr),
        };
        let mut buf = BytesMut::new();
        let mut codec = ProxyProtocolCodecV2::new();
        codec.encode(info.clone(), &mut buf).expect("encoding ok");
        let info2 = codec
            .decode(&mut buf)
            .expect("decoding ok")
            .expect("has enough data");
        assert_eq!(info, info2);
        assert!(buf.is_empty());
    }
}
