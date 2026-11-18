//! User-mode network gateway: gives the guest internet access through the
//! host, with no host-side configuration or privileges.
//!
//! IPv4 addressing follows QEMU's user networking: the guest is 10.0.2.15, the
//! gateway 10.0.2.2 (which reaches the host's loopback), and the DNS server
//! 10.0.2.3 is forwarded to the host's resolver. IPv6 uses the ULA prefix
//! fd00:2::/64: the guest is fd00:2::15, the gateway fd00:2::2 and the DNS
//! server fd00:2::3.
//!
//! * ARP and neighbor discovery: the gateway answers for every address except
//!   the guest's own.
//! * UDP: each guest flow gets a host UDP socket; replies are wrapped back.
//! * TCP: smoltcp terminates the guest's connection on the gateway side, and
//!   the bytes are shuttled to a host TCP socket. A listener is created for
//!   each new SYN, and the gateway's default route accepts any destination
//!   (smoltcp's AnyIP).
//! * ICMP and ICMPv6 echo: proxied through host datagram sockets.

mod dns;
mod host;
mod icmp;
mod proto;

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::thread;
use std::time::Duration as StdDuration;
use std::time::{Duration, Instant as WallClock};

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::tcp::{self, Socket as TcpSocket, State};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint, Ipv4Address, Ipv6Address};

use proto::*;

pub const GUEST_IP: [u8; 4] = [10, 0, 2, 15];
pub const GATEWAY_IP: [u8; 4] = [10, 0, 2, 2];
pub const DNS_IP: [u8; 4] = [10, 0, 2, 3];
pub const GUEST_IP6: [u8; 16] = [0xfd, 0x00, 0x00, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x15];
pub const GATEWAY_IP6: [u8; 16] = [0xfd, 0x00, 0x00, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x02];
pub const DNS_IP6: [u8; 16] = [0xfd, 0x00, 0x00, 0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x03];
/// The guest's link-local address, derived from its MAC address.
const GUEST_LL6: [u8; 16] = [0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0x50, 0x54, 0x00, 0xff, 0xfe, 0x12, 0x34, 0x56];
pub const GUEST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];
pub const GATEWAY_MAC: [u8; 6] = [0x52, 0x55, 0x0a, 0x00, 0x02, 0x02];

const TCP_BUFFER: usize = 64 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_PENDING_TCP: usize = 64;
const MAX_PENDING_DNS: usize = 32;
const UDP_IDLE: Duration = Duration::from_secs(60);
const FALLBACK_DNS: Ipv4Addr = Ipv4Addr::new(8, 8, 8, 8);
/// First port used for gateway-side sockets that connect into the guest.
const EPHEMERAL_START: u16 = 49152;

/// Forward connections to `bind:host_port` on the host to `guest_port` in the guest.
/// `bind` is 127.0.0.1 unless the forward should be reachable from the network
/// (0.0.0.0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortForward {
    pub bind: Ipv4Addr,
    pub host_port: u16,
    pub guest_port: u16,
}

impl PortForward {
    /// A forward on the host's loopback address.
    pub fn local(host_port: u16, guest_port: u16) -> Self {
        PortForward { bind: Ipv4Addr::LOCALHOST, host_port, guest_port }
    }
}

/// A forward an embedder opens and closes while the guest runs (the iOS app does
/// it when the guest asks for remote access). The listener is bound by the
/// caller, so a port in use is reported to it, and handed to the gateway through
/// this slot; the generation tells the gateway there is something new to take.
struct Exposure {
    handoff: Option<(TcpListener, u16)>,
    /// What is open now (bind address, host port, guest port), so asking for the
    /// same thing again changes nothing.
    active: Option<(Ipv4Addr, u16, u16)>,
}

static EXPOSED: std::sync::Mutex<Exposure> = std::sync::Mutex::new(Exposure { handoff: None, active: None });
static EXPOSE_GEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn exposure() -> std::sync::MutexGuard<'static, Exposure> {
    EXPOSED.lock().unwrap_or_else(|e| e.into_inner())
}

/// Listen on `bind:host_port` and forward connections to `guest_port` in the
/// guest, replacing an earlier call with other arguments. Fails if the port
/// cannot be bound.
pub fn expose(bind: Ipv4Addr, host_port: u16, guest_port: u16) -> io::Result<()> {
    let want = (bind, host_port, guest_port);
    if exposure().active == Some(want) {
        return Ok(());
    }
    unexpose();
    let mut tries = 0;
    let listener = loop {
        match TcpListener::bind((bind, host_port)) {
            Ok(l) => break l,
            // The gateway drops the listener it was using on its next poll.
            Err(e) if e.kind() == io::ErrorKind::AddrInUse && tries < 50 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e),
        }
    };
    listener.set_nonblocking(true)?;
    {
        let mut slot = exposure();
        slot.handoff = Some((listener, guest_port));
        slot.active = Some(want);
    }
    EXPOSE_GEN.fetch_add(1, std::sync::atomic::Ordering::Release);
    Ok(())
}

/// Stop listening on the port opened by `expose`. Connections already open stay.
pub fn unexpose() {
    {
        let mut slot = exposure();
        slot.handoff = None;
        slot.active = None;
    }
    EXPOSE_GEN.fetch_add(1, std::sync::atomic::Ordering::Release);
}

fn ip4(a: [u8; 4]) -> IpAddr {
    IpAddr::V4(Ipv4Addr::from(a))
}

fn ip6(a: [u8; 16]) -> IpAddr {
    IpAddr::V6(Ipv6Addr::from(a))
}

/// A smoltcp address for `ip`.
fn to_smol(ip: IpAddr) -> IpAddress {
    match ip {
        IpAddr::V4(a) => IpAddress::Ipv4(a),
        IpAddr::V6(a) => IpAddress::Ipv6(a),
    }
}

/// An Ethernet frame carrying the IP `proto` message `payload` from `src` to the guest.
fn ip_frame(src: IpAddr, dst: IpAddr, proto: u8, payload: &[u8]) -> Option<Vec<u8>> {
    match (src, dst) {
        (IpAddr::V4(s), IpAddr::V4(d)) => {
            let packet = ipv4_packet(s.octets(), d.octets(), proto, payload);
            Some(eth_frame(GUEST_MAC, GATEWAY_MAC, ETHERTYPE_IPV4, &packet))
        }
        (IpAddr::V6(s), IpAddr::V6(d)) => {
            let packet = ipv6_packet(s.octets(), d.octets(), proto, payload);
            Some(eth_frame(GUEST_MAC, GATEWAY_MAC, ETHERTYPE_IPV6, &packet))
        }
        _ => None,
    }
}

/// A UDP datagram from `src` to `dst`, with the checksum its family requires.
fn udp_payload(src: IpAddr, dst: IpAddr, sport: u16, dport: u16, data: &[u8]) -> Vec<u8> {
    match (src, dst) {
        (IpAddr::V4(s), IpAddr::V4(d)) => udp_datagram(s.octets(), d.octets(), sport, dport, data),
        (IpAddr::V6(s), IpAddr::V6(d)) => udp_datagram6(s.octets(), d.octets(), sport, dport, data),
        _ => Vec::new(),
    }
}

/// An ICMP echo reply message from `src` to `dst`, with its checksum set.
fn echo_reply_message(src: IpAddr, dst: IpAddr, message: Vec<u8>) -> Vec<u8> {
    match (src, dst) {
        (IpAddr::V6(s), IpAddr::V6(d)) => icmpv6_message(s.octets(), d.octets(), message),
        _ => message,
    }
}

/// The "cable" between the guest and smoltcp.
#[derive(Default)]
struct Link {
    rx: VecDeque<Vec<u8>>,
    tx: VecDeque<Vec<u8>>,
}

struct RxTok(Vec<u8>);

impl RxToken for RxTok {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.0)
    }
}

struct TxTok<'a>(&'a mut VecDeque<Vec<u8>>);

impl TxToken for TxTok<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0u8; len];
        let r = f(&mut buf);
        self.0.push_back(buf);
        r
    }
}

impl Device for Link {
    type RxToken<'a>
        = RxTok
    where
        Self: 'a;
    type TxToken<'a>
        = TxTok<'a>
    where
        Self: 'a;

    fn receive(&mut self, _now: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let frame = self.rx.pop_front()?;
        Some((RxTok(frame), TxTok(&mut self.tx)))
    }

    fn transmit(&mut self, _now: Instant) -> Option<Self::TxToken<'_>> {
        Some(TxTok(&mut self.tx))
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1500;
        caps
    }
}

/// One guest TCP connection and its host-side socket.
struct TcpConn {
    handle: SocketHandle,
    host: TcpPeer,
    host_eof: bool,
    host_shut: bool,
    handshake: Option<WallClock>,
}

enum TcpPeer {
    Host(TcpStream),
    Dns(UnixStream),
}

impl TcpPeer {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self { Self::Host(s) => s.read(buf), Self::Dns(s) => s.read(buf) }
    }

    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self { Self::Host(s) => s.write(buf), Self::Dns(s) => s.write(buf) }
    }

    fn shutdown_write(&self) -> io::Result<()> {
        match self { Self::Host(s) => s.shutdown(Shutdown::Write), Self::Dns(s) => s.shutdown(Shutdown::Write) }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct TcpKey {
    guest: IpAddr,
    source: u16,
    dest: IpAddr,
    port: u16,
}

struct Connecting {
    result: Receiver<io::Result<TcpStream>>,
    syn: Vec<u8>,
    seq: u32,
}

struct DnsDatagram {
    guest: IpAddr,
    server: IpAddr,
    port: u16,
    response: Receiver<Option<Vec<u8>>>,
}

/// One guest UDP flow and its host-side socket.
struct UdpFlow {
    sock: UdpSocket,
    /// The address the guest sent to, which replies must come from.
    dst: IpAddr,
    dst_port: u16,
    guest_port: u16,
    /// The guest's own address for this flow (IPv4 or IPv6).
    guest: IpAddr,
    last_used: WallClock,
}

/// Guest flow key: (guest port, destination address, destination port).
type FlowKey = (u16, IpAddr, u16);

/// A guest echo flow: the host socket and the identifier the guest chose.
struct IcmpFlow {
    sock: icmp::IcmpSocket,
    guest_id: u16,
    guest: IpAddr,
    last_used: WallClock,
}

const ICMP_IDLE: StdDuration = StdDuration::from_secs(30);

/// How long an unsent datagram is kept while the host has no route yet.
const PENDING_IDLE: StdDuration = StdDuration::from_secs(20);
/// Cap on unsent datagrams held, oldest dropped first.
const PENDING_LIMIT: usize = 64;

/// Test builds can pretend the host has no route, to exercise the retry path
/// that the phone hits when it comes up before its network does.
#[cfg(test)]
static FAIL_SENDS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[inline(always)]
fn host_send_blocked() -> bool {
    #[cfg(test)]
    {
        FAIL_SENDS.load(std::sync::atomic::Ordering::Relaxed)
    }
    #[cfg(not(test))]
    {
        false
    }
}

/// A datagram the host could not send yet: the flow's socket could not be
/// created, or the send failed because the host had no route to it. A real
/// gateway would have forwarded the packet, and the guest has no way to know it
/// was our side that failed: a dropped UDP query costs it a whole resolver
/// timeout, which is what a phone shows as a temporary name resolution failure
/// on the first command after boot. Keeping it and retrying turns that into a
/// delay of a few milliseconds.
struct Pending<A> {
    /// Guest-visible destination, used as the flow key for UDP.
    dst: A,
    /// The guest's own address for the flow, which replies go back to.
    guest: IpAddr,
    target: SocketAddr,
    payload: Vec<u8>,
    queued: WallClock,
}

pub struct Gateway {
    clock: WallClock,
    iface: Interface,
    sockets: SocketSet<'static>,
    link: Link,
    tcp: HashMap<TcpKey, TcpConn>,
    connecting: HashMap<TcpKey, Connecting>,
    tcp_read_buffer: Box<[u8]>,
    dns_datagrams: Vec<DnsDatagram>,
    udp: HashMap<FlowKey, UdpFlow>,
    icmp: HashMap<(u16, IpAddr), IcmpFlow>,
    to_guest: VecDeque<Vec<u8>>,
    /// Sockets of closed flows, removed after the final poll so that an
    /// aborted connection's reset still goes out.
    retired: Vec<SocketHandle>,
    dns_server: IpAddr,
    dns_fallback: Option<IpAddr>,
    system_dns: bool,
    /// Datagrams that could not be sent yet, retried every poll.
    pending_udp: VecDeque<Pending<(u16, IpAddr)>>,
    pending_icmp: VecDeque<Pending<(u16, IpAddr)>>,
    listeners: Vec<(TcpListener, u16)>,
    /// The forward opened by `expose`, and the generation of it this gateway has seen.
    exposed: Option<(TcpListener, u16)>,
    exposed_gen: u32,
    next_port: u16,
    ipv6: bool,
    /// Frames the guest sent (for statistics).
    pub frames_from_guest: u64,
    /// Frames the gateway sent to the guest (for statistics).
    pub frames_to_guest: u64,
}

impl Gateway {
    /// Create the gateway, listening on the host for each port forward. With
    /// `ipv6`, the gateway also serves the fd00:2::/64 network.
    pub fn new(forwards: &[PortForward], ipv6: bool) -> io::Result<Self> {
        let mut listeners = Vec::with_capacity(forwards.len());
        for f in forwards {
            let listener = TcpListener::bind((f.bind, f.host_port))?;
            listener.set_nonblocking(true)?;
            listeners.push((listener, f.guest_port));
        }
        let mut link = Link::default();
        let cfg = Config::new(HardwareAddress::Ethernet(EthernetAddress(GATEWAY_MAC)));
        let mut iface = Interface::new(cfg, &mut link, Instant::ZERO);
        iface.update_ip_addrs(|addrs| {
            let _ = addrs.push(IpCidr::new(to_smol(ip4(GATEWAY_IP)), 24));
            if ipv6 {
                let _ = addrs.push(IpCidr::new(to_smol(ip6(GATEWAY_IP6)), 64));
            }
        });
        iface.set_any_ip(true);
        iface
            .routes_mut()
            .add_default_ipv4_route(Ipv4Address::new(GATEWAY_IP[0], GATEWAY_IP[1], GATEWAY_IP[2], GATEWAY_IP[3]))
            .expect("route table has room for the default route");
        if ipv6 {
            let w = |i: usize| u16::from_be_bytes([GATEWAY_IP6[2 * i], GATEWAY_IP6[2 * i + 1]]);
            iface
                .routes_mut()
                .add_default_ipv6_route(Ipv6Address::new(w(0), w(1), w(2), w(3), w(4), w(5), w(6), w(7)))
                .expect("route table has room for the default route");
        }
        let forced_dns = forced_dns_server();
        let configured_dns = configured_dns_server();
        Ok(Gateway {
            clock: WallClock::now(),
            iface,
            sockets: SocketSet::new(Vec::new()),
            link,
            tcp: HashMap::new(),
            connecting: HashMap::new(),
            tcp_read_buffer: vec![0; TCP_BUFFER].into_boxed_slice(),
            dns_datagrams: Vec::new(),
            pending_udp: VecDeque::new(),
            pending_icmp: VecDeque::new(),
            udp: HashMap::new(),
            icmp: HashMap::new(),
            to_guest: VecDeque::new(),
            retired: Vec::new(),
            dns_server: forced_dns.or(configured_dns).unwrap_or(IpAddr::V4(FALLBACK_DNS)),
            dns_fallback: configured_dns,
            system_dns: cfg!(any(target_os = "macos", target_os = "ios")) && forced_dns.is_none(),
            listeners,
            exposed: None,
            exposed_gen: 0,
            next_port: EPHEMERAL_START,
            ipv6,
            frames_from_guest: 0,
            frames_to_guest: 0,
        })
    }

    /// Accept host connections on forwarded ports and open them into the guest.
    fn accept_forwards(&mut self) {
        let gen = EXPOSE_GEN.load(std::sync::atomic::Ordering::Acquire);
        if gen != self.exposed_gen {
            self.exposed_gen = gen;
            self.exposed = exposure().handoff.take();
        }
        let mut accepted = Vec::new();
        for (listener, guest_port) in self.listeners.iter().chain(self.exposed.iter()) {
            while let Ok((stream, _)) = listener.accept() {
                accepted.push((stream, *guest_port));
            }
        }
        for (stream, guest_port) in accepted {
            if stream.set_nonblocking(true).is_err() {
                continue;
            }
            let local_port = self.free_port(guest_port);
            let rx = tcp::SocketBuffer::new(vec![0u8; TCP_BUFFER]);
            let tx = tcp::SocketBuffer::new(vec![0u8; TCP_BUFFER]);
            let mut sock = TcpSocket::new(rx, tx);
            let remote = IpEndpoint::new(to_smol(ip4(GUEST_IP)), guest_port);
            let local = IpListenEndpoint { addr: Some(to_smol(ip4(GATEWAY_IP))), port: local_port };
            if sock.connect(self.iface.context(), remote, local).is_err() {
                continue;
            }
            let handle = self.sockets.add(sock);
            // The guest answers from its service port to our ephemeral port.
            let key = TcpKey { guest: ip4(GUEST_IP), source: guest_port, dest: ip4(GATEWAY_IP), port: local_port };
            self.tcp.insert(
                key,
                TcpConn { handle, host: TcpPeer::Host(stream), host_eof: false, host_shut: false, handshake: Some(WallClock::now()) },
            );
        }
    }

    /// An ephemeral port not already used by a gateway-side connection.
    fn free_port(&mut self, guest_port: u16) -> u16 {
        loop {
            let port = self.next_port;
            self.next_port = if port == u16::MAX { EPHEMERAL_START } else { port + 1 };
            let key = TcpKey { guest: ip4(GUEST_IP), source: guest_port, dest: ip4(GATEWAY_IP), port };
            if !self.tcp.contains_key(&key) && !self.connecting.contains_key(&key) {
                return port;
            }
        }
    }

    /// Whether any connection is open. The machine polls more often while so.
    pub fn is_busy(&self) -> bool {
        !self.tcp.is_empty() || !self.connecting.is_empty() || !self.dns_datagrams.is_empty()
            || !self.udp.is_empty() || !self.icmp.is_empty()
    }

    /// Feed one Ethernet frame sent by the guest.
    pub fn from_guest(&mut self, frame: &[u8]) {
        let Some(eth) = parse_eth(frame) else { return };
        // IPv6 neighbor discovery goes to solicited-node multicast MAC addresses.
        let ipv6_multicast = eth.ethertype == ETHERTYPE_IPV6 && eth.dst[..2] == [0x33, 0x33];
        if eth.dst != GATEWAY_MAC && eth.dst != BROADCAST_MAC && !ipv6_multicast {
            return;
        }
        self.frames_from_guest += 1;
        match eth.ethertype {
            ETHERTYPE_ARP => self.arp(frame, eth.payload),
            ETHERTYPE_IPV4 => self.ipv4(frame, eth.payload),
            ETHERTYPE_IPV6 if self.ipv6 => self.ipv6(frame, eth.payload),
            _ => {}
        }
    }

    fn arp(&mut self, frame: &[u8], payload: &[u8]) {
        let Some(arp) = parse_arp(payload) else { return };
        if arp.op == 1 && arp.target_ip != GUEST_IP {
            // Proxy ARP: every address other than the guest's is "us".
            let reply = arp_reply(GATEWAY_MAC, arp.target_ip, &arp);
            self.push_to_guest(eth_frame(arp.sender_mac, GATEWAY_MAC, ETHERTYPE_ARP, &reply));
        } else if arp.sender_ip == GUEST_IP {
            // Let smoltcp learn the guest's address from its requests and replies.
            self.link.rx.push_back(frame.to_vec());
        }
    }

    fn ipv4(&mut self, frame: &[u8], packet: &[u8]) {
        let Some(ip) = parse_ipv4(packet) else { return };
        if ip.src != GUEST_IP {
            return;
        }
        let guest = ip4(GUEST_IP);
        match ip.proto {
            PROTO_UDP => self.udp_from_guest(guest, ip4(ip.dst), ip.payload),
            PROTO_TCP => self.tcp_from_guest(guest, ip4(ip.dst), ip.payload, frame),
            PROTO_ICMP if ip.dst == GATEWAY_IP => self.link.rx.push_back(frame.to_vec()),
            PROTO_ICMP => self.icmp_from_guest(guest, ip4(ip.dst), ip.payload),
            _ => {}
        }
    }

    fn ipv6(&mut self, frame: &[u8], packet: &[u8]) {
        let Some(ip) = parse_ipv6(packet) else { return };
        // Packets from the unspecified address (duplicate address detection) are ignored.
        if ip.src != GUEST_IP6 && ip.src != GUEST_LL6 {
            return;
        }
        let guest = ip6(ip.src);
        match ip.next {
            PROTO_UDP => self.udp_from_guest(guest, ip6(ip.dst), ip.payload),
            PROTO_TCP => self.tcp_from_guest(guest, ip6(ip.dst), ip.payload, frame),
            PROTO_ICMPV6 => self.icmpv6_from_guest(ip.src, ip.dst, ip.payload, frame),
            _ => {}
        }
    }

    /// Where traffic for a guest-visible address really goes on the host.
    fn host_ip(&self, dst: IpAddr) -> IpAddr {
        match dst {
            IpAddr::V4(a) if a.octets() == DNS_IP => self.dns_server,
            IpAddr::V6(a) if a.octets() == DNS_IP6 => self.dns_server,
            IpAddr::V4(a) if a.octets() == GATEWAY_IP => IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(a) if a.octets() == GATEWAY_IP6 => IpAddr::V6(Ipv6Addr::LOCALHOST),
            other => other,
        }
    }

    fn udp_from_guest(&mut self, guest: IpAddr, dst: IpAddr, datagram: &[u8]) {
        let Some((sport, dport)) = udp_ports(datagram) else { return };
        let payload = &datagram[8..];
        if self.system_dns && dns::handles(dst, dport) {
            if self.dns_datagrams.len() < MAX_PENDING_DNS {
                let query = payload.to_vec();
                let fallback = self.dns_fallback;
                let (tx, response) = channel();
                if thread::Builder::new().name("rvemu-dns".into()).spawn(move || {
                    let _ = tx.send(dns::resolve(&query, true, fallback));
                }).is_ok() {
                    self.dns_datagrams.push(DnsDatagram { guest, server: dst, port: sport, response });
                }
            }
            return;
        }
        let key = (sport, dst, dport);
        let target = SocketAddr::new(self.host_ip(dst), dport);
        if !self.udp.contains_key(&key) {
            match Self::open_udp(target) {
                Some(sock) => {
                    self.udp.insert(
                        key,
                        UdpFlow { sock, dst, dst_port: dport, guest_port: sport, guest, last_used: WallClock::now() },
                    );
                }
                None => return self.defer_udp(key, guest, target, payload),
            }
        }
        let sent = if host_send_blocked() {
            false
        } else {
            match self.udp.get_mut(&key) {
                Some(flow) => match flow.sock.send(payload) {
                    Ok(_) => {
                        flow.last_used = WallClock::now();
                        true
                    }
                    Err(_) => false,
                },
                None => false,
            }
        };
        if !sent {
            crate::counters::inc(&crate::counters::UDP_DEFERRED);
            self.defer_udp(key, guest, target, payload);
        }
    }

    /// A host UDP socket for `target`, or `None` when the host is not ready.
    fn open_udp(target: SocketAddr) -> Option<UdpSocket> {
        let bind = if target.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
        let sock = UdpSocket::bind(bind).ok()?;
        sock.connect(target).ok()?;
        sock.set_nonblocking(true).ok()?;
        Some(sock)
    }

    fn defer_udp(&mut self, key: (u16, IpAddr, u16), guest: IpAddr, target: SocketAddr, payload: &[u8]) {
        if self.pending_udp.len() >= PENDING_LIMIT {
            self.pending_udp.pop_front();
        }
        self.pending_udp.push_back(Pending {
            dst: (key.0, key.1),
            guest,
            target,
            payload: payload.to_vec(),
            queued: WallClock::now(),
        });
    }

    /// Retry the datagrams the host could not send. Called once per poll.
    fn retry_pending(&mut self) {
        crate::counters::inc(&crate::counters::NET_POLLS);
        if self.pending_udp.is_empty() && self.pending_icmp.is_empty() {
            return;
        }
        let mut still = VecDeque::new();
        while let Some(p) = self.pending_udp.pop_front() {
            if p.queued.elapsed() > PENDING_IDLE {
                continue;
            }
            let key = (p.dst.0, p.dst.1, p.target.port());
            if !self.udp.contains_key(&key) {
                match Self::open_udp(p.target) {
                    Some(sock) => {
                        self.udp.insert(
                            key,
                            UdpFlow {
                                sock,
                                dst: p.dst.1,
                                dst_port: p.target.port(),
                                guest_port: p.dst.0,
                                guest: p.guest,
                                last_used: WallClock::now(),
                            },
                        );
                    }
                    None => {
                        still.push_back(p);
                        continue;
                    }
                }
            }
            let sent = !host_send_blocked()
                && self
                .udp
                .get_mut(&key)
                .map(|flow| {
                    let ok = flow.sock.send(&p.payload).is_ok();
                    if ok {
                        flow.last_used = WallClock::now();
                    }
                    ok
                })
                .unwrap_or(false);
            if sent {
                crate::counters::inc(&crate::counters::UDP_RETRIED);
            } else {
                still.push_back(p);
            }
        }
        self.pending_udp = still;
        let mut still_icmp = VecDeque::new();
        while let Some(p) = self.pending_icmp.pop_front() {
            if p.queued.elapsed() > PENDING_IDLE {
                continue;
            }
            let guest_id = p.dst.0;
            let dst = p.dst.1;
            let key = (guest_id, dst);
            if !self.icmp.contains_key(&key) {
                match icmp::IcmpSocket::new(dst) {
                    Ok(sock) => {
                        self.icmp.insert(
                            key,
                            IcmpFlow { sock, guest_id, guest: p.guest, last_used: WallClock::now() },
                        );
                    }
                    Err(_) => {
                        still_icmp.push_back(p);
                        continue;
                    }
                }
            }
            let sent = !host_send_blocked()
                && self
                .icmp
                .get_mut(&key)
                .map(|flow| {
                    let ok = flow.sock.send(&p.payload).is_ok();
                    if ok {
                        flow.last_used = WallClock::now();
                    }
                    ok
                })
                .unwrap_or(false);
            if sent {
                crate::counters::inc(&crate::counters::ICMP_RETRIED);
            } else {
                still_icmp.push_back(p);
            }
        }
        self.pending_icmp = still_icmp;
    }

    /// Proxy an IPv4 echo request to the host. Other ICMP messages are dropped.
    fn icmp_from_guest(&mut self, guest: IpAddr, dst: IpAddr, message: &[u8]) {
        if message.len() < 8 || message[0] != 8 {
            return;
        }
        let guest_id = u16::from_be_bytes([message[4], message[5]]);
        let seq = u16::from_be_bytes([message[6], message[7]]);
        let key = (guest_id, dst);
        if let std::collections::hash_map::Entry::Vacant(slot) = self.icmp.entry(key) {
            match icmp::IcmpSocket::new(dst) {
                Ok(sock) => {
                    slot.insert(IcmpFlow { sock, guest_id, guest, last_used: WallClock::now() });
                }
                Err(_) => {
                    let payload = icmp::echo_request(guest_id, seq, &message[8..]);
                    return self.defer_icmp(key, guest, dst, &payload);
                }
            }
        }
        let request = icmp::echo_request(guest_id, seq, &message[8..]);
        let sent = self
            .icmp
            .get_mut(&key)
            .map(|flow| match flow.sock.send(&request) {
                Ok(_) => {
                    flow.last_used = WallClock::now();
                    true
                }
                Err(_) => false,
            })
            .unwrap_or(false);
        if !sent {
            self.defer_icmp(key, guest, dst, &request);
        }
    }

    fn defer_icmp(&mut self, key: (u16, IpAddr), guest: IpAddr, dst: IpAddr, payload: &[u8]) {
        if self.pending_icmp.len() >= PENDING_LIMIT {
            self.pending_icmp.pop_front();
        }
        self.pending_icmp.push_back(Pending {
            dst: (key.0, dst),
            guest,
            target: SocketAddr::new(dst, 0),
            payload: payload.to_vec(),
            queued: WallClock::now(),
        });
    }

    /// ICMPv6: neighbor discovery for the gateway and proxied echo requests.
    fn icmpv6_from_guest(&mut self, src: [u8; 16], dst: [u8; 16], message: &[u8], frame: &[u8]) {
        let Some(&kind) = message.first() else { return };
        match kind {
            ICMPV6_NEIGHBOR_SOLICIT => {
                let Some((target, source_mac)) = parse_neighbor_solicit(message) else { return };
                if target == GATEWAY_IP6 {
                    // smoltcp answers for the gateway and learns the guest's address.
                    self.link.rx.push_back(frame.to_vec());
                } else if target != GUEST_IP6 && target != GUEST_LL6 {
                    // Proxy neighbor discovery: every other address is "us".
                    let mac = source_mac.unwrap_or(GUEST_MAC);
                    let advert = neighbor_advert(target, GATEWAY_MAC, target, src);
                    let packet = ipv6_packet_hop(target, src, PROTO_ICMPV6, 255, &advert);
                    self.push_to_guest(eth_frame(mac, GATEWAY_MAC, ETHERTYPE_IPV6, &packet));
                }
            }
            ICMPV6_NEIGHBOR_ADVERT => {
                let Some(target) = neighbor_advert_target(message) else { return };
                if target == GATEWAY_IP6 || target == GUEST_IP6 || target == GUEST_LL6 {
                    self.link.rx.push_back(frame.to_vec());
                }
            }
            ICMPV6_ECHO_REQUEST if dst == GATEWAY_IP6 => self.link.rx.push_back(frame.to_vec()),
            ICMPV6_ECHO_REQUEST => self.icmp6_from_guest(src, dst, message),
            _ => {}
        }
    }

    /// Proxy an IPv6 echo request to the host.
    fn icmp6_from_guest(&mut self, src: [u8; 16], dst: [u8; 16], message: &[u8]) {
        if message.len() < 8 {
            return;
        }
        let guest_id = u16::from_be_bytes([message[4], message[5]]);
        let seq = u16::from_be_bytes([message[6], message[7]]);
        let key = (guest_id, ip6(dst));
        if let std::collections::hash_map::Entry::Vacant(slot) = self.icmp.entry(key) {
            let Ok(sock) = icmp::IcmpSocket::new(ip6(dst)) else { return };
            slot.insert(IcmpFlow { sock, guest_id, guest: ip6(src), last_used: WallClock::now() });
        }
        if let Some(flow) = self.icmp.get_mut(&key) {
            let request = icmp::echo_request6(guest_id, seq, &message[8..]);
            let _ = flow.sock.send(&request);
            flow.last_used = WallClock::now();
        }
    }

    fn tcp_from_guest(&mut self, guest: IpAddr, dest: IpAddr, segment: &[u8], frame: &[u8]) {
        let Ok(packet) = smoltcp::wire::TcpPacket::new_checked(segment) else { return };
        let key = TcpKey { guest, source: packet.src_port(), dest, port: packet.dst_port() };
        if self.tcp.contains_key(&key) {
            self.link.rx.push_back(frame.to_vec());
            return;
        }
        if !packet.syn() || packet.ack() || packet.rst() || self.connecting.contains_key(&key)
            || !packet.verify_checksum(&to_smol(guest), &to_smol(dest))
        {
            return;
        }
        let seq = (packet.seq_number().0 as u32).wrapping_add(packet.payload().len() as u32);
        if self.system_dns && dns::handles(dest, key.port) {
            let clients = self.tcp.values().filter(|c| matches!(c.host, TcpPeer::Dns(_))).count();
            if clients < MAX_PENDING_DNS {
                if let Ok((guest_end, resolver_end)) = UnixStream::pair() {
                    let fallback = self.dns_fallback;
                    if guest_end.set_nonblocking(true).is_ok()
                        && thread::Builder::new().name("rvemu-dns-tcp".into()).spawn(move || {
                            let _ = dns::serve_tcp(resolver_end, fallback);
                        }).is_ok()
                    {
                        self.accept_syn(key, TcpPeer::Dns(guest_end), frame.to_vec(), seq);
                        return;
                    }
                }
            }
            self.reject_syn(key, seq);
            return;
        }
        if self.connecting.len() >= MAX_PENDING_TCP {
            self.reject_syn(key, seq);
            return;
        }
        let target = SocketAddr::new(self.host_ip(dest), key.port);
        let (tx, result) = channel();
        let spawned = thread::Builder::new().name("rvemu-tcp-dial".into()).spawn(move || {
            let stream = host::connect(target, CONNECT_TIMEOUT).and_then(|stream| {
                stream.set_nonblocking(true)?;
                Ok(stream)
            });
            let _ = tx.send(stream);
        });
        if spawned.is_ok() {
            self.connecting.insert(key, Connecting { result, syn: frame.to_vec(), seq });
        } else {
            self.reject_syn(key, seq);
        }
    }

    fn reject_syn(&mut self, key: TcpKey, seq: u32) {
        let segment = tcp_reset(key.dest, key.guest, key.port, key.source, seq);
        if let Some(frame) = ip_frame(key.dest, key.guest, PROTO_TCP, &segment) {
            self.push_to_guest(frame);
        }
    }

    /// Pass the SYN to smoltcp after the host connects. Failed destinations
    /// fail at connect time in the guest, so clients can try another address.
    fn service_connecting(&mut self) {
        let mut ready = Vec::new();
        for (key, pending) in &self.connecting {
            match pending.result.try_recv() {
                Ok(result) => ready.push((*key, result.ok())),
                Err(TryRecvError::Disconnected) => ready.push((*key, None)),
                Err(TryRecvError::Empty) => {}
            }
        }
        for (key, result) in ready {
            let pending = self.connecting.remove(&key).unwrap();
            let Some(host) = result else {
                self.reject_syn(key, pending.seq);
                continue;
            };
            self.accept_syn(key, TcpPeer::Host(host), pending.syn, pending.seq);
        }
    }

    fn accept_syn(&mut self, key: TcpKey, host: TcpPeer, frame: Vec<u8>, seq: u32) {
        let rx = tcp::SocketBuffer::new(vec![0u8; TCP_BUFFER]);
        let tx = tcp::SocketBuffer::new(vec![0u8; TCP_BUFFER]);
        let mut sock = TcpSocket::new(rx, tx);
        if sock.listen(IpListenEndpoint { addr: Some(to_smol(key.dest)), port: key.port }).is_err() {
            self.reject_syn(key, seq);
            return;
        }
        let handle = self.sockets.add(sock);
        // Install and consume this SYN together. A second host connection to
        // the same destination must not hand its SYN to the wrong listener.
        self.link.rx.push_front(frame);
        let now = Instant::from_micros(self.clock.elapsed().as_micros() as i64);
        self.iface.poll_ingress_single(now, &mut self.link, &mut self.sockets);
        if self.sockets.get::<TcpSocket>(handle).state() == State::Listen {
            self.sockets.remove(handle);
            self.reject_syn(key, seq);
            return;
        }
        self.tcp.insert(key, TcpConn { handle, host, host_eof: false, host_shut: false, handshake: Some(WallClock::now()) });
    }

    fn push_to_guest(&mut self, frame: Vec<u8>) {
        self.frames_to_guest += 1;
        self.to_guest.push_back(frame);
    }

    /// Advance TCP and UDP state and collect frames for the guest.
    pub fn poll(&mut self) {
        let now = Instant::from_micros(self.clock.elapsed().as_micros() as i64);
        self.retry_pending();
        self.accept_forwards();
        self.service_connecting();
        self.iface.poll(now, &mut self.link, &mut self.sockets);
        self.service_tcp();
        self.service_dns();
        self.service_udp();
        self.service_icmp();
        // Flush anything the host-side shuttling made smoltcp send, including
        // resets for connections that were just aborted.
        self.iface.poll(now, &mut self.link, &mut self.sockets);
        for handle in self.retired.drain(..) {
            self.sockets.remove(handle);
        }
        while let Some(frame) = self.link.tx.pop_front() {
            self.push_to_guest(frame);
        }
    }

    fn service_tcp(&mut self) {
        let mut dead = Vec::new();
        let scratch = &mut self.tcp_read_buffer;
        for (key, conn) in self.tcp.iter_mut() {
            let sock = self.sockets.get_mut::<TcpSocket>(conn.handle);
            if matches!(sock.state(), State::Listen | State::SynReceived | State::SynSent) {
                if conn.handshake.is_some_and(|start| start.elapsed() >= CONNECT_TIMEOUT) {
                    sock.abort();
                    dead.push(*key);
                }
                continue;
            }
            conn.handshake = None;
            let host = &mut conn.host;
            // Only consume bytes the host accepted. Keeping the rest in the
            // TCP receive window bounds memory and propagates backpressure.
            if sock.can_recv() {
                let sent = sock.recv(|buf| {
                    let result = host.write(buf);
                    (result.as_ref().copied().unwrap_or(0), result)
                });
                if let Ok(Err(e)) = sent {
                    if !matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) {
                        sock.abort();
                        dead.push(*key);
                        continue;
                    }
                }
            }

            // Host to guest.
            if !conn.host_eof && sock.can_send() {
                let room = (sock.send_capacity() - sock.send_queue()).min(scratch.len());
                match host.read(&mut scratch[..room]) {
                    Ok(0) => conn.host_eof = true,
                    Ok(n) => {
                        let _ = sock.send_slice(&scratch[..n]);
                    }
                    Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => {}
                    Err(_) => {
                        sock.abort();
                        dead.push(*key);
                        continue;
                    }
                }
            }
            if conn.host_eof && sock.may_send() {
                sock.close();
            }
            // The guest finished sending: half-close the host side too.
            if matches!(sock.state(), State::CloseWait | State::Closing | State::LastAck | State::TimeWait)
                && sock.recv_queue() == 0 && !conn.host_shut
            {
                let _ = host.shutdown_write();
                conn.host_shut = true;
            }
            if sock.state() == State::Closed {
                dead.push(*key);
            }
        }
        for key in dead {
            if let Some(conn) = self.tcp.remove(&key) {
                self.retired.push(conn.handle);
            }
        }
    }

    fn service_dns(&mut self) {
        let mut i = 0;
        while i < self.dns_datagrams.len() {
            let request = &self.dns_datagrams[i];
            match request.response.try_recv() {
                Ok(Some(answer)) => {
                    let datagram = udp_payload(request.server, request.guest, 53, request.port, &answer);
                    let frame = ip_frame(request.server, request.guest, PROTO_UDP, &datagram);
                    self.dns_datagrams.swap_remove(i);
                    if let Some(frame) = frame {
                        self.push_to_guest(frame);
                    }
                }
                Ok(None) | Err(TryRecvError::Disconnected) => { self.dns_datagrams.swap_remove(i); }
                Err(TryRecvError::Empty) => i += 1,
            }
        }
    }

    fn service_udp(&mut self) {
        let mut buf = [0u8; 2048];
        let mut dead = Vec::new();
        let mut replies = Vec::new();
        for (key, flow) in self.udp.iter_mut() {
            while let Ok(n) = flow.sock.recv(&mut buf) {
                let datagram = udp_payload(flow.dst, flow.guest, flow.dst_port, flow.guest_port, &buf[..n]);
                replies.push(ip_frame(flow.dst, flow.guest, PROTO_UDP, &datagram));
                flow.last_used = WallClock::now();
            }
            if flow.last_used.elapsed() > UDP_IDLE {
                dead.push(*key);
            }
        }
        for frame in replies.into_iter().flatten() {
            self.push_to_guest(frame);
        }
        for key in dead {
            self.udp.remove(&key);
        }
    }

    fn service_icmp(&mut self) {
        let mut buf = [0u8; 2048];
        let mut replies = Vec::new();
        let mut dead = Vec::new();
        for (key, flow) in self.icmp.iter_mut() {
            while let Some(n) = flow.sock.recv(&mut buf) {
                let message = match key.1 {
                    IpAddr::V4(_) => icmp::guest_reply(&buf[..n], flow.guest_id),
                    IpAddr::V6(_) => icmp::guest_reply6(&buf[..n], flow.guest_id),
                };
                if let Some(message) = message {
                    let message = echo_reply_message(key.1, flow.guest, message);
                    let proto = if key.1.is_ipv4() { PROTO_ICMP } else { PROTO_ICMPV6 };
                    replies.push(ip_frame(key.1, flow.guest, proto, &message));
                    flow.last_used = WallClock::now();
                }
            }
            if flow.last_used.elapsed() > ICMP_IDLE {
                dead.push(*key);
            }
        }
        for frame in replies.into_iter().flatten() {
            self.push_to_guest(frame);
        }
        for key in dead {
            self.icmp.remove(&key);
        }
    }

    /// Frames for the guest, in order.
    pub fn drain_to_guest(&mut self) -> Vec<Vec<u8>> {
        self.to_guest.drain(..).collect()
    }
}

/// IPv4 resolver override exposed to embedders, in network byte order.
pub static DNS_OVERRIDE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static IPV6_DNS_OVERRIDE: std::sync::Mutex<Option<Ipv6Addr>> = std::sync::Mutex::new(None);

/// Use `server` for guest DNS instead of the host's resolver configuration.
pub fn set_dns_override(server: impl Into<IpAddr>) {
    let mut v6 = IPV6_DNS_OVERRIDE.lock().unwrap_or_else(|err| err.into_inner());
    let v4 = match server.into() {
        IpAddr::V4(ip) => { *v6 = None; u32::from(ip).to_be() }
        IpAddr::V6(ip) => { *v6 = Some(ip); 0 }
    };
    DNS_OVERRIDE.store(v4, std::sync::atomic::Ordering::Relaxed);
}

fn forced_dns_server() -> Option<IpAddr> {
    let v6 = IPV6_DNS_OVERRIDE.lock().unwrap_or_else(|err| err.into_inner());
    let forced = DNS_OVERRIDE.load(std::sync::atomic::Ordering::Relaxed);
    if forced != 0 {
        return Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(forced))));
    }
    v6.map(IpAddr::V6)
}

fn configured_dns_server() -> Option<IpAddr> {
    std::fs::read_to_string("/etc/resolv.conf")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let mut words = line.split_whitespace();
                (words.next()? == "nameserver").then(|| words.next()?.parse().ok()).flatten()
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration as StdTimeout;

    /// `expose` is process-wide and every gateway looks at it when it polls, so
    /// the tests that poll a gateway take turns.
    static GATEWAY_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A guest UDP datagram to `dst_port` on 127.0.0.1, as the guest would send
    /// it. The gateway passes addresses other than its own through to the host,
    /// so a local listener can stand in for a name server.
    fn guest_udp_frame(dst_port: u16, sport: u16, payload: &[u8]) -> Vec<u8> {
        let dst = [127u8, 0, 0, 1];
        let udp = udp_payload(ip4(GUEST_IP), ip4(dst), sport, dst_port, payload);
        let packet = crate::net::proto::ipv4_packet(GUEST_IP, dst, PROTO_UDP, &udp);
        eth_frame(GATEWAY_MAC, [0x02, 0, 0, 0, 0, 1], ETHERTYPE_IPV4, &packet)
    }

    /// The phone comes up before its network does; a query sent in that window
    /// must not be lost, or the guest pays a whole resolver timeout for it.
    #[test]
    fn unsent_datagrams_are_retried_not_dropped() {
        let _turn = GATEWAY_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let mut gw = Gateway::new(&[], false).expect("gateway");
        let listener = UdpSocket::bind("127.0.0.1:0").expect("listener");
        let port = listener.local_addr().unwrap().port();
        listener.set_read_timeout(Some(StdTimeout::from_secs(2))).unwrap();
        let frame = guest_udp_frame(port, 40000, b"query");

        FAIL_SENDS.store(true, std::sync::atomic::Ordering::Relaxed);
        gw.from_guest(&frame);
        assert_eq!(gw.pending_udp.len(), 1, "the datagram must be kept");
        gw.poll();
        assert_eq!(gw.pending_udp.len(), 1, "still no route, still kept");
        assert!(listener.recv(&mut [0u8; 16]).is_err(), "nothing should arrive yet");

        FAIL_SENDS.store(false, std::sync::atomic::Ordering::Relaxed);
        gw.poll();
        assert!(gw.pending_udp.is_empty(), "sent, nothing left to retry");
        let mut buf = [0u8; 64];
        let n = listener.recv(&mut buf).expect("the query arrives after the retry");
        assert_eq!(&buf[..n], b"query");

        // A datagram the guest sends while the host is ready goes out at once.
        let frame = guest_udp_frame(port, 40001, b"second");
        gw.from_guest(&frame);
        assert!(gw.pending_udp.is_empty());
        let n = listener.recv(&mut buf).expect("second query");
        assert_eq!(&buf[..n], b"second");
    }

    /// A port opened with `expose` reaches the guest, asking again for the same
    /// port is harmless, and `unexpose` closes it.
    #[test]
    fn exposed_port_reaches_the_guest_until_closed() {
        let _turn = GATEWAY_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mut gw = Gateway::new(&[], false).expect("gateway");

        expose(Ipv4Addr::LOCALHOST, port, 22).expect("bind");
        let _client = TcpStream::connect(("127.0.0.1", port)).expect("connect to the exposed port");
        for _ in 0..200 {
            gw.poll();
            if !gw.tcp.is_empty() {
                break;
            }
            std::thread::sleep(StdTimeout::from_millis(5));
        }
        assert_eq!(gw.tcp.len(), 1, "the connection must be opened into the guest");

        // The same request again is a no-op even though the gateway owns the listener now.
        expose(Ipv4Addr::LOCALHOST, port, 22).expect("same arguments");
        gw.poll();
        assert!(TcpStream::connect(("127.0.0.1", port)).is_ok(), "still open");

        unexpose();
        gw.poll();
        assert!(TcpStream::connect(("127.0.0.1", port)).is_err(), "closed after unexpose");
    }
}
