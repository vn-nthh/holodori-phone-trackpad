package dev.holodori.trackpad;

import android.annotation.SuppressLint;
import android.app.Activity;
import android.app.PendingIntent;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.hardware.usb.UsbAccessory;
import android.hardware.usb.UsbManager;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.view.View;
import android.view.WindowManager;

public final class MainActivity extends Activity
        implements UsbAccessoryTransport.Listener {
    private static final String ACTION_USB_PERMISSION =
            "dev.holodori.trackpad.USB_PERMISSION";
    private static final long RECONNECT_MIN_MILLIS = 500;
    private static final long RECONNECT_MAX_MILLIS = 4_000;

    private UsbManager usbManager;
    private UsbAccessoryTransport transport;
    private TrackpadView trackpadView;
    private UsbAccessory currentAccessory;
    private final Handler reconnectHandler = new Handler(Looper.getMainLooper());
    private long reconnectDelayMillis = RECONNECT_MIN_MILLIS;
    private boolean reconnectScheduled;
    private boolean destroyed;

    private final Runnable reconnectRunnable = () -> {
        reconnectScheduled = false;
        if (destroyed || transport.isRunning()) {
            return;
        }
        UsbAccessory[] accessories = usbManager.getAccessoryList();
        if (accessories == null || accessories.length == 0) {
            return;
        }
        UsbAccessory accessory = accessories[0];
        if (!usbManager.hasPermission(accessory)) {
            return;
        }
        openAccessory(accessory);
        if (!transport.isRunning()) {
            scheduleReconnect();
        }
    };

    private final BroadcastReceiver usbReceiver = new BroadcastReceiver() {
        @Override
        public void onReceive(Context context, Intent intent) {
            String action = intent.getAction();
            if (ACTION_USB_PERMISSION.equals(action)) {
                UsbAccessory accessory = getAccessory(intent);
                if (intent.getBooleanExtra(
                        UsbManager.EXTRA_PERMISSION_GRANTED, false
                ) && accessory != null) {
                    openAccessory(accessory);
                } else {
                    onConnectionChanged(false, "USB permission denied");
                }
            } else if (UsbManager.ACTION_USB_ACCESSORY_DETACHED.equals(action)) {
                // Android revokes accessory permission before this broadcast
                // on some Samsung builds. UsbAccessory.equals() reads the
                // serial and throws SecurityException after that revocation.
                // Always close the transport. A writer failure may already
                // have cleared currentAccessory while its exclusive parcel
                // descriptor is still being unwound.
                cancelReconnect();
                transport.close();
                currentAccessory = null;
                onConnectionChanged(false, "Connect the USB cable");
            }
        }
    };

    @SuppressLint("UnspecifiedRegisterReceiverFlag")
    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);
        hideSystemUi();

        usbManager = (UsbManager) getSystemService(Context.USB_SERVICE);
        transport = new UsbAccessoryTransport(this);
        trackpadView = new TrackpadView(this, transport);
        setContentView(trackpadView);

        IntentFilter filter = new IntentFilter();
        filter.addAction(ACTION_USB_PERMISSION);
        filter.addAction(UsbManager.ACTION_USB_ACCESSORY_DETACHED);
        if (Build.VERSION.SDK_INT >= 33) {
            registerReceiver(usbReceiver, filter, Context.RECEIVER_NOT_EXPORTED);
        } else {
            registerReceiver(usbReceiver, filter);
        }
        handleIntent(getIntent());
    }

    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        setIntent(intent);
        handleIntent(intent);
    }

    @Override
    protected void onResume() {
        super.onResume();
        hideSystemUi();
        if (currentAccessory == null) {
            UsbAccessory[] accessories = usbManager.getAccessoryList();
            if (accessories != null && accessories.length > 0) {
                requestOrOpen(accessories[0]);
            }
        }
    }

    @Override
    protected void onDestroy() {
        destroyed = true;
        cancelReconnect();
        unregisterReceiver(usbReceiver);
        transport.close();
        super.onDestroy();
    }

    private void handleIntent(Intent intent) {
        if (intent == null) return;
        UsbAccessory accessory = getAccessory(intent);
        if (accessory != null) {
            requestOrOpen(accessory);
        }
    }

    private static UsbAccessory getAccessory(Intent intent) {
        if (Build.VERSION.SDK_INT >= 33) {
            return intent.getParcelableExtra(
                    UsbManager.EXTRA_ACCESSORY, UsbAccessory.class
            );
        }
        //noinspection deprecation
        return intent.getParcelableExtra(UsbManager.EXTRA_ACCESSORY);
    }

    private void requestOrOpen(UsbAccessory accessory) {
        if (currentAccessory != null && transport.isRunning()) {
            return;
        }
        if (usbManager.hasPermission(accessory)) {
            openAccessory(accessory);
            return;
        }
        int flags = PendingIntent.FLAG_UPDATE_CURRENT;
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            flags |= PendingIntent.FLAG_MUTABLE;
        }
        PendingIntent permissionIntent = PendingIntent.getBroadcast(
                this,
                0,
                new Intent(ACTION_USB_PERMISSION).setPackage(getPackageName()),
                flags
        );
        usbManager.requestPermission(accessory, permissionIntent);
        onConnectionChanged(false, "Allow USB access to connect");
    }

    private void openAccessory(UsbAccessory accessory) {
        if (currentAccessory != null && transport.isRunning()) {
            return;
        }
        if (transport.open(usbManager, accessory)) {
            currentAccessory = accessory;
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
    public void onConnectionChanged(boolean connected, String message) {
        runOnUiThread(() -> {
            trackpadView.setConnectionStatus(connected, message);
            if (connected) {
                cancelReconnect();
                reconnectDelayMillis = RECONNECT_MIN_MILLIS;
            } else if (!destroyed) {
                currentAccessory = null;
                scheduleReconnect();
            }
        });
    }

    @Override
    public void onHostLaneCountChanged(int laneCount) {
        runOnUiThread(() -> trackpadView.setLaneCount(laneCount));
    }

    private void scheduleReconnect() {
        if (destroyed || reconnectScheduled || transport.isRunning()) {
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

}
