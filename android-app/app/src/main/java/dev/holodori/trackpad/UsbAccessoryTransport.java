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
            int next = (head + 1) % CAPACITY;
            if (next == tail) {
                if (action == TouchSample.ACTION_MOVE) {
                    // Movement is state, not an edge. A later MOVE supersedes it.
                    return;
                }
                // Never lose DOWN/UP. Reset queued state first so the PC cannot
                // retain a key if USB was blocked for several seconds.
                tail = head;
                TouchSample cancel = queue[head];
                cancel.action = TouchSample.ACTION_CANCEL;
                cancel.pointerId = 0;
                cancel.flags = 0;
                cancel.x = 0;
                cancel.y = 0;
                cancel.sequence = sequence++;
                cancel.eventNanos = eventNanos;
                head = (head + 1) % CAPACITY;
                next = (head + 1) % CAPACITY;
            }
            TouchSample sample = queue[head];
            sample.action = action;
            sample.pointerId = pointerId & 0xFF;
            sample.flags =
                    (inside ? TouchSample.FLAG_INSIDE : 0)
                            | (locked ? TouchSample.FLAG_LOCKED : 0);
            sample.x = clampFixed(x);
            sample.y = clampFixed(y);
            sample.sequence = sequence++;
            sample.eventNanos = eventNanos;
            head = next;
            queueLock.notify();
        }
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
                        action = TouchSample.ACTION_HEARTBEAT;
                        pointerId = 0;
                        flags = 0;
                        x = 0;
                        y = 0;
                        currentSequence = sequence++;
                        eventNanos = System.nanoTime();
                    } else {
                        TouchSample sample = queue[tail];
                        action = sample.action;
                        pointerId = sample.pointerId;
                        flags = sample.flags;
                        x = sample.x;
                        y = sample.y;
                        currentSequence = sample.sequence;
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

    void close() {
        Thread previousWriter;
        ParcelFileDescriptor previousDescriptor;
        FileOutputStream previousOutput;
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
            head = 0;
            tail = 0;
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
    }
}
