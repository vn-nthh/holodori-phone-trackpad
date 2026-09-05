package dev.holodori.trackpad;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;

/** Bounded retained frames. Its owner supplies synchronization and the monotonic clock. */
final class V5SendQueue {
    static final long REPAIR_NANOS = 2_000_000L;
    static final int IMMEDIATE_COPIES = 2;

    static final class Frame {
        long sequence;
        final byte[] payload;
        final ByteBuffer writer;
        int payloadLength;
        long queuedNanos;
        long lastSentNanos;
        int sendCount;

        Frame(int payloadCapacity) {
            payload = new byte[payloadCapacity];
            writer = ByteBuffer.wrap(payload).order(ByteOrder.LITTLE_ENDIAN);
        }
    }

    private final Frame[] frames;
    private long head;
    private long tail;
    private long immediate;
    private long repair;
    private boolean lastWasRepair;

    V5SendQueue(int capacity, int payloadCapacity) {
        frames = new Frame[capacity];
        for (int index = 0; index < capacity; index++) frames[index] = new Frame(payloadCapacity);
    }

    int size() { return (int) (tail - head); }
    boolean isEmpty() { return head == tail; }
    Frame peekFirst() { return isEmpty() ? null : at(head); }
    Frame peekLast() { return isEmpty() ? null : at(tail - 1); }

    Frame add(long sequence, int payloadLength, long now) {
        if (size() == frames.length) return null;
        Frame frame = at(tail++);
        frame.sequence = sequence;
        frame.payloadLength = payloadLength;
        frame.queuedNanos = now;
        frame.lastSentNanos = 0;
        frame.sendCount = 0;
        frame.writer.clear();
        return frame;
    }

    void removeFirst() {
        if (!isEmpty()) head++;
        immediate = Math.max(head, immediate);
        repair = Math.max(head, repair);
    }

    void clear() {
        head = tail = immediate = repair = 0;
        lastWasRepair = false;
    }

    Frame next(long now, int window) {
        long end = Math.min(tail, head + window);
        while (immediate < end && at(immediate).sendCount >= IMMEDIATE_COPIES) immediate++;
        if (repair >= immediate || repair < head) repair = head;
        Frame fresh = immediate < end ? at(immediate) : null;
        Frame retry = repair < immediate ? at(repair) : null;
        // Keep each immediate pair contiguous. Between pairs, one due repair
        // gets priority so a missing old sequence cannot be starved by a burst.
        if (retry != null && now - retry.lastSentNanos >= REPAIR_NANOS
                && (fresh == null || (!lastWasRepair && fresh.sendCount == 0))) {
            repair++;
            lastWasRepair = true;
            return markSent(retry, now);
        }
        if (fresh != null) {
            lastWasRepair = false;
            return markSent(fresh, now);
        }
        return null;
    }

    long nanosUntilSend(long now, int window) {
        if (isEmpty()) return Long.MAX_VALUE;
        long end = Math.min(tail, head + window);
        if (immediate < end && at(immediate).sendCount < IMMEDIATE_COPIES) return 0;
        long nextRepair = repair >= immediate || repair < head ? head : repair;
        return Math.max(0L, REPAIR_NANOS - (now - at(nextRepair).lastSentNanos));
    }

    private Frame at(long index) { return frames[(int) (index % frames.length)]; }

    private static Frame markSent(Frame frame, long now) {
        frame.lastSentNanos = now;
        if (frame.sendCount <= IMMEDIATE_COPIES) frame.sendCount++;
        return frame;
    }
}
