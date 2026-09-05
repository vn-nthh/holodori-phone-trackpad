package dev.holodori.trackpad;

import android.content.Context;
import androidx.test.platform.app.InstrumentationRegistry;
import org.junit.Test;
import java.security.KeyStore;
import java.util.Arrays;
import java.util.UUID;
import static org.junit.Assert.*;

/** Requires Android's real Keystore provider; a desktop JVM cannot validate IV policy. */
public final class CredentialStoreTest {
    @Test
    public void firstUsePairingReloadAndForgetRoundTripThroughKeystore() throws Exception {
        Context context = InstrumentationRegistry.getInstrumentation().getTargetContext();
        String name = "v5-credential-test-" + UUID.randomUUID();
        CredentialStore store = new CredentialStore(context, name, name);
        CredentialStore.Identity created = null;
        CredentialStore.Identity reloaded = null;
        CredentialStore.Identity paired = null;
        CredentialStore.Identity forgotten = null;
        try {
            created = store.loadOrCreate();
            reloaded = store.load();
            assertNotNull(reloaded);
            assertArrayEquals(created.privateKey, reloaded.privateKey);
            assertArrayEquals(created.publicKey, reloaded.publicKey);
            assertFalse(reloaded.hasPairedHost());
            byte[] host = new byte[32];
            Arrays.fill(host, (byte) 0x42);
            store.savePairedHost(host);
            paired = store.load();
            assertTrue(store.isPaired());
            assertArrayEquals(created.privateKey, paired.privateKey);
            assertArrayEquals(host, paired.pairedHostPublicKey);
            store.forgetDevice();
            forgotten = store.load();
            assertArrayEquals(created.privateKey, forgotten.privateKey);
            assertFalse(forgotten.hasPairedHost());
        } finally {
            if (created != null) created.destroy();
            if (reloaded != null) reloaded.destroy();
            if (paired != null) paired.destroy();
            if (forgotten != null) forgotten.destroy();
            context.getSharedPreferences(name, Context.MODE_PRIVATE).edit().clear().commit();
            KeyStore keys = KeyStore.getInstance("AndroidKeyStore");
            keys.load(null);
            if (keys.containsAlias(name)) keys.deleteEntry(name);
        }
    }
}
