package dev.holodori.trackpad;

import android.content.Context;
import android.graphics.Color;
import android.graphics.drawable.GradientDrawable;
import android.graphics.drawable.RippleDrawable;
import android.graphics.drawable.StateListDrawable;

/** One cool-ink hue at different lightness; shared by every screen. */
final class Palette {
    static final int BG = 0xFF14161C;
    static final int SURFACE = 0xFF1C1F27;
    static final int SURFACE_2 = 0xFF252932;
    static final int BORDER = 0xFF30353F;
    static final int BORDER_STRONG = 0xFF4A505C;
    static final int MUTED = 0xFF8C93A1;
    static final int TEXT = 0xFFECEEF2;
    static final int ACCENT = 0xFFECEEF2;
    static final int ACCENT_PRESSED = 0xFFD7DBE3;
    static final int ON_ACCENT = 0xFF14161C;

    private Palette() {
    }

    static int withAlpha(int color, float alpha) {
        return Color.argb(
                Math.round(255 * Math.max(0f, Math.min(1f, alpha))),
                Color.red(color),
                Color.green(color),
                Color.blue(color)
        );
    }

    /** Solid rounded fill with an optional 1px outline; never a gradient. */
    static GradientDrawable rounded(Context context, int fill, int stroke, float radiusDp) {
        GradientDrawable drawable = new GradientDrawable();
        drawable.setShape(GradientDrawable.RECTANGLE);
        drawable.setColor(fill);
        drawable.setCornerRadius(dp(context, radiusDp));
        if (stroke != 0) drawable.setStroke(Math.max(1, dp(context, 1)), stroke);
        return drawable;
    }

    /** Pressed state darkens the fill; disabled state is handled by alpha. */
    static StateListDrawable pressable(
            Context context,
            int fill,
            int pressedFill,
            int stroke,
            float radiusDp
    ) {
        StateListDrawable states = new StateListDrawable();
        states.addState(
                new int[] {android.R.attr.state_pressed},
                rounded(context, pressedFill, stroke, radiusDp)
        );
        states.addState(new int[] {}, rounded(context, fill, stroke, radiusDp));
        return states;
    }

    static RippleDrawable ripple(Context context, int fill, int stroke, float radiusDp) {
        return new RippleDrawable(
                android.content.res.ColorStateList.valueOf(withAlpha(TEXT, 0.12f)),
                rounded(context, fill, stroke, radiusDp),
                rounded(context, Color.WHITE, 0, radiusDp)
        );
    }

    static int dp(Context context, float value) {
        return Math.round(value * context.getResources().getDisplayMetrics().density);
    }
}
