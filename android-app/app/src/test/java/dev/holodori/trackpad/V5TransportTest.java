package dev.holodori.trackpad;

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
}
