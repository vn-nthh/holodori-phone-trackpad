package dev.holodori.trackpad;

import android.content.ActivityNotFoundException;
import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.content.pm.PackageManager;
import android.content.res.ColorStateList;
import android.graphics.Typeface;
import android.os.Handler;
import android.os.Looper;
import android.provider.Settings;
import android.text.SpannableString;
import android.text.TextUtils;
import android.text.style.ForegroundColorSpan;
import android.text.style.UnderlineSpan;
import android.view.Gravity;
import android.view.View;
import android.widget.Button;
import android.widget.FrameLayout;
import android.widget.ImageButton;
import android.widget.LinearLayout;
import android.widget.ScrollView;
import android.widget.SeekBar;
import android.widget.Switch;
import android.widget.TextView;

/**
 * Setup flow: one screen per step. Connect (choose USB or Wi-Fi), Pair, Ready
 * (Start), plus a Preferences screen for things nobody needs every time. Which
 * screen is visible is derived from state, so Pair and Start never coexist.
 */
final class SetupView extends FrameLayout {
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

    private enum Screen { CONNECT, PAIR, READY, PREFERENCES }

    private enum PairStage { START, WAITING, PATTERN, MATCHED, FAILED }

    private static final long TOAST_MILLIS = 2_600;

    private final SharedPreferences preferences;
    private final Listener listener;
    private final Handler handler = new Handler(Looper.getMainLooper());

    // Header
    private final ImageButton backButton;
    private final TextView title;
    private final ImageButton prefsButton;

    // Screens
    private final View connectScreen;
    private final View pairScreen;
    private final View readyScreen;
    private final View preferencesScreen;

    // Pair
    private final View pairIntro;
    private final View pairPattern;
    private final TransportArtView pairArt;
    private final TextView pairLead;
    private final TextView pairDetail;
    private final Button pairButton;
    private final Button cancelPairButton;
    private final PairingLaneView lanes;
    private final TextView pairTetherLink;

    // Ready
    private final TransportArtView readyArt;
    private final TextView transportBadge;
    private final TextView readyTetherLink;
    private final Button startButton;

    // Preferences
    private final Switch thumbSwitch;
    private final View gapRow;
    private final TextView gapValue;
    private final SeekBar gapSeek;
    private final View legacyRow;
    private final Switch legacySwitch;
    private final View pairedRow;

    private final TextView toast;

    private boolean paired;
    private boolean pairing;
    private boolean preferencesOpen;
    private boolean choosingTransport;
    private PairStage pairStage = PairStage.START;
    private String pairDetailText = "";
    private V5Protocol.TransportKind transport;

    SetupView(Context context, Listener listener) {
        super(context);
        this.listener = listener;
        preferences = context.getSharedPreferences("trackpad", Context.MODE_PRIVATE);
        setBackgroundColor(Palette.BG);

        String savedTransport = preferences.getString("v5_transport", null);
        if ("wifi".equals(savedTransport)) transport = V5Protocol.TransportKind.WIFI;
        else if ("usb".equals(savedTransport)) transport = V5Protocol.TransportKind.USB;

        LinearLayout column = new LinearLayout(context);
        column.setOrientation(LinearLayout.VERTICAL);
        addView(column, new LayoutParams(LayoutParams.MATCH_PARENT, LayoutParams.MATCH_PARENT));

        // Header -----------------------------------------------------------
        LinearLayout header = new LinearLayout(context);
        header.setOrientation(LinearLayout.HORIZONTAL);
        header.setGravity(Gravity.CENTER_VERTICAL);
        header.setPadding(dp(12), dp(8), dp(12), 0);
        backButton = iconButton(R.drawable.ic_back, "Back");
        title = text("Doritrack", 17, Palette.TEXT);
        title.setTypeface(Typeface.create("sans-serif-medium", Typeface.NORMAL));
        title.setGravity(Gravity.CENTER);
        prefsButton = iconButton(R.drawable.ic_settings, "Preferences");
        header.addView(backButton, new LinearLayout.LayoutParams(dp(44), dp(44)));
        header.addView(title, new LinearLayout.LayoutParams(0, LayoutParams.WRAP_CONTENT, 1f));
        header.addView(prefsButton, new LinearLayout.LayoutParams(dp(44), dp(44)));
        column.addView(header, new LinearLayout.LayoutParams(
                LayoutParams.MATCH_PARENT, LayoutParams.WRAP_CONTENT
        ));

        FrameLayout body = new FrameLayout(context);
        column.addView(body, new LinearLayout.LayoutParams(LayoutParams.MATCH_PARENT, 0, 1f));

        // Connect ----------------------------------------------------------
        LinearLayout connect = new LinearLayout(context);
        connect.setOrientation(LinearLayout.VERTICAL);
        connect.setGravity(Gravity.CENTER);
        connect.setPadding(dp(24), 0, dp(24), dp(16));
        TextView connectLead = text("How is this phone connected to your PC?", 20, Palette.TEXT);
        connectLead.setGravity(Gravity.CENTER_HORIZONTAL);
        connect.addView(connectLead, wrap());
        LinearLayout cards = new LinearLayout(context);
        cards.setOrientation(LinearLayout.HORIZONTAL);
        cards.setGravity(Gravity.CENTER);
        cards.addView(
                transportCard(V5Protocol.TransportKind.USB, "USB cable"),
                cardParams()
        );
        cards.addView(
                transportCard(V5Protocol.TransportKind.WIFI, "Wi-Fi"),
                cardParams()
        );
        LinearLayout.LayoutParams cardsParams = wrap();
        cardsParams.topMargin = dp(20);
        connect.addView(cards, cardsParams);
        connectScreen = connect;
        body.addView(connect, fill());

        // Pair -------------------------------------------------------------
        FrameLayout pair = new FrameLayout(context);

        LinearLayout intro = new LinearLayout(context);
        intro.setOrientation(LinearLayout.HORIZONTAL);
        intro.setGravity(Gravity.CENTER_VERTICAL);
        intro.setPadding(dp(32), 0, dp(32), dp(16));
        pairArt = new TransportArtView(context);
        intro.addView(pairArt, new LinearLayout.LayoutParams(0, LayoutParams.WRAP_CONTENT, 5f));
        LinearLayout pairColumn = new LinearLayout(context);
        pairColumn.setOrientation(LinearLayout.VERTICAL);
        pairColumn.setPadding(dp(28), 0, 0, 0);
        pairLead = text("", 20, Palette.TEXT);
        pairDetail = text("", 13, Palette.MUTED);
        pairDetail.setMinLines(2);
        LinearLayout pairActions = new LinearLayout(context);
        pairActions.setOrientation(LinearLayout.HORIZONTAL);
        pairButton = primaryButton("Pair");
        cancelPairButton = quietButton("Cancel");
        pairActions.addView(pairButton, buttonParams());
        pairActions.addView(cancelPairButton, buttonParams());
        pairTetherLink = tetherLink();
        pairColumn.addView(pairLead, wrap());
        pairColumn.addView(pairDetail, spaced(wrap(), 0, 6, 0, 0));
        pairColumn.addView(pairTetherLink, spaced(wrap(), 0, 4, 0, 0));
        pairColumn.addView(pairActions, spaced(wrap(), 0, 18, 0, 0));
        intro.addView(pairColumn, new LinearLayout.LayoutParams(0, LayoutParams.WRAP_CONTENT, 6f));
        pairIntro = intro;
        pair.addView(intro, fill());

        LinearLayout pattern = new LinearLayout(context);
        pattern.setOrientation(LinearLayout.VERTICAL);
        pattern.setPadding(dp(24), 0, dp(24), dp(8));
        LinearLayout patternHeader = new LinearLayout(context);
        patternHeader.setOrientation(LinearLayout.HORIZONTAL);
        patternHeader.setGravity(Gravity.CENTER_VERTICAL);
        TextView patternLead = text("Tap the 8 lanes shown on your PC, in order.", 17, Palette.TEXT);
        patternHeader.addView(patternLead, new LinearLayout.LayoutParams(0, LayoutParams.WRAP_CONTENT, 1f));
        Button patternCancel = quietButton("Cancel");
        patternCancel.setOnClickListener(view -> listener.onPairCancelled());
        patternHeader.addView(patternCancel, new LinearLayout.LayoutParams(
                LayoutParams.WRAP_CONTENT, dp(40)
        ));
        pattern.addView(patternHeader, wrap());
        lanes = new PairingLaneView(
                context,
                preferences.getBoolean("thumb_mode", false),
                savedGap(),
                entered -> listener.onPatternEntered(entered)
        );
        pattern.addView(lanes, spaced(
                new LinearLayout.LayoutParams(LayoutParams.MATCH_PARENT, 0, 1f), 0, 8, 0, 0
        ));
        pairPattern = pattern;
        pair.addView(pattern, fill());
        pairScreen = pair;
        body.addView(pair, fill());

        // Ready ------------------------------------------------------------
        LinearLayout ready = new LinearLayout(context);
        ready.setOrientation(LinearLayout.HORIZONTAL);
        ready.setGravity(Gravity.CENTER_VERTICAL);
        ready.setPadding(dp(32), 0, dp(32), dp(16));
        readyArt = new TransportArtView(context);
        ready.addView(readyArt, new LinearLayout.LayoutParams(0, LayoutParams.WRAP_CONTENT, 5f));
        LinearLayout readyColumn = new LinearLayout(context);
        readyColumn.setOrientation(LinearLayout.VERTICAL);
        readyColumn.setPadding(dp(28), 0, 0, 0);
        TextView readyLead = text("Ready", 22, Palette.TEXT);
        transportBadge = text("", 13, Palette.MUTED);
        transportBadge.setBackground(Palette.ripple(context, Palette.BG, Palette.BORDER, 999));
        transportBadge.setPadding(dp(12), dp(6), dp(12), dp(6));
        transportBadge.setOnClickListener(view -> {
            if (pairing) return;
            choosingTransport = true;
            render();
        });
        startButton = primaryButton("Start");
        readyColumn.addView(readyLead, wrap());
        LinearLayout.LayoutParams badgeParams = new LinearLayout.LayoutParams(
                LayoutParams.WRAP_CONTENT, LayoutParams.WRAP_CONTENT
        );
        badgeParams.topMargin = dp(8);
        readyColumn.addView(transportBadge, badgeParams);
        readyTetherLink = tetherLink();
        readyColumn.addView(readyTetherLink, spaced(wrap(), 0, 10, 0, 0));
        LinearLayout.LayoutParams startParams = new LinearLayout.LayoutParams(
                LayoutParams.MATCH_PARENT, dp(56)
        );
        startParams.topMargin = dp(22);
        readyColumn.addView(startButton, startParams);
        ready.addView(readyColumn, new LinearLayout.LayoutParams(0, LayoutParams.WRAP_CONTENT, 6f));
        readyScreen = ready;
        body.addView(ready, fill());

        // Preferences ------------------------------------------------------
        ScrollView scroll = new ScrollView(context);
        scroll.setFillViewport(true);
        LinearLayout list = new LinearLayout(context);
        list.setOrientation(LinearLayout.VERTICAL);
        list.setPadding(dp(32), dp(4), dp(32), dp(16));

        thumbSwitch = switchControl();
        list.addView(prefRow("Thumb mode", "Two clusters with a gap in the middle.", thumbSwitch), wrap());

        LinearLayout gap = new LinearLayout(context);
        gap.setOrientation(LinearLayout.VERTICAL);
        gap.setPadding(0, dp(4), 0, dp(10));
        LinearLayout gapHeader = new LinearLayout(context);
        gapHeader.setOrientation(LinearLayout.HORIZONTAL);
        TextView gapLabel = text("Gap", 15, Palette.TEXT);
        gapValue = text("", 13, Palette.MUTED);
        gapHeader.addView(gapLabel, new LinearLayout.LayoutParams(0, LayoutParams.WRAP_CONTENT, 1f));
        gapHeader.addView(gapValue, new LinearLayout.LayoutParams(
                LayoutParams.WRAP_CONTENT, LayoutParams.WRAP_CONTENT
        ));
        gap.addView(gapHeader, wrap());
        gapSeek = new SeekBar(context);
        gapSeek.setMax(Math.round((ThumbTransform.MAX_GAP - ThumbTransform.MIN_GAP) * 1_000));
        gapSeek.setProgressTintList(ColorStateList.valueOf(Palette.TEXT));
        gapSeek.setProgressBackgroundTintList(ColorStateList.valueOf(Palette.BORDER_STRONG));
        gapSeek.setThumbTintList(ColorStateList.valueOf(Palette.TEXT));
        gap.addView(gapSeek, wrap());
        gapRow = gap;
        list.addView(gap, wrap());

        legacySwitch = switchControl();
        legacyRow = prefRow("Protocol v4", "No pairing. USB only.", legacySwitch);
        list.addView(legacyRow, wrap());

        Button forgetButton = quietButton("Forget");
        forgetButton.setOnClickListener(view -> listener.onForgetRequested());
        pairedRow = prefRow("Paired PC", null, forgetButton);
        list.addView(pairedRow, wrap());

        TextView footer = text(
                "Unofficial companion for hololive Dreams. v" + versionName(context),
                11,
                Palette.MUTED
        );
        footer.setGravity(Gravity.CENTER_HORIZONTAL);
        list.addView(footer, spaced(wrap(), 0, 18, 0, 0));
        scroll.addView(list, new LayoutParams(LayoutParams.MATCH_PARENT, LayoutParams.WRAP_CONTENT));
        preferencesScreen = scroll;
        body.addView(scroll, fill());

        // Toast ------------------------------------------------------------
        toast = text("", 13, Palette.TEXT);
        toast.setBackground(Palette.rounded(context, Palette.SURFACE_2, Palette.BORDER_STRONG, 10));
        toast.setPadding(dp(14), dp(10), dp(14), dp(10));
        toast.setVisibility(GONE);
        LayoutParams toastParams = new LayoutParams(LayoutParams.WRAP_CONTENT, LayoutParams.WRAP_CONTENT);
        toastParams.gravity = Gravity.BOTTOM | Gravity.CENTER_HORIZONTAL;
        toastParams.bottomMargin = dp(18);
        addView(toast, toastParams);

        // State ------------------------------------------------------------
        thumbSwitch.setChecked(preferences.getBoolean("thumb_mode", false));
        gapSeek.setProgress(Math.round((savedGap() - ThumbTransform.MIN_GAP) * 1_000));
        legacySwitch.setChecked(preferences.getBoolean("legacy_v4", false));

        backButton.setOnClickListener(view -> handleBack());
        prefsButton.setOnClickListener(view -> {
            preferencesOpen = true;
            render();
        });
        pairButton.setOnClickListener(view -> {
            saveSelection();
            listener.onPairRequested(selection());
        });
        cancelPairButton.setOnClickListener(view -> listener.onPairCancelled());
        startButton.setOnClickListener(view -> {
            saveSelection();
            listener.onStartRequested(selection());
        });
        thumbSwitch.setOnCheckedChangeListener((button, checked) -> {
            saveSelection();
            render();
        });
        legacySwitch.setOnCheckedChangeListener((button, checked) -> {
            saveSelection();
            render();
        });
        gapSeek.setOnSeekBarChangeListener(new SeekBar.OnSeekBarChangeListener() {
            @Override
            public void onProgressChanged(SeekBar seekBar, int progress, boolean fromUser) {
                gapValue.setText(Math.round(selectedGap() * 100) + "%");
            }

            @Override
            public void onStartTrackingTouch(SeekBar seekBar) {
            }

            @Override
            public void onStopTrackingTouch(SeekBar seekBar) {
                saveSelection();
            }
        });
        gapValue.setText(Math.round(selectedGap() * 100) + "%");
        render();
    }

    // --- MainActivity API ---------------------------------------------------

    Selection selection() {
        V5Protocol.TransportKind kind = transport == null ? V5Protocol.TransportKind.USB : transport;
        return new Selection(
                kind,
                legacySwitch.isChecked() && kind == V5Protocol.TransportKind.USB,
                thumbSwitch.isChecked(),
                selectedGap()
        );
    }

    void setPaired(boolean paired) {
        this.paired = paired;
        render();
    }

    /**
     * One step back: Preferences closes, Pair reopens the transport choice, and
     * a Connect screen opened via "Change" returns to where it was opened.
     * Returns false when there is nothing to go back to.
     */
    boolean handleBack() {
        if (pairing) return false;
        Screen screen = currentScreen();
        if (screen == Screen.PREFERENCES) {
            preferencesOpen = false;
        } else if (screen == Screen.PAIR) {
            choosingTransport = true;
        } else if (screen == Screen.CONNECT && transport != null && choosingTransport) {
            choosingTransport = false;
        } else {
            return false;
        }
        render();
        return true;
    }

    /** Called once when a fresh pairing attempt begins. */
    void startPairing() {
        pairing = true;
        preferencesOpen = false;
        choosingTransport = false;
        pairStage = PairStage.WAITING;
        pairDetailText = "";
        lanes.setVisibility(GONE);
        lanes.reset();
        lanes.configure(thumbSwitch.isChecked(), selectedGap());
        render();
    }

    void setPairingStatus(String message) {
        if (!pairing) startPairing();
        if (pairStage == PairStage.WAITING) pairDetailText = waitingDetail(message);
        render();
    }

    /** Turns transport progress lines into one short hint, or nothing. */
    private static String waitingDetail(String message) {
        if (message == null || message.startsWith("Opening selected")) return "";
        if (message.startsWith("Waiting for the host Pair window")) {
            return "Click Pair on your PC if you haven't yet.";
        }
        return message;
    }

    /** Turns transport failure lines into plain words where they are known. */
    private static String failureDetail(String message) {
        if (message.startsWith("Pattern did not match")) return "The pattern didn't match.";
        if (message.startsWith("Pairing failed.")) return "";
        return message;
    }

    void showPatternInput() {
        pairStage = PairStage.PATTERN;
        lanes.setAccepting(true);
        render();
    }

    void showPatternMatched() {
        pairStage = PairStage.MATCHED;
        lanes.setAccepting(false);
        render();
    }

    void setQuality(String message) {
        // Diagnostic link-quality text stays out of the setup screens.
    }

    void finishPairing(boolean success, String message) {
        pairing = false;
        lanes.setAccepting(false);
        if (success) {
            paired = true;
            pairStage = PairStage.START;
            pairDetailText = "";
            showToast("Paired");
        } else if (TextUtils.isEmpty(message) || "Pairing cancelled".equals(message)) {
            pairStage = PairStage.START;
            pairDetailText = "";
        } else {
            pairStage = PairStage.FAILED;
            pairDetailText = failureDetail(message);
        }
        render();
    }

    void showToast(String message) {
        handler.removeCallbacks(hideToast);
        toast.setText(message);
        toast.setAlpha(0f);
        toast.setVisibility(VISIBLE);
        toast.animate().alpha(1f).setDuration(160).start();
        handler.postDelayed(hideToast, TOAST_MILLIS);
    }

    private final Runnable hideToast = this::fadeOutToast;

    private void fadeOutToast() {
        toast.animate()
                .alpha(0f)
                .setDuration(200)
                .withEndAction(() -> toast.setVisibility(GONE))
                .start();
    }

    // --- Rendering -----------------------------------------------------------

    private Screen currentScreen() {
        if (pairing) return Screen.PAIR;
        if (preferencesOpen) return Screen.PREFERENCES;
        if (transport == null || choosingTransport) return Screen.CONNECT;
        if (!paired && !pairingSkipped()) return Screen.PAIR;
        return Screen.READY;
    }

    private boolean pairingSkipped() {
        return transport == V5Protocol.TransportKind.USB && legacySwitch.isChecked();
    }

    private void render() {
        Screen screen = currentScreen();
        boolean usb = transport != V5Protocol.TransportKind.WIFI;

        connectScreen.setVisibility(screen == Screen.CONNECT ? VISIBLE : GONE);
        pairScreen.setVisibility(screen == Screen.PAIR ? VISIBLE : GONE);
        readyScreen.setVisibility(screen == Screen.READY ? VISIBLE : GONE);
        preferencesScreen.setVisibility(screen == Screen.PREFERENCES ? VISIBLE : GONE);

        title.setText(screen == Screen.PREFERENCES ? "Preferences" : "Doritrack");
        boolean canGoBack = screen == Screen.PREFERENCES
                || (screen == Screen.CONNECT && transport != null && choosingTransport)
                || (screen == Screen.PAIR && !pairing);
        backButton.setVisibility(canGoBack ? VISIBLE : INVISIBLE);
        prefsButton.setVisibility(screen == Screen.PREFERENCES ? INVISIBLE : VISIBLE);
        prefsButton.setEnabled(!pairing);
        prefsButton.setAlpha(pairing ? 0.35f : 1f);

        // Pair
        boolean patternStage = pairStage == PairStage.PATTERN;
        pairIntro.setVisibility(patternStage ? GONE : VISIBLE);
        pairPattern.setVisibility(patternStage ? VISIBLE : GONE);
        lanes.setVisibility(patternStage ? VISIBLE : GONE);
        pairArt.setTransport(transport, true);
        switch (pairStage) {
            case WAITING:
                pairLead.setText("Looking for your PC…");
                pairDetail.setText(pairDetailText);
                pairArt.setState(TransportArtView.State.SEARCHING);
                break;
            case MATCHED:
                pairLead.setText("Pattern matched");
                pairDetail.setText("Now approve on your PC.");
                pairArt.setState(TransportArtView.State.CONNECTED);
                break;
            case FAILED:
                pairLead.setText("Pairing didn't finish.");
                pairDetail.setText(pairDetailText);
                pairArt.setState(TransportArtView.State.OFF);
                break;
            default:
                pairLead.setText("Click Pair on your PC, then tap Pair here.");
                pairDetail.setText("");
                pairArt.setState(TransportArtView.State.IDLE);
                break;
        }
        pairButton.setVisibility(pairing ? GONE : VISIBLE);
        pairButton.setText(pairStage == PairStage.FAILED ? "Try again" : "Pair");
        cancelPairButton.setVisibility(pairing ? VISIBLE : GONE);
        pairTetherLink.setVisibility(usb ? VISIBLE : GONE);
        readyTetherLink.setVisibility(usb ? VISIBLE : GONE);

        // Ready
        readyArt.setTransport(transport, true);
        readyArt.setState(TransportArtView.State.IDLE);
        String name = usb ? "USB cable" : "Wi-Fi";
        transportBadge.setText(pairingSkipped() ? name + " · v4 · Change" : name + " · Change");

        // Preferences
        gapRow.setVisibility(thumbSwitch.isChecked() ? VISIBLE : GONE);
        legacyRow.setVisibility(usb ? VISIBLE : GONE);
        pairedRow.setVisibility(paired ? VISIBLE : GONE);
    }

    // --- Persistence ---------------------------------------------------------

    private void saveSelection() {
        Selection selected = selection();
        SharedPreferences.Editor editor = preferences.edit()
                .putBoolean("legacy_v4", legacySwitch.isChecked())
                .putBoolean("thumb_mode", selected.thumbMode)
                .putFloat("thumb_gap", selected.thumbGap);
        if (transport != null) {
            editor.putString(
                    "v5_transport",
                    transport == V5Protocol.TransportKind.WIFI ? "wifi" : "usb"
            );
        }
        editor.apply();
    }

    private float savedGap() {
        return ThumbTransform.clampGap(
                preferences.getFloat("thumb_gap", ThumbTransform.DEFAULT_GAP)
        );
    }

    private float selectedGap() {
        return ThumbTransform.MIN_GAP + gapSeek.getProgress() / 1_000f;
    }

    private static String versionName(Context context) {
        try {
            return context.getPackageManager()
                    .getPackageInfo(context.getPackageName(), 0)
                    .versionName;
        } catch (PackageManager.NameNotFoundException error) {
            return "";
        }
    }

    // --- Widgets -------------------------------------------------------------

    private View transportCard(V5Protocol.TransportKind kind, String label) {
        Context context = getContext();
        LinearLayout card = new LinearLayout(context);
        card.setOrientation(LinearLayout.VERTICAL);
        card.setGravity(Gravity.CENTER_HORIZONTAL);
        card.setPadding(dp(18), dp(16), dp(18), dp(14));
        card.setBackground(Palette.ripple(context, Palette.SURFACE, Palette.BORDER, 16));
        card.setClickable(true);
        card.setFocusable(true);
        card.setContentDescription(label);
        TransportArtView art = new TransportArtView(context);
        art.setTransport(kind, false);
        card.addView(art, new LinearLayout.LayoutParams(LayoutParams.MATCH_PARENT, dp(88)));
        TextView caption = text(label, 15, Palette.TEXT);
        caption.setTypeface(Typeface.create("sans-serif-medium", Typeface.NORMAL));
        caption.setGravity(Gravity.CENTER_HORIZONTAL);
        card.addView(caption, spaced(wrap(), 0, 6, 0, 0));
        card.setOnClickListener(view -> {
            transport = kind;
            choosingTransport = false;
            saveSelection();
            render();
        });
        return card;
    }

    /** "USB tethering must be on. Open settings" with the last two words as a link. */
    private TextView tetherLink() {
        String lead = "USB tethering must be on. ";
        String action = "Open settings";
        SpannableString label = new SpannableString(lead + action);
        label.setSpan(new UnderlineSpan(), lead.length(), label.length(), 0);
        label.setSpan(new ForegroundColorSpan(Palette.TEXT), lead.length(), label.length(), 0);
        TextView link = text("", 13, Palette.MUTED);
        link.setText(label);
        link.setPadding(0, dp(6), 0, dp(6));
        link.setBackground(Palette.ripple(getContext(), Palette.BG, 0, 8));
        link.setClickable(true);
        link.setFocusable(true);
        link.setOnClickListener(view -> openTetherSettings());
        return link;
    }

    /** Opens the tethering page; falls back to broader settings pages when an OEM hides it. */
    private void openTetherSettings() {
        String[] actions = {
                "android.settings.TETHER_SETTINGS",
                Settings.ACTION_WIRELESS_SETTINGS,
                Settings.ACTION_SETTINGS,
        };
        for (String action : actions) {
            try {
                getContext().startActivity(new Intent(action));
                return;
            } catch (ActivityNotFoundException ignored) {
                // Try the next, broader page.
            }
        }
        showToast("Open Settings and turn on USB tethering.");
    }

    private LinearLayout.LayoutParams cardParams() {
        LinearLayout.LayoutParams params = new LinearLayout.LayoutParams(dp(220), LayoutParams.WRAP_CONTENT);
        params.setMargins(dp(8), 0, dp(8), 0);
        return params;
    }

    private View prefRow(String label, String hint, View control) {
        Context context = getContext();
        LinearLayout row = new LinearLayout(context);
        row.setOrientation(LinearLayout.HORIZONTAL);
        row.setGravity(Gravity.CENTER_VERTICAL);
        row.setMinimumHeight(dp(56));
        row.setPadding(0, dp(8), 0, dp(8));
        LinearLayout labels = new LinearLayout(context);
        labels.setOrientation(LinearLayout.VERTICAL);
        labels.addView(text(label, 15, Palette.TEXT), wrap());
        if (hint != null) labels.addView(text(hint, 12, Palette.MUTED), wrap());
        row.addView(labels, new LinearLayout.LayoutParams(0, LayoutParams.WRAP_CONTENT, 1f));
        row.addView(control, new LinearLayout.LayoutParams(
                LayoutParams.WRAP_CONTENT, LayoutParams.WRAP_CONTENT
        ));
        if (control instanceof Switch) {
            row.setClickable(true);
            row.setOnClickListener(view -> ((Switch) control).toggle());
        }
        View divider = new View(context);
        divider.setBackgroundColor(Palette.BORDER);
        LinearLayout wrapper = new LinearLayout(context);
        wrapper.setOrientation(LinearLayout.VERTICAL);
        wrapper.addView(row, wrap());
        wrapper.addView(divider, new LinearLayout.LayoutParams(LayoutParams.MATCH_PARENT, Math.max(1, dp(1))));
        return wrapper;
    }

    private Switch switchControl() {
        Switch control = new Switch(getContext());
        control.setThumbTintList(new ColorStateList(
                new int[][] {{android.R.attr.state_checked}, {}},
                new int[] {Palette.TEXT, Palette.MUTED}
        ));
        control.setTrackTintList(new ColorStateList(
                new int[][] {{android.R.attr.state_checked}, {}},
                new int[] {Palette.withAlpha(Palette.TEXT, 0.45f), Palette.BORDER_STRONG}
        ));
        return control;
    }

    private ImageButton iconButton(int drawable, String description) {
        ImageButton button = new ImageButton(getContext());
        button.setImageResource(drawable);
        button.setImageTintList(ColorStateList.valueOf(Palette.MUTED));
        button.setBackground(Palette.ripple(getContext(), Palette.BG, 0, 12));
        button.setContentDescription(description);
        return button;
    }

    private Button primaryButton(String label) {
        Button button = baseButton(label, Palette.ON_ACCENT);
        button.setBackground(Palette.pressable(
                getContext(), Palette.ACCENT, Palette.ACCENT_PRESSED, 0, 12
        ));
        button.setTypeface(Typeface.create("sans-serif-medium", Typeface.NORMAL));
        return button;
    }

    private Button quietButton(String label) {
        Button button = baseButton(label, Palette.TEXT);
        button.setBackground(Palette.pressable(
                getContext(), Palette.SURFACE_2, Palette.BORDER, Palette.BORDER_STRONG, 12
        ));
        return button;
    }

    private Button baseButton(String label, int color) {
        Button button = new Button(getContext());
        button.setText(label);
        button.setAllCaps(false);
        button.setTextColor(color);
        button.setTextSize(15);
        button.setStateListAnimator(null);
        button.setMinHeight(dp(48));
        button.setMinimumHeight(dp(48));
        button.setPadding(dp(22), 0, dp(22), 0);
        return button;
    }

    private LinearLayout.LayoutParams buttonParams() {
        LinearLayout.LayoutParams params = new LinearLayout.LayoutParams(0, dp(48), 1f);
        params.setMargins(0, 0, dp(8), 0);
        return params;
    }

    private TextView text(String value, float size, int color) {
        TextView view = new TextView(getContext());
        view.setText(value);
        view.setTextSize(size);
        view.setTextColor(color);
        return view;
    }

    private static LayoutParams fill() {
        return new LayoutParams(LayoutParams.MATCH_PARENT, LayoutParams.MATCH_PARENT);
    }

    private static LinearLayout.LayoutParams wrap() {
        return new LinearLayout.LayoutParams(LayoutParams.MATCH_PARENT, LayoutParams.WRAP_CONTENT);
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
        return Palette.dp(getContext(), value);
    }
}
