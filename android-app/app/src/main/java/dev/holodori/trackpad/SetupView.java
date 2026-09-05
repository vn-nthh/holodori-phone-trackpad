package dev.holodori.trackpad;

import android.content.Context;
import android.content.SharedPreferences;
import android.graphics.Color;
import android.view.Gravity;
import android.view.View;
import android.widget.Button;
import android.widget.CheckBox;
import android.widget.LinearLayout;
import android.widget.RadioButton;
import android.widget.RadioGroup;
import android.widget.ScrollView;
import android.widget.SeekBar;
import android.widget.TextView;

/** Explicit transport, pairing, legacy, and thumb-mode setup surface. */
final class SetupView extends ScrollView {
    interface Listener {
        void onPairRequested(Selection selection);

        void onPairCancelled();

        void onStartRequested(Selection selection);

        void onForgetRequested();

        void onPatternEntered(int[] lanes);
    }

    static final class Selection {
        final V5Protocol.TransportKind transport;
        final boolean legacyV4;
        final boolean thumbMode;
        final float thumbGap;

        Selection(
                V5Protocol.TransportKind transport,
                boolean legacyV4,
                boolean thumbMode,
                float thumbGap
        ) {
            this.transport = transport;
            this.legacyV4 = legacyV4;
            this.thumbMode = thumbMode;
            this.thumbGap = thumbGap;
        }
    }

    private static final int BACKGROUND = Color.rgb(9, 10, 18);
    private static final int TEXT = Color.rgb(215, 244, 247);
    private static final int MUTED = Color.rgb(130, 157, 164);
    private static final int ACCENT = Color.rgb(66, 217, 245);
    private final SharedPreferences preferences;
    private final Listener listener;
    private final RadioButton usb;
    private final RadioButton wifi;
    private final CheckBox legacy;
    private final CheckBox thumb;
    private final SeekBar gap;
    private final TextView gapLabel;
    private final TextView pairedLabel;
    private final TextView status;
    private final TextView quality;
    private final Button pair;
    private final Button start;
    private final Button forget;
    private final PairingLaneView lanes;

    private boolean paired;
    private boolean pairing;

    SetupView(Context context, Listener listener) {
        super(context);
        this.listener = listener;
        preferences = context.getSharedPreferences("trackpad", Context.MODE_PRIVATE);
        setFillViewport(true);
        setBackgroundColor(BACKGROUND);

        LinearLayout root = new LinearLayout(context);
        root.setOrientation(LinearLayout.VERTICAL);
        root.setPadding(dp(28), dp(18), dp(28), dp(22));
        addView(root, new LayoutParams(LayoutParams.MATCH_PARENT, LayoutParams.WRAP_CONTENT));

        TextView title = text("Doritrack · Protocol V5", 24, TEXT);
        title.setGravity(Gravity.CENTER_HORIZONTAL);
        root.addView(title, matchWrap());

        TextView intro = text(
                "Choose the exact local transport before Pair or Start. V5 never downgrades silently.",
                13,
                MUTED
        );
        intro.setGravity(Gravity.CENTER_HORIZONTAL);
        root.addView(intro, spaced(matchWrap(), 0, 2, 0, 10));

        RadioGroup transportGroup = new RadioGroup(context);
        transportGroup.setOrientation(LinearLayout.HORIZONTAL);
        transportGroup.setGravity(Gravity.CENTER_HORIZONTAL);
        usb = radio("USB tethering");
        wifi = radio("Wi-Fi / local network");
        transportGroup.addView(usb);
        transportGroup.addView(wifi);
        root.addView(transportGroup, matchWrap());

        String savedTransport = preferences.getString("v5_transport", "usb");
        if ("wifi".equals(savedTransport)) wifi.setChecked(true);
        else usb.setChecked(true);

        pairedLabel = text("No paired host", 14, MUTED);
        pairedLabel.setGravity(Gravity.CENTER_HORIZONTAL);
        root.addView(pairedLabel, spaced(matchWrap(), 0, 8, 0, 2));

        status = text("Pair once, then Start with either selected transport.", 13, MUTED);
        status.setGravity(Gravity.CENTER_HORIZONTAL);
        root.addView(status, matchWrap());

        quality = text("Wi-Fi accepts 2.4, 5, and 6 GHz; measured path quality decides warnings.", 12, MUTED);
        quality.setGravity(Gravity.CENTER_HORIZONTAL);
        root.addView(quality, spaced(matchWrap(), 0, 2, 0, 8));

        LinearLayout actions = new LinearLayout(context);
        actions.setOrientation(LinearLayout.HORIZONTAL);
        actions.setGravity(Gravity.CENTER_HORIZONTAL);
        pair = button("Pair");
        start = button("Start");
        forget = button("Forget device");
        actions.addView(pair, weighted());
        actions.addView(start, weighted());
        actions.addView(forget, weighted());
        root.addView(actions, matchWrap());

        legacy = checkBox("Legacy V4 (unpaired USB only)");
        legacy.setChecked(preferences.getBoolean("legacy_v4", false) && usb.isChecked());
        root.addView(legacy, spaced(matchWrap(), 0, 6, 0, 0));

        thumb = checkBox("Thumb mode: split into two three-lane clusters");
        thumb.setChecked(preferences.getBoolean("thumb_mode", false));
        root.addView(thumb, matchWrap());

        float savedGap = ThumbTransform.clampGap(
                preferences.getFloat("thumb_gap", ThumbTransform.DEFAULT_GAP)
        );
        gapLabel = text("Center gap: " + Math.round(savedGap * 100) + "%", 12, MUTED);
        root.addView(gapLabel, matchWrap());
        gap = new SeekBar(context);
        gap.setMax(Math.round((ThumbTransform.MAX_GAP - ThumbTransform.MIN_GAP) * 1_000));
        gap.setProgress(Math.round((savedGap - ThumbTransform.MIN_GAP) * 1_000));
        root.addView(gap, matchWrap());

        lanes = new PairingLaneView(
                context,
                thumb.isChecked(),
                selectedGap(),
                entered -> listener.onPatternEntered(entered)
        );
        lanes.setVisibility(GONE);
        root.addView(lanes, new LinearLayout.LayoutParams(
                LinearLayout.LayoutParams.MATCH_PARENT,
                dp(170)
        ));

        transportGroup.setOnCheckedChangeListener((group, checkedId) -> updateControls());
        legacy.setOnCheckedChangeListener((button, checked) -> updateControls());
        thumb.setOnCheckedChangeListener((button, checked) -> {
            gap.setEnabled(checked && !pairing);
            // Pairing geometry is frozen when Pair begins. Rebuild is handled
            // by MainActivity when the user changes setup before an attempt.
        });
        gap.setOnSeekBarChangeListener(new SeekBar.OnSeekBarChangeListener() {
            @Override
            public void onProgressChanged(SeekBar seekBar, int progress, boolean fromUser) {
                gapLabel.setText("Center gap: " + Math.round(selectedGap() * 100) + "%");
            }

            @Override
            public void onStartTrackingTouch(SeekBar seekBar) {
            }

            @Override
            public void onStopTrackingTouch(SeekBar seekBar) {
            }
        });
        pair.setOnClickListener(view -> {
            if (pairing) listener.onPairCancelled();
            else {
                saveSelection();
                listener.onPairRequested(selection());
            }
        });
        start.setOnClickListener(view -> {
            saveSelection();
            listener.onStartRequested(selection());
        });
        forget.setOnClickListener(view -> listener.onForgetRequested());
        updateControls();
    }

    Selection selection() {
        return new Selection(
                wifi.isChecked()
                        ? V5Protocol.TransportKind.WIFI
                        : V5Protocol.TransportKind.USB,
                legacy.isChecked() && usb.isChecked(),
                thumb.isChecked(),
                selectedGap()
        );
    }

    void setPaired(boolean paired) {
        this.paired = paired;
        pairedLabel.setText(paired ? "Paired host remembered securely" : "No paired host");
        pairedLabel.setTextColor(paired ? ACCENT : MUTED);
        updateControls();
    }

    void setPairingStatus(String message) {
        pairing = true;
        status.setText(message);
        lanes.setVisibility(GONE);
        lanes.reset();
        lanes.configure(thumb.isChecked(), selectedGap());
        updateControls();
    }

    void showPatternInput() {
        status.setText("Replicate all 8 numbered steps shown on the host.");
        lanes.setVisibility(VISIBLE);
        lanes.setAccepting(true);
    }

    void showPatternMatched() {
        status.setText("Pattern matched. Approve pairing on the real host now.");
        lanes.setAccepting(false);
    }

    void setQuality(String message) {
        quality.setText(message);
    }

    void finishPairing(boolean success, String message) {
        pairing = false;
        lanes.setAccepting(false);
        lanes.setVisibility(GONE);
        status.setText(message);
        if (success) setPaired(true);
        updateControls();
    }

    private void updateControls() {
        boolean wifiSelected = wifi.isChecked();
        if (wifiSelected && legacy.isChecked()) legacy.setChecked(false);
        usb.setEnabled(!pairing);
        wifi.setEnabled(!pairing);
        legacy.setEnabled(!pairing && !wifiSelected);
        thumb.setEnabled(!pairing);
        gap.setEnabled(!pairing && thumb.isChecked());
        pair.setText(pairing ? "Cancel pairing" : "Pair");
        start.setEnabled(!pairing && (paired || legacy.isChecked()));
        forget.setEnabled(!pairing && paired);
    }

    private void saveSelection() {
        Selection selected = selection();
        preferences.edit()
                .putString("v5_transport", selected.transport == V5Protocol.TransportKind.WIFI
                        ? "wifi"
                        : "usb")
                .putBoolean("legacy_v4", selected.legacyV4)
                .putBoolean("thumb_mode", selected.thumbMode)
                .putFloat("thumb_gap", selected.thumbGap)
                .apply();
    }

    private float selectedGap() {
        return ThumbTransform.MIN_GAP + gap.getProgress() / 1_000f;
    }

    private TextView text(String value, float size, int color) {
        TextView view = new TextView(getContext());
        view.setText(value);
        view.setTextSize(size);
        view.setTextColor(color);
        return view;
    }

    private RadioButton radio(String value) {
        RadioButton button = new RadioButton(getContext());
        button.setText(value);
        button.setTextColor(TEXT);
        button.setId(View.generateViewId());
        return button;
    }

    private CheckBox checkBox(String value) {
        CheckBox checkBox = new CheckBox(getContext());
        checkBox.setText(value);
        checkBox.setTextColor(TEXT);
        return checkBox;
    }

    private Button button(String value) {
        Button button = new Button(getContext());
        button.setText(value);
        return button;
    }

    private LinearLayout.LayoutParams matchWrap() {
        return new LinearLayout.LayoutParams(
                LinearLayout.LayoutParams.MATCH_PARENT,
                LinearLayout.LayoutParams.WRAP_CONTENT
        );
    }

    private LinearLayout.LayoutParams weighted() {
        return new LinearLayout.LayoutParams(0, LinearLayout.LayoutParams.WRAP_CONTENT, 1f);
    }

    private LinearLayout.LayoutParams spaced(
            LinearLayout.LayoutParams parameters,
            int left,
            int top,
            int right,
            int bottom
    ) {
        parameters.setMargins(dp(left), dp(top), dp(right), dp(bottom));
        return parameters;
    }

    private int dp(float value) {
        return Math.round(value * getResources().getDisplayMetrics().density);
    }
}
