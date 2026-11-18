//! Host TCP dialing, including Darwin DNS64 synthesis for IPv4-only peers.

use std::io;
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

pub(super) fn connect(target: SocketAddr, timeout: Duration) -> io::Result<TcpStream> {
    #[cfg(any(target_os = "ios", target_os = "macos"))]
    let start = std::time::Instant::now();
    let result = TcpStream::connect_timeout(&target, timeout);
    #[cfg(any(target_os = "ios", target_os = "macos"))]
    if let (SocketAddr::V4(v4), Err(error)) = (target, &result) {
        if !v4.ip().is_loopback()
            && matches!(
                error.kind(),
                io::ErrorKind::NetworkUnreachable
                    | io::ErrorKind::HostUnreachable
                    | io::ErrorKind::AddrNotAvailable
            )
        {
            if let Some(v6) = synthesized(v4) {
                let left = timeout.saturating_sub(start.elapsed());
                if !left.is_zero() {
                    return TcpStream::connect_timeout(&v6, left);
                }
            }
        }
    }
    result
}

#[cfg(any(target_os = "ios", target_os = "macos"))]
fn synthesized(v4: std::net::SocketAddrV4) -> Option<SocketAddr> {
    use std::ffi::CString;
    use std::net::{Ipv6Addr, SocketAddrV6};
    use std::ptr;

    let name = CString::new(v4.ip().to_string()).ok()?;
    // Darwin synthesizes a usable IPv6 address from an IPv4 literal on
    // DNS64/NAT64 networks when the resolver chooses the address family.
    unsafe {
        let mut hints: libc::addrinfo = std::mem::zeroed();
        hints.ai_family = libc::AF_UNSPEC;
        hints.ai_socktype = libc::SOCK_STREAM;
        let mut results = ptr::null_mut();
        if libc::getaddrinfo(name.as_ptr(), ptr::null(), &hints, &mut results) != 0
            || results.is_null()
        {
            return None;
        }
        let mut item = results;
        let mut chosen = None;
        while let Some(addr) = item.as_ref() {
            if addr.ai_family == libc::AF_INET6
                && !addr.ai_addr.is_null()
                && addr.ai_addrlen as usize >= std::mem::size_of::<libc::sockaddr_in6>()
            {
                let sin = &*addr.ai_addr.cast::<libc::sockaddr_in6>();
                let ip = Ipv6Addr::from(sin.sin6_addr.s6_addr);
                if ip.to_ipv4_mapped().is_none() {
                    chosen = Some(SocketAddr::V6(SocketAddrV6::new(
                        ip,
                        v4.port(),
                        0,
                        sin.sin6_scope_id,
                    )));
                    break;
                }
            }
            item = addr.ai_next;
        }
        libc::freeaddrinfo(results);
        chosen
    }
}
