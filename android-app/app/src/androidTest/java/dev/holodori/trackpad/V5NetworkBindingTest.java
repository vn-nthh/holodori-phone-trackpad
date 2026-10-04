package dev.holodori.trackpad;

import android.content.Context;
import android.net.ConnectivityManager;
import android.net.Network;
import android.net.NetworkCapabilities;
import android.net.wifi.WifiManager;
import android.os.Build;
import androidx.test.platform.app.InstrumentationRegistry;
import org.junit.Test;
import java.lang.reflect.Field;
import static org.junit.Assert.*;
import static org.junit.Assume.assumeTrue;

/** Run on Android 10+ connected to a private Wi-Fi subnet; no gameplay is sent. */
public final class V5NetworkBindingTest {
    @Test
    public void replacementBindingOwnsItsWifiLockIndependently() throws Exception {
        assumeTrue(Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q);
        Context context = InstrumentationRegistry.getInstrumentation().getTargetContext();
        ConnectivityManager manager =
                (ConnectivityManager) context.getSystemService(Context.CONNECTIVITY_SERVICE);
        assumeTrue(manager != null);
        boolean wifiAvailable = false;
        for (Network network : manager.getAllNetworks()) {
            NetworkCapabilities capabilities = manager.getNetworkCapabilities(network);
            wifiAvailable |= capabilities != null
                    && capabilities.hasTransport(NetworkCapabilities.TRANSPORT_WIFI)
                    && capabilities.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN);
        }
        assumeTrue("Connect the test phone to Wi-Fi", wifiAvailable);

        V5NetworkBinding first = V5NetworkBinding.open(context, V5Protocol.TransportKind.WIFI);
        V5NetworkBinding replacement = null;
        WifiManager.WifiLock firstLock = null;
        WifiManager.WifiLock replacementLock = null;
        try {
            Field field = V5NetworkBinding.class.getDeclaredField("wifiLock");
            field.setAccessible(true);
            firstLock = (WifiManager.WifiLock) field.get(first);
            assertNotNull(firstLock);
            assertTrue(firstLock.isHeld());
            replacement = V5NetworkBinding.open(context, V5Protocol.TransportKind.WIFI);
            replacementLock = (WifiManager.WifiLock) field.get(replacement);
            assertNotNull(replacementLock);
            assertTrue(replacementLock.isHeld());
            first.close();
            first.close(); // Late/repeated cleanup cannot release the new binding's lock.
            assertTrue(first.socket().isClosed());
            assertFalse(firstLock.isHeld());
            assertTrue(replacementLock.isHeld());
        } finally {
            first.close();
            if (replacement != null) replacement.close();
        }
        assertFalse(firstLock.isHeld());
        assertFalse(replacementLock.isHeld());
    }
}
