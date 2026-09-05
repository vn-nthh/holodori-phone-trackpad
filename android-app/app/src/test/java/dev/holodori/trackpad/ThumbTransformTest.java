package dev.holodori.trackpad;

import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertTrue;

import org.junit.Test;

public final class ThumbTransformTest {
    private static final float EPSILON = 0.00001f;

    @Test
    public void capturedCrossingIsContinuousAndMonotonic() {
        float gap = 0.18f;
        float previous = Float.NEGATIVE_INFINITY;
        for (int index = 0; index <= 1_000; index++) {
            float physical = index / 1_000f;
            float logical = ThumbTransform.mapCapturedX(physical, gap);
            assertTrue(logical + EPSILON >= previous);
            previous = logical;
        }
        assertEquals(0f, ThumbTransform.mapCapturedX(0f, gap), EPSILON);
        assertEquals(0.5f, ThumbTransform.mapCapturedX(0.5f, gap), EPSILON);
        assertEquals(1f, ThumbTransform.mapCapturedX(1f, gap), EPSILON);
    }

    @Test
    public void newGapDownOwnsNoLaneButClustersKeepAllSix() {
        float gap = 0.14f;
        assertEquals(0, ThumbTransform.laneAtPhysicalX(0.5f, gap));
        assertEquals(1, ThumbTransform.laneAtPhysicalX(0f, gap));
        assertEquals(3, ThumbTransform.laneAtPhysicalX(
                ThumbTransform.leftEnd(gap) - 0.001f,
                gap
        ));
        assertEquals(4, ThumbTransform.laneAtPhysicalX(
                ThumbTransform.rightStart(gap) + 0.001f,
                gap
        ));
        assertEquals(6, ThumbTransform.laneAtPhysicalX(0.999f, gap));
    }
}
