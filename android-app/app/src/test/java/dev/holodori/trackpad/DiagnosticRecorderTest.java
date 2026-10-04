package dev.holodori.trackpad;

import org.junit.Test;
import static org.junit.Assert.*;

public final class DiagnosticRecorderTest {
    @Test public void saturatedPublicationDoesNotOverwriteUnreadEvidence() {
        DiagnosticRecorder.Ring ring = new DiagnosticRecorder.Ring();
        for (int i = 0; i < DiagnosticRecorder.CAPACITY + 3; i++) ring.offer(20, i, 7, i, i, i, i, i, i, i, i, i);
        assertEquals(3, ring.dropped);
        long[] row = new long[12];
        for (int i = 0; i < DiagnosticRecorder.CAPACITY; i++) {
            assertTrue(ring.poll(row));
            assertEquals(i, row[3]);
            assertEquals(i, row[11]);
        }
        assertFalse(ring.poll(row));
    }

    @Test public void concurrentWrapPreservesEveryField() throws Exception {
        DiagnosticRecorder.Ring ring = new DiagnosticRecorder.Ring();
        Thread producer = new Thread(() -> {
            for (long i = 0; i < 100_000; i++) {
                while (ring.write - ring.read == DiagnosticRecorder.CAPACITY) Thread.yield();
                ring.offer(i, i, i, i, i, i, i, i, i, i, i, i);
            }
        });
        producer.start();
        long[] row = new long[12];
        for (long i = 0; i < 100_000; i++) {
            while (!ring.poll(row)) Thread.yield();
            for (long value : row) assertEquals(i, value);
        }
        producer.join();
    }

    @Test public void historicalAckTailAndDiscardSurviveHealthyCurrentTraffic() {
        DiagnosticRecorder recorder = new DiagnosticRecorder();
        recorder.queue.offer(20, 20_000_000, 7, 1, 1, 19_000_001, 3, 1, 64, 0, 0, 0);
        recorder.queue.offer(23, 22_000_000, 7, 1, 20_000_000, 1, 19_000_001, 3, 20_001_000, 3, 1, 1);
        recorder.queue.offer(24, 23_000_000, 7, 2, 21_000_000, 20_000_000, 20_000_000, 1, 0, 0, 1, 1);
        recorder.drain();
        assertEquals(1, recorder.statistics[1][0]);
        assertEquals(1, recorder.statistics[6][3]);
        assertEquals(0, recorder.statistics[5][0]);
        assertEquals(1, recorder.counts[24]);
        assertEquals(1, recorder.incidents);
        assertEquals(0, recorder.incident[6]); // latest discard has not recovered
    }

    @Test public void histogramHasExactThresholdAndNoHalfSecondCeiling() {
        DiagnosticRecorder recorder = new DiagnosticRecorder();
        recorder.sample(0, 8_333_334, 1);
        recorder.sample(0, 8_333_335, 1);
        recorder.sample(0, 900_000_001, 1);
        assertEquals(2, recorder.statistics[0][3]);
        assertEquals(900_000_000, recorder.statistics[0][2]);
    }

    @Test public void incidentStorageAndContinuousFaultAreBounded() {
        DiagnosticRecorder recorder = new DiagnosticRecorder();
        long[] row = {28, 1, 7, 1, 0, 0, 0, 0, 0, 0, 0, 0};
        for (int i = 0; i < 100_000; i++) { row[1]++; recorder.analyze(row); }
        assertEquals(1, recorder.incidents);
        assertEquals(DiagnosticRecorder.PER_INCIDENT, recorder.evidenceSize);
        assertTrue(recorder.omittedEvidence > 0);
        for (int i = 0; i < 1000; i++) { row[1] += 100_000_000; recorder.analyze(row); }
        assertEquals(DiagnosticRecorder.MAX_INCIDENTS, recorder.incidents);
        assertTrue(recorder.omittedIncidents > 0);
    }

    @Test public void actualQueueRepairsHaveAccurateCountAndDeadlineLateness() {
        V5SendQueue queue = new V5SendQueue(64, 64);
        V5SendQueue.Frame frame = queue.add(1, 44, 1);
        queue.next(100, 64);
        queue.next(200, 64);
        queue.next(2_100_200, 64);
        assertEquals(3, frame.sendAttempts);
        assertEquals(100_000, frame.repairLatenessNanos);
        queue.next(4_200_200, 64);
        assertEquals(4, frame.sendAttempts);
        assertEquals(100, frame.firstSelectedNanos);
    }

    @Test public void persistentEnvironmentConstraintOpensOneIncident() {
        DiagnosticRecorder recorder = new DiagnosticRecorder();
        long[] row = new long[DiagnosticRecorder.WIDTH];
        // Ten minutes of 2 s samples with power save on and no low-latency Wi-Fi lock.
        for (int n = 0; n < 300; n++) {
            java.util.Arrays.fill(row, -1);
            row[0] = DiagnosticRecorder.ENVIRONMENT;
            row[1] = n * 2_000_000_000L;
            row[2] = row[3] = 0;
            row[4] = 0; row[5] = 1; row[6] = 1; row[7] = 1; row[8] = 0;
            recorder.analyze(row);
        }
        assertEquals(1, recorder.incidents);
        assertEquals(300, recorder.constrainedEnvironmentSamples);
        // A newly appearing condition is still reported, and clearing re-arms it.
        row[1] += 2_000_000_000L; row[4] = 3;
        recorder.analyze(row);
        assertEquals(2, recorder.incidents);
        row[1] += 2_000_000_000L; row[4] = 0; row[5] = 0; row[8] = 1;
        recorder.analyze(row);
        row[1] += 2_000_000_000L; row[5] = 1;
        recorder.analyze(row);
        assertEquals(3, recorder.incidents);
        assertEquals(0, recorder.omittedIncidents);
    }
}
