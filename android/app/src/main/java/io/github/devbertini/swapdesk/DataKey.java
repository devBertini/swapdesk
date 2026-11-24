package io.github.devbertini.swapdesk;

import android.content.Context;
import android.security.keystore.KeyGenParameterSpec;
import android.security.keystore.KeyProperties;

import java.io.File;
import java.io.FileOutputStream;
import java.nio.file.Files;
import java.nio.file.StandardCopyOption;
import java.security.GeneralSecurityException;
import java.security.KeyStore;
import java.security.SecureRandom;
import java.util.Arrays;

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

    private DataKey() {}

    /**
     * The data key: unwrapped from the private folder of the app, or made and wrapped at the first start. Throws
     * when the Keystore no longer opens what it wrapped (its key was lost): the sealed files are then unreadable,
     * and nothing here replaces them silently.
     */
    static byte[] load(Context context) throws Exception {
        File file = new File(context.getFilesDir(), FILE);
        if (file.exists()) {
            byte[] wrapped = Files.readAllBytes(file.toPath());
            if (wrapped.length <= IV_LENGTH) {
                throw new GeneralSecurityException("the wrapped data key is too short");
            }
            Cipher cipher = Cipher.getInstance(TRANSFORMATION);
            cipher.init(Cipher.DECRYPT_MODE, wrappingKey(false), new GCMParameterSpec(128, wrapped, 0, IV_LENGTH));
            byte[] key = cipher.doFinal(wrapped, IV_LENGTH, wrapped.length - IV_LENGTH);
            if (key.length != KEY_LENGTH) {
                throw new GeneralSecurityException("the data key has another length");
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
            throw new GeneralSecurityException("the Keystore no longer holds the key that wrapped the data key");
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
