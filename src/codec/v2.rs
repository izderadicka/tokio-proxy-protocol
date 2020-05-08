#![allow(dead_code)]
use bytes::{Buf, BufMut, BytesMut};
use thiserror::Error;

pub const SIGNATURE: &[u8] = b"\x0D\x0A\x0D\x0A\x00\x0D\x0A\x51\x55\x49\x54\x0A";

// PROXY Protocol version
// As of this specification, it must
// always be sent as \x2 and the receiver must only accept this value.

const PROXY_VERSION: u8 = 0x2;

// Commands

// \x0 : LOCAL : the connection was established on purpose by the proxy
// without being relayed. The connection endpoints are the sender and the
// receiver. Such connections exist when the proxy sends health-checks to the
// server. The receiver must accept this connection as valid and must use the
// real connection endpoints and discard the protocol block including the
// family which is ignored.
const COMMAND_LOCAL: u8 = 0x0;

// \x1 : PROXY : the connection was established on behalf of another node,
// and reflects the original connection endpoints. The receiver must then use
// the information provided in the protocol block to get original the address.
const COMMAND_PROXY: u8 = 0x1;

// version and command
const VERSION_COMMAND: u8 = 0x21;

// Protocol byte

// \x00 : UNSPEC : the connection is forwarded for an unknown, unspecified
// or unsupported protocol. The sender should use this family when sending
// LOCAL commands or when dealing with unsupported protocol families. When
// used with a LOCAL command, the receiver must accept the connection and
// ignore any address information. For other commands, the receiver is free
// to accept the connection anyway and use the real endpoints addresses or to
// reject the connection. The receiver should ignore address information.
const PROTOCOL_UNSPEC: u8 = 0x00;

// \x11 : TCP over IPv4 : the forwarded connection uses TCP over the AF_INET
// protocol family. Address length is 2*4 + 2*2 = 12 bytes.
const PROTOCOL_TCP_IP4: u8 = 0x11;

// \x12 : UDP over IPv4 : the forwarded connection uses UDP over the AF_INET
// protocol family. Address length is 2*4 + 2*2 = 12 bytes.
const PROTOCOL_UDP_IP4: u8 = 0x12;

//  \x21 : TCP over IPv6 : the forwarded connection uses TCP over the AF_INET6
// protocol family. Address length is 2*16 + 2*2 = 36 bytes.
const PROTOCOL_TCP_IP6: u8 = 0x21;

// - \x22 : UDP over IPv6 : the forwarded connection uses UDP over the AF_INET6
// protocol family. Address length is 2*16 + 2*2 = 36 bytes.
const PROTOCOL_UDP_IP6: u8 = 0x22;

// - \x31 : UNIX stream : the forwarded connection uses SOCK_STREAM over the
// AF_UNIX protocol family. Address length is 2*108 = 216 bytes.
const PROTOCOL_UNIX_SOCKET: u8 = 0x31;

// - \x32 : UNIX datagram : the forwarded connection uses SOCK_DGRAM over the
// AF_UNIX protocol family. Address length is 2*108 = 216 bytes.
const PROTOCOL_UNIX_DATAGRAM: u8 = 0x32;

const VALID_PROTOCOLS: &[u8] = &[
    PROTOCOL_UNSPEC,
    PROTOCOL_TCP_IP4,
    PROTOCOL_TCP_IP6,
    PROTOCOL_UDP_IP4,
    PROTOCOL_UDP_IP6,
    PROTOCOL_UNIX_SOCKET,
    PROTOCOL_UNIX_DATAGRAM,
];

// Length

const SIZE_HEADER: u16 = 16;
const SIZE_ADDRESSES_IP4: u16 = 12;
const SIZE_ADDRESSES_IP6: u16 = 36;
const SIZE_ADDRESSES_UNIX: u16 = 216;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Invalid header: {0}")]
    Header(String),
    #[error("Invalid address: {0}")]
    Address(String),
}

type Result<T> = std::result::Result<T, Error>;
trait Serialize: Sized {
    fn deserialize(buf: &mut BytesMut) -> Result<Self>;
    fn serialize(&self, buf: &mut BytesMut);
}

#[derive(Debug, PartialEq, Eq)]
struct Header {
    version_and_command: u8,
    protocol: u8,
    len: u16,
}

impl Header {
    fn new_tcp4() -> Self {
        Header {
            version_and_command: VERSION_COMMAND,
            protocol: PROTOCOL_TCP_IP4,
            len: SIZE_ADDRESSES_IP4,
        }
    }
}

impl Serialize for Header {
    fn deserialize(buf: &mut BytesMut) -> Result<Self> {
        if buf.len() < SIZE_HEADER as usize {
            return Err(Error::Header("Too few bytes".into()));
        }

        if &buf[0..SIGNATURE.len()] != SIGNATURE {
            return Err(Error::Header("Invalid signature".into()));
        };
        buf.advance(SIGNATURE.len());
        let version_and_command = buf.get_u8();
        if (version_and_command & 0xF0) >> 4 != PROXY_VERSION {
            return Err(Error::Header("Invalid Version".into()));
        }
        if version_and_command & 0x0F > COMMAND_PROXY {
            return Err(Error::Header("Invalid command".into()));
        }

        let protocol = buf.get_u8();
        if ! VALID_PROTOCOLS.contains(&protocol) {
            return Err(Error::Header("Invalid network protocol specified".into()));
        }
        let len = buf.get_u16();
        Ok(Header{
            version_and_command,
            protocol,
            len
        })

    }
    fn serialize(&self, buf: &mut BytesMut) {
        buf.reserve((SIZE_HEADER + self.len) as usize);
        buf.put(SIGNATURE);
        buf.put_u8(self.version_and_command);
        buf.put_u8(self.protocol);
        buf.put_u16(self.len);
    }
}

struct Ip4Addresses {
    src_addr: u32,
    dst_addr: u32,
    src_port: u16,
    dst_port: u16,
}

struct Ip6Addresses {
    src_addr: [u8; 16],
    dst_addr: [u8; 16],
    src_port: u16,
    dst_port: u16,
}

struct UnixAddresses {
    src_addr: [u8; 108],
    dst_addr: [u8; 108],
}


#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_header_serialize_deserialize() {
        let h1 = Header::new_tcp4();
        let mut buf = BytesMut::new();
        h1.serialize(&mut buf);
        let h2 = Header::deserialize(&mut buf).expect("deserialization error");
        assert_eq!(h1, h2);
        assert!(buf.is_empty());
    }

}
