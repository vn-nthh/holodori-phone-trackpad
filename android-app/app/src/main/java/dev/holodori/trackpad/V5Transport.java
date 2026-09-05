package dev.holodori.trackpad;

import android.content.Context;
import android.net.wifi.WifiInfo;
import android.net.wifi.WifiManager;
import android.os.Process;
import android.util.Log;

import com.southernstorm.noise.protocol.CipherStatePair;
import com.southernstorm.noise.protocol.HandshakeState;

import java.io.IOException;
import java.net.DatagramPacket;
import java.net.DatagramSocket;
import java.net.InetSocketAddress;
import java.net.SocketTimeoutException;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.security.SecureRandom;
import java.util.Arrays;
import java.util.HashSet;
import dev.holodori.trackpad.V5SendQueue.Frame;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/** Authenticated protocol-v5 transport for pairing and lossless touch input. */
final class V5Transport implements TouchTransport {
    interface PairingListener {
        void onPairingStatus(String message);

        void onPatternReady();

        void onPatternMatched();

        void onQuality(String message);

        void onPairingComplete();

        void onPairingFailed(String message);
    }

    private static final String TAG = "HolodoriUDP5";
    private static final String XX_NAME = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
    private static final String IK_NAME = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
    private static final long DISCOVERY_INTERVAL_NANOS = 100_000_000L;
    private static final long APPLICATION_RETRY_NANOS = 100_000_000L;
    private static final long HANDSHAKE_TIMEOUT_NANOS = 5_000_000_000L;
    private static final long PAIRING_TIMEOUT_NANOS = 60_000_000_000L;
    private static final long INTERFACE_REVALIDATE_NANOS = 500_000_000L;
    private static final long IDLE_PING_NANOS = 500_000_000L;
    private static final long IDLE_HOST_TIMEOUT_NANOS = 2_000_000_000L;
    private static final long ACTIVE_HOST_TIMEOUT_NANOS = 64_000_000L;
    private static final long MAX_GAMEPLAY_BACKLOG_NANOS = 64_000_000L;
    private static final long WATCHDOG_INTERVAL_MILLIS = 4L;
    private static final long ACTIVE_HEARTBEAT_NANOS = 8_000_000L;
    private static final int INITIAL_SEND_COPIES = 2;
    private static final int DEFAULT_HOST_WINDOW = 64;
    private static final int MAX_HOST_WINDOW = 256;
    private static final int MAX_TOUCH_PAYLOAD = V5Protocol.TOUCH_PAYLOAD_HEADER_SIZE
            + TouchSample.MAX_CONTACTS * V5Protocol.CONTACT_SIZE;
    private static final int WRITER_READY_TIMEOUT_MILLIS = 1_000;

    private final Context context;
    private final TouchTransport.Listener listener;
    private final V5Protocol.TransportKind transport;
    private final CredentialStore credentials;
    private final WifiManager wifiManager;
    private final SecureRandom random = new SecureRandom();
    private final Object lifecycleLock = new Object();
    private final Object queueLock = new Object();
    private final V5SendQueue unacknowledged = new V5SendQueue(MAX_HOST_WINDOW, MAX_TOUCH_PAYLOAD);
    private final ArrayBlockingQueue<int[]> submittedPattern = new ArrayBlockingQueue<>(1);
    private final int[] retainedPointerIds = new int[TouchSample.MAX_CONTACTS];
    private final float[] retainedX = new float[TouchSample.MAX_CONTACTS];
    private final float[] retainedY = new float[TouchSample.MAX_CONTACTS];
    private final float[] retainedPressure = new float[TouchSample.MAX_CONTACTS];
    private final float[] retainedTouchMajor = new float[TouchSample.MAX_CONTACTS];
    private final boolean[] retainedTouching = new boolean[TouchSample.MAX_CONTACTS];

    private volatile int generation;
    private volatile boolean running;
    private volatile boolean pairing;
    private volatile V5NetworkBinding binding;
    private volatile V5Protocol.Channel channel;

    private Thread controlThread;
    private Thread writerThread;
    private Thread watchdogThread;
    private Thread pairingThread;
    private boolean hostReady;
    private boolean sessionStarted;
    private int hostWindow = DEFAULT_HOST_WINDOW;
    private int hostLaneCount = 6;
    private long sessionId;
    private long nextSequence;
    private long highestAcknowledged = V5Protocol.NO_ACK;
    private long lastHeartbeatNanos;
    private long lastPingNanos;
    private long nextPingId;
    private long lastPongId = V5Protocol.NO_ACK;
    private long lastIdleResponseNanos;
    private long lastHostSendNanos;
    private long lastControlReceiveNanos;
    private long lastAcknowledgementProgressNanos;
    private long activePathStartedNanos;
    private int activeContactCount;
    private int retainedContactCount;
    private boolean queueOverflowed;

    V5Transport(
            Context context,
            TouchTransport.Listener listener,
            V5Protocol.TransportKind transport
    ) {
        this.context = context.getApplicationContext() == null
                ? context
                : context.getApplicationContext();
        this.listener = listener;
        this.transport = transport;
        credentials = new CredentialStore(this.context);
        wifiManager = (WifiManager) this.context.getSystemService(Context.WIFI_SERVICE);
    }

    boolean startPairing(PairingListener callback) {
        if (callback == null) throw new IllegalArgumentException("Pairing listener is required");
        close();
        submittedPattern.clear();
        synchronized (lifecycleLock) {
            pairing = true;
            int pairGeneration = ++generation;
            pairingThread = new Thread(
                    () -> pairingLoop(pairGeneration, callback),
                    "UDP5 secure pairing"
            );
            pairingThread.start();
        }
        return true;
    }

    boolean submitPairingPattern(int[] pattern) {
        if (!pairing || pattern == null || pattern.length != 8) return false;
        int[] copy = pattern.clone();
        for (int lane : copy) {
            if (lane < 1 || lane > 6) return false;
        }
        return submittedPattern.offer(copy);
    }

    void cancelPairing() {
        if (pairing) sendAbortBestEffort(1);
        close();
    }

    @Override
    public boolean open() {
        close();
        try {
            if (!credentials.isPaired()) {
                listener.onConnectionChanged(false, "Pair this phone before Start");
                return false;
            }
        } catch (CredentialStore.CredentialException error) {
            listener.onConnectionChanged(false, error.getMessage());
            return false;
        }

        CountDownLatch writerReady = new CountDownLatch(1);
        synchronized (queueLock) {
            resetSessionStateLocked();
        }
        synchronized (lifecycleLock) {
            running = true;
            int sessionGeneration = ++generation;
            controlThread = new Thread(
                    () -> controlLoop(sessionGeneration),
                    "UDP5 handshake and acknowledgements"
            );
            writerThread = new Thread(
                    () -> writerLoop(sessionGeneration, writerReady),
                    "UDP5 touch writer"
            );
            watchdogThread = new Thread(
                    () -> watchdogLoop(sessionGeneration),
                    "UDP5 liveness watchdog"
            );
            controlThread.start();
            writerThread.start();
            watchdogThread.start();
        }
        try {
            if (!writerReady.await(WRITER_READY_TIMEOUT_MILLIS, TimeUnit.MILLISECONDS)
                    || !running) {
                close();
                listener.onConnectionChanged(false, "V5 writer did not become ready");
                return false;
            }
        } catch (InterruptedException error) {
            Thread.currentThread().interrupt();
            close();
            listener.onConnectionChanged(false, "V5 connection interrupted");
            return false;
        }
        listener.onConnectionChanged(
                false,
                "Searching for the paired host over "
                        + (transport == V5Protocol.TransportKind.WIFI
                        ? "Wi-Fi / local network"
                        : "USB tethering")
        );
        return true;
    }

    @Override
    public boolean isRunning() {
        return running;
    }

    @Override
    public void offerFrame(
            int action,
            int actionPointerId,
            boolean locked,
            long eventNanos,
            long callbackNanos,
            boolean historical,
            int contactCount,
            int[] pointerIds,
            float[] x,
            float[] y,
            float[] pressure,
            float[] touchMajor,
            boolean[] touching
    ) {
        if (contactCount < 0 || contactCount > TouchSample.MAX_CONTACTS) {
            throw new IllegalArgumentException("Unsupported contact count: " + contactCount);
        }
        synchronized (queueLock) {
            boolean wasActiveDataPath = hasActiveDataPathLocked();
            activeContactCount = countActiveContacts(touching, contactCount);
            retainContactsLocked(
                    contactCount,
                    pointerIds,
                    x,
                    y,
                    pressure,
                    touchMajor,
                    touching
            );
            // Handshake and sequence-zero CANCEL form a hard boundary. Keep
            // only the latest snapshot until the host commits that boundary.
            if (!running || !sessionStarted) return;
            enqueueFrameLocked(
                    action,
                    actionPointerId,
                    locked,
                    false,
                    eventNanos,
                    callbackNanos,
                    historical,
                    contactCount,
                    pointerIds,
                    x,
                    y,
                    pressure,
                    touchMajor,
                    touching
            );
            updateActivePathStateLocked(wasActiveDataPath, System.nanoTime());
            queueLock.notifyAll();
        }
    }

    private void enqueueFrameLocked(
            int action,
            int actionPointerId,
            boolean locked,
            boolean sessionStart,
            long eventNanos,
            long callbackNanos,
            boolean historical,
            int contactCount,
            int[] pointerIds,
            float[] x,
            float[] y,
            float[] pressure,
            float[] touchMajor,
            boolean[] touching
    ) {
        if (unacknowledged.size() >= MAX_HOST_WINDOW) {
            // Never evict or coalesce gameplay. The watchdog turns this into
            // a clean authenticated-session failure and the retained snapshot
            // is reconstructed only after a fresh IK + sequence-zero CANCEL.
            queueOverflowed = true;
            queueLock.notifyAll();
            return;
        }
        long sequence = nextSequence++;
        int payloadLength = V5Protocol.TOUCH_PAYLOAD_HEADER_SIZE
                + contactCount * V5Protocol.CONTACT_SIZE;
        Frame pending = unacknowledged.add(sequence, payloadLength, System.nanoTime());
        ByteBuffer packet = pending.writer;
        packet.putLong(eventNanos);
        packet.putLong(callbackNanos);
        packet.putLong(0L);
        packet.putLong(0L);
        packet.putLong(0L);
        packet.put((byte) action);
        packet.put((byte) (actionPointerId & 0xFF));
        packet.put((byte) contactCount);
        int frameFlags = locked ? TouchSample.FRAME_FLAG_LOCKED : 0;
        if (sessionStart) frameFlags |= TouchSample.FRAME_FLAG_SESSION_START;
        if (historical) frameFlags |= TouchSample.FRAME_FLAG_HISTORICAL;
        packet.put((byte) frameFlags);
        for (int index = 0; index < contactCount; index++) {
            float localX = x[index];
            float localY = y[index];
            boolean inside = localX >= 0f && localX <= 1f
                    && localY >= 0f && localY <= 1f;
            int contactFlags = inside ? TouchSample.CONTACT_FLAG_INSIDE : 0;
            if (touching[index]) contactFlags |= TouchSample.CONTACT_FLAG_TIP;
            packet.put((byte) (pointerIds[index] & 0xFF));
            packet.put((byte) contactFlags);
            packet.putShort((short) clampFixed(localX));
            packet.putShort((short) clampFixed(localY));
            packet.putShort((short) normalizeUnsigned(pressure[index]));
            packet.putShort((short) normalizeUnsigned(touchMajor[index]));
        }
    }

    private void controlLoop(int sessionGeneration) {
        try {
            V5NetworkBinding sessionBinding = V5NetworkBinding.open(context, transport);
            if (!installBinding(sessionGeneration, sessionBinding)) return;
            V5Protocol.Channel sessionChannel = establishRemembered(
                    sessionGeneration,
                    sessionBinding
            );
            if (!installChannel(sessionGeneration, sessionChannel)) return;

            long helloDeadline = System.nanoTime() + HANDSHAKE_TIMEOUT_NANOS;
            V5Protocol.RecordHeader header = new V5Protocol.RecordHeader();
            byte[] controlPayload = new byte[V5Protocol.CONTROL_PAYLOAD_SIZE];
            ByteBuffer control = ByteBuffer.wrap(controlPayload).order(ByteOrder.LITTLE_ENDIAN);
            byte[] receiveBytes = new byte[V5Protocol.MAX_DATAGRAM_SIZE];
            DatagramPacket incoming = new DatagramPacket(receiveBytes, receiveBytes.length);
            while (isGameplayActive(sessionGeneration) && !hostReady) {
                if (System.nanoTime() >= helloDeadline) {
                    throw new IOException("Authenticated host HELLO timed out");
                }
                boolean received = receiveAuthenticated(
                        sessionGeneration,
                        sessionBinding,
                        sessionChannel,
                        receiveBytes,
                        incoming,
                        header,
                        controlPayload
                );
                if (received) handleHostControl(sessionGeneration, header, control);
            }

            while (isGameplayActive(sessionGeneration)) {
                boolean received = receiveAuthenticated(
                        sessionGeneration,
                        sessionBinding,
                        sessionChannel,
                        receiveBytes,
                        incoming,
                        header,
                        controlPayload
                );
                if (received) handleHostControl(sessionGeneration, header, control);
                checkHostTimeout(sessionGeneration);
            }
        } catch (Exception error) {
            failGameplay(sessionGeneration, error);
        }
    }

    private V5Protocol.Channel establishRemembered(
            int sessionGeneration,
            V5NetworkBinding sessionBinding
    ) throws Exception {
        CredentialStore.Identity identity = credentials.load();
        if (identity == null || !identity.hasPairedHost()) {
            if (identity != null) identity.destroy();
            throw new IOException("No paired host identity");
        }
        HandshakeState handshake = null;
        byte[] exchangeId = new byte[16];
        byte[] message = new byte[V5Protocol.MAX_DATAGRAM_SIZE];
        byte[] payload = new byte[V5Protocol.MAX_DATAGRAM_SIZE];
        try {
            random.nextBytes(exchangeId);
            handshake = new HandshakeState(IK_NAME, HandshakeState.INITIATOR);
            handshake.getLocalKeyPair().setPrivateKey(identity.privateKey, 0);
            handshake.getRemotePublicKey().setPublicKey(identity.pairedHostPublicKey, 0);
            byte[] prologue = V5Protocol.prologue(transport, exchangeId);
            handshake.setPrologue(prologue, 0, prologue.length);
            Arrays.fill(prologue, (byte) 0);
            handshake.start();
            int messageLength = handshake.writeMessage(message, 0, null, 0, 0);
            byte[] request = V5Protocol.encodePairEnvelope(
                    V5Protocol.IK_MESSAGE_1,
                    exchangeId,
                    1,
                    transport,
                    Arrays.copyOf(message, messageLength)
            );
            long deadline = System.nanoTime() + HANDSHAKE_TIMEOUT_NANOS;
            long nextSend = 0;
            DatagramPacket incoming = new DatagramPacket(
                    new byte[V5Protocol.MAX_DATAGRAM_SIZE],
                    V5Protocol.MAX_DATAGRAM_SIZE
            );
            while (isGameplayActive(sessionGeneration) && System.nanoTime() < deadline) {
                long now = System.nanoTime();
                if (now >= nextSend) {
                    sessionBinding.sendDiscovery(request);
                    nextSend = now + DISCOVERY_INTERVAL_NANOS;
                }
                incoming.setLength(incoming.getData().length);
                try {
                    sessionBinding.socket().receive(incoming);
                } catch (SocketTimeoutException ignored) {
                    continue;
                }
                InetSocketAddress source = (InetSocketAddress) incoming.getSocketAddress();
                V5Protocol.PairEnvelope envelope;
                try {
                    envelope = V5Protocol.decodePairEnvelope(
                            incoming.getData(),
                            incoming.getLength()
                    );
                } catch (V5Protocol.ProtocolException ignored) {
                    continue;
                }
                if (envelope.transport != transport
                        || !Arrays.equals(envelope.exchangeId, exchangeId)
                        || envelope.kind != V5Protocol.IK_CONTINUE
                        || envelope.step != 2
                        || !sessionBinding.acceptAndPin(source)) {
                    continue;
                }
                int decrypted = handshake.readMessage(
                        envelope.payload,
                        0,
                        envelope.payload.length,
                        payload,
                        0
                );
                if (decrypted != 0 || handshake.getAction() != HandshakeState.SPLIT) {
                    throw new IOException("Malformed Noise IK response");
                }
                byte[] handshakeHash = handshake.getHandshakeHash().clone();
                CipherStatePair split = handshake.split();
                try {
                    return new V5Protocol.Channel(split, handshakeHash);
                } finally {
                    Arrays.fill(handshakeHash, (byte) 0);
                }
            }
            throw new IOException("Paired host not found on the selected interface");
        } finally {
            identity.destroy();
            Arrays.fill(exchangeId, (byte) 0);
            Arrays.fill(message, (byte) 0);
            Arrays.fill(payload, (byte) 0);
            if (handshake != null) handshake.destroy();
        }
    }

    private boolean receiveAuthenticated(
            int sessionGeneration,
            V5NetworkBinding sessionBinding,
            V5Protocol.Channel sessionChannel,
            byte[] bytes,
            DatagramPacket incoming,
            V5Protocol.RecordHeader header,
            byte[] plaintext
    ) throws IOException {
        incoming.setLength(bytes.length);
        try {
            sessionBinding.socket().receive(incoming);
        } catch (SocketTimeoutException ignored) {
            return false;
        }
        if (!isGameplayActive(sessionGeneration)
                || !sessionBinding.isPinnedPeer(incoming)) {
            return false;
        }
        try {
            header.receivedNanos = System.nanoTime();
            sessionChannel.openInto(bytes, incoming.getLength(), header, plaintext);
            return true;
        } catch (V5Protocol.AuthenticationException | V5Protocol.ReplayException ignored) {
            return false;
        } catch (V5Protocol.ProtocolException ignored) {
            // Header corruption is unauthenticated until AEAD succeeds. Drop it
            // silently and never let hostile traffic sustain liveness.
            return false;
        }
    }

    private void handleHostControl(
            int sessionGeneration,
            V5Protocol.RecordHeader header,
            ByteBuffer control
    ) throws IOException {
        if (header.flags != 0
                || header.payloadLength != V5Protocol.CONTROL_PAYLOAD_SIZE) {
            throw new IOException("Malformed authenticated host control");
        }
        control.clear();
        long window = Integer.toUnsignedLong(control.getInt());
        int lanes = Byte.toUnsignedInt(control.get());
        int controlFlags = Byte.toUnsignedInt(control.get());
        int reserved = Short.toUnsignedInt(control.getShort());
        long hostSendNanos = control.getLong();
        if (window < 1 || window > MAX_HOST_WINDOW || lanes != 6
                || controlFlags != 0 || reserved != 0) {
            throw new IOException("Invalid authenticated host control values");
        }

        boolean becameReady = false;
        synchronized (queueLock) {
            if (!isGameplayActive(sessionGeneration)) return;
            long receiveNanos = header.receivedNanos;
            if (header.messageType == V5Protocol.HOST_HELLO) {
                if (header.sessionId != 0
                        || header.logicalId != V5Protocol.NO_ACK) {
                    throw new IOException("Invalid authenticated HELLO");
                }
                if (!hostReady) {
                    hostWindow = (int) window;
                    hostLaneCount = lanes;
                    hostReady = true;
                    lastIdleResponseNanos = receiveNanos;
                    sessionStarted = false;
                    sessionId = nextSessionId();
                    nextSequence = 0;
                    highestAcknowledged = V5Protocol.NO_ACK;
                    clearUnacknowledgedLocked();
                    long now = System.nanoTime();
                    enqueueFrameLocked(
                            TouchSample.ACTION_CANCEL,
                            0,
                            false,
                            true,
                            now,
                            now,
                            false,
                            0,
                            null,
                            null,
                            null,
                            null,
                            null,
                            null
                    );
                    if (activeContactCount > 0) activePathStartedNanos = now;
                    becameReady = true;
                }
            } else if (header.messageType == V5Protocol.HOST_ACK) {
                if (!hostReady || header.sessionId != sessionId) {
                    throw new IOException("ACK belongs to the wrong V5 session");
                }
                if (acknowledgeLocked(header.logicalId)) {
                    lastAcknowledgementProgressNanos = receiveNanos;
                    lastIdleResponseNanos = receiveNanos;
                    if (!sessionStarted
                            && Long.compareUnsigned(header.logicalId, 0L) >= 0) {
                        sessionStarted = true;
                        lastHeartbeatNanos = 0;
                    }
                }
            } else if (header.messageType == V5Protocol.HOST_PONG) {
                if (!sessionStarted || header.sessionId != sessionId
                        || Long.compareUnsigned(header.logicalId, nextPingId) >= 0) {
                    throw new IOException("Invalid authenticated PONG");
                }
                if (lastPongId == V5Protocol.NO_ACK
                        || Long.compareUnsigned(header.logicalId, lastPongId) > 0) {
                    lastPongId = header.logicalId;
                    lastIdleResponseNanos = receiveNanos;
                }
            } else {
                throw new IOException("Unexpected authenticated host message");
            }
            lastHostSendNanos = hostSendNanos;
            lastControlReceiveNanos = receiveNanos;
            queueLock.notifyAll();
        }
        if (becameReady) {
            listener.onHostLaneCountChanged(hostLaneCount);
            listener.onConnectionChanged(
                    true,
                    "Authenticated V5 over "
                            + (transport == V5Protocol.TransportKind.WIFI
                            ? "Wi-Fi / local network"
                            : "USB tethering")
                            + " connected"
            );
        }
    }

    private void writerLoop(int sessionGeneration, CountDownLatch writerReady) {
        Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_DISPLAY);
        writerReady.countDown();
        byte[] attemptPayload = new byte[MAX_TOUCH_PAYLOAD];
        ByteBuffer attempt = ByteBuffer.wrap(attemptPayload).order(ByteOrder.LITTLE_ENDIAN);
        byte[] sendBuffer = new byte[V5Protocol.MAX_DATAGRAM_SIZE];
        DatagramPacket outgoing = new DatagramPacket(sendBuffer, 0);
        try {
            while (isGameplayActive(sessionGeneration)) {
                V5NetworkBinding sessionBinding;
                V5Protocol.Channel sessionChannel;
                int payloadLength;
                int messageType;
                long frameSession;
                long sequence;
                synchronized (queueLock) {
                    if (!isGameplayActive(sessionGeneration)) break;
                    long now = System.nanoTime();
                    maybeEnqueueHeartbeatLocked(now);
                    sessionBinding = binding;
                    sessionChannel = channel;
                    if (!hostReady || sessionBinding == null || sessionChannel == null) {
                        queueLock.wait();
                        continue;
                    }
                    Frame pending = unacknowledged.next(now, hostWindow);
                    frameSession = sessionId;
                    if (pending != null) {
                        payloadLength = pending.payloadLength;
                        System.arraycopy(pending.payload, 0, attemptPayload, 0, payloadLength);
                        attempt.putLong(24, lastHostSendNanos);
                        attempt.putLong(32, lastControlReceiveNanos);
                        sequence = pending.sequence;
                        messageType = V5Protocol.PHONE_TOUCH;
                    } else if (sessionStarted && !hasActiveDataPathLocked()
                            && now - lastPingNanos >= IDLE_PING_NANOS) {
                        payloadLength = 0;
                        sequence = nextPingId++;
                        lastPingNanos = now;
                        messageType = V5Protocol.PHONE_PING;
                    } else {
                        long delay = unacknowledged.nanosUntilSend(now, hostWindow);
                        if (sessionStarted && unacknowledged.isEmpty()) {
                            delay = Math.min(delay, activeContactCount > 0
                                    ? ACTIVE_HEARTBEAT_NANOS - (now - lastHeartbeatNanos)
                                    : IDLE_PING_NANOS - (now - lastPingNanos));
                        }
                        if (delay == Long.MAX_VALUE) queueLock.wait();
                        else if (delay > 0) TimeUnit.NANOSECONDS.timedWait(queueLock, delay);
                        continue;
                    }
                }
                if (payloadLength != 0) attempt.putLong(16, System.nanoTime());
                int sendLength = sessionChannel.sealInto(
                        messageType, frameSession, sequence, 0,
                        attemptPayload, payloadLength, sendBuffer
                );
                outgoing.setData(sendBuffer, 0, sendLength);
                sessionBinding.sendToPeer(outgoing);
                if (messageType == V5Protocol.PHONE_PING) {
                    sendLength = sessionChannel.sealInto(
                            messageType, frameSession, sequence, 0,
                            attemptPayload, payloadLength, sendBuffer
                    );
                    outgoing.setData(sendBuffer, 0, sendLength);
                    sessionBinding.sendToPeer(outgoing);
                }
            }
        } catch (InterruptedException ignored) {
            Thread.currentThread().interrupt();
        } catch (Exception error) {
            failGameplay(sessionGeneration, error);
        }
    }

    private void maybeEnqueueHeartbeatLocked(long nowNanos) {
        if (!hostReady || !sessionStarted || activeContactCount == 0
                || !unacknowledged.isEmpty()
                || nowNanos - lastHeartbeatNanos < ACTIVE_HEARTBEAT_NANOS) {
            return;
        }
        lastHeartbeatNanos = nowNanos;
        enqueueFrameLocked(
                TouchSample.ACTION_HEARTBEAT,
                0,
                true,
                false,
                nowNanos,
                nowNanos,
                false,
                retainedContactCount,
                retainedPointerIds,
                retainedX,
                retainedY,
                retainedPressure,
                retainedTouchMajor,
                retainedTouching
        );
    }

    private boolean acknowledgeLocked(long acknowledgedSequence) {
        if (acknowledgedSequence == V5Protocol.NO_ACK
                || (highestAcknowledged != V5Protocol.NO_ACK
                && Long.compareUnsigned(acknowledgedSequence, highestAcknowledged) <= 0)
                || Long.compareUnsigned(acknowledgedSequence, nextSequence) >= 0) {
            return false;
        }
        highestAcknowledged = acknowledgedSequence;
        while (!unacknowledged.isEmpty()) {
            Frame first = unacknowledged.peekFirst();
            if (Long.compareUnsigned(first.sequence, acknowledgedSequence) > 0) break;
            unacknowledged.removeFirst();
        }
        if (!hasActiveDataPathLocked()) activePathStartedNanos = 0;
        return true;
    }

    private void checkHostTimeout(int sessionGeneration) {
        boolean timedOut = false;
        boolean backlogExpired = false;
        boolean overflowed = false;
        int staleFrames = 0;
        synchronized (queueLock) {
            if (!isGameplayActive(sessionGeneration) || !hostReady) return;
            long now = System.nanoTime();
            boolean active = hasActiveDataPathLocked();
            backlogExpired = gameplayBacklogExpiredLocked(now);
            overflowed = queueOverflowed;
            long progress = Math.max(lastAcknowledgementProgressNanos, activePathStartedNanos);
            if ((active && now - progress >= ACTIVE_HOST_TIMEOUT_NANOS)
                    || (!active && sessionStarted
                    && now - lastIdleResponseNanos >= IDLE_HOST_TIMEOUT_NANOS)
                    || backlogExpired || overflowed) {
                timedOut = true;
                staleFrames = unacknowledged.size();
            }
        }
        if (timedOut && closeSession(sessionGeneration)) {
            listener.onConnectionChanged(
                    false,
                    (overflowed
                            ? "Input queue overflowed; abandoned "
                            : backlogExpired
                            ? "Input backlog expired; dropped "
                            : "Host not responding; dropped ")
                            + staleFrames
                            + " stale frames; restarting authenticated session"
            );
        }
    }

    private void watchdogLoop(int sessionGeneration) {
        long lastInterfaceCheck = System.nanoTime();
        while (isGameplayActive(sessionGeneration)) {
            try {
                Thread.sleep(WATCHDOG_INTERVAL_MILLIS);
                checkHostTimeout(sessionGeneration);
                long now = System.nanoTime();
                if (now - lastInterfaceCheck >= INTERFACE_REVALIDATE_NANOS) {
                    V5NetworkBinding current = binding;
                    if (current != null && !current.revalidate()) {
                        failGameplay(sessionGeneration, new IOException("Selected network interface changed"));
                        return;
                    }
                    lastInterfaceCheck = now;
                }
            } catch (InterruptedException ignored) {
                Thread.currentThread().interrupt();
                return;
            }
        }
    }

    private void pairingLoop(int pairGeneration, PairingListener callback) {
        CredentialStore.Identity identity = null;
        try {
            callback.onPairingStatus("Opening selected "
                    + (transport == V5Protocol.TransportKind.WIFI
                    ? "Wi-Fi / local network"
                    : "USB tether")
                    + " interface");
            V5NetworkBinding pairBinding = V5NetworkBinding.open(context, transport);
            if (!installBinding(pairGeneration, pairBinding)) return;
            identity = credentials.loadOrCreate();
            runInitialPairing(pairGeneration, pairBinding, identity, callback);
            if (closeSession(pairGeneration)) callback.onPairingComplete();
        } catch (ComparisonException error) {
            if (closeSession(pairGeneration)) {
                callback.onPairingFailed("Pattern did not match. Press Pair for a new attempt.");
            }
        } catch (Exception error) {
            if (closeSession(pairGeneration)) {
                Log.e(TAG, "V5 pairing failed", error);
                callback.onPairingFailed(safePairingMessage(error));
            }
        } finally {
            if (identity != null) identity.destroy();
        }
    }

    private void runInitialPairing(
            int pairGeneration,
            V5NetworkBinding pairBinding,
            CredentialStore.Identity identity,
            PairingListener callback
    ) throws Exception {
        long pairingDeadline = System.nanoTime() + PAIRING_TIMEOUT_NANOS;
        byte[] exchangeId = new byte[16];
        byte[] hostPublicKey = new byte[32];
        byte[] handshakeHash = null;
        byte[] phoneRandom = null;
        byte[] phoneCommit = null;
        byte[] hostCommit = null;
        byte[] expectedHostCommit = null;
        int[] expectedPattern = null;
        random.nextBytes(exchangeId);
        try {
        byte[] probe = V5Protocol.encodePairEnvelope(
                V5Protocol.PAIR_PROBE,
                exchangeId,
                0,
                transport,
                new byte[0]
        );
        callback.onPairingStatus("Waiting for the host Pair window");
        V5Protocol.PairEnvelope offer = waitForPairEnvelope(
                pairGeneration,
                pairBinding,
                pairingDeadline,
                pairingDeadline,
                exchangeId,
                V5Protocol.PAIR_OFFER,
                1,
                probe,
                true
        );

        HandshakeState handshake = new HandshakeState(XX_NAME, HandshakeState.RESPONDER);
        byte[] handshakeBuffer = new byte[V5Protocol.MAX_DATAGRAM_SIZE];
        byte[] payloadBuffer = new byte[V5Protocol.MAX_DATAGRAM_SIZE];
        V5Protocol.Channel pairChannel;
        try {
            handshake.getLocalKeyPair().setPrivateKey(identity.privateKey, 0);
            byte[] prologue = V5Protocol.prologue(transport, exchangeId);
            handshake.setPrologue(prologue, 0, prologue.length);
            Arrays.fill(prologue, (byte) 0);
            handshake.start();
            int offerPayload = handshake.readMessage(
                    offer.payload,
                    0,
                    offer.payload.length,
                    payloadBuffer,
                    0
            );
            if (offerPayload != 0) throw new IOException("Malformed Noise XX offer");
            int messageTwoLength = handshake.writeMessage(
                    handshakeBuffer,
                    0,
                    null,
                    0,
                    0
            );
            byte[] messageTwo = V5Protocol.encodePairEnvelope(
                    V5Protocol.PAIR_CONTINUE,
                    exchangeId,
                    2,
                    transport,
                    Arrays.copyOf(handshakeBuffer, messageTwoLength)
            );
            sendPlainRedundant(pairBinding, messageTwo);
            V5Protocol.PairEnvelope messageThree = waitForPairEnvelope(
                    pairGeneration,
                    pairBinding,
                    pairingDeadline,
                    System.nanoTime() + HANDSHAKE_TIMEOUT_NANOS,
                    exchangeId,
                    V5Protocol.PAIR_CONTINUE,
                    3,
                    messageTwo,
                    false
            );
            int messageThreePayload = handshake.readMessage(
                    messageThree.payload,
                    0,
                    messageThree.payload.length,
                    payloadBuffer,
                    0
            );
            if (messageThreePayload != 0 || handshake.getAction() != HandshakeState.SPLIT) {
                throw new IOException("Malformed Noise XX continuation");
            }
            handshake.getRemotePublicKey().getPublicKey(hostPublicKey, 0);
            handshakeHash = handshake.getHandshakeHash().clone();
            pairChannel = new V5Protocol.Channel(handshake.split(), handshakeHash);
        } finally {
            handshake.destroy();
            Arrays.fill(handshakeBuffer, (byte) 0);
            Arrays.fill(payloadBuffer, (byte) 0);
        }
        if (!installChannel(pairGeneration, pairChannel)) {
            return;
        }

        phoneRandom = new byte[32];
        random.nextBytes(phoneRandom);
        phoneCommit = V5Protocol.sasCommit(1, handshakeHash, phoneRandom);
        sendRecordRedundant(
                pairBinding,
                pairChannel,
                V5Protocol.PHONE_SAS_COMMITMENT,
                0,
                0,
                phoneCommit
        );
        V5Protocol.OpenedRecord hostCommitRecord = waitForPairRecord(
                pairGeneration,
                pairBinding,
                pairChannel,
                pairingDeadline,
                V5Protocol.HOST_SAS_COMMITMENT,
                V5Protocol.PHONE_SAS_COMMITMENT,
                phoneCommit
        );
        requirePairRecord(hostCommitRecord, 32);
        hostCommit = hostCommitRecord.payload.clone();
        sendRecordRedundant(
                pairBinding,
                pairChannel,
                V5Protocol.PHONE_SAS_REVEAL,
                0,
                0,
                phoneRandom
        );
        V5Protocol.OpenedRecord hostRevealRecord = waitForPairRecord(
                pairGeneration,
                pairBinding,
                pairChannel,
                pairingDeadline,
                V5Protocol.HOST_SAS_REVEAL,
                V5Protocol.PHONE_SAS_REVEAL,
                phoneRandom
        );
        requirePairRecord(hostRevealRecord, 32);
        expectedHostCommit = V5Protocol.sasCommit(
                2,
                handshakeHash,
                hostRevealRecord.payload
        );
        if (!MessageDigestSafe.equals(hostCommit, expectedHostCommit)) {
            sendAuthenticatedAbort(pairBinding, pairChannel, 2);
            throw new ComparisonException();
        }
        expectedPattern = V5Protocol.sasPattern(V5Protocol.sasDigest(
                handshakeHash,
                phoneRandom,
                hostRevealRecord.payload
        ));
        Arrays.fill(expectedHostCommit, (byte) 0);
        expectedHostCommit = null;
        callback.onPatternReady();
        callback.onQuality(currentSignalSummary());

        boolean patternConfirmed = false;
        long lastConfirmSend = 0;
        long lastInterfaceCheck = System.nanoTime();
        V5SendQueue repairs = new V5SendQueue(MAX_HOST_WINDOW, 32);
        HashSet<Long> qualitySeen = new HashSet<>();
        byte[] incomingBytes = new byte[V5Protocol.MAX_DATAGRAM_SIZE];
        DatagramPacket incoming = new DatagramPacket(incomingBytes, incomingBytes.length);
        while (isPairingActive(pairGeneration) && System.nanoTime() < pairingDeadline) {
            long now = System.nanoTime();
            if (now - lastInterfaceCheck >= INTERFACE_REVALIDATE_NANOS) {
                requireCurrentPairingBinding(pairBinding);
                lastInterfaceCheck = now;
            }
            Frame repair;
            while ((repair = repairs.next(now, MAX_HOST_WINDOW)) != null) {
                sendQualityReply(pairBinding, pairChannel, repair, true);
                repairs.removeFirst();
                now = System.nanoTime();
            }

            int[] entered = submittedPattern.poll();
            if (entered != null && !patternConfirmed) {
                if (!Arrays.equals(entered, expectedPattern)) {
                    sendAuthenticatedAbort(pairBinding, pairChannel, 2);
                    throw new ComparisonException();
                }
                patternConfirmed = true;
                callback.onPatternMatched();
                sendRecordRedundant(
                        pairBinding,
                        pairChannel,
                        V5Protocol.PHONE_PAIR_CONFIRM,
                        0,
                        0,
                        new byte[0]
                );
                lastConfirmSend = now;
            }
            if (patternConfirmed && now - lastConfirmSend >= APPLICATION_RETRY_NANOS) {
                sendRecordRedundant(
                        pairBinding,
                        pairChannel,
                        V5Protocol.PHONE_PAIR_CONFIRM,
                        0,
                        0,
                        new byte[0]
                );
                lastConfirmSend = now;
            }

            pairBinding.socket().setSoTimeout(repairs.isEmpty() ? 4 : 1);
            incoming.setLength(incomingBytes.length);
            try {
                pairBinding.socket().receive(incoming);
            } catch (SocketTimeoutException ignored) {
                continue;
            }
            if (!pairBinding.isPinnedPeer((InetSocketAddress) incoming.getSocketAddress())) {
                continue;
            }
            V5Protocol.OpenedRecord record;
            try {
                record = pairChannel.open(incomingBytes, incoming.getLength());
            } catch (V5Protocol.AuthenticationException
                     | V5Protocol.ReplayException ignored) {
                continue;
            } catch (V5Protocol.ProtocolException ignored) {
                continue;
            }
            if (record.header.sessionId != 0) {
                throw new IOException("Pairing record used a gameplay session");
            }
            if (record.header.messageType == V5Protocol.HOST_QUALITY_PROBE) {
                if (transport != V5Protocol.TransportKind.WIFI
                        || record.payload.length != 8
                        || (record.header.flags & ~V5Protocol.QUALITY_REPAIR_ONLY) != 0) {
                    throw new IOException("Malformed authenticated quality probe");
                }
                long received = System.nanoTime();
                if (!qualitySeen.add(record.header.logicalId)) continue;
                long hostSend = ByteBuffer.wrap(record.payload)
                        .order(ByteOrder.LITTLE_ENDIAN)
                        .getLong();
                boolean repairOnly = (record.header.flags & V5Protocol.QUALITY_REPAIR_ONLY) != 0;
                Frame quality = repairOnly ? repairs.add(record.header.logicalId, 32, received)
                        : new Frame(32);
                if (quality == null) throw new IOException("Quality probe queue overflowed");
                quality.sequence = record.header.logicalId;
                quality.writer.putLong(0, hostSend);
                quality.writer.putLong(8, received);
                if (repairOnly) {
                    quality.sendCount = INITIAL_SEND_COPIES;
                    quality.lastSentNanos = received;
                } else {
                    sendQualityReply(pairBinding, pairChannel, quality, false);
                }
            } else if (record.header.messageType == V5Protocol.HOST_PAIR_COMPLETE) {
                requirePairRecord(record, 0);
                if (!patternConfirmed) {
                    throw new IOException("Host completed pairing before phone confirmation");
                }
                credentials.savePairedHost(hostPublicKey);
                callback.onQuality(currentSignalSummary());
                return;
            } else if (record.header.messageType == V5Protocol.HOST_AUTH_ABORT) {
                throw new IOException("Host cancelled pairing");
            } else if (record.header.messageType == V5Protocol.HOST_SAS_REVEAL) {
                requirePairRecord(record, 32);
            } else {
                throw new IOException("Unexpected authenticated pairing record");
            }
        }
        sendAuthenticatedAbort(pairBinding, pairChannel, 3);
        throw new IOException("Pairing window expired");
        } finally {
            zeroPairingSecrets(
                    exchangeId,
                    hostPublicKey,
                    handshakeHash,
                    phoneRandom,
                    phoneCommit,
                    hostCommit,
                    expectedHostCommit,
                    expectedPattern
            );
        }
    }

    private V5Protocol.PairEnvelope waitForPairEnvelope(
            int pairGeneration,
            V5NetworkBinding pairBinding,
            long pairingDeadline,
            long stepDeadline,
            byte[] exchangeId,
            int expectedKind,
            long expectedStep,
            byte[] retry,
            boolean discovery
    ) throws Exception {
        long deadline = Math.min(pairingDeadline, stepDeadline);
        long nextSend = 0;
        long lastInterfaceCheck = System.nanoTime();
        byte[] bytes = new byte[V5Protocol.MAX_DATAGRAM_SIZE];
        DatagramPacket incoming = new DatagramPacket(bytes, bytes.length);
        while (isPairingActive(pairGeneration) && System.nanoTime() < deadline) {
            long now = System.nanoTime();
            if (now - lastInterfaceCheck >= INTERFACE_REVALIDATE_NANOS) {
                requireCurrentPairingBinding(pairBinding);
                lastInterfaceCheck = now;
            }
            if (now >= nextSend) {
                if (discovery) pairBinding.sendDiscovery(retry);
                else sendPlainRedundant(pairBinding, retry);
                nextSend = now + DISCOVERY_INTERVAL_NANOS;
            }
            incoming.setLength(bytes.length);
            try {
                pairBinding.socket().receive(incoming);
            } catch (SocketTimeoutException ignored) {
                continue;
            }
            InetSocketAddress source = (InetSocketAddress) incoming.getSocketAddress();
            V5Protocol.PairEnvelope envelope;
            try {
                envelope = V5Protocol.decodePairEnvelope(bytes, incoming.getLength());
            } catch (V5Protocol.ProtocolException ignored) {
                continue;
            }
            if (envelope.transport != transport
                    || !Arrays.equals(envelope.exchangeId, exchangeId)) {
                continue;
            }
            if (discovery) {
                if (envelope.kind != expectedKind || envelope.step != expectedStep
                        || !pairBinding.acceptAndPin(source)) {
                    continue;
                }
            } else if (!pairBinding.isPinnedPeer(source)) {
                continue;
            }
            if (envelope.kind == V5Protocol.PAIR_ABORT) {
                throw new IOException("Host cancelled pairing");
            }
            if (envelope.kind == expectedKind && envelope.step == expectedStep) {
                return envelope;
            }
        }
        throw new IOException("Noise pairing handshake timed out");
    }

    private V5Protocol.OpenedRecord waitForPairRecord(
            int pairGeneration,
            V5NetworkBinding pairBinding,
            V5Protocol.Channel pairChannel,
            long deadline,
            int expectedType,
            int retryType,
            byte[] retryPayload
    ) throws Exception {
        long nextRetry = System.nanoTime() + APPLICATION_RETRY_NANOS;
        long lastInterfaceCheck = System.nanoTime();
        byte[] bytes = new byte[V5Protocol.MAX_DATAGRAM_SIZE];
        DatagramPacket incoming = new DatagramPacket(bytes, bytes.length);
        while (isPairingActive(pairGeneration) && System.nanoTime() < deadline) {
            long now = System.nanoTime();
            if (now - lastInterfaceCheck >= INTERFACE_REVALIDATE_NANOS) {
                requireCurrentPairingBinding(pairBinding);
                lastInterfaceCheck = now;
            }
            if (now >= nextRetry) {
                sendRecordRedundant(
                        pairBinding,
                        pairChannel,
                        retryType,
                        0,
                        0,
                        retryPayload
                );
                nextRetry = now + APPLICATION_RETRY_NANOS;
            }
            incoming.setLength(bytes.length);
            try {
                pairBinding.socket().receive(incoming);
            } catch (SocketTimeoutException ignored) {
                continue;
            }
            if (!pairBinding.isPinnedPeer((InetSocketAddress) incoming.getSocketAddress())) {
                continue;
            }
            V5Protocol.OpenedRecord record;
            try {
                record = pairChannel.open(bytes, incoming.getLength());
            } catch (V5Protocol.AuthenticationException
                     | V5Protocol.ReplayException ignored) {
                continue;
            } catch (V5Protocol.ProtocolException ignored) {
                continue;
            }
            if (record.header.messageType == V5Protocol.HOST_AUTH_ABORT) {
                throw new IOException("Host cancelled pairing");
            }
            if (record.header.messageType == expectedType) return record;
        }
        throw new IOException("Authenticated pairing step timed out");
    }

    private static void requireCurrentPairingBinding(V5NetworkBinding pairBinding)
            throws IOException {
        if (!pairBinding.revalidate()) {
            throw new IOException("Selected network interface changed during pairing");
        }
    }

    private void sendQualityReply(
            V5NetworkBinding pairBinding,
            V5Protocol.Channel pairChannel,
            Frame quality,
            boolean repairOnly
    ) throws Exception {
        SignalInfo signal = currentSignal();
        byte[] payload = quality.payload;
        ByteBuffer reply = quality.writer;
        reply.position(24);
        reply.put((byte) (repairOnly ? 1 : 0));
        reply.put((byte) signal.level);
        reply.putShort((short) signal.rssi);
        reply.putInt(signal.frequency);
        for (int copy = 0; copy < (repairOnly ? 1 : INITIAL_SEND_COPIES); copy++) {
            reply.putLong(16, System.nanoTime());
            byte[] encrypted = pairChannel.seal(
                    V5Protocol.PHONE_QUALITY_REPLY,
                    0,
                    quality.sequence,
                    0,
                    payload
            );
            pairBinding.sendToPeer(encrypted);
        }
        Arrays.fill(payload, (byte) 0);
    }

    private static void requirePairRecord(V5Protocol.OpenedRecord record, int payloadLength)
            throws IOException {
        if (record.header.sessionId != 0
                || record.header.logicalId != 0
                || record.header.flags != 0
                || record.payload.length != payloadLength) {
            throw new IOException("Malformed authenticated pairing control");
        }
    }

    private void sendPlainRedundant(V5NetworkBinding pairBinding, byte[] bytes)
            throws IOException {
        pairBinding.sendToPeer(bytes);
        pairBinding.sendToPeer(bytes);
    }

    private void sendRecordRedundant(
            V5NetworkBinding pairBinding,
            V5Protocol.Channel pairChannel,
            int messageType,
            long logicalId,
            long flags,
            byte[] payload
    ) throws Exception {
        for (int copy = 0; copy < INITIAL_SEND_COPIES; copy++) {
            pairBinding.sendToPeer(pairChannel.seal(
                    messageType,
                    0,
                    logicalId,
                    flags,
                    payload
            ));
        }
    }

    private void sendAuthenticatedAbort(
            V5NetworkBinding pairBinding,
            V5Protocol.Channel pairChannel,
            int reason
    ) {
        byte[] payload = ByteBuffer.allocate(2)
                .order(ByteOrder.LITTLE_ENDIAN)
                .putShort((short) reason)
                .array();
        try {
            sendRecordRedundant(
                    pairBinding,
                    pairChannel,
                    V5Protocol.PHONE_AUTH_ABORT,
                    0,
                    0,
                    payload
            );
        } catch (Exception ignored) {
            // The socket may already be gone; cleanup remains the boundary.
        } finally {
            Arrays.fill(payload, (byte) 0);
        }
    }

    private void sendAbortBestEffort(int reason) {
        V5NetworkBinding currentBinding = binding;
        V5Protocol.Channel currentChannel = channel;
        if (currentBinding != null && currentChannel != null) {
            sendAuthenticatedAbort(currentBinding, currentChannel, reason);
        }
    }

    private boolean installBinding(int expectedGeneration, V5NetworkBinding next) {
        synchronized (lifecycleLock) {
            if (!isActive(expectedGeneration)) {
                next.close();
                return false;
            }
            binding = next;
            return true;
        }
    }

    private boolean installChannel(int expectedGeneration, V5Protocol.Channel next) {
        synchronized (lifecycleLock) {
            if (!isActive(expectedGeneration)) {
                next.close();
                return false;
            }
            channel = next;
            synchronized (queueLock) {
                queueLock.notifyAll();
            }
            return true;
        }
    }

    private void failGameplay(int sessionGeneration, Exception error) {
        if (!closeSession(sessionGeneration)) return;
        Log.e(TAG, "V5 authenticated session failed", error);
        listener.onConnectionChanged(false, "Authenticated "
                + (transport == V5Protocol.TransportKind.WIFI ? "Wi-Fi" : "USB")
                + " session lost; restarting");
    }

    private boolean isActive(int expectedGeneration) {
        return generation == expectedGeneration && (running || pairing);
    }

    private boolean isGameplayActive(int expectedGeneration) {
        return running && generation == expectedGeneration;
    }

    private boolean isPairingActive(int expectedGeneration) {
        return pairing && generation == expectedGeneration;
    }

    private boolean closeSession(int expectedGeneration) {
        return closeInternal(expectedGeneration, true);
    }

    @Override
    public void close() {
        closeInternal(0, false);
    }

    private boolean closeInternal(int expectedGeneration, boolean requireMatch) {
        Thread previousControl;
        Thread previousWriter;
        Thread previousWatchdog;
        Thread previousPairing;
        V5NetworkBinding previousBinding;
        V5Protocol.Channel previousChannel;
        synchronized (lifecycleLock) {
            if (requireMatch && !isActive(expectedGeneration)) return false;
            running = false;
            pairing = false;
            generation++;
            previousControl = controlThread;
            previousWriter = writerThread;
            previousWatchdog = watchdogThread;
            previousPairing = pairingThread;
            previousBinding = binding;
            previousChannel = channel;
            controlThread = null;
            writerThread = null;
            watchdogThread = null;
            pairingThread = null;
            binding = null;
            channel = null;
            synchronized (queueLock) {
                resetSessionStateLocked();
                queueLock.notifyAll();
            }
        }
        interrupt(previousControl);
        interrupt(previousWriter);
        interrupt(previousWatchdog);
        interrupt(previousPairing);
        // Stop can originate on Android's UI thread. Send the authenticated
        // boundary on a short-lived cleanup worker, then destroy both directions.
        if (previousChannel != null && previousBinding != null) {
            Thread cleanup = new Thread(() -> {
                try {
                    sendAuthenticatedAbort(previousBinding, previousChannel, 1);
                } finally {
                    previousBinding.close();
                    previousChannel.close();
                }
            }, "UDP5 authenticated stop");
            cleanup.setDaemon(true);
            cleanup.start();
        } else {
            if (previousBinding != null) previousBinding.close();
            if (previousChannel != null) previousChannel.close();
        }
        return true;
    }

    private static void interrupt(Thread thread) {
        if (thread != null && thread != Thread.currentThread()) thread.interrupt();
    }

    private void resetSessionStateLocked() {
        hostReady = false;
        sessionStarted = false;
        hostWindow = DEFAULT_HOST_WINDOW;
        hostLaneCount = 6;
        sessionId = 0;
        nextSequence = 0;
        highestAcknowledged = V5Protocol.NO_ACK;
        lastHeartbeatNanos = 0;
        lastPingNanos = 0;
        nextPingId = 0;
        lastPongId = V5Protocol.NO_ACK;
        lastIdleResponseNanos = 0;
        lastHostSendNanos = 0;
        lastControlReceiveNanos = 0;
        lastAcknowledgementProgressNanos = 0;
        activePathStartedNanos = 0;
        activeContactCount = countActiveContacts(retainedTouching, retainedContactCount);
        queueOverflowed = false;
        clearUnacknowledgedLocked();
    }

    private boolean hasGameplayPendingLocked() {
        Frame newest = unacknowledged.peekLast();
        return newest != null && newest.sequence > 0;
    }

    private boolean hasActiveDataPathLocked() {
        return activeContactCount > 0 || hasGameplayPendingLocked();
    }

    private void updateActivePathStateLocked(boolean wasActive, long nowNanos) {
        boolean active = hasActiveDataPathLocked();
        if (!wasActive && active) activePathStartedNanos = nowNanos;
        else if (!active) activePathStartedNanos = 0;
    }

    private boolean gameplayBacklogExpiredLocked(long nowNanos) {
        Frame oldest = unacknowledged.peekFirst();
        return oldest != null && gameplayBacklogExpired(oldest.queuedNanos, nowNanos);
    }

    static boolean gameplayBacklogExpired(long queuedNanos, long nowNanos) {
        return nowNanos - queuedNanos >= MAX_GAMEPLAY_BACKLOG_NANOS;
    }

    private void clearUnacknowledgedLocked() {
        unacknowledged.clear();
    }

    private void retainContactsLocked(
            int contactCount,
            int[] pointerIds,
            float[] x,
            float[] y,
            float[] pressure,
            float[] touchMajor,
            boolean[] touching
    ) {
        retainedContactCount = contactCount;
        if (contactCount == 0) return;
        System.arraycopy(pointerIds, 0, retainedPointerIds, 0, contactCount);
        System.arraycopy(x, 0, retainedX, 0, contactCount);
        System.arraycopy(y, 0, retainedY, 0, contactCount);
        System.arraycopy(pressure, 0, retainedPressure, 0, contactCount);
        System.arraycopy(touchMajor, 0, retainedTouchMajor, 0, contactCount);
        System.arraycopy(touching, 0, retainedTouching, 0, contactCount);
    }

    private long nextSessionId() {
        long candidate;
        do {
            candidate = random.nextLong();
        } while (candidate == 0);
        return candidate;
    }

    private SignalInfo currentSignal() {
        if (transport != V5Protocol.TransportKind.WIFI || wifiManager == null) {
            return SignalInfo.UNAVAILABLE;
        }
        try {
            WifiInfo info = wifiManager.getConnectionInfo();
            if (info == null) return SignalInfo.UNAVAILABLE;
            int rssi = info.getRssi();
            int frequency = info.getFrequency();
            if (rssi > 0 || rssi <= -127) rssi = Short.MIN_VALUE;
            if (frequency < 0) frequency = 0;
            int level = rssi == Short.MIN_VALUE
                    ? -1
                    : WifiManager.calculateSignalLevel(rssi, 5);
            return new SignalInfo(level, rssi, frequency);
        } catch (RuntimeException ignored) {
            return SignalInfo.UNAVAILABLE;
        }
    }

    private String currentSignalSummary() {
        SignalInfo signal = currentSignal();
        if (signal.level < 0) {
            return transport == V5Protocol.TransportKind.WIFI
                    ? "Wi-Fi signal unavailable; path timing still measured by the host"
                    : "USB selected; Wi-Fi signal measurement does not apply";
        }
        return signal.frequency + " MHz " + wifiBand(signal.frequency)
                + "; " + signal.rssi + " dBm; level " + signal.level + "/4"
                + " (active link; multi-link Wi-Fi may expose one link; "
                + "signal is diagnostic, not an authentication gate)";
    }

    private static String wifiBand(int frequency) {
        if (frequency >= 2_400 && frequency <= 2_500) return "2.4 GHz";
        if (frequency >= 4_900 && frequency <= 5_900) return "5 GHz";
        if (frequency >= 5_925 && frequency <= 7_125) return "6 GHz";
        return "unknown band";
    }

    private static String safePairingMessage(Exception error) {
        String message = error.getMessage();
        return message == null || message.trim().isEmpty()
                ? "Pairing failed. Press Pair for a fresh attempt."
                : message;
    }

    private static int clampFixed(float value) {
        int fixed = Math.round(value * 10_000f);
        return Math.max(Short.MIN_VALUE, Math.min(Short.MAX_VALUE, fixed));
    }

    private static int normalizeUnsigned(float value) {
        float clamped = Math.max(0f, Math.min(1f, value));
        return Math.round(clamped * 65_535f);
    }

    private static int countActiveContacts(boolean[] touching, int contactCount) {
        if (touching == null) return 0;
        int active = 0;
        for (int index = 0; index < contactCount; index++) {
            if (touching[index]) active++;
        }
        return active;
    }

    private static void zeroPairingSecrets(
            byte[] exchangeId,
            byte[] hostPublicKey,
            byte[] handshakeHash,
            byte[] phoneRandom,
            byte[] phoneCommit,
            byte[] hostCommit,
            byte[] expectedHostCommit,
            int[] pattern
    ) {
        zero(exchangeId);
        zero(hostPublicKey);
        zero(handshakeHash);
        zero(phoneRandom);
        zero(phoneCommit);
        zero(hostCommit);
        zero(expectedHostCommit);
        if (pattern != null) Arrays.fill(pattern, 0);
    }

    private static void zero(byte[] bytes) {
        if (bytes != null) Arrays.fill(bytes, (byte) 0);
    }

    private static final class SignalInfo {
        static final SignalInfo UNAVAILABLE = new SignalInfo(-1, Short.MIN_VALUE, 0);
        final int level;
        final int rssi;
        final int frequency;

        SignalInfo(int level, int rssi, int frequency) {
            this.level = level;
            this.rssi = rssi;
            this.frequency = frequency;
        }
    }

    private static final class ComparisonException extends Exception {
        private static final long serialVersionUID = 1L;
    }

    /** Constant-time byte-array comparison without depending on API-level JCA additions. */
    private static final class MessageDigestSafe {
        private MessageDigestSafe() {
        }

        static boolean equals(byte[] first, byte[] second) {
            if (first == null || second == null || first.length != second.length) return false;
            int difference = 0;
            for (int index = 0; index < first.length; index++) {
                difference |= first[index] ^ second[index];
            }
            return difference == 0;
        }
    }
}
