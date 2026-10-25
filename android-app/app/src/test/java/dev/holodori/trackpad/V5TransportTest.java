package dev.holodori.trackpad;

import static org.junit.Assert.assertEquals;
import static org.junit.Assert.assertFalse;
import static org.junit.Assert.assertTrue;

import org.junit.Test;

public final class V5TransportTest {
    @Test
    public void gameplayBacklogExpiresAtSixtyFourMilliseconds() {
        long queued = 1_000_000_000L;
        assertFalse(V5Transport.gameplayBacklogExpired(queued, queued + 63_999_999L));
        assertTrue(V5Transport.gameplayBacklogExpired(queued, queued + 64_000_000L));
    }

    @Test
    public void rememberedIkRetryStartsFastAndBacksOffToDiscoveryPace() {
        long[] expected = {8_000_000L, 16_000_000L, 32_000_000L, 64_000_000L,
                100_000_000L, 100_000_000L};
        long interval = 4_000_000L;
        for (long next : expected) {
            interval = V5Transport.nextRememberedRetry(interval);
            assertEquals(next, interval);
        }
    }
}
