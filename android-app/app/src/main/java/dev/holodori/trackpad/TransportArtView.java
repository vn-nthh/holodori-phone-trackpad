package dev.holodori.trackpad;

import android.animation.ValueAnimator;
import android.content.Context;
import android.graphics.Canvas;
import android.graphics.Paint;
import android.graphics.Path;
import android.graphics.PathMeasure;
import android.graphics.RectF;
import android.provider.Settings;
import android.view.View;
import android.view.animation.LinearInterpolator;

/**
 * Phone-to-PC link illustration. The same vector geometry as the desktop
 * launcher's SVG, drawn in a 216x120 design space: a phone, a PC, a USB cable
 * with travelling pulses, and Wi-Fi arcs. Transport changes cross-fade between
 * the cable and the arcs; {@link State} drives the motion. Honors the system
 * animator scale, so reduced-motion users get static frames.
 */
final class TransportArtView extends View {
    enum State { IDLE, SEARCHING, CONNECTED, OFF }

    private static final float DESIGN_WIDTH = 216f;
    private static final float DESIGN_HEIGHT = 120f;
    private static final float[] WAVE_RADII = {15f, 27f, 38f};
    private static final long LOOP_MILLIS = 1_600;
    private static final long BLEND_MILLIS = 350;

    private final Paint stroke = new Paint(Paint.ANTI_ALIAS_FLAG);
    private final Paint fill = new Paint(Paint.ANTI_ALIAS_FLAG);
    private final Path cable = new Path();
    private final Path cableSegment = new Path();
    private final PathMeasure cableMeasure = new PathMeasure();
    private final RectF rect = new RectF();
    private final ValueAnimator loop = ValueAnimator.ofFloat(0f, 1f);
    private ValueAnimator blendAnimator;

    private V5Protocol.TransportKind transport = V5Protocol.TransportKind.USB;
    private State state = State.IDLE;
    /** 0 = USB cable fully shown, 1 = Wi-Fi arcs fully shown. */
    private float blend;
    private float phase;
    private float cableLength;

    TransportArtView(Context context) {
        super(context);
        stroke.setStyle(Paint.Style.STROKE);
        stroke.setStrokeCap(Paint.Cap.ROUND);
        stroke.setStrokeJoin(Paint.Join.ROUND);
        stroke.setColor(Palette.TEXT);
        fill.setStyle(Paint.Style.FILL);
        fill.setColor(Palette.TEXT);
        cable.moveTo(66f, 60f);
        cable.cubicTo(90f, 60f, 96f, 72f, 108f, 72f);
        cable.cubicTo(120f, 72f, 126f, 60f, 150f, 60f);
        cableMeasure.setPath(cable, false);
        cableLength = cableMeasure.getLength();
        loop.setDuration(LOOP_MILLIS);
        loop.setInterpolator(new LinearInterpolator());
        loop.setRepeatCount(ValueAnimator.INFINITE);
        loop.addUpdateListener(animation -> {
            phase = (float) animation.getAnimatedValue();
            invalidate();
        });
        setContentDescription("Phone and PC link");
    }

    void setTransport(V5Protocol.TransportKind kind, boolean animate) {
        if (kind == null) kind = V5Protocol.TransportKind.USB;
        float target = kind == V5Protocol.TransportKind.WIFI ? 1f : 0f;
        boolean settled = blendAnimator == null || !blendAnimator.isRunning();
        if (kind == transport && blend == target && settled) return;
        transport = kind;
        if (blendAnimator != null) blendAnimator.cancel();
        if (!animate || !motionEnabled() || !isAttachedToWindow()) {
            blend = target;
            invalidate();
            return;
        }
        blendAnimator = ValueAnimator.ofFloat(blend, target);
        blendAnimator.setDuration(BLEND_MILLIS);
        blendAnimator.addUpdateListener(animation -> {
            blend = (float) animation.getAnimatedValue();
            invalidate();
        });
        blendAnimator.start();
    }

    V5Protocol.TransportKind transport() {
        return transport;
    }

    void setState(State state) {
        if (this.state == state) return;
        this.state = state;
        syncLoop();
        invalidate();
    }

    @Override
    protected void onAttachedToWindow() {
        super.onAttachedToWindow();
        syncLoop();
    }

    @Override
    protected void onDetachedFromWindow() {
        loop.cancel();
        if (blendAnimator != null) blendAnimator.cancel();
        super.onDetachedFromWindow();
    }

    @Override
    protected void onVisibilityChanged(View changedView, int visibility) {
        super.onVisibilityChanged(changedView, visibility);
        syncLoop();
    }

    @Override
    protected void onMeasure(int widthSpec, int heightSpec) {
        int width = MeasureSpec.getSize(widthSpec);
        int wanted = Math.round(width * DESIGN_HEIGHT / DESIGN_WIDTH);
        int height = MeasureSpec.getMode(heightSpec) == MeasureSpec.EXACTLY
                ? MeasureSpec.getSize(heightSpec)
                : MeasureSpec.getMode(heightSpec) == MeasureSpec.AT_MOST
                        ? Math.min(wanted, MeasureSpec.getSize(heightSpec))
                        : wanted;
        setMeasuredDimension(width, height);
    }

    private void syncLoop() {
        boolean wants = (state == State.SEARCHING || state == State.CONNECTED)
                && isAttachedToWindow()
                && isShown()
                && motionEnabled();
        if (wants && !loop.isStarted()) loop.start();
        else if (!wants && loop.isStarted()) loop.cancel();
    }

    private boolean motionEnabled() {
        return Settings.Global.getFloat(
                getContext().getContentResolver(),
                Settings.Global.ANIMATOR_DURATION_SCALE,
                1f
        ) > 0f;
    }

    @Override
    protected void onDraw(Canvas canvas) {
        super.onDraw(canvas);
        float scale = Math.min(getWidth() / DESIGN_WIDTH, getHeight() / DESIGN_HEIGHT);
        float offsetX = (getWidth() - DESIGN_WIDTH * scale) / 2f;
        float offsetY = (getHeight() - DESIGN_HEIGHT * scale) / 2f;
        float dim = state == State.OFF ? 0.4f : 1f;

        canvas.save();
        canvas.translate(offsetX, offsetY);
        canvas.scale(scale, scale);
        stroke.setStrokeWidth(2.4f);

        drawDevices(canvas, dim);
        if (blend < 1f) drawCable(canvas, dim * (1f - blend));
        if (blend > 0f) drawWaves(canvas, dim * blend);
        canvas.restore();
    }

    private void drawDevices(Canvas canvas, float dim) {
        boolean lit = state == State.CONNECTED;
        float phoneGlass = lit ? 0.22f : state == State.SEARCHING ? 0.16f : 0.06f;
        float pcGlass = lit ? 0.22f : 0.06f;

        stroke.setColor(Palette.withAlpha(Palette.TEXT, dim));
        rect.set(22f, 24f, 66f, 96f);
        canvas.drawRoundRect(rect, 8f, 8f, stroke);
        fill.setColor(Palette.withAlpha(Palette.TEXT, phoneGlass * dim));
        rect.set(28f, 32f, 60f, 84f);
        canvas.drawRoundRect(rect, 3f, 3f, fill);
        fill.setColor(Palette.withAlpha(Palette.TEXT, 0.5f * dim));
        canvas.drawCircle(44f, 90f, 1.8f, fill);

        rect.set(150f, 28f, 216f, 74f);
        canvas.drawRoundRect(rect, 6f, 6f, stroke);
        fill.setColor(Palette.withAlpha(Palette.TEXT, pcGlass * dim));
        rect.set(156f, 34f, 210f, 68f);
        canvas.drawRoundRect(rect, 2f, 2f, fill);
        canvas.drawLine(183f, 74f, 183f, 86f, stroke);
        canvas.drawLine(167f, 88f, 199f, 88f, stroke);
    }

    private void drawCable(Canvas canvas, float alpha) {
        float lineAlpha = state == State.CONNECTED ? 1f
                : state == State.SEARCHING ? 0.7f : 0.45f;
        stroke.setColor(Palette.withAlpha(Palette.TEXT, lineAlpha * alpha));
        canvas.drawPath(cable, stroke);
        fill.setColor(Palette.withAlpha(
                Palette.TEXT, (state == State.CONNECTED ? 1f : 0.55f) * alpha
        ));
        rect.set(140f, 55f, 150f, 65f);
        canvas.drawRoundRect(rect, 2f, 2f, fill);

        if (!loop.isStarted()) return;
        stroke.setStrokeWidth(3f);
        stroke.setColor(Palette.withAlpha(Palette.TEXT, alpha));
        drawPulse(canvas, phase, false);
        if (state == State.CONNECTED) drawPulse(canvas, (phase + 0.5f) % 1f, true);
        stroke.setStrokeWidth(2.4f);
    }

    /** A 14%-long dash travelling the cable once per loop, either direction. */
    private void drawPulse(Canvas canvas, float progress, boolean backwards) {
        float dash = cableLength * 0.14f;
        float start = progress * (cableLength + dash) - dash;
        float end = start + dash;
        start = Math.max(0f, start);
        end = Math.min(cableLength, end);
        if (end <= start) return;
        if (backwards) {
            float mirroredStart = cableLength - end;
            end = cableLength - start;
            start = mirroredStart;
        }
        cableSegment.rewind();
        cableMeasure.getSegment(start, end, cableSegment, true);
        canvas.drawPath(cableSegment, stroke);
    }

    private void drawWaves(Canvas canvas, float alpha) {
        canvas.save();
        float grow = 0.85f + 0.15f * blend;
        canvas.scale(grow, grow, 108f, 60f);
        for (int index = 0; index < WAVE_RADII.length; index++) {
            float radius = WAVE_RADII[index];
            float phoneAlpha;
            float pcAlpha;
            if (state == State.CONNECTED) {
                float breathe = loop.isStarted()
                        ? 0.7f + 0.3f * bump(phase - index * 0.09f)
                        : 1f;
                phoneAlpha = breathe;
                pcAlpha = breathe;
            } else if (state == State.SEARCHING) {
                phoneAlpha = loop.isStarted()
                        ? 0.15f + 0.85f * bump(phase - index * 0.14f)
                        : 0.6f;
                pcAlpha = 0.15f;
            } else {
                phoneAlpha = 0.3f;
                pcAlpha = 0.3f;
            }
            stroke.setColor(Palette.withAlpha(Palette.TEXT, phoneAlpha * alpha));
            rect.set(66f - radius, 60f - radius, 66f + radius, 60f + radius);
            canvas.drawArc(rect, -38f, 76f, false, stroke);
            stroke.setColor(Palette.withAlpha(Palette.TEXT, pcAlpha * alpha));
            rect.set(150f - radius, 60f - radius, 150f + radius, 60f + radius);
            canvas.drawArc(rect, 142f, 76f, false, stroke);
        }
        canvas.restore();
    }

    /** Smooth 0..1..0 pulse over one loop, peaking at 45% like the CSS. */
    private static float bump(float t) {
        t = t - (float) Math.floor(t);
        float up = 0.45f;
        float value = t < up ? t / up : (1f - t) / (1f - up);
        return value * value * (3f - 2f * value);
    }
}
