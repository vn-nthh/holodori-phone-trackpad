use crate::protocol::{
    DISCOVERY_ACK, DISCOVERY_HELLO, FRAME_MAGIC, decode_discovery, encode_discovery,
};
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

pub const DEFAULT_UDP_PORT: u16 = 42_825;
const DISCOVERY_READ_TIMEOUT: Duration = Duration::from_millis(250);
const FRAME_READ_TIMEOUT: Duration = Duration::from_millis(4);
const MAX_DATAGRAM_SIZE: usize = 2_048;

pub struct UdpHost {
    socket: UdpSocket,
    port: u16,
}

impl UdpHost {
    pub fn bind(port: u16) -> io::Result<Self> {
        let socket = UdpSocket::bind(("0.0.0.0", port))?;
        let port = socket.local_addr()?.port();
        socket.set_broadcast(true)?;
        socket.set_read_timeout(Some(DISCOVERY_READ_TIMEOUT))?;
        Ok(Self { socket, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn connect(&self, timeout: Duration) -> io::Result<UdpConnection<'_>> {
        let deadline = Instant::now() + timeout;
        let mut buffer = [0_u8; MAX_DATAGRAM_SIZE];
        loop {
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
                    self.socket.send_to(&acknowledgement, peer)?;
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
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("no USB-tethered phone discovered on UDP port {}", self.port),
                        ));
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{DISCOVERY_HELLO, decode_discovery, encode_discovery};

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
        let mut response = [0_u8; 32];
        let (count, _peer) = phone.recv_from(&mut response).unwrap();
        let message = decode_discovery(&response[..count]).unwrap();
        assert_eq!(message.nonce, 11);
        assert_eq!(message.session_id, 22);
        assert_eq!(connection.peer(), phone_address);
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

    pub fn read(&mut self, buffer: &mut [u8], _timeout_ms: u32) -> io::Result<usize> {
        loop {
            match self.socket.recv_from(buffer) {
                Ok((count, peer)) if peer == self.peer => {
                    if buffer[..count].starts_with(&FRAME_MAGIC) {
                        return Ok(count);
                    }
                    // The phone periodically repeats discovery so a host that
                    // was restarted can be found without restarting the app.
                    // Acknowledge it and keep the data path free for frames.
                    if let Some(discovery) = decode_discovery(&buffer[..count]) {
                        if discovery.kind == DISCOVERY_HELLO {
                            let acknowledgement = encode_discovery(
                                DISCOVERY_ACK,
                                discovery.nonce,
                                discovery.session_id,
                            );
                            self.socket.send_to(&acknowledgement, self.peer)?;
                        }
                    }
                }
                Ok((count, peer)) => {
                    if let Some(discovery) = decode_discovery(&buffer[..count]) {
                        if discovery.kind == DISCOVERY_HELLO {
                            let acknowledgement = encode_discovery(
                                DISCOVERY_ACK,
                                discovery.nonce,
                                discovery.session_id,
                            );
                            self.socket.send_to(&acknowledgement, peer)?;
                            return Err(io::Error::new(
                                io::ErrorKind::ConnectionAborted,
                                "phone UDP source changed; accepting a fresh discovery",
                            ));
                        }
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
        let sent = self.socket.send_to(bytes, self.peer)?;
        if sent == bytes.len() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!("short UDP datagram write: {sent}/{}", bytes.len()),
            ))
        }
    }
}
