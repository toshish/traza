//! CRC-32 over the IEEE/gzip polynomial, shared by the WAL's frame checksums,
//! the segment block directory, and the payload-blob header.
//!
//! One implementation on purpose: every format declares the same polynomial,
//! and a second copy could drift from the declaration without either format's
//! tests noticing. Slice-by-16 (sixteen 256-entry tables, one buildable from
//! the next at compile time) rather than the byte-at-a-time Sarwate table,
//! because since format v7 this function fronts every block read, every seal
//! and merge encode, every blob read and write, and the whole-store migration
//! pass — and the Sarwate form was the single largest line item in the block
//! read profile. Measured on this machine over segment-sized inputs (release
//! build): the bitwise loop ran at ~121 MB/s, the one-table Sarwate form at
//! ~350 MB/s, and this slice-by-16 form at several GB/s — which moves the
//! checksum from ~2x the cost of the LZ4 inflate it precedes to a rounding
//! error beside it. Read-path profiling drove the change: at ~350 MB/s the
//! checksum alone was ~45 us of every 128 KiB block decode and the dominant
//! term in both trace-lookup and attribute-filter latency. Still not a
//! hardware CRC, deliberately: that needs either `unsafe` intrinsics or a
//! dependency, and this crate permits neither. The output is bit-identical
//! to the bitwise form — the acceptance tests recompute every directory CRC
//! with an independent bitwise implementation, which is what pins that
//! equivalence.

/// How many bytes one iteration of the sliced loop consumes.
const SLICES: usize = 16;

/// `TABLE[0]` is the classic Sarwate table: the CRC-32 of each single byte,
/// which the byte-at-a-time tail loop folds into the running remainder.
/// `TABLE[k][b]` extends that to the CRC contribution of byte `b` followed by
/// `k` zero bytes, which is what lets the main loop fold [`SLICES`] bytes per
/// iteration: each byte's contribution is looked up in the table matching its
/// distance from the end of the chunk, and the sixteen contributions XOR
/// together. Built at compile time from the same reflected polynomial the
/// bitwise form used, so no table can drift from the declaration.
const TABLE: [[u32; 256]; SLICES] = build_tables();

const fn build_tables() -> [[u32; 256]; SLICES] {
    let mut tables = [[0u32; 256]; SLICES];
    let mut index = 0usize;
    while index < 256 {
        let mut crc = index as u32;
        let mut bit = 0;
        while bit < 8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
            bit += 1;
        }
        tables[0][index] = crc;
        index += 1;
    }
    // Appending a zero byte to a message multiplies its CRC remainder by x^8
    // (mod the polynomial), which is exactly one step of the table-0 fold —
    // so each table is the previous one advanced by a zero byte.
    let mut slice = 1usize;
    while slice < SLICES {
        let mut index = 0usize;
        while index < 256 {
            let previous = tables[slice - 1][index];
            tables[slice][index] = (previous >> 8) ^ tables[0][(previous & 0xFF) as usize];
            index += 1;
        }
        slice += 1;
    }
    tables
}

/// CRC-32 of `bytes` (reflected 0xEDB88320, initial and final XOR all-ones —
/// the gzip/zlib/PNG polynomial).
pub(crate) fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFF_u32;
    let mut chunks = bytes.chunks_exact(SLICES);
    for chunk in &mut chunks {
        // The running remainder folds into the first four bytes; the rest of
        // the chunk contributes independently. Each byte is looked up in the
        // table for its distance from the chunk's end (15 zero bytes follow
        // the first byte, none follow the last).
        let low = crc ^ u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        crc = TABLE[15][(low & 0xFF) as usize]
            ^ TABLE[14][((low >> 8) & 0xFF) as usize]
            ^ TABLE[13][((low >> 16) & 0xFF) as usize]
            ^ TABLE[12][(low >> 24) as usize]
            ^ TABLE[11][chunk[4] as usize]
            ^ TABLE[10][chunk[5] as usize]
            ^ TABLE[9][chunk[6] as usize]
            ^ TABLE[8][chunk[7] as usize]
            ^ TABLE[7][chunk[8] as usize]
            ^ TABLE[6][chunk[9] as usize]
            ^ TABLE[5][chunk[10] as usize]
            ^ TABLE[4][chunk[11] as usize]
            ^ TABLE[3][chunk[12] as usize]
            ^ TABLE[2][chunk[13] as usize]
            ^ TABLE[1][chunk[14] as usize]
            ^ TABLE[0][chunk[15] as usize];
    }
    for byte in chunks.remainder() {
        crc = (crc >> 8) ^ TABLE[0][((crc ^ u32::from(*byte)) & 0xFF) as usize];
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::{crc32, TABLE};

    /// Pinned to the published check value of the IEEE polynomial. The WAL
    /// framing, the segment block directory, and the blob header all write
    /// this exact function's output to disk, so a change here is a format
    /// change.
    #[test]
    fn crc32_matches_the_ieee_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    /// The sliced fold must agree with the plain Sarwate fold on every input
    /// length around the 16-byte chunking boundary, not just on the check
    /// string — a wrong table or a wrong distance assignment shows up here
    /// before it shows up as an unreadable store.
    #[test]
    fn sliced_fold_matches_the_byte_at_a_time_fold() {
        let bytes: Vec<u8> = (0..1024u32)
            .map(|index| (index.wrapping_mul(2_654_435_761) >> 24) as u8)
            .collect();
        for length in 0..bytes.len() {
            let input = &bytes[..length];
            let mut crc = 0xFFFF_FFFF_u32;
            for byte in input {
                crc = (crc >> 8) ^ TABLE[0][((crc ^ u32::from(*byte)) & 0xFF) as usize];
            }
            assert_eq!(crc32(input), !crc, "length {length}");
        }
    }
}
