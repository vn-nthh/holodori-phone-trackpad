//! Reversible Linux routing policy for the opt-in local-only tether mode.
//!
//! See the parent module doc comment for the feature-level description. This
//! file implements it with raw `AF_NETLINK`/`NETLINK_ROUTE` requests built by
//! hand with `libc`, deliberately without a netlink crate: `rtnetlink` pulls
//! in `tokio`, `neli` is a large dependency for a handful of messages, and
//! this project's style elsewhere is minimal-dependency raw FFI (see
//! `platform.rs`'s `libc::sigaction` signal handling). `libc` is already a
//! `cfg(target_os = "linux")` dependency.
//!
//! # SAFETY
//!
//! This code deletes routes from the *live* routing table of whatever
//! machine runs it. A misidentified interface (a real Ethernet/Wi-Fi
//! adapter, a VPN tunnel, a container bridge, ...) would cut that machine's
//! internet. Two independent guards exist to prevent that:
//!
//! 1. [`is_tether_interface`] only ever returns `true` for an interface
//!    backed by one of a fixed, small set of driver names *and* confirmed to
//!    be a USB device (see its doc comment). There is no fallback path that
//!    widens this match. However, that driver list is **not** exclusively
//!    USB-tethering drivers: `cdc_ether`, `cdc_ncm`, and `cdc_mbim` are
//!    generic USB networking *class* drivers built into the kernel for any
//!    device that advertises a CDC-ECM/CDC-NCM/CDC-MBIM USB interface, not
//!    just Android/iOS tethering. They also bind to USB-C dock/hub Ethernet
//!    adapters, standalone USB Ethernet dongles, and similar accessories.
//!    The second guard (USB bus membership) does not exclude those either —
//!    a dock or dongle is a USB device too. Residual risk: on a machine
//!    whose *real* uplink is a USB-C dock or USB Ethernet dongle using one of
//!    these drivers, this module would misidentify it as a phone tether and
//!    remove its default route. `rndis_host` and `ipheth` do not have this
//!    problem (they are Android/iOS-tethering-specific), but `cdc_ether`/
//!    `cdc_ncm`/`cdc_mbim` do. TODO: before this could be enabled by
//!    default, narrow the match beyond driver name + USB bus — e.g. checking
//!    the USB interface class/subclass/protocol against the values Android
//!    tethering actually presents, or the device's USB vendor/product id
//!    range, so a docked/donged Ethernet NIC is not misclassified.
//! 2. Every route this module deletes or recreates must pass
//!    [`dump_default_routes`]'s filter: `rtm_dst_len == 0` (default route
//!    only), `RTA_OIF` equal to a verified tether interface index, and the
//!    route's table (`rtm_table`, or the `RTA_TABLE` attribute when present)
//!    equal to `RT_TABLE_MAIN`. Nothing outside that filter is ever touched.
//!
//! Every route this module deletes is first fully captured (family, all
//! `rtmsg` fields, and the `RTA_DST`/`RTA_OIF`/`RTA_GATEWAY`/`RTA_PREFSRC`/
//! `RTA_PRIORITY`/`RTA_TABLE` attributes) so it can be re-encoded byte-for-
//! byte on restore.
//!
//! # Header constants
//!
//! The alignment/length macros below were checked against this machine's
//! `/usr/include/linux/netlink.h` and `/usr/include/linux/rtnetlink.h`:
//!
//! ```text
//! netlink.h:  #define NLMSG_ALIGNTO 4U
//!             #define NLMSG_ALIGN(len) (((len)+NLMSG_ALIGNTO-1) & ~(NLMSG_ALIGNTO-1))
//!             #define NLMSG_HDRLEN  ((int) NLMSG_ALIGN(sizeof(struct nlmsghdr)))
//!             #define NLMSG_LENGTH(len) ((len) + NLMSG_HDRLEN)
//!             struct nlmsghdr { __u32 nlmsg_len; __u16 nlmsg_type; __u16 nlmsg_flags;
//!                                __u32 nlmsg_seq; __u32 nlmsg_pid; };   // 16 bytes, already aligned
//!             struct nlmsgerr { int error; struct nlmsghdr msg; };
//!
//! rtnetlink.h: #define RTA_ALIGNTO 4U
//!              #define RTA_ALIGN(len) (((len)+RTA_ALIGNTO-1) & ~(RTA_ALIGNTO-1))
//!              #define RTA_LENGTH(len) (RTA_ALIGN(sizeof(struct rtattr)) + (len))  // == 4 + len
//!              struct rtattr { unsigned short rta_len; unsigned short rta_type; }; // 4 bytes
//!              struct rtmsg { unsigned char rtm_family, rtm_dst_len, rtm_src_len, rtm_tos,
//!                                            rtm_table, rtm_protocol, rtm_scope, rtm_type;
//!                              unsigned rtm_flags; };                              // 12 bytes
//!              RT_TABLE_MAIN = 254
//! ```
//!
//! `libc` 0.2.189 (this workspace's locked version) exposes `nlmsghdr`,
//! `rtattr`, `sockaddr_nl`, the `NETLINK_*`/`NLM_F_*`/`NLMSG_*`/`RTM_*`/
//! `RTA_*`/`RT_TABLE_MAIN` constants, but *not* `rtmsg` (only its Android
//! target module has that struct as of this version) or `nlmsgerr`'s layout
//! as a usable public type here. Rather than depend on that gap closing,
//! every netlink message in this file is built and parsed as raw bytes using
//! the constants above, which is also what makes the encode/parse round trip
//! straightforward to unit test without a socket.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::mem::{size_of, zeroed};
use std::net::{IpAddr, SocketAddr};
use std::path::Path;

// ---------------------------------------------------------------------------
// Netlink message framing
// ---------------------------------------------------------------------------

/// `NLMSG_HDRLEN`: size of `struct nlmsghdr`, already a multiple of 4.
const NLMSG_HDRLEN: usize = 16;
/// `RTA_ALIGN(sizeof(struct rtattr))`: size of the attribute header, already
/// a multiple of 4.
const RTA_HDRLEN: usize = 4;
/// Size of the fixed part of `struct rtmsg` (8 `unsigned char` fields plus
/// one `unsigned` flags field).
const RTM_HDRLEN: usize = 12;

/// `NLMSG_ALIGN(len)`.
const fn nlmsg_align(length: usize) -> usize {
    (length + 3) & !3
}

/// `RTA_ALIGN(len)`.
const fn rta_align(length: usize) -> usize {
    (length + 3) & !3
}

/// `RTA_LENGTH(len)` = `RTA_ALIGN(sizeof(struct rtattr)) + len` = `4 + len`.
const fn rta_length(payload_len: usize) -> usize {
    rta_align(RTA_HDRLEN) + payload_len
}

fn read_u32(bytes: &[u8]) -> Option<u32> {
    <[u8; 4]>::try_from(bytes).ok().map(u32::from_ne_bytes)
}

/// Appends one `rtattr` (header + value + alignment padding) to `buffer`.
fn push_attribute(buffer: &mut Vec<u8>, attribute_type: u16, value: &[u8]) {
    let start = buffer.len();
    let unaligned_len = rta_length(value.len());
    buffer.extend_from_slice(&(unaligned_len as u16).to_ne_bytes());
    buffer.extend_from_slice(&attribute_type.to_ne_bytes());
    buffer.extend_from_slice(value);
    buffer.resize(start + rta_align(unaligned_len), 0);
}

/// A captured route, holding every `rtmsg` field plus the subset of
/// attributes needed to recreate it exactly (`RTA_OIF`, `RTA_GATEWAY`,
/// `RTA_PREFSRC`, `RTA_PRIORITY`, `RTA_TABLE`, `RTA_DST`). Keyed by
/// attribute type in a `BTreeMap` so two captures of the same route compare
/// equal regardless of the order the kernel happened to emit attributes in,
/// and so encoding is deterministic (useful for the round-trip test).
#[derive(Clone, Debug, PartialEq, Eq)]
struct CapturedRoute {
    family: u8,
    dst_len: u8,
    src_len: u8,
    tos: u8,
    table: u8,
    protocol: u8,
    scope: u8,
    rtm_type: u8,
    flags: u32,
    attributes: BTreeMap<u16, Vec<u8>>,
}

impl CapturedRoute {
    fn oif(&self) -> Option<u32> {
        self.attributes
            .get(&libc::RTA_OIF)
            .and_then(|value| read_u32(value))
    }

    /// The route's real table id. `RTA_TABLE` carries the id when it does
    /// not fit in the one-byte `rtm_table` field (table ids above 255);
    /// `rtm_table` itself is authoritative otherwise. `RT_TABLE_MAIN` (254)
    /// always fits in the byte, but recognizing the attribute too matches
    /// what the task's safety rule asks for explicitly.
    fn effective_table(&self) -> u32 {
        self.attributes
            .get(&libc::RTA_TABLE)
            .and_then(|value| read_u32(value))
            .unwrap_or(self.table as u32)
    }

    fn is_default_route(&self) -> bool {
        self.dst_len == 0
    }
}

/// Encodes a captured route back into an `rtmsg` + attributes payload
/// (everything after the `nlmsghdr`), suitable for `RTM_DELROUTE` or
/// `RTM_NEWROUTE`.
fn encode_route(route: &CapturedRoute) -> Vec<u8> {
    let mut payload = Vec::with_capacity(
        RTM_HDRLEN
            + route
                .attributes
                .values()
                .map(|value| rta_align(rta_length(value.len())))
                .sum::<usize>(),
    );
    payload.push(route.family);
    payload.push(route.dst_len);
    payload.push(route.src_len);
    payload.push(route.tos);
    payload.push(route.table);
    payload.push(route.protocol);
    payload.push(route.scope);
    payload.push(route.rtm_type);
    payload.extend_from_slice(&route.flags.to_ne_bytes());
    for (&attribute_type, value) in &route.attributes {
        push_attribute(&mut payload, attribute_type, value);
    }
    payload
}

/// Only the attribute types this policy needs to capture/recreate a route.
/// Anything else the kernel reports (e.g. `RTA_CACHEINFO`, `RTA_METRICS`) is
/// deliberately dropped: it is derived/informational, not part of what
/// `RTM_NEWROUTE` needs to recreate the route, and keeping it out avoids
/// accidentally trying to re-submit kernel-only attributes on restore.
const CAPTURED_ATTRIBUTE_TYPES: [u16; 6] = [
    libc::RTA_DST,
    libc::RTA_OIF,
    libc::RTA_GATEWAY,
    libc::RTA_PREFSRC,
    libc::RTA_PRIORITY,
    libc::RTA_TABLE,
];

/// Parses an `rtmsg` + attributes payload (the bytes after the `nlmsghdr`)
/// as reported by `RTM_GETROUTE`, into a [`CapturedRoute`].
fn parse_route(payload: &[u8]) -> Option<CapturedRoute> {
    if payload.len() < RTM_HDRLEN {
        return None;
    }
    let family = payload[0];
    let dst_len = payload[1];
    let src_len = payload[2];
    let tos = payload[3];
    let table = payload[4];
    let protocol = payload[5];
    let scope = payload[6];
    let rtm_type = payload[7];
    let flags = u32::from_ne_bytes(payload[8..12].try_into().ok()?);

    let mut attributes = BTreeMap::new();
    let mut offset = RTM_HDRLEN;
    while offset + RTA_HDRLEN <= payload.len() {
        let attribute_len = u16::from_ne_bytes(payload[offset..offset + 2].try_into().ok()?) as usize;
        let attribute_type = u16::from_ne_bytes(payload[offset + 2..offset + 4].try_into().ok()?);
        if attribute_len < RTA_HDRLEN || offset + attribute_len > payload.len() {
            break;
        }
        if CAPTURED_ATTRIBUTE_TYPES.contains(&attribute_type) {
            attributes.insert(attribute_type, payload[offset + RTA_HDRLEN..offset + attribute_len].to_vec());
        }
        offset += rta_align(attribute_len);
    }

    Some(CapturedRoute {
        family,
        dst_len,
        src_len,
        tos,
        table,
        protocol,
        scope,
        rtm_type,
        flags,
        attributes,
    })
}

/// Encodes a full netlink datagram: header plus payload, sized to
/// `NLMSG_SPACE(len)` (the header's own `nlmsg_len` field records the
/// unaligned `NLMSG_LENGTH(len)`, matching kernel convention).
fn encode_message(message_type: u16, flags: u16, seq: u32, pid: u32, payload: &[u8]) -> Vec<u8> {
    let unaligned_len = NLMSG_HDRLEN + payload.len();
    let mut buffer = vec![0_u8; nlmsg_align(unaligned_len)];
    buffer[0..4].copy_from_slice(&(unaligned_len as u32).to_ne_bytes());
    buffer[4..6].copy_from_slice(&message_type.to_ne_bytes());
    buffer[6..8].copy_from_slice(&flags.to_ne_bytes());
    buffer[8..12].copy_from_slice(&seq.to_ne_bytes());
    buffer[12..16].copy_from_slice(&pid.to_ne_bytes());
    buffer[NLMSG_HDRLEN..NLMSG_HDRLEN + payload.len()].copy_from_slice(payload);
    buffer
}

// ---------------------------------------------------------------------------
// Netlink socket
// ---------------------------------------------------------------------------

/// A single `AF_NETLINK`/`NETLINK_ROUTE` socket, kept open for the lifetime
/// of the owning [`TetherRoutePolicy`] rather than reopened per call:
/// `refresh()` runs from the host's outer discovery loop (not the per-frame
/// hot path), but `AGENTS.md` still forbids incidental process/socket churn
/// there, and a bounded receive timeout keeps a lost reply from ever
/// blocking that loop.
struct NetlinkSocket {
    fd: i32,
    /// Our kernel-assigned port id (`sockaddr_nl.nl_pid`), learned via
    /// `getsockname` after binding with `nl_pid = 0`. Used to reject
    /// messages that are not addressed to us.
    pid: u32,
    seq: u32,
}

impl NetlinkSocket {
    fn open() -> io::Result<Self> {
        let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, libc::NETLINK_ROUTE) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // A lost/dropped reply must never hang the host's outer discovery
        // loop, so bound how long a receive can block.
        let timeout = libc::timeval { tv_sec: 2, tv_usec: 0 };
        let timeout_result = unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                (&timeout as *const libc::timeval).cast(),
                size_of::<libc::timeval>() as libc::socklen_t,
            )
        };
        if timeout_result != 0 {
            let error = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(error);
        }

        let mut address: libc::sockaddr_nl = unsafe { zeroed() };
        address.nl_family = libc::AF_NETLINK as libc::sa_family_t;
        // nl_pid = 0 asks the kernel to assign a free port id.
        let bind_result = unsafe {
            libc::bind(
                fd,
                (&address as *const libc::sockaddr_nl).cast(),
                size_of::<libc::sockaddr_nl>() as libc::socklen_t,
            )
        };
        if bind_result != 0 {
            let error = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(error);
        }

        let mut bound: libc::sockaddr_nl = unsafe { zeroed() };
        let mut bound_len = size_of::<libc::sockaddr_nl>() as libc::socklen_t;
        let getsockname_result = unsafe { libc::getsockname(fd, (&mut bound as *mut libc::sockaddr_nl).cast(), &mut bound_len) };
        if getsockname_result != 0 {
            let error = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(error);
        }

        Ok(Self {
            fd,
            pid: bound.nl_pid,
            seq: 0,
        })
    }

    /// Sends one request (always `NLM_F_REQUEST`, plus whatever `flags`
    /// the caller adds — `NLM_F_DUMP` for a dump, `NLM_F_ACK` for a
    /// mutation) and collects every reply belonging to it: data messages
    /// until `NLMSG_DONE` for a multipart dump, or the single reply/ack for
    /// a non-multipart request.
    fn transact(&mut self, message_type: u16, flags: u16, payload: &[u8]) -> io::Result<Vec<Vec<u8>>> {
        self.seq = self.seq.wrapping_add(1);
        let seq = self.seq;
        let flags = flags | (libc::NLM_F_REQUEST as u16);
        let request = encode_message(message_type, flags, seq, self.pid, payload);
        let sent = unsafe { libc::send(self.fd, request.as_ptr().cast(), request.len(), 0) };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        self.receive_replies(seq)
    }

    fn receive_replies(&mut self, seq: u32) -> io::Result<Vec<Vec<u8>>> {
        let mut results = Vec::new();
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            let received = unsafe { libc::recv(self.fd, buffer.as_mut_ptr().cast(), buffer.len(), 0) };
            if received < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "netlink route request timed out waiting for a kernel reply",
                    ));
                }
                return Err(error);
            }
            let received = received as usize;

            let mut offset = 0;
            while offset + NLMSG_HDRLEN <= received {
                let header = &buffer[offset..offset + NLMSG_HDRLEN];
                let message_len = u32::from_ne_bytes(header[0..4].try_into().unwrap()) as usize;
                let message_type = u16::from_ne_bytes(header[4..6].try_into().unwrap());
                let message_flags = u16::from_ne_bytes(header[6..8].try_into().unwrap());
                let message_seq = u32::from_ne_bytes(header[8..12].try_into().unwrap());
                let message_pid = u32::from_ne_bytes(header[12..16].try_into().unwrap());
                if message_len < NLMSG_HDRLEN || offset + message_len > received {
                    // Truncated/malformed; nothing safe to parse past here.
                    break;
                }
                let body = &buffer[offset + NLMSG_HDRLEN..offset + message_len];
                offset += nlmsg_align(message_len);

                // A reply's nlmsg_pid is normally the requester's own port
                // id (echoed back by the kernel); 0 covers kernel-generated
                // notification-style messages, which we should not need on
                // this unicast, no-multicast-group socket but are harmless
                // to accept.
                if message_seq != seq || (message_pid != self.pid && message_pid != 0) {
                    continue;
                }

                if message_type == libc::NLMSG_DONE as u16 {
                    return Ok(results);
                }
                if message_type == libc::NLMSG_ERROR as u16 {
                    if body.len() < 4 {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated netlink error reply"));
                    }
                    let code = i32::from_ne_bytes(body[0..4].try_into().unwrap());
                    return if code == 0 {
                        Ok(results) // NLMSG_ERROR with error == 0 is an ACK.
                    } else {
                        Err(netlink_errno(-code))
                    };
                }

                results.push(body.to_vec());
                if message_flags & (libc::NLM_F_MULTI as u16) == 0 {
                    // A single non-multipart reply (e.g. a route-get) is
                    // complete as soon as this one message arrives.
                    return Ok(results);
                }
            }
        }
    }
}

impl Drop for NetlinkSocket {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

/// Converts a positive errno from an `NLMSG_ERROR` reply into an
/// [`io::Error`]. `EPERM`/`EACCES` get an explicit `CAP_NET_ADMIN` hint;
/// `bin/host.rs` also matches on `ErrorKind::PermissionDenied` itself to
/// append its own "run elevated" hint, which `io::Error::new` with that kind
/// preserves.
fn netlink_errno(errno: i32) -> io::Error {
    if errno == libc::EPERM || errno == libc::EACCES {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "the kernel rejected a routing table change ({}); this needs CAP_NET_ADMIN, run the launcher elevated",
                io::Error::from_raw_os_error(errno)
            ),
        )
    } else {
        io::Error::from_raw_os_error(errno)
    }
}

fn is_missing_route_error(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(code) if code == libc::ESRCH || code == libc::ENOENT)
}

// ---------------------------------------------------------------------------
// Route operations
// ---------------------------------------------------------------------------

/// Dumps every `RT_TABLE_MAIN` default route (`rtm_dst_len == 0`) whose
/// `RTA_OIF` is `interface_index`, for the given address `family`
/// (`AF_INET` or `AF_INET6`). See the module's `# SAFETY` section: this
/// filter is the only thing standing between this module and deleting an
/// unrelated interface's default route.
fn dump_default_routes(socket: &mut NetlinkSocket, family: u8, interface_index: u32) -> io::Result<Vec<CapturedRoute>> {
    let mut payload = vec![0_u8; RTM_HDRLEN];
    payload[0] = family;
    let flags = libc::NLM_F_DUMP as u16;
    let replies = socket.transact(libc::RTM_GETROUTE, flags, &payload)?;

    Ok(replies
        .iter()
        .filter_map(|body| parse_route(body))
        .filter(|route| route.is_default_route())
        .filter(|route| route.oif() == Some(interface_index))
        .filter(|route| route.effective_table() == libc::RT_TABLE_MAIN as u32)
        .collect())
}

/// Deletes one previously-dumped default route. `ESRCH`/`ENOENT` (the route
/// is already gone — e.g. removed by something else between dump and
/// delete) counts as success, mirroring the Windows policy's `is_missing`
/// handling of `ERROR_FILE_NOT_FOUND`/`ERROR_NOT_FOUND`.
fn delete_route(socket: &mut NetlinkSocket, route: &CapturedRoute) -> io::Result<()> {
    let payload = encode_route(route);
    let flags = libc::NLM_F_ACK as u16;
    match socket.transact(libc::RTM_DELROUTE, flags, &payload) {
        Ok(_) => Ok(()),
        Err(error) if is_missing_route_error(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

/// Recreates one previously-deleted default route. `EEXIST` (something else
/// — DHCP, NetworkManager — already put an equivalent route back) counts as
/// success.
fn create_route(socket: &mut NetlinkSocket, route: &CapturedRoute) -> io::Result<()> {
    let payload = encode_route(route);
    let flags = (libc::NLM_F_CREATE | libc::NLM_F_EXCL | libc::NLM_F_ACK) as u16;
    match socket.transact(libc::RTM_NEWROUTE, flags, &payload) {
        Ok(_) => Ok(()),
        Err(error) if error.raw_os_error() == Some(libc::EEXIST) => Ok(()),
        Err(error) => Err(error),
    }
}

/// Resolves the interface a peer address would be routed through via a
/// kernel route-get (`RTM_GETROUTE` without `NLM_F_DUMP`, carrying
/// `RTA_DST`), the netlink equivalent of `ip route get <addr>`. Never shells
/// out to `ip`.
fn interface_index_for_peer(socket: &mut NetlinkSocket, peer: IpAddr) -> io::Result<u32> {
    let (family, dst_len, dst_bytes) = match peer {
        IpAddr::V4(address) => (libc::AF_INET as u8, 32_u8, address.octets().to_vec()),
        IpAddr::V6(address) => (libc::AF_INET6 as u8, 128_u8, address.octets().to_vec()),
    };
    let mut payload = vec![0_u8; RTM_HDRLEN];
    payload[0] = family;
    payload[1] = dst_len;
    push_attribute(&mut payload, libc::RTA_DST, &dst_bytes);

    let replies = socket.transact(libc::RTM_GETROUTE, 0, &payload)?;
    let route = replies.first().and_then(|body| parse_route(body)).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "the kernel returned no route for the discovered phone",
        )
    })?;
    route.oif().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "the kernel route reply for the discovered phone carried no output interface",
        )
    })
}

// ---------------------------------------------------------------------------
// Tether interface identification (sysfs)
// ---------------------------------------------------------------------------

/// USB "network class" kernel drivers Android tethering (and, for
/// completeness, iOS personal hotspot's `ipheth`) binds to. Matched by
/// driver *name*, not interface *name*: unlike Windows adapter description
/// strings, Linux interface names for these devices are unstable and
/// distribution/udev-rule dependent (`usb0`, `enx<mac>`, `rndis0`, ...),
/// while the kernel driver bound to the sysfs `device` node is not.
fn is_tether_driver(name: &str) -> bool {
    matches!(
        name,
        "rndis_host" | "cdc_ncm" | "cdc_ether" | "cdc_mbim" | "cdc_subset" | "ipheth"
    )
}

/// Decides whether `/sys/class/net/<name>` is a currently-up USB Android/iOS
/// tethering interface.
///
/// sysfs is used instead of name matching for the same reason as the driver
/// check above (unstable names), and gives two independent confirmations
/// instead of one: the driver bound to the device, and the bus the device is
/// enumerated on. Bridges, tailscale, veth pairs, and docker0 have no
/// `device` symlink at all (verified on this machine: only the real PCIe
/// Ethernet NIC does), so they are excluded before the driver name is even
/// read.
fn is_tether_interface(interface_dir: &Path) -> io::Result<bool> {
    let device_dir = interface_dir.join("device");
    if fs::symlink_metadata(&device_dir).is_err() {
        return Ok(false);
    }

    let Some(driver) = driver_name(&device_dir)? else {
        return Ok(false);
    };
    if !is_tether_driver(&driver) {
        return Ok(false);
    }
    if !is_usb_device(&device_dir)? {
        return Ok(false);
    }
    is_interface_up(interface_dir)
}

fn driver_name(device_dir: &Path) -> io::Result<Option<String>> {
    match fs::read_link(device_dir.join("driver")) {
        Ok(target) => Ok(target.file_name().map(|name| name.to_string_lossy().into_owned())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Confirms the device is enumerated on the USB bus by resolving the
/// `subsystem` symlink, which sysfs points at `/sys/bus/<bus name>` for
/// whatever bus a device belongs to (verified on this machine: the PCIe
/// Ethernet NIC's `device/subsystem` resolves to `/sys/bus/pci`, not
/// `/sys/bus/usb`). Chosen over a substring match on the canonicalized
/// `device` path itself, because `subsystem` gives an exact bus identity
/// rather than hoping "usb" cannot appear in an unrelated path segment.
fn is_usb_device(device_dir: &Path) -> io::Result<bool> {
    match fs::canonicalize(device_dir.join("subsystem")) {
        Ok(path) => Ok(path.file_name().is_some_and(|name| name == "usb")
            && path.parent().and_then(Path::file_name).is_some_and(|name| name == "bus")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// `IFF_UP` (`include/uapi/linux/if.h`): bit 0 of the hex flags word exposed
/// at `/sys/class/net/<if>/flags`. Named rather than inlined as a bare `1`
/// so the check in [`is_interface_up`] reads as "administratively up" and
/// not a magic number.
const IFF_UP: u32 = 0x1;

/// Whether `/sys/class/net/<name>` is both administratively up (`IFF_UP` set
/// in `flags`) and link-up (`carrier == 1`).
///
/// This is deliberately *not* `operstate == "up"`, which is what this
/// function replaced after that check shipped a bug: the USB tethering
/// drivers this module targets (`rndis_host`/`usbnet`, and the other
/// drivers in [`is_tether_driver`]) never report carrier state through
/// `IF_OPER_*`, so a fully working USB tether sits at `operstate=unknown`
/// forever and never reaches `operstate=up`. Measured on the machine this
/// bug was found on, with a real phone tethered: the working tether
/// (`enp0s20f0u1`, driver `rndis_host`) reported `operstate=unknown,
/// carrier=1`, while an administratively-down bridge (`docker0`,
/// `br-1d3ff46b91b2`) reported `operstate=down, carrier=0`. `carrier` is
/// the correct discriminator here; checking `operstate` instead silently
/// excludes every working USB tether and turns this whole module into a
/// no-op. Do not "simplify" this back to `operstate` — that regression is
/// exactly what this comment exists to prevent.
///
/// `flags`' `IFF_UP` bit is also required, even though `carrier` alone is
/// sufficient given the measurements above (`flags` read `0x1003` — `IFF_UP`
/// set — for every interface measured, tether and non-tether alike, so it
/// adds no discriminating power on its own). It costs nothing to check and
/// makes the "administratively up" half of the intent explicit in the code
/// instead of left implicit in "carrier happens to only read 1 when up".
fn is_interface_up(interface_dir: &Path) -> io::Result<bool> {
    Ok(flags_has_iff_up(interface_dir)? && carrier_is_up(interface_dir))
}

/// Parses `/sys/class/net/<if>/flags` — a `0x`-prefixed hex `short int` per
/// `netdevice(7)` — and reports whether `IFF_UP` is set.
fn flags_has_iff_up(interface_dir: &Path) -> io::Result<bool> {
    match fs::read_to_string(interface_dir.join("flags")) {
        Ok(value) => {
            let hex = value.trim().trim_start_matches("0x");
            let parsed = u32::from_str_radix(hex, 16).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            Ok(parsed & IFF_UP != 0)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Reads `/sys/class/net/<if>/carrier`: `true` only when the kernel reports
/// the physical/link layer as up (`1`).
///
/// Unlike the sibling `ifindex`/`driver_name`/`flags_has_iff_up` helpers,
/// *every* read error here is treated as "not up", not just `NotFound`:
/// reading `carrier` on an administratively-down interface returns
/// `EINVAL`, which is a routine state (e.g. an unplugged bridge), not a
/// failure worth propagating.
fn carrier_is_up(interface_dir: &Path) -> bool {
    fs::read_to_string(interface_dir.join("carrier")).is_ok_and(|value| value.trim() == "1")
}

fn ifindex(interface_dir: &Path) -> io::Result<Option<u32>> {
    match fs::read_to_string(interface_dir.join("ifindex")) {
        Ok(value) => Ok(value.trim().parse().ok()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Every currently-present, currently-up tether interface's ifindex.
/// Finding none is not an error — see [`TetherRoutePolicy::refresh`].
fn tether_interface_indices() -> io::Result<Vec<u32>> {
    let entries = match fs::read_dir("/sys/class/net") {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut indices = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_name() == "lo" {
            continue;
        }
        let interface_dir = entry.path();
        if !is_tether_interface(&interface_dir)? {
            continue;
        }
        if let Some(index) = ifindex(&interface_dir)?
            && !indices.contains(&index)
        {
            indices.push(index);
        }
    }
    Ok(indices)
}

/// Whether any interface currently reports `interface_index` as its
/// ifindex, tether or not. Used by [`restore_snapshot`] the same way the
/// Windows policy's `interface_row(...).is_none()` short-circuits restoring
/// an adapter that no longer exists (phone unplugged / RNDIS interface
/// torn down): recreating a route against a gone ifindex would just fail,
/// and there is nothing to restore if the interface itself is gone.
fn interface_exists(interface_index: u32) -> io::Result<bool> {
    let entries = match fs::read_dir("/sys/class/net") {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        if ifindex(&entry.path())? == Some(interface_index) {
            return Ok(true);
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct InterfaceSnapshot {
    interface_index: u32,
    family: u8,
    /// The `RT_TABLE_MAIN` default routes this policy removed for this
    /// (interface, family). Recreated verbatim on restore.
    routes: Vec<CapturedRoute>,
}

/// Owns the temporary local-only policy for the duration of a native host.
///
/// Mirrors the Windows `TetherRoutePolicy` (see `../windows.rs`) shape
/// exactly — `new`, `refresh`, `protect_peer`, `restore`, `Drop` — so
/// `bin/host.rs` needs no platform branching beyond the `#[cfg]` that
/// selects this module. The policy is deliberately opt-in and deliberately
/// scoped to interfaces [`is_tether_interface`] recognizes; a normal
/// Ethernet or Wi-Fi adapter is never selected merely because it currently
/// holds the default route or because a discovery packet happened to arrive
/// through it.
///
/// There is no Linux analogue of the Windows per-interface
/// `DisableDefaultRoutes` flag (a standing "never route through me"
/// switch). Instead, `refresh()` re-deletes any default route that
/// reappeared (e.g. NetworkManager or a DHCP client re-adding one) since the
/// last call, but `bin/host.rs` only calls it from the outer *discovery*
/// loop — the code that runs while waiting for a phone to connect. Once a
/// phone connects, the host enters `serve_connection` and stays there for
/// the entire session; `refresh()` is not called again until that
/// connection ends and discovery resumes. So, unlike the Windows flag, this
/// is not a standing guarantee for the life of the process: a default route
/// that reappears on the tether interface *while a session is active* stays
/// in place until the next discovery cycle. TODO: before this could be
/// enabled by default, either call `refresh()` periodically from inside
/// `serve_connection` too, or otherwise establish a standing guarantee for
/// the active-session case instead of only the discovery-loop case.
pub struct TetherRoutePolicy {
    // Opened lazily so `new()` has no side effects (matching the Windows
    // policy), then kept open for the policy's lifetime: `refresh()` runs
    // from the host's outer discovery loop, not the per-frame hot path, but
    // still should not open a fresh socket on every call.
    socket: Option<NetlinkSocket>,
    snapshots: Vec<InterfaceSnapshot>,
}

impl TetherRoutePolicy {
    pub fn new() -> Self {
        Self {
            socket: None,
            snapshots: Vec::new(),
        }
    }

    fn socket(&mut self) -> io::Result<&mut NetlinkSocket> {
        if self.socket.is_none() {
            self.socket = Some(NetlinkSocket::open()?);
        }
        Ok(self.socket.as_mut().expect("just initialized"))
    }

    /// Protect currently-present tether interfaces.
    ///
    /// It is valid for this to find no interface yet — the caller retries
    /// from the outer discovery loop until Linux has finished creating the
    /// tether interface (or the phone is plugged in after the host already
    /// started).
    pub fn refresh(&mut self) -> io::Result<()> {
        for interface_index in tether_interface_indices()? {
            self.protect_interface(interface_index)?;
        }
        Ok(())
    }

    /// Protect the interface the discovered phone is reachable through.
    pub fn protect_peer(&mut self, peer: SocketAddr) -> io::Result<()> {
        let interface_index = interface_index_for_peer(self.socket()?, peer.ip())?;
        if !tether_interface_indices()?.contains(&interface_index) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "the discovered phone is not connected through a recognized Android/iOS USB-tethering adapter (interface {interface_index})"
                ),
            ));
        }
        self.protect_interface(interface_index)
    }

    /// Restore every default route this guard removed, in reverse order.
    /// Collects the first error but keeps going; clears the snapshot list
    /// only once every restore attempt has run without error, mirroring the
    /// Windows policy's `restore()`.
    pub fn restore(&mut self) -> io::Result<()> {
        let snapshots = self.snapshots.clone();
        let mut first_error = None;
        for snapshot in snapshots.iter().rev() {
            let result = self.socket().and_then(|socket| restore_snapshot(socket, snapshot));
            if let Err(error) = result
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        if let Some(error) = first_error {
            Err(error)
        } else {
            self.snapshots.clear();
            Ok(())
        }
    }

    fn protect_interface(&mut self, interface_index: u32) -> io::Result<()> {
        let already_protected: Vec<u8> = self
            .snapshots
            .iter()
            .filter(|snapshot| snapshot.interface_index == interface_index)
            .map(|snapshot| snapshot.family)
            .collect();

        for family in &already_protected {
            let socket = self.socket()?;
            reassert_family(socket, interface_index, *family)?;
        }

        let mut added = Vec::new();
        for family in [libc::AF_INET as u8, libc::AF_INET6 as u8] {
            if already_protected.contains(&family) {
                continue;
            }
            let socket = self.socket()?;
            match capture_and_delete(socket, interface_index, family) {
                Ok(Some(snapshot)) => added.push(snapshot),
                Ok(None) => {}
                Err(error) => {
                    let socket = self.socket()?;
                    for snapshot in added.iter().rev() {
                        let _ = restore_snapshot(socket, snapshot);
                    }
                    return Err(error);
                }
            }
        }
        self.snapshots.extend(added);
        Ok(())
    }
}

impl Default for TetherRoutePolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TetherRoutePolicy {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            eprintln!("local-only tether cleanup failed: {error}");
        }
    }
}

/// Re-deletes any `RT_TABLE_MAIN` default route that reappeared on an
/// already-protected (interface, family) since the last call — the
/// re-assertion that replaces the missing Windows `DisableDefaultRoutes`
/// flag (see the struct doc comment).
fn reassert_family(socket: &mut NetlinkSocket, interface_index: u32, family: u8) -> io::Result<()> {
    for route in dump_default_routes(socket, family, interface_index)? {
        delete_route(socket, &route)?;
    }
    Ok(())
}

fn capture_and_delete(socket: &mut NetlinkSocket, interface_index: u32, family: u8) -> io::Result<Option<InterfaceSnapshot>> {
    let routes = dump_default_routes(socket, family, interface_index)?;
    if routes.is_empty() {
        return Ok(None);
    }

    let mut deleted = Vec::new();
    for route in &routes {
        if let Err(error) = delete_route(socket, route) {
            for deleted_route in deleted.iter().rev() {
                let _ = create_route(socket, deleted_route);
            }
            return Err(error);
        }
        deleted.push(route.clone());
    }
    Ok(Some(InterfaceSnapshot {
        interface_index,
        family,
        routes: deleted,
    }))
}

fn restore_snapshot(socket: &mut NetlinkSocket, snapshot: &InterfaceSnapshot) -> io::Result<()> {
    if !interface_exists(snapshot.interface_index)? {
        return Ok(());
    }

    // Remove anything that appeared while suppression was active and is not
    // part of the original snapshot...
    let current = dump_default_routes(socket, snapshot.family, snapshot.interface_index)?;
    for route in &current {
        if !snapshot.routes.contains(route) {
            delete_route(socket, route)?;
        }
    }

    // ...then recreate whatever from the original snapshot is still absent.
    let remaining = dump_default_routes(socket, snapshot.family, snapshot.interface_index)?;
    for route in &snapshot.routes {
        if !remaining.contains(route) {
            create_route(socket, route)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        CapturedRoute, driver_name, encode_message, encode_route, is_tether_driver, is_usb_device, nlmsg_align, parse_route,
        rta_align, rta_length,
    };
    use std::collections::BTreeMap;
    use std::path::Path;

    #[test]
    fn recognizes_known_usb_tether_drivers() {
        assert!(is_tether_driver("rndis_host"));
        assert!(is_tether_driver("cdc_ncm"));
        assert!(is_tether_driver("cdc_ether"));
        assert!(is_tether_driver("cdc_mbim"));
        assert!(is_tether_driver("cdc_subset"));
        assert!(is_tether_driver("ipheth"));
    }

    #[test]
    fn does_not_recognize_normal_network_drivers() {
        assert!(!is_tether_driver("r8169"));
        assert!(!is_tether_driver("e1000e"));
        assert!(!is_tether_driver("iwlwifi"));
        assert!(!is_tether_driver("tailscale"));
        assert!(!is_tether_driver("veth"));
        assert!(!is_tether_driver("bridge"));
        assert!(!is_tether_driver(""));
    }

    #[test]
    fn nlmsg_align_matches_kernel_macro() {
        // #define NLMSG_ALIGN(len) (((len)+NLMSG_ALIGNTO-1) & ~(NLMSG_ALIGNTO-1)), NLMSG_ALIGNTO = 4
        assert_eq!(nlmsg_align(0), 0);
        assert_eq!(nlmsg_align(1), 4);
        assert_eq!(nlmsg_align(4), 4);
        assert_eq!(nlmsg_align(5), 8);
        assert_eq!(nlmsg_align(16), 16);
        assert_eq!(nlmsg_align(17), 20);
    }

    #[test]
    fn rta_align_matches_kernel_macro() {
        // #define RTA_ALIGN(len) (((len)+RTA_ALIGNTO-1) & ~(RTA_ALIGNTO-1)), RTA_ALIGNTO = 4
        assert_eq!(rta_align(0), 0);
        assert_eq!(rta_align(1), 4);
        assert_eq!(rta_align(4), 4);
        assert_eq!(rta_align(6), 8);
    }

    #[test]
    fn rta_length_matches_kernel_macro() {
        // #define RTA_LENGTH(len) (RTA_ALIGN(sizeof(struct rtattr)) + (len)) == 4 + len
        assert_eq!(rta_length(0), 4);
        assert_eq!(rta_length(4), 8);
        assert_eq!(rta_length(1), 5);
    }

    #[test]
    fn is_usb_device_rejects_missing_subsystem_link() {
        // A path with no `subsystem` symlink (or no device at all) must not
        // be treated as USB; this is the guard that keeps a non-existent or
        // malformed sysfs entry from ever being misread as "yes, USB".
        assert!(!is_usb_device(Path::new("/nonexistent/holodori-test-path")).unwrap());
    }

    fn ipv4_default_route_via(oif: u32, gateway: [u8; 4]) -> CapturedRoute {
        let mut attributes = BTreeMap::new();
        attributes.insert(libc::RTA_OIF, oif.to_ne_bytes().to_vec());
        attributes.insert(libc::RTA_GATEWAY, gateway.to_vec());
        attributes.insert(libc::RTA_PRIORITY, 600_u32.to_ne_bytes().to_vec());
        CapturedRoute {
            family: libc::AF_INET as u8,
            dst_len: 0,
            src_len: 0,
            tos: 0,
            table: libc::RT_TABLE_MAIN,
            // 16 = RTPROT_DHCP per include/uapi/linux/rtnetlink.h; not
            // exposed as a libc constant in this crate version, so used as
            // a literal here (any protocol id round-trips identically).
            protocol: 16,
            scope: libc::RT_SCOPE_UNIVERSE,
            rtm_type: libc::RTN_UNICAST,
            flags: 0,
            attributes,
        }
    }

    #[test]
    fn captured_route_round_trips_through_encode_and_parse() {
        // This is where a NLMSG_ALIGN/RTA_ALIGN bug would show up: an
        // off-by-one in either alignment helper corrupts the attribute
        // stream and this equality fails.
        let route = ipv4_default_route_via(7, [192, 168, 42, 1]);
        let encoded = encode_route(&route);
        let parsed = parse_route(&encoded).expect("payload parses back");
        assert_eq!(parsed, route);
    }

    #[test]
    fn captured_route_with_odd_length_attribute_round_trips() {
        // RTA_GATEWAY for IPv6 is 16 bytes (already aligned), but exercise
        // an attribute whose value length is NOT a multiple of 4 (padding
        // required) to make sure padding bytes are inserted and skipped
        // correctly on both sides.
        let mut attributes = BTreeMap::new();
        attributes.insert(libc::RTA_OIF, 3_u32.to_ne_bytes().to_vec());
        // 1-byte value forces 3 bytes of alignment padding after it.
        attributes.insert(libc::RTA_TABLE, vec![254_u8]);
        let route = CapturedRoute {
            family: libc::AF_INET as u8,
            dst_len: 0,
            src_len: 0,
            tos: 0,
            table: libc::RT_TABLE_MAIN,
            protocol: libc::RTPROT_STATIC,
            scope: libc::RT_SCOPE_UNIVERSE,
            rtm_type: libc::RTN_UNICAST,
            flags: 0,
            attributes,
        };
        let encoded = encode_route(&route);
        let parsed = parse_route(&encoded).expect("payload parses back");
        assert_eq!(parsed, route);
    }

    #[test]
    fn encode_message_records_unaligned_length_and_pads_buffer() {
        // NLMSG_LENGTH(len) is deliberately unaligned; NLMSG_SPACE(len) (the
        // buffer size) is the aligned version. A payload of 1 byte makes
        // both differ, which is exactly the case that would hide an
        // off-by-one if the header field and the buffer length were mixed
        // up.
        let payload = [0xAB_u8];
        let message = encode_message(24 /* RTM_NEWROUTE */, 1, 42, 7, &payload);
        let nlmsg_len = u32::from_ne_bytes(message[0..4].try_into().unwrap());
        assert_eq!(nlmsg_len as usize, super::NLMSG_HDRLEN + payload.len());
        assert_eq!(message.len(), nlmsg_align(super::NLMSG_HDRLEN + payload.len()));
        let seq = u32::from_ne_bytes(message[8..12].try_into().unwrap());
        let pid = u32::from_ne_bytes(message[12..16].try_into().unwrap());
        assert_eq!(seq, 42);
        assert_eq!(pid, 7);
        assert_eq!(message[super::NLMSG_HDRLEN], 0xAB);
    }

    #[test]
    fn parse_route_rejects_truncated_payload() {
        assert!(parse_route(&[0_u8; 4]).is_none());
    }

    #[test]
    fn parse_route_ignores_uncaptured_attribute_types() {
        // RTA_CACHEINFO (12) is not in CAPTURED_ATTRIBUTE_TYPES; confirm it
        // is silently skipped rather than corrupting the parse.
        let mut payload = vec![0_u8; super::RTM_HDRLEN];
        payload[0] = libc::AF_INET as u8;
        super::push_attribute(&mut payload, libc::RTA_CACHEINFO, &[1, 2, 3, 4]);
        super::push_attribute(&mut payload, libc::RTA_OIF, &5_u32.to_ne_bytes());
        let route = parse_route(&payload).expect("payload parses");
        assert!(!route.attributes.contains_key(&libc::RTA_CACHEINFO));
        assert_eq!(route.oif(), Some(5));
    }

    // -----------------------------------------------------------------
    // Read-only live checks against this machine's real routing table.
    //
    // Not run by default `cargo test` (like
    // `keyboard::linux::tests::creates_and_destroys_a_real_uinput_device`):
    // these depend on the machine's actual network state and are meant to
    // be run explicitly with `--ignored --nocapture` as a manual safety
    // check before trusting this module with a real tether interface. None
    // of them delete or create anything; `RTM_GETROUTE` (dump or
    // route-get) needs no privilege.
    // -----------------------------------------------------------------

    fn interface_name(interface_index: u32) -> String {
        let Ok(entries) = std::fs::read_dir("/sys/class/net") else {
            return format!("ifindex={interface_index}");
        };
        for entry in entries.flatten() {
            if super::ifindex(&entry.path()).ok().flatten() == Some(interface_index) {
                return entry.file_name().to_string_lossy().into_owned();
            }
        }
        format!("ifindex={interface_index}")
    }

    #[test]
    #[ignore = "depends on this machine's live network state; run explicitly with --nocapture"]
    fn tether_interface_indices_never_returns_a_non_usb_interface() {
        // The single most important safety check for this module. It does
        // NOT assert "no phone is connected" (an environment precondition
        // this test cannot verify, and the exact reason a previous version
        // of this test passed on a machine with a phone plugged in the
        // whole time it should have caught the operstate/carrier bug).
        // Instead it asserts the property that must hold regardless of
        // whether a phone is attached: nothing not-USB-tether is ever
        // returned.
        let indices = super::tether_interface_indices().expect("sysfs enumeration succeeds");

        // Property 1: every returned ifindex, independently re-resolved
        // back to an interface name, re-passes the same three signals
        // `is_tether_interface` used (device symlink present, driver in the
        // accepted set, subsystem resolves to the USB bus). This re-derives
        // the checks rather than calling `is_tether_interface` again, so a
        // bug shared between production and this test is less likely to go
        // unnoticed.
        let entries = std::fs::read_dir("/sys/class/net").expect("read /sys/class/net");
        for entry in entries.flatten() {
            let interface_dir = entry.path();
            let Some(index) = super::ifindex(&interface_dir).unwrap() else { continue };
            if !indices.contains(&index) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let device_dir = interface_dir.join("device");
            assert!(
                std::fs::symlink_metadata(&device_dir).is_ok(),
                "{name} (ifindex {index}) was returned as a tether interface but has no `device` symlink"
            );
            let driver = driver_name(&device_dir).unwrap().unwrap_or_default();
            assert!(
                is_tether_driver(&driver),
                "{name} (ifindex {index}) was returned as a tether interface but its driver `{driver}` is not a recognized USB tethering driver"
            );
            assert!(
                is_usb_device(&device_dir).unwrap(),
                "{name} (ifindex {index}) was returned as a tether interface but is not enumerated on the USB bus"
            );
        }

        // Property 2: well-known non-tether interfaces, if present on this
        // machine, are never returned. Deliberately does NOT use an `enp*`/
        // `eno*`/`wlp*` interface *name* prefix as a stand-in for "this is a
        // PCI device": this machine's real USB tether happens to be named
        // `enp0s20f0u1` (predictable-network-interface-names assigns names
        // from the USB controller's PCI path), so a name-prefix check would
        // wrongly flag the one interface this test must allow. PCI-ness is
        // instead confirmed the same way production code confirms USB-ness:
        // by resolving the `device/subsystem` symlink.
        let entries = std::fs::read_dir("/sys/class/net").expect("read /sys/class/net");
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_bridge_veth_tunnel_or_loopback =
                name == "lo" || name == "docker0" || name.starts_with("br-") || name.starts_with("veth") || name == "tailscale0";
            let subsystem = std::fs::canonicalize(entry.path().join("device").join("subsystem")).ok();
            let is_pci_device = subsystem.is_some_and(|path| path.file_name().is_some_and(|bus| bus == "pci"));

            if !is_bridge_veth_tunnel_or_loopback && !is_pci_device {
                continue;
            }
            if let Some(index) = super::ifindex(&entry.path()).unwrap() {
                assert!(
                    !indices.contains(&index),
                    "{name} (ifindex {index}) is a known non-tether interface (bridge/veth/tunnel/loopback/PCI) but was returned by tether_interface_indices()"
                );
            }
        }
    }

    #[test]
    #[ignore = "depends on this machine's live network state; run explicitly with --nocapture; diagnostic only \
                (prints, does not assert beyond not panicking) so it stays green on CI machines with no phone attached"]
    fn diagnostic_print_detected_tether_interfaces() {
        let indices = super::tether_interface_indices().expect("sysfs enumeration succeeds");
        if indices.is_empty() {
            println!("no tether interfaces detected (expected on a machine with no phone attached)");
            return;
        }

        let entries = std::fs::read_dir("/sys/class/net").expect("read /sys/class/net");
        for entry in entries.flatten() {
            let interface_dir = entry.path();
            let Some(index) = super::ifindex(&interface_dir).unwrap() else { continue };
            if !indices.contains(&index) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let driver = driver_name(&interface_dir.join("device"))
                .unwrap()
                .unwrap_or_else(|| "<none>".to_owned());
            let operstate = std::fs::read_to_string(interface_dir.join("operstate"))
                .map(|value| value.trim().to_owned())
                .unwrap_or_else(|_| "<unreadable>".to_owned());
            let carrier = std::fs::read_to_string(interface_dir.join("carrier"))
                .map(|value| value.trim().to_owned())
                .unwrap_or_else(|_| "<unreadable>".to_owned());
            println!("detected tether interface: {name} (ifindex {index}) driver={driver} operstate={operstate} carrier={carrier}");
        }
    }

    #[test]
    #[ignore = "depends on this machine's live network state; run explicitly with --nocapture"]
    fn ipv4_default_route_dump_matches_ip_route_show_default() {
        // Deliberately does not filter by RTA_OIF (unlike
        // dump_default_routes, which the production code always scopes to
        // one already-verified tether ifindex): this dumps every
        // RT_TABLE_MAIN default route so the raw netlink output can be
        // eyeballed against `ip -4 route show default` for exactness.
        let mut socket = super::NetlinkSocket::open().expect("open a netlink socket");
        let mut payload = vec![0_u8; super::RTM_HDRLEN];
        payload[0] = libc::AF_INET as u8;
        let replies = socket
            .transact(libc::RTM_GETROUTE, libc::NLM_F_DUMP as u16, &payload)
            .expect("dump IPv4 routes");
        println!("--- netlink RTM_GETROUTE dump (AF_INET, default routes only) ---");
        for body in &replies {
            let Some(route) = super::parse_route(body) else { continue };
            if !route.is_default_route() || route.effective_table() != libc::RT_TABLE_MAIN as u32 {
                continue;
            }
            let gateway = route
                .attributes
                .get(&libc::RTA_GATEWAY)
                .and_then(|value| <[u8; 4]>::try_from(value.as_slice()).ok())
                .map(std::net::Ipv4Addr::from);
            let priority = route.attributes.get(&libc::RTA_PRIORITY).and_then(|value| super::read_u32(value));
            println!(
                "default via {:?} dev {} metric {:?} (table={} protocol={} scope={})",
                gateway,
                route.oif().map(interface_name).unwrap_or_default(),
                priority,
                route.effective_table(),
                route.protocol,
                route.scope,
            );
        }
        println!("--- `ip -4 route show default` ---");
        if let Ok(output) = std::process::Command::new("ip").args(["-4", "route", "show", "default"]).output() {
            print!("{}", String::from_utf8_lossy(&output.stdout));
        }
    }

    #[test]
    #[ignore = "depends on this machine's live network state; run explicitly with --nocapture"]
    fn route_get_matches_ip_route_get() {
        let peer: std::net::IpAddr = "8.8.8.8".parse().unwrap();
        let mut socket = super::NetlinkSocket::open().expect("open a netlink socket");
        let interface_index = super::interface_index_for_peer(&mut socket, peer).expect("route-get succeeds");
        println!(
            "netlink route-get for {peer}: oif={} ({})",
            interface_index,
            interface_name(interface_index)
        );
        println!("--- `ip route get {peer}` ---");
        if let Ok(output) = std::process::Command::new("ip").args(["route", "get", &peer.to_string()]).output() {
            print!("{}", String::from_utf8_lossy(&output.stdout));
        }
    }
}
