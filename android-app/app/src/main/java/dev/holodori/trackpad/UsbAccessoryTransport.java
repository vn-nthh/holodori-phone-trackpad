package dev.holodori.trackpad;

import android.hardware.usb.UsbAccessory;
import android.hardware.usb.UsbManager;
import android.os.ParcelFileDescriptor;
import android.os.Process;
import android.util.Log;

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

    interface Listener {
        void onConnectionChanged(boolean connected, String message);
    }

    private static final int CAPACITY = 1024;
    private final Object queueLock = new Object();
    private final Object lifecycleLock = new Object();
    private final TouchSample[] queue = new TouchSample[CAPACITY];
    private final Listener listener;

    private int head;
    private int tail;
    private int sequence;
    private long maxQueueAgeNanos;
    private int maxQueueDepth;
    private long queueWarningCount;
    private long queueResyncCount;
    private long queueFailsafeCount;
    private long coalescedMoveCount;
    private long lastQueueLogNanos;
    private long reportMaxQueueAgeNanos;
    private int reportMaxQueueDepth;
    private int reportResyncCount;
    private boolean reportQueueWarning;
    private boolean reportQueueFailsafe;
    private boolean queueWarningActive;
    private volatile int generation;
    private volatile boolean running;
    private ParcelFileDescriptor descriptor;
    private FileOutputStream output;
    private Thread writerThread;

    UsbAccessoryTransport(Listener listener) {
        this.listener = listener;
        for (int index = 0; index < CAPACITY; index++) {
            queue[index] = new TouchSample();
        }
    }

    boolean open(UsbManager manager, UsbAccessory accessory) {
        close();
        ParcelFileDescriptor nextDescriptor = manager.openAccessory(accessory);
        if (nextDescriptor == null) {
            listener.onConnectionChanged(false, "USB permission was not granted");
            return false;
        }

        synchronized (lifecycleLock) {
            descriptor = nextDescriptor;
            output = new FileOutputStream(descriptor.getFileDescriptor());
            running = true;
            int sessionGeneration = ++generation;
            FileOutputStream sessionOutput = output;
            writerThread = new Thread(
                    () -> writerLoop(sessionGeneration, sessionOutput),
                    "AOA touch writer"
            );
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
                    nowNanos, ageNanos, depth, true, "100 ms failsafe"
            );
        } else if (ageNanos >= QUEUE_RESYNC_AGE_NANOS) {
            resynchronizeQueue(
                    nowNanos, ageNanos, depth, false, "latency budget"
            );
        } else if (ageNanos >= QUEUE_WARNING_AGE_NANOS) {
            if (!queueWarningActive) {
                queueWarningCount++;
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
                    if (isSessionActive(sessionGeneration) && head == tail) {
                        queueLock.wait(HEARTBEAT_MILLIS);
                    }
                    if (!isSessionActive(sessionGeneration)) {
                        break;
                    }
                    if (head == tail) {
                        queueWarningActive = false;
                        action = TouchSample.ACTION_HEARTBEAT;
                        pointerId = Math.min(0xFF, reportMaxQueueDepth);
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
                packet.put((byte) 1);
                packet.put((byte) action);
                packet.put((byte) pointerId);
                packet.put((byte) flags);
                packet.putShort((short) x);
                packet.putShort((short) y);
                packet.putInt(currentSequence);
                packet.putLong(eventNanos);
                sessionOutput.write(packet.array());
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
        lastQueueLogNanos = 0;
        queueWarningActive = false;
        resetQueueReport();
    }

    void close() {
        Thread previousWriter;
        ParcelFileDescriptor previousDescriptor;
        FileOutputStream previousOutput;
        long summaryMaxAgeNanos;
        int summaryMaxDepth;
        long summaryWarningCount;
        long summaryResyncCount;
        long summaryFailsafeCount;
        long summaryCoalescedMoves;
        synchronized (lifecycleLock) {
            running = false;
            generation++;
            previousWriter = writerThread;
            previousDescriptor = descriptor;
            previousOutput = output;
            writerThread = null;
            descriptor = null;
            output = null;
        }
        synchronized (queueLock) {
            summaryMaxAgeNanos = maxQueueAgeNanos;
            summaryMaxDepth = maxQueueDepth;
            summaryWarningCount = queueWarningCount;
            summaryResyncCount = queueResyncCount;
            summaryFailsafeCount = queueFailsafeCount;
            summaryCoalescedMoves = coalescedMoveCount;
            head = 0;
            tail = 0;
            resetQueueStats();
            queueLock.notifyAll();
        }
        if (previousWriter != null
                && previousWriter != Thread.currentThread()) {
            previousWriter.interrupt();
        }
        try {
            if (previousOutput != null) previousOutput.close();
        } catch (IOException ignored) {
        }
        try {
            if (previousDescriptor != null) previousDescriptor.close();
        } catch (IOException ignored) {
        }
        if (summaryMaxDepth > 0) {
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
            );
        }
    }
}
