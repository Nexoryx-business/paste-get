// SPDX-License-Identifier: GPL-3.0-or-later

package io.github.nexoryx.pasteget

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder

class AdbKeyTest {
    @get:Rule
    val folder = TemporaryFolder()

    /** The test key of the Rust ADB client (`ffi/src/wireless_adb/test_key.pk8`). */
    private fun testKey(): ByteArray = javaClass.getResourceAsStream("/adb_test_key.pk8")!!.readBytes()

    @Test
    fun publicKeysAreInAdbFormat() {
        // Computed separately, with Python, from the key's modulus, following
        // android_pubkey_encode.
        val expected = "QAAAAAF7Me7/ehigUPCfeRwYgXomVEaheaa6uQlI/1Jc2Djh/vZFtKeGeiM9WQ1u50iBvs5F0+t7+85GDOfb4g1g" +
            "q96EaMURbY3IfPUCRNAtjR4JGGdJbROEfFHwTca7pQ8ZvDqJazKA8yqI7zdw2j8CPM0u2SDwvbIhC2rJMdkTmoCzl7Qhjk+M" +
            "ayBTdd+uZd1oBryPlP1hSss4xuPFzeZNF/qqniCZPwzkjoF1O7H//clBL0LiPr8RO/EkqCYNHsplS7LSWnP3sglvl48Qfl9Q" +
            "ngkyhBzgPmIVbhKeYOCUcLSkSQV9TGf2jJR68wrUCDybvnVUlN2RktGAukjXJNeSkA4yJRnNOTHMLQsLHYp2YKOlJWKq+CNv" +
            "PCPe0b29kS1hx7N+Z7yJ8zrLnO9Oi4DMTGujWXwBtXr2WVz7TRF3yY9LvrBSlsVd45M8OqU1AALO7Q8yTIiIfKMp0/CcXDcp" +
            "NPQE27IoAojSXasGpalVJgB4XeVYHx/p4eUNRKLgdW6XAu8MdjiWU6aAObMhiW59FqMfsIsHU5Q2nxSfyZnNfdqj9oMgJpL6" +
            "F6ZDtb9Dqk2hpRCrEUK8ryY3e70PHKyAH6JPfNfNW/fI1IbEpJb65f9UsNsQz09qzqcv2gOOZas/f+4i6Bus2V9XCUNJGz+K" +
            "BPZSQ9D0HKDpg3k2itPmkPpySQEzZwEAAQA="

        assertEquals("$expected paste-get", AdbKey.of(testKey()).adbPublicKey())
    }

    @Test
    fun aKeyIsMadeOnceThenKept() {
        val dir = folder.newFolder()
        assertTrue(!AdbKey.exists(dir))

        val first = AdbKey.loadOrCreate(dir)
        val second = AdbKey.loadOrCreate(dir)

        assertTrue(AdbKey.exists(dir))
        assertEquals(first.adbPublicKey(), second.adbPublicKey())
        assertTrue(first.pkcs8.contentEquals(second.pkcs8))

        AdbKey.delete(dir)
        assertTrue(!AdbKey.exists(dir))
    }
}
