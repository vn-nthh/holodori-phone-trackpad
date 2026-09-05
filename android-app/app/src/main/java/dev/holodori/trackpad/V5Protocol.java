package dev.holodori.trackpad;

import com.southernstorm.noise.protocol.CipherState;
import com.southernstorm.noise.protocol.CipherStatePair;
import com.southernstorm.noise.protocol.Noise;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.Arrays;

import javax.crypto.BadPaddingException;
import javax.crypto.ShortBufferException;

/** Byte-exact protocol-v5 framing and post-Noise record protection. */
final class V5Protocol {
    static final int VERSION = 5;
    static final int PORT = 42_825;
    static final int MAX_DATAGRAM_SIZE = 1_200;
    static final int PAIR_HEADER_SIZE = 32;
    static final int RECORD_HEADER_SIZE = 48;
    static final int TAG_SIZE = 16;
    static final int CONTROL_PAYLOAD_SIZE = 16;
    static final int TOUCH_PAYLOAD_HEADER_SIZE = 44;
    static final int CONTACT_SIZE = 10;

    static final int PAIR_PROBE = 1;
    static final int PAIR_OFFER = 2;
    static final int PAIR_CONTINUE = 3;
    static final int PAIR_ABORT = 4;
    static final int IK_MESSAGE_1 = 5;
    static final int IK_CONTINUE = 6;

    static final int PHONE_TOUCH = 1;
    static final int PHONE_QUALITY_REPLY = 2;
    static final int PHONE_SAS_COMMITMENT = 3;
    static final int PHONE_SAS_REVEAL = 4;
    static final int PHONE_PAIR_CONFIRM = 5;
    static final int PHONE_AUTH_ABORT = 6;
    static final int PHONE_PING = 7;

    static final int HOST_HELLO = 1;
    static final int HOST_ACK = 2;
    static final int HOST_QUALITY_PROBE = 3;
    static final int HOST_SAS_COMMITMENT = 4;
    static final int HOST_SAS_REVEAL = 5;
    static final int HOST_PAIR_COMPLETE = 6;
    static final int HOST_AUTH_ABORT = 7;
    static final int HOST_PONG = 8;

    static final int QUALITY_REPAIR_ONLY = 0x01;
    static final long NO_ACK = -1L;

    private static final byte[] PAIR_MAGIC = {'H', 'P', 'P', '5'};
    private static final byte[] PHONE_MAGIC = {'H', 'P', 'T', '5'};
    private static final byte[] HOST_MAGIC = {'H', 'P', 'A', '5'};
    private static final byte[] PROLOGUE_PREFIX =
            "holodori-phone-trackpad-v5\0".getBytes(StandardCharsets.US_ASCII);
    private static final byte[] CONNECTION_DOMAIN =
            "holodori-v5-connection".getBytes(StandardCharsets.US_ASCII);
    private static final byte[] SAS_COMMIT_DOMAIN =
            "holodori-v5-sas-commit".getBytes(StandardCharsets.US_ASCII);
    private static final byte[] SAS_DOMAIN =
            "holodori-v5-sas".getBytes(StandardCharsets.US_ASCII);
    private static final byte[] SAS_RETRY_DOMAIN =
            "holodori-v5-sas-retry".getBytes(StandardCharsets.US_ASCII);

    enum TransportKind {
        USB(1),
        WIFI(2);

        final int wireValue;

        TransportKind(int wireValue) {
            this.wireValue = wireValue;
        }

        static TransportKind fromWire(int value) throws ProtocolException {
            if (value == USB.wireValue) return USB;
            if (value == WIFI.wireValue) return WIFI;
            throw new ProtocolException("invalid transport");
        }
    }

    private V5Protocol() {
    }

    static byte[] encodePairEnvelope(
            int kind,
            byte[] exchangeId,
            long step,
            TransportKind transport,
            byte[] payload
    ) throws ProtocolException {
        if (exchangeId == null || exchangeId.length != 16 || payload == null) {
            throw new ProtocolException("invalid pairing envelope input");
        }
        int length = PAIR_HEADER_SIZE + payload.length;
        if (length > MAX_DATAGRAM_SIZE || payload.length > 0xFFFF) {
            throw new ProtocolException("pairing envelope too large");
        }
        ByteBuffer packet = littleEndian(length);
        packet.put(PAIR_MAGIC);
        packet.put((byte) VERSION);
        packet.put((byte) kind);
        packet.putShort((short) length);
        packet.put(exchangeId);
        packet.putInt((int) step);
        packet.putShort((short) payload.length);
        packet.put((byte) transport.wireValue);
        packet.put((byte) 1);
        packet.put(payload);
        return packet.array();
    }

    static PairEnvelope decodePairEnvelope(byte[] bytes, int length)
            throws ProtocolException {
        if (bytes == null || length < PAIR_HEADER_SIZE
                || length > MAX_DATAGRAM_SIZE || bytes.length < length) {
            throw new ProtocolException("invalid pairing envelope length");
        }
        ByteBuffer packet = littleEndian(bytes, length);
        requireMagic(packet, PAIR_MAGIC);
        if (Byte.toUnsignedInt(packet.get()) != VERSION) {
            throw new ProtocolException("invalid protocol version");
        }
        int kind = Byte.toUnsignedInt(packet.get());
        int declaredLength = Short.toUnsignedInt(packet.getShort());
        byte[] exchangeId = new byte[16];
        packet.get(exchangeId);
        long step = Integer.toUnsignedLong(packet.getInt());
        int payloadLength = Short.toUnsignedInt(packet.getShort());
        TransportKind transport = TransportKind.fromWire(Byte.toUnsignedInt(packet.get()));
        if (Byte.toUnsignedInt(packet.get()) != 1
                || declaredLength != length
                || PAIR_HEADER_SIZE + payloadLength != length) {
            throw new ProtocolException("malformed pairing envelope");
        }
        byte[] payload = new byte[payloadLength];
        packet.get(payload);
        return new PairEnvelope(kind, exchangeId, step, transport, payload);
    }

    static byte[] prologue(TransportKind transport, byte[] exchangeId)
            throws ProtocolException {
        if (exchangeId == null || exchangeId.length != 16) {
            throw new ProtocolException("invalid exchange id");
        }
        ByteBuffer bytes = ByteBuffer.allocate(PROLOGUE_PREFIX.length + 17);
        bytes.put(PROLOGUE_PREFIX);
        bytes.put((byte) transport.wireValue);
        bytes.put(exchangeId);
        return bytes.array();
    }

    static long connectionId(byte[] handshakeHash) throws ProtocolException {
        byte[] digest = blake2s(CONNECTION_DOMAIN, handshakeHash);
        return littleEndian(digest, digest.length).getLong();
    }

    static byte[] sasCommit(int role, byte[] handshakeHash, byte[] random)
            throws ProtocolException {
        if ((role != 1 && role != 2) || random == null || random.length != 32) {
            throw new ProtocolException("invalid SAS commitment input");
        }
        return blake2s(SAS_COMMIT_DOMAIN, new byte[]{(byte) role}, handshakeHash, random);
    }

    static byte[] sasDigest(byte[] handshakeHash, byte[] phoneRandom, byte[] hostRandom)
            throws ProtocolException {
        if (phoneRandom == null || phoneRandom.length != 32
                || hostRandom == null || hostRandom.length != 32) {
            throw new ProtocolException("invalid SAS reveal");
        }
        return blake2s(SAS_DOMAIN, handshakeHash, phoneRandom, hostRandom);
    }

    static int[] sasPattern(byte[] initialDigest) throws ProtocolException {
        if (initialDigest == null || initialDigest.length != 32) {
            throw new ProtocolException("invalid SAS digest");
        }
        byte[] digest = initialDigest.clone();
        final long space = 1_679_616L;
        final long limit = (1L << 32) / space * space;
        while (true) {
            ByteBuffer words = littleEndian(digest, digest.length);
            while (words.remaining() >= 4) {
                long value = Integer.toUnsignedLong(words.getInt());
                if (value < limit) {
                    value %= space;
                    int[] pattern = new int[8];
                    for (int index = pattern.length - 1; index >= 0; index--) {
                        pattern[index] = (int) (value % 6) + 1;
                        value /= 6;
                    }
                    Arrays.fill(digest, (byte) 0);
                    return pattern;
                }
            }
            byte[] next = blake2s(SAS_RETRY_DOMAIN, digest);
            Arrays.fill(digest, (byte) 0);
            digest = next;
        }
    }

    static final class PairEnvelope {
        final int kind;
        final byte[] exchangeId;
        final long step;
        final TransportKind transport;
        final byte[] payload;

        PairEnvelope(
                int kind,
                byte[] exchangeId,
                long step,
                TransportKind transport,
                byte[] payload
        ) {
            this.kind = kind;
            this.exchangeId = exchangeId;
            this.step = step;
            this.transport = transport;
            this.payload = payload;
        }
    }

    static final class RecordHeader {
        long receivedNanos;
        int messageType;
        long connectionId;
        long sessionId;
        long packetNumber;
        long logicalId;
        long flags;
        int payloadLength;

        RecordHeader() {
        }

        RecordHeader(
                int messageType,
                long connectionId,
                long sessionId,
                long packetNumber,
                long logicalId,
                long flags,
                int payloadLength
        ) {
            this.messageType = messageType;
            this.connectionId = connectionId;
            this.sessionId = sessionId;
            this.packetNumber = packetNumber;
            this.logicalId = logicalId;
            this.flags = flags;
            this.payloadLength = payloadLength;
        }
    }

    static final class OpenedRecord {
        final RecordHeader header;
        final byte[] payload;

        OpenedRecord(RecordHeader header, byte[] payload) {
            this.header = header;
            this.payload = payload;
        }
    }

    /** Phone-oriented split ciphers: sender is HPT5 and receiver is HPA5. */
    static final class Channel implements AutoCloseable {
        private final CipherState sender;
        private final CipherState receiver;
        private final Object sendLock = new Object();
        private final Object receiveLock = new Object();
        private boolean closed;
        private final long connectionId;
        private final ReplayWindow replay = new ReplayWindow();
        private final byte[] sendAssociatedData = new byte[RECORD_HEADER_SIZE];
        private final byte[] receiveAssociatedData = new byte[RECORD_HEADER_SIZE];
        private final ByteBuffer sendHeader = ByteBuffer.wrap(sendAssociatedData)
                .order(ByteOrder.LITTLE_ENDIAN);
        private long nextPacketNumber;

        Channel(CipherStatePair pair, byte[] handshakeHash) throws ProtocolException {
            if (pair == null) throw new ProtocolException("missing Noise split ciphers");
            sender = pair.getSender();
            receiver = pair.getReceiver();
            connectionId = V5Protocol.connectionId(handshakeHash);
        }

        Channel(CipherState sender, CipherState receiver, long connectionId) {
            this.sender = sender;
            this.receiver = receiver;
            this.connectionId = connectionId;
        }

        long connectionId() {
            return connectionId;
        }

        byte[] seal(
                int messageType,
                long sessionId,
                long logicalId,
                long flags,
                byte[] payload
        ) throws ProtocolException {
            if (payload == null) throw new ProtocolException("missing record payload");
            byte[] bytes = new byte[RECORD_HEADER_SIZE + payload.length + TAG_SIZE];
            int length = sealInto(
                    messageType,
                    sessionId,
                    logicalId,
                    flags,
                    payload,
                    payload.length,
                    bytes
            );
            if (length != bytes.length) {
                throw new ProtocolException("unexpected encrypted record length");
            }
            return bytes;
        }

        int sealInto(
                int messageType,
                long sessionId,
                long logicalId,
                long flags,
                byte[] payload,
                int payloadLength,
                byte[] output
        ) throws ProtocolException {
            synchronized (sendLock) {
                if (closed) throw new ProtocolException("channel is closed");
                if (payload == null || payloadLength < 0 || payloadLength > payload.length
                        || output == null) {
                    throw new ProtocolException("invalid record buffer");
                }
                if (nextPacketNumber == -1L) {
                    throw new ProtocolException("packet number exhausted");
                }
                long packetNumber = nextPacketNumber++;
                int completeLength = RECORD_HEADER_SIZE + payloadLength + TAG_SIZE;
                if (completeLength > MAX_DATAGRAM_SIZE || payloadLength > 0xFFFF
                        || output.length < completeLength) {
                    throw new ProtocolException("record too large");
                }
                ByteBuffer header = sendHeader;
                header.clear();
                header.put(PHONE_MAGIC);
                header.put((byte) VERSION);
                header.put((byte) messageType);
                header.putShort((short) completeLength);
                header.putLong(connectionId);
                header.putLong(sessionId);
                header.putLong(packetNumber);
                header.putLong(logicalId);
                header.putInt((int) flags);
                header.putShort((short) payloadLength);
                header.putShort((short) 0);
                System.arraycopy(sendAssociatedData, 0, output, 0, RECORD_HEADER_SIZE);
                try {
                    sender.setNonce(packetNumber);
                    int encrypted = sender.encryptWithAd(
                            sendAssociatedData,
                            payload,
                            0,
                            output,
                            RECORD_HEADER_SIZE,
                            payloadLength
                    );
                    if (encrypted != payloadLength + TAG_SIZE) {
                        throw new ProtocolException("unexpected encrypted record length");
                    }
                } catch (ShortBufferException error) {
                    throw new ProtocolException("record encryption failed", error);
                }
                return completeLength;
            }
        }

        OpenedRecord open(byte[] bytes, int length)
                throws ProtocolException {
            synchronized (receiveLock) {
                RecordHeader header = decodeRecordHeader(bytes, length, HOST_MAGIC);
                byte[] plaintext = new byte[header.payloadLength];
                openLocked(bytes, header, plaintext);
                return new OpenedRecord(header, plaintext);
            }
        }

        void openInto(byte[] bytes, int length, RecordHeader header, byte[] plaintext)
                throws ProtocolException {
            synchronized (receiveLock) {
                decodeRecordHeaderInto(bytes, length, HOST_MAGIC, header);
                openLocked(bytes, header, plaintext);
            }
        }

        private void openLocked(byte[] bytes, RecordHeader header, byte[] plaintext)
                throws ProtocolException {
            if (closed) throw new ProtocolException("channel is closed");
            if (plaintext.length < header.payloadLength) {
                throw new ProtocolException("plaintext buffer is too small");
            }
            if (header.connectionId != connectionId) {
                throw new ProtocolException("wrong connection");
            }
            if (!replay.wouldAccept(header.packetNumber)) {
                throw new ReplayException();
            }
            System.arraycopy(bytes, 0, receiveAssociatedData, 0, RECORD_HEADER_SIZE);
            try {
                receiver.setNonce(header.packetNumber);
                int opened = receiver.decryptWithAd(
                        receiveAssociatedData,
                        bytes,
                        RECORD_HEADER_SIZE,
                        plaintext,
                        0,
                        header.payloadLength + TAG_SIZE
                );
                if (opened != header.payloadLength) {
                    throw new ProtocolException("unexpected plaintext length");
                }
            } catch (BadPaddingException error) {
                Arrays.fill(plaintext, (byte) 0);
                throw new AuthenticationException();
            } catch (ShortBufferException error) {
                Arrays.fill(plaintext, (byte) 0);
                throw new ProtocolException("record decryption failed", error);
            }
            replay.commit(header.packetNumber);
        }

        @Override
        public void close() {
            synchronized (sendLock) {
                synchronized (receiveLock) {
                    if (closed) return;
                    closed = true;
                    sender.destroy();
                    receiver.destroy();
                }
            }
        }
    }

    static RecordHeader decodeRecordHeader(byte[] bytes, int length, byte[] magic)
            throws ProtocolException {
        RecordHeader header = new RecordHeader();
        decodeRecordHeaderInto(bytes, length, magic, header);
        return header;
    }

    private static void decodeRecordHeaderInto(
            byte[] bytes, int length, byte[] magic, RecordHeader header
    ) throws ProtocolException {
        if (bytes == null || magic == null || magic.length != 4
                || length < RECORD_HEADER_SIZE + TAG_SIZE
                || length > MAX_DATAGRAM_SIZE || bytes.length < length) {
            throw new ProtocolException("invalid record length");
        }
        for (int index = 0; index < magic.length; index++) {
            if (bytes[index] != magic[index]) throw new ProtocolException("invalid record magic");
        }
        if (Byte.toUnsignedInt(bytes[4]) != VERSION) {
            throw new ProtocolException("invalid record version");
        }
        int declaredLength = readUnsignedShort(bytes, 6);
        int payloadLength = readUnsignedShort(bytes, 44);
        int reserved = readUnsignedShort(bytes, 46);
        if (reserved != 0 || declaredLength != length
                || RECORD_HEADER_SIZE + payloadLength + TAG_SIZE != length) {
            throw new ProtocolException("malformed record header");
        }
        header.messageType = Byte.toUnsignedInt(bytes[5]);
        header.connectionId = readLong(bytes, 8);
        header.sessionId = readLong(bytes, 16);
        header.packetNumber = readLong(bytes, 24);
        header.logicalId = readLong(bytes, 32);
        header.flags = readUnsignedShort(bytes, 40) | ((long) readUnsignedShort(bytes, 42) << 16);
        header.payloadLength = payloadLength;
    }

    private static int readUnsignedShort(byte[] bytes, int offset) {
        return (bytes[offset] & 0xFF) | ((bytes[offset + 1] & 0xFF) << 8);
    }

    private static long readLong(byte[] bytes, int offset) {
        long value = 0;
        for (int index = 0; index < Long.BYTES; index++) {
            value |= (long) (bytes[offset + index] & 0xFF) << (index * 8);
        }
        return value;
    }

    private static byte[] blake2s(byte[]... parts) throws ProtocolException {
        try {
            MessageDigest digest = Noise.createHash("BLAKE2s");
            for (byte[] part : parts) {
                if (part == null) throw new ProtocolException("missing hash input");
                digest.update(part);
            }
            return digest.digest();
        } catch (NoSuchAlgorithmException error) {
            throw new ProtocolException("BLAKE2s unavailable", error);
        }
    }

    private static void requireMagic(ByteBuffer packet, byte[] expected)
            throws ProtocolException {
        for (byte value : expected) {
            if (packet.get() != value) throw new ProtocolException("invalid record magic");
        }
    }

    private static ByteBuffer littleEndian(int size) {
        return ByteBuffer.allocate(size).order(ByteOrder.LITTLE_ENDIAN);
    }

    private static ByteBuffer littleEndian(byte[] bytes, int length) {
        return ByteBuffer.wrap(bytes, 0, length).order(ByteOrder.LITTLE_ENDIAN);
    }

    static final class ReplayWindow {
        private static final int BITS = 1_024;
        private final long[] words = new long[BITS / Long.SIZE];
        private boolean initialized;
        private long highest;

        boolean wouldAccept(long packetNumber) {
            if (!initialized || Long.compareUnsigned(packetNumber, highest) > 0) return true;
            long distance = highest - packetNumber;
            if (Long.compareUnsigned(distance, BITS) >= 0) return false;
            int offset = (int) distance;
            return (words[offset / Long.SIZE] & (1L << (offset % Long.SIZE))) == 0;
        }

        void commit(long packetNumber) {
            if (!initialized) {
                initialized = true;
                highest = packetNumber;
                words[0] = 1L;
                return;
            }
            if (Long.compareUnsigned(packetNumber, highest) > 0) {
                long unsignedDistance = packetNumber - highest;
                if (Long.compareUnsigned(unsignedDistance, BITS) >= 0) {
                    Arrays.fill(words, 0L);
                } else {
                    shiftOlder((int) unsignedDistance);
                }
                highest = packetNumber;
                words[0] |= 1L;
                return;
            }
            int distance = (int) (highest - packetNumber);
            words[distance / Long.SIZE] |= 1L << (distance % Long.SIZE);
        }

        private void shiftOlder(int distance) {
            int wordShift = distance / Long.SIZE;
            int bitShift = distance % Long.SIZE;
            for (int destination = words.length - 1; destination >= 0; destination--) {
                long value = 0;
                if (destination >= wordShift) {
                    value = words[destination - wordShift] << bitShift;
                    if (bitShift != 0 && destination > wordShift) {
                        value |= words[destination - wordShift - 1]
                                >>> (Long.SIZE - bitShift);
                    }
                }
                words[destination] = value;
            }
        }
    }

    static class ProtocolException extends Exception {
        private static final long serialVersionUID = 1L;

        ProtocolException(String message) {
            super(message);
        }

        ProtocolException(String message, Throwable cause) {
            super(message, cause);
        }
    }

    static final class AuthenticationException extends ProtocolException {
        private static final long serialVersionUID = 1L;

        AuthenticationException() {
            super("authentication failed");
        }
    }

    static final class ReplayException extends ProtocolException {
        private static final long serialVersionUID = 1L;

        ReplayException() {
            super("replayed packet");
        }
    }
}
