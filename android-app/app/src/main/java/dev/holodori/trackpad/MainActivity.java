package dev.holodori.trackpad;

import android.app.Activity;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.os.Process;
import android.view.View;
import android.view.WindowManager;

/** The USB-tethering/RNDIS front end. No Android USB host/accessory API is used. */
public final class MainActivity extends Activity implements TouchTransport.Listener {
    private static final long RECONNECT_MIN_MILLIS = 4;
    private static final long RECONNECT_MAX_MILLIS = 64;

    private TouchTransport transport;
    private TrackpadView trackpadView;
    private final Handler reconnectHandler = new Handler(Looper.getMainLooper());
    private long reconnectDelayMillis = RECONNECT_MIN_MILLIS;
    private boolean reconnectScheduled;
    private boolean destroyed;

    private final Runnable reconnectRunnable = () -> {
        reconnectScheduled = false;
        if (destroyed || transport.isRunning()) return;
        transport.open();
        if (!transport.isRunning()) scheduleReconnect();
    };

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_DISPLAY);
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        hideSystemUi();

        transport = new UdpTransport(this, this);
        trackpadView = new TrackpadView(this, transport);
        setContentView(trackpadView);
        transport.open();
    }

    @Override
    protected void onResume() {
        super.onResume();
        hideSystemUi();
        if (!transport.isRunning()) transport.open();
    }

    @Override
    protected void onDestroy() {
        destroyed = true;
        cancelReconnect();
        transport.close();
        super.onDestroy();
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
    public void onConnectionChanged(boolean connected, String message) {
        runOnUiThread(() -> {
            boolean transportRunning = transport.isRunning();
            boolean liveConnection = connected && transportRunning;
            trackpadView.setConnectionStatus(liveConnection, message);
            if (liveConnection) {
                cancelReconnect();
                reconnectDelayMillis = RECONNECT_MIN_MILLIS;
            } else if (!destroyed && !transportRunning) {
                scheduleReconnect();
            }
        });
    }

    @Override
    public void onHostLaneCountChanged(int laneCount) {
        runOnUiThread(() -> trackpadView.setLaneCount(laneCount));
    }

    private void scheduleReconnect() {
        if (destroyed || reconnectScheduled || transport.isRunning()) return;
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
}
