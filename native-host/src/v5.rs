//! Protocol-v5 framing, authenticated records, replay defense, and SAS logic.
//!
//! Noise owns handshake key derivation. This module only consumes the two
//! split keys so the public UDP header can be authenticated as AEAD associated
//! data, which `snow`'s transport convenience type does not expose.

use std::fmt;

use blake2::{Blake2s256, Digest};
use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce, Tag};
use snow::HandshakeState;
use zeroize::{Zeroize, Zeroizing};

use crate::protocol::{
    ACTION_CANCEL, ACTION_DOWN, ACTION_HEARTBEAT, ACTION_MOVE, ACTION_UP, CONTACT_FLAG_INSIDE,
    CONTACT_FLAG_TIP, Contact, Contacts, FRAME_FLAG_HISTORICAL, FRAME_FLAG_LOCKED,
    FRAME_FLAG_SESSION_START, MAX_CONTACTS, TouchFrame,
};

pub const PROTOCOL_VERSION: u8 = 5;
pub const UDP_PORT: u16 = 42_825;
pub const PAIR_MAGIC: [u8; 4] = *b"HPP5";
pub const PHONE_MAGIC: [u8; 4] = *b"HPT5";
pub const HOST_MAGIC: [u8; 4] = *b"HPA5";
pub const PAIR_HEADER_SIZE: usize = 32;
pub const RECORD_HEADER_SIZE: usize = 48;
pub const TAG_SIZE: usize = 16;
pub const MAX_DATAGRAM_SIZE: usize = 1_200;
pub const TOUCH_PAYLOAD_HEADER_SIZE: usize = 44;
pub const CONTACT_SIZE: usize = 10;
pub const CONTROL_PAYLOAD_SIZE: usize = 16;
pub const REPLAY_WINDOW_BITS: usize = 1_024;

pub const PAIR_PROBE: u8 = 1;
pub const PAIR_OFFER: u8 = 2;
pub const PAIR_CONTINUE: u8 = 3;
pub const PAIR_ABORT: u8 = 4;
pub const IK_MESSAGE_1: u8 = 5;
pub const IK_CONTINUE: u8 = 6;

pub const PHONE_TOUCH: u8 = 1;
pub const PHONE_QUALITY_REPLY: u8 = 2;
pub const PHONE_SAS_COMMITMENT: u8 = 3;
pub const PHONE_SAS_REVEAL: u8 = 4;
pub const PHONE_PAIR_CONFIRM: u8 = 5;
pub const PHONE_AUTH_ABORT: u8 = 6;
pub const PHONE_PING: u8 = 7;

pub const HOST_HELLO: u8 = 1;
pub const HOST_ACK: u8 = 2;
pub const HOST_QUALITY_PROBE: u8 = 3;
pub const HOST_SAS_COMMITMENT: u8 = 4;
pub const HOST_SAS_REVEAL: u8 = 5;
pub const HOST_PAIR_COMPLETE: u8 = 6;
pub const HOST_AUTH_ABORT: u8 = 7;
pub const HOST_PONG: u8 = 8;

pub const QUALITY_REPAIR_ONLY: u32 = 0x01;
pub const NO_ACK: u64 = u64::MAX;

const VALID_FRAME_FLAGS: u8 = FRAME_FLAG_LOCKED | FRAME_FLAG_SESSION_START | FRAME_FLAG_HISTORICAL;
const VALID_CONTACT_FLAGS: u8 = CONTACT_FLAG_INSIDE | CONTACT_FLAG_TIP;
const NOISE_PROLOGUE_PREFIX: &[u8] = b"holodori-phone-trackpad-v5\0";
const CONNECTION_DOMAIN: &[u8] = b"holodori-v5-connection";
const SAS_COMMIT_DOMAIN: &[u8] = b"holodori-v5-sas-commit";
const SAS_DOMAIN: &[u8] = b"holodori-v5-sas";
const SAS_RETRY_DOMAIN: &[u8] = b"holodori-v5-sas-retry";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum TransportKind {
    Usb = 1,
    Wifi = 2,
}

impl TransportKind {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "usb" => Some(Self::Usb),
            "wifi" => Some(Self::Wifi),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Usb => "USB",
            Self::Wifi => "Wi-Fi / local network",
        }
    }
}

impl TryFrom<u8> for TransportKind {
    type Error = WireError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Usb),
            2 => Ok(Self::Wifi),
            _ => Err(WireError::BadTransport(value)),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    PhoneToHost,
    HostToPhone,
}

impl Direction {
    fn magic(self) -> [u8; 4] {
        match self {
            Self::PhoneToHost => PHONE_MAGIC,
            Self::HostToPhone => HOST_MAGIC,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PairEnvelope<'a> {
    pub kind: u8,
    pub exchange_id: [u8; 16],
    pub step: u32,
    pub transport: TransportKind,
    pub payload: &'a [u8],
}

pub fn encode_pair_envelope(
    kind: u8,
    exchange_id: [u8; 16],
    step: u32,
    transport: TransportKind,
    payload: &[u8],
) -> Result<Vec<u8>, WireError> {
    let length = PAIR_HEADER_SIZE
        .checked_add(payload.len())
        .ok_or(WireError::BadLength(payload.len()))?;
    if length > MAX_DATAGRAM_SIZE || payload.len() > u16::MAX as usize {
        return Err(WireError::BadLength(length));
    }
    let mut bytes = vec![0_u8; length];
    bytes[..4].copy_from_slice(&PAIR_MAGIC);
    bytes[4] = PROTOCOL_VERSION;
    bytes[5] = kind;
    bytes[6..8].copy_from_slice(&(length as u16).to_le_bytes());
    bytes[8..24].copy_from_slice(&exchange_id);
    bytes[24..28].copy_from_slice(&step.to_le_bytes());
    bytes[28..30].copy_from_slice(&(payload.len() as u16).to_le_bytes());
    bytes[30] = transport as u8;
    bytes[31] = 1;
    bytes[PAIR_HEADER_SIZE..].copy_from_slice(payload);
    Ok(bytes)
}

pub fn decode_pair_envelope(bytes: &[u8]) -> Result<PairEnvelope<'_>, WireError> {
    if bytes.len() < PAIR_HEADER_SIZE || bytes.len() > MAX_DATAGRAM_SIZE {
        return Err(WireError::BadLength(bytes.len()));
    }
    if bytes[..4] != PAIR_MAGIC {
        return Err(WireError::BadMagic);
    }
    if bytes[4] != PROTOCOL_VERSION {
        return Err(WireError::BadVersion(bytes[4]));
    }
    let declared = read_u16(bytes, 6) as usize;
    let payload_length = read_u16(bytes, 28) as usize;
    if declared != bytes.len() || PAIR_HEADER_SIZE + payload_length != bytes.len() {
        return Err(WireError::BadLength(declared));
    }
    if bytes[31] != 1 {
        return Err(WireError::BadSuite(bytes[31]));
    }
    let mut exchange_id = [0_u8; 16];
    exchange_id.copy_from_slice(&bytes[8..24]);
    Ok(PairEnvelope {
        kind: bytes[5],
        exchange_id,
        step: read_u32(bytes, 24),
        transport: bytes[30].try_into()?,
        payload: &bytes[PAIR_HEADER_SIZE..],
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordHeader {
    pub direction: Direction,
    pub message_type: u8,
    pub connection_id: u64,
    pub session_id: u64,
    pub packet_number: u64,
    pub logical_id: u64,
    pub flags: u32,
    pub payload_length: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenedRecord {
    pub header: RecordHeader,
    pub payload: Vec<u8>,
}

/// Directional keys and packet state for one completed Noise handshake.
pub struct RecordCipher {
    send_key: Zeroizing<[u8; 32]>,
    receive_key: Zeroizing<[u8; 32]>,
    connection_id: u64,
    next_packet_number: u64,
    replay: ReplayWindow,
}

impl RecordCipher {
    pub fn from_handshake(
        handshake: &mut HandshakeState,
        local_is_initiator: bool,
    ) -> Result<Self, WireError> {
        if !handshake.is_handshake_finished() {
            return Err(WireError::HandshakeIncomplete);
        }
        let connection_id = connection_id(handshake.get_handshake_hash());
        let (initiator_key, responder_key) = handshake.dangerously_get_raw_split();
        let (send_key, receive_key) = if local_is_initiator {
            (initiator_key, responder_key)
        } else {
            (responder_key, initiator_key)
        };
        Ok(Self::from_keys(send_key, receive_key, connection_id))
    }

    fn from_keys(send_key: [u8; 32], receive_key: [u8; 32], connection_id: u64) -> Self {
        Self {
            send_key: Zeroizing::new(send_key),
            receive_key: Zeroizing::new(receive_key),
            connection_id,
            next_packet_number: 0,
            replay: ReplayWindow::default(),
        }
    }

    pub fn connection_id(&self) -> u64 {
        self.connection_id
    }

    pub fn next_packet_number(&self) -> u64 {
        self.next_packet_number
    }

    pub fn seal(
        &mut self,
        direction: Direction,
        message_type: u8,
        session_id: u64,
        logical_id: u64,
        flags: u32,
        payload: &[u8],
    ) -> Result<Vec<u8>, WireError> {
        let mut bytes = [0_u8; MAX_DATAGRAM_SIZE];
        let length = self.seal_into(
            direction,
            message_type,
            session_id,
            logical_id,
            flags,
            payload,
            &mut bytes,
        )?;
        Ok(bytes[..length].to_vec())
    }

    /// The caller owns the datagram buffer; every attempt still burns a fresh nonce.
    #[allow(clippy::too_many_arguments)]
    pub fn seal_into(
        &mut self,
        direction: Direction,
        message_type: u8,
        session_id: u64,
        logical_id: u64,
        flags: u32,
        payload: &[u8],
        output: &mut [u8],
    ) -> Result<usize, WireError> {
        let packet_number = self.next_packet_number;
        self.next_packet_number = self
            .next_packet_number
            .checked_add(1)
            .ok_or(WireError::PacketNumberExhausted)?;
        let complete_length = RECORD_HEADER_SIZE
            .checked_add(payload.len())
            .and_then(|value| value.checked_add(TAG_SIZE))
            .ok_or(WireError::BadLength(payload.len()))?;
        if complete_length > MAX_DATAGRAM_SIZE || complete_length > output.len() {
            return Err(WireError::BadLength(complete_length));
        }

        let bytes = &mut output[..complete_length];
        write_record_header(
            &mut bytes[..RECORD_HEADER_SIZE],
            RecordHeader {
                direction,
                message_type,
                connection_id: self.connection_id,
                session_id,
                packet_number,
                logical_id,
                flags,
                payload_length: payload.len() as u16,
            },
        );
        bytes[RECORD_HEADER_SIZE..RECORD_HEADER_SIZE + payload.len()].copy_from_slice(payload);
        let (header, encrypted_and_tag) = bytes.split_at_mut(RECORD_HEADER_SIZE);
        let (encrypted, tag_bytes) = encrypted_and_tag.split_at_mut(payload.len());
        let cipher = ChaCha20Poly1305::new(Key::from_slice(self.send_key.as_ref()));
        let nonce = noise_nonce(packet_number);
        let tag = cipher
            .encrypt_in_place_detached(Nonce::from_slice(&nonce), header, encrypted)
            .map_err(|_| WireError::EncryptFailed)?;
        tag_bytes.copy_from_slice(tag.as_slice());
        Ok(complete_length)
    }

    pub fn open(
        &mut self,
        expected_direction: Direction,
        bytes: &[u8],
    ) -> Result<OpenedRecord, WireError> {
        if bytes.len() > MAX_DATAGRAM_SIZE {
            return Err(WireError::BadLength(bytes.len()));
        }
        let mut scratch = [0_u8; MAX_DATAGRAM_SIZE];
        scratch[..bytes.len()].copy_from_slice(bytes);
        let header = self.open_in_place(expected_direction, &mut scratch[..bytes.len()])?;
        Ok(OpenedRecord {
            header,
            payload: scratch
                [RECORD_HEADER_SIZE..RECORD_HEADER_SIZE + header.payload_length as usize]
                .to_vec(),
        })
    }

    pub fn open_in_place(
        &mut self,
        expected_direction: Direction,
        bytes: &mut [u8],
    ) -> Result<RecordHeader, WireError> {
        let header = decode_record_header(bytes, expected_direction)?;
        if header.connection_id != self.connection_id {
            return Err(WireError::WrongConnection);
        }
        if !self.replay.would_accept(header.packet_number) {
            return Err(WireError::Replay);
        }
        let payload_length = header.payload_length as usize;
        let (associated_data, encrypted_and_tag) = bytes.split_at_mut(RECORD_HEADER_SIZE);
        let (payload, tag) = encrypted_and_tag.split_at_mut(payload_length);
        let cipher = ChaCha20Poly1305::new(Key::from_slice(self.receive_key.as_ref()));
        let nonce = noise_nonce(header.packet_number);
        cipher
            .decrypt_in_place_detached(
                Nonce::from_slice(&nonce),
                associated_data,
                payload,
                Tag::from_slice(tag),
            )
            .map_err(|_| WireError::BadTag)?;
        self.replay.commit(header.packet_number);
        Ok(header)
    }
}

fn write_record_header(bytes: &mut [u8], header: RecordHeader) {
    debug_assert_eq!(bytes.len(), RECORD_HEADER_SIZE);
    let complete_length = RECORD_HEADER_SIZE + header.payload_length as usize + TAG_SIZE;
    bytes[..4].copy_from_slice(&header.direction.magic());
    bytes[4] = PROTOCOL_VERSION;
    bytes[5] = header.message_type;
    bytes[6..8].copy_from_slice(&(complete_length as u16).to_le_bytes());
    bytes[8..16].copy_from_slice(&header.connection_id.to_le_bytes());
    bytes[16..24].copy_from_slice(&header.session_id.to_le_bytes());
    bytes[24..32].copy_from_slice(&header.packet_number.to_le_bytes());
    bytes[32..40].copy_from_slice(&header.logical_id.to_le_bytes());
    bytes[40..44].copy_from_slice(&header.flags.to_le_bytes());
    bytes[44..46].copy_from_slice(&header.payload_length.to_le_bytes());
    bytes[46..48].fill(0);
}

pub fn decode_record_header(
    bytes: &[u8],
    expected_direction: Direction,
) -> Result<RecordHeader, WireError> {
    if bytes.len() < RECORD_HEADER_SIZE + TAG_SIZE || bytes.len() > MAX_DATAGRAM_SIZE {
        return Err(WireError::BadLength(bytes.len()));
    }
    if bytes[..4] != expected_direction.magic() {
        return Err(WireError::BadMagic);
    }
    if bytes[4] != PROTOCOL_VERSION {
        return Err(WireError::BadVersion(bytes[4]));
    }
    let payload_length = read_u16(bytes, 44);
    let expected_length = RECORD_HEADER_SIZE + payload_length as usize + TAG_SIZE;
    let declared_length = read_u16(bytes, 6) as usize;
    if expected_length != bytes.len() || declared_length != bytes.len() {
        return Err(WireError::BadLength(declared_length));
    }
    if read_u16(bytes, 46) != 0 {
        return Err(WireError::ReservedBits);
    }
    Ok(RecordHeader {
        direction: expected_direction,
        message_type: bytes[5],
        connection_id: read_u64(bytes, 8),
        session_id: read_u64(bytes, 16),
        packet_number: read_u64(bytes, 24),
        logical_id: read_u64(bytes, 32),
        flags: read_u32(bytes, 40),
        payload_length,
    })
}

#[derive(Clone)]
pub struct ReplayWindow {
    highest: Option<u64>,
    words: [u64; REPLAY_WINDOW_BITS / 64],
}

impl ReplayWindow {
    pub fn would_accept(&self, packet_number: u64) -> bool {
        let Some(highest) = self.highest else {
            return true;
        };
        if packet_number > highest {
            return true;
        }
        let distance = highest - packet_number;
        if distance >= REPLAY_WINDOW_BITS as u64 {
            return false;
        }
        self.words[distance as usize / 64] & (1_u64 << (distance % 64)) == 0
    }

    pub fn commit(&mut self, packet_number: u64) {
        match self.highest {
            None => {
                self.highest = Some(packet_number);
                self.words[0] = 1;
            }
            Some(highest) if packet_number > highest => {
                let shift = packet_number - highest;
                self.shift(shift as usize);
                self.highest = Some(packet_number);
                self.words[0] |= 1;
            }
            Some(highest) => {
                let distance = (highest - packet_number) as usize;
                if distance < REPLAY_WINDOW_BITS {
                    self.words[distance / 64] |= 1_u64 << (distance % 64);
                }
            }
        }
    }

    fn shift(&mut self, bits: usize) {
        if bits >= REPLAY_WINDOW_BITS {
            self.words.fill(0);
            return;
        }
        let word_shift = bits / 64;
        let bit_shift = bits % 64;
        for destination in (0..self.words.len()).rev() {
            let mut value = 0;
            if destination >= word_shift {
                value = self.words[destination - word_shift] << bit_shift;
                if bit_shift != 0 && destination > word_shift {
                    value |= self.words[destination - word_shift - 1] >> (64 - bit_shift);
                }
            }
            self.words[destination] = value;
        }
    }
}

impl Default for ReplayWindow {
    fn default() -> Self {
        Self {
            highest: None,
            words: [0; REPLAY_WINDOW_BITS / 64],
        }
    }
}

pub fn decode_touch_record(record: &OpenedRecord) -> Result<TouchFrame, WireError> {
    decode_touch_payload(&record.header, &record.payload)
}

pub fn decode_touch_payload(
    header: &RecordHeader,
    payload: &[u8],
) -> Result<TouchFrame, WireError> {
    if header.direction != Direction::PhoneToHost
        || header.message_type != PHONE_TOUCH
        || header.session_id == 0
        || header.flags != 0
        || payload.len() < TOUCH_PAYLOAD_HEADER_SIZE
    {
        return Err(WireError::BadMessage);
    }
    let action = payload[40];
    if !matches!(
        action,
        ACTION_HEARTBEAT | ACTION_DOWN | ACTION_MOVE | ACTION_UP | ACTION_CANCEL
    ) {
        return Err(WireError::BadAction(action));
    }
    let contact_count = payload[42] as usize;
    if contact_count > MAX_CONTACTS
        || payload.len() != TOUCH_PAYLOAD_HEADER_SIZE + contact_count * CONTACT_SIZE
    {
        return Err(WireError::BadContactCount(contact_count));
    }
    let flags = payload[43];
    if flags & !VALID_FRAME_FLAGS != 0 {
        return Err(WireError::ReservedBits);
    }
    let mut seen = [false; 256];
    let mut contacts = Contacts::default();
    for index in 0..contact_count {
        let offset = TOUCH_PAYLOAD_HEADER_SIZE + index * CONTACT_SIZE;
        let pointer_id = payload[offset];
        if seen[pointer_id as usize] {
            return Err(WireError::DuplicatePointer(pointer_id));
        }
        seen[pointer_id as usize] = true;
        let contact_flags = payload[offset + 1];
        if contact_flags & !VALID_CONTACT_FLAGS != 0 {
            return Err(WireError::ReservedBits);
        }
        contacts.push(Contact {
            pointer_id,
            flags: contact_flags,
            x: read_i16(payload, offset + 2) as f32 / 10_000.0,
            y: read_i16(payload, offset + 4) as f32 / 10_000.0,
            pressure: read_u16(payload, offset + 6) as f32 / 65_535.0,
            touch_major: read_u16(payload, offset + 8) as f32 / 65_535.0,
        });
    }
    let frame = TouchFrame {
        session_id: header.session_id,
        sequence: header.logical_id,
        phone_event_nanos: read_u64(payload, 0),
        phone_callback_nanos: read_u64(payload, 8),
        phone_send_nanos: read_u64(payload, 16),
        echo_host_send_nanos: read_u64(payload, 24),
        phone_control_receive_nanos: read_u64(payload, 32),
        action,
        action_pointer_id: payload[41],
        flags,
        contacts,
    };
    validate_session_start(&frame)?;
    Ok(frame)
}

fn validate_session_start(frame: &TouchFrame) -> Result<(), WireError> {
    if frame.sequence == 0 {
        if frame.action != ACTION_CANCEL
            || !frame.session_start()
            || frame.flags != FRAME_FLAG_SESSION_START
            || !frame.contacts.is_empty()
        {
            return Err(WireError::BadSessionStart);
        }
    } else if frame.session_start() {
        return Err(WireError::BadSessionStart);
    }
    Ok(())
}

pub fn encode_control_payload(
    receive_window: u32,
    lane_count: u8,
    host_send_nanos: u64,
) -> [u8; CONTROL_PAYLOAD_SIZE] {
    let mut payload = [0_u8; CONTROL_PAYLOAD_SIZE];
    payload[..4].copy_from_slice(&receive_window.to_le_bytes());
    payload[4] = lane_count;
    payload[8..16].copy_from_slice(&host_send_nanos.to_le_bytes());
    payload
}

pub fn decode_quality_reply(record: &OpenedRecord) -> Result<QualityReply, WireError> {
    if record.header.direction != Direction::PhoneToHost
        || record.header.message_type != PHONE_QUALITY_REPLY
        || record.header.session_id != 0
        || record.header.flags != 0
        || record.payload.len() != 32
    {
        return Err(WireError::BadMessage);
    }
    let repair = record.payload[24];
    let signal_level = record.payload[25] as i8;
    if repair > 1 || (signal_level != -1 && !(0..=4).contains(&signal_level)) {
        return Err(WireError::BadMessage);
    }
    Ok(QualityReply {
        probe_id: record.header.logical_id,
        host_send_nanos: read_u64(&record.payload, 0),
        phone_receive_nanos: read_u64(&record.payload, 8),
        phone_send_nanos: read_u64(&record.payload, 16),
        repair_only: repair == 1,
        signal_level,
        rssi_dbm: read_i16(&record.payload, 26),
        frequency_mhz: read_u32(&record.payload, 28),
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QualityReply {
    pub probe_id: u64,
    pub host_send_nanos: u64,
    pub phone_receive_nanos: u64,
    pub phone_send_nanos: u64,
    pub repair_only: bool,
    pub signal_level: i8,
    pub rssi_dbm: i16,
    pub frequency_mhz: u32,
}

pub fn prologue(transport: TransportKind, exchange_id: [u8; 16]) -> Vec<u8> {
    let mut prologue = Vec::with_capacity(NOISE_PROLOGUE_PREFIX.len() + 17);
    prologue.extend_from_slice(NOISE_PROLOGUE_PREFIX);
    prologue.push(transport as u8);
    prologue.extend_from_slice(&exchange_id);
    prologue
}

pub fn connection_id(handshake_hash: &[u8]) -> u64 {
    let digest = hash_parts(&[CONNECTION_DOMAIN, handshake_hash]);
    u64::from_le_bytes(
        digest[..8]
            .try_into()
            .expect("BLAKE2s digest has eight bytes"),
    )
}

pub fn sas_commit(role: u8, handshake_hash: &[u8], random: &[u8; 32]) -> [u8; 32] {
    hash_parts(&[SAS_COMMIT_DOMAIN, &[role], handshake_hash, random])
}

pub fn sas_digest(
    handshake_hash: &[u8],
    phone_random: &[u8; 32],
    host_random: &[u8; 32],
) -> [u8; 32] {
    hash_parts(&[SAS_DOMAIN, handshake_hash, phone_random, host_random])
}

pub fn sas_pattern(mut digest: [u8; 32]) -> [u8; 8] {
    const SPACE: u64 = 1_679_616; // 6^8
    const LIMIT: u64 = (1_u64 << 32) / SPACE * SPACE;
    loop {
        for word in digest.as_chunks::<4>().0 {
            let value = u32::from_le_bytes(*word) as u64;
            if value < LIMIT {
                let mut value = value % SPACE;
                let mut pattern = [0_u8; 8];
                for digit in pattern.iter_mut().rev() {
                    *digit = (value % 6) as u8 + 1;
                    value /= 6;
                }
                return pattern;
            }
        }
        digest = hash_parts(&[SAS_RETRY_DOMAIN, &digest]);
    }
}

pub fn fill_random(bytes: &mut [u8]) -> Result<(), WireError> {
    getrandom::fill(bytes).map_err(|_| WireError::RandomUnavailable)
}

fn hash_parts(parts: &[&[u8]]) -> [u8; 32] {
    let mut hash = Blake2s256::new();
    for part in parts {
        hash.update(part);
    }
    hash.finalize().into()
}

fn noise_nonce(packet_number: u64) -> [u8; 12] {
    let mut nonce = [0_u8; 12];
    nonce[4..].copy_from_slice(&packet_number.to_le_bytes());
    nonce
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(
        bytes[offset..offset + 2]
            .try_into()
            .expect("validated offset"),
    )
}

fn read_i16(bytes: &[u8], offset: usize) -> i16 {
    i16::from_le_bytes(
        bytes[offset..offset + 2]
            .try_into()
            .expect("validated offset"),
    )
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("validated offset"),
    )
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("validated offset"),
    )
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WireError {
    BadMagic,
    BadVersion(u8),
    BadSuite(u8),
    BadTransport(u8),
    BadLength(usize),
    ReservedBits,
    WrongConnection,
    Replay,
    BadTag,
    EncryptFailed,
    PacketNumberExhausted,
    HandshakeIncomplete,
    BadMessage,
    BadAction(u8),
    BadContactCount(usize),
    DuplicatePointer(u8),
    BadSessionStart,
    RandomUnavailable,
}

impl fmt::Display for WireError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic => formatter.write_str("invalid v5 magic"),
            Self::BadVersion(version) => write!(formatter, "unsupported protocol v{version}"),
            Self::BadSuite(suite) => write!(formatter, "unsupported v5 suite {suite}"),
            Self::BadTransport(transport) => write!(formatter, "invalid transport {transport}"),
            Self::BadLength(length) => write!(formatter, "invalid v5 datagram length {length}"),
            Self::ReservedBits => formatter.write_str("reserved v5 field is nonzero"),
            Self::WrongConnection => formatter.write_str("record belongs to another connection"),
            Self::Replay => formatter.write_str("replayed or expired packet number"),
            Self::BadTag => formatter.write_str("v5 authentication tag failed"),
            Self::EncryptFailed => formatter.write_str("v5 encryption failed"),
            Self::PacketNumberExhausted => formatter.write_str("v5 packet number exhausted"),
            Self::HandshakeIncomplete => formatter.write_str("Noise handshake is incomplete"),
            Self::BadMessage => formatter.write_str("invalid v5 message for this record"),
            Self::BadAction(action) => write!(formatter, "invalid touch action {action}"),
            Self::BadContactCount(count) => write!(formatter, "invalid contact count {count}"),
            Self::DuplicatePointer(pointer) => write!(formatter, "duplicate pointer {pointer}"),
            Self::BadSessionStart => formatter.write_str("invalid v5 session-start CANCEL"),
            Self::RandomUnavailable => formatter.write_str("secure random source unavailable"),
        }
    }
}

impl std::error::Error for WireError {}

impl Drop for RecordCipher {
    fn drop(&mut self) {
        self.next_packet_number.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cipher_pair() -> (RecordCipher, RecordCipher) {
        let first = [0x11; 32];
        let second = [0x22; 32];
        (
            RecordCipher::from_keys(first, second, 0x8877_6655_4433_2211),
            RecordCipher::from_keys(second, first, 0x8877_6655_4433_2211),
        )
    }

    #[test]
    fn pairing_envelope_is_strict() {
        let exchange = [0x41; 16];
        let bytes = encode_pair_envelope(PAIR_PROBE, exchange, 0, TransportKind::Wifi, &[])
            .expect("encode");
        let decoded = decode_pair_envelope(&bytes).expect("decode");
        assert_eq!(decoded.exchange_id, exchange);
        assert_eq!(decoded.transport, TransportKind::Wifi);
        assert!(decoded.payload.is_empty());
        let mut trailing = bytes;
        trailing.push(0);
        assert!(decode_pair_envelope(&trailing).is_err());
    }

    #[test]
    fn record_header_is_authenticated_and_replays_are_rejected() {
        let (mut phone, mut host) = cipher_pair();
        let bytes = phone
            .seal(Direction::PhoneToHost, PHONE_SAS_REVEAL, 0, 7, 0, b"secret")
            .expect("seal");
        assert_eq!(
            host.open(Direction::PhoneToHost, &bytes)
                .expect("open")
                .payload,
            b"secret"
        );
        assert_eq!(
            host.open(Direction::PhoneToHost, &bytes),
            Err(WireError::Replay)
        );

        let mut changed = phone
            .seal(Direction::PhoneToHost, PHONE_SAS_REVEAL, 0, 8, 0, b"secret")
            .expect("seal");
        changed[32] ^= 1;
        assert_eq!(
            host.open(Direction::PhoneToHost, &changed),
            Err(WireError::BadTag)
        );
    }

    #[test]
    fn replay_window_accepts_authenticated_reordering() {
        let (mut phone, mut host) = cipher_pair();
        let zero = phone
            .seal(Direction::PhoneToHost, PHONE_SAS_REVEAL, 0, 0, 0, b"zero")
            .unwrap();
        let one = phone
            .seal(Direction::PhoneToHost, PHONE_SAS_REVEAL, 0, 1, 0, b"one")
            .unwrap();
        assert_eq!(
            host.open(Direction::PhoneToHost, &one).unwrap().payload,
            b"one"
        );
        assert_eq!(
            host.open(Direction::PhoneToHost, &zero).unwrap().payload,
            b"zero"
        );
        assert_eq!(
            host.open(Direction::PhoneToHost, &zero),
            Err(WireError::Replay)
        );
    }

    #[test]
    fn every_copy_burns_a_new_nonce() {
        let (mut phone, _) = cipher_pair();
        let first = phone
            .seal(Direction::PhoneToHost, PHONE_TOUCH, 9, 3, 0, b"same")
            .unwrap();
        let second = phone
            .seal(Direction::PhoneToHost, PHONE_TOUCH, 9, 3, 0, b"same")
            .unwrap();
        assert_eq!(read_u64(&first, 24), 0);
        assert_eq!(read_u64(&second, 24), 1);
        assert_ne!(first, second);
    }

    #[test]
    fn quality_reply_rejects_undefined_status_values() {
        let mut payload = vec![0_u8; 32];
        payload[24] = 2;
        payload[25] = 0xff;
        let mut record = OpenedRecord {
            header: RecordHeader {
                direction: Direction::PhoneToHost,
                message_type: PHONE_QUALITY_REPLY,
                connection_id: 1,
                session_id: 0,
                packet_number: 0,
                logical_id: 7,
                flags: 0,
                payload_length: 32,
            },
            payload,
        };
        assert_eq!(decode_quality_reply(&record), Err(WireError::BadMessage));
        record.payload[24] = 0;
        record.payload[25] = 5;
        assert_eq!(decode_quality_reply(&record), Err(WireError::BadMessage));
        record.payload[25] = 4;
        assert!(decode_quality_reply(&record).is_ok());
    }

    #[test]
    fn sas_mapping_is_stable_and_one_based() {
        let hash = [0x10; 32];
        let phone = [0x20; 32];
        let host = [0x30; 32];
        let digest = sas_digest(&hash, &phone, &host);
        assert_eq!(sas_pattern(digest), [2, 4, 3, 2, 1, 4, 5, 5]);
    }

    #[test]
    fn invalid_tag_does_not_advance_replay_window() {
        let (mut phone, mut host) = cipher_pair();
        let valid = phone
            .seal(Direction::PhoneToHost, PHONE_SAS_REVEAL, 0, 0, 0, b"value")
            .unwrap();
        let mut corrupt = valid.clone();
        *corrupt.last_mut().unwrap() ^= 1;
        assert_eq!(
            host.open(Direction::PhoneToHost, &corrupt),
            Err(WireError::BadTag)
        );
        assert!(host.open(Direction::PhoneToHost, &valid).is_ok());
    }
}
