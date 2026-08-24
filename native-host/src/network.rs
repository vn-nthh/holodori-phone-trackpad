use crate::protocol::{
    DISCOVERY_ACK, DISCOVERY_HELLO, FRAME_MAGIC, decode_discovery, discovery_port_acceptable,
    encode_discovery,
};
use crate::tether_policy::{TetherBinding, current_tether_binding};
use std::io;
use std::mem::size_of;
use std::net::{SocketAddr, UdpSocket};
use std::os::windows::io::AsRawSocket;
use std::time::{Duration, Instant};
use windows_sys::Win32::Networking::WinSock::{
    SO_SNDTIMEO, SOCKET, SOCKET_ERROR, SOL_SOCKET, WSAGetLastError, setsockopt,
};

pub const DEFAULT_UDP_PORT: u16 = 42_825;
const DISCOVERY_READ_TIMEOUT: Duration = Duration::from_millis(250);
const FRAME_READ_TIMEOUT: Duration = Duration::from_millis(4);
const DATAGRAM_WRITE_TIMEOUT: Duration = Duration::from_millis(2);
const MAX_DATAGRAM_SIZE: usize = 2_048;
const REDUNDANT_SENDS: usize = 2;

pub struct UdpHost {
    socket: UdpSocket,
    port: u16,
}

impl UdpHost {
    pub fn bind(port: u16) -> io::Result<Self> {
        let socket = UdpSocket::bind(("0.0.0.0", port))?;
        set_write_timeout(&socket, DATAGRAM_WRITE_TIMEOUT)?;
        let port = socket.local_addr()?.port();
        socket.set_broadcast(true)?;
        socket.set_read_timeout(Some(DISCOVERY_READ_TIMEOUT))?;
        Ok(Self { socket, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn connect(&self, timeout: Duration) -> io::Result<UdpConnection<'_>> {
        self.connect_if(timeout, current_tether_binding)
    }

    fn connect_if<F>(
        &self,
        timeout: Duration,
        mut classify_peer: F,
    ) -> io::Result<UdpConnection<'_>>
    where
        F: FnMut(SocketAddr) -> io::Result<Option<TetherBinding>>,
    {
        // `UdpConnection` lowers this to the hot-path frame timeout. Restore
        // discovery polling whenever the host returns to searching.
        self.socket.set_read_timeout(Some(DISCOVERY_READ_TIMEOUT))?;
        let deadline = Instant::now() + timeout;
        let mut buffer = [0_u8; MAX_DATAGRAM_SIZE];
        loop {
            if Instant::now() >= deadline {
                return Err(discovery_timeout(self.port));
            }
            match self.socket.recv_from(&mut buffer) {
                Ok((count, peer)) => {
                    let Some(discovery) = decode_discovery(&buffer[..count]) else {
                        continue;
                    };
                    if discovery.kind != DISCOVERY_HELLO {
                        continue;
                    }
                    if !discovery_port_acceptable(discovery.port, self.port) {
                        continue;
                    }
                    let Some(binding) = classify_peer(peer)? else {
                        continue;
                    };
                    let acknowledgement = encode_discovery(
                        DISCOVERY_ACK,
                        discovery.nonce,
                        discovery.session_id,
                        self.port,
                    );
                    send_redundant(&self.socket, &acknowledgement, peer)?;
                    self.socket.set_read_timeout(Some(FRAME_READ_TIMEOUT))?;
                    return Ok(UdpConnection {
                        socket: &self.socket,
                        peer,
                        binding,
                        discovery_nonce: discovery.nonce,
                        discovery_session_id: discovery.session_id,
                        session_changed: false,
                        last_peer_activity: Instant::now(),
                    });
                }
                Err(error)
                    if error.kind() == io::ErrorKind::TimedOut
                        || error.kind() == io::ErrorKind::WouldBlock =>
                {
                    if Instant::now() >= deadline {
                        return Err(discovery_timeout(self.port));
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }
}

pub struct UdpConnection<'a> {
    socket: &'a UdpSocket,
    peer: SocketAddr,
    binding: TetherBinding,
    discovery_nonce: u64,
    discovery_session_id: u64,
    session_changed: bool,
    last_peer_activity: Instant,
}

impl UdpConnection<'_> {
    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    pub fn interface_index(&self) -> u32 {
        self.binding.interface_index()
    }

    pub fn tether_binding(&self) -> &TetherBinding {
        &self.binding
    }

    /// Reconfirm that Windows still routes this peer through the exact tether
    /// adapter accepted during discovery. Call this before exposing a newly
    /// discovered connection to the input loop; discovery and route-policy
    /// setup are separate system snapshots and the adapter can change between
    /// them.
    pub fn revalidate_peer(&self) -> io::Result<()> {
        self.binding.verify_peer(self.peer)
    }

    pub fn discovery_session_id(&self) -> u64 {
        self.discovery_session_id
    }

    pub fn take_session_changed(&mut self) -> bool {
        std::mem::take(&mut self.session_changed)
    }

    pub fn peer_activity_elapsed(&self) -> Duration {
        self.last_peer_activity.elapsed()
    }

    pub fn note_valid_peer_activity(&mut self) {
        self.last_peer_activity = Instant::now();
    }

    pub fn read(&mut self, buffer: &mut [u8], timeout_ms: u32) -> io::Result<usize> {
        let deadline = Instant::now() + Duration::from_millis(u64::from(timeout_ms.max(1)));
        loop {
            if Instant::now() >= deadline {
                return Ok(0);
            }
            match self.socket.recv_from(buffer) {
                Ok((count, peer)) if peer == self.peer => {
                    if buffer[..count].starts_with(&FRAME_MAGIC) {
                        if frame_session(&buffer[..count]) == Some(self.discovery_session_id) {
                            return Ok(count);
                        }
                        continue;
                    }
                    // The phone periodically repeats discovery so a host that
                    // was restarted can be found without restarting the app.
                    // Acknowledge it and keep the data path free for frames.
                    if let Some(discovery) = decode_discovery(&buffer[..count])
                        && discovery.kind == DISCOVERY_HELLO
                        && discovery_port_acceptable(
                            discovery.port,
                            self.socket.local_addr()?.port(),
                        )
                        && self.discovery_compatible(discovery.nonce, discovery.session_id)
                    {
                        self.binding.verify_peer(peer)?;
                        let acknowledgement = encode_discovery(
                            DISCOVERY_ACK,
                            discovery.nonce,
                            discovery.session_id,
                            self.socket.local_addr()?.port(),
                        );
                        send_redundant(self.socket, &acknowledgement, self.peer)?;
                        self.adopt_discovery(discovery.nonce, discovery.session_id);
                    }
                }
                Ok((count, peer)) => {
                    if let Some(discovery) = decode_discovery(&buffer[..count])
                        && discovery.kind == DISCOVERY_HELLO
                        && peer.ip() == self.peer.ip()
                        && discovery_port_acceptable(
                            discovery.port,
                            self.socket.local_addr()?.port(),
                        )
                        && self.discovery_compatible(discovery.nonce, discovery.session_id)
                    {
                        self.binding.verify_peer(peer)?;
                        let acknowledgement = encode_discovery(
                            DISCOVERY_ACK,
                            discovery.nonce,
                            discovery.session_id,
                            self.socket.local_addr()?.port(),
                        );
                        send_redundant(self.socket, &acknowledgement, peer)?;
                        self.peer = peer;
                        self.adopt_discovery(discovery.nonce, discovery.session_id);
                    }
                }
                Err(error)
                    if error.kind() == io::ErrorKind::TimedOut
                        || error.kind() == io::ErrorKind::WouldBlock =>
                {
                    return Ok(0);
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub fn write(&mut self, bytes: &[u8], _timeout_ms: u32) -> io::Result<()> {
        send_redundant(self.socket, bytes, self.peer)
    }

    fn adopt_discovery(&mut self, nonce: u64, session_id: u64) {
        if session_id != self.discovery_session_id {
            self.discovery_session_id = session_id;
            self.session_changed = true;
        }
        self.discovery_nonce = nonce;
        self.last_peer_activity = Instant::now();
    }

    fn discovery_compatible(&self, nonce: u64, session_id: u64) -> bool {
        session_id != self.discovery_session_id || nonce == self.discovery_nonce
    }
}

fn frame_session(bytes: &[u8]) -> Option<u64> {
    let session = bytes.get(8..16)?;
    Some(u64::from_le_bytes(session.try_into().ok()?))
}

fn discovery_timeout(port: u16) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!("no USB-tethered phone discovered on UDP port {port}"),
    )
}

fn send_redundant(socket: &UdpSocket, bytes: &[u8], peer: SocketAddr) -> io::Result<()> {
    let mut first_error = None;
    let mut delivered = false;
    for _ in 0..REDUNDANT_SENDS {
        match socket.send_to(bytes, peer) {
            Ok(sent) if sent == bytes.len() => delivered = true,
            Ok(sent) if first_error.is_none() => {
                first_error = Some(io::Error::new(
                    io::ErrorKind::WriteZero,
                    format!("short UDP datagram write: {sent}/{}", bytes.len()),
                ));
            }
            Ok(_) => {}
            Err(error) if first_error.is_none() => first_error = Some(error),
            Err(_) => {}
        }
    }
    if delivered {
        Ok(())
    } else {
        Err(first_error.unwrap_or_else(|| io::Error::other("UDP datagram write failed")))
    }
}

fn set_write_timeout(socket: &UdpSocket, timeout: Duration) -> io::Result<()> {
    let timeout_ms = timeout.as_millis().clamp(1, u128::from(u32::MAX)) as u32;
    let result = unsafe {
        setsockopt(
            socket.as_raw_socket() as SOCKET,
            SOL_SOCKET,
            SO_SNDTIMEO,
            (&timeout_ms as *const u32).cast(),
            size_of::<u32>() as i32,
        )
    };
    if result == SOCKET_ERROR {
        Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        ACTION_CANCEL, DISCOVERY_HELLO, FRAME_FLAG_SESSION_START, MESSAGE_TOUCH_FRAME,
        PROTOCOL_VERSION, crc32, decode_discovery, decode_frame, encode_discovery,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    #[test]
    fn discovers_a_phone_and_returns_the_ack() {
        let host = UdpHost::bind(0).unwrap();
        let phone = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        let phone_address = phone.local_addr().unwrap();
        phone
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        let destination = format!("127.0.0.1:{}", host.port());
        let hello = encode_discovery(DISCOVERY_HELLO, 11, 22, host.port());
        phone.send_to(&hello, destination).unwrap();

        let connection = host
            .connect_if(Duration::from_secs(1), |_| {
                Ok(Some(TetherBinding::for_test(7)))
            })
            .unwrap();
        for _ in 0..REDUNDANT_SENDS {
            let mut response = [0_u8; 32];
            let (count, _peer) = phone.recv_from(&mut response).unwrap();
            let message = decode_discovery(&response[..count]).unwrap();
            assert_eq!(message.nonce, 11);
            assert_eq!(message.session_id, 22);
            assert_eq!(message.port, host.port());
        }
        assert_eq!(connection.peer(), phone_address);
        assert_eq!(connection.interface_index(), 7);
    }

    #[test]
    fn unrelated_datagram_flood_cannot_hold_the_read_loop_past_its_deadline() {
        let host = UdpHost::bind(0).unwrap();
        let phone = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        phone
            .send_to(
                &encode_discovery(DISCOVERY_HELLO, 11, 22, host.port()),
                ("127.0.0.1", host.port()),
            )
            .unwrap();
        let mut connection = host
            .connect_if(Duration::from_secs(1), |_| {
                Ok(Some(TetherBinding::for_test(7)))
            })
            .unwrap();

        let noise = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let sender_stop = Arc::clone(&stop);
        let destination = ("127.0.0.1", host.port());
        let sender = thread::spawn(move || {
            while !sender_stop.load(Ordering::Relaxed) {
                let _ = noise.send_to(b"not a Holodori datagram", destination);
            }
        });

        let started = Instant::now();
        let mut buffer = [0_u8; MAX_DATAGRAM_SIZE];
        let result = connection.read(&mut buffer, 4).unwrap();
        stop.store(true, Ordering::Relaxed);
        sender.join().unwrap();

        assert_eq!(result, 0);
        assert!(started.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn non_tether_hello_is_ignored_without_an_ack() {
        let host = UdpHost::bind(0).unwrap();
        let phone = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        phone
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        phone
            .send_to(
                &encode_discovery(DISCOVERY_HELLO, 11, 22, host.port()),
                ("127.0.0.1", host.port()),
            )
            .unwrap();

        let error = match host.connect_if(Duration::from_millis(20), |_| Ok(None)) {
            Ok(_) => panic!("non-tether hello unexpectedly connected"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        let mut response = [0_u8; 32];
        assert!(phone.recv_from(&mut response).is_err());
    }

    #[test]
    fn foreign_hello_does_not_interrupt_the_pinned_peer() {
        let host = UdpHost::bind(0).unwrap();
        let phone = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        phone
            .send_to(
                &encode_discovery(DISCOVERY_HELLO, 11, 22, host.port()),
                ("127.0.0.1", host.port()),
            )
            .unwrap();
        let mut connection = host
            .connect_if(Duration::from_secs(1), |_| {
                Ok(Some(TetherBinding::for_test(7)))
            })
            .unwrap();
        drain_discovery_acks(&phone);

        let foreign = UdpSocket::bind(("127.0.0.2", 0)).unwrap();
        foreign
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        foreign
            .send_to(
                &encode_discovery(DISCOVERY_HELLO, 99, 100, host.port()),
                ("127.0.0.1", host.port()),
            )
            .unwrap();
        let frame = frame_prefix(22);
        phone.send_to(&frame, ("127.0.0.1", host.port())).unwrap();

        let mut buffer = [0_u8; 64];
        assert_eq!(connection.read(&mut buffer, 100).unwrap(), frame.len());
        assert_eq!(connection.peer(), phone.local_addr().unwrap());
        let mut response = [0_u8; 32];
        assert!(foreign.recv_from(&mut response).is_err());
    }

    #[test]
    fn same_ip_new_port_is_adopted_without_consuming_the_next_frame() {
        let host = UdpHost::bind(0).unwrap();
        let first = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        first
            .send_to(
                &encode_discovery(DISCOVERY_HELLO, 11, 22, host.port()),
                ("127.0.0.1", host.port()),
            )
            .unwrap();
        let mut connection = host
            .connect_if(Duration::from_secs(1), |_| {
                Ok(Some(TetherBinding::for_test(7)))
            })
            .unwrap();
        drain_discovery_acks(&first);

        let replacement = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        replacement
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        replacement
            .send_to(
                &encode_discovery(DISCOVERY_HELLO, 33, 44, host.port()),
                ("127.0.0.1", host.port()),
            )
            .unwrap();
        let frame = frame_prefix(44);
        replacement
            .send_to(&frame, ("127.0.0.1", host.port()))
            .unwrap();

        let mut buffer = [0_u8; 64];
        assert_eq!(connection.read(&mut buffer, 100).unwrap(), frame.len());
        assert_eq!(connection.peer(), replacement.local_addr().unwrap());
        assert_eq!(connection.discovery_session_id(), 44);
        assert!(connection.take_session_changed());
        assert!(!connection.take_session_changed());
        for _ in 0..REDUNDANT_SENDS {
            let mut response = [0_u8; 32];
            let (count, _) = replacement.recv_from(&mut response).unwrap();
            assert_eq!(decode_discovery(&response[..count]).unwrap().session_id, 44,);
        }
    }

    #[test]
    fn same_session_port_migration_preserves_session_state() {
        let host = UdpHost::bind(0).unwrap();
        let first = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        first
            .send_to(
                &encode_discovery(DISCOVERY_HELLO, 11, 22, host.port()),
                ("127.0.0.1", host.port()),
            )
            .unwrap();
        let mut connection = host
            .connect_if(Duration::from_secs(1), |_| {
                Ok(Some(TetherBinding::for_test(7)))
            })
            .unwrap();
        drain_discovery_acks(&first);

        let replacement = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        replacement
            .send_to(
                &encode_discovery(DISCOVERY_HELLO, 11, 22, host.port()),
                ("127.0.0.1", host.port()),
            )
            .unwrap();
        replacement
            .send_to(&frame_prefix(22), ("127.0.0.1", host.port()))
            .unwrap();

        let mut buffer = [0_u8; 64];
        assert_eq!(connection.read(&mut buffer, 100).unwrap(), 16);
        assert_eq!(connection.peer(), replacement.local_addr().unwrap());
        assert!(!connection.take_session_changed());
    }

    #[test]
    #[ignore = "local loopback timing contract; run explicitly for release validation"]
    fn loopback_fault_recovery_stays_inside_one_120_hz_frame() {
        let corrupt_then_redundant = measure_loopback_delivery(true, false);
        let one_copy_lost = measure_loopback_delivery(false, false);
        let both_immediate_copies_lost = measure_loopback_delivery(false, true);
        let budget = Duration::from_nanos(8_333_000);

        println!(
            "corrupt+redundant={corrupt_then_redundant:?}, one-copy-loss={one_copy_lost:?}, both-copy-loss={both_immediate_copies_lost:?}"
        );
        assert!(corrupt_then_redundant <= budget);
        assert!(one_copy_lost <= budget);
        assert!(both_immediate_copies_lost <= budget);
    }

    fn measure_loopback_delivery(corrupt_first_copy: bool, replay_after_two_ms: bool) -> Duration {
        let session_id = 0x1234_5678;
        let host = UdpHost::bind(0).unwrap();
        let phone = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        phone
            .send_to(
                &encode_discovery(DISCOVERY_HELLO, 11, session_id, host.port()),
                ("127.0.0.1", host.port()),
            )
            .unwrap();
        let mut connection = host
            .connect_if(Duration::from_secs(1), |_| {
                Ok(Some(TetherBinding::for_test(7)))
            })
            .unwrap();
        drain_discovery_acks(&phone);

        let frame = valid_loopback_frame(session_id);
        let started = Instant::now();
        if replay_after_two_ms {
            while started.elapsed() < Duration::from_millis(2) {
                std::hint::spin_loop();
            }
            phone.send_to(&frame, ("127.0.0.1", host.port())).unwrap();
        } else {
            if corrupt_first_copy {
                let mut corrupt = frame.clone();
                corrupt[64] ^= 1;
                phone.send_to(&corrupt, ("127.0.0.1", host.port())).unwrap();
            }
            phone.send_to(&frame, ("127.0.0.1", host.port())).unwrap();
        }

        let mut buffer = [0_u8; 128];
        loop {
            let count = connection.read(&mut buffer, 20).unwrap();
            assert_ne!(count, 0, "loopback frame timed out");
            if decode_frame(&buffer[..count]).is_ok() {
                return started.elapsed();
            }
        }
    }

    fn valid_loopback_frame(session_id: u64) -> Vec<u8> {
        const LENGTH: usize = 72;
        let mut frame = vec![0_u8; LENGTH];
        frame[..4].copy_from_slice(&FRAME_MAGIC);
        frame[4] = PROTOCOL_VERSION;
        frame[5] = MESSAGE_TOUCH_FRAME;
        frame[6..8].copy_from_slice(&(LENGTH as u16).to_le_bytes());
        frame[8..16].copy_from_slice(&session_id.to_le_bytes());
        frame[64] = ACTION_CANCEL;
        frame[67] = FRAME_FLAG_SESSION_START;
        let checksum = crc32(&frame[..LENGTH - 4]);
        frame[LENGTH - 4..].copy_from_slice(&checksum.to_le_bytes());
        frame
    }

    fn drain_discovery_acks(phone: &UdpSocket) {
        phone
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        for _ in 0..REDUNDANT_SENDS {
            let mut response = [0_u8; 32];
            phone.recv_from(&mut response).unwrap();
        }
    }

    fn frame_prefix(session_id: u64) -> [u8; 16] {
        let mut frame = [0_u8; 16];
        frame[..4].copy_from_slice(&FRAME_MAGIC);
        frame[8..16].copy_from_slice(&session_id.to_le_bytes());
        frame
    }
}
