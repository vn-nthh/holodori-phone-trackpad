package dev.holodori.trackpad;

import android.os.Process;
import android.util.Log;

import java.io.IOException;
import java.net.DatagramPacket;
import java.net.DatagramSocket;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.NetworkInterface;
import java.net.SocketException;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.security.SecureRandom;
import java.util.ArrayDeque;
import java.util.Enumeration;
import java.util.HashSet;
import java.util.Iterator;
import java.util.Locale;
import java.util.Set;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.zip.CRC32;

/**
 * Reliable protocol-v4 transport over Android USB tethering/RNDIS.
 *
 * <p>Each HPT4 touch frame is one UDP datagram. The existing cumulative HPA4
 * acknowledgement protocol is retained, so a lost datagram is retransmitted
 * and never silently dropped. USB tethering supplies the network; this class
 * never opens the Android USB API or asks for a device driver.</p>
 */
final class UdpTransport implements TouchTransport {
    private static final String TAG = "HolodoriUDP4";
    private static final int DISCOVERY_PORT = 42_825;
    private static final int DISCOVERY_VERSION = 1;
    private static final int DISCOVERY_HELLO = 1;
    private static final int DISCOVERY_ACK = 2;
    private static final int DISCOVERY_SIZE = 32;
    private static final long DISCOVERY_INTERVAL_NANOS = 500_000_000L;
    private static final long HOST_TIMEOUT_NANOS = 2_000_000_000L;
    // Windows touch injection needs a high-rate refresh while a contact is
    // stationary. Idle sessions do not need synthetic touch frames; discovery
    // acknowledgements keep the host liveness check alive without creating a
    // 125 Hz allocation and network workload for the whole session.
    private static final long ACTIVE_HEARTBEAT_NANOS = 8_000_000L;
    private static final long RETRANSMIT_NANOS = 4_000_000L;
    private static final long WRITER_READY_TIMEOUT_MILLIS = 1_000;
    private static final int DEFAULT_HOST_WINDOW = 64;
    private static final int MAX_HOST_WINDOW = 256;
    private static final int PHONE_SEND_NANOS_OFFSET = 40;
    private static final int ECHO_HOST_SEND_NANOS_OFFSET = 48;
    private static final int PHONE_CONTROL_RECEIVE_NANOS_OFFSET = 56;

    private static final class PendingPacket {
        final long sessionId;
        final long sequence;
        final byte[] bytes;
        long lastSentNanos;
        int sendCount;

        PendingPacket(long sessionId, long sequence, byte[] bytes) {
            this.sessionId = sessionId;
            this.sequence = sequence;
            this.bytes = bytes;
        }
    }

    private final Object queueLock = new Object();
    private final Object lifecycleLock = new Object();
    private final ArrayDeque<PendingPacket> unacknowledged = new ArrayDeque<>();
    private final TouchTransport.Listener listener;
    private final SecureRandom random = new SecureRandom();
    private final CRC32 writerCrc = new CRC32();
    // The control loop is single-threaded, so this state can be reused for
    // discovery and ACK validation instead of allocating per datagram.
    private final CRC32 controlCrc = new CRC32();
    private final byte[] controlBuffer = new byte[2_048];
    private final ByteBuffer controlPacket =
            ByteBuffer.wrap(controlBuffer).order(ByteOrder.LITTLE_ENDIAN);

    private volatile int generation;
    private volatile boolean running;
    private volatile InetSocketAddress hostAddress;
    private boolean hostReady;
    private int hostWindow = DEFAULT_HOST_WINDOW;
    private int hostLaneCount = 6;
    private long sessionId;
    private long discoveryNonce;
    private long nextSequence;
    private long highestAcknowledged = -1;
    private long lastHeartbeatNanos;
    private long lastHostSendNanos;
    private long lastControlReceiveNanos;
    private long sessionStartedNanos;
    private int activeContactCount;

    private DatagramSocket socket;
    private Thread controlThread;
    private Thread writerThread;

    UdpTransport(TouchTransport.Listener listener) {
        this.listener = listener;
    }

    @Override
    public boolean open() {
        close();
        DatagramSocket nextSocket;
        try {
            nextSocket = new DatagramSocket(0);
            nextSocket.setBroadcast(true);
            nextSocket.setSoTimeout(100);
        } catch (SocketException error) {
            listener.onConnectionChanged(false, "Could not open USB-tethering network");
            return false;
        }

        CountDownLatch writerReady = new CountDownLatch(1);
        synchronized (queueLock) {
            beginSessionLocked();
        }

        synchronized (lifecycleLock) {
            socket = nextSocket;
            running = true;
            int sessionGeneration = ++generation;
            controlThread = new Thread(
                    () -> controlLoop(sessionGeneration, nextSocket),
                    "UDP4 discovery and acknowledgements"
            );
            writerThread = new Thread(
                    () -> writerLoop(sessionGeneration, nextSocket, writerReady),
                    "UDP4 touch writer"
            );
            controlThread.start();
            writerThread.start();
        }

        try {
            if (!writerReady.await(
                    WRITER_READY_TIMEOUT_MILLIS,
                    TimeUnit.MILLISECONDS
            ) || !running) {
                close();
                listener.onConnectionChanged(false, "UDP writer did not become ready");
                return false;
            }
        } catch (InterruptedException error) {
            Thread.currentThread().interrupt();
            close();
            listener.onConnectionChanged(false, "USB-tethering connection interrupted");
            return false;
        }

        listener.onConnectionChanged(
                false,
                "USB tethering active, searching for host"
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
        if (!running) return;
        if (contactCount < 0 || contactCount > TouchSample.MAX_CONTACTS) {
            throw new IllegalArgumentException("Unsupported contact count: " + contactCount);
        }
        synchronized (queueLock) {
            if (!running) return;
            activeContactCount = countActiveContacts(touching, contactCount);
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
        long sequence = nextSequence++;
        int length = TouchSample.FRAME_HEADER_SIZE
                + contactCount * TouchSample.CONTACT_SIZE
                + TouchSample.CRC_SIZE;
        byte[] bytes = new byte[length];
        ByteBuffer packet = ByteBuffer.wrap(bytes).order(ByteOrder.LITTLE_ENDIAN);
        packet.put((byte) 'H');
        packet.put((byte) 'P');
        packet.put((byte) 'T');
        packet.put((byte) '4');
        packet.put((byte) TouchSample.PROTOCOL_VERSION);
        packet.put((byte) TouchSample.MESSAGE_TOUCH_FRAME);
        packet.putShort((short) length);
        packet.putLong(sessionId);
        packet.putLong(sequence);
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
        packet.putInt(0);
        unacknowledged.addLast(new PendingPacket(sessionId, sequence, bytes));
    }

    private void controlLoop(int sessionGeneration, DatagramSocket sessionSocket) {
        DatagramPacket incoming = new DatagramPacket(controlBuffer, controlBuffer.length);
        long nextDiscoveryNanos = 0;
        try {
            while (isSessionActive(sessionGeneration)) {
                long nowNanos = System.nanoTime();
                if (nowNanos >= nextDiscoveryNanos) {
                    sendDiscovery(sessionSocket);
                    nextDiscoveryNanos = nowNanos + DISCOVERY_INTERVAL_NANOS;
                }

                incoming.setLength(controlBuffer.length);
                try {
                    sessionSocket.receive(incoming);
                } catch (java.net.SocketTimeoutException ignored) {
                    checkHostTimeout();
                    continue;
                }
                int count = incoming.getLength();
                if (isDiscovery(incoming.getData(), count)) {
                    if (isValidDiscoveryAck(incoming.getData(), count)) {
                        synchronized (queueLock) {
                            if (!isSessionActive(sessionGeneration)) {
                                continue;
                            }
                            hostAddress = new InetSocketAddress(
                                    incoming.getAddress(),
                                    incoming.getPort()
                            );
                            // Discovery ACKs keep an idle session alive without
                            // synthetic touch traffic. During active input they
                            // must not mask a stalled data/ACK path: otherwise
                            // old touch frames remain queued and can be
                            // delivered seconds late when UDP recovers.
                            if (activeContactCount == 0 && unacknowledged.isEmpty()) {
                                lastControlReceiveNanos = System.nanoTime();
                            }
                            queueLock.notifyAll();
                        }
                    }
                    // Discovery traffic can arrive continuously, so do not
                    // rely only on the socket timeout path to detect a stale
                    // active data/ACK connection.
                    checkHostTimeout();
                    continue;
                }
                if (!isHostPacket(incoming.getAddress(), incoming.getPort())) {
                    continue;
                }
                if (count == TouchSample.CONTROL_SIZE) {
                    handleControl(sessionGeneration, incoming.getData(), System.nanoTime());
                }
            }
        } catch (IOException error) {
            failSession(sessionGeneration, "UDP acknowledgement receive failed", error);
        }
    }

    private void sendDiscovery(DatagramSocket sessionSocket) throws IOException {
        byte[] bytes = new byte[DISCOVERY_SIZE];
        ByteBuffer packet = ByteBuffer.wrap(bytes).order(ByteOrder.LITTLE_ENDIAN);
        packet.put((byte) 'H');
        packet.put((byte) 'P');
        packet.put((byte) 'T');
        packet.put((byte) 'D');
        packet.put((byte) DISCOVERY_VERSION);
        packet.put((byte) DISCOVERY_HELLO);
        packet.putShort((short) 0);
        packet.putLong(discoveryNonce);
        packet.putLong(sessionId);
        packet.putShort((short) DISCOVERY_PORT);
        packet.putShort((short) 0);
        controlCrc.reset();
        controlCrc.update(bytes, 0, DISCOVERY_SIZE - 4);
        packet.putInt((int) controlCrc.getValue());

        Set<InetAddress> destinations = new HashSet<>();
        Enumeration<NetworkInterface> interfaces = NetworkInterface.getNetworkInterfaces();
        while (interfaces != null && interfaces.hasMoreElements()) {
            NetworkInterface network = interfaces.nextElement();
            try {
                if (!network.isUp() || network.isLoopback() || !isUsbTetherInterface(network)) {
                    continue;
                }
            } catch (SocketException ignored) {
                continue;
            }
            for (java.net.InterfaceAddress address : network.getInterfaceAddresses()) {
                if (address.getBroadcast() != null) destinations.add(address.getBroadcast());
            }
        }
        for (InetAddress destination : destinations) {
            sessionSocket.send(new DatagramPacket(
                    bytes,
                    bytes.length,
                    destination,
                    DISCOVERY_PORT
            ));
        }
    }

    private static boolean isUsbTetherInterface(NetworkInterface network) {
        String name = network.getName();
        String displayName = network.getDisplayName();
        String identity = ((name == null ? "" : name) + " "
                + (displayName == null ? "" : displayName)).toLowerCase(Locale.ROOT);
        return identity.contains("rndis")
                || identity.contains("usb")
                || identity.contains("ncm")
                || identity.contains("ethernet")
                || identity.startsWith("eth");
    }

    private boolean isHostPacket(InetAddress address, int port) {
        InetSocketAddress known = hostAddress;
        return known != null
                && known.getPort() == port
                && known.getAddress().equals(address);
    }

    private boolean isDiscovery(byte[] bytes, int length) {
        return length >= 4
                && bytes[0] == 'H'
                && bytes[1] == 'P'
                && bytes[2] == 'T'
                && bytes[3] == 'D';
    }

    private boolean isValidDiscoveryAck(byte[] bytes, int length) {
        if (length != DISCOVERY_SIZE
                || (bytes[4] & 0xFF) != DISCOVERY_VERSION
                || (bytes[5] & 0xFF) != DISCOVERY_ACK) {
            return false;
        }
        if (controlPacket.getLong(8) != discoveryNonce
                || controlPacket.getLong(16) != sessionId) {
            return false;
        }
        controlCrc.reset();
        controlCrc.update(bytes, 0, DISCOVERY_SIZE - 4);
        return (int) controlCrc.getValue() == controlPacket.getInt(DISCOVERY_SIZE - 4);
    }

    private void handleControl(
            int sessionGeneration,
            byte[] control,
            long receiveNanos
    ) {
        if (control[0] != 'H'
                || control[1] != 'P'
                || control[2] != 'A'
                || control[3] != '4'
                || (control[4] & 0xFF) != TouchSample.PROTOCOL_VERSION) {
            return;
        }
        controlCrc.reset();
        controlCrc.update(control, 0, TouchSample.CONTROL_SIZE - 4);
        if ((int) controlCrc.getValue()
                != controlPacket.getInt(TouchSample.CONTROL_SIZE - 4)) {
            Log.w(TAG, "Ignoring host control record with invalid CRC");
            return;
        }
        int type = control[5] & 0xFF;
        int flags = Short.toUnsignedInt(controlPacket.getShort(6));
        long acknowledgedSession = controlPacket.getLong(8);
        long acknowledgedSequence = controlPacket.getLong(16);
        int requestedWindow = controlPacket.getInt(24);
        long hostSendNanos = controlPacket.getLong(28);
        int requestedLanes = flags & 0xFF;
        boolean becameReady = false;
        int lanesToReport = hostLaneCount;

        synchronized (queueLock) {
            if (!isSessionActive(sessionGeneration)
                    || acknowledgedSession != sessionId
                    || (type != TouchSample.CONTROL_HELLO
                    && type != TouchSample.CONTROL_ACK)) {
                return;
            }
            if (type == TouchSample.CONTROL_HELLO || !hostReady) {
                hostWindow = Math.max(1, Math.min(MAX_HOST_WINDOW, requestedWindow));
                if (requestedLanes >= 1 && requestedLanes <= 16) hostLaneCount = requestedLanes;
                lanesToReport = hostLaneCount;
                becameReady = !hostReady;
                hostReady = true;
            }
            lastHostSendNanos = hostSendNanos;
            lastControlReceiveNanos = receiveNanos;
            acknowledgeLocked(acknowledgedSequence);
            queueLock.notifyAll();
        }

        if (becameReady) {
            listener.onHostLaneCountChanged(lanesToReport);
            listener.onConnectionChanged(true, "Lossless UDP over USB tethering connected");
        }
    }

    private void acknowledgeLocked(long acknowledgedSequence) {
        if (acknowledgedSequence < 0 || acknowledgedSequence <= highestAcknowledged) return;
        highestAcknowledged = acknowledgedSequence;
        while (!unacknowledged.isEmpty()) {
            PendingPacket first = unacknowledged.peekFirst();
            if (first.sessionId != sessionId || first.sequence > acknowledgedSequence) break;
            unacknowledged.removeFirst();
        }
    }

    private void writerLoop(
            int sessionGeneration,
            DatagramSocket sessionSocket,
            CountDownLatch writerReady
    ) {
        Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_DISPLAY);
        writerReady.countDown();
        DatagramPacket outgoing = new DatagramPacket(new byte[0], 0);
        try {
            while (isSessionActive(sessionGeneration)) {
                PendingPacket pending;
                InetSocketAddress destination;
                synchronized (queueLock) {
                    long nowNanos = System.nanoTime();
                    maybeEnqueueHeartbeatLocked(nowNanos);
                    destination = hostAddress;
                    pending = destination == null
                            ? null
                            : nextPacketToWriteLocked(nowNanos);
                    if (pending == null) {
                        queueLock.wait(1);
                        continue;
                    }
                    pending.sendCount++;
                    pending.lastSentNanos = nowNanos;
                    preparePacketForWriteLocked(pending, nowNanos);
                }
                outgoing.setData(pending.bytes, 0, pending.bytes.length);
                outgoing.setSocketAddress(destination);
                sessionSocket.send(outgoing);
            }
        } catch (InterruptedException ignored) {
            Thread.currentThread().interrupt();
        } catch (IOException error) {
            failSession(sessionGeneration, "UDP touch datagram write failed", error);
        }
    }

    private PendingPacket nextPacketToWriteLocked(long nowNanos) {
        if (unacknowledged.isEmpty()) return null;
        int allowed = hostReady ? hostWindow : 1;
        int index = 0;
        PendingPacket oldestEligible = null;
        Iterator<PendingPacket> iterator = unacknowledged.iterator();
        while (iterator.hasNext() && index < allowed) {
            PendingPacket packet = iterator.next();
            if (packet.sendCount == 0) return packet;
            if (oldestEligible == null) oldestEligible = packet;
            index++;
        }
        if (oldestEligible != null
                && nowNanos - oldestEligible.lastSentNanos >= RETRANSMIT_NANOS) {
            return oldestEligible;
        }
        return null;
    }

    private void preparePacketForWriteLocked(PendingPacket pending, long sendNanos) {
        ByteBuffer packet = ByteBuffer.wrap(pending.bytes).order(ByteOrder.LITTLE_ENDIAN);
        packet.putLong(PHONE_SEND_NANOS_OFFSET, sendNanos);
        packet.putLong(ECHO_HOST_SEND_NANOS_OFFSET, lastHostSendNanos);
        packet.putLong(PHONE_CONTROL_RECEIVE_NANOS_OFFSET, lastControlReceiveNanos);
        writerCrc.reset();
        writerCrc.update(pending.bytes, 0, pending.bytes.length - TouchSample.CRC_SIZE);
        packet.putInt(pending.bytes.length - TouchSample.CRC_SIZE, (int) writerCrc.getValue());
    }

    private void maybeEnqueueHeartbeatLocked(long nowNanos) {
        if (!hostReady || activeContactCount == 0 || !unacknowledged.isEmpty()
                || nowNanos - lastHeartbeatNanos < ACTIVE_HEARTBEAT_NANOS) return;
        lastHeartbeatNanos = nowNanos;
        enqueueFrameLocked(
                TouchSample.ACTION_HEARTBEAT,
                0,
                true,
                false,
                nowNanos,
                nowNanos,
                false,
                0,
                null,
                null,
                null,
                null,
                null,
                null
        );
    }

    private void checkHostTimeout() {
        boolean reset = false;
        int staleFrames = 0;
        synchronized (queueLock) {
            long nowNanos = System.nanoTime();
            boolean hostTimedOut = hostReady
                    && nowNanos - lastControlReceiveNanos >= HOST_TIMEOUT_NANOS;
            boolean discoveryTimedOut = !hostReady
                    && nowNanos - sessionStartedNanos >= HOST_TIMEOUT_NANOS
                    && hasGameplayPendingLocked();
            if (hostTimedOut || discoveryTimedOut) {
                staleFrames = unacknowledged.size();
                beginSessionLocked();
                reset = true;
                queueLock.notifyAll();
            }
        }
        if (reset && running) {
            listener.onConnectionChanged(
                    false,
                    "Host not responding; dropped "
                            + staleFrames
                            + " stale frames; searching"
            );
        }
    }

    private boolean hasGameplayPendingLocked() {
        // The first packet is the session-start CANCEL. Any additional packet
        // is gameplay that must not be replayed after a multi-second outage.
        return unacknowledged.size() > 1;
    }

    private void beginSessionLocked() {
        sessionId = nextSessionId();
        discoveryNonce = nextSessionId();
        nextSequence = 0;
        highestAcknowledged = -1;
        hostReady = false;
        hostAddress = null;
        hostWindow = DEFAULT_HOST_WINDOW;
        unacknowledged.clear();
        activeContactCount = 0;
        resetSessionState();
        long nowNanos = System.nanoTime();
        enqueueFrameLocked(
                TouchSample.ACTION_CANCEL,
                0,
                false,
                true,
                nowNanos,
                nowNanos,
                false,
                0,
                null,
                null,
                null,
                null,
                null,
                null
        );
    }

    private void failSession(int sessionGeneration, String message, IOException error) {
        if (!isSessionActive(sessionGeneration)) return;
        Log.e(TAG, message, error);
        close();
        listener.onConnectionChanged(false, "USB-tethering network lost");
    }

    private boolean isSessionActive(int sessionGeneration) {
        return running && generation == sessionGeneration;
    }

    private long nextSessionId() {
        long candidate;
        do {
            candidate = random.nextLong() & Long.MAX_VALUE;
        } while (candidate == 0);
        return candidate;
    }

    private void resetSessionState() {
        lastHeartbeatNanos = 0;
        lastHostSendNanos = 0;
        lastControlReceiveNanos = 0;
        sessionStartedNanos = System.nanoTime();
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

    @Override
    public void close() {
        Thread previousWriter;
        Thread previousControl;
        DatagramSocket previousSocket;
        synchronized (lifecycleLock) {
            running = false;
            generation++;
            previousWriter = writerThread;
            previousControl = controlThread;
            previousSocket = socket;
            writerThread = null;
            controlThread = null;
            socket = null;
        }
        synchronized (queueLock) {
            hostReady = false;
            hostAddress = null;
            activeContactCount = 0;
            unacknowledged.clear();
            queueLock.notifyAll();
        }
        if (previousWriter != null && previousWriter != Thread.currentThread()) previousWriter.interrupt();
        if (previousControl != null && previousControl != Thread.currentThread()) previousControl.interrupt();
        if (previousSocket != null) previousSocket.close();
    }
}
