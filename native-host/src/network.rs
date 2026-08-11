use crate::protocol::{
    DISCOVERY_ACK, DISCOVERY_HELLO, FRAME_MAGIC, decode_discovery, encode_discovery,
};
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
                    let acknowledgement =
                        encode_discovery(DISCOVERY_ACK, discovery.nonce, discovery.session_id);
                    send_redundant(&self.socket, &acknowledgement, peer)?;
                    self.socket.set_read_timeout(Some(FRAME_READ_TIMEOUT))?;
                    return Ok(UdpConnection {
                        socket: &self.socket,
                        peer,
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
}

impl UdpConnection<'_> {
    pub fn peer(&self) -> SocketAddr {
        self.peer
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
                        return Ok(count);
                    }
                    // The phone periodically repeats discovery so a host that
                    // was restarted can be found without restarting the app.
                    // Acknowledge it and keep the data path free for frames.
                    if let Some(discovery) = decode_discovery(&buffer[..count])
                        && discovery.kind == DISCOVERY_HELLO
                    {
                        let acknowledgement =
                            encode_discovery(DISCOVERY_ACK, discovery.nonce, discovery.session_id);
                        send_redundant(self.socket, &acknowledgement, self.peer)?;
                    }
                }
                Ok((count, peer)) => {
                    if let Some(discovery) = decode_discovery(&buffer[..count])
                        && discovery.kind == DISCOVERY_HELLO
                    {
                        let acknowledgement =
                            encode_discovery(DISCOVERY_ACK, discovery.nonce, discovery.session_id);
                        send_redundant(self.socket, &acknowledgement, peer)?;
                        return Err(io::Error::new(
                            io::ErrorKind::ConnectionAborted,
                            "phone UDP source changed; accepting a fresh discovery",
                        ));
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
    use crate::protocol::{DISCOVERY_HELLO, decode_discovery, encode_discovery};
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
        let hello = encode_discovery(DISCOVERY_HELLO, 11, 22);
        phone.send_to(&hello, destination).unwrap();

        let connection = host.connect(Duration::from_secs(1)).unwrap();
        for _ in 0..REDUNDANT_SENDS {
            let mut response = [0_u8; 32];
            let (count, _peer) = phone.recv_from(&mut response).unwrap();
            let message = decode_discovery(&response[..count]).unwrap();
            assert_eq!(message.nonce, 11);
            assert_eq!(message.session_id, 22);
        }
        assert_eq!(connection.peer(), phone_address);
    }

    #[test]
    fn unrelated_datagram_flood_cannot_hold_the_read_loop_past_its_deadline() {
        let host = UdpHost::bind(0).unwrap();
        let phone = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        phone
            .send_to(
                &encode_discovery(DISCOVERY_HELLO, 11, 22),
                ("127.0.0.1", host.port()),
            )
            .unwrap();
        let mut connection = host.connect(Duration::from_secs(1)).unwrap();

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
}
