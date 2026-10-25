package dev.holodori.trackpad;

import static org.junit.Assert.*;
import org.junit.Test;

public final class SendQueueTest {
    @Test
    public void immediatePairAndTwoMillisecondRepairUseTheSameFrame() {
        SendQueue queue = new SendQueue(4, 32);
        SendQueue.Frame frame = queue.add(9, 8, 100);
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
        SendQueue queue = new SendQueue(128, 16);
        SendQueue.Frame old = queue.add(0, 8, 0);
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
        SendQueue queue = new SendQueue(4, 16);
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
    public void queuedFramesShareEachCopyAndEachRepair() {
        SendQueue queue = new SendQueue(8, 64);
        for (int sequence = 0; sequence < 3; sequence++) queue.add(sequence, 40, 0);
        assertEquals(0, queue.nextBatch(0, 8, 16, 1_000).sequence);
        assertEquals(3, queue.batchSize());
        assertEquals(2, queue.batchFrame(2).sequence);
        assertEquals(0, queue.nextBatch(0, 8, 16, 1_000).sequence);
        assertEquals(3, queue.batchSize());
        assertNull(queue.nextBatch(1_999_999, 8, 16, 1_000));
        assertEquals(0, queue.nextBatch(2_000_000, 8, 16, 1_000).sequence);
        assertEquals(3, queue.batchSize());
    }

    @Test
    public void aTwinNeverAbsorbsANewFirstCopy() {
        SendQueue queue = new SendQueue(8, 64);
        queue.add(0, 40, 0);
        queue.add(1, 40, 0);
        queue.nextBatch(0, 8, 16, 1_000);
        queue.add(2, 40, 0);
        assertEquals(0, queue.nextBatch(0, 8, 16, 1_000).sequence);
        assertEquals(2, queue.batchSize());
        assertEquals(2, queue.nextBatch(0, 8, 16, 1_000).sequence);
        assertEquals(1, queue.batchSize());
    }

    @Test
    public void batchesRespectFrameByteAndWindowLimits() {
        SendQueue queue = new SendQueue(8, 64);
        for (int sequence = 0; sequence < 6; sequence++) queue.add(sequence, 40, 0);
        queue.nextBatch(0, 8, 2, 1_000);
        assertEquals(2, queue.batchSize());
        queue.nextBatch(0, 8, 16, 80);
        assertEquals(2, queue.batchSize());
        queue.nextBatch(0, 3, 16, 1_000);
        assertEquals("the window ends the batch", 1, queue.batchSize());
    }

    @Test
    public void framesTheHostHoldsAreNeverRepaired() {
        SendQueue queue = new SendQueue(8, 64);
        for (int sequence = 0; sequence < 3; sequence++) queue.add(sequence, 40, 0);
        queue.nextBatch(0, 8, 16, 1_000);
        queue.nextBatch(0, 8, 16, 1_000);
        queue.markHeld(1);
        assertEquals(0, queue.nextBatch(2_000_000, 8, 16, 1_000).sequence);
        assertEquals("a held frame ends the repair batch", 1, queue.batchSize());
        assertEquals(2, queue.nextBatch(2_000_000, 8, 16, 1_000).sequence);
        assertEquals(1, queue.batchSize());
        queue.markHeld(0);
        queue.markHeld(2);
        assertNull(queue.nextBatch(10_000_000, 8, 16, 1_000));
        assertEquals(Long.MAX_VALUE, queue.nanosUntilSend(10_000_000, 8));
    }

    @Test
    public void aSingleCopyFrameSendsOnceThenRepairsNormally() {
        SendQueue queue = new SendQueue(4, 64);
        SendQueue.Frame heartbeat = queue.add(5, 40, 0);
        heartbeat.singleCopy = true;
        assertSame(heartbeat, queue.next(0, 4));
        assertNull(queue.next(0, 4));
        assertEquals(2_000_000, queue.nanosUntilSend(0, 4));
        assertSame(heartbeat, queue.next(2_000_000, 4));
    }

    @Test
    public void qualityProbeUsesTheGameplayRepairDeadline() {
        SendQueue queue = new SendQueue(4, 32);
        SendQueue.Frame probe = queue.add(4, 32, 100);
        probe.sendCount = SendQueue.IMMEDIATE_COPIES;
        probe.lastSentNanos = 100;
        assertNull(queue.next(2_000_099, 4));
        assertSame(probe, queue.next(2_000_100, 4));
    }
}
