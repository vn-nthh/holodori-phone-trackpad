use std::fmt;
use std::ops::Deref;

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
pub const MAX_REORDERED_FRAMES: usize = 256;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Contact {
    pub pointer_id: u8,
    pub flags: u8,
    pub x: f32,
    pub y: f32,
    pub pressure: f32,
    pub touch_major: f32,
}

/// A complete snapshot has a protocol-defined maximum, so it never needs a heap allocation.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Contacts {
    items: [Contact; MAX_CONTACTS],
    len: usize,
}

impl Contacts {
    pub fn push(&mut self, contact: Contact) {
        self.items[self.len] = contact;
        self.len += 1;
    }
}

impl Deref for Contacts {
    type Target = [Contact];

    fn deref(&self) -> &Self::Target {
        &self.items[..self.len]
    }
}

impl<'a> IntoIterator for &'a Contacts {
    type Item = &'a Contact;
    type IntoIter = std::slice::Iter<'a, Contact>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl FromIterator<Contact> for Contacts {
    fn from_iter<T: IntoIterator<Item = Contact>>(iter: T) -> Self {
        let mut contacts = Self::default();
        for contact in iter {
            contacts.push(contact);
        }
        contacts
    }
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
    pub contacts: Contacts,
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
    BadMagic,
    BadVersion(u8),
    BadMessageType(u8),
    BadAction(u8),
    BadLength(usize),
    BadContactCount(usize),
    DuplicatePointerId(u8),
    BadCrc { expected: u32, actual: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiscoveryMessage {
    pub kind: u8,
    pub nonce: u64,
    pub session_id: u64,
    pub port: u16,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic => formatter.write_str("invalid touch frame magic"),
            Self::BadVersion(version) => {
                write!(formatter, "unsupported touch protocol v{version}")
            }
            Self::BadMessageType(message_type) => {
                write!(formatter, "unsupported message type {message_type}")
            }
            Self::BadAction(action) => {
                write!(formatter, "unsupported touch action {action}")
            }
            Self::BadLength(length) => {
                write!(formatter, "invalid touch frame length {length}")
            }
            Self::BadContactCount(count) => {
                write!(formatter, "invalid contact count {count}")
            }
            Self::DuplicatePointerId(pointer_id) => {
                write!(formatter, "duplicate pointer ID {pointer_id}")
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
    pub discarded_bytes: u64,
    pub connection_discarded_bytes: u64,
    pub invalid_frames: u64,
    incompatible_version: Option<u8>,
}

impl FrameParser {
    pub fn begin_connection(&mut self) {
        self.incompatible_version = None;
    }

    /// Decode one UDP datagram without allocating a stream-parser result
    /// vector or copying the datagram into the stream buffer.
    pub fn decode_datagram(&mut self, bytes: &[u8]) -> Result<TouchFrame, ProtocolError> {
        if bytes.len() > FRAME_MAGIC.len()
            && bytes[..FRAME_MAGIC.len()] == FRAME_MAGIC
            && bytes[FRAME_MAGIC.len()] != PROTOCOL_VERSION
        {
            self.incompatible_version = Some(bytes[FRAME_MAGIC.len()]);
        }
        let result = decode_frame(bytes);
        if result.is_err() {
            self.invalid_frames += 1;
        }
        result
    }

    pub fn take_incompatible_version(&mut self) -> Option<u8> {
        self.incompatible_version.take()
    }
}

pub fn decode_frame(bytes: &[u8]) -> Result<TouchFrame, ProtocolError> {
    if bytes.len() < FRAME_HEADER_SIZE + CRC_SIZE {
        return Err(ProtocolError::BadLength(bytes.len()));
    }
    if bytes[..FRAME_MAGIC.len()] != FRAME_MAGIC {
        return Err(ProtocolError::BadMagic);
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
    if !matches!(
        bytes[64],
        ACTION_HEARTBEAT | ACTION_DOWN | ACTION_MOVE | ACTION_UP | ACTION_CANCEL
    ) {
        return Err(ProtocolError::BadAction(bytes[64]));
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

    let mut contacts = Contacts::default();
    let mut pointer_ids = [false; u8::MAX as usize + 1];
    for index in 0..contact_count {
        let offset = FRAME_HEADER_SIZE + index * CONTACT_SIZE;
        let pointer_id = bytes[offset];
        if pointer_ids[pointer_id as usize] {
            return Err(ProtocolError::DuplicatePointerId(pointer_id));
        }
        pointer_ids[pointer_id as usize] = true;
        contacts.push(Contact {
            pointer_id,
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

pub fn encode_discovery(kind: u8, nonce: u64, session_id: u64, port: u16) -> [u8; DISCOVERY_SIZE] {
    let mut bytes = [0_u8; DISCOVERY_SIZE];
    bytes[..4].copy_from_slice(&DISCOVERY_MAGIC);
    bytes[4] = DISCOVERY_VERSION;
    bytes[5] = kind;
    bytes[8..16].copy_from_slice(&nonce.to_le_bytes());
    bytes[16..24].copy_from_slice(&session_id.to_le_bytes());
    bytes[24..26].copy_from_slice(&port.to_le_bytes());
    let checksum = crc32(&bytes[..28]);
    bytes[28..32].copy_from_slice(&checksum.to_le_bytes());
    bytes
}

pub fn discovery_port_acceptable(advertised_port: u16, listening_port: u16) -> bool {
    advertised_port == 0 || advertised_port == listening_port
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
        port: read_u16(bytes, 24),
    })
}

pub struct OrderedFrames {
    session_id: Option<u64>,
    next_sequence: u64,
    buffered: Box<[Option<TouchFrame>]>,
    requires_session_start: bool,
}

impl OrderedFrames {
    pub fn new() -> Self {
        Self {
            session_id: None,
            next_sequence: 0,
            buffered: vec![None; MAX_REORDERED_FRAMES].into_boxed_slice(),
            requires_session_start: false,
        }
    }

    pub fn begin_session(&mut self, frame: &TouchFrame) {
        self.session_id = Some(frame.session_id);
        self.next_sequence = frame.sequence;
        self.buffered.fill(None);
        self.requires_session_start = false;
    }

    /// Abandon all input from the current phone session after its heartbeat
    /// disappears. Until Android sends a new session-start CANCEL, delayed
    /// gameplay from the dead session must not be injected or acknowledged.
    pub fn require_fresh_session(&mut self) {
        self.session_id = None;
        self.next_sequence = 0;
        self.buffered.fill(None);
        self.requires_session_start = true;
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
        self.buffered[sequence as usize % MAX_REORDERED_FRAMES]
            .as_ref()
            .is_some_and(|frame| frame.sequence == sequence)
    }

    pub fn push(&mut self, frame: TouchFrame) {
        if self.requires_session_start && !frame.session_start() {
            return;
        }
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
        if frame.sequence - self.next_sequence >= MAX_REORDERED_FRAMES as u64 {
            return;
        }
        let slot = &mut self.buffered[frame.sequence as usize % MAX_REORDERED_FRAMES];
        if slot.is_none() {
            *slot = Some(frame);
        }
    }

    pub fn next_ready(&self) -> Option<&TouchFrame> {
        self.buffered[self.next_sequence as usize % MAX_REORDERED_FRAMES]
            .as_ref()
            .filter(|frame| frame.sequence == self.next_sequence)
    }

    /// Commit only after the OS sink accepted this exact frame. This is the
    /// durability boundary used for the cumulative ACK sent to Android.
    pub fn commit_ready(&mut self) -> bool {
        if self.buffered[self.next_sequence as usize % MAX_REORDERED_FRAMES]
            .take()
            .is_none()
        {
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
    fn parser_identifies_an_incompatible_wire_version() {
        let mut parser = FrameParser::default();
        assert!(parser.decode_datagram(b"HPT4\x05").is_err());
        assert_eq!(parser.take_incompatible_version(), Some(5));
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
    fn datagram_decoder_rejects_bad_magic_and_unknown_actions() {
        let mut bad_magic = packet(1, 0, ACTION_DOWN, 0, 5_000);
        bad_magic[..4].copy_from_slice(b"NOPE");
        assert_eq!(decode_frame(&bad_magic), Err(ProtocolError::BadMagic));

        let mut bad_action = packet(1, 0, 99, 0, 5_000);
        let checksum = crc32(&bad_action[..bad_action.len() - CRC_SIZE]);
        let crc_offset = bad_action.len() - CRC_SIZE;
        bad_action[crc_offset..].copy_from_slice(&checksum.to_le_bytes());
        assert_eq!(decode_frame(&bad_action), Err(ProtocolError::BadAction(99)));
    }

    #[test]
    fn datagram_decoder_rejects_duplicate_pointer_ids() {
        let mut bytes = packet(1, 0, ACTION_DOWN, 0, 5_000);
        let length = FRAME_HEADER_SIZE + 2 * CONTACT_SIZE + CRC_SIZE;
        bytes.resize(length, 0);
        bytes[6..8].copy_from_slice(&(length as u16).to_le_bytes());
        bytes[66] = 2;
        bytes.copy_within(FRAME_HEADER_SIZE..FRAME_HEADER_SIZE + CONTACT_SIZE, 78);
        let checksum = crc32(&bytes[..length - CRC_SIZE]);
        bytes[length - CRC_SIZE..].copy_from_slice(&checksum.to_le_bytes());
        assert_eq!(
            decode_frame(&bytes),
            Err(ProtocolError::DuplicatePointerId(7))
        );
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
    fn ordered_frames_bounds_far_future_datagrams() {
        let start =
            decode_frame(&packet(5, 0, ACTION_CANCEL, FRAME_FLAG_SESSION_START, 0)).unwrap();
        let too_far = decode_frame(&packet(
            5,
            MAX_REORDERED_FRAMES as u64 + 1,
            ACTION_MOVE,
            FRAME_FLAG_LOCKED,
            4_000,
        ))
        .unwrap();
        let mut ordered = OrderedFrames::new();
        ordered.push(start);
        assert!(ordered.commit_ready());
        ordered.push(too_far);
        assert!(ordered.buffered.iter().all(Option::is_none));
        assert_eq!(ordered.expected_sequence(), 1);
    }

    #[test]
    fn reordered_bursts_survive_ring_reuse_without_overwriting_a_gap() {
        let mut ordered = OrderedFrames::new();
        ordered
            .push(decode_frame(&packet(5, 0, ACTION_CANCEL, FRAME_FLAG_SESSION_START, 0)).unwrap());
        assert!(ordered.commit_ready());
        for round in 0..8 {
            let first = 1 + round * MAX_REORDERED_FRAMES as u64;
            let end = first + MAX_REORDERED_FRAMES as u64;
            for sequence in (first + 1..end).rev() {
                ordered.push(
                    decode_frame(&packet(5, sequence, ACTION_MOVE, FRAME_FLAG_LOCKED, 4_000))
                        .unwrap(),
                );
            }
            ordered.push(
                decode_frame(&packet(5, end, ACTION_MOVE, FRAME_FLAG_LOCKED, 4_000)).unwrap(),
            );
            assert!(ordered.next_ready().is_none());
            ordered.push(
                decode_frame(&packet(5, first, ACTION_MOVE, FRAME_FLAG_LOCKED, 4_000)).unwrap(),
            );
            for sequence in first..end {
                assert_eq!(ordered.next_ready().unwrap().sequence, sequence);
                assert!(ordered.commit_ready());
            }
            assert_eq!(ordered.acknowledged_sequence(), Some(end - 1));
            assert!(ordered.next_ready().is_none());
        }
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
    fn liveness_failure_rejects_delayed_gameplay_until_a_fresh_session() {
        let start =
            decode_frame(&packet(10, 0, ACTION_CANCEL, FRAME_FLAG_SESSION_START, 0)).unwrap();
        let delayed = decode_frame(&packet(10, 1, ACTION_MOVE, FRAME_FLAG_LOCKED, 4_000)).unwrap();
        let fresh_start =
            decode_frame(&packet(11, 0, ACTION_CANCEL, FRAME_FLAG_SESSION_START, 0)).unwrap();

        let mut ordered = OrderedFrames::new();
        ordered.push(start);
        assert!(ordered.commit_ready());
        ordered.require_fresh_session();
        ordered.push(delayed);
        assert!(ordered.session_id().is_none());
        assert!(ordered.next_ready().is_none());

        ordered.push(fresh_start);
        assert_eq!(ordered.session_id(), Some(11));
        assert_eq!(ordered.next_ready().unwrap().sequence, 0);
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
        let bytes = encode_discovery(DISCOVERY_HELLO, 123, 456, 42_825);
        assert_eq!(
            decode_discovery(&bytes),
            Some(DiscoveryMessage {
                kind: DISCOVERY_HELLO,
                nonce: 123,
                session_id: 456,
                port: 42_825,
            })
        );

        let mut corrupted = bytes;
        corrupted[16] ^= 1;
        assert_eq!(decode_discovery(&corrupted), None);
    }

    #[test]
    fn discovery_port_accepts_legacy_zero_and_the_listening_port() {
        assert!(discovery_port_acceptable(0, 42_825));
        assert!(discovery_port_acceptable(42_825, 42_825));
        assert!(!discovery_port_acceptable(9, 42_825));

        let legacy = encode_discovery(DISCOVERY_ACK, 1, 2, 0);
        assert_eq!(decode_discovery(&legacy).unwrap().port, 0);
    }
}
