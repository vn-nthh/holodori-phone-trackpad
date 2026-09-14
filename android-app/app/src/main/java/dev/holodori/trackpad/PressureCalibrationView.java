package dev.holodori.trackpad;

import android.content.Context;
import android.content.SharedPreferences;
import android.content.res.ColorStateList;
import android.graphics.Canvas;
import android.graphics.Paint;
import android.view.Gravity;
import android.view.MotionEvent;
import android.view.View;
import android.widget.LinearLayout;
import android.widget.SeekBar;
import android.widget.Switch;
import android.widget.TextView;
import java.util.Locale;

/** Local settings-only preview. Never connected to a gameplay transport. */
final class PressureCalibrationView extends LinearLayout {
    private final TextView reading;
    private final TextView cutoffLabel;
    private final TextView result;
    private final PressureLine line;
    private int pointerId = -1;
    private float current;
    private float peak;
    private float minimum = 1f;
    private float maximum;
    private int samples;
    private boolean registered;

    PressureCalibrationView(Context context) {
        super(context);
        SharedPreferences preferences = context.getSharedPreferences("trackpad", Context.MODE_PRIVATE);
        setOrientation(VERTICAL);
        setPadding(0, dp(10), 0, dp(10));

        LinearLayout header = new LinearLayout(context);
        header.setGravity(Gravity.CENTER_VERTICAL);
        header.addView(label("Light-touch filter · keys only", 15),
                new LayoutParams(0, LayoutParams.WRAP_CONTENT, 1f));
        Switch enabled = new Switch(context);
        enabled.setContentDescription("Light-touch filter, keyboard mode only");
        enabled.setThumbTintList(ColorStateList.valueOf(Palette.TEXT));
        enabled.setChecked(preferences.getBoolean(PressureFilter.ENABLED, false));
        header.addView(enabled);
        addView(header);

        LinearLayout calibration = new LinearLayout(context);
        calibration.setOrientation(VERTICAL);
        calibration.setVisibility(enabled.isChecked() ? VISIBLE : GONE);
        addView(calibration);
        enabled.setOnCheckedChangeListener((button, checked) -> {
            preferences.edit().putBoolean(PressureFilter.ENABLED, checked).apply();
            calibration.setVisibility(checked ? VISIBLE : GONE);
        });

        TextView instructions = label("Brush the test pad lightly, then tap normally. "
                + "Drag the round cutoff between the readings. The vertical line shows live pressure.", 13);
        instructions.setTextColor(Palette.MUTED);
        calibration.addView(instructions);
        cutoffLabel = label("", 13);
        calibration.addView(cutoffLabel);
        line = new PressureLine(context);
        line.setMax(1000);
        line.setProgress(Math.round(PressureFilter.normalize(preferences.getFloat(
                PressureFilter.CUTOFF, PressureFilter.DEFAULT_CUTOFF)) * 1000));
        line.setContentDescription("Minimum touch pressure");
        line.setProgressTintList(ColorStateList.valueOf(Palette.BORDER_STRONG));
        line.setProgressBackgroundTintList(ColorStateList.valueOf(Palette.BORDER));
        line.setThumbTintList(ColorStateList.valueOf(Palette.TEXT));
        calibration.addView(line, new LayoutParams(LayoutParams.MATCH_PARENT, dp(48)));
        reading = label("Live —   Last peak —", 13);
        calibration.addView(reading);
        result = new TextView(context) {
            @Override
            public boolean onTouchEvent(MotionEvent event) {
                boolean clicked = event.getActionMasked() == MotionEvent.ACTION_UP
                        || (event.getActionMasked() == MotionEvent.ACTION_POINTER_UP
                        && event.getPointerId(event.getActionIndex()) == pointerId);
                boolean handled = testTouch(event);
                if (clicked) performClick();
                return handled;
            }

            @Override
            public boolean performClick() {
                super.performClick();
                return true;
            }
        };
        result.setText("Touch here to test");
        result.setTextSize(15);
        result.setTextColor(Palette.TEXT);
        result.setGravity(Gravity.CENTER);
        result.setBackground(Palette.rounded(context, Palette.SURFACE_2, Palette.BORDER_STRONG, 10));
        result.setContentDescription("Pressure test pad. Test touches here do not send keys.");
        result.setClickable(true);
        LayoutParams padParams = new LayoutParams(LayoutParams.MATCH_PARENT, dp(88));
        padParams.topMargin = dp(8);
        calibration.addView(result, padParams);
        TextView detail = label("Test only; no keys are sent. Once a touch reaches the cutoff, "
                + "it stays registered until lift. Raising the cutoff can miss light taps. "
                + "If brushes and taps show the same pressure, leave this filter off.", 12);
        detail.setTextColor(Palette.MUTED);
        detail.setPadding(0, dp(8), 0, 0);
        calibration.addView(detail);
        line.setOnSeekBarChangeListener(new SeekBar.OnSeekBarChangeListener() {
            @Override
            public void onProgressChanged(SeekBar seekBar, int progress, boolean fromUser) {
                updateCutoff();
                // Persist keyboard/accessibility adjustments as well as drags.
                if (fromUser) preferences.edit().putFloat(PressureFilter.CUTOFF, cutoff()).apply();
            }

            @Override public void onStartTrackingTouch(SeekBar seekBar) { }
            @Override public void onStopTrackingTouch(SeekBar seekBar) { }
        });
        updateCutoff();
    }

    private float cutoff() { return line.getProgress() / 1000f; }

    private void updateCutoff() {
        cutoffLabel.setText(String.format(Locale.ROOT,
                "Register at %.1f%% or above · scale 0–100%%", cutoff() * 100));
    }

    private boolean testTouch(MotionEvent event) {
        int action = event.getActionMasked();
        if (action == MotionEvent.ACTION_DOWN) {
            pointerId = event.getPointerId(0);
            peak = 0f;
            registered = false;
            getParent().requestDisallowInterceptTouchEvent(true);
        }
        int index = event.findPointerIndex(pointerId);
        boolean lifted = action == MotionEvent.ACTION_CANCEL || action == MotionEvent.ACTION_UP
                || (action == MotionEvent.ACTION_POINTER_UP
                && event.getPointerId(event.getActionIndex()) == pointerId);
        if (index >= 0 && !lifted) {
            for (int history = 0; history < event.getHistorySize(); history++) {
                sample(event.getHistoricalPressure(index, history));
            }
            sample(event.getPressure(index));
            result.setText(registered ? "Registered · hold stays active" : "Below cutoff · ignored");
        }
        if (lifted) {
            pointerId = -1;
            current = 0f;
            result.setText(action == MotionEvent.ACTION_CANCEL ? "Test cancelled"
                    : registered ? "Registered · lift released" : "Ignored · try a normal tap");
            getParent().requestDisallowInterceptTouchEvent(false);
        }
        line.livePressure = current;
        line.invalidate();
        String range = samples == 0 ? "" : String.format(Locale.ROOT,
                "\nObserved %.1f–%.1f%%%s", minimum * 100, maximum * 100,
                samples >= 8 && maximum - minimum < 0.001f ? " · no variation seen yet" : "");
        reading.setText(String.format(Locale.ROOT, "Live %.1f%%   Last peak %.1f%%%s",
                current * 100, peak * 100, range));
        return true;
    }

    private void sample(float pressure) {
        current = PressureFilter.normalize(pressure);
        peak = Math.max(peak, current);
        minimum = Math.min(minimum, current);
        maximum = Math.max(maximum, current);
        samples = Math.min(1000, samples + 1);
        registered |= PressureFilter.meetsCutoff(current, cutoff());
    }

    private TextView label(String value, int size) {
        TextView view = new TextView(getContext());
        view.setText(value);
        view.setTextSize(size);
        view.setTextColor(Palette.TEXT);
        return view;
    }

    private int dp(float value) { return Palette.dp(getContext(), value); }

    private static final class PressureLine extends SeekBar {
        private final Paint paint = new Paint(Paint.ANTI_ALIAS_FLAG);
        float livePressure;

        PressureLine(Context context) {
            super(context);
            // Pressure grows left-to-right on both the native slider and marker.
            setLayoutDirection(View.LAYOUT_DIRECTION_LTR);
            paint.setColor(0xFF86CFC3);
            paint.setStrokeWidth(Palette.dp(context, 2));
        }

        @Override
        protected synchronized void onDraw(Canvas canvas) {
            super.onDraw(canvas);
            float x = getPaddingLeft()
                    + livePressure * (getWidth() - getPaddingLeft() - getPaddingRight());
            float half = Palette.dp(getContext(), 14);
            canvas.drawLine(x, getHeight() / 2f - half, x, getHeight() / 2f + half, paint);
        }
    }
}
