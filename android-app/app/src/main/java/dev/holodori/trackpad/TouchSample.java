package dev.holodori.trackpad;

final class TouchSample {
    static final int PROTOCOL_VERSION = 4;

    static final int ACTION_HEARTBEAT = 0;
    static final int ACTION_DOWN = 1;
    static final int ACTION_MOVE = 2;
    static final int ACTION_UP = 3;
    static final int ACTION_CANCEL = 4;

    static final int MESSAGE_TOUCH_FRAME = 1;
    static final int CONTROL_HELLO = 1;
    static final int CONTROL_ACK = 2;

    static final int FRAME_FLAG_LOCKED = 0x01;
    static final int FRAME_FLAG_SESSION_START = 0x02;
    static final int FRAME_FLAG_HISTORICAL = 0x04;
    static final int CONTACT_FLAG_INSIDE = 0x01;
    static final int CONTACT_FLAG_TIP = 0x02;

    static final int FRAME_HEADER_SIZE = 68;
    static final int CONTACT_SIZE = 10;
    static final int CRC_SIZE = 4;
    static final int CONTROL_SIZE = 40;
    static final int MAX_CONTACTS = 16;

    private TouchSample() {
    }
}
