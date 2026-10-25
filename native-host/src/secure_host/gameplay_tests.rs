use super::*;
use crate::keyboard::KeyboardSink;
use crate::protocol::{
    ACTION_CANCEL, ACTION_DOWN, ACTION_MOVE, ACTION_UP, CONTACT_FLAG_INSIDE,
    CONTACT_FLAG_KEY_SUPPRESSED, CONTACT_FLAG_TIP, FRAME_FLAG_LOCKED, FRAME_FLAG_SESSION_START,
    MAX_CONTACTS, TouchFrame,
};
use crate::secure::{
    PROTOCOL_VERSION, SESSION_PARAMETERS_SIZE, TAG_SIZE, TestContact, TestFrame, encode_test_record,
};
use std::mem::ManuallyDrop;

/// Android's `SendQueue.REPAIR_NANOS`.
const ANDROID_REPAIR_INTERVAL: Duration = Duration::from_millis(2);
const SESSION: u64 = 9;

const TIP: [TestContact; 1] = [TestContact {
    pointer_id: 0,
    flags: CONTACT_FLAG_INSIDE | CONTACT_FLAG_TIP,
    x: 5_000,
    y: 5_000,
}];
const LIFTED: [TestContact; 1] = [TestContact {
    pointer_id: 0,
    flags: CONTACT_FLAG_INSIDE,
    x: 5_000,
    y: 5_000,
}];

/// A one-finger frame at the center of lane 3; CANCEL is the session start.
fn frame(action: u8, event_nanos: u64) -> TestFrame<'static> {
    let (flags, contacts): (u8, &'static [TestContact]) = match action {
        ACTION_CANCEL => (FRAME_FLAG_SESSION_START, &[]),
        ACTION_UP => (FRAME_FLAG_LOCKED, &LIFTED),
        _ => (FRAME_FLAG_LOCKED, &TIP),
    };
    TestFrame {
        action,
        flags,
        event_nanos,
        contacts,
    }
}

struct Phone {
    socket: UdpSocket,
    cipher: RecordCipher,
    bytes: [u8; MAX_DATAGRAM_SIZE],
    epoch: Instant,
}

impl Phone {
    fn now_nanos(&self) -> u64 {
        duration_nanos(self.epoch.elapsed())
    }

    fn send(
        &mut self,
        kind: u8,
        session: u64,
        sequence: u64,
        payload: &[u8],
    ) -> Result<(), SecureHostError> {
        let length = self.seal(kind, session, sequence, payload);
        self.socket.send(&self.bytes[..length])?;
        Ok(())
    }

    fn seal(&mut self, kind: u8, session: u64, sequence: u64, payload: &[u8]) -> usize {
        self.cipher
            .seal_into(
                Direction::PhoneToHost,
                kind,
                session,
                sequence,
                0,
                payload,
                &mut self.bytes,
            )
            .unwrap()
    }

    /// Send one record carrying `actions` as consecutive frames.
    fn touches(&mut self, first_sequence: u64, actions: &[u8]) {
        let (payload, length) = record_payload(actions, self.now_nanos());
        self.send(PHONE_TOUCH, SESSION, first_sequence, &payload[..length])
            .unwrap();
    }

    fn touch(&mut self, sequence: u64, action: u8) {
        self.touches(sequence, &[action]);
    }

    /// The next ACK's cumulative sequence and selective bitmap.
    fn next_ack(&mut self) -> io::Result<(u64, u64)> {
        loop {
            let length = self.socket.recv(&mut self.bytes)?;
            let header = self
                .cipher
                .open_in_place(Direction::HostToPhone, &mut self.bytes[..length])
                .unwrap();
            if header.message_type == HOST_ACK {
                let at = RECORD_HEADER_SIZE + SESSION_PARAMETERS_SIZE;
                let selective = u64::from_le_bytes(self.bytes[at..at + 8].try_into().unwrap());
                return Ok((header.logical_id, selective));
            }
        }
    }

    fn ack(&mut self, expected: u64) -> io::Result<()> {
        while self.next_ack()?.0 != expected {}
        Ok(())
    }

    /// Every ACK still queued, without waiting.
    fn drain_acks(&mut self) -> Vec<u64> {
        self.socket.set_nonblocking(true).unwrap();
        let mut acknowledgements = Vec::new();
        while let Ok((cumulative, _)) = self.next_ack() {
            acknowledgements.push(cumulative);
        }
        self.socket.set_nonblocking(false).unwrap();
        acknowledgements
    }

    fn abort(&mut self) {
        let _ = self.send(PHONE_AUTH_ABORT, 0, 0, &ABORT_CANCELLED.to_le_bytes());
    }
}

fn record_payload(actions: &[u8], event_nanos: u64) -> ([u8; MAX_DATAGRAM_SIZE], usize) {
    let frames: Vec<TestFrame<'_>> = actions
        .iter()
        .map(|&action| frame(action, event_nanos))
        .collect();
    let mut payload = [0; MAX_DATAGRAM_SIZE];
    let length = encode_test_record(event_nanos, &frames, &mut payload);
    (payload, length)
}

fn pair() -> (SecureConnection, Phone) {
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
    let connection = SecureConnection::new(
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
    connection: SecureConnection,
    sink: MeasuredSink,
) -> thread::JoinHandle<(MeasuredSink, Result<(), SecureHostError>)> {
    start_with_metrics(connection, sink, false)
}

fn start_with_metrics(
    mut connection: SecureConnection,
    mut sink: MeasuredSink,
    diagnostics: bool,
) -> thread::JoinHandle<(MeasuredSink, Result<(), SecureHostError>)> {
    thread::spawn(move || {
        let result = serve_gameplay(
            &mut connection,
            &mut OrderedFrames::new(),
            &mut sink,
            &mut HostMetrics::new(diagnostics, 8.333, PROTOCOL_VERSION),
            6,
            &AtomicBool::new(false),
        );
        (sink, result)
    })
}

/// The host loop's state, driven one record at a time without a thread.
struct Harness {
    connection: SecureConnection,
    phone: Phone,
    ordered: OrderedFrames,
    sink: MeasuredSink,
    metrics: HostMetrics,
    state: GameplayState,
}

impl Harness {
    fn new(diagnostics: bool) -> Self {
        let (connection, phone) = pair();
        let mut ordered = OrderedFrames::new();
        ordered.require_fresh_session();
        Self {
            connection,
            sink: MeasuredSink::new(phone.epoch),
            phone,
            ordered,
            metrics: HostMetrics::new(diagnostics, 8.333, PROTOCOL_VERSION),
            state: GameplayState::new(6),
        }
    }

    /// Seal, authenticate, parse, and apply one record of `payload`.
    fn deliver(&mut self, first_sequence: u64, payload: &[u8]) {
        let length = self
            .phone
            .seal(PHONE_TOUCH, SESSION, first_sequence, payload);
        let arrival = Instant::now();
        let header = self
            .connection
            .cipher
            .open_in_place(Direction::PhoneToHost, &mut self.phone.bytes[..length])
            .unwrap();
        let record = TouchRecord::parse(
            &header,
            &self.phone.bytes[RECORD_HEADER_SIZE..length - TAG_SIZE],
        )
        .unwrap();
        process_touch_record(
            &mut self.connection,
            &mut self.ordered,
            &mut self.sink,
            &mut self.metrics,
            &mut self.state,
            &record,
            arrival,
            &AtomicBool::new(false),
        )
        .unwrap();
    }

    fn deliver_actions(&mut self, first_sequence: u64, actions: &[u8]) {
        let (payload, length) = record_payload(actions, self.phone.now_nanos());
        self.deliver(first_sequence, &payload[..length]);
    }
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
    // A duplicate spaced like a real repair is acknowledged but not reapplied.
    thread::sleep(ANDROID_REPAIR_INTERVAL);
    phone.touch(2, ACTION_UP);
    phone.ack(2).unwrap();
    phone.abort();
    let (sink, result) = worker.join().unwrap();
    assert!(result.is_err());
    assert_eq!(sink.sequences, [0, 1, 2]);
    assert!(!sink.has_active_input());
}

#[test]
fn each_progress_step_sends_one_ack_and_holes_report_selective_acks() {
    let (connection, mut phone) = pair();
    let worker = start(connection, MeasuredSink::new(phone.epoch));
    phone.touch(0, ACTION_CANCEL);
    assert_eq!(phone.next_ack().unwrap(), (0, 0));
    // Sequence one is missing. Bit `i` reports frame `cumulative + 2 + i`.
    phone.touch(2, ACTION_DOWN);
    assert_eq!(phone.next_ack().unwrap(), (0, 0b01));
    phone.touch(3, ACTION_UP);
    assert_eq!(phone.next_ack().unwrap(), (0, 0b11));
    phone.touch(1, ACTION_DOWN);
    assert_eq!(phone.next_ack().unwrap(), (3, 0));
    phone.abort();
    let (sink, _) = worker.join().unwrap();
    assert_eq!(sink.sequences, [0, 1, 2, 3]);
    assert!(phone.drain_acks().is_empty(), "one ACK per record");
}

#[test]
fn a_multi_frame_record_commits_in_order_under_one_ack() {
    let (connection, mut phone) = pair();
    let worker = start(connection, MeasuredSink::new(phone.epoch));
    phone.touches(0, &[ACTION_CANCEL, ACTION_DOWN, ACTION_MOVE, ACTION_UP]);
    assert_eq!(phone.next_ack().unwrap(), (3, 0));
    // A repair overlapping committed frames applies only what is new.
    phone.touches(2, &[ACTION_MOVE, ACTION_UP, ACTION_DOWN]);
    assert_eq!(phone.next_ack().unwrap(), (4, 0));
    phone.abort();
    let (sink, _) = worker.join().unwrap();
    assert_eq!(sink.sequences, [0, 1, 2, 3, 4]);
    assert!(phone.drain_acks().is_empty());
}

#[test]
fn immediate_twin_is_not_acked_but_a_later_repair_is() {
    let mut harness = Harness::new(false);
    for _ in 0..2 {
        harness.deliver_actions(0, &[ACTION_CANCEL]);
    }
    assert_eq!(
        harness.phone.drain_acks(),
        [0],
        "the twin must not add a second ACK"
    );

    // A repair arrives at least 2 ms after its copy; model that age directly.
    let last = harness.state.last_ack.as_mut().unwrap();
    last.at -= DUPLICATE_ACK_HOLDOFF;
    harness.deliver_actions(0, &[ACTION_CANCEL]);
    assert_eq!(
        harness.phone.drain_acks(),
        [0],
        "a repair recovers lost feedback"
    );
    assert_eq!(harness.sink.sequences, [0]);
}

#[test]
fn a_twin_that_changes_the_selective_ack_is_still_acknowledged() {
    let mut harness = Harness::new(false);
    harness.deliver_actions(0, &[ACTION_CANCEL]);
    harness.deliver_actions(2, &[ACTION_DOWN]);
    harness.deliver_actions(3, &[ACTION_UP]);
    // Each record changed what the host holds, however soon it followed.
    assert_eq!(harness.phone.drain_acks(), [0, 0, 0]);
    assert_eq!(harness.ordered.selective_ack(), 0b11);
}

#[test]
fn host_failure_sends_an_authenticated_abort() {
    let (connection, mut phone) = pair();
    let mut sink = MeasuredSink::new(phone.epoch);
    sink.reject = Some(1);
    let worker = start(connection, sink);
    phone.touch(0, ACTION_CANCEL);
    phone.ack(0).unwrap();
    phone.touch(1, ACTION_DOWN);
    let (_, result) = worker.join().unwrap();
    assert!(result.is_err());
    let mut aborted = false;
    while let Ok(length) = phone.socket.recv(&mut phone.bytes) {
        let header = phone
            .cipher
            .open_in_place(Direction::HostToPhone, &mut phone.bytes[..length])
            .unwrap();
        if header.message_type == HOST_AUTH_ABORT {
            assert_eq!(header.session_id, SESSION);
            aborted = true;
            break;
        }
    }
    assert!(
        aborted,
        "Android must learn of a host failure without its watchdog"
    );
}

#[test]
fn lost_acks_recover_on_replay_without_reinjecting_input() {
    let (connection, mut phone) = pair();
    let worker = start(connection, MeasuredSink::new(phone.epoch));
    for (sequence, action) in [(0, ACTION_CANCEL), (1, ACTION_DOWN)] {
        phone.touch(sequence, action);
        // Discard the ACK as if it never reached Android's retained queue.
        phone.ack(sequence).unwrap();
        // Android's repair timer is 2 ms; an earlier copy is the immediate
        // twin, which the host deliberately does not acknowledge again.
        thread::sleep(ANDROID_REPAIR_INTERVAL);
        // A fresh packet number carrying the same logical frame repairs that
        // lost feedback, including the initial CANCEL boundary and a held key.
        phone.touch(sequence, action);
        phone.ack(sequence).unwrap();
    }
    phone.touch(2, ACTION_UP);
    phone.ack(2).unwrap();
    phone.abort();
    let (sink, _) = worker.join().unwrap();
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
        let _ = phone.send(PHONE_PING, SESSION, ping, &[]);
        thread::sleep(Duration::from_millis(2));
    }
    let (sink, result) = worker.join().unwrap();
    assert!(
        matches!(result, Err(SecureHostError::Io(error)) if error.kind() == io::ErrorKind::TimedOut)
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
        phone.send(PHONE_PING, SESSION, ping, &[]).unwrap();
        thread::sleep(Duration::from_millis(100));
        assert!(!worker.is_finished());
    }
    // Re-encryption gives these fresh packet numbers, but no fresh idle progress.
    for _ in 0..24 {
        if worker.is_finished() {
            break;
        }
        let _ = phone.send(PHONE_PING, SESSION, 21, &[]);
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
    let (payload, length) = record_payload(&[ACTION_CANCEL], 0);
    phone
        .send(PHONE_TOUCH, SESSION + 1, 0, &payload[..length])
        .unwrap();
    let (sink, result) = worker.join().unwrap();
    assert!(matches!(result, Err(SecureHostError::Authentication(_))));
    assert_eq!(sink.sequences, [0]);
}

#[test]
fn authenticated_receive_commit_and_ack_allocate_nothing_after_setup() {
    let mut harness = Harness::new(true);
    let mut apply = |sequence: u64| {
        let cancel = sequence == 0 || sequence % 128 == 127;
        let contacts: [TestContact; MAX_CONTACTS] = std::array::from_fn(|index| TestContact {
            pointer_id: (index + if sequence % 64 >= 32 { MAX_CONTACTS } else { 0 }) as u8,
            flags: CONTACT_FLAG_INSIDE | CONTACT_FLAG_TIP,
            x: if (sequence + index as u64).is_multiple_of(2) {
                0
            } else {
                10_000
            },
            y: 0,
        });
        let frame = TestFrame {
            action: if cancel { ACTION_CANCEL } else { ACTION_MOVE },
            flags: if sequence == 0 {
                FRAME_FLAG_SESSION_START
            } else {
                FRAME_FLAG_LOCKED
            },
            event_nanos: harness.phone.now_nanos(),
            contacts: if cancel { &[] } else { &contacts },
        };
        let mut payload = [0; MAX_DATAGRAM_SIZE];
        let length = encode_test_record(frame.event_nanos, &[frame], &mut payload);
        harness.deliver(sequence, &payload[..length]);
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
    assert_eq!(harness.ordered.acknowledged_sequence(), Some(1023));
    harness.sink.cancel_all().unwrap();
}

#[test]
fn production_receiver_reports_corruption_order_and_outcomes() {
    let (mut connection, mut phone) = pair();
    let mut sink = MeasuredSink::new(phone.epoch);
    let worker = thread::spawn(move || {
        let mut metrics = HostMetrics::new(true, 8.333, PROTOCOL_VERSION);
        metrics.begin_connection();
        let result = serve_gameplay(
            &mut connection,
            &mut OrderedFrames::new(),
            &mut sink,
            &mut metrics,
            6,
            &AtomicBool::new(false),
        );
        (metrics, result)
    });
    phone.touch(0, ACTION_CANCEL);
    phone.ack(0).unwrap();
    // Corrupt a real authenticated record; it must not count as a logical frame.
    let (payload, payload_length) = record_payload(&[ACTION_DOWN], phone.now_nanos());
    let length = phone.seal(PHONE_TOUCH, SESSION, 1, &payload[..payload_length]);
    phone.bytes[length - 1] ^= 1;
    phone.socket.send(&phone.bytes[..length]).unwrap();
    phone.touch(2, ACTION_UP);
    phone.ack(0).unwrap();
    phone.touch(1, ACTION_DOWN);
    phone.ack(2).unwrap();
    // A duplicate spaced like a real repair is acknowledged but not reapplied.
    thread::sleep(ANDROID_REPAIR_INTERVAL);
    phone.touch(2, ACTION_UP);
    phone.ack(2).unwrap();
    phone.abort();
    let (mut metrics, _) = worker.join().unwrap();
    let folder =
        std::env::temp_dir().join(format!("doritrack-diagnostic-test-{}", std::process::id()));
    let path = folder.join("production.json");
    metrics.write_report(&path).unwrap();
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path.with_extension("events.json")).unwrap())
            .unwrap();
    let analysis = &report["analysis"];
    assert_eq!(analysis["counts"]["bad_tag"], 1);
    assert_eq!(analysis["counts"]["os_accepted"], 3);
    assert_eq!(analysis["counts"]["logical_duplicate_datagrams"], 1);
    assert_eq!(analysis["counts"]["ordering_holes"], 1);
    assert_eq!(analysis["diagnostic_records_dropped"], 0);
    assert!(!analysis["incidents"].as_array().unwrap().is_empty());
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .starts_with("Doritrack diagnostic report")
    );
    std::fs::remove_file(path.with_extension("events.json")).unwrap();
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(folder).unwrap();
}

#[test]
#[ignore = "optimized production-loop timing; run explicitly without competing builds"]
fn production_loopback_latency() {
    for diagnostics in [false, true] {
        for fault in [
            "healthy",
            "corrupt-first",
            "first-lost",
            "both-lost",
            "pressure-healthy",
            "pressure-both-lost",
        ] {
            let (connection, mut phone) = pair();
            let worker =
                start_with_metrics(connection, MeasuredSink::new(phone.epoch), diagnostics);
            phone.touch(0, ACTION_CANCEL);
            phone.ack(0).unwrap();
            for sequence in 1..=160 {
                let event_nanos = phone.now_nanos();
                let pressure_case = fault.starts_with("pressure-");
                let action = if pressure_case {
                    match sequence % 3 {
                        1 => ACTION_DOWN,
                        2 => ACTION_MOVE,
                        _ => ACTION_UP,
                    }
                } else if sequence % 2 == 0 {
                    ACTION_UP
                } else {
                    ACTION_DOWN
                };
                // Phone admission itself is exercised by PressureFilterTest. Here
                // the real wire/host path sees a rejected brush, admission, then lift.
                let suppressed = [TestContact {
                    flags: TIP[0].flags | CONTACT_FLAG_KEY_SUPPRESSED,
                    ..TIP[0]
                }];
                let mut sample = frame(action, event_nanos);
                if pressure_case && action == ACTION_DOWN {
                    sample.contacts = &suppressed;
                }
                let mut payload = [0; MAX_DATAGRAM_SIZE];
                let payload_length = encode_test_record(event_nanos, &[sample], &mut payload);
                let payload = &payload[..payload_length];
                // The timer includes both first encryption and each independent retry encryption.
                for copy in 0..2 {
                    let length = phone.seal(PHONE_TOUCH, SESSION, sequence, payload);
                    if fault.ends_with("both-lost") || (fault == "first-lost" && copy == 0) {
                        continue;
                    }
                    if fault == "corrupt-first" && copy == 0 {
                        phone.bytes[length - 1] ^= 1;
                    }
                    phone.socket.send(&phone.bytes[..length]).unwrap();
                }
                if fault.ends_with("both-lost") {
                    // Real OS scheduling instead of a busy spin. Android's actual repair
                    // selector is covered separately by SendQueueTest/device validation.
                    let due =
                        phone.epoch + Duration::from_nanos(event_nanos) + ANDROID_REPAIR_INTERVAL;
                    if let Some(wait) = due.checked_duration_since(Instant::now()) {
                        thread::sleep(wait);
                    }
                    phone.send(PHONE_TOUCH, SESSION, sequence, payload).unwrap();
                }
                phone.ack(sequence).unwrap();
            }
            phone.abort();
            let (mut sink, _) = worker.join().unwrap();
            assert_eq!(sink.latency_nanos.len(), 160);
            // Every gameplay sample, including startup, participates in the budget check.
            let samples = &mut sink.latency_nanos[..];
            samples.sort_unstable();
            let p99 = samples[(samples.len() * 99).div_ceil(100) - 1];
            let maximum = *samples.last().unwrap();
            eprintln!(
                "v6 production {fault} diagnostics={diagnostics}: n={} p50={:.3}ms observed_rank99={:.3}ms max={:.3}ms (OS acceptance simulated)",
                samples.len(),
                samples[samples.len() / 2] as f64 / 1e6,
                p99 as f64 / 1e6,
                maximum as f64 / 1e6
            );
            assert!(maximum <= 8_333_333, "{fault} exceeded one 120 Hz frame");
        }
    }
}
