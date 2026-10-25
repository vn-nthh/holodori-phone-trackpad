package dev.holodori.trackpad;

import static org.junit.Assert.assertArrayEquals;
import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertFalse;
import static org.junit.Assert.assertNotEquals;
import static org.junit.Assert.assertThrows;
import static org.junit.Assert.assertTrue;

import com.southernstorm.noise.protocol.CipherState;
import com.southernstorm.noise.protocol.HandshakeState;
import com.southernstorm.noise.protocol.Noise;

import org.junit.Test;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Proxy;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;

public final class SecureProtocolTest {
    private static final String XX_NAME = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
    private static final String IK_NAME = "Noise_IK_25519_ChaChaPoly_BLAKE2s";

    @Test
    public void reusableControlBuffersAuthenticateBeforeReplayCommit() throws Exception {
        try (SecureProtocol.Channel channel = new SecureProtocol.Channel(cipher(), cipher(), 9)) {
            byte[] record = hostControl(0);
            byte[] plaintext = new byte[SecureProtocol.CONTROL_PAYLOAD_SIZE];
            SecureProtocol.RecordHeader header = new SecureProtocol.RecordHeader();
            record[record.length - 1] ^= 1;
            assertThrows(SecureProtocol.AuthenticationException.class,
                    () -> channel.openInto(record, record.length, header, plaintext));
            assertArrayEquals(new byte[plaintext.length], plaintext);
            record[record.length - 1] ^= 1;
            channel.openInto(record, record.length, header, plaintext);
            assertEquals(0, header.packetNumber);
            SecureProtocol.HostControl control = new SecureProtocol.HostControl();
            SecureProtocol.decodeHostControl(plaintext, header.payloadLength, 256, control);
            assertEquals(128, control.receiveWindow);
            assertEquals(42, control.hostSendNanos);
            assertEquals(0b101, control.selectiveAck);
            assertThrows(SecureProtocol.ReplayException.class,
                    () -> channel.openInto(record, record.length, header, plaintext));
            byte[] next = hostControl(1);
            channel.openInto(next, next.length, header, plaintext);
            assertEquals(1, header.packetNumber);
        }
    }

    @Test
    public void receivingAnAckDoesNotLockTheSender() throws Exception {
        CountDownLatch decryptStarted = new CountDownLatch(1);
        CountDownLatch finishDecrypt = new CountDownLatch(1);
        CipherState delegate = cipher();
        CipherState receiver = (CipherState) Proxy.newProxyInstance(
                CipherState.class.getClassLoader(), new Class<?>[]{CipherState.class},
                (proxy, method, arguments) -> {
                    if (method.getName().equals("decryptWithAd")) {
                        decryptStarted.countDown();
                        if (!finishDecrypt.await(5, TimeUnit.SECONDS)) throw new AssertionError("receive blocked");
                    }
                    try {
                        return method.invoke(delegate, arguments);
                    } catch (InvocationTargetException error) {
                        throw error.getCause();
                    }
                });
        ExecutorService workers = Executors.newFixedThreadPool(2);
        SecureProtocol.Channel channel = new SecureProtocol.Channel(cipher(), receiver, 9);
        try {
            byte[] ack = hostControl(0);
            Future<?> receive = workers.submit(() -> {
                channel.openInto(ack, ack.length, new SecureProtocol.RecordHeader(),
                        new byte[SecureProtocol.CONTROL_PAYLOAD_SIZE]);
                return null;
            });
            assertTrue(decryptStarted.await(2, TimeUnit.SECONDS));
            Future<byte[]> send = workers.submit(() -> channel.seal(SecureProtocol.PHONE_PING, 7, 0, 0, new byte[0]));
            assertEquals(64, send.get(1, TimeUnit.SECONDS).length);
            finishDecrypt.countDown();
            receive.get(2, TimeUnit.SECONDS);
        } finally {
            finishDecrypt.countDown();
            workers.shutdownNow();
            channel.close();
        }
    }

    private static CipherState cipher() throws Exception {
        CipherState cipher = Noise.createCipher("ChaChaPoly");
        cipher.initializeKey(sequence(0x20), 0);
        return cipher;
    }

    private static byte[] hostControl(long packetNumber) throws Exception {
        int payloadSize = SecureProtocol.CONTROL_PAYLOAD_SIZE;
        int length = SecureProtocol.RECORD_HEADER_SIZE + payloadSize + SecureProtocol.TAG_SIZE;
        byte[] record = new byte[length];
        ByteBuffer header = ByteBuffer.wrap(record).order(ByteOrder.LITTLE_ENDIAN);
        header.put(new byte[]{'H', 'P', 'A', '6'}).put((byte) SecureProtocol.VERSION)
                .put((byte) SecureProtocol.HOST_ACK);
        header.putShort((short) length).putLong(9).putLong(7).putLong(packetNumber).putLong(0);
        header.putInt(0).putShort((short) payloadSize).putShort((short) 0);
        byte[] payload = ByteBuffer.allocate(payloadSize).order(ByteOrder.LITTLE_ENDIAN)
                .putInt(128).put((byte) 6).put((byte) 0).putShort((short) 0).putLong(42)
                .putLong(0b101).array();
        CipherState cipher = cipher();
        try {
            cipher.setNonce(packetNumber);
            cipher.encryptWithAd(Arrays.copyOf(record, 48), payload, 0, record, 48, payload.length);
        } finally {
            cipher.destroy();
        }
        return record;
    }

    @Test
    public void pairingEnvelopeIsStrictAndRoundTrips() throws Exception {
        byte[] exchange = new byte[16];
        for (int index = 0; index < exchange.length; index++) exchange[index] = (byte) index;
        byte[] encoded = SecureProtocol.encodePairEnvelope(
                SecureProtocol.PAIR_CONTINUE,
                exchange,
                2,
                SecureProtocol.TransportKind.WIFI,
                new byte[]{7, 8, 9}
        );
        SecureProtocol.PairEnvelope decoded = SecureProtocol.decodePairEnvelope(encoded, encoded.length);
        assertEquals(SecureProtocol.PAIR_CONTINUE, decoded.kind);
        assertArrayEquals(exchange, decoded.exchangeId);
        assertEquals(2, decoded.step);
        assertEquals(SecureProtocol.TransportKind.WIFI, decoded.transport);
        assertArrayEquals(new byte[]{7, 8, 9}, decoded.payload);

        encoded[6]++;
        assertThrows(
                SecureProtocol.ProtocolException.class,
                () -> SecureProtocol.decodePairEnvelope(encoded, encoded.length)
        );
    }

    @Test
    public void sasMappingMatchesPublishedRustVector() throws Exception {
        byte[] handshakeHash = new byte[32];
        byte[] phoneRandom = new byte[32];
        byte[] hostRandom = new byte[32];
        Arrays.fill(handshakeHash, (byte) 0x10);
        Arrays.fill(phoneRandom, (byte) 0x20);
        Arrays.fill(hostRandom, (byte) 0x30);
        assertArrayEquals(
                new int[]{2, 4, 3, 2, 1, 4, 5, 5},
                SecureProtocol.sasPattern(SecureProtocol.sasDigest(
                        handshakeHash,
                        phoneRandom,
                        hostRandom
                ))
        );
    }

    @Test
    public void replayWindowAcceptsReorderingOnlyOnce() {
        SecureProtocol.ReplayWindow replay = new SecureProtocol.ReplayWindow();
        assertTrue(replay.wouldAccept(0));
        replay.commit(0);
        assertTrue(replay.wouldAccept(2));
        replay.commit(2);
        assertTrue(replay.wouldAccept(1));
        replay.commit(1);
        assertFalse(replay.wouldAccept(1));
        assertFalse(replay.wouldAccept(0));
        assertTrue(replay.wouldAccept(1_025));
        replay.commit(1_025);
        assertFalse(replay.wouldAccept(0));
    }

    @Test
    public void everyLogicalCopyUsesANewPacketNonce() throws Exception {
        byte[] sendKey = new byte[32];
        byte[] receiveKey = new byte[32];
        Arrays.fill(sendKey, (byte) 0x11);
        Arrays.fill(receiveKey, (byte) 0x22);
        CipherState sender = Noise.createCipher("ChaChaPoly");
        CipherState receiver = Noise.createCipher("ChaChaPoly");
        sender.initializeKey(sendKey, 0);
        receiver.initializeKey(receiveKey, 0);
        SecureProtocol.Channel channel = new SecureProtocol.Channel(
                sender,
                receiver,
                0x0102_0304_0506_0708L
        );
        byte[] first = channel.seal(
                SecureProtocol.PHONE_TOUCH,
                9,
                4,
                0,
                "same".getBytes(StandardCharsets.US_ASCII)
        );
        byte[] second = channel.seal(
                SecureProtocol.PHONE_TOUCH,
                9,
                4,
                0,
                "same".getBytes(StandardCharsets.US_ASCII)
        );
        assertEquals(0, readLong(first, 24));
        assertEquals(1, readLong(second, 24));
        assertNotEquals(toHex(first), toHex(second));
        channel.close();
    }

    @Test
    public void noiseAndRecordVectorsMatchPublishedRustBytes() throws Exception {
        byte[] hostStatic = sequence(0x10);
        byte[] phoneStatic = sequence(0x30);
        byte[] exchange = Arrays.copyOf(sequence(0), 16);
        byte[] prologue = SecureProtocol.prologue(SecureProtocol.TransportKind.WIFI, exchange);
        HandshakeState host = handshake(
                XX_NAME,
                HandshakeState.INITIATOR,
                hostStatic,
                sequence(0x50),
                null,
                prologue
        );
        HandshakeState phone = handshake(
                XX_NAME,
                HandshakeState.RESPONDER,
                phoneStatic,
                sequence(0x70),
                null,
                prologue
        );
        byte[] buffer = new byte[SecureProtocol.MAX_DATAGRAM_SIZE];
        byte[] payload = new byte[SecureProtocol.MAX_DATAGRAM_SIZE];
        int length = host.writeMessage(buffer, 0, null, 0, 0);
        assertEquals(
                "392d174a38b3b1beafaf1fe824870841c5fa531bc6eafdb6402c124664488c1c",
                toHex(buffer, length)
        );
        assertEquals(0, phone.readMessage(buffer, 0, length, payload, 0));
        length = phone.writeMessage(buffer, 0, null, 0, 0);
        assertEquals(
                "23b7bb8c91ae008711fb12846780bcdf1e065f821bdfec49f57e7c7dcd4c4823"
                        + "f56b5ed019d6b4f7d390bd2416f19670654ee0fdcfd6a275323659d4bc92bd3b"
                        + "eac7cf12d97459e8bce0f17b8686e1e9c32d8910b963c3f7ab6b51b3fdda186a",
                toHex(buffer, length)
        );
        assertEquals(0, host.readMessage(buffer, 0, length, payload, 0));
        length = host.writeMessage(buffer, 0, null, 0, 0);
        assertEquals(
                "f531830cca96c417accf9c7fbb8b15f7eb91cc4ec6e41d779f704ed44dc67f66"
                        + "3bdd1c6d809eb4c5cea8fcc8b37a03f7a58efc53a85d46c286b57342424311c1",
                toHex(buffer, length)
        );
        assertEquals(0, phone.readMessage(buffer, 0, length, payload, 0));
        assertEquals(
                "6977c5b56b759d9a520bd729380074a5613eae3bf5fe5642158d81e2b9f1b50a",
                toHex(host.getHandshakeHash())
        );
        assertArrayEquals(host.getHandshakeHash(), phone.getHandshakeHash());

        byte[] xxHash = phone.getHandshakeHash().clone();
        SecureProtocol.Channel channel = new SecureProtocol.Channel(phone.split(), xxHash);
        byte[] record = channel.seal(
                SecureProtocol.PHONE_SAS_REVEAL,
                0,
                9,
                0,
                "vector".getBytes(StandardCharsets.US_ASCII)
        );
        assertEquals(
                "4850543606044600326fc09cffc2e6c200000000000000000000000000000000"
                        + "09000000000000000000000006000000eae096ab93850975f72cbd58bf71cc41"
                        + "c31a0dea4fae",
                toHex(record)
        );
        byte[] secondRecord = channel.seal(
                SecureProtocol.PHONE_SAS_REVEAL,
                0,
                9,
                0,
                "vector".getBytes(StandardCharsets.US_ASCII)
        );
        assertEquals(
                "4850543606044600326fc09cffc2e6c200000000000000000100000000000000"
                        + "0900000000000000000000000600000033de984324ab761f90dcad0d39c3b93a"
                        + "dcfaeebc7fd0",
                toHex(secondRecord)
        );
        channel.close();
        host.destroy();

        byte[] hostPublic = fromHex(
                "d89e3bad79437dbed9f843418304f460ff05c7fe81fe4a9577a804cb9367ff66"
        );
        byte[] ikExchange = Arrays.copyOfRange(sequence(0xF0), 0, 16);
        byte[] ikPrologue = SecureProtocol.prologue(SecureProtocol.TransportKind.USB, ikExchange);
        HandshakeState ikPhone = handshake(
                IK_NAME,
                HandshakeState.INITIATOR,
                phoneStatic,
                sequence(0x90),
                hostPublic,
                ikPrologue
        );
        HandshakeState ikHost = handshake(
                IK_NAME,
                HandshakeState.RESPONDER,
                hostStatic,
                sequence(0xB0),
                null,
                ikPrologue
        );
        length = ikPhone.writeMessage(buffer, 0, null, 0, 0);
        assertEquals(
                "9fd7ad6dcff4298dd3f96d5b1b2af910a0535b1488d7f8fabb349a982880b615"
                        + "ea374cd73714b7bd8d86c36ef4edda85485b3a2b38748dff758fd6ec58a7fb5a"
                        + "8a8e41171e51f08d93b6aa549f14cc552d48f941f2db491d8f61982720b5602d",
                toHex(buffer, length)
        );
        assertEquals(0, ikHost.readMessage(buffer, 0, length, payload, 0));
        // IK message 2 carries the session parameters (replacing v5 HELLO).
        byte[] parameters = ByteBuffer.allocate(SecureProtocol.SESSION_PARAMETERS_SIZE)
                .order(ByteOrder.LITTLE_ENDIAN)
                .putInt(128).put((byte) 6).put((byte) 0).putShort((short) 0)
                .putLong(0x0807_0605_0403_0201L)
                .array();
        length = ikHost.writeMessage(buffer, 0, parameters, 0, parameters.length);
        assertEquals(
                "3f3e5f6d86926c9c128cf84581574f96840d98ee5ab53b1ec3b76e2bb25b945e"
                        + "512fc5d532d298f88d3122fadeced96e315f4782fef1855fabe09742b9d4dbf7",
                toHex(buffer, length)
        );
        assertEquals(parameters.length, ikPhone.readMessage(buffer, 0, length, payload, 0));
        assertArrayEquals(parameters, Arrays.copyOf(payload, parameters.length));
        SecureProtocol.HostControl control = new SecureProtocol.HostControl();
        SecureProtocol.decodeHostControl(payload, parameters.length, 256, control);
        assertEquals(128, control.receiveWindow);
        assertEquals(0x0807_0605_0403_0201L, control.hostSendNanos);
        assertEquals(
                "45f5c953a9233003fc076d6d069257ec76a04d65af607be74cb70246411d10e7",
                toHex(ikPhone.getHandshakeHash())
        );
        assertArrayEquals(ikPhone.getHandshakeHash(), ikHost.getHandshakeHash());
        ikPhone.destroy();
        ikHost.destroy();
    }

    private static HandshakeState handshake(
            String protocol,
            int role,
            byte[] localPrivate,
            byte[] ephemeralPrivate,
            byte[] remotePublic,
            byte[] prologue
    ) throws Exception {
        HandshakeState state = new HandshakeState(protocol, role);
        state.getLocalKeyPair().setPrivateKey(localPrivate, 0);
        state.getFixedEphemeralKey().setPrivateKey(ephemeralPrivate, 0);
        if (remotePublic != null) state.getRemotePublicKey().setPublicKey(remotePublic, 0);
        state.setPrologue(prologue, 0, prologue.length);
        state.start();
        return state;
    }

    private static byte[] sequence(int start) {
        byte[] bytes = new byte[32];
        for (int index = 0; index < bytes.length; index++) {
            bytes[index] = (byte) (start + index);
        }
        return bytes;
    }

    private static byte[] fromHex(String value) {
        byte[] bytes = new byte[value.length() / 2];
        for (int index = 0; index < bytes.length; index++) {
            bytes[index] = (byte) Integer.parseInt(value.substring(index * 2, index * 2 + 2), 16);
        }
        return bytes;
    }

    private static long readLong(byte[] bytes, int offset) {
        return ByteBuffer.wrap(bytes).order(ByteOrder.LITTLE_ENDIAN).getLong(offset);
    }

    private static String toHex(byte[] bytes) {
        return toHex(bytes, bytes.length);
    }

    private static String toHex(byte[] bytes, int length) {
        StringBuilder builder = new StringBuilder(bytes.length * 2);
        for (int index = 0; index < length; index++) {
            builder.append(String.format("%02x", bytes[index] & 0xFF));
        }
        return builder.toString();
    }
}
