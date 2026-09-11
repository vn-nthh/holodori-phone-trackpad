package dev.holodori.trackpad;

import android.content.Context;
import android.graphics.Canvas;
import android.graphics.Paint;
import android.view.HapticFeedbackConstants;
import android.view.MotionEvent;
import android.view.View;

import java.util.Arrays;

/** Neutral eight-step lane collector; it never displays the hidden SAS. */
final class PairingLaneView extends View {
    interface Listener {
        void onPatternEntered(int[] lanes);
    }

    private static final int BACKGROUND = Palette.BG;
    private static final int SURFACE = Palette.SURFACE_2;
    private static final int OUTLINE = Palette.BORDER_STRONG;
    private static final int ACCENT = Palette.ACCENT;
    private static final int ON_ACCENT = Palette.ON_ACCENT;
    private static final int MUTED = Palette.MUTED;
    private static final int TEXT = Palette.TEXT;
    private final Paint paint = new Paint(Paint.ANTI_ALIAS_FLAG);
    private final int[] entered = new int[8];
    private boolean thumbMode;
    private float thumbGap;
    private final Listener listener;

    private boolean accepting;
    private int count;
    private int activePointer = -1;
    private int ignoredPointer = -1;
    private int activeLane;

    PairingLaneView(
            Context context,
            boolean thumbMode,
            float thumbGap,
            Listener listener
    ) {
        super(context);
        this.thumbMode = thumbMode;
        this.thumbGap = ThumbTransform.clampGap(thumbGap);
        this.listener = listener;
        setBackgroundColor(BACKGROUND);
        setContentDescription("Six pairing lanes");
    }

    void reset() {
        Arrays.fill(entered, 0);
        count = 0;
        activePointer = -1;
        ignoredPointer = -1;
        activeLane = 0;
        accepting = false;
        invalidate();
    }

    void configure(boolean thumbMode, float thumbGap) {
        if (accepting || activePointer != -1 || ignoredPointer != -1) return;
        this.thumbMode = thumbMode;
        this.thumbGap = ThumbTransform.clampGap(thumbGap);
        invalidate();
    }

    void setAccepting(boolean accepting) {
        this.accepting = accepting && count < entered.length;
        activePointer = -1;
        ignoredPointer = -1;
        activeLane = 0;
        invalidate();
    }

    int progress() {
        return count;
    }

    @Override
    protected void onDraw(Canvas canvas) {
        super.onDraw(canvas);
        float top = dp(6);
        float bottom = getHeight() - dp(30);
        float width = getWidth();
        for (int lane = 1; lane <= 6; lane++) {
            float left = width * laneStart(lane) + dp(3);
            float right = width * laneEnd(lane) - dp(3);
            boolean active = lane == activeLane;
            paint.setStyle(Paint.Style.FILL);
            paint.setColor(active ? ACCENT : SURFACE);
            canvas.drawRoundRect(left, top, right, bottom, dp(10), dp(10), paint);
            if (!active) {
                paint.setStyle(Paint.Style.STROKE);
                paint.setStrokeWidth(dp(1));
                paint.setColor(OUTLINE);
                canvas.drawRoundRect(left, top, right, bottom, dp(10), dp(10), paint);
                paint.setStyle(Paint.Style.FILL);
            }
            paint.setTextAlign(Paint.Align.CENTER);
            paint.setTextSize(dp(24));
            paint.setFakeBoldText(true);
            paint.setColor(active ? ON_ACCENT : TEXT);
            canvas.drawText(
                    Integer.toString(lane),
                    (left + right) / 2f,
                    (top + bottom) / 2f - (paint.ascent() + paint.descent()) / 2f,
                    paint
            );
        }
        paint.setFakeBoldText(false);
        paint.setTextSize(dp(13));
        paint.setTextAlign(Paint.Align.CENTER);
        paint.setColor(accepting ? TEXT : MUTED);
        String status = accepting
                ? "Step " + (count + 1) + " of 8"
                : (count == 8 ? "8 of 8" : "");
        canvas.drawText(status, width / 2f, getHeight() - dp(9), paint);
    }

    @Override
    public boolean onTouchEvent(MotionEvent event) {
        int action = event.getActionMasked();
        int actionIndex = event.getActionIndex();
        int pointerId = event.getPointerId(actionIndex);
        if (!accepting) return true;
        if (action == MotionEvent.ACTION_DOWN || action == MotionEvent.ACTION_POINTER_DOWN) {
            if (activePointer != -1 || ignoredPointer != -1) return true;
            int lane = laneAt(event.getX(actionIndex));
            if (lane == 0) {
                ignoredPointer = pointerId;
                return true;
            }
            activePointer = pointerId;
            activeLane = lane;
            invalidate();
        } else if (action == MotionEvent.ACTION_UP
                || action == MotionEvent.ACTION_POINTER_UP) {
            if (pointerId == ignoredPointer) {
                ignoredPointer = -1;
                return true;
            }
            if (pointerId != activePointer) return true;
            entered[count++] = activeLane;
            activePointer = -1;
            activeLane = 0;
            performHapticFeedback(HapticFeedbackConstants.KEYBOARD_TAP);
            if (count == entered.length) {
                accepting = false;
                listener.onPatternEntered(entered.clone());
            }
            invalidate();
        } else if (action == MotionEvent.ACTION_CANCEL) {
            activePointer = -1;
            ignoredPointer = -1;
            activeLane = 0;
            invalidate();
        }
        return true;
    }

    private int laneAt(float x) {
        float physical = getWidth() <= 0 ? 0.5f : x / getWidth();
        if (!thumbMode) {
            float clamped = Math.max(0f, Math.min(0.999999f, physical));
            return (int) (clamped * 6f) + 1;
        }
        return ThumbTransform.laneAtPhysicalX(physical, thumbGap);
    }

    private float laneStart(int lane) {
        if (!thumbMode) return (lane - 1) / 6f;
        float left = ThumbTransform.leftEnd(thumbGap);
        float right = ThumbTransform.rightStart(thumbGap);
        if (lane <= 3) return left * (lane - 1) / 3f;
        return right + (1f - right) * (lane - 4) / 3f;
    }

    private float laneEnd(int lane) {
        if (!thumbMode) return lane / 6f;
        float left = ThumbTransform.leftEnd(thumbGap);
        float right = ThumbTransform.rightStart(thumbGap);
        if (lane <= 3) return left * lane / 3f;
        return right + (1f - right) * (lane - 3) / 3f;
    }

    private float dp(float value) {
        return value * getResources().getDisplayMetrics().density;
    }
}
