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
import dev.holodori.trackpad.SendQueue.Frame;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/** Authenticated protocol-v6 transport for pairing and lossless touch input. */
final class SecureTransport implements TouchTransport {
    interface PairingListener {
        void onPairingStatus(String message);

        void onPatternReady();

        void onPatternMatched();

        void onQuality(String message);

        void onPairingComplete();

        void onPairingFailed(String message);
    }

    private static final String TAG = "HolodoriSecure";
    private static final String XX_NAME = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
    private static final String IK_NAME = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
    private static final long DISCOVERY_INTERVAL_NANOS = 100_000_000L;
    // A paired phone's IK retry starts fast so an in-game recovery is not
    // held behind a lost first datagram, then backs off to discovery pace.
    private static final long REMEMBERED_FIRST_RETRY_NANOS = 4_000_000L;
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
    private static final int MAX_TOUCH_FRAME = SecureProtocol.TOUCH_FRAME_HEADER_SIZE
            + TouchSample.MAX_CONTACTS * SecureProtocol.CONTACT_SIZE;
    private static final int WRITER_READY_TIMEOUT_MILLIS = 1_000;

    private final Context context;
    private final TouchTransport.Listener listener;
    private final SecureProtocol.TransportKind transport;
    private final CredentialStore credentials;
    private final WifiManager wifiManager;
    private final SecureRandom random = new SecureRandom();
    private final Object lifecycleLock = new Object();
    private final Object queueLock = new Object();
    private final SendQueue unacknowledged = new SendQueue(MAX_HOST_WINDOW, MAX_TOUCH_FRAME);
    private final ArrayBlockingQueue<int[]> submittedPattern = new ArrayBlockingQueue<>(1);
    /** The latest complete snapshot: every queued frame, heartbeat, and reconstruction. */
    private final ContactSnapshot retained = new ContactSnapshot();
    private final PressureFilter pressureFilter;

    private volatile int generation;
    private volatile boolean running;
    private volatile boolean pairing;
    private volatile NetworkBinding binding;
    private volatile SecureProtocol.Channel channel;

    private Thread controlThread;
    private Thread writerThread;
    private Thread watchdogThread;
    private Thread pairingThread;
    private boolean sessionStarted;
    private int hostWindow = DEFAULT_HOST_WINDOW;
    private int hostLaneCount = 6;
    private long sessionId;
    private long nextSequence;
    private long highestAcknowledged = SecureProtocol.NO_ACK;
    private long lastHeartbeatNanos;
    private long lastPingNanos;
    private long nextPingId;
    private long lastPongId = SecureProtocol.NO_ACK;
    private long lastIdleResponseNanos;
    private long lastHostSendNanos;
    private long lastControlReceiveNanos;
    private long lastAcknowledgementProgressNanos;
    private long activePathStartedNanos;
    private int activeContactCount;
    private boolean queueOverflowed;
    private final DiagnosticWorker diagnostics;
    // Unwrapped once per Start: reconnects must not repeat a Keystore unwrap
    // (one of which ran on the UI thread). Destroyed by close(); Forget is
    // only reachable after Stop, and every open() still checks pairing.
    private final Object identityLock = new Object();
    private CredentialStore.Identity rememberedIdentity;
    // Last authenticated host on this Start. IK is unicast there first so a
    // reconnect does not depend on Wi-Fi broadcast delivery (basic rate,
    // no MAC retry, held to DTIM for power-saving receivers).
    private volatile InetSocketAddress lastHost;

    SecureTransport(
            Context context,
            TouchTransport.Listener listener,
            SecureProtocol.TransportKind transport
    ) {
        this.context = context.getApplicationContext() == null
                ? context
                : context.getApplicationContext();
        this.listener = listener;
        this.transport = transport;
        credentials = new CredentialStore(this.context);
        pressureFilter = PressureFilter.load(
                this.context.getSharedPreferences("trackpad", Context.MODE_PRIVATE));
        wifiManager = (WifiManager) this.context.getSystemService(Context.WIFI_SERVICE);
        diagnostics = this.context.getSharedPreferences("trackpad", Context.MODE_PRIVATE)
                .getBoolean("diagnostics", false)
                ? new DiagnosticWorker(this.context, transport, () -> binding) : null;
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
                    "Secure pairing"
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
        closeInternal(0, false);
        try {
            CredentialStore.Identity identity = rememberedIdentity();
            if (identity == null || !identity.hasPairedHost()) {
                listener.onConnectionChanged(false, "Pair this phone before Start");
                return false;
            }
        } catch (CredentialStore.CredentialException error) {
            listener.onConnectionChanged(false, error.getMessage());
            return false;
        }

        CountDownLatch writerReady = new CountDownLatch(1);
        if (diagnostics != null) diagnostics.start();
        synchronized (queueLock) {
            resetSessionStateLocked();
        }
        synchronized (lifecycleLock) {
            running = true;
            int sessionGeneration = ++generation;
            controlThread = new Thread(
                    traced(() -> controlLoop(sessionGeneration)),
                    "Secure handshake and ACKs"
            );
            writerThread = new Thread(
                    traced(() -> writerLoop(sessionGeneration, writerReady)),
                    "Secure touch writer"
            );
            watchdogThread = new Thread(
                    traced(() -> watchdogLoop(sessionGeneration)),
                    "Secure liveness watchdog"
            );
            controlThread.start();
            writerThread.start();
            watchdogThread.start();
        }
        try {
            if (!writerReady.await(WRITER_READY_TIMEOUT_MILLIS, TimeUnit.MILLISECONDS)
                    || !running) {
                closeInternal(0, false);
                listener.onConnectionChanged(false, "Touch writer did not become ready");
                return false;
            }
        } catch (InterruptedException error) {
            Thread.currentThread().interrupt();
            closeInternal(0, false);
            listener.onConnectionChanged(false, "Connection interrupted");
            return false;
        }
        listener.onConnectionChanged(false, "Searching for the paired host over " + transportLabel());
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
            pressureFilter.update(action, actionPointerId, contactCount, pointerIds, pressure, touching);
            retained.copyFrom(contactCount, pointerIds, x, y, pressure, touchMajor, touching);
            activeContactCount = retained.activeCount();
            int frameFlags = (locked ? TouchSample.FRAME_FLAG_LOCKED : 0)
                    | (historical ? TouchSample.FRAME_FLAG_HISTORICAL : 0);
            // Handshake and sequence-zero CANCEL form a hard boundary. Keep
            // only the latest snapshot until the host commits that boundary.
            if (!running || !sessionStarted) {
                if (diagnostics != null) recordLocked(DiagnosticRecorder.NOT_QUEUED, callbackNanos, -1,
                        eventNanos, historical ? 1 : 0, 1, 0, 0, 0, 0, 0);
                return;
            }
            enqueueFrameLocked(action, actionPointerId, frameFlags, eventNanos, callbackNanos, retained);
            updateActivePathStateLocked(wasActiveDataPath, System.nanoTime());
            queueLock.notifyAll();
        }
    }

    /** Queues one frame; returns null, flagging overflow, when the window is full. */
    private Frame enqueueFrameLocked(
            int action,
            int actionPointerId,
            int frameFlags,
            long eventNanos,
            long callbackNanos,
            ContactSnapshot contacts
    ) {
        boolean historical = (frameFlags & TouchSample.FRAME_FLAG_HISTORICAL) != 0;
        if (unacknowledged.size() >= MAX_HOST_WINDOW) {
            // Never evict or coalesce gameplay. The watchdog turns this into
            // a clean authenticated-session failure and the retained snapshot
            // is reconstructed only after a fresh IK + sequence-zero CANCEL.
            queueOverflowed = true;
            if (diagnostics != null) recordLocked(DiagnosticRecorder.NOT_QUEUED, callbackNanos, -1,
                    eventNanos, historical ? 1 : 0, 2, unacknowledged.size(), hostWindow, 0, 0, 0);
            queueLock.notifyAll();
            return null;
        }
        long sequence = nextSequence++;
        int payloadLength = SecureProtocol.TOUCH_FRAME_HEADER_SIZE
                + contacts.count * SecureProtocol.CONTACT_SIZE;
        Frame pending = unacknowledged.add(sequence, payloadLength, System.nanoTime());
        ByteBuffer packet = pending.writer;
        packet.putLong(eventNanos);
        packet.putLong(callbackNanos);
        packet.put((byte) action);
        packet.put((byte) (actionPointerId & 0xFF));
        packet.put((byte) contacts.count);
        packet.put((byte) frameFlags);
        if (diagnostics != null) recordLocked(DiagnosticRecorder.QUEUED,
                pending.queuedNanos, sequence, eventNanos, callbackNanos,
                diagnosticFlags(action, frameFlags), unacknowledged.size(), hostWindow, 0, 0, 0);
        for (int index = 0; index < contacts.count; index++) {
            float localX = contacts.x[index];
            float localY = contacts.y[index];
            boolean touching = contacts.touching[index];
            boolean inside = localX >= 0f && localX <= 1f
                    && localY >= 0f && localY <= 1f;
            int contactFlags = inside ? TouchSample.CONTACT_FLAG_INSIDE : 0;
            if (touching) contactFlags |= TouchSample.CONTACT_FLAG_TIP;
            contactFlags |= pressureFilter.contactFlags(contacts.pointerIds[index], touching);
            packet.put((byte) (contacts.pointerIds[index] & 0xFF));
            packet.put((byte) contactFlags);
            packet.putShort((short) clampFixed(localX));
            packet.putShort((short) clampFixed(localY));
            packet.putShort((short) normalizeUnsigned(contacts.pressure[index]));
            packet.putShort((short) normalizeUnsigned(contacts.touchMajor[index]));
        }
        return pending;
    }

    private void controlLoop(int sessionGeneration) {
        try {
            NetworkBinding sessionBinding = NetworkBinding.open(context, transport);
            if (!installBinding(sessionGeneration, sessionBinding)) return;
            SecureProtocol.HostControl control = new SecureProtocol.HostControl();
            EstablishedChannel established = establishRemembered(
                    sessionGeneration,
                    sessionBinding,
                    control
            );
            if (!installChannel(sessionGeneration, established.channel)) return;
            startSession(sessionGeneration, control, established.receivedNanos);

            SecureProtocol.RecordHeader header = new SecureProtocol.RecordHeader();
            byte[] controlPayload = new byte[SecureProtocol.CONTROL_PAYLOAD_SIZE];
            byte[] receiveBytes = new byte[SecureProtocol.MAX_DATAGRAM_SIZE];
            DatagramPacket incoming = new DatagramPacket(receiveBytes, receiveBytes.length);
            while (isGameplayActive(sessionGeneration)) {
                boolean received = receiveAuthenticated(
                        sessionGeneration,
                        sessionBinding,
                        established.channel,
                        receiveBytes,
                        incoming,
                        header,
                        controlPayload
                );
                if (received) handleHostControl(sessionGeneration, header, controlPayload, control);
                checkHostTimeout(sessionGeneration);
            }
        } catch (Exception error) {
            failGameplay(sessionGeneration, error);
        }
    }

    /** A remembered-peer channel and when its IK response arrived. */
    private static final class EstablishedChannel {
        final SecureProtocol.Channel channel;
        final long receivedNanos;

        EstablishedChannel(SecureProtocol.Channel channel, long receivedNanos) {
            this.channel = channel;
            this.receivedNanos = receivedNanos;
        }
    }

    /**
     * Runs Noise IK with the paired host. IK message 2 carries the host's session
     * parameters, decoded into {@code parameters}, so the session starts at once.
     */
    private EstablishedChannel establishRemembered(
            int sessionGeneration,
            NetworkBinding sessionBinding,
            SecureProtocol.HostControl parameters
    ) throws Exception {
        CredentialStore.Identity identity = rememberedIdentity();
        if (identity == null || !identity.hasPairedHost()) {
            throw new IOException("No paired host identity");
        }
        HandshakeState handshake = null;
        byte[] exchangeId = new byte[16];
        byte[] message = new byte[SecureProtocol.MAX_DATAGRAM_SIZE];
        byte[] payload = new byte[SecureProtocol.MAX_DATAGRAM_SIZE];
        try {
            random.nextBytes(exchangeId);
            handshake = new HandshakeState(IK_NAME, HandshakeState.INITIATOR);
            handshake.getLocalKeyPair().setPrivateKey(identity.privateKey, 0);
            handshake.getRemotePublicKey().setPublicKey(identity.pairedHostPublicKey, 0);
            byte[] prologue = SecureProtocol.prologue(transport, exchangeId);
            handshake.setPrologue(prologue, 0, prologue.length);
            Arrays.fill(prologue, (byte) 0);
            handshake.start();
            int messageLength = handshake.writeMessage(message, 0, null, 0, 0);
            byte[] request = SecureProtocol.encodePairEnvelope(
                    SecureProtocol.IK_MESSAGE_1,
                    exchangeId,
                    1,
                    transport,
                    Arrays.copyOf(message, messageLength)
            );
            long deadline = System.nanoTime() + HANDSHAKE_TIMEOUT_NANOS;
            long nextSend = 0;
            long retryInterval = REMEMBERED_FIRST_RETRY_NANOS;
            InetSocketAddress preferredHost = lastHost;
            DatagramPacket incoming = new DatagramPacket(
                    new byte[SecureProtocol.MAX_DATAGRAM_SIZE],
                    SecureProtocol.MAX_DATAGRAM_SIZE
            );
            while (isGameplayActive(sessionGeneration) && System.nanoTime() < deadline) {
                long now = System.nanoTime();
                if (now >= nextSend) {
                    sessionBinding.sendDiscovery(request, preferredHost);
                    nextSend = now + retryInterval;
                    retryInterval = nextRememberedRetry(retryInterval);
                }
                incoming.setLength(incoming.getData().length);
                try {
                    sessionBinding.socket().receive(incoming);
                } catch (SocketTimeoutException ignored) {
                    continue;
                }
                long receivedNanos = System.nanoTime();
                InetSocketAddress source = (InetSocketAddress) incoming.getSocketAddress();
                SecureProtocol.PairEnvelope envelope;
                try {
                    envelope = SecureProtocol.decodePairEnvelope(
                            incoming.getData(),
                            incoming.getLength()
                    );
                } catch (SecureProtocol.ProtocolException ignored) {
                    continue;
                }
                if (envelope.transport != transport
                        || !Arrays.equals(envelope.exchangeId, exchangeId)
                        || envelope.kind != SecureProtocol.IK_CONTINUE
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
                if (decrypted != SecureProtocol.SESSION_PARAMETERS_SIZE
                        || handshake.getAction() != HandshakeState.SPLIT) {
                    throw new IOException("Malformed Noise IK response");
                }
                SecureProtocol.decodeHostControl(payload, decrypted, MAX_HOST_WINDOW, parameters);
                byte[] handshakeHash = handshake.getHandshakeHash().clone();
                CipherStatePair split = handshake.split();
                lastHost = source;
                try {
                    return new EstablishedChannel(
                            new SecureProtocol.Channel(split, handshakeHash),
                            receivedNanos
                    );
                } finally {
                    Arrays.fill(handshakeHash, (byte) 0);
                }
            }
            throw new IOException("Paired host not found on the selected interface");
        } finally {
            Arrays.fill(exchangeId, (byte) 0);
            Arrays.fill(message, (byte) 0);
            Arrays.fill(payload, (byte) 0);
            if (handshake != null) handshake.destroy();
        }
    }

    /**
     * Opens a fresh gameplay session: sequence zero is the session-start CANCEL,
     * then a reconstruction of any held contacts. The host applies sequences in
     * order, so neither waits for an ACK, and taps meanwhile are sent rather than
     * collapsing into the retained snapshot.
     */
    private void startSession(
            int sessionGeneration,
            SecureProtocol.HostControl parameters,
            long receivedNanos
    ) {
        synchronized (queueLock) {
            if (!isGameplayActive(sessionGeneration)) return;
            hostWindow = parameters.receiveWindow;
            hostLaneCount = parameters.laneCount;
            lastHostSendNanos = parameters.hostSendNanos;
            lastControlReceiveNanos = receivedNanos;
            lastIdleResponseNanos = receivedNanos;
            sessionId = nextSessionId();
            nextSequence = 0;
            highestAcknowledged = SecureProtocol.NO_ACK;
            clearUnacknowledgedLocked();
            long now = System.nanoTime();
            enqueueFrameLocked(TouchSample.ACTION_CANCEL, 0, TouchSample.FRAME_FLAG_SESSION_START,
                    now, now, ContactSnapshot.NONE);
            sessionStarted = true;
            if (activeContactCount > 0) {
                activePathStartedNanos = now;
                lastHeartbeatNanos = now;
                enqueueRetainedSnapshotLocked(now, false);
            }
            queueLock.notifyAll();
        }
        listener.onHostLaneCountChanged(hostLaneCount);
        listener.onConnectionChanged(true, "Authenticated " + transportLabel() + " connected");
    }

    private boolean receiveAuthenticated(
            int sessionGeneration,
            NetworkBinding sessionBinding,
            SecureProtocol.Channel sessionChannel,
            byte[] bytes,
            DatagramPacket incoming,
            SecureProtocol.RecordHeader header,
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
        } catch (SecureProtocol.AuthenticationException ignored) {
            if (diagnostics != null) diagnostics.controlDrops.incrementAndGet(0);
            return false;
        } catch (SecureProtocol.ReplayException ignored) {
            if (diagnostics != null) diagnostics.controlDrops.incrementAndGet(1);
            return false;
        } catch (SecureProtocol.ProtocolException ignored) {
            if (diagnostics != null) diagnostics.controlDrops.incrementAndGet(2);
            // Header corruption is unauthenticated until AEAD succeeds. Drop it
            // silently and never let hostile traffic sustain liveness.
            return false;
        }
    }

    private void handleHostControl(
            int sessionGeneration,
            SecureProtocol.RecordHeader header,
            byte[] payload,
            SecureProtocol.HostControl control
    ) throws IOException {
        if (header.messageType == SecureProtocol.HOST_AUTH_ABORT) {
            // The host already released input; restart now rather than after
            // the 64 ms active or 2 s idle watchdog.
            throw new IOException("Host ended the authenticated session");
        }
        if (header.flags != 0 || header.payloadLength != SecureProtocol.CONTROL_PAYLOAD_SIZE) {
            throw new IOException("Malformed authenticated host control");
        }
        try {
            SecureProtocol.decodeHostControl(payload, header.payloadLength, MAX_HOST_WINDOW, control);
        } catch (SecureProtocol.ProtocolException error) {
            throw new IOException("Invalid authenticated host control values", error);
        }
        synchronized (queueLock) {
            if (!isGameplayActive(sessionGeneration)) return;
            if (header.sessionId != sessionId) {
                throw new IOException("Host control belongs to the wrong session");
            }
            long receiveNanos = header.receivedNanos;
            if (header.messageType == SecureProtocol.HOST_ACK) {
                if (acknowledgeLocked(header.logicalId, receiveNanos, control.hostSendNanos)) {
                    lastAcknowledgementProgressNanos = receiveNanos;
                    lastIdleResponseNanos = receiveNanos;
                }
                markHeldLocked(header.logicalId, control.selectiveAck);
            } else if (header.messageType == SecureProtocol.HOST_PONG) {
                if (control.selectiveAck != 0
                        || Long.compareUnsigned(header.logicalId, nextPingId) >= 0) {
                    throw new IOException("Invalid authenticated PONG");
                }
                if (lastPongId == SecureProtocol.NO_ACK
                        || Long.compareUnsigned(header.logicalId, lastPongId) > 0) {
                    lastPongId = header.logicalId;
                    lastIdleResponseNanos = receiveNanos;
                }
            } else {
                throw new IOException("Unexpected authenticated host message");
            }
            lastHostSendNanos = control.hostSendNanos;
            lastControlReceiveNanos = receiveNanos;
            queueLock.notifyAll();
        }
    }

    /** Selective ACK: stop repairing frames the host already holds behind a hole. */
    private void markHeldLocked(long cumulative, long selectiveAck) {
        // Bit i reports frame cumulative + 2 + i; NO_ACK (-1) makes that 1 + i.
        for (long remaining = selectiveAck; remaining != 0; remaining &= remaining - 1) {
            long sequence = cumulative + 2 + Long.numberOfTrailingZeros(remaining);
            if (Long.compareUnsigned(sequence, nextSequence) < 0) unacknowledged.markHeld(sequence);
        }
    }

    private void writerLoop(int sessionGeneration, CountDownLatch writerReady) {
        Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_DISPLAY);
        writerReady.countDown();
        byte[] recordPayload = new byte[SecureProtocol.MAX_DATAGRAM_SIZE];
        ByteBuffer record = ByteBuffer.wrap(recordPayload).order(ByteOrder.LITTLE_ENDIAN);
        byte[] sendBuffer = new byte[SecureProtocol.MAX_DATAGRAM_SIZE];
        DatagramPacket outgoing = new DatagramPacket(sendBuffer, 0);
        BatchDiagnostics batch = new BatchDiagnostics();
        boolean recordWriter = diagnostics != null && diagnostics.recorder.writer.claimed.compareAndSet(false, true);
        if (diagnostics != null && !recordWriter) diagnostics.recorder.writerCoverageUnavailable++;
        try {
            while (isGameplayActive(sessionGeneration)) {
                NetworkBinding sessionBinding;
                SecureProtocol.Channel sessionChannel;
                int payloadLength;
                int messageType;
                long frameSession;
                long logicalId;
                synchronized (queueLock) {
                    if (!isGameplayActive(sessionGeneration)) break;
                    long now = System.nanoTime();
                    maybeEnqueueHeartbeatLocked(now);
                    sessionBinding = binding;
                    sessionChannel = channel;
                    if (!sessionStarted || sessionBinding == null || sessionChannel == null) {
                        queueLock.wait();
                        continue;
                    }
                    frameSession = sessionId;
                    Frame first = unacknowledged.nextBatch(now, hostWindow,
                            SecureProtocol.MAX_FRAMES_PER_RECORD, SecureProtocol.MAX_TOUCH_FRAME_BYTES);
                    if (first != null) {
                        payloadLength = writeRecordLocked(record, batch, now);
                        logicalId = first.sequence;
                        messageType = SecureProtocol.PHONE_TOUCH;
                    } else if (isPingDueLocked(now)) {
                        payloadLength = 0;
                        logicalId = nextPingId++;
                        lastPingNanos = now;
                        messageType = SecureProtocol.PHONE_PING;
                    } else {
                        long delay = writerDelayLocked(now);
                        if (delay == Long.MAX_VALUE) queueLock.wait();
                        else if (delay > 0) TimeUnit.NANOSECONDS.timedWait(queueLock, delay);
                        continue;
                    }
                }
                boolean touch = messageType == SecureProtocol.PHONE_TOUCH;
                // An idle ping is the only record still sent as a redundant pair here;
                // touch twins are separate selections so a repair can slot between pairs.
                for (int copy = 0; copy < (touch ? 1 : INITIAL_SEND_COPIES); copy++) {
                    long sendStarted = System.nanoTime();
                    if (touch) record.putLong(0, sendStarted);
                    boolean sendFailed = true;
                    try {
                        int sendLength = sessionChannel.sealInto(messageType, frameSession, logicalId, 0,
                                recordPayload, payloadLength, sendBuffer);
                        outgoing.setData(sendBuffer, 0, sendLength);
                        sessionBinding.sendToPeer(outgoing);
                        sendFailed = false;
                    } finally {
                        if (recordWriter && touch) {
                            batch.record(diagnostics.recorder.writer, frameSession, sendStarted, sendFailed);
                        }
                    }
                }
            }
        } catch (InterruptedException ignored) {
            Thread.currentThread().interrupt();
        } catch (Exception error) {
            failGameplay(sessionGeneration, error);
        } finally {
            if (recordWriter) diagnostics.recorder.writer.claimed.set(false);
        }
    }

    /**
     * Copies the selected batch into one touch record: the per-datagram prefix,
     * then each frame. Returns the payload length; the send time is set per attempt.
     */
    private int writeRecordLocked(ByteBuffer record, BatchDiagnostics batch, long selectedAt) {
        int count = unacknowledged.batchSize();
        record.clear();
        record.putLong(0L);
        record.putLong(lastHostSendNanos);
        record.putLong(lastControlReceiveNanos);
        record.put((byte) count);
        while (record.position() < SecureProtocol.TOUCH_RECORD_PREFIX_SIZE) record.put((byte) 0);
        batch.count = count;
        batch.selectedAt = selectedAt;
        for (int index = 0; index < count; index++) {
            Frame frame = unacknowledged.batchFrame(index);
            record.put(frame.payload, 0, frame.payloadLength);
            batch.capture(index, frame);
        }
        return record.position();
    }

    private boolean isPingDueLocked(long now) {
        return !hasActiveDataPathLocked()
                // The host owns the session only once CANCEL commits.
                && highestAcknowledged != SecureProtocol.NO_ACK
                && now - lastPingNanos >= IDLE_PING_NANOS;
    }

    /** How long the writer may sleep before a repair, heartbeat, or ping is due. */
    private long writerDelayLocked(long now) {
        long delay = unacknowledged.nanosUntilSend(now, hostWindow);
        if (unacknowledged.isEmpty()) {
            delay = Math.min(delay, activeContactCount > 0
                    ? ACTIVE_HEARTBEAT_NANOS - (now - lastHeartbeatNanos)
                    : IDLE_PING_NANOS - (now - lastPingNanos));
        }
        return delay;
    }

    /** Per-frame writer diagnostics for one batch, captured under the queue lock. */
    private static final class BatchDiagnostics {
        final long[] sequences = new long[SecureProtocol.MAX_FRAMES_PER_RECORD];
        final long[] queuedNanos = new long[SecureProtocol.MAX_FRAMES_PER_RECORD];
        final long[] ordinals = new long[SecureProtocol.MAX_FRAMES_PER_RECORD];
        final long[] repairLateness = new long[SecureProtocol.MAX_FRAMES_PER_RECORD];
        final int[] frameFlags = new int[SecureProtocol.MAX_FRAMES_PER_RECORD];
        int count;
        long selectedAt;

        void capture(int index, Frame frame) {
            sequences[index] = frame.sequence;
            queuedNanos[index] = frame.queuedNanos;
            ordinals[index] = frame.sendAttempts;
            repairLateness[index] = frame.repairLatenessNanos;
            frameFlags[index] = Byte.toUnsignedInt(frame.payload[SecureProtocol.FRAME_FLAGS_OFFSET]);
        }

        void record(DiagnosticRecorder.Ring writer, long session, long sendStarted, boolean failed) {
            long returned = System.nanoTime();
            for (int index = 0; index < count; index++) {
                writer.offer(DiagnosticRecorder.ATTEMPT, returned, session, sequences[index],
                        sendStarted, queuedNanos[index], ordinals[index], failed ? 1 : 0,
                        returned, repairLateness[index], selectedAt, frameFlags[index]);
            }
        }
    }

    private void maybeEnqueueHeartbeatLocked(long nowNanos) {
        if (!sessionStarted || activeContactCount == 0
                || !unacknowledged.isEmpty()
                || nowNanos - lastHeartbeatNanos < ACTIVE_HEARTBEAT_NANOS) {
            return;
        }
        lastHeartbeatNanos = nowNanos;
        // A heartbeat restates held contacts: one copy suffices, and a lost
        // one is repaired at 2 ms, far inside the host's 32 ms watchdog.
        enqueueRetainedSnapshotLocked(nowNanos, true);
    }

    /** Full-state frame from the latest snapshot: heartbeat and fresh-session reconstruction. */
    private void enqueueRetainedSnapshotLocked(long nowNanos, boolean singleCopy) {
        Frame frame = enqueueFrameLocked(TouchSample.ACTION_HEARTBEAT, 0,
                TouchSample.FRAME_FLAG_LOCKED, nowNanos, nowNanos, retained);
        if (frame != null) frame.singleCopy = singleCopy;
    }

    private boolean acknowledgeLocked(long acknowledgedSequence, long receivedNanos, long ackHostSendNanos) {
        boolean future = acknowledgedSequence != SecureProtocol.NO_ACK
                && Long.compareUnsigned(acknowledgedSequence, nextSequence) >= 0;
        if (acknowledgedSequence == SecureProtocol.NO_ACK
                || (highestAcknowledged != SecureProtocol.NO_ACK
                && Long.compareUnsigned(acknowledgedSequence, highestAcknowledged) <= 0)
                || Long.compareUnsigned(acknowledgedSequence, nextSequence) >= 0) {
            diagnosticAckLocked(acknowledgedSequence, receivedNanos, future ? 2 : 0, ackHostSendNanos);
            return false;
        }
        diagnosticAckLocked(acknowledgedSequence, receivedNanos, 1, ackHostSendNanos);
        highestAcknowledged = acknowledgedSequence;
        while (!unacknowledged.isEmpty()) {
            Frame first = unacknowledged.peekFirst();
            if (Long.compareUnsigned(first.sequence, acknowledgedSequence) > 0) break;
            if (diagnostics != null) {
                recordFrameLocked(DiagnosticRecorder.RETIRED, receivedNanos, first, acknowledgedSequence);
            }
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
            if (!isGameplayActive(sessionGeneration) || !sessionStarted) return;
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
                if (diagnostics != null) recordLocked(DiagnosticRecorder.WATCHDOG,
                        now, highestAcknowledged, overflowed ? 3 : backlogExpired ? 2 : active ? 1 : 4,
                        progress, staleFrames, unacknowledged.peekFirst() == null ? 0 : unacknowledged.peekFirst().queuedNanos,
                        hostWindow, 0, 0, 0);
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
                    NetworkBinding current = binding;
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
            callback.onPairingStatus("Opening selected " + transportLabel() + " interface");
            NetworkBinding pairBinding = NetworkBinding.open(context, transport);
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
                Log.e(TAG, "Secure pairing failed", error);
                callback.onPairingFailed(safePairingMessage(error));
            }
        } finally {
            if (identity != null) identity.destroy();
        }
    }

    private void runInitialPairing(
            int pairGeneration,
            NetworkBinding pairBinding,
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
        byte[] probe = SecureProtocol.encodePairEnvelope(
                SecureProtocol.PAIR_PROBE,
                exchangeId,
                0,
                transport,
                new byte[0]
        );
        callback.onPairingStatus("Waiting for the host Pair window");
        SecureProtocol.PairEnvelope offer = waitForPairEnvelope(
                pairGeneration,
                pairBinding,
                pairingDeadline,
                pairingDeadline,
                exchangeId,
                SecureProtocol.PAIR_OFFER,
                1,
                probe,
                true
        );

        HandshakeState handshake = new HandshakeState(XX_NAME, HandshakeState.RESPONDER);
        byte[] handshakeBuffer = new byte[SecureProtocol.MAX_DATAGRAM_SIZE];
        byte[] payloadBuffer = new byte[SecureProtocol.MAX_DATAGRAM_SIZE];
        SecureProtocol.Channel pairChannel;
        try {
            handshake.getLocalKeyPair().setPrivateKey(identity.privateKey, 0);
            byte[] prologue = SecureProtocol.prologue(transport, exchangeId);
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
            byte[] messageTwo = SecureProtocol.encodePairEnvelope(
                    SecureProtocol.PAIR_CONTINUE,
                    exchangeId,
                    2,
                    transport,
                    Arrays.copyOf(handshakeBuffer, messageTwoLength)
            );
            sendPlainRedundant(pairBinding, messageTwo);
            SecureProtocol.PairEnvelope messageThree = waitForPairEnvelope(
                    pairGeneration,
                    pairBinding,
                    pairingDeadline,
                    System.nanoTime() + HANDSHAKE_TIMEOUT_NANOS,
                    exchangeId,
                    SecureProtocol.PAIR_CONTINUE,
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
            pairChannel = new SecureProtocol.Channel(handshake.split(), handshakeHash);
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
        phoneCommit = SecureProtocol.sasCommit(1, handshakeHash, phoneRandom);
        sendRecordRedundant(
                pairBinding,
                pairChannel,
                SecureProtocol.PHONE_SAS_COMMITMENT,
                0,
                0,
                phoneCommit
        );
        SecureProtocol.OpenedRecord hostCommitRecord = waitForPairRecord(
                pairGeneration,
                pairBinding,
                pairChannel,
                pairingDeadline,
                SecureProtocol.HOST_SAS_COMMITMENT,
                SecureProtocol.PHONE_SAS_COMMITMENT,
                phoneCommit
        );
        requirePairRecord(hostCommitRecord, 32);
        hostCommit = hostCommitRecord.payload.clone();
        sendRecordRedundant(
                pairBinding,
                pairChannel,
                SecureProtocol.PHONE_SAS_REVEAL,
                0,
                0,
                phoneRandom
        );
        SecureProtocol.OpenedRecord hostRevealRecord = waitForPairRecord(
                pairGeneration,
                pairBinding,
                pairChannel,
                pairingDeadline,
                SecureProtocol.HOST_SAS_REVEAL,
                SecureProtocol.PHONE_SAS_REVEAL,
                phoneRandom
        );
        requirePairRecord(hostRevealRecord, 32);
        expectedHostCommit = SecureProtocol.sasCommit(
                2,
                handshakeHash,
                hostRevealRecord.payload
        );
        if (!MessageDigestSafe.equals(hostCommit, expectedHostCommit)) {
            sendAuthenticatedAbort(pairBinding, pairChannel, 2);
            throw new ComparisonException();
        }
        expectedPattern = SecureProtocol.sasPattern(SecureProtocol.sasDigest(
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
        SendQueue repairs = new SendQueue(MAX_HOST_WINDOW, 32);
        HashSet<Long> qualitySeen = new HashSet<>();
        byte[] incomingBytes = new byte[SecureProtocol.MAX_DATAGRAM_SIZE];
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
                        SecureProtocol.PHONE_PAIR_CONFIRM,
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
                        SecureProtocol.PHONE_PAIR_CONFIRM,
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
            SecureProtocol.OpenedRecord record;
            try {
                record = pairChannel.open(incomingBytes, incoming.getLength());
            } catch (SecureProtocol.AuthenticationException
                     | SecureProtocol.ReplayException ignored) {
                continue;
            } catch (SecureProtocol.ProtocolException ignored) {
                continue;
            }
            if (record.header.sessionId != 0) {
                throw new IOException("Pairing record used a gameplay session");
            }
            if (record.header.messageType == SecureProtocol.HOST_QUALITY_PROBE) {
                if (transport != SecureProtocol.TransportKind.WIFI
                        || record.payload.length != 8
                        || (record.header.flags & ~SecureProtocol.QUALITY_REPAIR_ONLY) != 0) {
                    throw new IOException("Malformed authenticated quality probe");
                }
                long received = System.nanoTime();
                if (!qualitySeen.add(record.header.logicalId)) continue;
                long hostSend = ByteBuffer.wrap(record.payload)
                        .order(ByteOrder.LITTLE_ENDIAN)
                        .getLong();
                boolean repairOnly = (record.header.flags & SecureProtocol.QUALITY_REPAIR_ONLY) != 0;
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
            } else if (record.header.messageType == SecureProtocol.HOST_PAIR_COMPLETE) {
                requirePairRecord(record, 0);
                if (!patternConfirmed) {
                    throw new IOException("Host completed pairing before phone confirmation");
                }
                credentials.savePairedHost(hostPublicKey);
                callback.onQuality(currentSignalSummary());
                return;
            } else if (record.header.messageType == SecureProtocol.HOST_AUTH_ABORT) {
                throw new IOException("Host cancelled pairing");
            } else if (record.header.messageType == SecureProtocol.HOST_SAS_REVEAL) {
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

    private SecureProtocol.PairEnvelope waitForPairEnvelope(
            int pairGeneration,
            NetworkBinding pairBinding,
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
        byte[] bytes = new byte[SecureProtocol.MAX_DATAGRAM_SIZE];
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
            SecureProtocol.PairEnvelope envelope;
            try {
                envelope = SecureProtocol.decodePairEnvelope(bytes, incoming.getLength());
            } catch (SecureProtocol.ProtocolException ignored) {
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
            if (envelope.kind == SecureProtocol.PAIR_ABORT) {
                throw new IOException("Host cancelled pairing");
            }
            if (envelope.kind == expectedKind && envelope.step == expectedStep) {
                return envelope;
            }
        }
        throw new IOException("Noise pairing handshake timed out");
    }

    private SecureProtocol.OpenedRecord waitForPairRecord(
            int pairGeneration,
            NetworkBinding pairBinding,
            SecureProtocol.Channel pairChannel,
            long deadline,
            int expectedType,
            int retryType,
            byte[] retryPayload
    ) throws Exception {
        long nextRetry = System.nanoTime() + APPLICATION_RETRY_NANOS;
        long lastInterfaceCheck = System.nanoTime();
        byte[] bytes = new byte[SecureProtocol.MAX_DATAGRAM_SIZE];
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
            SecureProtocol.OpenedRecord record;
            try {
                record = pairChannel.open(bytes, incoming.getLength());
            } catch (SecureProtocol.AuthenticationException
                     | SecureProtocol.ReplayException ignored) {
                continue;
            } catch (SecureProtocol.ProtocolException ignored) {
                continue;
            }
            if (record.header.messageType == SecureProtocol.HOST_AUTH_ABORT) {
                throw new IOException("Host cancelled pairing");
            }
            if (record.header.messageType == expectedType) return record;
        }
        throw new IOException("Authenticated pairing step timed out");
    }

    private static void requireCurrentPairingBinding(NetworkBinding pairBinding)
            throws IOException {
        if (!pairBinding.revalidate()) {
            throw new IOException("Selected network interface changed during pairing");
        }
    }

    private void sendQualityReply(
            NetworkBinding pairBinding,
            SecureProtocol.Channel pairChannel,
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
                    SecureProtocol.PHONE_QUALITY_REPLY,
                    0,
                    quality.sequence,
                    0,
                    payload
            );
            pairBinding.sendToPeer(encrypted);
        }
        Arrays.fill(payload, (byte) 0);
    }

    private static void requirePairRecord(SecureProtocol.OpenedRecord record, int payloadLength)
            throws IOException {
        if (record.header.sessionId != 0
                || record.header.logicalId != 0
                || record.header.flags != 0
                || record.payload.length != payloadLength) {
            throw new IOException("Malformed authenticated pairing control");
        }
    }

    private void sendPlainRedundant(NetworkBinding pairBinding, byte[] bytes)
            throws IOException {
        pairBinding.sendToPeer(bytes);
        pairBinding.sendToPeer(bytes);
    }

    private void sendRecordRedundant(
            NetworkBinding pairBinding,
            SecureProtocol.Channel pairChannel,
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
            NetworkBinding pairBinding,
            SecureProtocol.Channel pairChannel,
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
                    SecureProtocol.PHONE_AUTH_ABORT,
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
        NetworkBinding currentBinding = binding;
        SecureProtocol.Channel currentChannel = channel;
        if (currentBinding != null && currentChannel != null) {
            sendAuthenticatedAbort(currentBinding, currentChannel, reason);
        }
    }

    private boolean installBinding(int expectedGeneration, NetworkBinding next) {
        synchronized (lifecycleLock) {
            if (!isActive(expectedGeneration)) {
                next.close();
                return false;
            }
            binding = next;
            return true;
        }
    }

    private boolean installChannel(int expectedGeneration, SecureProtocol.Channel next) {
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
        if (!closeInternal(sessionGeneration, true, error)) return;
        listener.onConnectionChanged(false, "Authenticated "
                + (transport == SecureProtocol.TransportKind.WIFI ? "Wi-Fi" : "USB")
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
        forgetRememberedIdentity();
        if (diagnostics != null) diagnostics.stop();
    }

    private CredentialStore.Identity rememberedIdentity()
            throws CredentialStore.CredentialException {
        synchronized (identityLock) {
            if (rememberedIdentity == null) rememberedIdentity = credentials.load();
            return rememberedIdentity;
        }
    }

    private void forgetRememberedIdentity() {
        synchronized (identityLock) {
            if (rememberedIdentity != null) rememberedIdentity.destroy();
            rememberedIdentity = null;
        }
        lastHost = null;
    }

    private String transportLabel() {
        return transport == SecureProtocol.TransportKind.WIFI ? "Wi-Fi / local network" : "USB tethering";
    }

    static long nextRememberedRetry(long currentNanos) {
        return Math.min(DISCOVERY_INTERVAL_NANOS, currentNanos * 2);
    }

    private boolean closeInternal(int expectedGeneration, boolean requireMatch) {
        return closeInternal(expectedGeneration, requireMatch, null);
    }

    private boolean closeInternal(int expectedGeneration, boolean requireMatch, Exception failure) {
        Thread previousControl;
        Thread previousWriter;
        Thread previousWatchdog;
        Thread previousPairing;
        NetworkBinding previousBinding;
        SecureProtocol.Channel previousChannel;
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
                if (diagnostics != null && (previousWriter != null || previousControl != null)) {
                    recordLocked(DiagnosticRecorder.BOUNDARY, System.nanoTime(),
                            highestAcknowledged, requireMatch ? 1 : 0, unacknowledged.size(),
                            failure == null ? 0 : failure instanceof SocketTimeoutException ? 1
                                    : failure instanceof IOException ? 2 : 3, 0, 0, 0, 0, 0);
                }
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
            }, "Secure stop");
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
        clearUnacknowledgedLocked();
        sessionStarted = false;
        hostWindow = DEFAULT_HOST_WINDOW;
        hostLaneCount = 6;
        sessionId = 0;
        nextSequence = 0;
        highestAcknowledged = SecureProtocol.NO_ACK;
        lastHeartbeatNanos = 0;
        lastPingNanos = 0;
        nextPingId = 0;
        lastPongId = SecureProtocol.NO_ACK;
        lastIdleResponseNanos = 0;
        lastHostSendNanos = 0;
        lastControlReceiveNanos = 0;
        lastAcknowledgementProgressNanos = 0;
        activePathStartedNanos = 0;
        activeContactCount = retained.activeCount();
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
        if (diagnostics != null) {
            long now = System.nanoTime();
            for (; !unacknowledged.isEmpty(); unacknowledged.removeFirst()) {
                recordFrameLocked(DiagnosticRecorder.DISCARDED, now,
                        unacknowledged.peekFirst(), highestAcknowledged);
            }
        }
        unacknowledged.clear();
    }

    private static long diagnosticFlags(int action, int frameFlags) {
        boolean start = (frameFlags & TouchSample.FRAME_FLAG_SESSION_START) != 0;
        boolean historical = (frameFlags & TouchSample.FRAME_FLAG_HISTORICAL) != 0;
        return (start ? 8 : action == TouchSample.ACTION_HEARTBEAT ? 4 : 1) | (historical ? 2 : 0);
    }

    private static long diagnosticFlags(Frame frame) {
        return diagnosticFlags(Byte.toUnsignedInt(frame.payload[SecureProtocol.FRAME_ACTION_OFFSET]),
                Byte.toUnsignedInt(frame.payload[SecureProtocol.FRAME_FLAGS_OFFSET]));
    }

    private void diagnosticAckLocked(long sequence, long now, int progress, long hostSend) {
        if (diagnostics == null) return;
        Frame oldest = unacknowledged.peekFirst();
        recordLocked(DiagnosticRecorder.ACK, now, sequence, progress,
                Math.max(lastAcknowledgementProgressNanos, activePathStartedNanos), unacknowledged.size(), oldest == null ? 0 : oldest.queuedNanos,
                hostWindow, hostSend, 0, 0);
    }

    private Runnable traced(Runnable work) {
        return diagnostics == null ? work : diagnostics.producer(work);
    }

    /** Queue-domain record; callers hold queueLock and have checked diagnostics. */
    private void recordLocked(int kind, long at, long sequence, long a, long b, long c,
                              long d, long e, long f, long g, long h) {
        diagnostics.recorder.queue.offer(kind, at, sessionId, sequence, a, b, c, d, e, f, g, h);
    }

    private void recordFrameLocked(int kind, long at, Frame frame, long acknowledged) {
        recordLocked(kind, at, frame.sequence, frame.queuedNanos, frame.writer.getLong(0),
                frame.writer.getLong(8), diagnosticFlags(frame), frame.firstSelectedNanos,
                frame.sendAttempts, unacknowledged.size(), acknowledged);
    }

    private long nextSessionId() {
        long candidate;
        do {
            candidate = random.nextLong();
        } while (candidate == 0);
        return candidate;
    }

    private SignalInfo currentSignal() {
        if (transport != SecureProtocol.TransportKind.WIFI || wifiManager == null) {
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
            return transport == SecureProtocol.TransportKind.WIFI
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

    /** One complete simultaneous contact snapshot, copied without allocating. */
    private static final class ContactSnapshot {
        static final ContactSnapshot NONE = new ContactSnapshot();

        final int[] pointerIds = new int[TouchSample.MAX_CONTACTS];
        final float[] x = new float[TouchSample.MAX_CONTACTS];
        final float[] y = new float[TouchSample.MAX_CONTACTS];
        final float[] pressure = new float[TouchSample.MAX_CONTACTS];
        final float[] touchMajor = new float[TouchSample.MAX_CONTACTS];
        final boolean[] touching = new boolean[TouchSample.MAX_CONTACTS];
        int count;

        void copyFrom(int contactCount, int[] pointerIds, float[] x, float[] y,
                      float[] pressure, float[] touchMajor, boolean[] touching) {
            count = contactCount;
            if (contactCount == 0) return;
            System.arraycopy(pointerIds, 0, this.pointerIds, 0, contactCount);
            System.arraycopy(x, 0, this.x, 0, contactCount);
            System.arraycopy(y, 0, this.y, 0, contactCount);
            System.arraycopy(pressure, 0, this.pressure, 0, contactCount);
            System.arraycopy(touchMajor, 0, this.touchMajor, 0, contactCount);
            System.arraycopy(touching, 0, this.touching, 0, contactCount);
        }

        int activeCount() {
            int active = 0;
            for (int index = 0; index < count; index++) {
                if (touching[index]) active++;
            }
            return active;
        }
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
