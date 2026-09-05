package dev.holodori.trackpad;

/** Stateless physical-to-logical X transform shared by pairing and gameplay. */
final class ThumbTransform {
    static final float DEFAULT_GAP = 0.14f;
    static final float MIN_GAP = 0.04f;
    static final float MAX_GAP = 0.30f;

    private ThumbTransform() {
    }

    static float clampGap(float gap) {
        return Math.max(MIN_GAP, Math.min(MAX_GAP, gap));
    }

    static float leftEnd(float gap) {
        return 0.5f - clampGap(gap) / 2f;
    }

    static float rightStart(float gap) {
        return 0.5f + clampGap(gap) / 2f;
    }

    static boolean isInGap(float physicalX, float gap) {
        return physicalX > leftEnd(gap) && physicalX < rightStart(gap);
    }

    /**
     * Maps a pointer that already owns a cluster. The flat center bridge is
     * continuous and non-decreasing, so a lane-3 to lane-4 crossing cannot
     * disappear even when Android reports only the far side of the gap.
     */
    static float mapCapturedX(float physicalX, float gap) {
        float left = leftEnd(gap);
        float right = rightStart(gap);
        if (physicalX <= left) {
            return Math.min(0.499999f, physicalX * 0.5f / left);
        }
        if (physicalX >= right) return 0.5f + (physicalX - right) * 0.5f / (1f - right);
        return 0.5f;
    }

    static int laneAtPhysicalX(float physicalX, float gap) {
        if (isInGap(physicalX, gap)) return 0;
        float left = leftEnd(gap);
        float right = rightStart(gap);
        if (physicalX <= left) {
            float fraction = Math.max(0f, Math.min(0.999999f, physicalX / left));
            return (int) (fraction * 3f) + 1;
        }
        float fraction = Math.max(
                0f,
                Math.min(0.999999f, (physicalX - right) / (1f - right))
        );
        return (int) (fraction * 3f) + 4;
    }
}
