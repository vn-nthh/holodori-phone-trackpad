package dev.holodori.trackpad;

import android.content.SharedPreferences;
import java.util.Arrays;

/** Per-finger keyboard admission; physical touch snapshots remain intact. */
final class PressureFilter {
    static final String ENABLED = "pressure_filter_enabled";
    static final String CUTOFF = "pressure_filter_cutoff";
    static final float DEFAULT_CUTOFF = 0.05f;
    private final boolean enabled;
    private final float cutoff;
    private final boolean[] admitted = new boolean[256];
    private final boolean[] present = new boolean[256];

    PressureFilter(boolean enabled, float cutoff) {
        this.enabled = enabled;
        this.cutoff = normalize(cutoff);
    }

    static PressureFilter load(SharedPreferences preferences) {
        return new PressureFilter(preferences.getBoolean(ENABLED, false),
                preferences.getFloat(CUTOFF, DEFAULT_CUTOFF));
    }

    static float normalize(float pressure) {
        return Float.isNaN(pressure) ? 0f : Math.max(0f, Math.min(1f, pressure));
    }

    static boolean meetsCutoff(float pressure, float cutoff) {
        return normalize(pressure) >= cutoff;
    }

    // Called once per historical/current snapshot under the transport queue lock,
    // including while disconnected. Replays and heartbeats only read this state.
    void update(int action, int actionPointerId, int count, int[] ids,
                float[] pressure, boolean[] touching) {
        if (!enabled) return;
        if (action == TouchSample.ACTION_CANCEL) {
            Arrays.fill(admitted, false);
            return;
        }
        if (action == TouchSample.ACTION_DOWN) admitted[actionPointerId & 0xff] = false;
        Arrays.fill(present, false);
        for (int index = 0; index < count; index++) {
            int id = ids[index] & 0xff;
            present[id] = touching[index];
            admitted[id] = touching[index]
                    && (admitted[id] || meetsCutoff(pressure[index], cutoff));
        }
        for (int id = 0; id < admitted.length; id++) {
            if (!present[id]) admitted[id] = false;
        }
    }

    int contactFlags(int pointerId, boolean touching) {
        return enabled && touching && !admitted[pointerId & 0xff]
                ? TouchSample.CONTACT_FLAG_KEY_SUPPRESSED : 0;
    }
}
