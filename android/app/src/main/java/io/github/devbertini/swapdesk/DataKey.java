package io.github.devbertini.swapdesk;

import android.content.Context;
import android.security.keystore.KeyGenParameterSpec;
import android.security.keystore.KeyPermanentlyInvalidatedException;
import android.security.keystore.KeyProperties;

import java.io.File;
import java.io.FileOutputStream;
import java.nio.file.Files;
import java.nio.file.StandardCopyOption;
import java.security.GeneralSecurityException;
import java.security.KeyStore;
import java.security.SecureRandom;
import java.util.Arrays;

import javax.crypto.BadPaddingException;
import javax.crypto.Cipher;
import javax.crypto.KeyGenerator;
import javax.crypto.SecretKey;
import javax.crypto.spec.GCMParameterSpec;

/**
 * The data key that seals everything the core stores (docs/ANDROID.md 5): 32 random bytes that rest on disk only
 * wrapped by a key of the Android Keystore, which never leaves the secure hardware of the phone and cannot be
 * copied to another device. The core gets the bytes once, at its start.
 */
final class DataKey {
    private static final String ALIAS = "swapdesk-data-key";
    private static final String FILE = "datakey.bin";
    private static final String TRANSFORMATION = "AES/GCM/NoPadding";
    private static final int IV_LENGTH = 12;
    private static final int KEY_LENGTH = 32;
    /** A key store that did not answer is asked again, this many times in all, a little later each time. */
    private static final int ATTEMPTS = 3;
    private static final long PAUSE_MS = 250;

    private DataKey() {}

    /**
     * What the app sealed on this phone cannot be opened again, and waiting or asking again changes nothing: the
     * Keystore no longer holds the key that wrapped the data key, that key was invalidated for good, or the wrapped
     * file is not what this phone's key wrapped. Nothing replaces the files silently; the screen says so.
     */
    static final class Unreadable extends Exception {
        Unreadable(String why) {
            super(why);
        }
    }

    /**
     * The data key: unwrapped from the private folder of the app, or made and wrapped at the first start. Throws
     * {@link Unreadable} only for what is final; any other failure (a key store that did not answer) is tried
     * again a few times, and thrown as what it was when it stays.
     */
    static byte[] load(Context context) throws Exception {
        Exception last = null;
        for (int attempt = 0; attempt < ATTEMPTS; attempt++) {
            if (attempt > 0) {
                Thread.sleep(PAUSE_MS * attempt);
            }
            try {
                return loadOnce(context);
            } catch (Unreadable e) {
                throw e;
            } catch (Exception e) {
                last = e;
            }
        }
        throw last;
    }

    private static byte[] loadOnce(Context context) throws Exception {
        File file = new File(context.getFilesDir(), FILE);
        if (file.exists()) {
            byte[] wrapped = Files.readAllBytes(file.toPath());
            if (wrapped.length <= IV_LENGTH) {
                throw new Unreadable("the wrapped data key is too short");
            }
            SecretKey wrapping = wrappingKey(false);
            byte[] key;
            try {
                Cipher cipher = Cipher.getInstance(TRANSFORMATION);
                cipher.init(Cipher.DECRYPT_MODE, wrapping, new GCMParameterSpec(128, wrapped, 0, IV_LENGTH));
                key = cipher.doFinal(wrapped, IV_LENGTH, wrapped.length - IV_LENGTH);
            } catch (KeyPermanentlyInvalidatedException e) {
                throw new Unreadable("the Keystore's key was invalidated for good");
            } catch (BadPaddingException e) {
                // The authentication tag does not match: this phone's key did not wrap this file, or it changed.
                throw new Unreadable("the wrapped data key does not open with this phone's key");
            }
            if (key.length != KEY_LENGTH) {
                throw new Unreadable("the data key has another length");
            }
            return key;
        }
        byte[] key = new byte[KEY_LENGTH];
        new SecureRandom().nextBytes(key);
        Cipher cipher = Cipher.getInstance(TRANSFORMATION);
        cipher.init(Cipher.ENCRYPT_MODE, wrappingKey(true));
        byte[] iv = cipher.getIV();
        byte[] sealed = cipher.doFinal(key);
        byte[] wrapped = Arrays.copyOf(iv, iv.length + sealed.length);
        System.arraycopy(sealed, 0, wrapped, iv.length, sealed.length);
        // Written whole or not at all: a temporary file, flushed, then renamed over the name.
        File temp = new File(context.getFilesDir(), FILE + ".tmp");
        try (FileOutputStream out = new FileOutputStream(temp)) {
            out.write(wrapped);
            out.getFD().sync();
        }
        Files.move(temp.toPath(), file.toPath(), StandardCopyOption.ATOMIC_MOVE, StandardCopyOption.REPLACE_EXISTING);
        return key;
    }

    private static SecretKey wrappingKey(boolean create) throws Exception {
        KeyStore store = KeyStore.getInstance("AndroidKeyStore");
        store.load(null);
        SecretKey key = (SecretKey) store.getKey(ALIAS, null);
        if (key != null) {
            return key;
        }
        if (!create) {
            throw new Unreadable("the Keystore no longer holds the key that wrapped the data key");
        }
        KeyGenerator generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore");
        generator.init(new KeyGenParameterSpec.Builder(ALIAS, KeyProperties.PURPOSE_ENCRYPT | KeyProperties.PURPOSE_DECRYPT)
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .build());
        return generator.generateKey();
    }
}
