//! A deterministic 128-bit hash for persisted index keys.
//!
//! Traza indexes attribute values by hash rather than by text, so a segment's
//! resident index does not grow with the size of an LLM prompt. That makes the
//! hash part of the **on-disk format**: a segment written by one build is
//! probed by another, so the same input must produce the same 128 bits
//! forever.
//!
//! That requirement rules out the obvious choice. `std`'s [`DefaultHasher`]
//! documents that its algorithm "may change between Rust releases", and
//! `RandomState` — what `HashMap` uses by default — is seeded per process, so
//! two runs disagree on the first byte. Either one would produce an index that
//! silently stops matching its own postings after a toolchain upgrade or a
//! restart. The construction therefore has to be written down here, where it
//! can be pinned and tested, and it must stay dependency-free like the rest of
//! the crate.
//!
//! # The construction
//!
//! [`hash128`] is the SipHash round function (Aumasson–Bernstein) in a
//! 1-round-per-block, 3-round-finalization arrangement, run twice over the
//! same message with two different key pairs and concatenated into 128 bits.
//! The two-pass form is deliberate: it is obviously correct by construction
//! from a 64-bit primitive, where the single-pass 128-bit SipHash variant
//! depends on finalization constants that this crate cannot verify against
//! the reference implementation without taking a dependency.
//!
//! This is **not** a claim of byte compatibility with any published SipHash
//! test vector, and nothing outside this crate should treat it as SipHash. It
//! is Traza's hash, specified by this file. What is claimed, and tested, is
//! what a persisted index actually needs: the same input gives the same
//! output on every platform and every build, and distinct inputs scatter.
//!
//! # This is not a cryptographic commitment
//!
//! A 128-bit digest is small enough to attack offline, and the keys below are
//! public constants. An attacker who chooses attribute values can therefore
//! manufacture colliding keys. That is *not* a correctness problem for Traza,
//! because every index probe is verified against the record it points at (see
//! `segment::Segment::query_attribute`): a collision costs one wasted record
//! decode and can never produce a wrong answer. Do not reuse this hash
//! anywhere the digest itself is trusted.
//!
//! [`DefaultHasher`]: std::collections::hash_map::DefaultHasher

/// A 128-bit digest used as a persisted index key.
///
/// Stored and compared as raw bytes so the on-disk encoding is exactly the
/// in-memory one and no endianness question arises at the format boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Hash128(pub [u8; 16]);

impl Hash128 {
    /// The digest as its 16 on-disk bytes.
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// Rebuilds a digest from its 16 on-disk bytes.
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }
}

impl std::fmt::Display for Hash128 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// Keys for the two passes. These are format constants: changing one changes
/// the meaning of every persisted index, so they may only move behind a
/// segment format version bump. The values are the fractional digits of pi,
/// phi, e, and sqrt(2) in hex — "nothing up my sleeve" numbers, chosen only
/// to make it evident they were not selected to produce any particular
/// collision.
const KEY_LOW: (u64, u64) = (0x243f_6a88_85a3_08d3, 0x9e37_79b9_7f4a_7c15);
const KEY_HIGH: (u64, u64) = (0xb7e1_5162_8aed_2a6b, 0x6a09_e667_f3bc_c908);

/// The 128-bit digest of `bytes`.
///
/// Stable across processes, platforms, and builds — see the module docs for
/// why that is a format requirement rather than a nicety.
pub fn hash128(bytes: &[u8]) -> Hash128 {
    let low = siphash(bytes, KEY_LOW.0, KEY_LOW.1);
    let high = siphash(bytes, KEY_HIGH.0, KEY_HIGH.1);
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&low.to_le_bytes());
    out[8..].copy_from_slice(&high.to_le_bytes());
    Hash128(out)
}

/// The 128-bit digest of an attribute value, bound to its key.
///
/// The key is mixed in rather than stored beside the hash so that the same
/// text under two different attribute keys lands on two different digests.
/// Without that, `gen_ai.prompt` and `gen_ai.completion` holding identical
/// text would share a posting list and each would return the other's spans
/// as candidates — correct after verification, but needlessly so.
pub fn hash_attribute(key: &str, value: &str) -> Hash128 {
    // Length-prefix the key so ("ab", "c") and ("a", "bc") cannot collide by
    // concatenation.
    let mut buffer = Vec::with_capacity(4 + key.len() + value.len());
    buffer.extend_from_slice(&(key.len() as u32).to_le_bytes());
    buffer.extend_from_slice(key.as_bytes());
    buffer.extend_from_slice(value.as_bytes());
    hash128(&buffer)
}

/// One SipHash round, operating in place on the four state words.
#[inline(always)]
fn round(v: &mut [u64; 4]) {
    v[0] = v[0].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(13);
    v[1] ^= v[0];
    v[0] = v[0].rotate_left(32);
    v[2] = v[2].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(16);
    v[3] ^= v[2];
    v[0] = v[0].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(21);
    v[3] ^= v[0];
    v[2] = v[2].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(17);
    v[1] ^= v[2];
    v[2] = v[2].rotate_left(32);
}

/// SipHash-1-3 over `bytes` under the key `(k0, k1)`.
///
/// One compression round per 8-byte block and three finalization rounds — the
/// same cost profile `std` chose for its own hasher, which is the right trade
/// for a value that is verified downstream rather than trusted.
fn siphash(bytes: &[u8], k0: u64, k1: u64) -> u64 {
    let mut v: [u64; 4] = [
        k0 ^ 0x736f_6d65_7073_6575,
        k1 ^ 0x646f_7261_6e64_6f6d,
        k0 ^ 0x6c79_6765_6e65_7261,
        k1 ^ 0x7465_6462_7974_6573,
    ];

    let mut chunks = bytes.chunks_exact(8);
    for chunk in &mut chunks {
        let word = u64::from_le_bytes(chunk.try_into().expect("chunks_exact(8) yields 8 bytes"));
        v[3] ^= word;
        round(&mut v);
        v[0] ^= word;
    }

    // Final partial block: the remaining bytes little-endian, with the total
    // message length in the top byte so that inputs differing only by
    // trailing zero bytes still differ.
    let remainder = chunks.remainder();
    let mut tail = (bytes.len() as u64 & 0xff) << 56;
    for (index, byte) in remainder.iter().enumerate() {
        tail |= u64::from(*byte) << (8 * index);
    }
    v[3] ^= tail;
    round(&mut v);
    v[0] ^= tail;

    v[2] ^= 0xff;
    round(&mut v);
    round(&mut v);
    round(&mut v);
    v[0] ^ v[1] ^ v[2] ^ v[3]
}

#[cfg(test)]
mod tests {
    use super::{hash128, hash_attribute, Hash128};
    use std::collections::HashSet;

    /// The property the on-disk format actually depends on. These literals
    /// were produced by this implementation and are pinned here on purpose:
    /// if a refactor changes the digest of a fixed input, every persisted
    /// attribute index in existence has silently stopped matching its own
    /// postings, and this test is the only thing that says so.
    #[test]
    fn digests_are_pinned_to_the_format() {
        assert_eq!(
            hash128(b"").to_string(),
            "6c09e895418edaaafb787eeb2020ab05",
            "the empty input's digest is part of the segment format"
        );
        assert_eq!(
            hash128(b"gpt-4o").to_string(),
            "8b34ddd2aa7a13ef929d9c5f1742137d"
        );
        assert_eq!(
            hash_attribute("model", "gpt-4o").to_string(),
            "68b62b3192d4e725581218c068242650"
        );
    }

    #[test]
    fn the_same_input_always_gives_the_same_digest() {
        let text = "the quick brown fox jumps over the lazy dog".repeat(37);
        let first = hash128(text.as_bytes());
        for _ in 0..64 {
            assert_eq!(hash128(text.as_bytes()), first);
        }
    }

    #[test]
    fn a_one_bit_change_moves_about_half_the_output_bits() {
        // Avalanche. A hash that failed this would cluster similar prompts
        // into the same buckets and turn the index into a linked list.
        let base = b"gen_ai.prompt: summarize the following support ticket";
        let reference = hash128(base);
        let mut total_flipped = 0usize;
        let mut samples = 0usize;
        for index in 0..base.len() {
            for bit in 0..8 {
                let mut mutated = base.to_vec();
                mutated[index] ^= 1 << bit;
                let flipped: u32 = hash128(&mutated)
                    .0
                    .iter()
                    .zip(reference.0.iter())
                    .map(|(a, b)| (a ^ b).count_ones())
                    .sum();
                total_flipped += flipped as usize;
                samples += 1;
            }
        }
        let mean = total_flipped as f64 / samples as f64;
        assert!(
            (56.0..72.0).contains(&mean),
            "mean flipped bits {mean} is far from the ideal 64 of 128"
        );
    }

    #[test]
    fn distinct_values_do_not_collide_at_realistic_cardinality() {
        // One segment's worth of all-distinct LLM-shaped values.
        let mut seen: HashSet<Hash128> = HashSet::new();
        for index in 0..200_000u32 {
            let value = format!(
                "conversation turn {index}: {}",
                "x".repeat(index as usize % 97)
            );
            assert!(
                seen.insert(hash_attribute("gen_ai.prompt", &value)),
                "collision at {index}"
            );
        }
    }

    #[test]
    fn the_attribute_key_is_bound_into_the_digest() {
        // Identical text under two keys must not share a posting list.
        assert_ne!(
            hash_attribute("gen_ai.prompt", "hello"),
            hash_attribute("gen_ai.completion", "hello")
        );
        // And the key/value boundary must be unambiguous.
        assert_ne!(hash_attribute("ab", "c"), hash_attribute("a", "bc"));
    }

    #[test]
    fn length_is_mixed_in_so_trailing_zeros_matter() {
        assert_ne!(hash128(b"abc"), hash128(b"abc\0"));
        assert_ne!(hash128(b""), hash128(b"\0"));
    }
}
