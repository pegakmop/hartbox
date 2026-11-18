//! ICMP echo proxying through unprivileged host "datagram" ICMP sockets, for
//! IPv4 (ICMP) and IPv6 (ICMPv6).
//!
//! Each guest echo flow (identifier, destination) gets its own host socket,
//! so replies cannot be mixed up between flows. The host kernel may rewrite
//! the echo identifier, so the gateway restores the guest's identifier. The
//! kernel also computes the ICMPv6 checksum, which needs the source address it
//! picks, so requests leave with a zero checksum.

use std::io;
use std::mem;
use std::net::IpAddr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

const ECHO_REQUEST: u8 = 8;
const ECHO_REPLY: u8 = 0;
const ECHO_REQUEST6: u8 = 128;
const ECHO_REPLY6: u8 = 129;

/// A host ICMP datagram socket connected to one destination.
pub struct IcmpSocket {
    fd: OwnedFd,
    dst: IpAddr,
}

impl IcmpSocket {
    pub fn new(dst: IpAddr) -> io::Result<Self> {
        let (family, proto) = match dst {
            IpAddr::V4(_) => (libc::AF_INET, libc::IPPROTO_ICMP),
            IpAddr::V6(_) => (libc::AF_INET6, libc::IPPROTO_ICMPV6),
        };
        // SAFETY: plain socket creation; the returned descriptor is owned by OwnedFd.
        let raw = unsafe { libc::socket(family, libc::SOCK_DGRAM, proto) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh descriptor that nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: fcntl on a descriptor we own.
        unsafe {
            let flags = libc::fcntl(fd.as_raw_fd(), libc::F_GETFL);
            if flags < 0 || libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(IcmpSocket { fd, dst })
    }

    /// Send an ICMP message (type, code, checksum, rest of header, data).
    pub fn send(&self, message: &[u8]) -> io::Result<()> {
        let (addr, len) = sockaddr(self.dst);
        // SAFETY: `message` and `addr` are valid for the duration of the call.
        let n = unsafe {
            libc::sendto(
                self.fd.as_raw_fd(),
                message.as_ptr().cast(),
                message.len(),
                0,
                (&addr as *const libc::sockaddr_storage).cast(),
                len,
            )
        };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Receive one ICMP message, if any. Returns the ICMP part (IP header removed).
    pub fn recv(&self, buf: &mut [u8]) -> Option<usize> {
        // SAFETY: `buf` is valid for writes of its length.
        let n = unsafe { libc::recv(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n <= 0 {
            return None;
        }
        let n = n as usize;
        // Some IPv4 hosts include the IP header in datagram ICMP replies.
        if self.dst.is_ipv4() && buf[0] >> 4 == 4 {
            let ihl = (buf[0] & 0x0f) as usize * 4;
            if ihl > n {
                return None;
            }
            buf.copy_within(ihl..n, 0);
            return Some(n - ihl);
        }
        Some(n)
    }
}

/// A socket address for `ip`, and its length.
fn sockaddr(ip: IpAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    // SAFETY: sockaddr_storage is plain old data; all-zero is a valid value.
    let mut storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
    match ip {
        IpAddr::V4(a) => {
            // SAFETY: sockaddr_storage is large and aligned enough for sockaddr_in.
            let sin = unsafe { &mut *(&mut storage as *mut libc::sockaddr_storage).cast::<libc::sockaddr_in>() };
            #[cfg(target_os = "macos")]
            {
                sin.sin_len = mem::size_of::<libc::sockaddr_in>() as u8;
            }
            sin.sin_family = libc::AF_INET as libc::sa_family_t;
            sin.sin_addr = libc::in_addr { s_addr: u32::from_ne_bytes(a.octets()) };
            (storage, mem::size_of::<libc::sockaddr_in>() as libc::socklen_t)
        }
        IpAddr::V6(a) => {
            // SAFETY: as above, for sockaddr_in6.
            let sin6 = unsafe { &mut *(&mut storage as *mut libc::sockaddr_storage).cast::<libc::sockaddr_in6>() };
            #[cfg(target_os = "macos")]
            {
                sin6.sin6_len = mem::size_of::<libc::sockaddr_in6>() as u8;
            }
            sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
            sin6.sin6_addr = libc::in6_addr { s6_addr: a.octets() };
            (storage, mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t)
        }
    }
}

/// Build an IPv4 echo request for the host socket.
pub fn echo_request(id: u16, seq: u16, data: &[u8]) -> Vec<u8> {
    let mut m = vec![ECHO_REQUEST, 0, 0, 0];
    m.extend_from_slice(&id.to_be_bytes());
    m.extend_from_slice(&seq.to_be_bytes());
    m.extend_from_slice(data);
    let c = super::proto::checksum(&m, 0);
    m[2..4].copy_from_slice(&c.to_be_bytes());
    m
}

/// Build an IPv6 echo request. The checksum is left zero for the kernel.
pub fn echo_request6(id: u16, seq: u16, data: &[u8]) -> Vec<u8> {
    let mut m = vec![ECHO_REQUEST6, 0, 0, 0];
    m.extend_from_slice(&id.to_be_bytes());
    m.extend_from_slice(&seq.to_be_bytes());
    m.extend_from_slice(data);
    m
}

/// Turn a host IPv4 echo reply into the reply the guest expects, restoring its identifier.
pub fn guest_reply(host_message: &[u8], guest_id: u16) -> Option<Vec<u8>> {
    echo_reply(host_message, guest_id, ECHO_REPLY, ECHO_REPLY)
}

/// Turn a host IPv6 echo reply into the reply the guest expects. The checksum is
/// left zero: it covers the addresses, which the caller knows.
pub fn guest_reply6(host_message: &[u8], guest_id: u16) -> Option<Vec<u8>> {
    echo_reply(host_message, guest_id, ECHO_REPLY6, ECHO_REPLY6)
}

fn echo_reply(host_message: &[u8], guest_id: u16, host_type: u8, guest_type: u8) -> Option<Vec<u8>> {
    if host_message.len() < 8 || host_message[0] != host_type {
        return None;
    }
    let seq = u16::from_be_bytes([host_message[6], host_message[7]]);
    let mut m = vec![guest_type, 0, 0, 0];
    m.extend_from_slice(&guest_id.to_be_bytes());
    m.extend_from_slice(&seq.to_be_bytes());
    m.extend_from_slice(&host_message[8..]);
    if guest_type == ECHO_REPLY {
        let c = super::proto::checksum(&m, 0);
        m[2..4].copy_from_slice(&c.to_be_bytes());
    }
    Some(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_echo_reply_restores_identifier() {
        let host = [ECHO_REPLY, 0, 0, 0, 0x12, 0x34, 0, 7, b'x'];
        let reply = guest_reply(&host, 0xabcd).expect("echo reply");
        assert_eq!(&reply[4..6], &[0xab, 0xcd]);
        assert_eq!(super::super::proto::checksum(&reply, 0), 0);
    }

    #[test]
    fn ipv6_echo_reply_restores_identifier_and_type() {
        let host = [ECHO_REPLY6, 0, 0, 0, 0x12, 0x34, 0, 9];
        let reply = guest_reply6(&host, 0x0042).expect("echo reply");
        assert_eq!((reply[0], &reply[4..8]), (ECHO_REPLY6, &[0, 0x42, 0, 9][..]));
        assert!(guest_reply(&host, 0x0042).is_none());
    }

    #[test]
    fn socket_addresses_have_the_right_family() {
        let (v4, len4) = sockaddr("10.0.2.3".parse().unwrap());
        assert_eq!(len4 as usize, mem::size_of::<libc::sockaddr_in>());
        assert_eq!(v4.ss_family as i32, libc::AF_INET);
        let (v6, len6) = sockaddr("fd00:2::3".parse().unwrap());
        assert_eq!(len6 as usize, mem::size_of::<libc::sockaddr_in6>());
        assert_eq!(v6.ss_family as i32, libc::AF_INET6);
    }
}
