package dev.holodori.trackpad;

import android.content.Context;
import android.content.SharedPreferences;
import android.graphics.Canvas;
import android.graphics.Color;
import android.graphics.Paint;
import android.graphics.PointF;
import android.graphics.RectF;
import android.os.Build;
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
    private static final int HANDLE_NONE = 0;
    private static final int HANDLE_TOP_LEFT = 1;
    private static final int HANDLE_TOP_RIGHT = 2;
    private static final int HANDLE_BOTTOM_LEFT = 3;
    private static final int HANDLE_BOTTOM_RIGHT = 4;
    private static final int HANDLE_TOP = 5;
    private static final int HANDLE_RIGHT = 6;
    private static final int HANDLE_BOTTOM = 7;
    private static final int HANDLE_LEFT = 8;
    private static final int EDIT_NONE = 0;
    private static final int EDIT_MOVE = 1;
    private static final int EDIT_SIDE = 2;
    private static final int EDIT_CORNERS = 3;

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

    private int editType = EDIT_NONE;
    private int editHandle = HANDLE_NONE;
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
    private final PointF startCornerA = new PointF();
    private final PointF startCornerB = new PointF();
    private final PointF lastCornerA = new PointF();
    private final PointF lastCornerB = new PointF();
    private final PointF startMidpoint = new PointF();

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
        if (!locked) {
            drawEditorHandles(canvas, zone);
        }
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
                    "Corners: resize / rotate  •  Sides: stretch one axis  •  Drag zone: move",
                    width / 2,
                    height - dp(22),
                    paint
            );
        }
    }

    private void drawEditorHandles(Canvas canvas, RectF zone) {
        paint.setStyle(Paint.Style.FILL);
        paint.setColor(withAlpha(ACCENT, 195));
        float cornerRadius = dp(9);
        canvas.drawCircle(zone.left, zone.top, cornerRadius, paint);
        canvas.drawCircle(zone.right, zone.top, cornerRadius, paint);
        canvas.drawCircle(zone.left, zone.bottom, cornerRadius, paint);
        canvas.drawCircle(zone.right, zone.bottom, cornerRadius, paint);

        float longSide = dp(22);
        float shortSide = dp(10);
        float radius = dp(5);
        canvas.drawRoundRect(
                zone.centerX() - longSide / 2,
                zone.top - shortSide / 2,
                zone.centerX() + longSide / 2,
                zone.top + shortSide / 2,
                radius,
                radius,
                paint
        );
        canvas.drawRoundRect(
                zone.centerX() - longSide / 2,
                zone.bottom - shortSide / 2,
                zone.centerX() + longSide / 2,
                zone.bottom + shortSide / 2,
                radius,
                radius,
                paint
        );
        canvas.drawRoundRect(
                zone.left - shortSide / 2,
                zone.centerY() - longSide / 2,
                zone.left + shortSide / 2,
                zone.centerY() + longSide / 2,
                radius,
                radius,
                paint
        );
        canvas.drawRoundRect(
                zone.right - shortSide / 2,
                zone.centerY() - longSide / 2,
                zone.right + shortSide / 2,
                zone.centerY() + longSide / 2,
                radius,
                radius,
                paint
        );
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
            cancelAll(eventTimeNanos(event));
        }
    }

    private void sendPointer(MotionEvent event, int index, int action) {
        int pointerId = event.getPointerId(index);
        PointF local = toZoneLocal(event.getX(index), event.getY(index));
        activeTouches.put(
                pointerId, new PointF(event.getX(index), event.getY(index))
        );
        sentPointers.add(pointerId);
        transport.offer(
                action,
                pointerId,
                local.x,
                local.y,
                true,
                true,
                eventTimeNanos(event)
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
            beginEditorPointer(
                    pointerId,
                    event.getX(actionIndex),
                    event.getY(actionIndex)
            );
        } else if (action == MotionEvent.ACTION_POINTER_DOWN) {
            beginEditorPointer(
                    pointerId,
                    event.getX(actionIndex),
                    event.getY(actionIndex)
            );
        } else if (action == MotionEvent.ACTION_MOVE) {
            if (editType == EDIT_MOVE) {
                int index = event.findPointerIndex(gesturePointerA);
                if (index >= 0) {
                    zoneX = startZoneX
                            + (event.getX(index) - gestureStartX) / getWidth();
                    zoneY = startZoneY
                            + (event.getY(index) - gestureStartY) / getHeight();
                }
            } else if (editType == EDIT_SIDE) {
                int index = event.findPointerIndex(gesturePointerA);
                if (index >= 0) {
                    applySideResize(event.getX(index), event.getY(index));
                }
            } else if (editType == EDIT_CORNERS) {
                updateCornerPointers(event);
                applyCornerResize();
            }
        } else if (action == MotionEvent.ACTION_POINTER_UP
                || action == MotionEvent.ACTION_UP) {
            if (pointerId == lockPointerId) {
                if (lockButton.contains(
                        event.getX(actionIndex), event.getY(actionIndex)
                )) {
                    setLocked(true);
                }
                lockPointerId = -1;
                return;
            }
            finishEditorPointer(pointerId);
        } else if (action == MotionEvent.ACTION_CANCEL) {
            clearEdit();
            lockPointerId = -1;
        }
    }

    private void beginEditorPointer(int pointerId, float x, float y) {
        int handle = hitHandle(x, y);
        if (isSideHandle(handle)) {
            editType = EDIT_SIDE;
            editHandle = handle;
            gesturePointerA = pointerId;
            gesturePointerB = -1;
            gestureStartX = x;
            gestureStartY = y;
            snapshotZone();
            return;
        }

        if (isCornerHandle(handle)) {
            if (editType == EDIT_CORNERS) {
                if (handle == oppositeCorner(editHandle)
                        && gesturePointerB < 0) {
                    gesturePointerB = pointerId;
                    return;
                }
                if (handle == editHandle) {
                    return;
                }
            }
            beginCornerEdit(handle, pointerId);
            return;
        }

        if (editType == EDIT_NONE && isInsideZone(x, y)) {
            editType = EDIT_MOVE;
            editHandle = HANDLE_NONE;
            gesturePointerA = pointerId;
            gesturePointerB = -1;
            gestureStartX = x;
            gestureStartY = y;
            snapshotZone();
        }
    }

    private void beginCornerEdit(int handle, int pointerId) {
        editType = EDIT_CORNERS;
        editHandle = handle;
        gesturePointerA = pointerId;
        gesturePointerB = -1;
        PointF a = cornerToScreen(handle);
        PointF b = cornerToScreen(oppositeCorner(handle));
        startCornerA.set(a.x, a.y);
        startCornerB.set(b.x, b.y);
        lastCornerA.set(a.x, a.y);
        lastCornerB.set(b.x, b.y);
        snapshotZone();
        snapshotCornerPair();
    }

    private void updateCornerPointers(MotionEvent event) {
        for (int index = 0; index < event.getPointerCount(); index++) {
            int pointerId = event.getPointerId(index);
            if (pointerId == gesturePointerA) {
                lastCornerA.set(event.getX(index), event.getY(index));
            }
            if (pointerId == gesturePointerB) {
                lastCornerB.set(event.getX(index), event.getY(index));
            }
        }
    }

    private void applyCornerResize() {
        PointF currentA =
                gesturePointerA >= 0 ? lastCornerA : startCornerA;
        PointF currentB =
                gesturePointerB >= 0 ? lastCornerB : startCornerB;
        float dx = currentB.x - currentA.x;
        float dy = currentB.y - currentA.y;
        float distance = (float) Math.hypot(dx, dy);
        float angle = (float) Math.toDegrees(Math.atan2(dy, dx));
        float midpointX = (currentA.x + currentB.x) / 2;
        float midpointY = (currentA.y + currentB.y) / 2;
        float scale = Math.max(0.15f, distance / Math.max(1, startDistance));

        zoneWidth = Math.max(0.08f, startZoneWidth * scale);
        zoneHeight = Math.max(0.04f, startZoneHeight * scale);
        zoneRotation = startZoneRotation + angle - startAngle;
        zoneX = startZoneX + (midpointX - startMidpoint.x) / getWidth();
        zoneY = startZoneY + (midpointY - startMidpoint.y) / getHeight();
    }

    private void applySideResize(float x, float y) {
        float radians = (float) Math.toRadians(startZoneRotation);
        float cos = (float) Math.cos(radians);
        float sin = (float) Math.sin(radians);
        float dx = x - gestureStartX;
        float dy = y - gestureStartY;
        float localX = dx * cos + dy * sin;
        float localY = -dx * sin + dy * cos;
        float newWidth = startZoneWidth;
        float newHeight = startZoneHeight;
        float centerX = startZoneX * getWidth();
        float centerY = startZoneY * getHeight();

        if (editHandle == HANDLE_RIGHT) {
            newWidth = Math.max(
                    0.08f, startZoneWidth + localX / getWidth()
            );
            float half = (newWidth - startZoneWidth) * getWidth() / 2;
            centerX += half * cos;
            centerY += half * sin;
        } else if (editHandle == HANDLE_LEFT) {
            newWidth = Math.max(
                    0.08f, startZoneWidth - localX / getWidth()
            );
            float half = (newWidth - startZoneWidth) * getWidth() / 2;
            centerX -= half * cos;
            centerY -= half * sin;
        } else if (editHandle == HANDLE_BOTTOM) {
            newHeight = Math.max(
                    0.04f, startZoneHeight + localY / getHeight()
            );
            float half = (newHeight - startZoneHeight) * getHeight() / 2;
            centerX -= half * sin;
            centerY += half * cos;
        } else if (editHandle == HANDLE_TOP) {
            newHeight = Math.max(
                    0.04f, startZoneHeight - localY / getHeight()
            );
            float half = (newHeight - startZoneHeight) * getHeight() / 2;
            centerX += half * sin;
            centerY -= half * cos;
        }

        zoneWidth = newWidth;
        zoneHeight = newHeight;
        zoneRotation = startZoneRotation;
        zoneX = centerX / getWidth();
        zoneY = centerY / getHeight();
    }

    private void finishEditorPointer(int pointerId) {
        if ((editType == EDIT_MOVE || editType == EDIT_SIDE)
                && pointerId == gesturePointerA) {
            clearEdit();
            saveZone();
            return;
        }
        if (editType != EDIT_CORNERS) {
            return;
        }
        if (pointerId == gesturePointerA) {
            gesturePointerA = -1;
        }
        if (pointerId == gesturePointerB) {
            gesturePointerB = -1;
        }
        if (gesturePointerA < 0 && gesturePointerB < 0) {
            clearEdit();
            saveZone();
            return;
        }

        startCornerA.set(lastCornerA.x, lastCornerA.y);
        startCornerB.set(lastCornerB.x, lastCornerB.y);
        snapshotZone();
        snapshotCornerPair();
    }

    private void snapshotCornerPair() {
        float dx = startCornerB.x - startCornerA.x;
        float dy = startCornerB.y - startCornerA.y;
        startDistance = (float) Math.hypot(dx, dy);
        startAngle = (float) Math.toDegrees(Math.atan2(dy, dx));
        startMidpoint.set(
                (startCornerA.x + startCornerB.x) / 2,
                (startCornerA.y + startCornerB.y) / 2
        );
    }

    private void clearEdit() {
        editType = EDIT_NONE;
        editHandle = HANDLE_NONE;
        gesturePointerA = -1;
        gesturePointerB = -1;
    }

    private int hitHandle(float screenX, float screenY) {
        PointF local = toZonePixel(screenX, screenY);
        float halfWidth = zoneWidth * getWidth() / 2;
        float halfHeight = zoneHeight * getHeight() / 2;
        float hitHalf = dp(HANDLE_SIZE_DP) / 2;

        if (contains(
                local.x, local.y, -halfWidth, -halfHeight, hitHalf, hitHalf
        )) {
            return HANDLE_TOP_LEFT;
        }
        if (contains(
                local.x, local.y, halfWidth, -halfHeight, hitHalf, hitHalf
        )) {
            return HANDLE_TOP_RIGHT;
        }
        if (contains(
                local.x, local.y, -halfWidth, halfHeight, hitHalf, hitHalf
        )) {
            return HANDLE_BOTTOM_LEFT;
        }
        if (contains(
                local.x, local.y, halfWidth, halfHeight, hitHalf, hitHalf
        )) {
            return HANDLE_BOTTOM_RIGHT;
        }

        float horizontalHalf = Math.min(
                zoneWidth * getWidth() * 0.42f, dp(160)
        ) / 2;
        float verticalHalf = Math.min(
                zoneHeight * getHeight() * 0.42f, dp(160)
        ) / 2;
        if (contains(
                local.x, local.y, 0, -halfHeight, horizontalHalf, hitHalf
        )) {
            return HANDLE_TOP;
        }
        if (contains(
                local.x, local.y, halfWidth, 0, hitHalf, verticalHalf
        )) {
            return HANDLE_RIGHT;
        }
        if (contains(
                local.x, local.y, 0, halfHeight, horizontalHalf, hitHalf
        )) {
            return HANDLE_BOTTOM;
        }
        if (contains(
                local.x, local.y, -halfWidth, 0, hitHalf, verticalHalf
        )) {
            return HANDLE_LEFT;
        }
        return HANDLE_NONE;
    }

    private PointF cornerToScreen(int handle) {
        float localX = (
                handle == HANDLE_TOP_LEFT || handle == HANDLE_BOTTOM_LEFT
        ) ? -zoneWidth * getWidth() / 2 : zoneWidth * getWidth() / 2;
        float localY = (
                handle == HANDLE_TOP_LEFT || handle == HANDLE_TOP_RIGHT
        ) ? -zoneHeight * getHeight() / 2 : zoneHeight * getHeight() / 2;
        double angle = Math.toRadians(zoneRotation);
        float centerX = zoneX * getWidth();
        float centerY = zoneY * getHeight();
        return new PointF(
                centerX + (float) (
                        localX * Math.cos(angle) - localY * Math.sin(angle)
                ),
                centerY + (float) (
                        localX * Math.sin(angle) + localY * Math.cos(angle)
                )
        );
    }

    private PointF toZonePixel(float screenX, float screenY) {
        float centerX = zoneX * getWidth();
        float centerY = zoneY * getHeight();
        double angle = Math.toRadians(-zoneRotation);
        float dx = screenX - centerX;
        float dy = screenY - centerY;
        return new PointF(
                (float) (dx * Math.cos(angle) - dy * Math.sin(angle)),
                (float) (dx * Math.sin(angle) + dy * Math.cos(angle))
        );
    }

    private boolean isInsideZone(float screenX, float screenY) {
        PointF local = toZoneLocal(screenX, screenY);
        return local.x >= 0 && local.x <= 1
                && local.y >= 0 && local.y <= 1;
    }

    private static boolean contains(
            float x,
            float y,
            float centerX,
            float centerY,
            float halfWidth,
            float halfHeight
    ) {
        return Math.abs(x - centerX) <= halfWidth
                && Math.abs(y - centerY) <= halfHeight;
    }

    private static boolean isCornerHandle(int handle) {
        return handle >= HANDLE_TOP_LEFT && handle <= HANDLE_BOTTOM_RIGHT;
    }

    private static boolean isSideHandle(int handle) {
        return handle >= HANDLE_TOP && handle <= HANDLE_LEFT;
    }

    private static int oppositeCorner(int handle) {
        if (handle == HANDLE_TOP_LEFT) return HANDLE_BOTTOM_RIGHT;
        if (handle == HANDLE_TOP_RIGHT) return HANDLE_BOTTOM_LEFT;
        if (handle == HANDLE_BOTTOM_LEFT) return HANDLE_TOP_RIGHT;
        return HANDLE_TOP_LEFT;
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
            cancelAll(System.nanoTime());
        }
        this.locked = locked;
        clearEdit();
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

    private static long eventTimeNanos(MotionEvent event) {
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            return event.getEventTimeNanos();
        }
        return event.getEventTime() * 1_000_000L;
    }
}
