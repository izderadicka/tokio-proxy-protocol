use crate::error::{Error, Result};
use bytes::Buf;
use std::net::{Ipv4Addr, SocketAddr};
use tokio_util::codec::{Decoder, Encoder};

pub const MAX_HEADER_SIZE: usize = 536;
pub const MIN_HEADER_SIZE: usize = 15;

pub enum SocketType {
    Ipv4,
    Ipv6,
    Unknown,
}

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

    fn decode(&mut self, buf: &mut bytes::BytesMut) -> Result<Option<Self::Item>> {
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

#[cfg(test)]
mod tests {
    use super::*;
}
