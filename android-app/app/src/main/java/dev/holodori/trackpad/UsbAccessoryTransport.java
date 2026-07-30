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
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

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
    private static final int HOST_CAP_TIMING_BREAKDOWN = 0x0001;
    private static final int HOST_CAP_MOTION_BATCH_DIAGNOSTICS = 0x0002;
    private static final int HOST_LANE_COUNT_SHIFT = 8;
    private static final long WRITER_READY_TIMEOUT_MILLIS = 1_000;
    private static final long TIMING_DURATION_UNIT_NANOS = 25_000L;
    private static final int TIMING_DURATION_MASK = 0x0FFF;
    private static final int TIMING_DISPATCH_SHIFT = 0;
    private static final int TIMING_APP_SHIFT = 12;
    private static final int TIMING_QUEUE_SHIFT = 24;
    private static final int TIMING_WRITE_SHIFT = 36;
    private static final long MOTION_HISTORY_SPAN_UNIT_NANOS = 25_000L;

    interface Listener {
        void onConnectionChanged(boolean connected, String message);
        void onHostLaneCountChanged(int laneCount);
    }

    private static final int CAPACITY = 1024;
    private static final int INCIDENT_CAPACITY = 256;
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
    private boolean hostTimingBreakdownEnabled;
    private boolean hostMotionBatchDiagnosticsEnabled;
    private int hostLaneCount = 6;
    private int nextIncidentToken = 1;
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
        CountDownLatch writerReady = new CountDownLatch(1);
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
                    () -> writerLoop(
                            sessionGeneration,
                            sessionOutput,
                            writerReady
                    ),
                    "AOA touch writer"
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
            listener.onConnectionChanged(
                    false,
                    "AOA connection interrupted"
            );
            return false;
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
            long eventNanos,
            long callbackNanos,
            int motionHistorySize,
            long motionHistorySpanNanos,
            int motionCrossedLaneCount
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
            long enqueuedNanos = System.nanoTime();

            int normalizedPointerId = pointerId & 0xFF;
            updateActiveTouchState(action, normalizedPointerId);
            if (action == TouchSample.ACTION_MOVE
                    && coalescePendingMove(
                            normalizedPointerId,
                            x,
                            y,
                            inside,
                            locked,
                            eventNanos,
                            callbackNanos,
                            enqueuedNanos,
                            motionHistorySize,
                            motionHistorySpanNanos,
                            motionCrossedLaneCount
                    )) {
                coalescedMoveCount++;
                recordQueueMetrics(enqueuedNanos);
                return;
            }

            int next = (head + 1) % CAPACITY;
            if (next == tail) {
                TouchSample delayedSample = queue[tail];
                resynchronizeQueue(
                        nowNanos,
                        queueAgeNanos(nowNanos),
                        queueDepth(),
                        true,
                        INCIDENT_REASON_CAPACITY,
                        "capacity",
                        delayedSample
                );
                next = (head + 1) % CAPACITY;
            }
            TouchSample sample = queue[head];
            sample.action = action;
            sample.pointerId = normalizedPointerId;
            sample.timingIncident = false;
            sample.incidentToken = 0;
            updateSample(
                    sample,
                    x,
                    y,
                    inside,
                    locked,
                    eventNanos,
                    callbackNanos,
                    enqueuedNanos,
                    motionHistorySize,
                    motionHistorySpanNanos,
                    motionCrossedLaneCount
            );
            head = next;
            recordQueueMetrics(enqueuedNanos);
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
        TouchSample delayedSample = queue[tail];
        recordQueueMetrics(ageNanos, depth);

        if (ageNanos >= MAX_QUEUE_AGE_NANOS) {
            resynchronizeQueue(
                    nowNanos,
                    ageNanos,
                    depth,
                    true,
                    INCIDENT_REASON_FAILSAFE,
                    "100 ms failsafe",
                    delayedSample
            );
        } else if (ageNanos >= QUEUE_RESYNC_AGE_NANOS) {
            resynchronizeQueue(
                    nowNanos,
                    ageNanos,
                    depth,
                    false,
                    INCIDENT_REASON_RESYNC,
                    "latency budget",
                    delayedSample
            );
        } else if (ageNanos >= QUEUE_WARNING_AGE_NANOS) {
            if (!queueWarningActive) {
                queueWarningCount++;
                recordQueueIncident(
                        nowNanos,
                        ageNanos,
                        depth,
                        INCIDENT_REASON_WARNING,
                        delayedSample
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
            String reason,
            TouchSample delayedSample
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
                nowNanos,
                ageNanos,
                depth,
                incidentReason,
                delayedSample
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
            int reason,
            TouchSample delayedSample
    ) {
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

        int token = 0;
        if (
                hostTimingBreakdownEnabled
                        || hostMotionBatchDiagnosticsEnabled
        ) {
            token = nextIncidentToken;
            nextIncidentToken = nextIncidentToken == 0xFF
                    ? 1
                    : nextIncidentToken + 1;
        }

        TouchSample incident = nextIncidentRecord();
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
        // A capable protocol-v2 host opts into the timing token. Older hosts
        // receive zero here and continue treating incidents exactly as before.
        incident.y = token;
        incident.eventNanos = (nowNanos & ~0xFFFFL) | detail;

        if (token == 0) {
            return;
        }
        if (reason == INCIDENT_REASON_WARNING) {
            delayedSample.timingIncident = true;
            delayedSample.incidentToken = token;
        } else {
            if (hostTimingBreakdownEnabled) {
                recordTimingBreakdown(
                        delayedSample,
                        nowNanos,
                        nowNanos,
                        0,
                        token
                );
            }
            if (hostMotionBatchDiagnosticsEnabled) {
                recordMotionBatch(
                        delayedSample, nowNanos, token
                );
            }
        }
    }

    private TouchSample nextIncidentRecord() {
        int next = (incidentHead + 1) % INCIDENT_CAPACITY;
        if (next == incidentTail) {
            incidentTail = (incidentTail + 1) % INCIDENT_CAPACITY;
        }
        TouchSample incident = incidentQueue[incidentHead];
        incidentHead = next;
        return incident;
    }

    private static long timingUnits(long durationNanos) {
        return Math.min(
                TIMING_DURATION_MASK,
                Math.max(0, durationNanos) / TIMING_DURATION_UNIT_NANOS
        );
    }

    private void recordTimingBreakdown(
            TouchSample sample,
            long dequeuedNanos,
            long writeCompletedNanos,
            long writeDurationNanos,
            int token
    ) {
        recordTimingBreakdown(
                sample.eventNanos,
                sample.callbackNanos,
                sample.enqueuedNanos,
                dequeuedNanos,
                writeCompletedNanos,
                writeDurationNanos,
                token
        );
    }

    private void recordTimingBreakdown(
            long eventNanos,
            long callbackNanos,
            long enqueuedNanos,
            long dequeuedNanos,
            long writeCompletedNanos,
            long writeDurationNanos,
            int token
    ) {
        long dispatchNanos = Math.max(
                0, callbackNanos - eventNanos
        );
        long appNanos = Math.max(
                0, enqueuedNanos - callbackNanos
        );
        long queueNanos = Math.max(
                0, dequeuedNanos - enqueuedNanos
        );
        long packed = (
                timingUnits(dispatchNanos) << TIMING_DISPATCH_SHIFT
        ) | (
                timingUnits(appNanos) << TIMING_APP_SHIFT
        ) | (
                timingUnits(queueNanos) << TIMING_QUEUE_SHIFT
        ) | (
                timingUnits(writeDurationNanos) << TIMING_WRITE_SHIFT
        );

        TouchSample timing = nextIncidentRecord();
        timing.action = TouchSample.ACTION_HEARTBEAT;
        timing.pointerId = token;
        timing.flags = TouchSample.FLAG_QUEUE_DIAGNOSTICS
                | TouchSample.FLAG_QUEUE_INCIDENT
                | TouchSample.FLAG_INCIDENT_TIMING_BREAKDOWN;
        timing.x = (int) (packed & 0xFFFFL);
        timing.y = (int) ((packed >> 16) & 0xFFFFL);
        timing.eventNanos = (
                writeCompletedNanos & ~0xFFFFL
        ) | ((packed >> 32) & 0xFFFFL);
    }

    private void recordMotionBatch(
            TouchSample sample, long reportNanos, int token
    ) {
        recordMotionBatch(
                sample.motionHistorySize,
                sample.motionHistorySpanNanos,
                sample.motionCrossedLaneCount,
                reportNanos,
                token
        );
    }

    private void recordMotionBatch(
            int historySize,
            long historySpanNanos,
            int crossedLaneCount,
            long reportNanos,
            int token
    ) {
        int spanUnits = (int) Math.min(
                0xFFFFL,
                Math.max(0, historySpanNanos)
                        / MOTION_HISTORY_SPAN_UNIT_NANOS
        );
        TouchSample motion = nextIncidentRecord();
        motion.action = TouchSample.ACTION_HEARTBEAT;
        motion.pointerId = token;
        motion.flags = TouchSample.FLAG_QUEUE_DIAGNOSTICS
                | TouchSample.FLAG_QUEUE_INCIDENT
                | TouchSample.FLAG_INCIDENT_MOTION_BATCH;
        motion.x = Math.min(
                Short.MAX_VALUE, historySize
        );
        motion.y = Math.min(
                Short.MAX_VALUE, crossedLaneCount
        );
        motion.eventNanos = (
                reportNanos & ~0xFFFFL
        ) | spanUnits;
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
            long eventNanos,
            long callbackNanos,
            long enqueuedNanos,
            int motionHistorySize,
            long motionHistorySpanNanos,
            int motionCrossedLaneCount
    ) {
        int lane = laneFor(x);
        int index = head;
        while (index != tail) {
            index = (index - 1 + CAPACITY) % CAPACITY;
            TouchSample pending = queue[index];
            if (pending.action != TouchSample.ACTION_MOVE) {
                break;
            }
            if (pending.pointerId == pointerId) {
                if (pending.timingIncident || pending.lane != lane) {
                    return false;
                }
                updateSample(
                        pending,
                        x,
                        y,
                        inside,
                        locked,
                        eventNanos,
                        callbackNanos,
                        enqueuedNanos,
                        motionHistorySize,
                        motionHistorySpanNanos,
                        motionCrossedLaneCount
                );
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
        cancel.callbackNanos = eventNanos;
        cancel.enqueuedNanos = eventNanos;
        cancel.motionHistorySpanNanos = 0;
        cancel.lane = -1;
        cancel.motionHistorySize = 0;
        cancel.motionCrossedLaneCount = 0;
        cancel.timingIncident = false;
        cancel.incidentToken = 0;
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
        reset.callbackNanos = eventNanos;
        reset.enqueuedNanos = eventNanos;
        reset.motionHistorySpanNanos = 0;
        reset.lane = -1;
        reset.motionHistorySize = 0;
        reset.motionCrossedLaneCount = 0;
        reset.timingIncident = false;
        reset.incidentToken = 0;
        head = (head + 1) % CAPACITY;
    }

    private void updateSample(
            TouchSample sample,
            float x,
            float y,
            boolean inside,
            boolean locked,
            long eventNanos,
            long callbackNanos,
            long enqueuedNanos,
            int motionHistorySize,
            long motionHistorySpanNanos,
            int motionCrossedLaneCount
    ) {
        sample.flags =
                (inside ? TouchSample.FLAG_INSIDE : 0)
                        | (locked ? TouchSample.FLAG_LOCKED : 0);
        sample.x = clampFixed(x);
        sample.y = clampFixed(y);
        sample.eventNanos = eventNanos;
        sample.callbackNanos = callbackNanos;
        sample.enqueuedNanos = enqueuedNanos;
        sample.motionHistorySpanNanos = Math.max(
                0, motionHistorySpanNanos
        );
        sample.lane = laneFor(x);
        sample.motionHistorySize = Math.max(0, motionHistorySize);
        sample.motionCrossedLaneCount = Math.max(
                0, motionCrossedLaneCount
        );
    }

    private int laneFor(float x) {
        return Math.max(
                0,
                Math.min(hostLaneCount - 1, (int) (x * hostLaneCount))
        );
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
                    int capabilities = (control[6] & 0xFF)
                            | ((control[7] & 0xFF) << 8);
                    handleHostAttach(sessionGeneration, capabilities);
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

    private void handleHostAttach(
            int sessionGeneration, int capabilities
    ) {
        boolean recovered;
        int requestedLaneCount = (
                capabilities >> HOST_LANE_COUNT_SHIFT
        ) & 0xFF;
        int configuredLaneCount = (
                requestedLaneCount >= 1 && requestedLaneCount <= 16
        ) ? requestedLaneCount : 6;
        synchronized (queueLock) {
            if (!isSessionActive(sessionGeneration)) {
                return;
            }
            recovered = hostAttachSeen;
            hostAttachSeen = true;
            hostTimingBreakdownEnabled = (
                    capabilities & HOST_CAP_TIMING_BREAKDOWN
            ) != 0;
            hostMotionBatchDiagnosticsEnabled = (
                    capabilities & HOST_CAP_MOTION_BATCH_DIAGNOSTICS
            ) != 0;
            hostLaneCount = configuredLaneCount;
            if (recovered) {
                hostRecoveryCount++;
                replaceQueueWithSessionReset(System.nanoTime(), true);
                queueLock.notifyAll();
            }
        }
        if (recovered) {
            Log.w(TAG, "New host process attached; cleared stale touch queue");
        }
        listener.onHostLaneCountChanged(configuredLaneCount);
    }

    private void writerLoop(
            int sessionGeneration,
            FileOutputStream sessionOutput,
            CountDownLatch writerReady
    ) {
        Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_DISPLAY);
        ByteBuffer packet = ByteBuffer.allocate(24).order(ByteOrder.LITTLE_ENDIAN);
        writerReady.countDown();
        try {
            while (isSessionActive(sessionGeneration)) {
                int action;
                int pointerId;
                int flags;
                int x;
                int y;
                int currentSequence;
                long eventNanos;
                boolean emitTimingBreakdown = false;
                int timingToken = 0;
                long timingEventNanos = 0;
                long timingCallbackNanos = 0;
                long timingEnqueuedNanos = 0;
                long timingDequeuedNanos = 0;
                int timingMotionHistorySize = 0;
                long timingMotionHistorySpanNanos = 0;
                int timingMotionCrossedLaneCount = 0;

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
                        long pressureCheckNanos = System.nanoTime();
                        checkQueuePressure(pressureCheckNanos);
                        timingDequeuedNanos = System.nanoTime();
                        TouchSample sample = queue[tail];
                        action = sample.action;
                        pointerId = sample.pointerId;
                        flags = sample.flags;
                        x = sample.x;
                        y = sample.y;
                        currentSequence = sequence++;
                        eventNanos = sample.eventNanos;
                        emitTimingBreakdown = sample.timingIncident
                                && (
                                hostTimingBreakdownEnabled
                                        || hostMotionBatchDiagnosticsEnabled
                        );
                        if (emitTimingBreakdown) {
                            timingToken = sample.incidentToken;
                            timingEventNanos = sample.eventNanos;
                            timingCallbackNanos = sample.callbackNanos;
                            timingEnqueuedNanos = sample.enqueuedNanos;
                            timingMotionHistorySize =
                                    sample.motionHistorySize;
                            timingMotionHistorySpanNanos =
                                    sample.motionHistorySpanNanos;
                            timingMotionCrossedLaneCount =
                                    sample.motionCrossedLaneCount;
                            sample.timingIncident = false;
                            sample.incidentToken = 0;
                        }
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
                long completedNanos;
                long writeDurationNanos;
                try {
                    sessionOutput.write(packet.array());
                } finally {
                    completedNanos = System.nanoTime();
                    writeDurationNanos = Math.max(
                            0, completedNanos - startedNanos
                    );
                    lastWriteDurationNanos = writeDurationNanos;
                    lastWriteCompletedNanos = completedNanos;
                    writeStartedNanos = 0;
                }
                if (emitTimingBreakdown) {
                    synchronized (queueLock) {
                        if (isSessionActive(sessionGeneration)) {
                            if (hostTimingBreakdownEnabled) {
                                recordTimingBreakdown(
                                        timingEventNanos,
                                        timingCallbackNanos,
                                        timingEnqueuedNanos,
                                        timingDequeuedNanos,
                                        completedNanos,
                                        writeDurationNanos,
                                        timingToken
                                );
                            }
                            if (hostMotionBatchDiagnosticsEnabled) {
                                recordMotionBatch(
                                        timingMotionHistorySize,
                                        timingMotionHistorySpanNanos,
                                        timingMotionCrossedLaneCount,
                                        completedNanos,
                                        timingToken
                                );
                            }
                            queueLock.notify();
                        }
                    }
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
        hostTimingBreakdownEnabled = false;
        hostMotionBatchDiagnosticsEnabled = false;
        hostLaneCount = 6;
        nextIncidentToken = 1;
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
