package dev.holodori.trackpad;

import android.content.Context;
import android.content.SharedPreferences;
import android.graphics.Canvas;
import android.graphics.Color;
import android.graphics.Paint;
import android.graphics.PointF;
import android.graphics.RectF;
import android.os.SystemClock;
import android.view.MotionEvent;
import android.view.View;

import java.util.HashMap;
import java.util.HashSet;
import java.util.Locale;
import java.util.Map;
import java.util.Set;

final class TrackpadView extends View {
    private static final int BACKGROUND = Color.rgb(9, 10, 18);
    private static final int SURFACE = Color.rgb(15, 20, 31);
    private static final int ACCENT = Color.rgb(66, 217, 245);
    private static final int TEXT = Color.rgb(215, 244, 247);
    private static final int MUTED = Color.rgb(108, 137, 145);
    private static final int CONNECTED = Color.rgb(111, 230, 139);
    private static final int HANDLE_SIZE_DP = 56;

    private final UsbAccessoryTransport transport;
    private final SharedPreferences preferences;
    private final Paint paint = new Paint(Paint.ANTI_ALIAS_FLAG);
    private final RectF lockButton = new RectF();
    private final Map<Integer, PointF> activeTouches = new HashMap<>();
    private final Set<Integer> sentPointers = new HashSet<>();

    private float zoneX = 0.50f;
    private float zoneY = 0.76f;
    private float zoneWidth = 0.80f;
    private float zoneHeight = 0.18f;
    private float zoneRotation;
    private int laneCount = 6;
    private boolean locked;
    private boolean connected;
    private String status = "Connect the USB cable";
    private int lockPointerId = -1;

    private int gesturePointerA = -1;
    private int gesturePointerB = -1;
    private float gestureStartX;
    private float gestureStartY;
    private float startZoneX;
    private float startZoneY;
    private float startZoneWidth;
    private float startZoneHeight;
    private float startZoneRotation;
    private float startDistance;
    private float startAngle;

    TrackpadView(Context context, UsbAccessoryTransport transport) {
        super(context);
        this.transport = transport;
        preferences = context.getSharedPreferences("trackpad", Context.MODE_PRIVATE);
        zoneX = preferences.getFloat("zone_x", zoneX);
        zoneY = preferences.getFloat("zone_y", zoneY);
        zoneWidth = preferences.getFloat("zone_w", zoneWidth);
        zoneHeight = preferences.getFloat("zone_h", zoneHeight);
        zoneRotation = preferences.getFloat("zone_r", 0);
        setBackgroundColor(BACKGROUND);
        setSystemUiVisibility(SYSTEM_UI_FLAG_IMMERSIVE_STICKY);
    }

    void setConnectionStatus(boolean connected, String status) {
        this.connected = connected;
        this.status = status;
        invalidate();
    }

    void setLaneCount(int laneCount) {
        this.laneCount = Math.max(1, Math.min(16, laneCount));
        invalidate();
    }

    @Override
    protected void onSizeChanged(int width, int height, int oldWidth, int oldHeight) {
        float radius = dp(26);
        lockButton.set(
                width - radius * 2.4f,
                radius * 0.4f,
                width - radius * 0.4f,
                radius * 2.4f
        );
    }

    @Override
    protected void onDraw(Canvas canvas) {
        super.onDraw(canvas);
        float width = getWidth();
        float height = getHeight();
        float centerX = zoneX * width;
        float centerY = zoneY * height;
        float zonePixelWidth = zoneWidth * width;
        float zonePixelHeight = zoneHeight * height;

        canvas.save();
        canvas.rotate(zoneRotation, centerX, centerY);
        RectF zone = new RectF(
                centerX - zonePixelWidth / 2,
                centerY - zonePixelHeight / 2,
                centerX + zonePixelWidth / 2,
                centerY + zonePixelHeight / 2
        );
        paint.setStyle(Paint.Style.FILL);
        paint.setColor(SURFACE);
        canvas.drawRoundRect(zone, dp(8), dp(8), paint);
        paint.setStyle(Paint.Style.STROKE);
        paint.setStrokeWidth(dp(locked ? 1 : 2));
        paint.setColor(withAlpha(ACCENT, locked ? 90 : 210));
        canvas.drawRoundRect(zone, dp(8), dp(8), paint);

        for (int lane = 1; lane < laneCount; lane++) {
            float x = zone.left + zone.width() * lane / laneCount;
            paint.setColor(withAlpha(ACCENT, 60));
            paint.setStrokeWidth(dp(1));
            canvas.drawLine(x, zone.top, x, zone.bottom, paint);
        }

        paint.setStyle(Paint.Style.FILL);
        paint.setTextAlign(Paint.Align.CENTER);
        paint.setTextSize(Math.min(dp(22), zone.height() * 0.28f));
        paint.setFakeBoldText(true);
        for (int lane = 0; lane < laneCount; lane++) {
            float x = zone.left + zone.width() * (lane + 0.5f) / laneCount;
            paint.setColor(withAlpha(ACCENT, locked ? 70 : 130));
            canvas.drawText(
                    String.format(Locale.US, "%d", lane + 1),
                    x,
                    zone.centerY() - (paint.ascent() + paint.descent()) / 2,
                    paint
            );
        }
        paint.setFakeBoldText(false);
        canvas.restore();

        if (locked) {
            paint.setStyle(Paint.Style.STROKE);
            paint.setStrokeWidth(dp(3));
            for (Map.Entry<Integer, PointF> entry : activeTouches.entrySet()) {
                PointF point = entry.getValue();
                paint.setColor(withAlpha(ACCENT, 205));
                canvas.drawCircle(point.x, point.y, dp(18), paint);
                paint.setStyle(Paint.Style.FILL);
                canvas.drawCircle(point.x, point.y, dp(5), paint);
                paint.setStyle(Paint.Style.STROKE);
            }
        }

        paint.setStyle(Paint.Style.FILL);
        paint.setColor(withAlpha(BACKGROUND, 230));
        canvas.drawCircle(lockButton.centerX(), lockButton.centerY(),
                lockButton.width() / 2, paint);
        paint.setStyle(Paint.Style.STROKE);
        paint.setStrokeWidth(dp(2));
        paint.setColor(locked ? CONNECTED : ACCENT);
        canvas.drawCircle(lockButton.centerX(), lockButton.centerY(),
                lockButton.width() / 2 - dp(1), paint);
        drawLockIcon(canvas, lockButton.centerX(), lockButton.centerY(), locked);

        paint.setStyle(Paint.Style.FILL);
        paint.setTextAlign(Paint.Align.LEFT);
        paint.setTextSize(dp(12));
        paint.setColor(connected ? CONNECTED : MUTED);
        canvas.drawText(status, dp(16), dp(24), paint);

        if (!locked) {
            paint.setTextAlign(Paint.Align.CENTER);
            paint.setTextSize(dp(13));
            paint.setColor(MUTED);
            canvas.drawText(
                    "Drag to position  •  Pinch to resize and rotate  •  Tap lock to play",
                    width / 2,
                    height - dp(22),
                    paint
            );
        }
    }

    private void drawLockIcon(Canvas canvas, float x, float y, boolean locked) {
        paint.setStyle(Paint.Style.STROKE);
        paint.setStrokeWidth(dp(2.2f));
        paint.setStrokeCap(Paint.Cap.ROUND);
        paint.setColor(locked ? CONNECTED : ACCENT);
        RectF body = new RectF(x - dp(8), y - dp(1), x + dp(8), y + dp(11));
        canvas.drawRoundRect(body, dp(2), dp(2), paint);
        RectF shackle = new RectF(x - dp(5), y - dp(10), x + dp(5), y + dp(4));
        canvas.drawArc(shackle, 180, locked ? 180 : 135, false, paint);
    }

    @Override
    public boolean onTouchEvent(MotionEvent event) {
        int action = event.getActionMasked();
        int actionIndex = event.getActionIndex();
        int pointerId = event.getPointerId(actionIndex);
        if (action == MotionEvent.ACTION_DOWN) {
            // Ask InputDispatcher not to align MOVE delivery to display frames.
            requestUnbufferedDispatch(event);
        }

        if (locked) {
            handleLockedTouch(event, action, actionIndex, pointerId);
        } else {
            handleEditorTouch(event, action, actionIndex, pointerId);
        }
        invalidate();
        return true;
    }

    private void handleLockedTouch(
            MotionEvent event, int action, int actionIndex, int pointerId
    ) {
        if (action == MotionEvent.ACTION_DOWN
                || action == MotionEvent.ACTION_POINTER_DOWN) {
            if (lockButton.contains(event.getX(actionIndex), event.getY(actionIndex))) {
                lockPointerId = pointerId;
                return;
            }
            sendPointer(event, actionIndex, TouchSample.ACTION_DOWN);
        } else if (action == MotionEvent.ACTION_MOVE) {
            for (int index = 0; index < event.getPointerCount(); index++) {
                if (event.getPointerId(index) != lockPointerId) {
                    sendPointer(event, index, TouchSample.ACTION_MOVE);
                }
            }
        } else if (action == MotionEvent.ACTION_UP
                || action == MotionEvent.ACTION_POINTER_UP) {
            if (pointerId == lockPointerId) {
                if (lockButton.contains(
                        event.getX(actionIndex), event.getY(actionIndex)
                )) {
                    setLocked(false);
                }
                lockPointerId = -1;
                return;
            }
            sendPointer(event, actionIndex, TouchSample.ACTION_UP);
            activeTouches.remove(pointerId);
            sentPointers.remove(pointerId);
        } else if (action == MotionEvent.ACTION_CANCEL) {
            cancelAll(event.getEventTime() * 1_000_000L);
        }
    }

    private void sendPointer(MotionEvent event, int index, int action) {
        int pointerId = event.getPointerId(index);
        PointF local = toZoneLocal(event.getX(index), event.getY(index));
        boolean inside =
                local.x >= 0 && local.x <= 1 && local.y >= 0 && local.y <= 1;
        activeTouches.put(
                pointerId, new PointF(event.getX(index), event.getY(index))
        );
        sentPointers.add(pointerId);
        transport.offer(
                action,
                pointerId,
                local.x,
                local.y,
                inside,
                true,
                event.getEventTime() * 1_000_000L
        );
    }

    private void handleEditorTouch(
            MotionEvent event, int action, int actionIndex, int pointerId
    ) {
        if (action == MotionEvent.ACTION_DOWN) {
            if (lockButton.contains(event.getX(), event.getY())) {
                lockPointerId = pointerId;
                return;
            }
            PointF local = toZoneLocal(event.getX(), event.getY());
            if (local.x >= 0 && local.x <= 1 && local.y >= 0 && local.y <= 1) {
                gesturePointerA = pointerId;
                gestureStartX = event.getX();
                gestureStartY = event.getY();
                snapshotZone();
            }
        } else if (action == MotionEvent.ACTION_POINTER_DOWN
                && gesturePointerA >= 0 && gesturePointerB < 0) {
            gesturePointerB = pointerId;
            snapshotZone();
            recordTwoPointerStart(event);
        } else if (action == MotionEvent.ACTION_MOVE) {
            if (gesturePointerA >= 0 && gesturePointerB >= 0) {
                int a = event.findPointerIndex(gesturePointerA);
                int b = event.findPointerIndex(gesturePointerB);
                if (a >= 0 && b >= 0) {
                    float dx = event.getX(b) - event.getX(a);
                    float dy = event.getY(b) - event.getY(a);
                    float distance = Math.max(1, (float) Math.hypot(dx, dy));
                    float scale = distance / Math.max(1, startDistance);
                    zoneWidth = clamp(startZoneWidth * scale, 0.18f, 0.96f);
                    zoneHeight = clamp(startZoneHeight * scale, 0.08f, 0.70f);
                    zoneRotation = startZoneRotation
                            + (float) Math.toDegrees(Math.atan2(dy, dx))
                            - startAngle;
                    zoneX = clamp(
                            ((event.getX(a) + event.getX(b)) / 2) / getWidth(),
                            0.05f,
                            0.95f
                    );
                    zoneY = clamp(
                            ((event.getY(a) + event.getY(b)) / 2) / getHeight(),
                            0.05f,
                            0.95f
                    );
                }
            } else if (gesturePointerA >= 0) {
                int index = event.findPointerIndex(gesturePointerA);
                if (index >= 0) {
                    zoneX = clamp(
                            startZoneX
                                    + (event.getX(index) - gestureStartX) / getWidth(),
                            0.05f,
                            0.95f
                    );
                    zoneY = clamp(
                            startZoneY
                                    + (event.getY(index) - gestureStartY) / getHeight(),
                            0.05f,
                            0.95f
                    );
                }
            }
        } else if (action == MotionEvent.ACTION_POINTER_UP) {
            if (pointerId == gesturePointerB) {
                gesturePointerB = -1;
                int a = event.findPointerIndex(gesturePointerA);
                if (a >= 0) {
                    gestureStartX = event.getX(a);
                    gestureStartY = event.getY(a);
                    snapshotZone();
                }
            } else if (pointerId == gesturePointerA) {
                gesturePointerA = gesturePointerB;
                gesturePointerB = -1;
                if (gesturePointerA >= 0) {
                    int a = event.findPointerIndex(gesturePointerA);
                    gestureStartX = event.getX(a);
                    gestureStartY = event.getY(a);
                    snapshotZone();
                }
            }
        } else if (action == MotionEvent.ACTION_UP) {
            if (pointerId == lockPointerId) {
                if (lockButton.contains(event.getX(), event.getY())) {
                    setLocked(true);
                }
                lockPointerId = -1;
            }
            gesturePointerA = -1;
            gesturePointerB = -1;
            saveZone();
        } else if (action == MotionEvent.ACTION_CANCEL) {
            gesturePointerA = -1;
            gesturePointerB = -1;
            lockPointerId = -1;
        }
    }

    private void recordTwoPointerStart(MotionEvent event) {
        int a = event.findPointerIndex(gesturePointerA);
        int b = event.findPointerIndex(gesturePointerB);
        if (a < 0 || b < 0) return;
        float dx = event.getX(b) - event.getX(a);
        float dy = event.getY(b) - event.getY(a);
        startDistance = (float) Math.hypot(dx, dy);
        startAngle = (float) Math.toDegrees(Math.atan2(dy, dx));
        zoneX = ((event.getX(a) + event.getX(b)) / 2) / getWidth();
        zoneY = ((event.getY(a) + event.getY(b)) / 2) / getHeight();
        startZoneX = zoneX;
        startZoneY = zoneY;
    }

    private void snapshotZone() {
        startZoneX = zoneX;
        startZoneY = zoneY;
        startZoneWidth = zoneWidth;
        startZoneHeight = zoneHeight;
        startZoneRotation = zoneRotation;
    }

    private PointF toZoneLocal(float screenX, float screenY) {
        float centerX = zoneX * getWidth();
        float centerY = zoneY * getHeight();
        double angle = Math.toRadians(-zoneRotation);
        float dx = screenX - centerX;
        float dy = screenY - centerY;
        float localX = (float) (dx * Math.cos(angle) - dy * Math.sin(angle));
        float localY = (float) (dx * Math.sin(angle) + dy * Math.cos(angle));
        return new PointF(
                localX / (zoneWidth * getWidth()) + 0.5f,
                localY / (zoneHeight * getHeight()) + 0.5f
        );
    }

    private void setLocked(boolean locked) {
        if (this.locked == locked) return;
        if (!locked) {
            cancelAll(SystemClock.uptimeMillis() * 1_000_000L);
        }
        this.locked = locked;
        activeTouches.clear();
        sentPointers.clear();
        saveZone();
    }

    private void cancelAll(long eventNanos) {
        if (!sentPointers.isEmpty()) {
            transport.offer(
                    TouchSample.ACTION_CANCEL,
                    0,
                    0,
                    0,
                    false,
                    false,
                    eventNanos
            );
        }
        activeTouches.clear();
        sentPointers.clear();
    }

    private void saveZone() {
        preferences.edit()
                .putFloat("zone_x", zoneX)
                .putFloat("zone_y", zoneY)
                .putFloat("zone_w", zoneWidth)
                .putFloat("zone_h", zoneHeight)
                .putFloat("zone_r", zoneRotation)
                .apply();
    }

    private float dp(float value) {
        return value * getResources().getDisplayMetrics().density;
    }

    private static int withAlpha(int color, int alpha) {
        return Color.argb(
                alpha, Color.red(color), Color.green(color), Color.blue(color)
        );
    }

    private static float clamp(float value, float min, float max) {
        return Math.max(min, Math.min(max, value));
    }
}
