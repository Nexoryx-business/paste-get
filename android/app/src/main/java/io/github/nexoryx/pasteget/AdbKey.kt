// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import java.io.File
import java.math.BigInteger
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.security.KeyFactory
import java.security.KeyPairGenerator
import java.security.interfaces.RSAPrivateCrtKey
import java.security.spec.PKCS8EncodedKeySpec
import java.security.spec.RSAKeyGenParameterSpec
import java.util.Base64

/**
 * The app's key for Android's wireless debugging: RSA-2048, as adb requires, made by Android's
 * own provider and kept in the app's private files. Pairing it once ([BuiltInShell]) makes the
 * phone trust it like a paired computer.
 */
class AdbKey private constructor(val pkcs8: ByteArray, private val key: RSAPrivateCrtKey) {
    /** The public key as adb writes it in `adb_keys`: `<base64> <name>`. */
    fun adbPublicKey(): String = "${AdbPublicKey.encode(key.modulus, key.publicExponent)} $NAME"

    companion object {
        /** Shown in Settings → Wireless debugging → Paired devices. */
        const val NAME = "paste-get"
        private const val FILE = "adb.key"

        fun exists(filesDir: File): Boolean = File(filesDir, FILE).isFile

        fun loadOrCreate(filesDir: File): AdbKey {
            val file = File(filesDir, FILE)
            if (file.isFile) return of(file.readBytes())
            val generator = KeyPairGenerator.getInstance("RSA")
            generator.initialize(RSAKeyGenParameterSpec(2048, RSAKeyGenParameterSpec.F4))
            val pkcs8 = generator.generateKeyPair().private.encoded
            // The files directory is private to the app; write then rename, like the identity.
            val temp = File(filesDir, "$FILE.tmp")
            temp.writeBytes(pkcs8)
            if (!temp.renameTo(file)) throw IllegalStateException("could not save the debugging key")
            return of(pkcs8)
        }

        /** Removes the key: wireless debugging then forgets the app once it's unpaired there. */
        fun delete(filesDir: File) {
            File(filesDir, FILE).delete()
        }

        fun of(pkcs8: ByteArray): AdbKey {
            val key = KeyFactory.getInstance("RSA").generatePrivate(PKCS8EncodedKeySpec(pkcs8)) as RSAPrivateCrtKey
            return AdbKey(pkcs8, key)
        }
    }
}

/**
 * adb's public key format, `android_pubkey_encode` in AOSP's libcrypto_utils: the modulus size
 * in 32-bit words, `-1/n mod 2^32`, the modulus and `R^2 mod n` (R = 2^2048) little-endian, then
 * the exponent; all of it base64.
 */
object AdbPublicKey {
    private const val MODULUS_BYTES = 256
    private val TWO_32: BigInteger = BigInteger.ONE.shiftLeft(32)

    fun encode(modulus: BigInteger, exponent: BigInteger): String {
        require(modulus.bitLength() == MODULUS_BYTES * 8) { "adb keys are RSA-2048" }
        val n0inv = modulus.mod(TWO_32).modInverse(TWO_32).negate().mod(TWO_32)
        val rr = BigInteger.ONE.shiftLeft(MODULUS_BYTES * 8 * 2).mod(modulus)
        val buffer = ByteBuffer.allocate(4 + 4 + MODULUS_BYTES * 2 + 4).order(ByteOrder.LITTLE_ENDIAN)
        buffer.putInt(MODULUS_BYTES / 4)
        buffer.putInt(n0inv.toInt())
        buffer.put(littleEndian(modulus))
        buffer.put(littleEndian(rr))
        buffer.putInt(exponent.toInt())
        return Base64.getEncoder().encodeToString(buffer.array())
    }

    private fun littleEndian(value: BigInteger): ByteArray {
        val bigEndian = value.toByteArray().dropWhile { it == 0.toByte() }
        return ByteArray(MODULUS_BYTES) { i -> bigEndian.getOrElse(bigEndian.size - 1 - i) { 0 } }
    }
}
