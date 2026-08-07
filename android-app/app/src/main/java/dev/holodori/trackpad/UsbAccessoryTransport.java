package dev.holodori.trackpad;

import android.hardware.usb.UsbAccessory;
import android.hardware.usb.UsbManager;
import android.os.ParcelFileDescriptor;
import android.os.Process;
import android.util.Log;

import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.IOException;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.security.SecureRandom;
import java.util.ArrayDeque;
import java.util.Iterator;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.zip.CRC32;

/**
 * Protocol-v4 AOA transport.
 *
 * <p>Frames remain in {@link #unacknowledged} until the Windows host confirms
 * that it accepted every sequence through that frame. Queue age is observable
 * but never causes a gameplay frame to be discarded.</p>
 */
final class UsbAccessoryTransport {
    private static final String TAG = "HolodoriAOA4";
    // Also drives the Win32 contact keepalive at just above the game's
    // maximum 120 Hz update rate, even while a finger is stationary.
    private static final long HEARTBEAT_NANOS = 8_000_000L;
    private static final long RETRANSMIT_NANOS = 4_000_000L;
    private static final long WRITER_READY_TIMEOUT_MILLIS = 1_000;
    private static final int DEFAULT_HOST_WINDOW = 64;
    private static final int MAX_HOST_WINDOW = 256;
    private static final int PHONE_SEND_NANOS_OFFSET = 40;
    private static final int ECHO_HOST_SEND_NANOS_OFFSET = 48;
    private static final int PHONE_CONTROL_RECEIVE_NANOS_OFFSET = 56;

    interface Listener {
        void onConnectionChanged(boolean connected, String message);
        void onHostLaneCountChanged(int laneCount);
    }

    private static final class PendingPacket {
        final long sessionId;
        final long sequence;
        final byte[] bytes;
        long lastSentNanos;
        int sendCount;

        PendingPacket(
                long sessionId,
                long sequence,
                byte[] bytes
        ) {
            this.sessionId = sessionId;
            this.sequence = sequence;
            this.bytes = bytes;
        }
    }

    private final Object queueLock = new Object();
    private final Object lifecycleLock = new Object();
    private final ArrayDeque<PendingPacket> unacknowledged =
            new ArrayDeque<>();
    private final Listener listener;
    private final SecureRandom random = new SecureRandom();
    private final CRC32 writerCrc = new CRC32();

    private volatile int generation;
    private volatile boolean running;
    private boolean hostReady;
    private int hostWindow = DEFAULT_HOST_WINDOW;
    private int hostLaneCount = 6;
    private long sessionId;
    private long nextSequence;
    private long highestAcknowledged = -1;
    private long lastHeartbeatNanos;
    private long lastHostSendNanos;
    private long lastControlReceiveNanos;

    private ParcelFileDescriptor descriptor;
    private FileInputStream input;
    private FileOutputStream output;
    private Thread controlThread;
    private Thread writerThread;

    UsbAccessoryTransport(Listener listener) {
        this.listener = listener;
    }

    boolean open(UsbManager manager, UsbAccessory accessory) {
        close();
        ParcelFileDescriptor nextDescriptor = manager.openAccessory(accessory);
        if (nextDescriptor == null) {
            listener.onConnectionChanged(false, "USB permission was not granted");
            return false;
        }

        CountDownLatch writerReady = new CountDownLatch(1);
        synchronized (queueLock) {
            sessionId = nextSessionId();
            nextSequence = 0;
            highestAcknowledged = -1;
            hostReady = false;
            hostWindow = DEFAULT_HOST_WINDOW;
            unacknowledged.clear();
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

        synchronized (lifecycleLock) {
            descriptor = nextDescriptor;
            input = new FileInputStream(descriptor.getFileDescriptor());
            output = new FileOutputStream(descriptor.getFileDescriptor());
            running = true;
            int sessionGeneration = ++generation;
            FileInputStream sessionInput = input;
            FileOutputStream sessionOutput = output;
            controlThread = new Thread(
                    () -> controlLoop(sessionGeneration, sessionInput),
                    "AOA4 acknowledgements"
            );
            writerThread = new Thread(
                    () -> writerLoop(
                            sessionGeneration,
                            sessionOutput,
                            writerReady
                    ),
                    "AOA4 touch writer"
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
                listener.onConnectionChanged(
                        false,
                        "AOA writer did not become ready"
                );
                return false;
            }
        } catch (InterruptedException error) {
            Thread.currentThread().interrupt();
            close();
            listener.onConnectionChanged(false, "AOA connection interrupted");
            return false;
        }

        listener.onConnectionChanged(
                false,
                "USB connected, waiting for lossless host"
        );
        return true;
    }

    boolean isRunning() {
        return running;
    }

    void offerFrame(
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
        if (!running) {
            return;
        }
        if (contactCount < 0 || contactCount > TouchSample.MAX_CONTACTS) {
            throw new IllegalArgumentException(
                    "Unsupported contact count: " + contactCount
            );
        }
        synchronized (queueLock) {
            if (!running) {
                return;
            }
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
        if (sessionStart) {
            frameFlags |= TouchSample.FRAME_FLAG_SESSION_START;
        }
        if (historical) {
            frameFlags |= TouchSample.FRAME_FLAG_HISTORICAL;
        }
        packet.put((byte) frameFlags);

        for (int index = 0; index < contactCount; index++) {
            float localX = x[index];
            float localY = y[index];
            boolean inside = localX >= 0f
                    && localX <= 1f
                    && localY >= 0f
                    && localY <= 1f;
            int contactFlags = inside
                    ? TouchSample.CONTACT_FLAG_INSIDE
                    : 0;
            if (touching[index]) {
                contactFlags |= TouchSample.CONTACT_FLAG_TIP;
            }
            packet.put((byte) (pointerIds[index] & 0xFF));
            packet.put((byte) contactFlags);
            packet.putShort((short) clampFixed(localX));
            packet.putShort((short) clampFixed(localY));
            packet.putShort((short) normalizeUnsigned(pressure[index]));
            packet.putShort((short) normalizeUnsigned(touchMajor[index]));
        }

        // The writer fills send/synchronization timestamps and computes the
        // only CRC immediately before each actual USB write.
        packet.putInt(0);
        unacknowledged.addLast(
                new PendingPacket(sessionId, sequence, bytes)
        );
    }

    private void controlLoop(
            int sessionGeneration,
            FileInputStream sessionInput
    ) {
        byte[] control = new byte[TouchSample.CONTROL_SIZE];
        int filled = 0;
        try {
            while (isSessionActive(sessionGeneration)) {
                int count = sessionInput.read(
                        control,
                        filled,
                        TouchSample.CONTROL_SIZE - filled
                );
                if (count < 0) {
                    throw new IOException("Host control stream closed");
                }
                filled += count;
                if (filled < TouchSample.CONTROL_SIZE) {
                    continue;
                }
                handleControl(
                        sessionGeneration,
                        control,
                        System.nanoTime()
                );
                filled = 0;
            }
        } catch (IOException error) {
            failSession(
                    sessionGeneration,
                    "AOA acknowledgement stream failed",
                    error
            );
        }
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
        CRC32 crc = new CRC32();
        crc.update(control, 0, TouchSample.CONTROL_SIZE - 4);
        ByteBuffer packet = ByteBuffer.wrap(control)
                .order(ByteOrder.LITTLE_ENDIAN);
        long expectedCrc = Integer.toUnsignedLong(
                packet.getInt(TouchSample.CONTROL_SIZE - 4)
        );
        if (crc.getValue() != expectedCrc) {
            Log.w(TAG, "Ignoring host control record with invalid CRC");
            return;
        }

        int type = control[5] & 0xFF;
        int flags = Short.toUnsignedInt(packet.getShort(6));
        long acknowledgedSession = packet.getLong(8);
        long acknowledgedSequence = packet.getLong(16);
        int requestedWindow = packet.getInt(24);
        long hostSendNanos = packet.getLong(28);
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
            if (type == TouchSample.CONTROL_HELLO) {
                hostWindow = Math.max(
                        1,
                        Math.min(MAX_HOST_WINDOW, requestedWindow)
                );
                if (requestedLanes >= 1 && requestedLanes <= 16) {
                    hostLaneCount = requestedLanes;
                }
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
            listener.onConnectionChanged(
                    true,
                    "Lossless AOA v4 connected"
            );
        }
    }

    private void acknowledgeLocked(long acknowledgedSequence) {
        if (acknowledgedSequence < 0
                || acknowledgedSequence <= highestAcknowledged) {
            return;
        }
        highestAcknowledged = acknowledgedSequence;
        while (!unacknowledged.isEmpty()) {
            PendingPacket first = unacknowledged.peekFirst();
            if (first.sessionId != sessionId
                    || first.sequence > acknowledgedSequence) {
                break;
            }
            unacknowledged.removeFirst();
        }
    }

    private void writerLoop(
            int sessionGeneration,
            FileOutputStream sessionOutput,
            CountDownLatch writerReady
    ) {
        Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_DISPLAY);
        writerReady.countDown();
        try {
            while (isSessionActive(sessionGeneration)) {
                PendingPacket pending;
                synchronized (queueLock) {
                    long nowNanos = System.nanoTime();
                    maybeEnqueueHeartbeatLocked(nowNanos);
                    pending = nextPacketToWriteLocked(nowNanos);
                    if (pending == null) {
                        queueLock.wait(1);
                        continue;
                    }
                    pending.sendCount++;
                    pending.lastSentNanos = nowNanos;
                    preparePacketForWriteLocked(pending, nowNanos);
                }
                sessionOutput.write(pending.bytes);
            }
        } catch (InterruptedException ignored) {
            Thread.currentThread().interrupt();
        } catch (IOException error) {
            failSession(
                    sessionGeneration,
                    "AOA touch stream write failed",
                    error
            );
        }
    }

    private PendingPacket nextPacketToWriteLocked(long nowNanos) {
        if (unacknowledged.isEmpty()) {
            return null;
        }

        int allowed = hostReady ? hostWindow : 1;
        int index = 0;
        PendingPacket oldestEligible = null;
        Iterator<PendingPacket> iterator = unacknowledged.iterator();
        while (iterator.hasNext() && index < allowed) {
            PendingPacket packet = iterator.next();
            if (packet.sendCount == 0) {
                return packet;
            }
            if (oldestEligible == null) {
                oldestEligible = packet;
            }
            index++;
        }
        if (oldestEligible != null
                && nowNanos - oldestEligible.lastSentNanos
                >= RETRANSMIT_NANOS) {
            return oldestEligible;
        }
        return null;
    }

    private void preparePacketForWriteLocked(
            PendingPacket pending,
            long sendNanos
    ) {
        ByteBuffer packet = ByteBuffer.wrap(pending.bytes)
                .order(ByteOrder.LITTLE_ENDIAN);
        packet.putLong(PHONE_SEND_NANOS_OFFSET, sendNanos);
        packet.putLong(ECHO_HOST_SEND_NANOS_OFFSET, lastHostSendNanos);
        packet.putLong(
                PHONE_CONTROL_RECEIVE_NANOS_OFFSET,
                lastControlReceiveNanos
        );
        writerCrc.reset();
        writerCrc.update(
                pending.bytes,
                0,
                pending.bytes.length - TouchSample.CRC_SIZE
        );
        packet.putInt(
                pending.bytes.length - TouchSample.CRC_SIZE,
                (int) writerCrc.getValue()
        );
    }

    private void maybeEnqueueHeartbeatLocked(long nowNanos) {
        if (!hostReady
                || !unacknowledged.isEmpty()
                || nowNanos - lastHeartbeatNanos < HEARTBEAT_NANOS) {
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
                0,
                null,
                null,
                null,
                null,
                null,
                null
        );
    }

    private void failSession(
            int sessionGeneration,
            String message,
            IOException error
    ) {
        if (!isSessionActive(sessionGeneration)) {
            return;
        }
        Log.e(TAG, message, error);
        close();
        listener.onConnectionChanged(false, "AOA connection lost");
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
    }

    private static int clampFixed(float value) {
        int fixed = Math.round(value * 10_000f);
        return Math.max(Short.MIN_VALUE, Math.min(Short.MAX_VALUE, fixed));
    }

    private static int normalizeUnsigned(float value) {
        float clamped = Math.max(0f, Math.min(1f, value));
        return Math.round(clamped * 65_535f);
    }

    void close() {
        Thread previousWriter;
        Thread previousControl;
        ParcelFileDescriptor previousDescriptor;
        FileInputStream previousInput;
        FileOutputStream previousOutput;

        synchronized (lifecycleLock) {
            running = false;
            generation++;
            previousWriter = writerThread;
            previousControl = controlThread;
            previousDescriptor = descriptor;
            previousInput = input;
            previousOutput = output;
            writerThread = null;
            controlThread = null;
            descriptor = null;
            input = null;
            output = null;
        }
        synchronized (queueLock) {
            hostReady = false;
            unacknowledged.clear();
            queueLock.notifyAll();
        }

        if (previousWriter != null
                && previousWriter != Thread.currentThread()) {
            previousWriter.interrupt();
        }
        if (previousControl != null
                && previousControl != Thread.currentThread()) {
            previousControl.interrupt();
        }
        try {
            if (previousInput != null) previousInput.close();
        } catch (IOException ignored) {
        }
        try {
            if (previousOutput != null) previousOutput.close();
        } catch (IOException ignored) {
        }
        try {
            if (previousDescriptor != null) previousDescriptor.close();
        } catch (IOException ignored) {
        }

    }
}
