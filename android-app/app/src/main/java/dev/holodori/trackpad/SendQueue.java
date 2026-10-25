package dev.holodori.trackpad;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;

/**
 * Bounded retained frames awaiting acknowledgement, in sequence order. Its owner
 * supplies synchronization and the monotonic clock.
 *
 * <p>Each frame is sent as an immediate pair (or once, if {@link Frame#singleCopy}), then
 * repaired every {@link #REPAIR_NANOS} until acknowledged. {@link #nextBatch} packs
 * already-queued consecutive frames in the same state into one datagram; it never waits
 * for more frames to arrive.
 */
final class SendQueue {
    static final long REPAIR_NANOS = 2_000_000L;
    static final int IMMEDIATE_COPIES = 2;

    static final class Frame {
        long sequence;
        final byte[] payload;
        final ByteBuffer writer;
        int payloadLength;
        long queuedNanos;
        long lastSentNanos;
        long firstSelectedNanos;
        long repairLatenessNanos;
        /** Immediate copies sent so far, plus one once repaired. */
        int sendCount;
        long sendAttempts;
        /** Sent once instead of as a pair; a lost copy is repaired like any other. */
        boolean singleCopy;
        /** The receiver reported holding this frame (selective ACK), so it is never repaired. */
        boolean held;

        Frame(int payloadCapacity) {
            payload = new byte[payloadCapacity];
            writer = ByteBuffer.wrap(payload).order(ByteOrder.LITTLE_ENDIAN);
        }
    }

    private final Frame[] frames;
    private long head;
    private long tail;
    /** First frame that may still owe an immediate copy. */
    private long immediate;
    /** Round-robin position among frames that have had their immediate copies. */
    private long repair;
    private boolean lastWasRepair;
    private long batchStart;
    private int batchSize;

    SendQueue(int capacity, int payloadCapacity) {
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
        frame.firstSelectedNanos = 0;
        frame.repairLatenessNanos = 0;
        frame.sendCount = 0;
        frame.sendAttempts = 0;
        frame.singleCopy = false;
        frame.held = false;
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
        batchSize = 0;
    }

    /** Selective ACK. Requires consecutive sequences, as gameplay frames have. */
    void markHeld(long sequence) {
        if (isEmpty()) return;
        long offset = sequence - at(head).sequence;
        if (offset >= 0 && offset < size()) at(head + offset).held = true;
    }

    /** The next single frame to send; quality probes are never batched. */
    Frame next(long now, int window) {
        return nextBatch(now, window, 1, Integer.MAX_VALUE);
    }

    /**
     * Selects and marks sent the next datagram's frames: up to {@code maxFrames}
     * consecutive frames totalling at most {@code maxBytes} of payload. Returns the
     * first, or null if nothing is due; {@link #batchFrame} returns the rest.
     */
    Frame nextBatch(long now, int window, int maxFrames, int maxBytes) {
        long end = Math.min(tail, head + window);
        advanceImmediate(end);
        Frame fresh = immediate < end ? at(immediate) : null;
        long retry = repairCandidate();
        // Keep each immediate pair contiguous. Between pairs, one due repair
        // gets priority so a missing old sequence cannot be starved by a burst.
        if (retry >= 0 && isRepairDue(at(retry), now)
                && (fresh == null || (!lastWasRepair && fresh.sendCount == 0))) {
            int count = 1;
            int bytes = at(retry).payloadLength;
            for (; count < maxFrames && retry + count < immediate; count++) {
                Frame next = at(retry + count);
                if (next.held || !isRepairDue(next, now) || bytes + next.payloadLength > maxBytes) {
                    break;
                }
                bytes += next.payloadLength;
            }
            repair = retry + count;
            lastWasRepair = true;
            return markBatch(retry, count, now);
        }
        if (fresh != null) {
            // A first copy batches with other first copies, a twin with other twins.
            int count = 1;
            int bytes = fresh.payloadLength;
            for (; count < maxFrames && immediate + count < end; count++) {
                Frame next = at(immediate + count);
                if (next.sendCount != fresh.sendCount || bytes + next.payloadLength > maxBytes) break;
                bytes += next.payloadLength;
            }
            lastWasRepair = false;
            return markBatch(immediate, count, now);
        }
        batchSize = 0;
        return null;
    }

    int batchSize() { return batchSize; }

    Frame batchFrame(int offset) { return at(batchStart + offset); }

    long nanosUntilSend(long now, int window) {
        if (isEmpty()) return Long.MAX_VALUE;
        long end = Math.min(tail, head + window);
        advanceImmediate(end);
        if (immediate < end) return 0;
        long retry = repairCandidate();
        if (retry < 0) return Long.MAX_VALUE;
        return Math.max(0L, REPAIR_NANOS - (now - at(retry).lastSentNanos));
    }

    private void advanceImmediate(long end) {
        while (immediate < end && at(immediate).sendCount >= IMMEDIATE_COPIES) immediate++;
    }

    /** The next frame in round-robin order that may need a repair, or -1. */
    private long repairCandidate() {
        if (repair >= immediate || repair < head) repair = head;
        for (long index = repair; index < immediate; index++) {
            if (!at(index).held) return index;
        }
        for (long index = head; index < repair; index++) {
            if (!at(index).held) return index;
        }
        return -1;
    }

    private static boolean isRepairDue(Frame frame, long now) {
        return now - frame.lastSentNanos >= REPAIR_NANOS;
    }

    private Frame markBatch(long start, int count, long now) {
        batchStart = start;
        batchSize = count;
        for (int offset = 0; offset < count; offset++) markSent(at(start + offset), now);
        return at(start);
    }

    private Frame at(long index) { return frames[(int) (index % frames.length)]; }

    private static void markSent(Frame frame, long now) {
        if (frame.sendCount == 0) frame.firstSelectedNanos = now;
        frame.repairLatenessNanos = frame.sendCount >= IMMEDIATE_COPIES
                ? Math.max(0L, now - frame.lastSentNanos - REPAIR_NANOS) : 0;
        frame.lastSentNanos = now;
        if (frame.sendAttempts < Long.MAX_VALUE) frame.sendAttempts++;
        if (frame.sendCount == 0 && frame.singleCopy) frame.sendCount = IMMEDIATE_COPIES;
        else if (frame.sendCount <= IMMEDIATE_COPIES) frame.sendCount++;
    }
}
