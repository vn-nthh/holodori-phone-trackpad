package dev.holodori.trackpad;

interface TouchTransport {
    interface Listener {
        void onConnectionChanged(boolean connected, String message);
        void onHostLaneCountChanged(int laneCount);
    }

    boolean open();
    boolean isRunning();
    void offerFrame(
            int action,
            int actionPointerId,
            boolean locked,
            long eventNanos,
            long callbackNanos,
            boolean historical,
            int contactCount,
            int[] pointerIds,
            float[] x,
            float[] y,
            float[] pressure,
            float[] touchMajor,
            boolean[] touching
    );
    void close();
}
