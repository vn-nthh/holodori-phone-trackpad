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

public final class V5ProtocolTest {
    private static final String XX_NAME = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
    private static final String IK_NAME = "Noise_IK_25519_ChaChaPoly_BLAKE2s";

    @Test
    public void reusableControlBuffersAuthenticateBeforeReplayCommit() throws Exception {
        try (V5Protocol.Channel channel = new V5Protocol.Channel(cipher(), cipher(), 9)) {
            byte[] record = hostControl(0);
            byte[] plaintext = new byte[V5Protocol.CONTROL_PAYLOAD_SIZE];
            V5Protocol.RecordHeader header = new V5Protocol.RecordHeader();
            record[record.length - 1] ^= 1;
            assertThrows(V5Protocol.AuthenticationException.class,
                    () -> channel.openInto(record, record.length, header, plaintext));
            assertArrayEquals(new byte[plaintext.length], plaintext);
            record[record.length - 1] ^= 1;
            channel.openInto(record, record.length, header, plaintext);
            assertEquals(0, header.packetNumber);
            assertEquals(128, ByteBuffer.wrap(plaintext).order(ByteOrder.LITTLE_ENDIAN).getInt());
            assertThrows(V5Protocol.ReplayException.class,
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
        V5Protocol.Channel channel = new V5Protocol.Channel(cipher(), receiver, 9);
        try {
            byte[] ack = hostControl(0);
            Future<?> receive = workers.submit(() -> {
                channel.openInto(ack, ack.length, new V5Protocol.RecordHeader(), new byte[16]);
                return null;
            });
            assertTrue(decryptStarted.await(2, TimeUnit.SECONDS));
            Future<byte[]> send = workers.submit(() -> channel.seal(V5Protocol.PHONE_PING, 7, 0, 0, new byte[0]));
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
        byte[] record = new byte[80];
        ByteBuffer header = ByteBuffer.wrap(record).order(ByteOrder.LITTLE_ENDIAN);
        header.put(new byte[]{'H', 'P', 'A', '5'}).put((byte) 5).put((byte) V5Protocol.HOST_ACK);
        header.putShort((short) 80).putLong(9).putLong(7).putLong(packetNumber).putLong(0);
        header.putInt(0).putShort((short) 16).putShort((short) 0);
        byte[] payload = ByteBuffer.allocate(16).order(ByteOrder.LITTLE_ENDIAN)
                .putInt(128).put((byte) 6).put((byte) 0).putShort((short) 0).putLong(42).array();
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
        byte[] encoded = V5Protocol.encodePairEnvelope(
                V5Protocol.PAIR_CONTINUE,
                exchange,
                2,
                V5Protocol.TransportKind.WIFI,
                new byte[]{7, 8, 9}
        );
        V5Protocol.PairEnvelope decoded = V5Protocol.decodePairEnvelope(encoded, encoded.length);
        assertEquals(V5Protocol.PAIR_CONTINUE, decoded.kind);
        assertArrayEquals(exchange, decoded.exchangeId);
        assertEquals(2, decoded.step);
        assertEquals(V5Protocol.TransportKind.WIFI, decoded.transport);
        assertArrayEquals(new byte[]{7, 8, 9}, decoded.payload);

        encoded[6]++;
        assertThrows(
                V5Protocol.ProtocolException.class,
                () -> V5Protocol.decodePairEnvelope(encoded, encoded.length)
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
                V5Protocol.sasPattern(V5Protocol.sasDigest(
                        handshakeHash,
                        phoneRandom,
                        hostRandom
                ))
        );
    }

    @Test
    public void replayWindowAcceptsReorderingOnlyOnce() {
        V5Protocol.ReplayWindow replay = new V5Protocol.ReplayWindow();
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
        V5Protocol.Channel channel = new V5Protocol.Channel(
                sender,
                receiver,
                0x0102_0304_0506_0708L
        );
        byte[] first = channel.seal(
                V5Protocol.PHONE_TOUCH,
                9,
                4,
                0,
                "same".getBytes(StandardCharsets.US_ASCII)
        );
        byte[] second = channel.seal(
                V5Protocol.PHONE_TOUCH,
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
        byte[] prologue = V5Protocol.prologue(V5Protocol.TransportKind.WIFI, exchange);
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
        byte[] buffer = new byte[V5Protocol.MAX_DATAGRAM_SIZE];
        byte[] payload = new byte[V5Protocol.MAX_DATAGRAM_SIZE];
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
                        + "bfa33a1e12cb80ccbaa5fe3be21e12a6cf4b9a56b3cdc11bcb166b362cb1b576",
                toHex(buffer, length)
        );
        assertEquals(0, host.readMessage(buffer, 0, length, payload, 0));
        length = host.writeMessage(buffer, 0, null, 0, 0);
        assertEquals(
                "f531830cca96c417accf9c7fbb8b15f7eb91cc4ec6e41d779f704ed44dc67f66"
                        + "d8795cbaffa82eeb78befae0e0cde6c0d922ad90d8718e5c88d2cdcb78ed9563",
                toHex(buffer, length)
        );
        assertEquals(0, phone.readMessage(buffer, 0, length, payload, 0));
        assertEquals(
                "bbd8c76e72aba9685e6855cc0862de61d1d01529342cb8987f23c9a8b65e647e",
                toHex(host.getHandshakeHash())
        );
        assertArrayEquals(host.getHandshakeHash(), phone.getHandshakeHash());

        byte[] xxHash = phone.getHandshakeHash().clone();
        V5Protocol.Channel channel = new V5Protocol.Channel(phone.split(), xxHash);
        byte[] record = channel.seal(
                V5Protocol.PHONE_SAS_REVEAL,
                0,
                9,
                0,
                "vector".getBytes(StandardCharsets.US_ASCII)
        );
        assertEquals(
                "4850543505044600b8be365398adc6fe000000000000000000000000000000000"
                        + "9000000000000000000000006000000eae096ab9385ca84ff8fd2b82c4de6cc"
                        + "4890137c4c0d",
                toHex(record)
        );
        byte[] secondRecord = channel.seal(
                V5Protocol.PHONE_SAS_REVEAL,
                0,
                9,
                0,
                "vector".getBytes(StandardCharsets.US_ASCII)
        );
        assertEquals(
                "4850543505044600b8be365398adc6fe000000000000000001000000000000000"
                        + "900000000000000000000000600000033de984324ab289c9dd1f981e60265f9f"
                        + "f97e2d743e6",
                toHex(secondRecord)
        );
        channel.close();
        host.destroy();

        byte[] hostPublic = fromHex(
                "d89e3bad79437dbed9f843418304f460ff05c7fe81fe4a9577a804cb9367ff66"
        );
        byte[] ikExchange = Arrays.copyOfRange(sequence(0xF0), 0, 16);
        byte[] ikPrologue = V5Protocol.prologue(V5Protocol.TransportKind.USB, ikExchange);
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
                        + "742888fec59468946610d729351f3f31f7693e1d35a73a19431d9b717c57d0fb",
                toHex(buffer, length)
        );
        assertEquals(0, ikHost.readMessage(buffer, 0, length, payload, 0));
        length = ikHost.writeMessage(buffer, 0, null, 0, 0);
        assertEquals(
                "3f3e5f6d86926c9c128cf84581574f96840d98ee5ab53b1ec3b76e2bb25b945e"
                        + "d563e952a259dcdc24aab223c0760b12",
                toHex(buffer, length)
        );
        assertEquals(0, ikPhone.readMessage(buffer, 0, length, payload, 0));
        assertEquals(
                "217b487f44138992d172c6902fc2ba17c08d0205cb11c9b2e209f9aeeffaf3a8",
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
