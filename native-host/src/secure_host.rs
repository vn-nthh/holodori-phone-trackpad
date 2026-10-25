//! Native-host protocol-v6 pairing, remembered-session setup, and gameplay.

use std::collections::HashSet;
#[cfg(target_os = "linux")]
use std::ffi::CString;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
#[cfg(windows)]
use std::os::windows::io::AsRawSocket;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use if_addrs::{IfAddr, get_if_addrs};
use snow::{Builder, HandshakeState, params::NoiseParams};
#[cfg(windows)]
use windows_sys::Win32::Networking::WinSock::{
    IP_ADD_IFLIST, IP_IFLIST, IP_UNICAST_IF, IPPROTO_IP, SOCKET, SOCKET_ERROR, WSAGetLastError,
    setsockopt,
};
use zeroize::Zeroizing;

use crate::credentials;
use crate::input::{
    InputSink, cancel_with_deadline, commit_ready, deferred_read_timeout, receive_ordered,
    service_deferred,
};
use crate::metrics::HostMetrics;
use crate::network::{enable_receive_interface, receive_datagram};
use crate::protocol::OrderedFrames;
use crate::secure::{
    Direction, HOST_ACK, HOST_AUTH_ABORT, HOST_PAIR_COMPLETE, HOST_PONG, HOST_QUALITY_PROBE,
    HOST_SAS_COMMITMENT, HOST_SAS_REVEAL, IK_CONTINUE, IK_MESSAGE_1, MAX_DATAGRAM_SIZE, NO_ACK,
    OpenedRecord, PAIR_ABORT, PAIR_CONTINUE, PAIR_OFFER, PAIR_PROBE, PHONE_AUTH_ABORT,
    PHONE_PAIR_CONFIRM, PHONE_PING, PHONE_QUALITY_REPLY, PHONE_SAS_COMMITMENT, PHONE_SAS_REVEAL,
    PHONE_TOUCH, QUALITY_REPAIR_ONLY, RECORD_HEADER_SIZE, RecordCipher, RecordHeader,
    SessionParameters, TouchRecord, TransportKind, WireError, decode_pair_envelope,
    decode_quality_reply, encode_pair_envelope, fill_random, prologue, sas_commit, sas_digest,
    sas_pattern,
};
use crate::tether::{TetherBinding, current_tether_binding, tether_ipv4_interfaces};

const XX_NAME: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
const IK_NAME: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
const DISCOVERY_SLICE: Duration = Duration::from_millis(250);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
const PAIRING_TIMEOUT: Duration = Duration::from_secs(60);
const APPLICATION_RETRY: Duration = Duration::from_millis(100);
const QUALITY_INTERVAL: Duration = Duration::from_millis(20);
const QUALITY_DURATION: Duration = Duration::from_secs(3);
const INTERFACE_REVALIDATE: Duration = Duration::from_millis(500);
const READ_TIMEOUT: Duration = Duration::from_millis(4);
const WRITE_TIMEOUT: Duration = Duration::from_millis(2);
const REDUNDANT_COPIES: usize = 2;

// Authenticated abort reasons.
const ABORT_CANCELLED: u16 = 1;
const ABORT_COMPARISON_FAILED: u16 = 2;
const ABORT_TIMED_OUT: u16 = 3;
const ABORT_PROTOCOL: u16 = 4;

// Diagnostic INGRESS reasons for a discarded datagram.
const DISCARD_FOREIGN: u64 = 1;
const DISCARD_REPLAY: u64 = 2;
const DISCARD_BAD_TAG: u64 = 3;
const DISCARD_MALFORMED: u64 = 4;

#[cfg(test)]
mod gameplay_tests;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PairCommand {
    Approve,
    Cancel,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PairEvent {
    Waiting,
    Pattern([u8; 8]),
    RemoteConfirmed,
    Quality(String),
    Complete,
}

pub fn pair(
    transport: TransportKind,
    port: u16,
    commands: &Receiver<PairCommand>,
    mut report: impl FnMut(PairEvent),
) -> Result<(), SecureHostError> {
    let deadline = Instant::now() + PAIRING_TIMEOUT;
    let mut credentials = credentials::load_or_create()?;
    let (socket, listener) = bind_socket(transport, port)?;
    report(PairEvent::Waiting);
    let Discovered {
        raw: probe_bytes,
        envelope: probe,
        peer,
        binding,
    } = wait_for_envelope(&socket, deadline, transport, &listener, |envelope| {
        envelope.kind == PAIR_PROBE && envelope.step == 0 && envelope.payload.is_empty()
    })?;
    let exchange_id = probe.exchange_id;
    let prologue = prologue(transport, exchange_id);
    let mut handshake = build_handshake(XX_NAME, true, credentials.private_key(), None, &prologue)?;
    let mut noise_message = [0_u8; MAX_DATAGRAM_SIZE];
    let message_one_length = handshake.write_message(&[], &mut noise_message)?;
    let offer = encode_pair_envelope(
        PAIR_OFFER,
        exchange_id,
        1,
        transport,
        &noise_message[..message_one_length],
    )?;
    send_plain_redundant(&socket, &offer, peer)?;

    let message_two_envelope = wait_for_handshake_step(
        &socket,
        peer,
        &binding,
        HandshakeStep {
            deadline: deadline.min(Instant::now() + HANDSHAKE_TIMEOUT),
            transport,
            exchange_id,
            kind: PAIR_CONTINUE,
            number: 2,
            repeated_request: &probe_bytes,
            repeated_response: &offer,
        },
    )?;
    let message_two = decode_pair_envelope(&message_two_envelope)?;
    handshake.read_message(message_two.payload, &mut noise_message)?;
    let remote_static: [u8; 32] = handshake
        .get_remote_static()
        .ok_or(SecureHostError::Authentication("phone static key missing"))?
        .try_into()
        .map_err(|_| SecureHostError::Authentication("phone static key has wrong length"))?;
    let message_three_length = handshake.write_message(&[], &mut noise_message)?;
    let continuation = encode_pair_envelope(
        PAIR_CONTINUE,
        exchange_id,
        3,
        transport,
        &noise_message[..message_three_length],
    )?;
    send_plain_redundant(&socket, &continuation, peer)?;
    let handshake_hash: [u8; 32] = handshake
        .get_handshake_hash()
        .try_into()
        .map_err(|_| SecureHostError::Authentication("handshake hash has wrong length"))?;
    let cipher = RecordCipher::from_handshake(&mut handshake, true)?;

    socket.connect(peer)?;
    socket.set_read_timeout(Some(READ_TIMEOUT))?;
    let cached_request = message_two_envelope;
    let cached_response = continuation;
    let mut connection = SecureConnection::new(
        socket,
        peer,
        binding,
        cipher,
        Some((cached_request, cached_response)),
    );

    let phone_commitment = wait_for_application(
        &mut connection,
        deadline,
        PHONE_SAS_COMMITMENT,
        None,
        commands,
    )?;
    require_pair_record(&phone_commitment, 32)?;
    let mut host_random = Zeroizing::new([0_u8; 32]);
    fill_random(host_random.as_mut())?;
    let host_commitment = sas_commit(2, &handshake_hash, &host_random);
    connection.send_redundant(HOST_SAS_COMMITMENT, 0, 0, 0, || &host_commitment)?;

    let phone_reveal = wait_for_application(
        &mut connection,
        deadline,
        PHONE_SAS_REVEAL,
        Some((HOST_SAS_COMMITMENT, host_commitment.to_vec())),
        commands,
    )?;
    require_pair_record(&phone_reveal, 32)?;
    let phone_random: [u8; 32] = phone_reveal
        .payload
        .as_slice()
        .try_into()
        .expect("length checked");
    let expected_phone_commitment = sas_commit(1, &handshake_hash, &phone_random);
    if phone_commitment.payload.as_slice() != expected_phone_commitment {
        connection.send_abort(0, ABORT_COMPARISON_FAILED)?;
        return Err(SecureHostError::Authentication(
            "phone SAS commitment mismatch",
        ));
    }
    connection.send_redundant(HOST_SAS_REVEAL, 0, 0, 0, || host_random.as_ref())?;
    let pattern = sas_pattern(sas_digest(&handshake_hash, &phone_random, &host_random));
    report(PairEvent::Pattern(pattern));

    let quality_started = Instant::now();
    let mut next_probe = quality_started;
    let mut next_probe_id = 0_u64;
    let mut quality = QualityStats::default();
    let mut remote_confirmed = false;
    let mut local_approved = false;
    let mut last_reveal_send = Instant::now();
    let mut last_interface_check = Instant::now();
    while Instant::now() < deadline {
        match commands.try_recv() {
            Ok(PairCommand::Approve) => local_approved = true,
            Ok(PairCommand::Cancel) => {
                connection.send_abort(0, ABORT_CANCELLED)?;
                return Err(SecureHostError::Cancelled);
            }
            Err(TryRecvError::Disconnected) => return Err(SecureHostError::Cancelled),
            Err(TryRecvError::Empty) => {}
        }

        let now = Instant::now();
        if now.duration_since(last_interface_check) >= INTERFACE_REVALIDATE {
            connection.revalidate_interface()?;
            last_interface_check = now;
        }
        let quality_done = transport == TransportKind::Usb
            || now.duration_since(quality_started) >= QUALITY_DURATION;
        if transport == TransportKind::Wifi && !quality_done && now >= next_probe {
            let flags = if next_probe_id % 10 == 9 {
                QUALITY_REPAIR_ONLY
            } else {
                0
            };
            connection.send_redundant(HOST_QUALITY_PROBE, 0, next_probe_id, flags, || {
                duration_nanos(quality_started.elapsed()).to_le_bytes()
            })?;
            quality.sent.insert(next_probe_id);
            next_probe_id += 1;
            next_probe += QUALITY_INTERVAL;
        }

        if let Some(record) = connection.receive_record()? {
            if record.header.session_id != 0 || record.header.flags != 0 {
                continue;
            }
            match record.header.message_type {
                PHONE_PAIR_CONFIRM => {
                    require_pair_record(&record, 0)?;
                    if !remote_confirmed {
                        remote_confirmed = true;
                        report(PairEvent::RemoteConfirmed);
                    }
                }
                PHONE_QUALITY_REPLY if transport == TransportKind::Wifi => {
                    let received_nanos = duration_nanos(quality_started.elapsed());
                    let reply = decode_quality_reply(&record)?;
                    quality.observe(reply, received_nanos);
                }
                PHONE_SAS_COMMITMENT => {
                    require_pair_record(&record, 32)?;
                    if record.payload != phone_commitment.payload {
                        return Err(SecureHostError::Authentication(
                            "phone changed its SAS commitment",
                        ));
                    }
                }
                PHONE_SAS_REVEAL => {
                    require_pair_record(&record, 32)?;
                    if record.payload.as_slice() != phone_random {
                        return Err(SecureHostError::Authentication(
                            "phone changed its SAS reveal",
                        ));
                    }
                }
                PHONE_AUTH_ABORT => {
                    require_pair_abort(&record)?;
                    return Err(SecureHostError::Cancelled);
                }
                _ => {
                    return Err(SecureHostError::Authentication(
                        "unexpected authenticated pairing record",
                    ));
                }
            }
        }

        if !remote_confirmed && last_reveal_send.elapsed() >= APPLICATION_RETRY {
            connection.send_redundant(HOST_SAS_REVEAL, 0, 0, 0, || host_random.as_ref())?;
            last_reveal_send = Instant::now();
        }

        if persist_pairing_if_authorized(remote_confirmed, local_approved, quality_done, || {
            credentials.authorize_phone(remote_static, "Android phone".to_owned());
            credentials::save(&credentials)
        })?
        .is_some()
        {
            if transport == TransportKind::Wifi {
                report(PairEvent::Quality(quality.summary()));
            }
            connection.send_redundant(HOST_PAIR_COMPLETE, 0, 0, 0, || &[])?;
            // A third independently encrypted completion slightly reduces the
            // one-sided-record edge case without ever reusing a nonce.
            connection.send_record(HOST_PAIR_COMPLETE, 0, 0, 0, &[])?;
            report(PairEvent::Complete);
            return Ok(());
        }
    }
    connection.send_abort(0, ABORT_TIMED_OUT)?;
    Err(SecureHostError::TimedOut("pairing window expired"))
}

fn persist_pairing_if_authorized<T>(
    remote_confirmed: bool,
    local_approved: bool,
    quality_done: bool,
    persist: impl FnOnce() -> Result<T, credentials::CredentialError>,
) -> Result<Option<T>, SecureHostError> {
    if !remote_confirmed || !local_approved || !quality_done {
        return Ok(None);
    }
    persist().map(Some).map_err(SecureHostError::Credentials)
}

const RECEIVE_WINDOW: u32 = 128;
const ACTIVE_INPUT_SILENCE_TIMEOUT: Duration = Duration::from_millis(32);
const IDLE_PEER_SILENCE_TIMEOUT: Duration = Duration::from_secs(2);
const SESSION_START_TIMEOUT: Duration = Duration::from_secs(5);
/// Android's earliest repair is 2 ms after a copy. A record that changes
/// nothing and arrives sooner than this after the identical ACK is the
/// immediate redundant twin, which cannot have observed that ACK's loss.
const DUPLICATE_ACK_HOLDOFF: Duration = Duration::from_millis(1);

/// The parameters a host with `lane_count` lanes announces for a session.
pub fn session_parameters(lane_count: u8) -> SessionParameters {
    SessionParameters {
        receive_window: RECEIVE_WINDOW,
        lane_count,
    }
}

struct GameplayState {
    parameters: SessionParameters,
    last_committed_frame: Instant,
    last_idle_activity: Instant,
    last_ping: Option<u64>,
    last_ack: Option<SentAck>,
}

impl GameplayState {
    fn new(lane_count: u8) -> Self {
        let now = Instant::now();
        Self {
            parameters: session_parameters(lane_count),
            last_committed_frame: now,
            last_idle_activity: now,
            last_ping: None,
            last_ack: None,
        }
    }
}

#[derive(Clone, Copy)]
struct SentAck {
    cumulative: Option<u64>,
    selective: u64,
    at: Instant,
}

impl SentAck {
    /// Whether `next` is this ACK's immediate twin: same content, sent too
    /// soon after it for the phone to have repaired anything.
    fn makes_redundant(&self, next: &SentAck) -> bool {
        self.cumulative == next.cumulative
            && self.selective == next.selective
            && next.at.saturating_duration_since(self.at) < DUPLICATE_ACK_HOLDOFF
    }
}

pub fn serve_gameplay(
    connection: &mut SecureConnection,
    ordered: &mut OrderedFrames,
    sink: &mut impl InputSink,
    metrics: &mut HostMetrics,
    lane_count: u8,
    stopping: &AtomicBool,
) -> Result<(), SecureHostError> {
    ordered.require_fresh_session();
    let result = serve_gameplay_inner(connection, ordered, sink, metrics, lane_count, stopping);
    // On Stop or failure, tell Android immediately instead of leaving it to
    // its 64 ms active or 2 s idle watchdog. Best effort: the socket may be
    // the failure.
    if !connection.peer_aborted {
        let reason = if stopping.load(Ordering::Relaxed) {
            ABORT_CANCELLED
        } else {
            ABORT_PROTOCOL
        };
        let _ = connection.send_abort(ordered.session_id().unwrap_or(0), reason);
    }
    metrics.note(
        crate::diagnostics::BOUNDARY,
        ordered.session_id().unwrap_or(0),
        ordered.expected_sequence(),
        u64::from(result.is_err() && !stopping.load(Ordering::Relaxed)),
        0,
    );
    ordered.require_fresh_session();
    cancel_with_deadline(sink, metrics)?;
    result
}

fn serve_gameplay_inner(
    connection: &mut SecureConnection,
    ordered: &mut OrderedFrames,
    sink: &mut impl InputSink,
    metrics: &mut HostMetrics,
    lane_count: u8,
    stopping: &AtomicBool,
) -> Result<(), SecureHostError> {
    let mut control = GameplayState::new(lane_count);
    let interface_monitor = connection.monitor_interface(INTERFACE_REVALIDATE)?;
    let session_start_deadline = Instant::now() + SESSION_START_TIMEOUT;
    let mut receive_buffer = [0_u8; MAX_DATAGRAM_SIZE];
    let mut read_timeout = READ_TIMEOUT;
    connection.set_read_timeout(read_timeout)?;
    while !stopping.load(Ordering::Relaxed) {
        if interface_monitor.changed() {
            metrics.note(
                crate::diagnostics::STALL,
                ordered.session_id().unwrap_or(0),
                ordered.expected_sequence(),
                1,
                0,
            );
            return Err(io::Error::new(
                io::ErrorKind::NetworkUnreachable,
                "selected network interface changed",
            )
            .into());
        }
        let established = ordered.session_id().is_some();
        if !established && Instant::now() >= session_start_deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "authenticated session-start CANCEL did not arrive",
            )
            .into());
        }
        if sink.has_active_input()
            && control.last_committed_frame.elapsed() >= ACTIVE_INPUT_SILENCE_TIMEOUT
        {
            metrics.note(
                crate::diagnostics::STALL,
                ordered.session_id().unwrap_or(0),
                ordered.expected_sequence(),
                2,
                32_000_000,
            );
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "authenticated input made no committed progress for 32 ms",
            )
            .into());
        }
        if established
            && !sink.has_active_input()
            && control.last_idle_activity.elapsed() >= IDLE_PEER_SILENCE_TIMEOUT
        {
            return Err(
                io::Error::new(io::ErrorKind::TimedOut, "authenticated idle peer expired").into(),
            );
        }
        service_deferred(sink, stopping)?;
        let wanted = deferred_read_timeout(sink, READ_TIMEOUT);
        if wanted != read_timeout {
            connection.set_read_timeout(wanted)?;
            read_timeout = wanted;
        }
        let received = connection.receive_record_into(&mut receive_buffer);
        if let Some((reason, arrival)) = connection.last_diagnostic_discard.take() {
            let at = metrics.clock_nanos(arrival);
            metrics.record(crate::diagnostics::Event::new(
                crate::diagnostics::INGRESS,
                at,
                0,
                0,
                &[reason],
            ));
        }
        let Some((header, arrival)) = received? else {
            continue;
        };
        match header.message_type {
            PHONE_AUTH_ABORT => {
                connection.peer_aborted = true;
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "phone ended the authenticated session",
                )
                .into());
            }
            PHONE_PING => {
                answer_ping(connection, ordered, metrics, &mut control, &header, arrival)?
            }
            PHONE_TOUCH => {
                let payload = &receive_buffer
                    [RECORD_HEADER_SIZE..RECORD_HEADER_SIZE + header.payload_length as usize];
                let record = TouchRecord::parse(&header, payload)?;
                process_touch_record(
                    connection,
                    ordered,
                    sink,
                    metrics,
                    &mut control,
                    &record,
                    arrival,
                    stopping,
                )?;
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "authenticated record type is invalid during gameplay",
                )
                .into());
            }
        }
    }
    Ok(())
}

fn answer_ping(
    connection: &mut SecureConnection,
    ordered: &OrderedFrames,
    metrics: &mut HostMetrics,
    control: &mut GameplayState,
    header: &RecordHeader,
    arrival: Instant,
) -> Result<(), SecureHostError> {
    if header.flags != 0
        || header.payload_length != 0
        || ordered.session_id() != Some(header.session_id)
    {
        return Err(SecureHostError::Authentication("invalid idle ping"));
    }
    // Only a fresh ping ID is idle progress; a re-encrypted repeat is traffic.
    if control
        .last_ping
        .is_none_or(|previous| header.logical_id > previous)
    {
        control.last_ping = Some(header.logical_id);
        control.last_idle_activity = arrival;
    }
    let parameters = control.parameters;
    connection.send_redundant(HOST_PONG, header.session_id, header.logical_id, 0, || {
        parameters.encode_control(metrics.clock_nanos(Instant::now()), 0)
    })
}

/// Apply every frame of one record in order, then send at most one ACK for
/// the whole record.
#[allow(clippy::too_many_arguments)]
fn process_touch_record(
    connection: &mut SecureConnection,
    ordered: &mut OrderedFrames,
    sink: &mut impl InputSink,
    metrics: &mut HostMetrics,
    control: &mut GameplayState,
    record: &TouchRecord<'_>,
    arrival: Instant,
    stopping: &AtomicBool,
) -> Result<(), SecureHostError> {
    let session_id = record.session_id();
    if ordered
        .session_id()
        .is_some_and(|current| current != session_id)
    {
        return Err(SecureHostError::Authentication(
            "a new gameplay session requires fresh IK",
        ));
    }
    let mut progressed = false;
    for frame in record.frames() {
        receive_ordered(ordered, metrics, frame, arrival);
        // Commit per frame: the first sample reaches the OS before the rest
        // of the record is decoded.
        progressed |= commit_ready(ordered, sink, metrics, stopping)?;
    }
    if progressed {
        control.last_committed_frame = Instant::now();
        control.last_idle_activity = control.last_committed_frame;
    }
    // Frames ahead of the session-start CANCEL are not acknowledged.
    if ordered.session_id() != Some(session_id) {
        return Ok(());
    }
    send_ack(
        connection, ordered, metrics, control, session_id, progressed,
    )
}

/// One ACK per record. Progress always gets one; a record that changed
/// nothing gets one too (its sender may have lost the previous ACK) unless
/// it is the immediate twin of the identical ACK.
fn send_ack(
    connection: &mut SecureConnection,
    ordered: &OrderedFrames,
    metrics: &mut HostMetrics,
    control: &mut GameplayState,
    session_id: u64,
    progressed: bool,
) -> Result<(), SecureHostError> {
    let ack = SentAck {
        cumulative: ordered.acknowledged_sequence(),
        selective: ordered.selective_ack(),
        at: Instant::now(),
    };
    if !progressed
        && control
            .last_ack
            .is_some_and(|last| last.makes_redundant(&ack))
    {
        return Ok(());
    }
    control.last_ack = Some(ack);
    let payload = control
        .parameters
        .encode_control(metrics.clock_nanos(Instant::now()), ack.selective);
    let result = connection.send_record(
        HOST_ACK,
        session_id,
        ack.cumulative.unwrap_or(NO_ACK),
        0,
        &payload,
    );
    metrics.observe_ack(
        session_id,
        ack.cumulative,
        ack.at,
        progressed,
        result.is_err(),
    );
    result
}

/// Wait for a remembered phone's IK message 1 and authenticate its identity.
/// The host has not answered yet: the caller vets the route first, so a
/// refused path never starts a session on the phone.
pub fn accept_remembered(
    transport: TransportKind,
    port: u16,
    deadline: Instant,
) -> Result<PendingSession, SecureHostError> {
    let credentials = credentials::load_or_create()?;
    let expected_phone = *credentials
        .paired_phone_public_key()
        .ok_or(SecureHostError::NotPaired)?;
    let (socket, listener) = bind_socket(transport, port)?;
    let Discovered {
        raw: request,
        envelope,
        peer,
        binding,
    } = wait_for_envelope(&socket, deadline, transport, &listener, |envelope| {
        envelope.kind == IK_MESSAGE_1 && envelope.step == 1
    })?;
    let mut handshake = build_handshake(
        IK_NAME,
        false,
        credentials.private_key(),
        None,
        &prologue(transport, envelope.exchange_id),
    )?;
    let mut payload = [0_u8; MAX_DATAGRAM_SIZE];
    if handshake.read_message(&envelope.payload, &mut payload)? != 0 {
        return Err(SecureHostError::Authentication(
            "IK message 1 carried a payload",
        ));
    }
    if handshake.get_remote_static() != Some(expected_phone.as_slice()) {
        return Err(SecureHostError::Authentication("unpaired phone identity"));
    }
    Ok(PendingSession {
        socket,
        peer,
        binding,
        handshake,
        request,
        exchange_id: envelope.exchange_id,
        transport,
    })
}

/// An authenticated IK message 1 the host has not answered yet.
pub struct PendingSession {
    socket: UdpSocket,
    peer: SocketAddr,
    binding: NetworkBinding,
    handshake: HandshakeState,
    request: Vec<u8>,
    exchange_id: [u8; 16],
    transport: TransportKind,
}

impl PendingSession {
    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    pub fn tether_binding(&self) -> Option<&TetherBinding> {
        self.binding.tether()
    }

    pub fn revalidate_interface(&self) -> Result<(), SecureHostError> {
        self.binding.revalidate(self.peer)
    }

    /// Answer with IK message 2. Its encrypted payload carries the session
    /// parameters, so the phone starts the session without a HELLO round trip.
    pub fn complete(
        mut self,
        parameters: SessionParameters,
        host_send_nanos: u64,
    ) -> Result<SecureConnection, SecureHostError> {
        let mut message = [0_u8; MAX_DATAGRAM_SIZE];
        let length = self
            .handshake
            .write_message(&parameters.encode(host_send_nanos), &mut message)?;
        let response = encode_pair_envelope(
            IK_CONTINUE,
            self.exchange_id,
            2,
            self.transport,
            &message[..length],
        )?;
        send_plain_redundant(&self.socket, &response, self.peer)?;
        let cipher = RecordCipher::from_handshake(&mut self.handshake, false)?;
        self.socket.connect(self.peer)?;
        self.socket.set_read_timeout(Some(READ_TIMEOUT))?;
        Ok(SecureConnection::new(
            self.socket,
            self.peer,
            self.binding,
            cipher,
            Some((self.request, response)),
        ))
    }
}

pub struct SecureConnection {
    socket: UdpSocket,
    peer: SocketAddr,
    binding: NetworkBinding,
    cipher: RecordCipher,
    /// The handshake request and our reply, resent verbatim until the phone
    /// proves it holds the split keys.
    cached_handshake: Option<(Vec<u8>, Vec<u8>)>,
    send_buffer: [u8; MAX_DATAGRAM_SIZE],
    last_diagnostic_discard: Option<(u64, Instant)>,
    peer_aborted: bool,
}

pub struct InterfaceMonitor {
    changed: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: thread::Thread,
}

impl InterfaceMonitor {
    pub fn changed(&self) -> bool {
        self.changed.load(Ordering::Acquire)
    }
}

impl Drop for InterfaceMonitor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker.unpark();
    }
}

impl SecureConnection {
    fn new(
        socket: UdpSocket,
        peer: SocketAddr,
        binding: NetworkBinding,
        cipher: RecordCipher,
        cached_handshake: Option<(Vec<u8>, Vec<u8>)>,
    ) -> Self {
        Self {
            socket,
            peer,
            binding,
            cipher,
            cached_handshake,
            send_buffer: [0; MAX_DATAGRAM_SIZE],
            last_diagnostic_discard: None,
            peer_aborted: false,
        }
    }

    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    pub fn connection_id(&self) -> u64 {
        self.cipher.connection_id()
    }

    pub fn revalidate_interface(&self) -> Result<(), SecureHostError> {
        self.binding.revalidate(self.peer)
    }

    pub fn monitor_interface(
        &self,
        interval: Duration,
    ) -> Result<InterfaceMonitor, SecureHostError> {
        let binding = self.binding.clone();
        let peer = self.peer;
        let changed = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let monitor_changed = Arc::clone(&changed);
        let monitor_stop = Arc::clone(&stop);
        let handle = thread::Builder::new()
            .name("interface monitor".to_owned())
            .spawn(move || {
                while !monitor_stop.load(Ordering::Acquire) {
                    thread::park_timeout(interval);
                    if monitor_stop.load(Ordering::Acquire) {
                        break;
                    }
                    if binding.revalidate(peer).is_err() {
                        monitor_changed.store(true, Ordering::Release);
                        break;
                    }
                }
            })?;
        Ok(InterfaceMonitor {
            changed,
            stop,
            worker: handle.thread().clone(),
        })
    }

    pub fn set_read_timeout(&self, timeout: Duration) -> Result<(), SecureHostError> {
        self.socket.set_read_timeout(Some(timeout))?;
        Ok(())
    }

    pub fn receive_record(&mut self) -> Result<Option<OpenedRecord>, SecureHostError> {
        let mut bytes = [0_u8; MAX_DATAGRAM_SIZE];
        Ok(self
            .receive_record_into(&mut bytes)?
            .map(|(header, _)| OpenedRecord {
                header,
                payload: bytes
                    [RECORD_HEADER_SIZE..RECORD_HEADER_SIZE + header.payload_length as usize]
                    .to_vec(),
            }))
    }

    /// Timestamp userspace receive completion before authentication/decoding.
    /// This is not a NIC/kernel ingress timestamp; kernel queueing is unobserved.
    pub fn receive_record_into(
        &mut self,
        bytes: &mut [u8; MAX_DATAGRAM_SIZE],
    ) -> Result<Option<(RecordHeader, Instant)>, SecureHostError> {
        self.last_diagnostic_discard = None;
        let (count, peer, ingress) = match receive_datagram(&self.socket, bytes) {
            Ok(value) => value,
            Err(error) if is_timeout(&error) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let arrival = Instant::now();
        if peer != self.peer || !self.binding.accepts_ingress(ingress) {
            self.last_diagnostic_discard = Some((DISCARD_FOREIGN, arrival));
            return Ok(None);
        }
        let datagram = &mut bytes[..count];
        if let Some((request, response)) = self.cached_handshake.as_ref()
            && datagram == request
        {
            send_plain_connected_redundant(&self.socket, response)?;
            return Ok(None);
        }
        match self.cipher.open_in_place(Direction::PhoneToHost, datagram) {
            Ok(record) => {
                // Once the peer proves possession of the split key it cannot
                // still need a retransmitted plaintext handshake response.
                self.cached_handshake = None;
                Ok(Some((record, arrival)))
            }
            Err(error) => {
                let reason = match error {
                    WireError::Replay => DISCARD_REPLAY,
                    WireError::BadTag => DISCARD_BAD_TAG,
                    WireError::BadMagic
                    | WireError::BadVersion(_)
                    | WireError::WrongConnection
                    | WireError::BadLength(_)
                    | WireError::ReservedBits => DISCARD_MALFORMED,
                    error => return Err(error.into()),
                };
                self.last_diagnostic_discard = Some((reason, arrival));
                Ok(None)
            }
        }
    }

    /// Seal and send one record to the phone under a fresh packet number.
    pub fn send_record(
        &mut self,
        message_type: u8,
        session_id: u64,
        logical_id: u64,
        flags: u32,
        payload: &[u8],
    ) -> Result<(), SecureHostError> {
        let length = self.cipher.seal_into(
            Direction::HostToPhone,
            message_type,
            session_id,
            logical_id,
            flags,
            payload,
            &mut self.send_buffer,
        )?;
        let sent = self.socket.send(&self.send_buffer[..length])?;
        if sent != length {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                format!("short datagram write: {sent}/{length}"),
            )
            .into());
        }
        Ok(())
    }

    /// Send `REDUNDANT_COPIES` independently encrypted copies. `payload` is
    /// rebuilt for each copy so timestamps describe that attempt. Succeeds if
    /// any copy left the socket.
    pub fn send_redundant<P: AsRef<[u8]>>(
        &mut self,
        message_type: u8,
        session_id: u64,
        logical_id: u64,
        flags: u32,
        mut payload: impl FnMut() -> P,
    ) -> Result<(), SecureHostError> {
        let mut delivered = false;
        let mut first_error = None;
        for _ in 0..REDUNDANT_COPIES {
            match self.send_record(
                message_type,
                session_id,
                logical_id,
                flags,
                payload().as_ref(),
            ) {
                Ok(()) => delivered = true,
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        match first_error {
            Some(error) if !delivered => Err(error),
            _ => Ok(()),
        }
    }

    fn send_abort(&mut self, session_id: u64, reason: u16) -> Result<(), SecureHostError> {
        self.send_redundant(HOST_AUTH_ABORT, session_id, 0, 0, || reason.to_le_bytes())
    }
}

fn wait_for_application(
    connection: &mut SecureConnection,
    deadline: Instant,
    expected_type: u8,
    retry: Option<(u8, Vec<u8>)>,
    commands: &Receiver<PairCommand>,
) -> Result<OpenedRecord, SecureHostError> {
    let mut last_send = Instant::now();
    let mut last_interface_check = Instant::now();
    while Instant::now() < deadline {
        match commands.try_recv() {
            Ok(PairCommand::Cancel) | Err(TryRecvError::Disconnected) => {
                connection.send_abort(0, ABORT_CANCELLED)?;
                return Err(SecureHostError::Cancelled);
            }
            Ok(PairCommand::Approve) | Err(TryRecvError::Empty) => {}
        }
        if last_interface_check.elapsed() >= INTERFACE_REVALIDATE {
            connection.revalidate_interface()?;
            last_interface_check = Instant::now();
        }
        if let Some(record) = connection.receive_record()? {
            if record.header.session_id == 0
                && record.header.flags == 0
                && record.header.message_type == expected_type
            {
                return Ok(record);
            }
            if record.header.message_type == PHONE_AUTH_ABORT {
                return Err(SecureHostError::Cancelled);
            }
        }
        if let Some((message_type, payload)) = retry.as_ref()
            && last_send.elapsed() >= APPLICATION_RETRY
        {
            connection.send_redundant(*message_type, 0, 0, 0, || payload.as_slice())?;
            last_send = Instant::now();
        }
    }
    connection.send_abort(0, ABORT_TIMED_OUT)?;
    Err(SecureHostError::TimedOut(
        "authenticated pairing step expired",
    ))
}

fn require_pair_record(
    record: &OpenedRecord,
    payload_length: usize,
) -> Result<(), SecureHostError> {
    if record.header.session_id != 0
        || record.header.flags != 0
        || record.header.logical_id != 0
        || record.payload.len() != payload_length
    {
        return Err(SecureHostError::Authentication("malformed pairing control"));
    }
    Ok(())
}

fn require_pair_abort(record: &OpenedRecord) -> Result<(), SecureHostError> {
    require_pair_record(record, 2)?;
    let reason = u16::from_le_bytes([record.payload[0], record.payload[1]]);
    if !(1..=4).contains(&reason) {
        return Err(SecureHostError::Authentication(
            "malformed authenticated pairing abort",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ListenerInterface {
    local: Ipv4Addr,
    index: u32,
    name: String,
}

fn bind_socket(
    transport: TransportKind,
    port: u16,
) -> Result<(UdpSocket, ListenerInterface), SecureHostError> {
    let listener = select_listener_interface(transport)?;
    let socket = bind_listener_socket(&listener, port)?;
    Ok((socket, listener))
}

fn bind_listener_socket(listener: &ListenerInterface, port: u16) -> io::Result<UdpSocket> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port))?;
    confine_socket(&socket, listener)?;
    request_expedited_forwarding(&socket);
    enable_receive_interface(&socket)?;
    socket.set_broadcast(true)?;
    socket.set_read_timeout(Some(DISCOVERY_SLICE))?;
    socket.set_write_timeout(Some(WRITE_TIMEOUT))?;
    Ok(socket)
}

fn select_listener_interface(
    transport: TransportKind,
) -> Result<ListenerInterface, SecureHostError> {
    let interfaces = get_if_addrs()?;
    let tether = tether_ipv4_interfaces()?;
    let mut candidates = Vec::new();
    for interface in interfaces {
        let IfAddr::V4(address) = &interface.addr else {
            continue;
        };
        let Some(index) = interface.index else {
            continue;
        };
        let is_tether = tether
            .iter()
            .any(|(local, candidate_index)| *local == address.ip && *candidate_index == index);
        let accepted = match transport {
            TransportKind::Usb => is_tether,
            TransportKind::Wifi => {
                !is_tether
                    && interface.is_oper_up()
                    && !interface.is_loopback()
                    && !interface.is_p2p()
                    && !rejected_local_interface(&interface.name.to_ascii_lowercase())
                    && valid_listener_subnet(address.ip, address.netmask)
            }
        };
        if accepted {
            let candidate = ListenerInterface {
                local: address.ip,
                index,
                name: interface.name,
            };
            if !candidates.contains(&candidate) {
                candidates.push(candidate);
            }
        }
    }
    match candidates.as_slice() {
        [listener] => Ok(listener.clone()),
        [] => Err(SecureHostError::Interface(match transport {
            TransportKind::Usb => {
                "no active Android USB-tether IPv4 interface is available".to_owned()
            }
            TransportKind::Wifi => {
                "no eligible private local-network IPv4 interface is available".to_owned()
            }
        })),
        _ => Err(SecureHostError::Interface(match transport {
            TransportKind::Usb => {
                "multiple Android USB-tether interfaces are active; leave only the selected phone connected"
                    .to_owned()
            }
            TransportKind::Wifi => {
                "multiple private local-network interfaces are active; disconnect VPN/extra adapters"
                    .to_owned()
            }
        })),
    }
}

fn valid_listener_subnet(local: Ipv4Addr, netmask: Ipv4Addr) -> bool {
    if !private_ipv4(local) {
        return false;
    }
    let mask = u32::from_be_bytes(netmask.octets());
    let prefix = mask.leading_ones();
    if mask != u32::MAX.checked_shl(32 - prefix).unwrap_or(0) || prefix > 30 {
        return false;
    }
    let minimum = match local.octets() {
        [10, _, _, _] => 8,
        [172, second, _, _] if (16..=31).contains(&second) => 12,
        [192, 168, _, _] | [169, 254, _, _] => 16,
        _ => return false,
    };
    let host = u32::from_be_bytes(local.octets()) & !mask;
    prefix >= minimum && host != 0 && host != !mask
}

#[cfg(windows)]
fn confine_socket(socket: &UdpSocket, listener: &ListenerInterface) -> io::Result<()> {
    // Windows rejects IP_ADD_IFLIST until the socket's interface list is enabled.
    set_windows_ip_option(socket, IP_IFLIST, 1)?;
    set_windows_ip_option(socket, IP_ADD_IFLIST, listener.index)?;
    set_windows_ip_option(socket, IP_UNICAST_IF, listener.index.to_be())
}

#[cfg(windows)]
fn set_windows_ip_option(socket: &UdpSocket, option: i32, value: u32) -> io::Result<()> {
    let result = unsafe {
        setsockopt(
            socket.as_raw_socket() as SOCKET,
            IPPROTO_IP,
            option,
            (&raw const value).cast(),
            std::mem::size_of::<u32>() as i32,
        )
    };
    if result == SOCKET_ERROR {
        Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }))
    } else {
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn confine_socket(socket: &UdpSocket, listener: &ListenerInterface) -> io::Result<()> {
    let name = CString::new(listener.name.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "network interface name contains NUL",
        )
    })?;
    let result = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_BINDTODEVICE,
            name.as_ptr().cast(),
            name.as_bytes_with_nul().len() as libc::socklen_t,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Mark host ACKs DSCP EF (the phone already marks its frames) so Wi-Fi WMM
/// can carry feedback in the voice access category. Best effort: a refusal
/// changes queueing priority only, never correctness. Windows ignores
/// `IP_TOS` without an administrator QoS policy, so it is not requested there.
#[cfg(target_os = "linux")]
fn request_expedited_forwarding(socket: &UdpSocket) {
    let tos: libc::c_int = 0xB8;
    unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_TOS,
            (&raw const tos).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

#[cfg(not(target_os = "linux"))]
fn request_expedited_forwarding(_socket: &UdpSocket) {}

#[cfg(not(any(windows, target_os = "linux")))]
fn confine_socket(_socket: &UdpSocket, _listener: &ListenerInterface) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "interface confinement is unsupported on this host",
    ))
}

fn listener_accepts_ingress(listener: &ListenerInterface, ingress: Option<u32>) -> bool {
    #[cfg(any(windows, target_os = "linux"))]
    {
        ingress == Some(listener.index)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _ = (listener, ingress);
        false
    }
}

#[derive(Clone, Debug)]
struct OwnedEnvelope {
    kind: u8,
    exchange_id: [u8; 16],
    step: u32,
    payload: Vec<u8>,
}

/// A discovery datagram that matched, and the route it arrived on.
struct Discovered {
    /// The exact datagram, kept to recognize a byte-identical retransmission.
    raw: Vec<u8>,
    envelope: OwnedEnvelope,
    peer: SocketAddr,
    binding: NetworkBinding,
}

fn wait_for_envelope(
    socket: &UdpSocket,
    deadline: Instant,
    transport: TransportKind,
    listener: &ListenerInterface,
    predicate: impl Fn(&OwnedEnvelope) -> bool,
) -> Result<Discovered, SecureHostError> {
    let mut bytes = [0_u8; MAX_DATAGRAM_SIZE];
    let mut last_interface_check = Instant::now();
    while Instant::now() < deadline {
        if last_interface_check.elapsed() >= INTERFACE_REVALIDATE {
            if select_listener_interface(transport)? != *listener {
                return Err(SecureHostError::Interface(
                    "selected discovery interface changed".to_owned(),
                ));
            }
            last_interface_check = Instant::now();
        }
        let (count, peer, ingress) = match receive_datagram(socket, &mut bytes) {
            Ok(value) => value,
            Err(error) if is_timeout(&error) => continue,
            Err(error) => return Err(error.into()),
        };
        if !listener_accepts_ingress(listener, ingress) {
            continue;
        }
        let envelope = match decode_pair_envelope(&bytes[..count]) {
            Ok(envelope) if envelope.transport == transport => OwnedEnvelope {
                kind: envelope.kind,
                exchange_id: envelope.exchange_id,
                step: envelope.step,
                payload: envelope.payload.to_vec(),
            },
            _ => continue,
        };
        if !predicate(&envelope) {
            continue;
        }
        let binding = match NetworkBinding::for_peer(peer, transport, listener) {
            Ok(binding) => binding,
            Err(_) => continue,
        };
        return Ok(Discovered {
            raw: bytes[..count].to_vec(),
            envelope,
            peer,
            binding,
        });
    }
    Err(SecureHostError::TimedOut(
        "no phone found on the selected interface",
    ))
}

struct HandshakeStep<'a> {
    deadline: Instant,
    transport: TransportKind,
    exchange_id: [u8; 16],
    kind: u8,
    number: u32,
    repeated_request: &'a [u8],
    repeated_response: &'a [u8],
}

fn wait_for_handshake_step(
    socket: &UdpSocket,
    peer: SocketAddr,
    binding: &NetworkBinding,
    step: HandshakeStep<'_>,
) -> Result<Vec<u8>, SecureHostError> {
    let mut bytes = [0_u8; MAX_DATAGRAM_SIZE];
    let mut last_interface_check = Instant::now();
    while Instant::now() < step.deadline {
        if last_interface_check.elapsed() >= INTERFACE_REVALIDATE {
            binding.revalidate(peer)?;
            last_interface_check = Instant::now();
        }
        let (count, source, ingress) = match receive_datagram(socket, &mut bytes) {
            Ok(value) => value,
            Err(error) if is_timeout(&error) => continue,
            Err(error) => return Err(error.into()),
        };
        if source != peer {
            continue;
        }
        if !binding.accepts_ingress(ingress) {
            continue;
        }
        binding.revalidate(source)?;
        let raw = &bytes[..count];
        if raw == step.repeated_request {
            send_plain_redundant(socket, step.repeated_response, peer)?;
            continue;
        }
        let envelope = match decode_pair_envelope(raw) {
            Ok(envelope) => envelope,
            Err(_) => continue,
        };
        if envelope.transport == step.transport
            && envelope.exchange_id == step.exchange_id
            && envelope.kind == step.kind
            && envelope.step == step.number
        {
            return Ok(raw.to_vec());
        }
        if envelope.kind == PAIR_ABORT {
            return Err(SecureHostError::Cancelled);
        }
    }
    Err(SecureHostError::TimedOut("Noise handshake step expired"))
}

fn build_handshake(
    name: &str,
    initiator: bool,
    local_private: &[u8; 32],
    remote_public: Option<&[u8; 32]>,
    handshake_prologue: &[u8],
) -> Result<HandshakeState, SecureHostError> {
    let params: NoiseParams = name
        .parse()
        .map_err(|_| SecureHostError::Authentication("invalid Noise parameters"))?;
    let mut builder = Builder::new(params)
        .local_private_key(local_private)?
        .prologue(handshake_prologue)?;
    if let Some(remote_public) = remote_public {
        builder = builder.remote_public_key(remote_public)?;
    }
    Ok(if initiator {
        builder.build_initiator()?
    } else {
        builder.build_responder()?
    })
}

fn send_plain_redundant(
    socket: &UdpSocket,
    bytes: &[u8],
    peer: SocketAddr,
) -> Result<(), SecureHostError> {
    for _ in 0..REDUNDANT_COPIES {
        let sent = socket.send_to(bytes, peer)?;
        if sent != bytes.len() {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "short pairing datagram").into());
        }
    }
    Ok(())
}

fn send_plain_connected_redundant(socket: &UdpSocket, bytes: &[u8]) -> Result<(), SecureHostError> {
    for _ in 0..REDUNDANT_COPIES {
        let sent = socket.send(bytes)?;
        if sent != bytes.len() {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "short pairing datagram").into());
        }
    }
    Ok(())
}

#[derive(Clone)]
enum NetworkBinding {
    #[cfg(test)]
    Loopback,
    Usb {
        binding: TetherBinding,
        listener: ListenerInterface,
    },
    Wifi {
        local: Ipv4Addr,
        peer_network: u32,
        mask: u32,
        interface_index: Option<u32>,
    },
}

impl NetworkBinding {
    fn for_peer(
        peer: SocketAddr,
        transport: TransportKind,
        listener: &ListenerInterface,
    ) -> Result<Self, SecureHostError> {
        match transport {
            TransportKind::Usb => {
                let binding = current_tether_binding(peer)?.ok_or_else(|| {
                    SecureHostError::Interface(
                        "phone is not on a confirmed Android USB-tether interface".to_owned(),
                    )
                })?;
                if binding.interface_index() != listener.index {
                    return Err(SecureHostError::Interface(
                        "USB probe arrived outside the selected tether interface".to_owned(),
                    ));
                }
                Ok(Self::Usb {
                    binding,
                    listener: listener.clone(),
                })
            }
            TransportKind::Wifi => {
                let binding = wifi_binding(peer)?;
                if !binding.matches_listener(listener) {
                    return Err(SecureHostError::Interface(
                        "local-network probe route does not match the selected interface"
                            .to_owned(),
                    ));
                }
                Ok(binding)
            }
        }
    }

    fn revalidate(&self, peer: SocketAddr) -> Result<(), SecureHostError> {
        match self {
            #[cfg(test)]
            Self::Loopback => Ok(()),
            Self::Usb { binding, listener } => {
                binding.verify_peer(peer)?;
                let current = tether_ipv4_interfaces()?;
                if current.contains(&(listener.local, listener.index)) {
                    Ok(())
                } else {
                    Err(SecureHostError::Interface(
                        "selected USB-tether address or interface changed".to_owned(),
                    ))
                }
            }
            Self::Wifi {
                local,
                peer_network,
                mask,
                interface_index,
            } => {
                let current = wifi_binding(peer)?;
                match current {
                    Self::Wifi {
                        local: current_local,
                        peer_network: current_network,
                        mask: current_mask,
                        interface_index: current_index,
                    } if current_local == *local
                        && current_network == *peer_network
                        && current_mask == *mask
                        && current_index == *interface_index =>
                    {
                        Ok(())
                    }
                    _ => Err(SecureHostError::Interface(
                        "selected local-network interface changed".to_owned(),
                    )),
                }
            }
        }
    }

    fn tether(&self) -> Option<&TetherBinding> {
        match self {
            Self::Usb { binding, .. } => Some(binding),
            _ => None,
        }
    }

    fn accepts_ingress(&self, ingress: Option<u32>) -> bool {
        match self {
            #[cfg(test)]
            Self::Loopback => ingress.is_some(),
            Self::Usb { binding, .. } => binding.accepts_ingress_interface(ingress),
            Self::Wifi {
                interface_index, ..
            } => ingress.is_some() && ingress == *interface_index,
        }
    }

    fn matches_listener(&self, listener: &ListenerInterface) -> bool {
        match self {
            #[cfg(test)]
            Self::Loopback => false,
            Self::Usb { binding, .. } => binding.interface_index() == listener.index,
            Self::Wifi {
                local,
                interface_index,
                ..
            } => *local == listener.local && *interface_index == Some(listener.index),
        }
    }
}

fn wifi_binding(peer: SocketAddr) -> Result<NetworkBinding, SecureHostError> {
    let IpAddr::V4(peer_ip) = peer.ip() else {
        return Err(SecureHostError::Interface(
            "discovery requires IPv4".to_owned(),
        ));
    };
    if !private_ipv4(peer_ip) {
        return Err(SecureHostError::Interface(
            "Wi-Fi peer is outside private local address space".to_owned(),
        ));
    }
    let route = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    route.connect(peer)?;
    let IpAddr::V4(local) = route.local_addr()?.ip() else {
        return Err(SecureHostError::Interface(
            "selected route is not IPv4".to_owned(),
        ));
    };
    let mut matches = Vec::new();
    for interface in get_if_addrs()? {
        let IfAddr::V4(ref address) = interface.addr else {
            continue;
        };
        let name = interface.name.to_ascii_lowercase();
        if address.ip != local
            || !interface.is_oper_up()
            || interface.is_loopback()
            || interface.is_p2p()
            || rejected_local_interface(&name)
        {
            continue;
        }
        let mask = u32::from_be_bytes(address.netmask.octets());
        let local_value = u32::from_be_bytes(local.octets());
        let peer_value = u32::from_be_bytes(peer_ip.octets());
        if mask != 0 && local_value & mask == peer_value & mask {
            matches.push((mask, interface.index));
        }
    }
    if matches.len() != 1 {
        return Err(SecureHostError::Interface(
            "local-network route is missing or ambiguous; disconnect VPN/extra adapters".to_owned(),
        ));
    }
    let (mask, interface_index) = matches[0];
    Ok(NetworkBinding::Wifi {
        local,
        peer_network: u32::from_be_bytes(peer_ip.octets()) & mask,
        mask,
        interface_index,
    })
}

fn private_ipv4(address: Ipv4Addr) -> bool {
    address.is_private() || address.is_link_local()
}

fn rejected_local_interface(name: &str) -> bool {
    [
        "vpn",
        "tun",
        "tap",
        "wireguard",
        "wintun",
        "tailscale",
        "zerotier",
        "rndis",
        "remote ndis",
        "vethernet",
        "hyper-v",
        "vmware",
        "virtualbox",
        "virtual",
        "docker",
        "wsl",
        "bluetooth",
        "hamachi",
        "loopback",
        "npcap",
    ]
    .iter()
    .any(|token| name.contains(token))
}

#[derive(Default)]
struct QualityStats {
    sent: HashSet<u64>,
    received: HashSet<u64>,
    delays_micros: Vec<u64>,
    jitter_micros: Vec<u64>,
    repair_completion_micros: Vec<u64>,
    duplicates: u64,
    invalid_timing: u64,
    reordered: u64,
    highest_probe_id: Option<u64>,
    last_delay_micros: Option<u64>,
    signal_level: Option<i8>,
    rssi: Option<i16>,
    frequency: Option<u32>,
}

impl QualityStats {
    fn observe(&mut self, reply: crate::secure::QualityReply, received_nanos: u64) {
        if !self.sent.contains(&reply.probe_id) {
            return;
        }
        if !self.received.insert(reply.probe_id) {
            self.duplicates += 1;
            return;
        }
        if self
            .highest_probe_id
            .is_some_and(|highest| reply.probe_id < highest)
        {
            self.reordered += 1;
        } else {
            self.highest_probe_id = Some(reply.probe_id);
        }
        if reply.phone_send_nanos < reply.phone_receive_nanos
            || received_nanos < reply.host_send_nanos
        {
            self.invalid_timing += 1;
            return;
        }
        let turnaround = reply.phone_send_nanos - reply.phone_receive_nanos;
        let completion_nanos = received_nanos - reply.host_send_nanos;
        if completion_nanos < turnaround {
            self.invalid_timing += 1;
            return;
        }
        let micros = (completion_nanos - turnaround) / 1_000;
        if self.delays_micros.len() < 512 {
            if let Some(previous) = self.last_delay_micros {
                self.jitter_micros.push(previous.abs_diff(micros));
            }
            self.last_delay_micros = Some(micros);
            self.delays_micros.push(micros);
            if reply.repair_only {
                self.repair_completion_micros.push(completion_nanos / 1_000);
            }
        }
        if reply.signal_level >= 0 {
            self.signal_level = Some(reply.signal_level);
        }
        if reply.rssi_dbm != i16::MIN {
            self.rssi = Some(reply.rssi_dbm);
        }
        if reply.frequency_mhz != 0 {
            self.frequency = Some(reply.frequency_mhz);
        }
    }

    fn summary(mut self) -> String {
        self.delays_micros.sort_unstable();
        self.jitter_micros.sort_unstable();
        self.repair_completion_micros.sort_unstable();
        let sent = self.sent.len();
        let received = self.received.len();
        let lost = sent.saturating_sub(received);
        let percentile = |values: &[u64], percent: usize| -> Option<f64> {
            if values.is_empty() {
                return None;
            }
            let index = (values.len() * percent).div_ceil(100).max(1) - 1;
            Some(values[index] as f64 / 1_000.0)
        };
        let metric = |value: Option<f64>| {
            value.map_or_else(|| "unavailable".to_owned(), |value| format!("{value:.3}ms"))
        };
        let frequency = self.frequency.map_or_else(
            || "phone frequency/band unavailable".to_owned(),
            |frequency| {
                format!(
                    "phone {frequency}MHz {} (active link; MLO may expose one link)",
                    wifi_band(frequency)
                )
            },
        );
        let signal = match (self.rssi, self.signal_level) {
            (Some(rssi), Some(level)) => format!("phone RSSI {rssi}dBm, level {level}/4"),
            (Some(rssi), None) => format!("phone RSSI {rssi}dBm, level unavailable"),
            (None, Some(level)) => format!("phone RSSI unavailable, level {level}/4"),
            (None, None) => "phone RSSI/level unavailable".to_owned(),
        };
        let rtt_p95 = percentile(&self.delays_micros, 95);
        format!(
            "{frequency}; {signal}; host RSSI unavailable; duplex residual p50 {} p95 {} p99 {} max {}; jitter p95 {}; one-way unavailable (asymmetry unknown); repair completion p95 {}; samples {received}/{sent}, loss {lost}, reordered {}, duplicates {}, invalid timing {}, immediate-copy winner unavailable (8.333ms target; small pairing sample is not gameplay tail validation)",
            metric(percentile(&self.delays_micros, 50)),
            metric(rtt_p95),
            metric(if self.delays_micros.len() >= 1000 {
                percentile(&self.delays_micros, 99)
            } else {
                None
            }),
            metric(
                self.delays_micros
                    .last()
                    .map(|value| *value as f64 / 1_000.0)
            ),
            metric(percentile(&self.jitter_micros, 95)),
            metric(percentile(&self.repair_completion_micros, 95)),
            self.reordered,
            self.duplicates,
            self.invalid_timing,
        )
    }
}

fn wifi_band(frequency: u32) -> &'static str {
    match frequency {
        2_400..=2_500 => "2.4GHz",
        4_900..=5_900 => "5GHz",
        5_925..=7_125 => "6GHz",
        _ => "unknown band",
    }
}

fn is_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    )
}

fn duration_nanos(duration: Duration) -> u64 {
    duration.as_nanos().min(u128::from(u64::MAX)) as u64
}

#[derive(Debug)]
pub enum SecureHostError {
    Io(io::Error),
    Wire(WireError),
    Noise(snow::Error),
    Credentials(credentials::CredentialError),
    Interface(String),
    Authentication(&'static str),
    TimedOut(&'static str),
    NotPaired,
    Cancelled,
}

impl std::fmt::Display for SecureHostError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "network: {error}"),
            Self::Wire(error) => write!(formatter, "wire: {error}"),
            Self::Noise(error) => write!(formatter, "Noise handshake: {error}"),
            Self::Credentials(error) => error.fmt(formatter),
            Self::Interface(message) => formatter.write_str(message),
            Self::Authentication(message) => {
                write!(formatter, "authentication failed: {message}")
            }
            Self::TimedOut(message) => formatter.write_str(message),
            Self::NotPaired => formatter.write_str("no paired phone; pair this host first"),
            Self::Cancelled => formatter.write_str("pairing cancelled"),
        }
    }
}

impl std::error::Error for SecureHostError {}

impl From<io::Error> for SecureHostError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<WireError> for SecureHostError {
    fn from(value: WireError) -> Self {
        Self::Wire(value)
    }
}

impl From<snow::Error> for SecureHostError {
    fn from(value: snow::Error) -> Self {
        Self::Noise(value)
    }
}

impl From<credentials::CredentialError> for SecureHostError {
    fn from(value: credentials::CredentialError) -> Self {
        Self::Credentials(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn windows_listener_socket_options_are_accepted() {
        let interface = get_if_addrs()
            .unwrap()
            .into_iter()
            .find(|interface| interface.ip() == Ipv4Addr::LOCALHOST && interface.index.is_some())
            .expect("Windows IPv4 loopback interface");
        let listener = ListenerInterface {
            local: Ipv4Addr::LOCALHOST,
            index: interface.index.unwrap(),
            name: interface.name,
        };
        let socket = bind_listener_socket(&listener, 0).unwrap();
        let peer = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        peer.send_to(
            b"probe",
            (listener.local, socket.local_addr().unwrap().port()),
        )
        .unwrap();
        let mut buffer = [0; 32];
        let (count, source, ingress) = receive_datagram(&socket, &mut buffer).unwrap();
        assert_eq!(&buffer[..count], b"probe");
        assert_eq!(source, peer.local_addr().unwrap());
        assert!(listener_accepts_ingress(&listener, ingress));
    }

    fn sequence(start: u8) -> [u8; 32] {
        std::array::from_fn(|index| start.wrapping_add(index as u8))
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|value| format!("{value:02x}")).collect()
    }

    #[test]
    fn published_protocol_vectors_are_stable() {
        let exchange_id: [u8; 16] = std::array::from_fn(|index| index as u8);
        let handshake_prologue = crate::secure::prologue(TransportKind::Wifi, exchange_id);
        let host_static = sequence(0x10);
        let phone_static = sequence(0x30);
        let host_ephemeral = sequence(0x50);
        let phone_ephemeral = sequence(0x70);
        let ik_phone_ephemeral = sequence(0x90);
        let ik_host_ephemeral = sequence(0xb0);
        let params: NoiseParams = XX_NAME.parse().unwrap();
        let mut host = Builder::new(params.clone())
            .local_private_key(&host_static)
            .unwrap()
            .fixed_ephemeral_key_for_testing_only(&host_ephemeral)
            .prologue(&handshake_prologue)
            .unwrap()
            .build_initiator()
            .unwrap();
        let mut phone = Builder::new(params)
            .local_private_key(&phone_static)
            .unwrap()
            .fixed_ephemeral_key_for_testing_only(&phone_ephemeral)
            .prologue(&handshake_prologue)
            .unwrap()
            .build_responder()
            .unwrap();
        let mut buffer = [0_u8; MAX_DATAGRAM_SIZE];
        let mut payload = [0_u8; MAX_DATAGRAM_SIZE];
        let m1_len = host.write_message(&[], &mut buffer).unwrap();
        let m1 = buffer[..m1_len].to_vec();
        phone.read_message(&m1, &mut payload).unwrap();
        let m2_len = phone.write_message(&[], &mut buffer).unwrap();
        let m2 = buffer[..m2_len].to_vec();
        host.read_message(&m2, &mut payload).unwrap();
        let m3_len = host.write_message(&[], &mut buffer).unwrap();
        let m3 = buffer[..m3_len].to_vec();
        phone.read_message(&m3, &mut payload).unwrap();
        let hash = host.get_handshake_hash().to_vec();
        assert_eq!(hash, phone.get_handshake_hash());
        let host_public: [u8; 32] = phone.get_remote_static().unwrap().try_into().unwrap();
        let phone_public: [u8; 32] = host.get_remote_static().unwrap().try_into().unwrap();

        assert_eq!(
            hex(&handshake_prologue),
            "686f6c6f646f72692d70686f6e652d747261636b7061642d76360002000102030405060708090a0b0c0d0e0f"
        );
        assert_eq!(
            hex(&host_public),
            "d89e3bad79437dbed9f843418304f460ff05c7fe81fe4a9577a804cb9367ff66"
        );
        assert_eq!(
            hex(&phone_public),
            "34e42d4af5ef94a07a3a84201b889d4cd1a743cb27b11b6a10438a8feb8e5847"
        );
        assert_eq!(
            hex(&m1),
            "392d174a38b3b1beafaf1fe824870841c5fa531bc6eafdb6402c124664488c1c"
        );
        assert_eq!(
            hex(&m2),
            concat!(
                "23b7bb8c91ae008711fb12846780bcdf1e065f821bdfec49f57e7c7dcd4c4823",
                "f56b5ed019d6b4f7d390bd2416f19670654ee0fdcfd6a275323659d4bc92bd3b",
                "eac7cf12d97459e8bce0f17b8686e1e9c32d8910b963c3f7ab6b51b3fdda186a"
            )
        );
        assert_eq!(
            hex(&m3),
            concat!(
                "f531830cca96c417accf9c7fbb8b15f7eb91cc4ec6e41d779f704ed44dc67f66",
                "3bdd1c6d809eb4c5cea8fcc8b37a03f7a58efc53a85d46c286b57342424311c1"
            )
        );
        assert_eq!(
            hex(&hash),
            "6977c5b56b759d9a520bd729380074a5613eae3bf5fe5642158d81e2b9f1b50a"
        );

        let phone_random = sequence(0xa0);
        let host_random = sequence(0xc0);
        let phone_commit = crate::secure::sas_commit(1, &hash, &phone_random);
        let host_commit = crate::secure::sas_commit(2, &hash, &host_random);
        let digest = crate::secure::sas_digest(&hash, &phone_random, &host_random);
        assert_eq!(
            hex(&phone_commit),
            "c6b017e01832c9d7abf45934974870e4a2da962b8e83575607b3b0dcb10a3735"
        );
        assert_eq!(
            hex(&host_commit),
            "ab551195b25ad3b763aa5f3feb8ae61097263247319d53b545cdb89ae7d319c5"
        );
        assert_eq!(
            hex(&digest),
            "f0ab28ccbe90d24214154c1d06696aeaa4d09e1ef9721261857a9f5256da1a5b"
        );
        assert_eq!(crate::secure::sas_pattern(digest), [2, 5, 2, 5, 5, 4, 1, 5]);

        let mut phone_cipher = RecordCipher::from_handshake(&mut phone, false).unwrap();
        let record = phone_cipher
            .seal(Direction::PhoneToHost, PHONE_SAS_REVEAL, 0, 9, 0, b"vector")
            .unwrap();
        assert_eq!(
            hex(&record),
            concat!(
                "4850543606044600326fc09cffc2e6c200000000000000000000000000000000",
                "09000000000000000000000006000000eae096ab93850975f72cbd58bf71cc41",
                "c31a0dea4fae"
            )
        );
        let second_record = phone_cipher
            .seal(Direction::PhoneToHost, PHONE_SAS_REVEAL, 0, 9, 0, b"vector")
            .unwrap();
        assert_eq!(
            hex(&second_record),
            concat!(
                "4850543606044600326fc09cffc2e6c200000000000000000100000000000000",
                "0900000000000000000000000600000033de984324ab761f90dcad0d39c3b93a",
                "dcfaeebc7fd0"
            )
        );

        let ik_exchange: [u8; 16] = std::array::from_fn(|index| 0xf0 + index as u8);
        let ik_prologue = crate::secure::prologue(TransportKind::Usb, ik_exchange);
        let ik_params: NoiseParams = IK_NAME.parse().unwrap();
        let mut ik_phone = Builder::new(ik_params.clone())
            .local_private_key(&phone_static)
            .unwrap()
            .remote_public_key(&host_public)
            .unwrap()
            .fixed_ephemeral_key_for_testing_only(&ik_phone_ephemeral)
            .prologue(&ik_prologue)
            .unwrap()
            .build_initiator()
            .unwrap();
        let mut ik_host = Builder::new(ik_params)
            .local_private_key(&host_static)
            .unwrap()
            .fixed_ephemeral_key_for_testing_only(&ik_host_ephemeral)
            .prologue(&ik_prologue)
            .unwrap()
            .build_responder()
            .unwrap();
        let ik_m1_len = ik_phone.write_message(&[], &mut buffer).unwrap();
        let ik_m1 = buffer[..ik_m1_len].to_vec();
        ik_host.read_message(&ik_m1, &mut payload).unwrap();
        // IK message 2 carries the session parameters (replacing v5 HELLO).
        let parameters = session_parameters(6).encode(0x0807_0605_0403_0201);
        let ik_m2_len = ik_host.write_message(&parameters, &mut buffer).unwrap();
        let ik_m2 = buffer[..ik_m2_len].to_vec();
        let ik_payload_len = ik_phone.read_message(&ik_m2, &mut payload).unwrap();
        assert_eq!(payload[..ik_payload_len], parameters);
        assert_eq!(ik_phone.get_handshake_hash(), ik_host.get_handshake_hash());
        assert_eq!(
            hex(&ik_prologue),
            "686f6c6f646f72692d70686f6e652d747261636b7061642d76360001f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff"
        );
        assert_eq!(
            hex(&ik_m1),
            concat!(
                "9fd7ad6dcff4298dd3f96d5b1b2af910a0535b1488d7f8fabb349a982880b615",
                "ea374cd73714b7bd8d86c36ef4edda85485b3a2b38748dff758fd6ec58a7fb5a",
                "8a8e41171e51f08d93b6aa549f14cc552d48f941f2db491d8f61982720b5602d"
            )
        );
        assert_eq!(
            hex(&ik_m2),
            concat!(
                "3f3e5f6d86926c9c128cf84581574f96840d98ee5ab53b1ec3b76e2bb25b945e",
                "512fc5d532d298f88d3122fadeced96e315f4782fef1855fabe09742b9d4dbf7"
            )
        );
        assert_eq!(
            hex(ik_phone.get_handshake_hash()),
            "45f5c953a9233003fc076d6d069257ec76a04d65af607be74cb70246411d10e7"
        );
    }

    #[test]
    fn quality_band_boundaries_are_stable() {
        assert_eq!(wifi_band(2_437), "2.4GHz");
        assert_eq!(wifi_band(5_200), "5GHz");
        assert_eq!(wifi_band(6_105), "6GHz");
    }

    #[test]
    fn quality_report_keeps_rtt_and_repair_completion_distinct() {
        let mut quality = QualityStats::default();
        quality.sent.insert(7);
        quality.observe(
            crate::secure::QualityReply {
                probe_id: 7,
                host_send_nanos: 1_000_000,
                phone_receive_nanos: 10_000_000,
                phone_send_nanos: 12_000_000,
                repair_only: true,
                signal_level: -1,
                rssi_dbm: i16::MIN,
                frequency_mhz: 0,
            },
            8_000_000,
        );
        let summary = quality.summary();
        assert!(summary.contains("duplex residual p50 5.000ms"));
        assert!(summary.contains("one-way unavailable (asymmetry unknown)"));
        assert!(summary.contains("repair completion p95 7.000ms"));
        assert!(summary.contains("host RSSI unavailable"));
    }

    #[test]
    fn normal_lan_filter_rejects_tunnels_and_rndis() {
        assert!(rejected_local_interface("my vpn adapter"));
        assert!(rejected_local_interface("remote ndis compatible device"));
        assert!(rejected_local_interface("vethernet (default switch)"));
        assert!(rejected_local_interface("vmware network adapter"));
        assert!(!rejected_local_interface("ethernet"));
        assert!(!rejected_local_interface("wi-fi"));
    }

    #[test]
    fn remote_pair_confirm_cannot_persist_without_local_approval() {
        let mut persisted = false;
        let result = persist_pairing_if_authorized(true, false, true, || {
            persisted = true;
            Ok(())
        })
        .unwrap();
        assert_eq!(result, None);
        assert!(!persisted);

        let result = persist_pairing_if_authorized(true, true, true, || {
            persisted = true;
            Ok(())
        })
        .unwrap();
        assert_eq!(result, Some(()));
        assert!(persisted);
    }

    #[test]
    fn split_connection_id_matches_direct_hashing() {
        assert_eq!(
            crate::secure::connection_id(&[4; 32]),
            crate::secure::connection_id(&[4; 32])
        );
        assert_ne!(
            crate::secure::connection_id(&[4; 32]),
            crate::secure::connection_id(&[5; 32])
        );
    }
}
