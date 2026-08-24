package dev.holodori.trackpad;

import android.content.Context;
import android.net.ConnectivityManager;
import android.net.LinkProperties;
import android.net.Network;
import android.os.Process;
import android.util.Log;

import java.io.IOException;
import java.net.DatagramPacket;
import java.net.DatagramSocket;
import java.net.Inet4Address;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.InterfaceAddress;
import java.net.NetworkInterface;
import java.net.SocketException;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.security.SecureRandom;
import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Enumeration;
import java.util.HashSet;
import java.util.Iterator;
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
    private static final int DISCOVERY_PORT = DiscoveryPolicy.PORT;
    private static final int DISCOVERY_VERSION = DiscoveryPolicy.VERSION;
    private static final int DISCOVERY_HELLO = DiscoveryPolicy.HELLO;
    private static final int DISCOVERY_SIZE = DiscoveryPolicy.SIZE;
    private static final long DISCOVERY_INTERVAL_NANOS = 500_000_000L;
    private static final long HOST_TIMEOUT_NANOS = 2_000_000_000L;
    private static final long ACTIVE_HOST_TIMEOUT_NANOS = 64_000_000L;
    private static final long WATCHDOG_INTERVAL_MILLIS = 4L;
    // Windows touch injection needs a high-rate refresh while a contact is
    // stationary. Idle sessions do not need synthetic touch frames; discovery
    // acknowledgements keep the host liveness check alive without creating a
    // 125 Hz allocation and network workload for the whole session.
    private static final long ACTIVE_HEARTBEAT_NANOS = 8_000_000L;
    // Every frame is sent twice immediately. If both copies disappear, the
    // first timed replay still starts early enough to fit inside one 120 Hz
    // frame on a healthy USB-tethered path.
    private static final int INITIAL_SEND_COPIES = 2;
    private static final long RETRANSMIT_NANOS = 2_000_000L;
    private static final long WRITER_READY_TIMEOUT_MILLIS = 1_000;
    private static final int DEFAULT_HOST_WINDOW = 64;
    private static final int MAX_HOST_WINDOW = 256;
    private static final int PACKET_POOL_CAPACITY = MAX_HOST_WINDOW;
    private static final int PHONE_SEND_NANOS_OFFSET = 40;
    private static final int ECHO_HOST_SEND_NANOS_OFFSET = 48;
    private static final int PHONE_CONTROL_RECEIVE_NANOS_OFFSET = 56;

    private static final class PendingPacket {
        long sessionId;
        long sequence;
        final byte[] bytes = new byte[TouchSample.MAX_FRAME_SIZE];
        final ByteBuffer packet = ByteBuffer.wrap(bytes).order(ByteOrder.LITTLE_ENDIAN);
        int length;
        long lastSentNanos;
        int sendCount;

        void reset(long sessionId, long sequence, int length) {
            this.sessionId = sessionId;
            this.sequence = sequence;
            this.length = length;
            lastSentNanos = 0;
            sendCount = 0;
            packet.clear();
        }
    }

    private final Object queueLock = new Object();
    private final Object lifecycleLock = new Object();
    private final ArrayDeque<PendingPacket> unacknowledged = new ArrayDeque<>();
    private final ArrayDeque<PendingPacket> packetPool = new ArrayDeque<>();
    private final ConnectivityManager connectivityManager;
    private final TouchTransport.Listener listener;
    private final SecureRandom random = new SecureRandom();
    private final CRC32 writerCrc = new CRC32();
    // The control loop is single-threaded, so this state can be reused for
    // discovery and ACK validation instead of allocating per datagram.
    private final CRC32 controlCrc = new CRC32();
    private final byte[] controlBuffer = new byte[2_048];
    private final ByteBuffer controlPacket =
            ByteBuffer.wrap(controlBuffer).order(ByteOrder.LITTLE_ENDIAN);
    private final byte[] discoveryBuffer = new byte[DISCOVERY_SIZE];
    private final ByteBuffer discoveryPacket =
            ByteBuffer.wrap(discoveryBuffer).order(ByteOrder.LITTLE_ENDIAN);
    private final Set<String> androidNetworkInterfaces = new HashSet<>();
    private final Set<InetAddress> discoveryDestinations = new HashSet<>();
    private final ArrayList<DiscoveryPolicy.Ipv4Subnet> discoverySubnets =
            new ArrayList<>();
    private final DatagramPacket discoveryDatagram =
            new DatagramPacket(discoveryBuffer, discoveryBuffer.length);
    // Retain the latest complete contact snapshot so a restarted host can
    // reconstruct a stationary hold from the next heartbeat.
    private final int[] retainedPointerIds = new int[TouchSample.MAX_CONTACTS];
    private final float[] retainedX = new float[TouchSample.MAX_CONTACTS];
    private final float[] retainedY = new float[TouchSample.MAX_CONTACTS];
    private final float[] retainedPressure = new float[TouchSample.MAX_CONTACTS];
    private final float[] retainedTouchMajor = new float[TouchSample.MAX_CONTACTS];
    private final boolean[] retainedTouching = new boolean[TouchSample.MAX_CONTACTS];

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
    private long lastAcknowledgementProgressNanos;
    private long activePathStartedNanos;
    private int activeContactCount;
    private int retainedContactCount;

    private DatagramSocket socket;
    private Thread controlThread;
    private Thread writerThread;
    private Thread watchdogThread;

    UdpTransport(Context context, TouchTransport.Listener listener) {
        this.listener = listener;
        Context applicationContext = context.getApplicationContext();
        Context serviceContext = applicationContext == null ? context : applicationContext;
        connectivityManager = (ConnectivityManager) serviceContext.getSystemService(
                Context.CONNECTIVITY_SERVICE
        );
        for (int index = 0; index < PACKET_POOL_CAPACITY; index++) {
            packetPool.addLast(new PendingPacket());
        }
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
            watchdogThread = new Thread(
                    () -> watchdogLoop(sessionGeneration),
                    "UDP4 liveness watchdog"
            );
            controlThread.start();
            writerThread.start();
            watchdogThread.start();
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

        synchronized (queueLock) {
            if (!hostReady) {
                listener.onConnectionChanged(
                        false,
                        "USB tethering active, searching for host"
                );
            }
        }
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
            // During a short socket restart, keep the latest full snapshot but
            // do not queue stale transitions. A still-held finger is then
            // reconstructed by the first heartbeat of the fresh session.
            if (!running) return;
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
        long sequence = nextSequence++;
        int length = TouchSample.FRAME_HEADER_SIZE
                + contactCount * TouchSample.CONTACT_SIZE
                + TouchSample.CRC_SIZE;
        PendingPacket pending = obtainPacketLocked(sessionId, sequence, length);
        ByteBuffer packet = pending.packet;
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
        unacknowledged.addLast(pending);
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
                    checkHostTimeout(sessionGeneration);
                    continue;
                }
                int count = incoming.getLength();
                if (DiscoveryPolicy.hasDiscoveryMagic(incoming.getData(), count)) {
                    if (DiscoveryPolicy.isValidAck(
                            incoming.getData(),
                            count,
                            discoveryNonce,
                            sessionId,
                            controlCrc
                    )) {
                        InetSocketAddress source =
                                (InetSocketAddress) incoming.getSocketAddress();
                        synchronized (queueLock) {
                            if (!isSessionActive(sessionGeneration)) {
                                continue;
                            }
                            DiscoveryPolicy.EndpointDecision endpointDecision =
                                    DiscoveryPolicy.decideEndpoint(
                                            hostAddress,
                                            source,
                                            discoverySubnets
                            );
                            if (endpointDecision
                                    != DiscoveryPolicy.EndpointDecision.REJECT) {
                                if (endpointDecision
                                        == DiscoveryPolicy.EndpointDecision.PIN) {
                                    hostAddress = source;
                                }
                                // Discovery ACKs from the pinned endpoint keep
                                // an idle session alive without synthetic touch
                                // traffic. During active input they must not mask
                                // a stalled data/ACK path.
                                if (activeContactCount == 0
                                        && unacknowledged.isEmpty()) {
                                    lastControlReceiveNanos = System.nanoTime();
                                }
                                queueLock.notifyAll();
                            }
                        }
                    }
                    // Discovery traffic can arrive continuously, so do not
                    // rely only on the socket timeout path to detect a stale
                    // active data/ACK connection.
                    checkHostTimeout(sessionGeneration);
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
        ByteBuffer packet = discoveryPacket;
        packet.clear();
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
        controlCrc.update(discoveryBuffer, 0, DISCOVERY_SIZE - 4);
        packet.putInt((int) controlCrc.getValue());

        boolean linkPropertiesSnapshotComplete = refreshAndroidNetworkInterfaces();
        discoveryDestinations.clear();
        discoverySubnets.clear();
        int bestCandidatePriority = 0;
        Enumeration<NetworkInterface> interfaces = NetworkInterface.getNetworkInterfaces();
        while (interfaces != null && interfaces.hasMoreElements()) {
            NetworkInterface network = interfaces.nextElement();
            int candidatePriority;
            try {
                if (!network.isUp() || network.isLoopback()) {
                    continue;
                }
                candidatePriority = DiscoveryPolicy.candidatePriority(
                        network.getName(),
                        network.getDisplayName(),
                        androidNetworkInterfaces,
                        linkPropertiesSnapshotComplete
                );
                if (candidatePriority == 0) {
                    continue;
                }
            } catch (SocketException ignored) {
                continue;
            }
            for (InterfaceAddress address : network.getInterfaceAddresses()) {
                InetAddress localAddress = address.getAddress();
                InetAddress broadcastAddress = address.getBroadcast();
                if (!(localAddress instanceof Inet4Address)
                        || !(broadcastAddress instanceof Inet4Address)) {
                    continue;
                }
                DiscoveryPolicy.Ipv4Subnet subnet = DiscoveryPolicy.Ipv4Subnet.from(
                        localAddress,
                        address.getNetworkPrefixLength()
                );
                if (subnet != null) {
                    if (candidatePriority < bestCandidatePriority) {
                        continue;
                    }
                    if (candidatePriority > bestCandidatePriority) {
                        discoveryDestinations.clear();
                        discoverySubnets.clear();
                        bestCandidatePriority = candidatePriority;
                    }
                    discoveryDestinations.add(broadcastAddress);
                    if (!discoverySubnets.contains(subnet)) {
                        discoverySubnets.add(subnet);
                    }
                }
            }
        }
        for (InetAddress destination : discoveryDestinations) {
            discoveryDatagram.setAddress(destination);
            discoveryDatagram.setPort(DISCOVERY_PORT);
            discoveryDatagram.setLength(discoveryBuffer.length);
            // Losing one discovery datagram must not add the old 500 ms
            // discovery interval to startup or reconnect.
            sessionSocket.send(discoveryDatagram);
            sessionSocket.send(discoveryDatagram);
        }
    }

    private boolean refreshAndroidNetworkInterfaces() {
        androidNetworkInterfaces.clear();
        if (connectivityManager == null) {
            return false;
        }
        boolean complete = true;
        try {
            Network[] networks = connectivityManager.getAllNetworks();
            if (networks == null) {
                return false;
            }
            for (Network network : networks) {
                LinkProperties properties = connectivityManager.getLinkProperties(network);
                if (properties == null) {
                    complete = false;
                    continue;
                }
                collectAndroidNetworkInterfaces(properties);
            }
        } catch (RuntimeException ignored) {
            androidNetworkInterfaces.clear();
            return false;
        }
        return complete;
    }

    private void collectAndroidNetworkInterfaces(LinkProperties properties) {
        String interfaceName = DiscoveryPolicy.normalizeInterfaceName(
                properties.getInterfaceName()
        );
        if (!interfaceName.isEmpty()) {
            androidNetworkInterfaces.add(interfaceName);
        }
    }

    private boolean isHostPacket(InetAddress address, int port) {
        InetSocketAddress known = hostAddress;
        return known != null
                && known.getPort() == port
                && known.getAddress().equals(address);
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
            if (acknowledgeLocked(acknowledgedSequence)) {
                lastAcknowledgementProgressNanos = receiveNanos;
            }
            queueLock.notifyAll();
        }

        if (becameReady) {
            listener.onHostLaneCountChanged(lanesToReport);
            listener.onConnectionChanged(true, "Lossless UDP over USB tethering connected");
        }
    }

    private boolean acknowledgeLocked(long acknowledgedSequence) {
        if (acknowledgedSequence < 0 || acknowledgedSequence <= highestAcknowledged) return false;
        if (acknowledgedSequence >= nextSequence) {
            return false;
        }
        highestAcknowledged = acknowledgedSequence;
        while (!unacknowledged.isEmpty()) {
            PendingPacket first = unacknowledged.peekFirst();
            if (first.sessionId != sessionId || first.sequence > acknowledgedSequence) break;
            recyclePacketLocked(unacknowledged.removeFirst());
        }
        if (!hasActiveDataPathLocked()) activePathStartedNanos = 0;
        return true;
    }

    private void writerLoop(
            int sessionGeneration,
            DatagramSocket sessionSocket,
            CountDownLatch writerReady
    ) {
        Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_DISPLAY);
        writerReady.countDown();
        byte[] sendBuffer = new byte[TouchSample.MAX_FRAME_SIZE];
        DatagramPacket outgoing = new DatagramPacket(new byte[0], 0);
        try {
            while (isSessionActive(sessionGeneration)) {
                InetSocketAddress destination;
                int sendLength;
                synchronized (queueLock) {
                    long nowNanos = System.nanoTime();
                    maybeEnqueueHeartbeatLocked(nowNanos);
                    destination = hostAddress;
                    PendingPacket pending = destination == null
                            ? null
                            : nextPacketToWriteLocked(nowNanos);
                    if (pending == null) {
                        queueLock.wait(1);
                        continue;
                    }
                    pending.sendCount++;
                    pending.lastSentNanos = nowNanos;
                    preparePacketForWriteLocked(pending, nowNanos);
                    sendLength = pending.length;
                    System.arraycopy(pending.bytes, 0, sendBuffer, 0, sendLength);
                }
                outgoing.setData(sendBuffer, 0, sendLength);
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
        Iterator<PendingPacket> iterator = unacknowledged.iterator();

        // A burst of new MotionEvent history must not postpone repair of an
        // ordering hole beyond the frame budget. Once a packet's redundant
        // pair is 2 ms old, repair it before serving newer packets.
        while (iterator.hasNext() && index < allowed) {
            PendingPacket packet = iterator.next();
            if (packet.sendCount >= INITIAL_SEND_COPIES
                    && nowNanos - packet.lastSentNanos >= RETRANSMIT_NANOS) {
                return packet;
            }
            index++;
        }

        index = 0;
        iterator = unacknowledged.iterator();
        while (iterator.hasNext() && index < allowed) {
            PendingPacket packet = iterator.next();
            if (packet.sendCount < INITIAL_SEND_COPIES) return packet;
            index++;
        }
        return null;
    }

    private void preparePacketForWriteLocked(PendingPacket pending, long sendNanos) {
        ByteBuffer packet = pending.packet;
        int length = pending.length;
        packet.putLong(PHONE_SEND_NANOS_OFFSET, sendNanos);
        packet.putLong(ECHO_HOST_SEND_NANOS_OFFSET, lastHostSendNanos);
        packet.putLong(PHONE_CONTROL_RECEIVE_NANOS_OFFSET, lastControlReceiveNanos);
        writerCrc.reset();
        writerCrc.update(pending.bytes, 0, length - TouchSample.CRC_SIZE);
        packet.putInt(length - TouchSample.CRC_SIZE, (int) writerCrc.getValue());
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
                retainedContactCount,
                retainedPointerIds,
                retainedX,
                retainedY,
                retainedPressure,
                retainedTouchMajor,
                retainedTouching
        );
    }

    private void checkHostTimeout(int sessionGeneration) {
        boolean timedOut = false;
        int staleFrames = 0;
        synchronized (queueLock) {
            if (!isSessionActive(sessionGeneration)) return;
            long nowNanos = System.nanoTime();
            boolean activeDataPath = hasActiveDataPathLocked();
            long hostTimeoutNanos = activeDataPath
                    ? ACTIVE_HOST_TIMEOUT_NANOS
                    : HOST_TIMEOUT_NANOS;
            long responseWindowStartedNanos = activeDataPath
                    ? Math.max(lastAcknowledgementProgressNanos, activePathStartedNanos)
                    : lastControlReceiveNanos;
            boolean hostTimedOut = hostReady
                    && nowNanos - responseWindowStartedNanos >= hostTimeoutNanos;
            boolean discoveryTimedOut = !hostReady
                    && activeDataPath
                    && activePathStartedNanos > 0
                    && nowNanos - activePathStartedNanos >= ACTIVE_HOST_TIMEOUT_NANOS;
            if (hostTimedOut || discoveryTimedOut) {
                staleFrames = unacknowledged.size();
                timedOut = true;
            }
        }
        if (timedOut && closeSession(sessionGeneration)) {
            // Close the socket before clearing state. DatagramSocket.close()
            // interrupts network I/O, so a wedged send cannot hold the
            // watchdog or prevent MainActivity from opening a fresh session.
            listener.onConnectionChanged(
                    false,
                    "Host not responding; dropped "
                            + staleFrames
                            + " stale frames; restarting"
            );
        }
    }

    private void watchdogLoop(int sessionGeneration) {
        while (isSessionActive(sessionGeneration)) {
            synchronized (queueLock) {
                try {
                    queueLock.wait(WATCHDOG_INTERVAL_MILLIS);
                } catch (InterruptedException ignored) {
                    Thread.currentThread().interrupt();
                    return;
                }
            }
            checkHostTimeout(sessionGeneration);
        }
    }

    private boolean hasGameplayPendingLocked() {
        // Sequence zero is the session-start CANCEL. It may already have been
        // acknowledged, so queue size alone cannot distinguish a lone pending
        // UP/CANCEL from setup traffic.
        PendingPacket newest = unacknowledged.peekLast();
        return newest != null && newest.sequence > 0;
    }

    private boolean hasActiveDataPathLocked() {
        return activeContactCount > 0 || hasGameplayPendingLocked();
    }

    private void updateActivePathStateLocked(boolean wasActiveDataPath, long nowNanos) {
        boolean activeDataPath = hasActiveDataPathLocked();
        if (!wasActiveDataPath && activeDataPath) {
            // Idle discovery ACKs may be hundreds of milliseconds old. Give
            // the first DOWN/UP/CANCEL its own response window instead of
            // immediately applying the 64 ms gameplay timeout to that stale
            // idle timestamp. MOVE frames during a hold/slide do not reset
            // this window; only cumulative ACK advancement proves that the
            // host committed the ordered stream through its input sink.
            activePathStartedNanos = nowNanos;
        } else if (!activeDataPath) {
            activePathStartedNanos = 0;
        }
    }

    private void beginSessionLocked() {
        sessionId = nextSessionId();
        discoveryNonce = nextSessionId();
        nextSequence = 0;
        highestAcknowledged = -1;
        hostReady = false;
        hostAddress = null;
        hostWindow = DEFAULT_HOST_WINDOW;
        clearUnacknowledgedLocked();
        activeContactCount = countActiveContacts(retainedTouching, retainedContactCount);
        resetSessionState();
        if (activeContactCount > 0) activePathStartedNanos = System.nanoTime();
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
        if (!closeSession(sessionGeneration)) return;
        Log.e(TAG, message, error);
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
        lastAcknowledgementProgressNanos = 0;
        activePathStartedNanos = 0;
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

    private PendingPacket obtainPacketLocked(long packetSessionId, long sequence, int length) {
        PendingPacket packet = packetPool.pollFirst();
        if (packet == null) packet = new PendingPacket();
        packet.reset(packetSessionId, sequence, length);
        return packet;
    }

    private void recyclePacketLocked(PendingPacket packet) {
        if (packetPool.size() < PACKET_POOL_CAPACITY) packetPool.addLast(packet);
    }

    private void clearUnacknowledgedLocked() {
        while (!unacknowledged.isEmpty()) {
            recyclePacketLocked(unacknowledged.removeFirst());
        }
    }

    @Override
    public void close() {
        closeInternal(0, false);
    }

    private boolean closeSession(int sessionGeneration) {
        return closeInternal(sessionGeneration, true);
    }

    private boolean closeInternal(int expectedGeneration, boolean requireGenerationMatch) {
        Thread previousWriter;
        Thread previousControl;
        Thread previousWatchdog;
        DatagramSocket previousSocket;
        synchronized (lifecycleLock) {
            if (requireGenerationMatch
                    && (!running || generation != expectedGeneration)) {
                return false;
            }
            running = false;
            generation++;
            previousWriter = writerThread;
            previousControl = controlThread;
            previousWatchdog = watchdogThread;
            previousSocket = socket;
            writerThread = null;
            controlThread = null;
            watchdogThread = null;
            socket = null;
            if (previousSocket != null) previousSocket.close();
            synchronized (queueLock) {
                hostReady = false;
                hostAddress = null;
                clearUnacknowledgedLocked();
                queueLock.notifyAll();
            }
        }
        if (previousWriter != null && previousWriter != Thread.currentThread()) previousWriter.interrupt();
        if (previousControl != null && previousControl != Thread.currentThread()) previousControl.interrupt();
        if (previousWatchdog != null && previousWatchdog != Thread.currentThread()) {
            previousWatchdog.interrupt();
        }
        return true;
    }
}
