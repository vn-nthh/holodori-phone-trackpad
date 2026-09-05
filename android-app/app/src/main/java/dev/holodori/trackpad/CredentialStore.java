package dev.holodori.trackpad;

import android.content.Context;
import android.content.SharedPreferences;
import android.security.keystore.KeyGenParameterSpec;
import android.security.keystore.KeyProperties;
import android.util.Base64;

import com.southernstorm.noise.protocol.DHState;
import com.southernstorm.noise.protocol.Noise;

import java.nio.ByteBuffer;
import java.security.KeyStore;
import java.security.NoSuchAlgorithmException;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;

import javax.crypto.Cipher;
import javax.crypto.KeyGenerator;
import javax.crypto.SecretKey;
import javax.crypto.spec.GCMParameterSpec;

/** Android-Keystore-wrapped V5 installation identity and paired host key. */
final class CredentialStore {
    private static final String PREFERENCES = "doritrack-v5-credentials";
    private static final String BLOB_KEY = "encrypted-identity";
    private static final String KEY_ALIAS = "doritrack-v5-wrap";
    private static final byte[] AAD = "holodori-v5-android-credentials"
            .getBytes(StandardCharsets.US_ASCII);
    private static final int FORMAT_VERSION = 1;
    private static final int NONCE_SIZE = 12;
    private static final int KEY_SIZE = 32;
    private static final int PLAINTEXT_SIZE = 1 + KEY_SIZE + KEY_SIZE + 1 + KEY_SIZE;

    private final SharedPreferences preferences;
    private final String wrappingKeyAlias;

    CredentialStore(Context context) {
        this(context, PREFERENCES, KEY_ALIAS);
    }

    CredentialStore(Context context, String preferencesName, String wrappingKeyAlias) {
        Context application = context.getApplicationContext();
        preferences = (application == null ? context : application)
                .getSharedPreferences(preferencesName, Context.MODE_PRIVATE);
        this.wrappingKeyAlias = wrappingKeyAlias;
    }

    synchronized Identity loadOrCreate() throws CredentialException {
        Identity identity = load();
        if (identity != null) return identity;
        DHState dh = null;
        byte[] privateKey = new byte[KEY_SIZE];
        byte[] publicKey = new byte[KEY_SIZE];
        try {
            dh = Noise.createDH("25519");
            dh.generateKeyPair();
            dh.getPrivateKey(privateKey, 0);
            dh.getPublicKey(publicKey, 0);
            identity = new Identity(privateKey, publicKey, null);
            save(identity);
            return identity;
        } catch (NoSuchAlgorithmException error) {
            throw new CredentialException("X25519 is unavailable", error);
        } finally {
            Arrays.fill(privateKey, (byte) 0);
            Arrays.fill(publicKey, (byte) 0);
            if (dh != null) dh.destroy();
        }
    }

    synchronized Identity load() throws CredentialException {
        String encoded = preferences.getString(BLOB_KEY, null);
        if (encoded == null) return null;
        byte[] envelope;
        try {
            envelope = Base64.decode(encoded, Base64.NO_WRAP);
        } catch (IllegalArgumentException error) {
            throw new CredentialException("stored V5 credentials are corrupt", error);
        }
        if (envelope.length < 1 + NONCE_SIZE + 16 || envelope[0] != FORMAT_VERSION) {
            Arrays.fill(envelope, (byte) 0);
            throw new CredentialException("stored V5 credentials have an invalid format");
        }
        byte[] nonce = Arrays.copyOfRange(envelope, 1, 1 + NONCE_SIZE);
        byte[] encrypted = Arrays.copyOfRange(envelope, 1 + NONCE_SIZE, envelope.length);
        byte[] plaintext = null;
        try {
            Cipher cipher = Cipher.getInstance("AES/GCM/NoPadding");
            cipher.init(Cipher.DECRYPT_MODE, getWrappingKey(false), new GCMParameterSpec(128, nonce));
            cipher.updateAAD(AAD);
            plaintext = cipher.doFinal(encrypted);
            return decodeIdentity(plaintext);
        } catch (CredentialException error) {
            throw error;
        } catch (Exception error) {
            throw new CredentialException(
                    "could not unlock V5 credentials; use Forget device to reset them",
                    error
            );
        } finally {
            Arrays.fill(envelope, (byte) 0);
            Arrays.fill(nonce, (byte) 0);
            Arrays.fill(encrypted, (byte) 0);
            if (plaintext != null) Arrays.fill(plaintext, (byte) 0);
        }
    }

    synchronized boolean isPaired() throws CredentialException {
        Identity identity = load();
        try {
            return identity != null && identity.hasPairedHost();
        } finally {
            if (identity != null) identity.destroy();
        }
    }

    synchronized void savePairedHost(byte[] hostPublicKey) throws CredentialException {
        if (hostPublicKey == null || hostPublicKey.length != KEY_SIZE
                || isAllZero(hostPublicKey)) {
            throw new CredentialException("invalid host identity key");
        }
        Identity current = loadOrCreate();
        Identity paired = new Identity(current.privateKey, current.publicKey, hostPublicKey);
        try {
            save(paired);
        } finally {
            paired.destroy();
            current.destroy();
        }
    }

    synchronized void forgetDevice() throws CredentialException {
        Identity current;
        try {
            current = load();
        } catch (CredentialException unreadable) {
            // This is an explicit destructive user action. If an Android backup
            // restored the blob without its non-exportable Keystore key, reset
            // the unusable installation identity so pairing can recover.
            preferences.edit().remove(BLOB_KEY).commit();
            deleteWrappingKey();
            return;
        }
        if (current == null) return;
        Identity unpaired = new Identity(current.privateKey, current.publicKey, null);
        try {
            save(unpaired);
        } finally {
            unpaired.destroy();
            current.destroy();
        }
    }

    private void save(Identity identity) throws CredentialException {
        byte[] plaintext = encodeIdentity(identity);
        byte[] nonce = null;
        byte[] encrypted = null;
        byte[] envelope = null;
        try {
            Cipher cipher = Cipher.getInstance("AES/GCM/NoPadding");
            // Android Keystore generates the IV when randomized encryption is required.
            cipher.init(Cipher.ENCRYPT_MODE, getWrappingKey(true));
            nonce = cipher.getIV();
            cipher.updateAAD(AAD);
            encrypted = cipher.doFinal(plaintext);
            envelope = ByteBuffer.allocate(1 + nonce.length + encrypted.length)
                    .put((byte) FORMAT_VERSION)
                    .put(nonce)
                    .put(encrypted)
                    .array();
            boolean committed = preferences.edit()
                    .putString(BLOB_KEY, Base64.encodeToString(envelope, Base64.NO_WRAP))
                    .commit();
            if (!committed) throw new CredentialException("could not persist V5 credentials");
        } catch (CredentialException error) {
            throw error;
        } catch (Exception error) {
            throw new CredentialException("could not protect V5 credentials", error);
        } finally {
            Arrays.fill(plaintext, (byte) 0);
            if (nonce != null) Arrays.fill(nonce, (byte) 0);
            if (encrypted != null) Arrays.fill(encrypted, (byte) 0);
            if (envelope != null) Arrays.fill(envelope, (byte) 0);
        }
    }

    private static byte[] encodeIdentity(Identity identity) {
        ByteBuffer bytes = ByteBuffer.allocate(PLAINTEXT_SIZE);
        bytes.put((byte) FORMAT_VERSION);
        bytes.put(identity.privateKey);
        bytes.put(identity.publicKey);
        bytes.put((byte) (identity.pairedHostPublicKey == null ? 0 : 1));
        if (identity.pairedHostPublicKey == null) {
            bytes.put(new byte[KEY_SIZE]);
        } else {
            bytes.put(identity.pairedHostPublicKey);
        }
        return bytes.array();
    }

    private static Identity decodeIdentity(byte[] plaintext) throws CredentialException {
        if (plaintext.length != PLAINTEXT_SIZE) {
            throw new CredentialException("stored V5 credentials have an invalid length");
        }
        ByteBuffer bytes = ByteBuffer.wrap(plaintext);
        if (Byte.toUnsignedInt(bytes.get()) != FORMAT_VERSION) {
            throw new CredentialException("stored V5 credentials have an invalid version");
        }
        byte[] privateKey = new byte[KEY_SIZE];
        byte[] publicKey = new byte[KEY_SIZE];
        byte[] hostKey = new byte[KEY_SIZE];
        bytes.get(privateKey);
        bytes.get(publicKey);
        int paired = Byte.toUnsignedInt(bytes.get());
        bytes.get(hostKey);
        try {
            if (paired > 1 || isAllZero(privateKey) || isAllZero(publicKey)
                    || (paired == 1 && isAllZero(hostKey))) {
                throw new CredentialException("stored V5 credentials failed validation");
            }
            verifyKeyPair(privateKey, publicKey);
            return new Identity(privateKey, publicKey, paired == 1 ? hostKey : null);
        } finally {
            Arrays.fill(privateKey, (byte) 0);
            Arrays.fill(publicKey, (byte) 0);
            Arrays.fill(hostKey, (byte) 0);
        }
    }

    private SecretKey getWrappingKey(boolean create) throws Exception {
        KeyStore keyStore = KeyStore.getInstance("AndroidKeyStore");
        keyStore.load(null);
        if (keyStore.containsAlias(wrappingKeyAlias)) {
            return (SecretKey) keyStore.getKey(wrappingKeyAlias, null);
        }
        if (!create) throw new CredentialException("Android Keystore key is missing");
        KeyGenerator generator = KeyGenerator.getInstance(
                KeyProperties.KEY_ALGORITHM_AES,
                "AndroidKeyStore"
        );
        generator.init(new KeyGenParameterSpec.Builder(
                wrappingKeyAlias,
                KeyProperties.PURPOSE_ENCRYPT | KeyProperties.PURPOSE_DECRYPT
        )
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .setRandomizedEncryptionRequired(true)
                .setUserAuthenticationRequired(false)
                .build());
        return generator.generateKey();
    }

    private void deleteWrappingKey() throws CredentialException {
        try {
            KeyStore keyStore = KeyStore.getInstance("AndroidKeyStore");
            keyStore.load(null);
            if (keyStore.containsAlias(wrappingKeyAlias)) keyStore.deleteEntry(wrappingKeyAlias);
        } catch (Exception error) {
            throw new CredentialException("could not reset Android Keystore credentials", error);
        }
    }

    private static boolean isAllZero(byte[] bytes) {
        int combined = 0;
        for (byte value : bytes) combined |= value;
        return combined == 0;
    }

    private static void verifyKeyPair(byte[] privateKey, byte[] publicKey)
            throws CredentialException {
        DHState dh = null;
        byte[] derived = new byte[KEY_SIZE];
        try {
            dh = Noise.createDH("25519");
            dh.setPrivateKey(privateKey, 0);
            dh.getPublicKey(derived, 0);
            int difference = 0;
            for (int index = 0; index < KEY_SIZE; index++) {
                difference |= derived[index] ^ publicKey[index];
            }
            if (difference != 0) {
                throw new CredentialException("stored V5 identity key pair is inconsistent");
            }
        } catch (NoSuchAlgorithmException error) {
            throw new CredentialException("X25519 is unavailable", error);
        } finally {
            Arrays.fill(derived, (byte) 0);
            if (dh != null) dh.destroy();
        }
    }

    static final class Identity {
        final byte[] privateKey;
        final byte[] publicKey;
        final byte[] pairedHostPublicKey;

        Identity(byte[] privateKey, byte[] publicKey, byte[] pairedHostPublicKey) {
            this.privateKey = privateKey.clone();
            this.publicKey = publicKey.clone();
            this.pairedHostPublicKey = pairedHostPublicKey == null
                    ? null
                    : pairedHostPublicKey.clone();
        }

        boolean hasPairedHost() {
            return pairedHostPublicKey != null;
        }

        void destroy() {
            Arrays.fill(privateKey, (byte) 0);
            Arrays.fill(publicKey, (byte) 0);
            if (pairedHostPublicKey != null) {
                Arrays.fill(pairedHostPublicKey, (byte) 0);
            }
        }
    }

    static final class CredentialException extends Exception {
        private static final long serialVersionUID = 1L;

        CredentialException(String message) {
            super(message);
        }

        CredentialException(String message, Throwable cause) {
            super(message, cause);
        }
    }
}
