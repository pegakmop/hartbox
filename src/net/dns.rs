//! Bridge guest DNS to the Apple system resolver, including split DNS.

use std::ffi::CString;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, UdpSocket};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use super::{DNS_IP, DNS_IP6};

const HEADER: usize = 12;
const UDP_MAX: usize = 1232;
const MAX_DNS: usize = u16::MAX as usize;

pub(super) fn handles(dest: IpAddr, port: u16) -> bool {
    port == 53 && (dest == Ipv4Addr::from(DNS_IP) || dest == Ipv6Addr::from(DNS_IP6))
}

struct Question {
    name: CString,
    kind: u16,
    class: u16,
    end: usize,
}

fn word(data: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(data.get(at..at + 2)?.try_into().ok()?))
}

fn question(packet: &[u8]) -> Option<Question> {
    if word(packet, 2)? & 0xf800 != 0
        || word(packet, 4)? != 1
        || word(packet, 6)? != 0
        || word(packet, 8)? != 0
    {
        return None;
    }
    let mut at = HEADER;
    let mut name = Vec::new();
    loop {
        let length = usize::from(*packet.get(at)?);
        at += 1;
        if length == 0 {
            break;
        }
        if length > 63 || at + length - HEADER >= 255 {
            return None;
        }
        for &byte in packet.get(at..at + length)? {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') {
                name.push(byte);
            } else {
                // The resolver expects a presentation name. Escape characters
                // that would otherwise become separators or string terminators.
                name.extend_from_slice(&[
                    b'\\',
                    b'0' + byte / 100,
                    b'0' + byte / 10 % 10,
                    b'0' + byte % 10,
                ]);
            }
        }
        name.push(b'.');
        at += length;
    }
    if name.is_empty() {
        name.push(b'.');
    }
    Some(Question {
        name: CString::new(name).ok()?,
        kind: word(packet, at)?,
        class: word(packet, at + 2)?,
        end: at + 4,
    })
}

fn short_reply(query: &[u8], end: usize, flags: u16) -> Vec<u8> {
    let mut reply = query[..end].to_vec();
    let flags = 0x8080 | (word(query, 2).unwrap_or(0) & 0x0100) | flags;
    reply[2..4].copy_from_slice(&flags.to_be_bytes());
    reply[6..HEADER].fill(0);
    if end == HEADER {
        reply[4..6].fill(0);
    }
    reply
}

pub(super) fn resolve(query: &[u8], udp: bool, fallback: Option<IpAddr>) -> Option<Vec<u8>> {
    if query.len() < HEADER || query[2] & 0x80 != 0 {
        return None;
    }
    let Some(q) = question(query) else {
        return Some(short_reply(query, HEADER, 1));
    };
    let mut answer = match system_query(&q) {
        Some(SystemReply::Message(reply)) => reply,
        Some(SystemReply::Negative(code)) => return Some(short_reply(query, q.end, code)),
        None => match fallback.and_then(|ip| forward(query, udp, ip).ok()) {
            Some(reply) => reply,
            None => return Some(short_reply(query, q.end, 2)),
        },
    };
    if udp && answer.len() > udp_limit(query, q.end) {
        return Some(short_reply(query, q.end, 0x0200));
    }
    answer[..2].copy_from_slice(&query[..2]);
    Some(answer)
}

fn forward(query: &[u8], udp: bool, server: IpAddr) -> io::Result<Vec<u8>> {
    let server = SocketAddr::new(server, 53);
    let timeout = Duration::from_secs(3);
    let answer = if udp {
        let socket = UdpSocket::bind(if server.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        })?;
        socket.connect(server)?;
        socket.set_read_timeout(Some(timeout))?;
        socket.set_write_timeout(Some(timeout))?;
        socket.send(query)?;
        let mut buf = vec![0; MAX_DNS];
        let len = socket.recv(&mut buf)?;
        buf.truncate(len);
        buf
    } else {
        let mut socket = TcpStream::connect_timeout(&server, timeout)?;
        socket.set_read_timeout(Some(timeout))?;
        socket.set_write_timeout(Some(timeout))?;
        socket.write_all(&(query.len() as u16).to_be_bytes())?;
        socket.write_all(query)?;
        read_message(&mut socket)?
    };
    if answer.len() < HEADER || answer[2] & 0x80 == 0 || answer[..2] != query[..2] {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unmatched DNS reply",
        ));
    }
    Ok(answer)
}

fn udp_limit(query: &[u8], end: usize) -> usize {
    // An EDNS OPT record uses the root owner and puts the UDP size in CLASS.
    if word(query, 10) == Some(1) && query.get(end) == Some(&0) && word(query, end + 1) == Some(41)
    {
        return usize::from(word(query, end + 3).unwrap_or(512)).clamp(512, UDP_MAX);
    }
    512
}

#[cfg_attr(not(any(target_os = "ios", target_os = "macos")), allow(dead_code))]
enum SystemReply {
    Message(Vec<u8>),
    Negative(u16),
}

#[cfg(any(target_os = "ios", target_os = "macos"))]
fn system_query(q: &Question) -> Option<SystemReply> {
    use libc::{c_char, c_int, c_void, sockaddr, sockaddr_storage};
    #[link(name = "resolv")]
    extern "C" {
        fn dns_open(name: *const c_char) -> *const c_void;
        fn dns_free(handle: *const c_void);
        fn dns_query(
            handle: *const c_void,
            name: *const c_char,
            class: u32,
            kind: u32,
            reply: *mut c_char,
            len: u32,
            from: *mut sockaddr,
            fromlen: *mut u32,
        ) -> c_int;
    }
    // Each worker uses its own resolver handle and valid output storage.
    unsafe {
        let handle = dns_open(std::ptr::null());
        if handle.is_null() {
            return None;
        }
        let mut answer = vec![0; MAX_DNS];
        let mut from: sockaddr_storage = std::mem::zeroed();
        let mut from_len = std::mem::size_of_val(&from) as u32;
        let count = dns_query(
            handle,
            q.name.as_ptr(),
            u32::from(q.class),
            u32::from(q.kind),
            answer.as_mut_ptr().cast(),
            answer.len() as u32,
            (&mut from as *mut sockaddr_storage).cast(),
            &mut from_len,
        );
        dns_free(handle);
        if count < 0 && answer[2] & 0x80 != 0 {
            let code = u16::from(answer[3] & 15);
            if matches!(code, 0 | 3) {
                return Some(SystemReply::Negative(code));
            }
        }
        let len = usize::try_from(count).ok()?;
        if !(HEADER..=MAX_DNS).contains(&len) || answer[2] & 0x80 == 0 {
            return None;
        }
        answer.truncate(len);
        Some(SystemReply::Message(answer))
    }
}

#[cfg(not(any(target_os = "ios", target_os = "macos")))]
fn system_query(q: &Question) -> Option<SystemReply> {
    let _ = (&q.name, q.kind, q.class);
    None
}

/// DNS over TCP uses the same resolver and a length prefix for each message.
/// A local, connected socket pair keeps the resolver off the machine thread.
pub(super) fn serve_tcp(mut socket: UnixStream, fallback: Option<IpAddr>) -> io::Result<()> {
    let idle = Some(Duration::from_secs(10));
    socket.set_read_timeout(idle)?;
    socket.set_write_timeout(idle)?;
    loop {
        let packet = read_message(&mut socket)?;
        let Some(answer) = resolve(&packet, false, fallback) else {
            return Ok(());
        };
        socket.write_all(&(answer.len() as u16).to_be_bytes())?;
        socket.write_all(&answer)?;
    }
}

fn read_message(socket: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut prefix = [0; 2];
    socket.read_exact(&mut prefix)?;
    let mut packet = vec![0; usize::from(u16::from_be_bytes(prefix))];
    socket.read_exact(&mut packet)?;
    Ok(packet)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_question_and_caps_edns_for_either_guest_family() {
        let mut packet = vec![0x42, 0x19, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0];
        packet.extend_from_slice(b"\x07example\x03com\0\0\x1c\0\x01");
        let q = question(&packet).unwrap();
        assert_eq!(q.name.to_bytes(), b"example.com.");
        assert_eq!((q.kind, q.class, q.end), (28, 1, packet.len()));
        assert_eq!(udp_limit(&packet, q.end), 512);
        packet[11] = 1;
        packet.extend_from_slice(&[0, 0, 41, 0x10, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(udp_limit(&packet, q.end), UDP_MAX);
        let reply = short_reply(&packet, q.end, 0x0200);
        assert_eq!(&reply[..4], &[0x42, 0x19, 0x83, 0x80]);
        assert_eq!(word(&reply, 10), Some(0));
        assert_eq!(reply.len(), q.end);
        packet[HEADER] = 0xc0;
        assert!(question(&packet).is_none());
        assert_eq!(resolve(&packet, true, None).unwrap()[3] & 15, 1);
    }
}
