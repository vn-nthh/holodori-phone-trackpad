package dev.holodori.trackpad;

import org.junit.Test;
import static org.junit.Assert.*;

public final class PressureFilterTest {
    @Test
    public void admissionIsImmediateThenLatchedAcrossHistoryAndHeartbeatReads() {
        PressureFilter filter = new PressureFilter(true, 0.05f);
        update(filter, TouchSample.ACTION_DOWN, 0.01f, true);
        assertSuppressed(filter, true);
        // Historical MOVE first crosses the threshold: no confirmation window.
        update(filter, TouchSample.ACTION_MOVE, 0.05f, true);
        assertSuppressed(filter, false);
        update(filter, TouchSample.ACTION_MOVE, 0f, true);
        assertSuppressed(filter, false);
        // Heartbeats/recovery encode the retained contact with the same decision.
        assertSuppressed(filter, false);
        update(filter, TouchSample.ACTION_UP, 1f, false);
        assertEquals(0, filter.contactFlags(7, false));
        update(filter, TouchSample.ACTION_DOWN, 0.01f, true);
        assertSuppressed(filter, true);
    }

    @Test
    public void fingersAreIndependentAndOmissionCancellationAndIdReuseResetAdmission() {
        PressureFilter filter = new PressureFilter(true, 0.1f);
        filter.update(TouchSample.ACTION_DOWN, 7, 2,
                new int[] {7, 8}, new float[] {0.5f, 0.01f}, new boolean[] {true, true});
        assertEquals(0, filter.contactFlags(7, true));
        assertEquals(TouchSample.CONTACT_FLAG_KEY_SUPPRESSED, filter.contactFlags(8, true));
        filter.update(TouchSample.ACTION_MOVE, 0, 1,
                new int[] {8}, new float[] {0.2f}, new boolean[] {true});
        update(filter, TouchSample.ACTION_MOVE, 0.01f, true);
        assertSuppressed(filter, true); // Omitted 7 did not keep its admission.
        update(filter, TouchSample.ACTION_MOVE, 0.2f, true);
        update(filter, TouchSample.ACTION_CANCEL, 0.2f, true);
        update(filter, TouchSample.ACTION_MOVE, 0.01f, true);
        assertSuppressed(filter, true);
        update(filter, TouchSample.ACTION_MOVE, 0.2f, true);
        update(filter, TouchSample.ACTION_DOWN, 0.01f, true);
        assertSuppressed(filter, true);
    }

    @Test
    public void disabledAndBoundaryValuesDoNotInventDeviceCalibration() {
        PressureFilter disabled = new PressureFilter(false, 1f);
        update(disabled, TouchSample.ACTION_DOWN, 0f, true);
        assertSuppressed(disabled, false);
        PressureFilter fixed = new PressureFilter(true, 1f);
        update(fixed, TouchSample.ACTION_DOWN, 1f, true);
        assertSuppressed(fixed, false);
        assertTrue(PressureFilter.meetsCutoff(2f, 1f));
        assertFalse(PressureFilter.meetsCutoff(Float.NaN, 0.1f));
        assertFalse(PressureFilter.meetsCutoff(-1f, 0.1f));
        assertTrue(PressureFilter.meetsCutoff(0f, 0f));
    }

    private static void update(PressureFilter filter, int action, float pressure, boolean touching) {
        filter.update(action, 7, 1, new int[] {7}, new float[] {pressure}, new boolean[] {touching});
    }

    private static void assertSuppressed(PressureFilter filter, boolean suppressed) {
        assertEquals(suppressed ? TouchSample.CONTACT_FLAG_KEY_SUPPRESSED : 0,
                filter.contactFlags(7, true));
    }
}
