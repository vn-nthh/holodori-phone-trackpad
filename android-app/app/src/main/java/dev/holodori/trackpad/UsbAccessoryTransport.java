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

final class UsbAccessoryTransport {
    private static final String TAG = "HolodoriAOA";
    private static final long HEARTBEAT_MILLIS = 200;
    private static final long QUEUE_WARNING_AGE_NANOS = 8_000_000L;
    private static final long QUEUE_RESYNC_AGE_NANOS = 25_000_000L;
    private static final long MAX_QUEUE_AGE_NANOS = 100_000_000L;
    private static final long QUEUE_LOG_INTERVAL_NANOS = 1_000_000_000L;
    // Heartbeats encode queue age in 10 us units, up to about 328 ms.
    private static final long QUEUE_AGE_REPORT_UNIT_NANOS = 10_000L;
    private static final long INCIDENT_WRITE_AGE_UNIT_NANOS = 20_000L;
    private static final int INCIDENT_DETAIL_DURATION_MASK = 0x3FFF;
    private static final int INCIDENT_DETAIL_REASON_SHIFT = 14;
    private static final int INCIDENT_REASON_WARNING = 0;
    private static final int INCIDENT_REASON_RESYNC = 1;
    private static final int INCIDENT_REASON_FAILSAFE = 2;
    private static final int INCIDENT_REASON_CAPACITY = 3;
    private static final int HOST_CONTROL_SIZE = 8;
    private static final int HOST_CONTROL_ATTACH = 1;

    interface Listener {
        void onConnectionChanged(boolean connected, String message);
    }

    private static final int CAPACITY = 1024;
    private static final int INCIDENT_CAPACITY = 64;
    private final Object queueLock = new Object();
    private final Object lifecycleLock = new Object();
    private final TouchSample[] queue = new TouchSample[CAPACITY];
    private final TouchSample[] incidentQueue =
            new TouchSample[INCIDENT_CAPACITY];
    private final boolean[] activePointers = new boolean[256];
    private final Listener listener;

    private int head;
    private int tail;
    private int incidentHead;
    private int incidentTail;
    private int activeTouchCount;
    private int sequence;
    private long maxQueueAgeNanos;
    private int maxQueueDepth;
    private long queueWarningCount;
    private long queueResyncCount;
    private long queueFailsafeCount;
    private long coalescedMoveCount;
    private long hostRecoveryCount;
    private long lastQueueLogNanos;
    private long reportMaxQueueAgeNanos;
    private int reportMaxQueueDepth;
    private int reportResyncCount;
    private boolean reportQueueWarning;
    private boolean reportQueueFailsafe;
    private boolean queueWarningActive;
    private boolean hostAttachSeen;
    private volatile int generation;
    private volatile boolean running;
    private volatile long writeStartedNanos;
    private volatile long lastWriteCompletedNanos;
    private volatile long lastWriteDurationNanos;
    private ParcelFileDescriptor descriptor;
    private FileInputStream input;
    private FileOutputStream output;
    private Thread controlThread;
    private Thread writerThread;

    UsbAccessoryTransport(Listener listener) {
        this.listener = listener;
        for (int index = 0; index < CAPACITY; index++) {
            queue[index] = new TouchSample();
        }
        for (int index = 0; index < INCIDENT_CAPACITY; index++) {
            incidentQueue[index] = new TouchSample();
        }
    }

    boolean open(UsbManager manager, UsbAccessory accessory) {
        close();
        ParcelFileDescriptor nextDescriptor = manager.openAccessory(accessory);
        if (nextDescriptor == null) {
            listener.onConnectionChanged(false, "USB permission was not granted");
            return false;
        }

        synchronized (queueLock) {
            replaceQueueWithSessionReset(System.nanoTime(), false);
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
                    "AOA host control"
            );
            writerThread = new Thread(
                    () -> writerLoop(sessionGeneration, sessionOutput),
                    "AOA touch writer"
            );
            controlThread.start();
            writerThread.start();
        }
        listener.onConnectionChanged(true, "AOA connected");
        return true;
    }

    boolean isRunning() {
        return running;
    }

    void offer(
            int action,
            int pointerId,
            float x,
            float y,
            boolean inside,
            boolean locked,
            long eventNanos
    ) {
        if (!running) {
            return;
        }
        synchronized (queueLock) {
            if (!running) {
                return;
            }
            long nowNanos = System.nanoTime();
            checkQueuePressure(nowNanos);

            int normalizedPointerId = pointerId & 0xFF;
            updateActiveTouchState(action, normalizedPointerId);
            if (action == TouchSample.ACTION_MOVE
                    && coalescePendingMove(
                            normalizedPointerId,
                            x,
                            y,
                            inside,
                            locked,
                            eventNanos
                    )) {
                coalescedMoveCount++;
                recordQueueMetrics(nowNanos);
                return;
            }

            int next = (head + 1) % CAPACITY;
            if (next == tail) {
                resynchronizeQueue(
                        nowNanos,
                        queueAgeNanos(nowNanos),
                        queueDepth(),
                        true,
                        INCIDENT_REASON_CAPACITY,
                        "capacity"
                );
                next = (head + 1) % CAPACITY;
            }
            TouchSample sample = queue[head];
            sample.action = action;
            sample.pointerId = normalizedPointerId;
            updateSample(sample, x, y, inside, locked, eventNanos);
            head = next;
            recordQueueMetrics(nowNanos);
            queueLock.notify();
        }
    }

    private void checkQueuePressure(long nowNanos) {
        if (head == tail) {
            queueWarningActive = false;
            return;
        }
        long ageNanos = queueAgeNanos(nowNanos);
        int depth = queueDepth();
        recordQueueMetrics(ageNanos, depth);

        if (ageNanos >= MAX_QUEUE_AGE_NANOS) {
            resynchronizeQueue(
                    nowNanos,
                    ageNanos,
                    depth,
                    true,
                    INCIDENT_REASON_FAILSAFE,
                    "100 ms failsafe"
            );
        } else if (ageNanos >= QUEUE_RESYNC_AGE_NANOS) {
            resynchronizeQueue(
                    nowNanos,
                    ageNanos,
                    depth,
                    false,
                    INCIDENT_REASON_RESYNC,
                    "latency budget"
            );
        } else if (ageNanos >= QUEUE_WARNING_AGE_NANOS) {
            if (!queueWarningActive) {
                queueWarningCount++;
                recordQueueIncident(
                        nowNanos,
                        ageNanos,
                        depth,
                        INCIDENT_REASON_WARNING
                );
            }
            queueWarningActive = true;
            reportQueueWarning = true;
            logQueuePressure(nowNanos, ageNanos, depth, false);
        } else {
            queueWarningActive = false;
        }
    }

    private void resynchronizeQueue(
            long nowNanos,
            long ageNanos,
            int depth,
            boolean failsafe,
            int incidentReason,
            String reason
    ) {
        if (!queueWarningActive) {
            queueWarningCount++;
        }
        queueWarningActive = true;
        queueResyncCount++;
        reportQueueWarning = true;
        reportResyncCount = Math.min(
                Short.MAX_VALUE, reportResyncCount + 1
        );
        if (failsafe) {
            queueFailsafeCount++;
            reportQueueFailsafe = true;
        }
        recordQueueIncident(
                nowNanos, ageNanos, depth, incidentReason
        );
        if (logQueuePressure(nowNanos, ageNanos, depth, failsafe)) {
            Log.w(
                    TAG,
                    "Dropping stale touch backlog and sending CANCEL ("
                            + reason + ")"
            );
        }
        // Replaying an old rhythm input is worse than dropping it. Tell the
        // host to release every key; a following current sample resumes state.
        replaceQueueWithCancel(nowNanos);
        queueWarningActive = false;
    }

    private boolean logQueuePressure(
            long nowNanos, long ageNanos, int depth, boolean force
    ) {
        if (!force
                && nowNanos - lastQueueLogNanos
                < QUEUE_LOG_INTERVAL_NANOS) {
            return false;
        }
        lastQueueLogNanos = nowNanos;
        Log.w(
                TAG,
                "Touch queue age="
                        + ageNanos / 1_000_000.0
                        + " ms, depth="
                        + depth
        );
        return true;
    }

    private int queueDepth() {
        return (head - tail + CAPACITY) % CAPACITY;
    }

    private long queueAgeNanos(long nowNanos) {
        if (head == tail) {
            return 0;
        }
        return Math.max(0, nowNanos - queue[tail].eventNanos);
    }

    private void recordQueueMetrics(long nowNanos) {
        recordQueueMetrics(queueAgeNanos(nowNanos), queueDepth());
    }

    private void recordQueueMetrics(long ageNanos, int depth) {
        maxQueueAgeNanos = Math.max(maxQueueAgeNanos, ageNanos);
        maxQueueDepth = Math.max(maxQueueDepth, depth);
        reportMaxQueueAgeNanos = Math.max(
                reportMaxQueueAgeNanos, ageNanos
        );
        reportMaxQueueDepth = Math.max(reportMaxQueueDepth, depth);
    }

    private void recordQueueIncident(
            long nowNanos,
            long ageNanos,
            int depth,
            int reason
    ) {
        int next = (incidentHead + 1) % INCIDENT_CAPACITY;
        if (next == incidentTail) {
            incidentTail = (incidentTail + 1) % INCIDENT_CAPACITY;
        }

        long startedNanos = writeStartedNanos;
        long writeAgeNanos = 0;
        boolean writeBlocked = startedNanos > 0;
        if (writeBlocked) {
            writeAgeNanos = Math.max(0, nowNanos - startedNanos);
        } else {
            long completedNanos = lastWriteCompletedNanos;
            long durationNanos = lastWriteDurationNanos;
            if (completedNanos > 0
                    && nowNanos - completedNanos <= ageNanos
                    && durationNanos >= QUEUE_WARNING_AGE_NANOS) {
                // The oldest queued sample existed during the immediately
                // preceding slow write, even if detection ran just after the
                // write returned.
                writeBlocked = true;
                writeAgeNanos = durationNanos;
            }
        }
        int durationUnits = (int) Math.min(
                INCIDENT_DETAIL_DURATION_MASK,
                writeAgeNanos / INCIDENT_WRITE_AGE_UNIT_NANOS
        );
        int detail = (
                (reason & 0x03) << INCIDENT_DETAIL_REASON_SHIFT
        ) | durationUnits;

        TouchSample incident = incidentQueue[incidentHead];
        incident.action = TouchSample.ACTION_HEARTBEAT;
        incident.pointerId = Math.min(0xFF, depth);
        incident.flags = TouchSample.FLAG_QUEUE_DIAGNOSTICS
                | TouchSample.FLAG_QUEUE_INCIDENT;
        if (activeTouchCount > 0) {
            incident.flags |= TouchSample.FLAG_INCIDENT_ACTIVE_TOUCH;
        }
        if (writeBlocked) {
            incident.flags |= TouchSample.FLAG_INCIDENT_WRITER_BLOCKED;
        }
        incident.x = (int) Math.min(
                Short.MAX_VALUE,
                ageNanos / QUEUE_AGE_REPORT_UNIT_NANOS
        );
        // Preserve compatibility with protocol-v2 hosts, which treat Y on
        // every diagnostic heartbeat as a resync count. Incident metadata
        // uses the low 16 timestamp bits instead, retaining ~66 us timing.
        incident.y = 0;
        incident.eventNanos = (nowNanos & ~0xFFFFL) | detail;
        incidentHead = next;
    }

    private void updateActiveTouchState(int action, int pointerId) {
        if (action == TouchSample.ACTION_CANCEL) {
            clearActiveTouches();
            return;
        }
        if (action == TouchSample.ACTION_UP) {
            if (activePointers[pointerId]) {
                activePointers[pointerId] = false;
                activeTouchCount--;
            }
            return;
        }
        if ((action == TouchSample.ACTION_DOWN
                || action == TouchSample.ACTION_MOVE)
                && !activePointers[pointerId]) {
            activePointers[pointerId] = true;
            activeTouchCount++;
        }
    }

    private void clearActiveTouches() {
        for (int index = 0; index < activePointers.length; index++) {
            activePointers[index] = false;
        }
        activeTouchCount = 0;
    }

    private boolean coalescePendingMove(
            int pointerId,
            float x,
            float y,
            boolean inside,
            boolean locked,
            long eventNanos
    ) {
        int index = head;
        while (index != tail) {
            index = (index - 1 + CAPACITY) % CAPACITY;
            TouchSample pending = queue[index];
            if (pending.action != TouchSample.ACTION_MOVE) {
                break;
            }
            if (pending.pointerId == pointerId) {
                updateSample(pending, x, y, inside, locked, eventNanos);
                return true;
            }
        }
        return false;
    }

    private void replaceQueueWithCancel(long eventNanos) {
        tail = head;
        TouchSample cancel = queue[head];
        cancel.action = TouchSample.ACTION_CANCEL;
        cancel.pointerId = 0;
        cancel.flags = 0;
        cancel.x = 0;
        cancel.y = 0;
        cancel.eventNanos = eventNanos;
        head = (head + 1) % CAPACITY;
    }

    private void replaceQueueWithSessionReset(
            long eventNanos, boolean hostRecovery
    ) {
        tail = head;
        incidentTail = incidentHead;
        clearActiveTouches();
        resetQueueReport();
        queueWarningActive = false;
        TouchSample reset = queue[head];
        reset.action = TouchSample.ACTION_CANCEL;
        reset.pointerId = 0;
        reset.flags = TouchSample.FLAG_SESSION_RESET
                | (hostRecovery ? TouchSample.FLAG_HOST_RECOVERY : 0);
        reset.x = 0;
        reset.y = 0;
        reset.eventNanos = eventNanos;
        head = (head + 1) % CAPACITY;
    }

    private static void updateSample(
            TouchSample sample,
            float x,
            float y,
            boolean inside,
            boolean locked,
            long eventNanos
    ) {
        sample.flags =
                (inside ? TouchSample.FLAG_INSIDE : 0)
                        | (locked ? TouchSample.FLAG_LOCKED : 0);
        sample.x = clampFixed(x);
        sample.y = clampFixed(y);
        sample.eventNanos = eventNanos;
    }

    private static int clampFixed(float value) {
        int fixed = Math.round(value * 10000f);
        return Math.max(Short.MIN_VALUE, Math.min(Short.MAX_VALUE, fixed));
    }

    private void controlLoop(
            int sessionGeneration,
            FileInputStream sessionInput
    ) {
        byte[] control = new byte[HOST_CONTROL_SIZE];
        int filled = 0;
        try {
            while (isSessionActive(sessionGeneration)) {
                int count = sessionInput.read(
                        control, filled, HOST_CONTROL_SIZE - filled
                );
                if (count < 0) {
                    throw new IOException("Host control stream closed");
                }
                filled += count;
                if (filled < HOST_CONTROL_SIZE) {
                    continue;
                }
                if (control[0] == 'H'
                        && control[1] == 'P'
                        && control[2] == 'T'
                        && control[3] == 'C'
                        && (control[4] & 0xFF)
                        == TouchSample.PROTOCOL_VERSION
                        && (control[5] & 0xFF) == HOST_CONTROL_ATTACH) {
                    handleHostAttach(sessionGeneration);
                }
                filled = 0;
            }
        } catch (IOException error) {
            if (isSessionActive(sessionGeneration)) {
                Log.e(TAG, "Host control stream failed", error);
                close();
                listener.onConnectionChanged(false, "AOA connection lost");
            }
        }
    }

    private void handleHostAttach(int sessionGeneration) {
        boolean recovered;
        synchronized (queueLock) {
            if (!isSessionActive(sessionGeneration)) {
                return;
            }
            recovered = hostAttachSeen;
            hostAttachSeen = true;
            if (recovered) {
                hostRecoveryCount++;
                replaceQueueWithSessionReset(System.nanoTime(), true);
                queueLock.notifyAll();
            }
        }
        if (recovered) {
            Log.w(TAG, "New host process attached; cleared stale touch queue");
        }
    }

    private void writerLoop(
            int sessionGeneration,
            FileOutputStream sessionOutput
    ) {
        Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_DISPLAY);
        ByteBuffer packet = ByteBuffer.allocate(24).order(ByteOrder.LITTLE_ENDIAN);
        try {
            while (isSessionActive(sessionGeneration)) {
                int action;
                int pointerId;
                int flags;
                int x;
                int y;
                int currentSequence;
                long eventNanos;

                synchronized (queueLock) {
                    if (isSessionActive(sessionGeneration)
                            && head == tail
                            && incidentHead == incidentTail) {
                        queueLock.wait(HEARTBEAT_MILLIS);
                    }
                    if (!isSessionActive(sessionGeneration)) {
                        break;
                    }
                    if (head == tail) {
                        queueWarningActive = false;
                        if (incidentHead != incidentTail) {
                            TouchSample incident =
                                    incidentQueue[incidentTail];
                            action = incident.action;
                            pointerId = incident.pointerId;
                            flags = incident.flags;
                            x = incident.x;
                            y = incident.y;
                            currentSequence = sequence++;
                            eventNanos = incident.eventNanos;
                            incidentTail = (
                                    incidentTail + 1
                            ) % INCIDENT_CAPACITY;
                        } else {
                            action = TouchSample.ACTION_HEARTBEAT;
                            pointerId = Math.min(
                                    0xFF, reportMaxQueueDepth
                            );
                            flags = TouchSample.FLAG_QUEUE_DIAGNOSTICS;
                            if (reportQueueWarning) {
                                flags |= TouchSample.FLAG_QUEUE_WARNING;
                            }
                            if (reportResyncCount > 0) {
                                flags |= TouchSample.FLAG_QUEUE_RESYNC;
                            }
                            if (reportQueueFailsafe) {
                                flags |= TouchSample.FLAG_QUEUE_FAILSAFE;
                            }
                            x = (int) Math.min(
                                    Short.MAX_VALUE,
                                    reportMaxQueueAgeNanos
                                            / QUEUE_AGE_REPORT_UNIT_NANOS
                            );
                            y = reportResyncCount;
                            currentSequence = sequence++;
                            eventNanos = System.nanoTime();
                            resetQueueReport();
                        }
                    } else {
                        checkQueuePressure(System.nanoTime());
                        TouchSample sample = queue[tail];
                        action = sample.action;
                        pointerId = sample.pointerId;
                        flags = sample.flags;
                        x = sample.x;
                        y = sample.y;
                        currentSequence = sequence++;
                        eventNanos = sample.eventNanos;
                        tail = (tail + 1) % CAPACITY;
                    }
                }

                packet.clear();
                packet.put((byte) 'H');
                packet.put((byte) 'P');
                packet.put((byte) 'T');
                packet.put((byte) '1');
                packet.put((byte) TouchSample.PROTOCOL_VERSION);
                packet.put((byte) action);
                packet.put((byte) pointerId);
                packet.put((byte) flags);
                packet.putShort((short) x);
                packet.putShort((short) y);
                packet.putInt(currentSequence);
                packet.putLong(eventNanos);
                long startedNanos = System.nanoTime();
                writeStartedNanos = startedNanos;
                try {
                    sessionOutput.write(packet.array());
                } finally {
                    long completedNanos = System.nanoTime();
                    lastWriteDurationNanos = Math.max(
                            0, completedNanos - startedNanos
                    );
                    lastWriteCompletedNanos = completedNanos;
                    writeStartedNanos = 0;
                }
            }
        } catch (InterruptedException ignored) {
            Thread.currentThread().interrupt();
        } catch (IOException error) {
            if (isSessionActive(sessionGeneration)) {
                Log.e(TAG, "Touch stream write failed", error);
                // AOA exposes one exclusive descriptor. Releasing it here is
                // essential: if the host disappears without a clean USB
                // detach, retaining the descriptor prevents Android's USB
                // service from entering accessory mode on the next attempt.
                close();
                listener.onConnectionChanged(false, "AOA connection lost");
            }
        } finally {
            synchronized (lifecycleLock) {
                if (generation == sessionGeneration) {
                    running = false;
                }
            }
        }
    }

    private boolean isSessionActive(int sessionGeneration) {
        return running && generation == sessionGeneration;
    }

    private void resetQueueReport() {
        reportMaxQueueAgeNanos = 0;
        reportMaxQueueDepth = 0;
        reportResyncCount = 0;
        reportQueueWarning = false;
        reportQueueFailsafe = false;
    }

    private void resetQueueStats() {
        maxQueueAgeNanos = 0;
        maxQueueDepth = 0;
        queueWarningCount = 0;
        queueResyncCount = 0;
        queueFailsafeCount = 0;
        coalescedMoveCount = 0;
        hostRecoveryCount = 0;
        lastQueueLogNanos = 0;
        queueWarningActive = false;
        hostAttachSeen = false;
        writeStartedNanos = 0;
        lastWriteCompletedNanos = 0;
        lastWriteDurationNanos = 0;
        incidentHead = 0;
        incidentTail = 0;
        clearActiveTouches();
        resetQueueReport();
    }

    void close() {
        Thread previousWriter;
        Thread previousControl;
        ParcelFileDescriptor previousDescriptor;
        FileInputStream previousInput;
        FileOutputStream previousOutput;
        long summaryMaxAgeNanos;
        int summaryMaxDepth;
        long summaryWarningCount;
        long summaryResyncCount;
        long summaryFailsafeCount;
        long summaryCoalescedMoves;
        long summaryHostRecoveries;
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
            summaryMaxAgeNanos = maxQueueAgeNanos;
            summaryMaxDepth = maxQueueDepth;
            summaryWarningCount = queueWarningCount;
            summaryResyncCount = queueResyncCount;
            summaryFailsafeCount = queueFailsafeCount;
            summaryCoalescedMoves = coalescedMoveCount;
            summaryHostRecoveries = hostRecoveryCount;
            head = 0;
            tail = 0;
            resetQueueStats();
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
        if (summaryMaxDepth > 0 || summaryHostRecoveries > 0) {
            Log.i(
                    TAG,
                    "Queue summary: maxAge="
                            + summaryMaxAgeNanos / 1_000_000.0
                            + " ms, maxDepth="
                            + summaryMaxDepth
                            + ", warnings="
                            + summaryWarningCount
                            + ", resyncs="
                            + summaryResyncCount
                            + ", failsafes="
                            + summaryFailsafeCount
                            + ", coalescedMoves="
                            + summaryCoalescedMoves
                            + ", hostRecoveries="
                            + summaryHostRecoveries
            );
        }
    }
}
