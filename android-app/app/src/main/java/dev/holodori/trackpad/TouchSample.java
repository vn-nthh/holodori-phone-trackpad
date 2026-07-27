package dev.holodori.trackpad;

final class TouchSample {
    static final int ACTION_HEARTBEAT = 0;
    static final int ACTION_DOWN = 1;
    static final int ACTION_MOVE = 2;
    static final int ACTION_UP = 3;
    static final int ACTION_CANCEL = 4;
    static final int FLAG_INSIDE = 1;
    static final int FLAG_LOCKED = 2;
    static final int FLAG_QUEUE_WARNING = 0x10;
    static final int FLAG_QUEUE_RESYNC = 0x20;
    static final int FLAG_QUEUE_FAILSAFE = 0x40;
    static final int FLAG_QUEUE_DIAGNOSTICS = 0x80;

    int action;
    int pointerId;
    int flags;
    int x;
    int y;
    long eventNanos;
}
