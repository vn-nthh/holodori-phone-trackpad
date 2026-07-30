package dev.holodori.trackpad;

final class TouchSample {
    static final int PROTOCOL_VERSION = 2;
    static final int ACTION_HEARTBEAT = 0;
    static final int ACTION_DOWN = 1;
    static final int ACTION_MOVE = 2;
    static final int ACTION_UP = 3;
    static final int ACTION_CANCEL = 4;
    static final int FLAG_INSIDE = 1;
    static final int FLAG_LOCKED = 2;
    static final int FLAG_SESSION_RESET = 0x04;
    static final int FLAG_HOST_RECOVERY = 0x08;
    // Heartbeat-only contextual flags for exact queue incidents.
    static final int FLAG_INCIDENT_ACTIVE_TOUCH = 0x01;
    static final int FLAG_INCIDENT_WRITER_BLOCKED = 0x02;
    static final int FLAG_QUEUE_INCIDENT = 0x04;
    static final int FLAG_INCIDENT_TIMING_BREAKDOWN = 0x08;
    static final int FLAG_INCIDENT_MOTION_BATCH = 0x10;
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
    long callbackNanos;
    long enqueuedNanos;
    long motionHistorySpanNanos;
    int lane;
    int motionHistorySize;
    int motionCrossedLaneCount;
    int incidentToken;
    boolean timingIncident;
}
