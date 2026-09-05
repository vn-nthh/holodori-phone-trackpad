use super::*;
use crate::keyboard::KeyboardSink;
use crate::protocol::{
    ACTION_CANCEL, ACTION_DOWN, ACTION_MOVE, ACTION_UP, FRAME_FLAG_LOCKED,
    FRAME_FLAG_SESSION_START, MAX_CONTACTS,
};
use crate::v5::{CONTACT_SIZE, TOUCH_PAYLOAD_HEADER_SIZE};
use std::mem::ManuallyDrop;

struct Phone {
    socket: UdpSocket,
    cipher: RecordCipher,
    bytes: [u8; MAX_DATAGRAM_SIZE],
    epoch: Instant,
}

impl Phone {
    fn send(
        &mut self,
        kind: u8,
        session: u64,
        sequence: u64,
        payload: &[u8],
    ) -> Result<(), HostV5Error> {
        let length = self.cipher.seal_into(
            Direction::PhoneToHost,
            kind,
            session,
            sequence,
            0,
            payload,
            &mut self.bytes,
        )?;
        self.socket.send(&self.bytes[..length])?;
        Ok(())
    }

    fn touch(&mut self, sequence: u64, action: u8) {
        let payload = touch_payload(action, duration_nanos(self.epoch.elapsed()));
        self.send(PHONE_TOUCH, 9, sequence, &payload[..payload_length(action)])
            .unwrap();
    }

    fn ack(&mut self, expected: u64) -> io::Result<()> {
        loop {
            let length = self.socket.recv(&mut self.bytes)?;
            let header = self
                .cipher
                .open_in_place(Direction::HostToPhone, &mut self.bytes[..length])
                .unwrap();
            if header.message_type == HOST_ACK && header.logical_id == expected {
                return Ok(());
            }
        }
    }

    fn abort(&mut self) {
        let _ = self.send(PHONE_AUTH_ABORT, 0, 0, &1_u16.to_le_bytes());
    }
}

fn payload_length(action: u8) -> usize {
    TOUCH_PAYLOAD_HEADER_SIZE
        + if action == ACTION_CANCEL {
            0
        } else {
            CONTACT_SIZE
        }
}

fn touch_payload(action: u8, event_nanos: u64) -> [u8; TOUCH_PAYLOAD_HEADER_SIZE + CONTACT_SIZE] {
    let mut bytes = [0; TOUCH_PAYLOAD_HEADER_SIZE + CONTACT_SIZE];
    bytes[..8].copy_from_slice(&event_nanos.to_le_bytes());
    bytes[8..16].copy_from_slice(&event_nanos.to_le_bytes());
    bytes[16..24].copy_from_slice(&event_nanos.to_le_bytes());
    bytes[40] = action;
    bytes[42] = u8::from(action != ACTION_CANCEL);
    bytes[43] = if action == ACTION_CANCEL {
        FRAME_FLAG_SESSION_START
    } else {
        FRAME_FLAG_LOCKED
    };
    bytes[45] = if action == ACTION_UP { 1 } else { 3 };
    bytes[46..48].copy_from_slice(&5_000_i16.to_le_bytes());
    bytes[48..50].copy_from_slice(&5_000_i16.to_le_bytes());
    bytes
}

fn pair() -> (V5Connection, Phone) {
    let epoch = Instant::now();
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let phone_socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    enable_receive_interface(&socket).unwrap();
    socket.connect(phone_socket.local_addr().unwrap()).unwrap();
    phone_socket.connect(socket.local_addr().unwrap()).unwrap();
    phone_socket
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let mut host = build_handshake(XX_NAME, true, &[0x10; 32], None, b"loopback test").unwrap();
    let mut phone = build_handshake(XX_NAME, false, &[0x30; 32], None, b"loopback test").unwrap();
    let mut bytes = [0; MAX_DATAGRAM_SIZE];
    let mut payload = [0; MAX_DATAGRAM_SIZE];
    let n = host.write_message(&[], &mut bytes).unwrap();
    phone.read_message(&bytes[..n], &mut payload).unwrap();
    let n = phone.write_message(&[], &mut bytes).unwrap();
    host.read_message(&bytes[..n], &mut payload).unwrap();
    let n = host.write_message(&[], &mut bytes).unwrap();
    phone.read_message(&bytes[..n], &mut payload).unwrap();
    let host_cipher = RecordCipher::from_handshake(&mut host, true).unwrap();
    let phone_cipher = RecordCipher::from_handshake(&mut phone, false).unwrap();
    let connection = V5Connection::new(
        socket,
        phone_socket.local_addr().unwrap(),
        NetworkBinding::Loopback,
        host_cipher,
        None,
    );
    (
        connection,
        Phone {
            socket: phone_socket,
            cipher: phone_cipher,
            bytes,
            epoch,
        },
    )
}

/// Uses the real six-lane planner and partial-submission bookkeeping. Only the final
/// OS syscall is substituted, so these tests never type into the user's foreground app.
struct MeasuredSink {
    keys: ManuallyDrop<KeyboardSink>,
    sequences: Vec<u64>,
    latency_nanos: Vec<u64>,
    reject: Option<u64>,
    epoch: Instant,
}

impl MeasuredSink {
    fn new(epoch: Instant) -> Self {
        Self {
            keys: crate::keyboard::tests::test_sink(6),
            sequences: Vec::with_capacity(4096),
            latency_nanos: Vec::with_capacity(4096),
            reject: None,
            epoch,
        }
    }
}

impl InputSink for MeasuredSink {
    fn accept(&mut self, frame: &TouchFrame) -> io::Result<()> {
        if self.reject == Some(frame.sequence) {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        self.keys.accept_recorded(frame)?;
        self.sequences.push(frame.sequence);
        if frame.sequence != 0 {
            self.latency_nanos
                .push(duration_nanos(self.epoch.elapsed()).saturating_sub(frame.phone_event_nanos));
        }
        Ok(())
    }
    fn has_active_input(&self) -> bool {
        self.keys.has_active_input()
    }
    fn cancel_all(&mut self) -> io::Result<()> {
        self.keys.cancel_recorded()
    }
}

fn start(
    mut connection: V5Connection,
    mut sink: MeasuredSink,
) -> thread::JoinHandle<(MeasuredSink, Result<(), HostV5Error>)> {
    thread::spawn(move || {
        let result = serve_gameplay(
            &mut connection,
            &mut OrderedFrames::new(),
            &mut sink,
            &mut HostMetrics::new(false, 8.333, 5),
            6,
            &AtomicBool::new(false),
        );
        (sink, result)
    })
}

#[test]
fn real_receive_loop_reorders_deduplicates_and_commits_before_ack() {
    let (connection, mut phone) = pair();
    let worker = start(connection, MeasuredSink::new(phone.epoch));
    phone.touch(0, ACTION_CANCEL);
    phone.ack(0).unwrap();
    phone.touch(2, ACTION_UP);
    phone.ack(0).unwrap();
    phone.touch(1, ACTION_DOWN);
    phone.ack(2).unwrap();
    phone.touch(2, ACTION_UP);
    phone.ack(2).unwrap();
    phone.abort();
    let (sink, result) = worker.join().unwrap();
    assert!(result.is_err());
    assert_eq!(sink.sequences, [0, 1, 2]);
    assert!(!sink.has_active_input());
}

#[test]
fn sink_failure_withholds_ack_and_releases_held_keys() {
    let (connection, mut phone) = pair();
    let mut sink = MeasuredSink::new(phone.epoch);
    sink.reject = Some(2);
    let worker = start(connection, sink);
    phone.touch(0, ACTION_CANCEL);
    phone.ack(0).unwrap();
    phone.touch(1, ACTION_DOWN);
    phone.ack(1).unwrap();
    phone.touch(2, ACTION_UP);
    assert!(phone.ack(2).is_err());
    let (sink, result) = worker.join().unwrap();
    assert!(result.is_err());
    assert_eq!(sink.sequences, [0, 1]);
    assert!(!sink.has_active_input());
}

#[test]
fn authenticated_pings_cannot_sustain_uncommitted_held_input() {
    let (connection, mut phone) = pair();
    let worker = start(connection, MeasuredSink::new(phone.epoch));
    phone.touch(0, ACTION_CANCEL);
    phone.ack(0).unwrap();
    phone.touch(1, ACTION_DOWN);
    phone.ack(1).unwrap();
    for ping in 0..100 {
        if worker.is_finished() {
            break;
        }
        let _ = phone.send(PHONE_PING, 9, ping, &[]);
        thread::sleep(Duration::from_millis(2));
    }
    let (sink, result) = worker.join().unwrap();
    assert!(
        matches!(result, Err(HostV5Error::Io(error)) if error.kind() == io::ErrorKind::TimedOut)
    );
    assert!(!sink.has_active_input());
}

#[test]
fn idle_keepalives_preserve_session_but_repeated_ping_ids_expire() {
    let (connection, mut phone) = pair();
    let worker = start(connection, MeasuredSink::new(phone.epoch));
    phone.touch(0, ACTION_CANCEL);
    phone.ack(0).unwrap();
    for ping in 0..22 {
        phone.send(PHONE_PING, 9, ping, &[]).unwrap();
        thread::sleep(Duration::from_millis(100));
        assert!(!worker.is_finished());
    }
    // Re-encryption gives these fresh packet numbers, but no fresh idle progress.
    for _ in 0..24 {
        if worker.is_finished() {
            break;
        }
        let _ = phone.send(PHONE_PING, 9, 21, &[]);
        thread::sleep(Duration::from_millis(100));
    }
    assert!(worker.is_finished());
    let (sink, result) = worker.join().unwrap();
    assert!(result.is_err());
    assert_eq!(sink.sequences, [0]);
}

#[test]
fn changing_gameplay_session_requires_fresh_authentication() {
    let (connection, mut phone) = pair();
    let worker = start(connection, MeasuredSink::new(phone.epoch));
    phone.touch(0, ACTION_CANCEL);
    phone.ack(0).unwrap();
    let payload = touch_payload(ACTION_CANCEL, 0);
    phone
        .send(PHONE_TOUCH, 10, 0, &payload[..TOUCH_PAYLOAD_HEADER_SIZE])
        .unwrap();
    let (sink, result) = worker.join().unwrap();
    assert!(matches!(result, Err(HostV5Error::Authentication(_))));
    assert_eq!(sink.sequences, [0]);
}

#[test]
fn authenticated_receive_commit_and_ack_allocate_nothing_after_setup() {
    let (mut connection, mut phone) = pair();
    let mut sink = MeasuredSink::new(phone.epoch);
    let mut ordered = OrderedFrames::new();
    ordered.require_fresh_session();
    let mut metrics = HostMetrics::new(true, 8.333, 5);
    let mut state = GameplayState {
        lane_count: 6,
        last_committed_frame: Instant::now(),
        last_idle_activity: Instant::now(),
        last_ping: None,
    };
    let stopping = AtomicBool::new(false);
    let mut apply = |sequence: u64| {
        let action = if sequence == 0 || sequence % 128 == 127 {
            ACTION_CANCEL
        } else {
            ACTION_MOVE
        };
        let count = if action == ACTION_CANCEL {
            0
        } else {
            MAX_CONTACTS
        };
        let mut payload = [0; TOUCH_PAYLOAD_HEADER_SIZE + MAX_CONTACTS * CONTACT_SIZE];
        payload[..TOUCH_PAYLOAD_HEADER_SIZE].copy_from_slice(
            &touch_payload(action, duration_nanos(phone.epoch.elapsed()))
                [..TOUCH_PAYLOAD_HEADER_SIZE],
        );
        payload[42] = count as u8;
        payload[43] = if sequence == 0 {
            FRAME_FLAG_SESSION_START
        } else {
            FRAME_FLAG_LOCKED
        };
        for index in 0..count {
            let offset = TOUCH_PAYLOAD_HEADER_SIZE + index * CONTACT_SIZE;
            payload[offset] = (index + if sequence % 64 >= 32 { MAX_CONTACTS } else { 0 }) as u8;
            payload[offset + 1] = 3;
            let x: i16 = if (sequence + index as u64).is_multiple_of(2) {
                0
            } else {
                10_000
            };
            payload[offset + 2..offset + 4].copy_from_slice(&x.to_le_bytes());
        }
        let length = phone
            .cipher
            .seal_into(
                Direction::PhoneToHost,
                PHONE_TOUCH,
                9,
                sequence,
                0,
                &payload[..TOUCH_PAYLOAD_HEADER_SIZE + count * CONTACT_SIZE],
                &mut phone.bytes,
            )
            .unwrap();
        let arrival = Instant::now();
        let header = connection
            .cipher
            .open_in_place(Direction::PhoneToHost, &mut phone.bytes[..length])
            .unwrap();
        let frame = decode_touch_payload(
            &header,
            &phone.bytes[RECORD_HEADER_SIZE..length - crate::v5::TAG_SIZE],
        )
        .unwrap();
        process_gameplay_frame(
            &mut connection,
            &mut ordered,
            &mut sink,
            &mut metrics,
            &mut state,
            frame,
            arrival,
            &stopping,
        )
        .unwrap();
    };
    // Initialize the crypto dispatch; the first large chord and later cancellations
    // are inside the allocation count, with platform event encoding and metrics enabled.
    apply(0);
    let (_, allocations) = crate::allocation_check::count(|| {
        for sequence in 1..1024 {
            apply(sequence);
        }
    });
    assert_eq!(
        allocations, 0,
        "steady input must not allocate, including metrics and ACK encryption"
    );
    assert_eq!(ordered.acknowledged_sequence(), Some(1023));
    sink.cancel_all().unwrap();
}

#[test]
#[ignore = "optimized production-loop timing; run explicitly without competing builds"]
fn production_loopback_latency() {
    for fault in ["healthy", "corrupt-first", "first-lost", "both-lost"] {
        let (connection, mut phone) = pair();
        let worker = start(connection, MeasuredSink::new(phone.epoch));
        phone.touch(0, ACTION_CANCEL);
        phone.ack(0).unwrap();
        for sequence in 1..=160 {
            let event_nanos = duration_nanos(phone.epoch.elapsed());
            let action = if sequence % 2 == 0 {
                ACTION_UP
            } else {
                ACTION_DOWN
            };
            let payload = touch_payload(action, event_nanos);
            // The timer includes both first encryption and each independent retry encryption.
            for copy in 0..2 {
                let length = phone
                    .cipher
                    .seal_into(
                        Direction::PhoneToHost,
                        PHONE_TOUCH,
                        9,
                        sequence,
                        0,
                        &payload,
                        &mut phone.bytes,
                    )
                    .unwrap();
                if fault == "both-lost" || (fault == "first-lost" && copy == 0) {
                    continue;
                }
                if fault == "corrupt-first" && copy == 0 {
                    phone.bytes[length - 1] ^= 1;
                }
                phone.socket.send(&phone.bytes[..length]).unwrap();
            }
            if fault == "both-lost" {
                // Real OS scheduling instead of a busy spin. Android's actual repair
                // selector is covered separately by V5SendQueueTest/device validation.
                let due =
                    phone.epoch + Duration::from_nanos(event_nanos) + Duration::from_millis(2);
                if let Some(wait) = due.checked_duration_since(Instant::now()) {
                    thread::sleep(wait);
                }
                phone.send(PHONE_TOUCH, 9, sequence, &payload).unwrap();
            }
            phone.ack(sequence).unwrap();
        }
        phone.abort();
        let (mut sink, _) = worker.join().unwrap();
        assert_eq!(sink.latency_nanos.len(), 160);
        // Exclude setup/JIT-free allocator warm-up explicitly, then report the tail.
        let samples = &mut sink.latency_nanos[32..];
        samples.sort_unstable();
        let p99 = samples[(samples.len() * 99).div_ceil(100) - 1];
        let maximum = *samples.last().unwrap();
        eprintln!(
            "v5 production {fault}: n={} p50={:.3}ms p99={:.3}ms max={:.3}ms (OS acceptance simulated)",
            samples.len(),
            samples[samples.len() / 2] as f64 / 1e6,
            p99 as f64 / 1e6,
            maximum as f64 / 1e6
        );
        assert!(maximum <= 8_333_333, "{fault} exceeded one 120 Hz frame");
    }
}
