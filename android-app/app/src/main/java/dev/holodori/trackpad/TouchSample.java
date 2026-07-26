package dev.holodori.trackpad;

final class TouchSample {
    static final int ACTION_HEARTBEAT = 0;
    static final int ACTION_DOWN = 1;
    static final int ACTION_MOVE = 2;
    static final int ACTION_UP = 3;
    static final int ACTION_CANCEL = 4;
    static final int FLAG_INSIDE = 1;
    static final int FLAG_LOCKED = 2;

    int action;
    int pointerId;
    int flags;
    int x;
    int y;
    int sequence;
    long eventNanos;
}
