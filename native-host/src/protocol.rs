use std::collections::BTreeMap;
use std::fmt;

pub const PROTOCOL_VERSION: u8 = 4;
pub const FRAME_MAGIC: [u8; 4] = *b"HPT4";
pub const CONTROL_MAGIC: [u8; 4] = *b"HPA4";
pub const DISCOVERY_MAGIC: [u8; 4] = *b"HPTD";
pub const DISCOVERY_VERSION: u8 = 1;
pub const DISCOVERY_HELLO: u8 = 1;
pub const DISCOVERY_ACK: u8 = 2;
pub const MESSAGE_TOUCH_FRAME: u8 = 1;
pub const CONTROL_HELLO: u8 = 1;
pub const CONTROL_ACK: u8 = 2;

pub const ACTION_HEARTBEAT: u8 = 0;
pub const ACTION_DOWN: u8 = 1;
pub const ACTION_MOVE: u8 = 2;
pub const ACTION_UP: u8 = 3;
pub const ACTION_CANCEL: u8 = 4;

pub const FRAME_FLAG_LOCKED: u8 = 0x01;
pub const FRAME_FLAG_SESSION_START: u8 = 0x02;
pub const FRAME_FLAG_HISTORICAL: u8 = 0x04;
pub const CONTACT_FLAG_INSIDE: u8 = 0x01;
pub const CONTACT_FLAG_TIP: u8 = 0x02;

pub const FRAME_HEADER_SIZE: usize = 68;
pub const CONTACT_SIZE: usize = 10;
pub const CRC_SIZE: usize = 4;
pub const CONTROL_SIZE: usize = 40;
pub const DISCOVERY_SIZE: usize = 32;
pub const MAX_CONTACTS: usize = 16;
pub const MAX_FRAME_SIZE: usize = FRAME_HEADER_SIZE + MAX_CONTACTS * CONTACT_SIZE + CRC_SIZE;

#[derive(Clone, Debug, PartialEq)]
pub struct Contact {
    pub pointer_id: u8,
    pub flags: u8,
    pub x: f32,
    pub y: f32,
    pub pressure: f32,
    pub touch_major: f32,
}

impl Contact {
    pub fn inside(&self) -> bool {
        self.flags & CONTACT_FLAG_INSIDE != 0
    }

    pub fn touching(&self) -> bool {
        self.flags & CONTACT_FLAG_TIP != 0
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TouchFrame {
    pub session_id: u64,
    pub sequence: u64,
    pub phone_event_nanos: u64,
    pub phone_callback_nanos: u64,
    pub phone_send_nanos: u64,
    pub echo_host_send_nanos: u64,
    pub phone_control_receive_nanos: u64,
    pub action: u8,
    pub action_pointer_id: u8,
    pub flags: u8,
    pub contacts: Vec<Contact>,
}

impl TouchFrame {
    pub fn locked(&self) -> bool {
        self.flags & FRAME_FLAG_LOCKED != 0
    }

    pub fn session_start(&self) -> bool {
        self.flags & FRAME_FLAG_SESSION_START != 0
    }

    pub fn historical(&self) -> bool {
        self.flags & FRAME_FLAG_HISTORICAL != 0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProtocolError {
    BadVersion(u8),
    BadMessageType(u8),
    BadLength(usize),
    BadContactCount(usize),
    BadCrc { expected: u32, actual: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiscoveryMessage {
    pub kind: u8,
    pub nonce: u64,
    pub session_id: u64,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadVersion(version) => {
                write!(formatter, "unsupported touch protocol v{version}")
            }
            Self::BadMessageType(message_type) => {
                write!(formatter, "unsupported message type {message_type}")
            }
            Self::BadLength(length) => {
                write!(formatter, "invalid touch frame length {length}")
            }
            Self::BadContactCount(count) => {
                write!(formatter, "invalid contact count {count}")
            }
            Self::BadCrc { expected, actual } => write!(
                formatter,
                "touch frame CRC mismatch: expected {expected:08x}, got {actual:08x}"
            ),
        }
    }
}

impl std::error::Error for ProtocolError {}

#[derive(Default)]
pub struct FrameParser {
    buffer: Vec<u8>,
    pub discarded_bytes: u64,
    pub connection_discarded_bytes: u64,
    pub invalid_frames: u64,
    incompatible_version: Option<u8>,
    seeking_connection_start: bool,
}

impl FrameParser {
    pub fn begin_connection(&mut self) {
        self.connection_discarded_bytes += self.buffer.len() as u64;
        self.buffer.clear();
        self.incompatible_version = None;
        self.seeking_connection_start = true;
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Result<TouchFrame, ProtocolError>> {
        self.buffer.extend_from_slice(bytes);
        if self.incompatible_version.is_none() {
            self.incompatible_version = self.buffer.windows(5).find_map(|window| {
                (window[..3] == *b"HPT"
                    && window[3].is_ascii_digit()
                    && window[4] != PROTOCOL_VERSION)
                    .then_some(window[4])
            });
        }
        let mut frames = Vec::new();

        loop {
            if self.buffer.len() < FRAME_HEADER_SIZE {
                break;
            }
            if self.buffer[..4] != FRAME_MAGIC {
                let offset = self.buffer[1..]
                    .windows(FRAME_MAGIC.len())
                    .position(|window| window == FRAME_MAGIC)
                    .map(|position| position + 1);
                match offset {
                    Some(offset) => {
                        self.record_discard(offset);
                        self.buffer.drain(..offset);
                    }
                    None => {
                        let retained = FRAME_MAGIC.len() - 1;
                        let discarded = self.buffer.len().saturating_sub(retained);
                        self.record_discard(discarded);
                        self.buffer.drain(..discarded);
                        break;
                    }
                }
                continue;
            }

            let length = u16::from_le_bytes([self.buffer[6], self.buffer[7]]) as usize;
            if !(FRAME_HEADER_SIZE + CRC_SIZE..=MAX_FRAME_SIZE).contains(&length) {
                self.invalid_frames += 1;
                frames.push(Err(ProtocolError::BadLength(length)));
                self.buffer.drain(..1);
                continue;
            }
            if self.buffer.len() < length {
                break;
            }
            let candidate: Vec<u8> = self.buffer.drain(..length).collect();
            let result = decode_frame(&candidate);
            if result.is_err() {
                self.invalid_frames += 1;
            } else {
                self.seeking_connection_start = false;
            }
            frames.push(result);
        }

        frames
    }

    /// Parse one complete UDP datagram without allowing a malformed packet to
    /// contaminate the next datagram. Stream-oriented callers continue to use
    /// the `feed` method above.
    pub fn feed_datagram(&mut self, bytes: &[u8]) -> Vec<Result<TouchFrame, ProtocolError>> {
        vec![self.decode_datagram(bytes)]
    }

    /// Decode one UDP datagram without allocating a stream-parser result
    /// vector or copying the datagram into the stream buffer.
    pub fn decode_datagram(&mut self, bytes: &[u8]) -> Result<TouchFrame, ProtocolError> {
        if bytes.len() >= FRAME_MAGIC.len() + 1
            && bytes[..FRAME_MAGIC.len()] == FRAME_MAGIC
            && bytes[FRAME_MAGIC.len()] != PROTOCOL_VERSION
        {
            self.incompatible_version = Some(bytes[FRAME_MAGIC.len()]);
        }
        let result = decode_frame(bytes);
        if result.is_err() {
            self.invalid_frames += 1;
        } else {
            self.seeking_connection_start = false;
        }
        result
    }

    pub fn take_incompatible_version(&mut self) -> Option<u8> {
        self.incompatible_version.take()
    }

    fn record_discard(&mut self, count: usize) {
        if self.seeking_connection_start {
            self.connection_discarded_bytes += count as u64;
        } else {
            self.discarded_bytes += count as u64;
        }
    }
}

pub fn decode_frame(bytes: &[u8]) -> Result<TouchFrame, ProtocolError> {
    if bytes.len() < FRAME_HEADER_SIZE + CRC_SIZE {
        return Err(ProtocolError::BadLength(bytes.len()));
    }
    let declared_length = read_u16(bytes, 6) as usize;
    if declared_length != bytes.len() || bytes.len() > MAX_FRAME_SIZE {
        return Err(ProtocolError::BadLength(declared_length));
    }
    if bytes[4] != PROTOCOL_VERSION {
        return Err(ProtocolError::BadVersion(bytes[4]));
    }
    if bytes[5] != MESSAGE_TOUCH_FRAME {
        return Err(ProtocolError::BadMessageType(bytes[5]));
    }

    let expected_crc = read_u32(bytes, bytes.len() - CRC_SIZE);
    let actual_crc = crc32(&bytes[..bytes.len() - CRC_SIZE]);
    if expected_crc != actual_crc {
        return Err(ProtocolError::BadCrc {
            expected: expected_crc,
            actual: actual_crc,
        });
    }

    let contact_count = bytes[66] as usize;
    if contact_count > MAX_CONTACTS {
        return Err(ProtocolError::BadContactCount(contact_count));
    }
    let expected_length = FRAME_HEADER_SIZE + contact_count * CONTACT_SIZE + CRC_SIZE;
    if bytes.len() != expected_length {
        return Err(ProtocolError::BadLength(bytes.len()));
    }

    let mut contacts = Vec::with_capacity(contact_count);
    for index in 0..contact_count {
        let offset = FRAME_HEADER_SIZE + index * CONTACT_SIZE;
        contacts.push(Contact {
            pointer_id: bytes[offset],
            flags: bytes[offset + 1],
            x: read_i16(bytes, offset + 2) as f32 / 10_000.0,
            y: read_i16(bytes, offset + 4) as f32 / 10_000.0,
            pressure: read_u16(bytes, offset + 6) as f32 / 65_535.0,
            touch_major: read_u16(bytes, offset + 8) as f32 / 65_535.0,
        });
    }

    Ok(TouchFrame {
        session_id: read_u64(bytes, 8),
        sequence: read_u64(bytes, 16),
        phone_event_nanos: read_u64(bytes, 24),
        phone_callback_nanos: read_u64(bytes, 32),
        phone_send_nanos: read_u64(bytes, 40),
        echo_host_send_nanos: read_u64(bytes, 48),
        phone_control_receive_nanos: read_u64(bytes, 56),
        action: bytes[64],
        action_pointer_id: bytes[65],
        flags: bytes[67],
        contacts,
    })
}

pub fn encode_control(
    control_type: u8,
    lane_count: u8,
    session_id: u64,
    acknowledged_sequence: Option<u64>,
    receive_window: u32,
    host_send_nanos: u64,
) -> [u8; CONTROL_SIZE] {
    let mut bytes = [0_u8; CONTROL_SIZE];
    bytes[..4].copy_from_slice(&CONTROL_MAGIC);
    bytes[4] = PROTOCOL_VERSION;
    bytes[5] = control_type;
    bytes[6..8].copy_from_slice(&(lane_count as u16).to_le_bytes());
    bytes[8..16].copy_from_slice(&session_id.to_le_bytes());
    bytes[16..24].copy_from_slice(&acknowledged_sequence.unwrap_or(u64::MAX).to_le_bytes());
    bytes[24..28].copy_from_slice(&receive_window.to_le_bytes());
    bytes[28..36].copy_from_slice(&host_send_nanos.to_le_bytes());
    let checksum = crc32(&bytes[..36]);
    bytes[36..40].copy_from_slice(&checksum.to_le_bytes());
    bytes
}

pub fn encode_discovery(kind: u8, nonce: u64, session_id: u64) -> [u8; DISCOVERY_SIZE] {
    let mut bytes = [0_u8; DISCOVERY_SIZE];
    bytes[..4].copy_from_slice(&DISCOVERY_MAGIC);
    bytes[4] = DISCOVERY_VERSION;
    bytes[5] = kind;
    bytes[8..16].copy_from_slice(&nonce.to_le_bytes());
    bytes[16..24].copy_from_slice(&session_id.to_le_bytes());
    let checksum = crc32(&bytes[..28]);
    bytes[28..32].copy_from_slice(&checksum.to_le_bytes());
    bytes
}

pub fn decode_discovery(bytes: &[u8]) -> Option<DiscoveryMessage> {
    if bytes.len() != DISCOVERY_SIZE
        || bytes[..4] != DISCOVERY_MAGIC
        || bytes[4] != DISCOVERY_VERSION
        || !matches!(bytes[5], DISCOVERY_HELLO | DISCOVERY_ACK)
        || read_u32(bytes, 28) != crc32(&bytes[..28])
    {
        return None;
    }
    Some(DiscoveryMessage {
        kind: bytes[5],
        nonce: read_u64(bytes, 8),
        session_id: read_u64(bytes, 16),
    })
}

pub struct OrderedFrames {
    session_id: Option<u64>,
    next_sequence: u64,
    buffered: BTreeMap<u64, TouchFrame>,
}

impl OrderedFrames {
    pub fn new() -> Self {
        Self {
            session_id: None,
            next_sequence: 0,
            buffered: BTreeMap::new(),
        }
    }

    pub fn begin_session(&mut self, frame: &TouchFrame) {
        self.session_id = Some(frame.session_id);
        self.next_sequence = frame.sequence;
        self.buffered.clear();
    }

    pub fn session_id(&self) -> Option<u64> {
        self.session_id
    }

    pub fn acknowledged_sequence(&self) -> Option<u64> {
        self.next_sequence.checked_sub(1)
    }

    pub fn expected_sequence(&self) -> u64 {
        self.next_sequence
    }

    pub fn contains_sequence(&self, sequence: u64) -> bool {
        self.buffered.contains_key(&sequence)
    }

    pub fn push(&mut self, frame: TouchFrame) {
        if self.session_id.is_none() {
            // A restarted host may join an Android session whose original
            // session-start frame was already acknowledged by the previous
            // process. The oldest replayed frame is the safe new boundary.
            self.begin_session(&frame);
        } else if self.session_id != Some(frame.session_id) {
            if frame.session_start() {
                self.begin_session(&frame);
            } else {
                return;
            }
        }
        if frame.sequence < self.next_sequence {
            return;
        }
        self.buffered.entry(frame.sequence).or_insert(frame);
    }

    pub fn next_ready(&self) -> Option<&TouchFrame> {
        self.buffered.get(&self.next_sequence)
    }

    /// Commit only after the OS sink accepted this exact frame. This is the
    /// durability boundary used for the cumulative ACK sent to Android.
    pub fn commit_ready(&mut self) -> bool {
        if self.buffered.remove(&self.next_sequence).is_none() {
            return false;
        }
        self.next_sequence = self.next_sequence.wrapping_add(1);
        true
    }
}

impl Default for OrderedFrames {
    fn default() -> Self {
        Self::new()
    }
}

pub fn crc32(bytes: &[u8]) -> u32 {
    let mut value = 0xffff_ffff_u32;
    for byte in bytes {
        value ^= u32::from(*byte);
        for _ in 0..8 {
            value = if value & 1 != 0 {
                (value >> 1) ^ 0xedb8_8320
            } else {
                value >> 1
            };
        }
    }
    !value
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn read_i16(bytes: &[u8], offset: usize) -> i16 {
    i16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(session_id: u64, sequence: u64, action: u8, flags: u8, x: i16) -> Vec<u8> {
        let length = FRAME_HEADER_SIZE + CONTACT_SIZE + CRC_SIZE;
        let mut bytes = vec![0_u8; length];
        bytes[..4].copy_from_slice(&FRAME_MAGIC);
        bytes[4] = PROTOCOL_VERSION;
        bytes[5] = MESSAGE_TOUCH_FRAME;
        bytes[6..8].copy_from_slice(&(length as u16).to_le_bytes());
        bytes[8..16].copy_from_slice(&session_id.to_le_bytes());
        bytes[16..24].copy_from_slice(&sequence.to_le_bytes());
        bytes[24..32].copy_from_slice(&123_456_789_u64.to_le_bytes());
        bytes[32..40].copy_from_slice(&123_456_999_u64.to_le_bytes());
        bytes[40..48].copy_from_slice(&123_457_111_u64.to_le_bytes());
        bytes[48..56].copy_from_slice(&98_000_u64.to_le_bytes());
        bytes[56..64].copy_from_slice(&123_457_000_u64.to_le_bytes());
        bytes[64] = action;
        bytes[65] = 7;
        bytes[66] = 1;
        bytes[67] = flags;
        bytes[68] = 7;
        bytes[69] = CONTACT_FLAG_INSIDE | CONTACT_FLAG_TIP;
        bytes[70..72].copy_from_slice(&x.to_le_bytes());
        bytes[72..74].copy_from_slice(&2_500_i16.to_le_bytes());
        bytes[74..76].copy_from_slice(&32_768_u16.to_le_bytes());
        bytes[76..78].copy_from_slice(&8_192_u16.to_le_bytes());
        let checksum = crc32(&bytes[..length - CRC_SIZE]);
        bytes[length - CRC_SIZE..].copy_from_slice(&checksum.to_le_bytes());
        bytes
    }

    #[test]
    fn decodes_complete_touch_frame() {
        let bytes = packet(
            44,
            9,
            ACTION_MOVE,
            FRAME_FLAG_LOCKED | FRAME_FLAG_HISTORICAL,
            7_500,
        );
        let frame = decode_frame(&bytes).unwrap();
        assert_eq!(frame.session_id, 44);
        assert_eq!(frame.sequence, 9);
        assert_eq!(frame.phone_callback_nanos, 123_456_999);
        assert_eq!(frame.phone_send_nanos, 123_457_111);
        assert!(frame.locked());
        assert!(frame.historical());
        assert_eq!(frame.contacts.len(), 1);
        assert!((frame.contacts[0].x - 0.75).abs() < 0.0001);
        assert!(frame.contacts[0].touching());
    }

    #[test]
    fn parser_handles_fragmentation_noise_and_concatenation() {
        let first = packet(8, 0, ACTION_CANCEL, FRAME_FLAG_SESSION_START, 0);
        let second = packet(8, 1, ACTION_DOWN, FRAME_FLAG_LOCKED, 5_000);
        let mut parser = FrameParser::default();
        assert!(parser.feed(b"xy").is_empty());
        assert!(parser.feed(&first[..17]).is_empty());
        let mut tail = first[17..].to_vec();
        tail.extend_from_slice(&second);
        let decoded = parser.feed(&tail);
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[0].as_ref().unwrap().sequence, 0);
        assert_eq!(decoded[1].as_ref().unwrap().sequence, 1);
        assert_eq!(parser.discarded_bytes, 2);
    }

    #[test]
    fn parser_identifies_an_old_wire_version() {
        let mut parser = FrameParser::default();
        assert!(parser.feed(b"HPT3\x03").is_empty());
        assert_eq!(parser.take_incompatible_version(), Some(3));
    }

    #[test]
    fn connection_start_resync_is_separate_from_stream_discard() {
        let bytes = packet(8, 0, ACTION_CANCEL, FRAME_FLAG_SESSION_START, 0);
        let mut parser = FrameParser::default();
        parser.begin_connection();
        let mut stream = b"old-tail".to_vec();
        stream.extend(bytes);
        assert_eq!(parser.feed(&stream).len(), 1);
        assert_eq!(parser.connection_discarded_bytes, 8);
        assert_eq!(parser.discarded_bytes, 0);
    }

    #[test]
    fn crc_rejects_corruption() {
        let mut bytes = packet(1, 0, ACTION_DOWN, 0, 5_000);
        bytes[70] ^= 0x40;
        assert!(matches!(
            decode_frame(&bytes),
            Err(ProtocolError::BadCrc { .. })
        ));
    }

    #[test]
    fn crc_matches_ieee_reference_vector() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn ordered_frames_deduplicates_and_fills_gap() {
        let start =
            decode_frame(&packet(5, 0, ACTION_CANCEL, FRAME_FLAG_SESSION_START, 0)).unwrap();
        let one = decode_frame(&packet(5, 1, ACTION_DOWN, 0, 2_000)).unwrap();
        let two = decode_frame(&packet(5, 2, ACTION_MOVE, 0, 4_000)).unwrap();
        let mut ordered = OrderedFrames::new();
        ordered.push(start);
        assert_eq!(ordered.next_ready().unwrap().sequence, 0);
        assert_eq!(ordered.acknowledged_sequence(), None);
        assert!(ordered.commit_ready());
        ordered.push(two.clone());
        assert!(ordered.next_ready().is_none());
        ordered.push(one.clone());
        assert_eq!(ordered.next_ready(), Some(&one));
        assert!(ordered.commit_ready());
        assert_eq!(ordered.next_ready(), Some(&two));
        assert!(ordered.commit_ready());
        ordered.push(one);
        assert!(ordered.next_ready().is_none());
        assert_eq!(ordered.acknowledged_sequence(), Some(2));
    }

    #[test]
    fn first_replayed_frame_bootstraps_a_restarted_host() {
        let replay = decode_frame(&packet(77, 42, ACTION_MOVE, FRAME_FLAG_LOCKED, 5_000)).unwrap();
        let mut ordered = OrderedFrames::new();
        ordered.push(replay);
        assert_eq!(ordered.session_id(), Some(77));
        assert_eq!(ordered.expected_sequence(), 42);
        assert_eq!(ordered.next_ready().unwrap().sequence, 42);
        assert!(ordered.commit_ready());
        assert_eq!(ordered.acknowledged_sequence(), Some(42));
    }

    #[test]
    fn session_start_cancel_switches_to_a_fresh_live_session() {
        let old_start =
            decode_frame(&packet(10, 7, ACTION_CANCEL, FRAME_FLAG_SESSION_START, 0)).unwrap();
        let fresh_start =
            decode_frame(&packet(11, 0, ACTION_CANCEL, FRAME_FLAG_SESSION_START, 0)).unwrap();
        let fresh_move =
            decode_frame(&packet(11, 1, ACTION_MOVE, FRAME_FLAG_LOCKED, 4_000)).unwrap();

        let mut ordered = OrderedFrames::new();
        ordered.push(old_start);
        assert!(ordered.commit_ready());
        ordered.push(fresh_start);
        assert_eq!(ordered.session_id(), Some(11));
        assert_eq!(ordered.expected_sequence(), 0);
        assert_eq!(ordered.next_ready().unwrap().action, ACTION_CANCEL);
        assert!(ordered.commit_ready());
        ordered.push(fresh_move);
        assert_eq!(ordered.next_ready().unwrap().sequence, 1);
    }

    #[test]
    fn control_record_has_valid_crc_and_sentinel_ack() {
        let control = encode_control(CONTROL_HELLO, 6, 99, None, 64, 123_000);
        assert_eq!(&control[..4], &CONTROL_MAGIC);
        assert_eq!(read_u64(&control, 8), 99);
        assert_eq!(read_u64(&control, 16), u64::MAX);
        assert_eq!(read_u64(&control, 28), 123_000);
        assert_eq!(read_u32(&control, 36), crc32(&control[..36]));
    }

    #[test]
    fn discovery_record_round_trips_with_crc() {
        let bytes = encode_discovery(DISCOVERY_HELLO, 123, 456);
        assert_eq!(
            decode_discovery(&bytes),
            Some(DiscoveryMessage {
                kind: DISCOVERY_HELLO,
                nonce: 123,
                session_id: 456,
            })
        );

        let mut corrupted = bytes;
        corrupted[16] ^= 1;
        assert_eq!(decode_discovery(&corrupted), None);
    }
}
