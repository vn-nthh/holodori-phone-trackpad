package dev.holodori.trackpad;

import static org.junit.Assert.*;
import org.junit.Test;

public final class V5SendQueueTest {
    @Test
    public void immediatePairAndTwoMillisecondRepairUseTheSameFrame() {
        V5SendQueue queue = new V5SendQueue(4, 32);
        V5SendQueue.Frame frame = queue.add(9, 8, 100);
        frame.writer.putLong(123);
        assertSame(frame, queue.next(100, 4));
        assertSame(frame, queue.next(101, 4));
        assertNull(queue.next(2_000_100L, 4));
        assertEquals(1, queue.nanosUntilSend(2_000_100L, 4));
        assertSame(frame, queue.next(2_000_101L, 4));
        assertEquals(123, frame.writer.getLong(0));
    }

    @Test
    public void oldGapRepairCannotBeStarvedByNewImmediatePairs() {
        V5SendQueue queue = new V5SendQueue(128, 16);
        V5SendQueue.Frame old = queue.add(0, 8, 0);
        queue.next(0, 128);
        queue.next(0, 128);
        for (int sequence = 1; sequence < 128; sequence++) queue.add(sequence, 8, 2_000_000);
        assertSame(old, queue.next(2_000_000, 128));
        assertEquals(1, queue.next(2_000_000, 128).sequence);
        assertEquals(1, queue.next(4_000_000, 128).sequence);
        // Even when a repair becomes due between the two copies, the pair stays contiguous.
        assertSame(old, queue.next(4_000_000, 128));
        assertEquals(2, queue.next(4_000_000, 128).sequence);
    }

    @Test
    public void windowCapacityAcknowledgementAndRingReusePreserveOrder() {
        V5SendQueue queue = new V5SendQueue(4, 16);
        for (int round = 0; round < 1_000; round++) {
            for (int index = 0; index < 4; index++) assertNotNull(queue.add(round * 4L + index, 8, 0));
            assertNull(queue.add(99, 8, 0));
            assertEquals(round * 4L, queue.next(0, 1).sequence);
            assertEquals(round * 4L, queue.next(0, 1).sequence);
            assertNull(queue.next(0, 1));
            for (int index = 0; index < 4; index++) queue.removeFirst();
            assertTrue(queue.isEmpty());
        }
        queue.clear();
        assertEquals(Long.MAX_VALUE, queue.nanosUntilSend(0, 4));
    }

    @Test
    public void qualityProbeUsesTheGameplayRepairDeadline() {
        V5SendQueue queue = new V5SendQueue(4, 32);
        V5SendQueue.Frame probe = queue.add(4, 32, 100);
        probe.sendCount = V5SendQueue.IMMEDIATE_COPIES;
        probe.lastSentNanos = 100;
        assertNull(queue.next(2_000_099, 4));
        assertSame(probe, queue.next(2_000_100, 4));
    }
}
