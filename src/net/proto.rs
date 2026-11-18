//! Minimal Ethernet, ARP, IPv4, IPv6, UDP, ICMPv6 and neighbor discovery
//! helpers for the gateway.

pub const ETH_HDR: usize = 14;
pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_ARP: u16 = 0x0806;
pub const ETHERTYPE_IPV6: u16 = 0x86DD;
pub const PROTO_ICMP: u8 = 1;
pub const PROTO_TCP: u8 = 6;
pub const PROTO_UDP: u8 = 17;
pub const PROTO_ICMPV6: u8 = 58;
pub const ICMPV6_ECHO_REQUEST: u8 = 128;
pub const ICMPV6_NEIGHBOR_SOLICIT: u8 = 135;
pub const ICMPV6_NEIGHBOR_ADVERT: u8 = 136;
pub const BROADCAST_MAC: [u8; 6] = [0xff; 6];

/// One's-complement sum over `data`, folded to 16 bits and complemented.
pub fn checksum(data: &[u8], init: u32) -> u16 {
    let mut sum = init;
    let mut chunks = data.chunks_exact(2);
    for c in &mut chunks {
        sum += u16::from_be_bytes([c[0], c[1]]) as u32;
    }
    if let [last] = chunks.remainder() {
        sum += (*last as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Parsed Ethernet header and payload.
pub struct Eth<'a> {
    pub dst: [u8; 6],
    pub ethertype: u16,
    pub payload: &'a [u8],
}

pub fn parse_eth(frame: &[u8]) -> Option<Eth<'_>> {
    if frame.len() < ETH_HDR {
        return None;
    }
    Some(Eth {
        dst: frame[0..6].try_into().ok()?,
        ethertype: u16::from_be_bytes([frame[12], frame[13]]),
        payload: &frame[ETH_HDR..],
    })
}

pub fn eth_frame(dst: [u8; 6], src: [u8; 6], ethertype: u16, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(ETH_HDR + payload.len());
    f.extend_from_slice(&dst);
    f.extend_from_slice(&src);
    f.extend_from_slice(&ethertype.to_be_bytes());
    f.extend_from_slice(payload);
    f
}

/// Parsed IPv4 packet (header fields we need, plus the payload).
pub struct Ipv4<'a> {
    pub proto: u8,
    pub src: [u8; 4],
    pub dst: [u8; 4],
    pub payload: &'a [u8],
}

pub fn parse_ipv4(p: &[u8]) -> Option<Ipv4<'_>> {
    if p.len() < 20 || p[0] >> 4 != 4 {
        return None;
    }
    let ihl = (p[0] & 0x0f) as usize * 4;
    let total = u16::from_be_bytes([p[2], p[3]]) as usize;
    if ihl < 20 || total < ihl || p.len() < total {
        return None;
    }
    Some(Ipv4 {
        proto: p[9],
        src: p[12..16].try_into().ok()?,
        dst: p[16..20].try_into().ok()?,
        payload: &p[ihl..total],
    })
}

pub fn ipv4_packet(src: [u8; 4], dst: [u8; 4], proto: u8, payload: &[u8]) -> Vec<u8> {
    let total = 20 + payload.len();
    let mut h = vec![0u8; 20];
    h[0] = 0x45;
    h[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    h[6..8].copy_from_slice(&0x4000u16.to_be_bytes()); // don't fragment
    h[8] = 64; // ttl
    h[9] = proto;
    h[12..16].copy_from_slice(&src);
    h[16..20].copy_from_slice(&dst);
    let c = checksum(&h, 0);
    h[10..12].copy_from_slice(&c.to_be_bytes());
    h.extend_from_slice(payload);
    h
}

/// Parsed IPv6 packet. Extension headers are not followed: `next` is the
/// header that directly follows the fixed 40-byte header.
pub struct Ipv6<'a> {
    pub next: u8,
    pub src: [u8; 16],
    pub dst: [u8; 16],
    pub payload: &'a [u8],
}

pub fn parse_ipv6(p: &[u8]) -> Option<Ipv6<'_>> {
    if p.len() < 40 || p[0] >> 4 != 6 {
        return None;
    }
    let len = u16::from_be_bytes([p[4], p[5]]) as usize;
    if p.len() < 40 + len {
        return None;
    }
    Some(Ipv6 {
        next: p[6],
        src: p[8..24].try_into().ok()?,
        dst: p[24..40].try_into().ok()?,
        payload: &p[40..40 + len],
    })
}

pub fn ipv6_packet(src: [u8; 16], dst: [u8; 16], next: u8, payload: &[u8]) -> Vec<u8> {
    ipv6_packet_hop(src, dst, next, 64, payload)
}

/// An IPv6 packet with an explicit hop limit. Neighbor discovery needs 255.
pub fn ipv6_packet_hop(src: [u8; 16], dst: [u8; 16], next: u8, hop_limit: u8, payload: &[u8]) -> Vec<u8> {
    let mut h = Vec::with_capacity(40 + payload.len());
    h.extend_from_slice(&[0x60, 0, 0, 0]);
    h.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    h.push(next);
    h.push(hop_limit);
    h.extend_from_slice(&src);
    h.extend_from_slice(&dst);
    h.extend_from_slice(payload);
    h
}

/// Sum of the IPv6 pseudo-header, for checksums of upper-layer messages.
fn pseudo_header_sum6(src: [u8; 16], dst: [u8; 16], next: u8, len: u32) -> u32 {
    let mut s = 0u32;
    for a in [src, dst] {
        for c in a.chunks_exact(2) {
            s += u16::from_be_bytes([c[0], c[1]]) as u32;
        }
    }
    s + (len >> 16) + (len & 0xffff) + next as u32
}

/// Fill in the checksum of an ICMPv6 message from `src` to `dst`.
pub fn icmpv6_message(src: [u8; 16], dst: [u8; 16], mut message: Vec<u8>) -> Vec<u8> {
    message[2..4].copy_from_slice(&[0, 0]);
    let len = message.len() as u32;
    let c = checksum(&message, pseudo_header_sum6(src, dst, PROTO_ICMPV6, len));
    message[2..4].copy_from_slice(&c.to_be_bytes());
    message
}

/// UDP datagram with the checksum over the IPv6 pseudo-header, which IPv6 requires.
pub fn udp_datagram6(src: [u8; 16], dst: [u8; 16], sport: u16, dport: u16, payload: &[u8]) -> Vec<u8> {
    let len = 8 + payload.len();
    let mut d = Vec::with_capacity(len);
    d.extend_from_slice(&sport.to_be_bytes());
    d.extend_from_slice(&dport.to_be_bytes());
    d.extend_from_slice(&(len as u16).to_be_bytes());
    d.extend_from_slice(&[0, 0]);
    d.extend_from_slice(payload);
    let mut c = checksum(&d, pseudo_header_sum6(src, dst, PROTO_UDP, len as u32));
    if c == 0 {
        c = 0xffff;
    }
    d[6..8].copy_from_slice(&c.to_be_bytes());
    d
}

/// UDP datagram with a correct checksum over the IPv4 pseudo-header.
pub fn udp_datagram(src: [u8; 4], dst: [u8; 4], sport: u16, dport: u16, payload: &[u8]) -> Vec<u8> {
    let len = 8 + payload.len();
    let mut d = Vec::with_capacity(len);
    d.extend_from_slice(&sport.to_be_bytes());
    d.extend_from_slice(&dport.to_be_bytes());
    d.extend_from_slice(&(len as u16).to_be_bytes());
    d.extend_from_slice(&[0, 0]);
    d.extend_from_slice(payload);
    let pseudo = pseudo_header_sum(src, dst, PROTO_UDP, len as u16);
    let mut c = checksum(&d, pseudo);
    if c == 0 {
        c = 0xffff;
    }
    d[6..8].copy_from_slice(&c.to_be_bytes());
    d
}

fn pseudo_header_sum(src: [u8; 4], dst: [u8; 4], proto: u8, len: u16) -> u32 {
    let mut s = 0u32;
    for w in [src, dst].iter().flat_map(|a| [u16::from_be_bytes([a[0], a[1]]), u16::from_be_bytes([a[2], a[3]])]) {
        s += w as u32;
    }
    s + proto as u32 + len as u32
}

/// Reject a SYN whose upstream connect failed, with its sequence acknowledged.
pub(super) fn tcp_reset(
    src: std::net::IpAddr, dst: std::net::IpAddr, source: u16, dest: u16, seq: u32,
) -> [u8; 20] {
    use std::net::IpAddr;
    let mut segment = [0; 20];
    segment[..2].copy_from_slice(&source.to_be_bytes());
    segment[2..4].copy_from_slice(&dest.to_be_bytes());
    segment[8..12].copy_from_slice(&seq.wrapping_add(1).to_be_bytes());
    segment[12] = 5 << 4;
    segment[13] = 0x14; // RST | ACK
    let sum = match (src, dst) {
        (IpAddr::V4(a), IpAddr::V4(b)) => pseudo_header_sum(a.octets(), b.octets(), PROTO_TCP, 20),
        (IpAddr::V6(a), IpAddr::V6(b)) => pseudo_header_sum6(a.octets(), b.octets(), PROTO_TCP, 20),
        _ => unreachable!("TCP flow changed address family"),
    };
    let c = checksum(&segment, sum);
    segment[16..18].copy_from_slice(&c.to_be_bytes());
    segment
}

/// Source and destination ports of a UDP datagram.
pub fn udp_ports(d: &[u8]) -> Option<(u16, u16)> {
    (d.len() >= 8).then(|| (u16::from_be_bytes([d[0], d[1]]), u16::from_be_bytes([d[2], d[3]])))
}

/// The target and link-layer address of a neighbor solicitation message.
pub fn parse_neighbor_solicit(message: &[u8]) -> Option<([u8; 16], Option<[u8; 6]>)> {
    if message.len() < 24 || message[0] != ICMPV6_NEIGHBOR_SOLICIT {
        return None;
    }
    let target: [u8; 16] = message[8..24].try_into().ok()?;
    let mut source = None;
    let mut options = &message[24..];
    while options.len() >= 8 {
        let len = options[1] as usize * 8;
        if len == 0 || len > options.len() {
            break;
        }
        if options[0] == 1 && len >= 8 {
            source = options[2..8].try_into().ok();
        }
        options = &options[len..];
    }
    Some((target, source))
}

/// The target address of a neighbor advertisement message.
pub fn neighbor_advert_target(message: &[u8]) -> Option<[u8; 16]> {
    (message.len() >= 24 && message[0] == ICMPV6_NEIGHBOR_ADVERT).then(|| message[8..24].try_into().ok()).flatten()
}

/// A neighbor advertisement for `target`, with the link-layer address `mac`.
/// The result is an ICMPv6 message whose checksum is set for `src` to `dst`.
pub fn neighbor_advert(target: [u8; 16], mac: [u8; 6], src: [u8; 16], dst: [u8; 16]) -> Vec<u8> {
    let mut m = vec![ICMPV6_NEIGHBOR_ADVERT, 0, 0, 0];
    m.extend_from_slice(&[0x60, 0, 0, 0]); // solicited and override
    m.extend_from_slice(&target);
    m.extend_from_slice(&[2, 1]); // target link-layer address option
    m.extend_from_slice(&mac);
    icmpv6_message(src, dst, m)
}

/// ARP over Ethernet/IPv4.
pub struct Arp {
    pub op: u16,
    pub sender_mac: [u8; 6],
    pub sender_ip: [u8; 4],
    pub target_ip: [u8; 4],
}

pub fn parse_arp(p: &[u8]) -> Option<Arp> {
    if p.len() < 28 || p[..6] != [0, 1, 8, 0, 6, 4] {
        return None;
    }
    Some(Arp {
        op: u16::from_be_bytes([p[6], p[7]]),
        sender_mac: p[8..14].try_into().ok()?,
        sender_ip: p[14..18].try_into().ok()?,
        target_ip: p[24..28].try_into().ok()?,
    })
}

pub fn arp_reply(my_mac: [u8; 6], my_ip: [u8; 4], to: &Arp) -> Vec<u8> {
    let mut a = vec![0, 1, 8, 0, 6, 4, 0, 2];
    a.extend_from_slice(&my_mac);
    a.extend_from_slice(&my_ip);
    a.extend_from_slice(&to.sender_mac);
    a.extend_from_slice(&to.sender_ip);
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_header_checksum_verifies() {
        let pkt = ipv4_packet([10, 0, 2, 15], [93, 184, 216, 34], PROTO_TCP, b"payload");
        assert_eq!(pkt.len(), 20 + 7);
        // Summing a valid header including its checksum field gives zero.
        assert_eq!(checksum(&pkt[..20], 0), 0);
        let parsed = parse_ipv4(&pkt).expect("valid packet");
        assert_eq!(parsed.src, [10, 0, 2, 15]);
        assert_eq!(parsed.dst, [93, 184, 216, 34]);
        assert_eq!(parsed.payload, b"payload");
    }

    #[test]
    fn udp_checksum_covers_pseudo_header() {
        let src = [10, 0, 2, 3];
        let dst = [10, 0, 2, 15];
        let d = udp_datagram(src, dst, 53, 40000, b"dns reply");
        assert_eq!(udp_ports(&d), Some((53, 40000)));
        let pseudo = pseudo_header_sum(src, dst, PROTO_UDP, d.len() as u16);
        assert_eq!(checksum(&d, pseudo), 0);
    }

    fn v6(last: u8) -> [u8; 16] {
        let mut a = [0u8; 16];
        a[0] = 0xfd;
        a[3] = 2;
        a[15] = last;
        a
    }

    #[test]
    fn ipv6_packet_roundtrip_and_udp_checksum() {
        let (src, dst) = (v6(0x15), v6(0x3));
        let d = udp_datagram6(src, dst, 40000, 53, b"query");
        // A correct datagram sums to zero over the pseudo-header.
        assert_eq!(checksum(&d, pseudo_header_sum6(src, dst, PROTO_UDP, d.len() as u32)), 0);
        let pkt = ipv6_packet(src, dst, PROTO_UDP, &d);
        let parsed = parse_ipv6(&pkt).expect("valid packet");
        assert_eq!((parsed.next, parsed.src, parsed.dst), (PROTO_UDP, src, dst));
        assert_eq!(parsed.payload, &d[..]);
    }

    #[test]
    fn icmpv6_checksum_and_neighbor_discovery() {
        let (guest, gateway) = (v6(0x15), v6(0x2));
        // A neighbor solicitation for the gateway, with the guest's link-layer address.
        let mut ns = vec![ICMPV6_NEIGHBOR_SOLICIT, 0, 0, 0, 0, 0, 0, 0];
        ns.extend_from_slice(&gateway);
        ns.extend_from_slice(&[1, 1, 0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
        let ns = icmpv6_message(guest, v6(0xff), ns);
        assert_eq!(checksum(&ns, pseudo_header_sum6(guest, v6(0xff), PROTO_ICMPV6, ns.len() as u32)), 0);
        let (target, mac) = parse_neighbor_solicit(&ns).expect("solicitation");
        assert_eq!(target, gateway);
        assert_eq!(mac, Some([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]));

        let na = neighbor_advert(target, [1, 2, 3, 4, 5, 6], gateway, guest);
        assert_eq!(na[0], ICMPV6_NEIGHBOR_ADVERT);
        assert_eq!(neighbor_advert_target(&na), Some(gateway));
        assert_eq!(checksum(&na, pseudo_header_sum6(gateway, guest, PROTO_ICMPV6, na.len() as u32)), 0);
        // Hosts drop neighbor discovery packets whose hop limit is not 255.
        let pkt = ipv6_packet_hop(gateway, guest, PROTO_ICMPV6, 255, &na);
        assert_eq!(pkt[7], 255);
    }

    #[test]
    fn ethernet_roundtrip_and_arp_reply() {
        let request = Arp {
            op: 1,
            sender_mac: [1, 2, 3, 4, 5, 6],
            sender_ip: [10, 0, 2, 15],
            target_ip: [10, 0, 2, 3],
        };
        let mut req_bytes = vec![0, 1, 8, 0, 6, 4, 0, 1];
        req_bytes.extend_from_slice(&request.sender_mac);
        req_bytes.extend_from_slice(&request.sender_ip);
        req_bytes.extend_from_slice(&[0; 6]);
        req_bytes.extend_from_slice(&request.target_ip);
        let parsed = parse_arp(&req_bytes).expect("valid ARP");
        assert_eq!((parsed.op, parsed.target_ip), (1, [10, 0, 2, 3]));

        let reply = arp_reply([0x52, 0x55, 0, 0, 2, 2], [10, 0, 2, 3], &parsed);
        assert_eq!(&reply[6..8], &[0, 2]); // operation: reply
        assert_eq!(&reply[14..18], &[10, 0, 2, 3]); // sender IP is the address answered for
        let frame = eth_frame([1, 2, 3, 4, 5, 6], [9; 6], ETHERTYPE_ARP, &reply);
        let eth = parse_eth(&frame).expect("valid frame");
        assert_eq!((eth.dst, eth.ethertype), ([1, 2, 3, 4, 5, 6], ETHERTYPE_ARP));
        assert_eq!(eth.payload, &reply[..]);
    }
}
