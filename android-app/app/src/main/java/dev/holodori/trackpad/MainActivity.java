package dev.holodori.trackpad;

import android.app.Activity;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.os.Process;
import android.view.View;
import android.view.WindowManager;

/** Explicit V5 setup/pairing front end and latency-sensitive play surface. */
public final class MainActivity extends Activity implements
        TouchTransport.Listener,
        SetupView.Listener {
    private static final long RECONNECT_MIN_MILLIS = 4;
    private static final long RECONNECT_MAX_MILLIS = 64;

    private TouchTransport transport;
    private TrackpadView trackpadView;
    private SetupView setupView;
    private final Handler reconnectHandler = new Handler(Looper.getMainLooper());
    private long reconnectDelayMillis = RECONNECT_MIN_MILLIS;
    private boolean reconnectScheduled;
    private boolean playing;
    private boolean destroyed;

    private final Runnable reconnectRunnable = () -> {
        reconnectScheduled = false;
        if (destroyed || !playing || transport == null || transport.isRunning()) return;
        transport.open();
        if (!transport.isRunning()) scheduleReconnect();
    };

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_DISPLAY);
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        hideSystemUi();
        showSetup();
    }

    @Override
    protected void onResume() {
        super.onResume();
        hideSystemUi();
        if (playing && transport != null && !transport.isRunning()) {
            transport.open();
        }
    }

    @Override
    protected void onDestroy() {
        destroyed = true;
        stopCurrent();
        super.onDestroy();
    }

    @Override
    public void onBackPressed() {
        if (playing || transport instanceof V5Transport) {
            stopCurrent();
            showSetup();
            return;
        }
        if (setupView != null && setupView.handleBack()) return;
        super.onBackPressed();
    }

    private void showSetup() {
        playing = false;
        trackpadView = null;
        setupView = new SetupView(this, this);
        setContentView(setupView);
        try {
            setupView.setPaired(new CredentialStore(this).isPaired());
        } catch (CredentialStore.CredentialException error) {
            setupView.setPaired(false);
            setupView.finishPairing(false, error.getMessage());
        }
    }

    private void hideSystemUi() {
        getWindow().getDecorView().setSystemUiVisibility(
                View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY
                        | View.SYSTEM_UI_FLAG_FULLSCREEN
                        | View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                        | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                        | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                        | View.SYSTEM_UI_FLAG_LAYOUT_STABLE
        );
    }

    @Override
    public void onPairRequested(SetupView.Selection selection) {
        stopCurrent();
        setupView.startPairing();
        V5Transport v5 = new V5Transport(this, this, selection.transport);
        transport = v5;
        v5.startPairing(pairingListenerFor(v5));
    }

    @Override
    public void onPairCancelled() {
        if (transport instanceof V5Transport) {
            ((V5Transport) transport).cancelPairing();
        }
        transport = null;
        setupView.finishPairing(false, "Pairing cancelled");
    }

    @Override
    public void onStartRequested(SetupView.Selection selection) {
        stopCurrent();
        transport = selection.legacyV4
                ? new UdpTransport(this, this)
                : new V5Transport(this, this, selection.transport);
        trackpadView = new TrackpadView(
                this,
                transport,
                selection.thumbMode,
                selection.thumbGap
        );
        setupView = null;
        playing = true;
        reconnectDelayMillis = RECONNECT_MIN_MILLIS;
        setContentView(trackpadView);
        if (!transport.open()) scheduleReconnect();
    }

    @Override
    public void onForgetRequested() {
        try {
            new CredentialStore(this).forgetDevice();
            setupView.setPaired(false);
            setupView.showToast("Forgotten. Forget this phone on your PC too.");
        } catch (CredentialStore.CredentialException error) {
            setupView.showToast(error.getMessage());
        }
    }

    @Override
    public void onPatternEntered(int[] lanes) {
        if (!(transport instanceof V5Transport)
                || !((V5Transport) transport).submitPairingPattern(lanes)) {
            setupView.finishPairing(false, "Pairing attempt is no longer active");
        }
    }

    @Override
    public void onConnectionChanged(boolean connected, String message) {
        runOnUiThread(() -> {
            if (!playing || trackpadView == null || transport == null) return;
            boolean transportRunning = transport.isRunning();
            boolean liveConnection = connected && transportRunning;
            trackpadView.setConnectionStatus(liveConnection, playStatus(liveConnection, message));
            if (liveConnection) {
                cancelReconnect();
                reconnectDelayMillis = RECONNECT_MIN_MILLIS;
            } else if (!destroyed && !transportRunning) {
                scheduleReconnect();
            }
        });
    }

    /** Short status line for the pad; transport diagnostics stay in the log. */
    private static String playStatus(boolean connected, String message) {
        if (connected) return "Connected";
        if (message == null) return "Looking for your PC…";
        if (message.startsWith("Searching for")) return "Looking for your PC…";
        if (message.contains("lost") || message.contains("interrupted")) return "Reconnecting…";
        if (message.startsWith("Pair this phone")) return "Pair this phone first.";
        return message;
    }

    @Override
    public void onHostLaneCountChanged(int laneCount) {
        runOnUiThread(() -> {
            if (trackpadView != null) trackpadView.setLaneCount(laneCount);
        });
    }

    private V5Transport.PairingListener pairingListenerFor(V5Transport owner) {
        return new V5Transport.PairingListener() {
            private void update(Runnable action) {
                runOnUiThread(() -> {
                    if (transport == owner && setupView != null) action.run();
                });
            }

            @Override
            public void onPairingStatus(String message) {
                update(() -> setupView.setPairingStatus(message));
            }

            @Override
            public void onPatternReady() {
                update(() -> setupView.showPatternInput());
            }

            @Override
            public void onPatternMatched() {
                update(() -> setupView.showPatternMatched());
            }

            @Override
            public void onQuality(String message) {
                update(() -> setupView.setQuality(message));
            }

            @Override
            public void onPairingComplete() {
                update(() -> {
                    transport = null;
                    setupView.finishPairing(true, null);
                });
            }

            @Override
            public void onPairingFailed(String message) {
                update(() -> {
                    transport = null;
                    setupView.finishPairing(false, message);
                });
            }
        };
    }

    private void scheduleReconnect() {
        if (destroyed || !playing || reconnectScheduled
                || transport == null || transport.isRunning()) {
            return;
        }
        reconnectScheduled = true;
        reconnectHandler.postDelayed(reconnectRunnable, reconnectDelayMillis);
        reconnectDelayMillis = Math.min(
                RECONNECT_MAX_MILLIS,
                reconnectDelayMillis * 2
        );
    }

    private void cancelReconnect() {
        reconnectHandler.removeCallbacks(reconnectRunnable);
        reconnectScheduled = false;
    }

    private void stopCurrent() {
        cancelReconnect();
        if (transport != null) transport.close();
        transport = null;
    }
}
