//! gzip request bodies: a small, dependency-free inflate (RFC 1951) inside
//! gzip framing (RFC 1952), with a hard cap on the decompressed size.
//!
//! OpenTelemetry exporters and collectors compress with `Content-Encoding:
//! gzip` as soon as anyone sets `compression: gzip`, and a server that cannot
//! read it forces every deployment to put a decompressing proxy in front. The
//! crate's dependency budget rules out pulling in a deflate implementation,
//! and inflate — unlike deflate — is small: three block types, two Huffman
//! constructions, one copy loop. This is that, written for clarity first.
//!
//! **Untrusted input, so every step is bounded.** The decompressed size is
//! capped by the caller (the server passes its request-body cap) and checked
//! before every byte is produced, so a decompression bomb costs at most the
//! cap in memory and the cap's worth of copying in time. All indexing is
//! checked; malformed input is an [`GzipError::Malformed`] naming the defect,
//! never a panic. Each member's CRC-32 and length trailer is verified, so a
//! decoding bug or a corrupted body cannot reach ingest as plausible-looking
//! bytes.
//!
//! **Speed is adequate, not tuned.** Huffman codes of up to nine bits resolve
//! through one table lookup; longer ones walk the canonical code a bit at a
//! time. No throughput is claimed for it.

use std::fmt;
use std::sync::OnceLock;

/// Why a gzip body was refused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GzipError {
    /// Not a valid gzip stream; the message names what is wrong with it.
    Malformed(String),
    /// The decompressed body would exceed this many bytes.
    TooLarge(usize),
}

impl fmt::Display for GzipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(reason) => write!(f, "invalid gzip body: {reason}"),
            Self::TooLarge(limit) => {
                write!(f, "gzip body decompresses to more than {limit} bytes")
            }
        }
    }
}

impl std::error::Error for GzipError {}

fn malformed(reason: &str) -> GzipError {
    GzipError::Malformed(reason.to_owned())
}

/// Decompresses a gzip body of one or more members, refusing it if the
/// output would exceed `limit` bytes.
pub fn gunzip(input: &[u8], limit: usize) -> Result<Vec<u8>, GzipError> {
    if input.is_empty() {
        return Err(malformed("the body is empty"));
    }
    let mut output = Vec::new();
    let mut rest = input;
    loop {
        let header = header_len(rest)?;
        let start = output.len();
        let consumed = inflate_into(&rest[header..], &mut output, start, limit)?;
        let trailer = rest
            .get(header + consumed..header + consumed + 8)
            .ok_or_else(|| malformed("truncated member trailer"))?;
        let crc = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
        let size = u32::from_le_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]);
        if crate::crc::crc32(&output[start..]) != crc {
            return Err(malformed("CRC-32 mismatch"));
        }
        // ISIZE is the length modulo 2^32, by definition.
        if (output.len() - start) as u32 != size {
            return Err(malformed("length trailer mismatch"));
        }
        rest = &rest[header + consumed + 8..];
        if rest.is_empty() {
            return Ok(output);
        }
        // Another member, or garbage. Concatenated members are valid gzip
        // (RFC 1952 §2.2) and some writers produce them; anything else after
        // a complete member is refused rather than silently dropped.
        if !rest.starts_with(&[0x1f, 0x8b]) {
            return Err(malformed("trailing bytes after the last member"));
        }
    }
}

/// The length of a member header, validated (RFC 1952 §2.3).
fn header_len(input: &[u8]) -> Result<usize, GzipError> {
    const FHCRC: u8 = 0x02;
    const FEXTRA: u8 = 0x04;
    const FNAME: u8 = 0x08;
    const FCOMMENT: u8 = 0x10;
    let fixed = input
        .get(..10)
        .ok_or_else(|| malformed("truncated member header"))?;
    if fixed[0] != 0x1f || fixed[1] != 0x8b {
        return Err(malformed("not gzip (bad magic bytes)"));
    }
    if fixed[2] != 8 {
        return Err(malformed(
            "unsupported compression method (only deflate is gzip)",
        ));
    }
    let flags = fixed[3];
    if flags & 0xe0 != 0 {
        return Err(malformed("reserved header flags are set"));
    }
    let mut at = 10;
    if flags & FEXTRA != 0 {
        let length = input
            .get(at..at + 2)
            .ok_or_else(|| malformed("truncated extra field"))?;
        at += 2 + usize::from(u16::from_le_bytes([length[0], length[1]]));
    }
    for flag in [FNAME, FCOMMENT] {
        if flags & flag != 0 {
            let terminator = input
                .get(at..)
                .and_then(|tail| tail.iter().position(|byte| *byte == 0))
                .ok_or_else(|| malformed("unterminated header string"))?;
            at += terminator + 1;
        }
    }
    if flags & FHCRC != 0 {
        let stored = input
            .get(at..at + 2)
            .ok_or_else(|| malformed("truncated header CRC"))?;
        let computed = crate::crc::crc32(&input[..at]) as u16;
        if computed != u16::from_le_bytes([stored[0], stored[1]]) {
            return Err(malformed("header CRC mismatch"));
        }
        at += 2;
    }
    if at > input.len() {
        return Err(malformed("truncated member header"));
    }
    Ok(at)
}

/// Inflates one raw deflate stream from `input`, appending to `output`, and
/// returns how many input bytes it occupied. `start` is where this stream's
/// output begins: back-references may not reach before it.
fn inflate_into(
    input: &[u8],
    output: &mut Vec<u8>,
    start: usize,
    limit: usize,
) -> Result<usize, GzipError> {
    let mut bits = Bits::new(input);
    loop {
        let last = bits.take(1)? == 1;
        match bits.take(2)? {
            0 => stored(&mut bits, output, limit)?,
            1 => {
                let (literals, distances) = fixed_codes();
                codes(&mut bits, output, start, limit, literals, distances)?;
            }
            2 => {
                let (literals, distances) = dynamic_codes(&mut bits)?;
                codes(&mut bits, output, start, limit, &literals, &distances)?;
            }
            _ => return Err(malformed("reserved deflate block type")),
        }
        if last {
            bits.align();
            return Ok(bits.byte_position());
        }
    }
}

/// An LSB-first bit reader over a byte slice (RFC 1951 §3.1.1).
struct Bits<'a> {
    input: &'a [u8],
    /// Next byte to load into `buffer`.
    position: usize,
    buffer: u64,
    /// Valid low bits in `buffer`; every bit above them is zero.
    count: u32,
}

impl<'a> Bits<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            position: 0,
            buffer: 0,
            count: 0,
        }
    }

    fn refill(&mut self) {
        while self.count <= 56 && self.position < self.input.len() {
            self.buffer |= u64::from(self.input[self.position]) << self.count;
            self.position += 1;
            self.count += 8;
        }
    }

    fn take(&mut self, n: u32) -> Result<u32, GzipError> {
        if n == 0 {
            return Ok(0);
        }
        if self.count < n {
            self.refill();
            if self.count < n {
                return Err(malformed("truncated deflate stream"));
            }
        }
        let value = (self.buffer & ((1_u64 << n) - 1)) as u32;
        self.buffer >>= n;
        self.count -= n;
        Ok(value)
    }

    /// Discards the bits left in the current byte.
    fn align(&mut self) {
        let partial = self.count % 8;
        self.buffer >>= partial;
        self.count -= partial;
    }

    /// Input bytes consumed so far, once aligned: what was loaded, minus the
    /// whole bytes still buffered.
    fn byte_position(&self) -> usize {
        self.position - (self.count / 8) as usize
    }

    /// Takes `length` whole bytes, starting at the next byte boundary.
    fn bytes(&mut self, length: usize) -> Result<&'a [u8], GzipError> {
        self.align();
        let from = self.byte_position();
        let bytes = self
            .input
            .get(from..from + length)
            .ok_or_else(|| malformed("truncated stored block"))?;
        self.position = from + length;
        self.buffer = 0;
        self.count = 0;
        Ok(bytes)
    }
}

/// Codes of up to this many bits decode with one table lookup.
const FAST_BITS: u32 = 9;
const MAX_BITS: usize = 15;

/// A canonical Huffman code (RFC 1951 §3.2.2).
struct Huffman {
    /// Codes of each length, 1 through 15.
    counts: [u16; MAX_BITS + 1],
    /// Symbols in canonical order: by code length, then by symbol.
    symbols: Vec<u16>,
    /// Indexed by the next [`FAST_BITS`] input bits (which arrive
    /// bit-reversed relative to the code): `length << 12 | symbol` for codes
    /// no longer than that, `0` where a longer code must be walked.
    fast: Vec<u16>,
    /// Code space left unassigned: zero for a complete code.
    unused: i32,
    /// Symbols with a nonzero length.
    used: usize,
}

impl Huffman {
    /// Builds the code for `lengths` (one per symbol, `0` = unused). An
    /// over-subscribed set is refused; an incomplete one is built, and the
    /// caller decides whether it is acceptable.
    fn new(lengths: &[u8]) -> Result<Self, GzipError> {
        let mut counts = [0_u16; MAX_BITS + 1];
        for &length in lengths {
            let length = usize::from(length);
            if length > MAX_BITS {
                return Err(malformed("Huffman code length over 15"));
            }
            counts[length] += 1;
        }
        let used = lengths.len() - usize::from(counts[0]);
        counts[0] = 0;
        let mut unused: i32 = 1;
        for &count in &counts[1..] {
            unused = (unused << 1) - i32::from(count);
            if unused < 0 {
                return Err(malformed("over-subscribed Huffman code"));
            }
        }

        let mut offsets = [0_u16; MAX_BITS + 2];
        for length in 1..=MAX_BITS {
            offsets[length + 1] = offsets[length] + counts[length];
        }
        let mut symbols = vec![0_u16; used];
        // The first canonical code of each length (RFC 1951 §3.2.2, step 2).
        let mut next = [0_u32; MAX_BITS + 1];
        let mut code = 0_u32;
        for length in 1..=MAX_BITS {
            code = (code + u32::from(counts[length - 1])) << 1;
            next[length] = code;
        }
        let mut fast = vec![0_u16; 1 << FAST_BITS];
        for (symbol, &length) in lengths.iter().enumerate() {
            let length = usize::from(length);
            if length == 0 {
                continue;
            }
            symbols[usize::from(offsets[length])] = symbol as u16;
            offsets[length] += 1;
            let code = next[length];
            next[length] += 1;
            if length as u32 <= FAST_BITS {
                let reversed = reverse(code, length as u32) as usize;
                let entry = (length as u16) << 12 | symbol as u16;
                let mut fill = 0;
                while fill < 1 << (FAST_BITS - length as u32) {
                    fast[reversed | fill << length] = entry;
                    fill += 1;
                }
            }
        }
        Ok(Self {
            counts,
            symbols,
            fast,
            unused,
            used,
        })
    }

    /// RFC 1951 permits an incomplete literal/length or distance code only
    /// when it has a single code (of length one) — or none at all, for a
    /// distance code in a block of literals. zlib holds the same line.
    fn acceptable_incomplete(&self) -> bool {
        self.unused == 0 || self.used == usize::from(self.counts[1])
    }

    fn decode(&self, bits: &mut Bits<'_>) -> Result<u16, GzipError> {
        bits.refill();
        let entry = self.fast[(bits.buffer & ((1 << FAST_BITS) - 1)) as usize];
        if entry != 0 {
            let length = u32::from(entry >> 12);
            if length <= bits.count {
                bits.buffer >>= length;
                bits.count -= length;
                return Ok(entry & 0x0fff);
            }
        }
        // The canonical walk: extend the code a bit at a time until it falls
        // inside the range of codes of its length.
        let mut code: i32 = 0;
        let mut first: i32 = 0;
        let mut index: i32 = 0;
        for length in 1..=MAX_BITS {
            code |= bits.take(1)? as i32;
            let count = i32::from(self.counts[length]);
            if code - first < count {
                return self
                    .symbols
                    .get((index + code - first) as usize)
                    .copied()
                    .ok_or_else(|| malformed("invalid Huffman code"));
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(malformed("invalid Huffman code"))
    }
}

/// The low `length` bits of `code`, in reverse order.
fn reverse(code: u32, length: u32) -> u32 {
    code.reverse_bits() >> (32 - length)
}

/// A stored block (RFC 1951 §3.2.4).
fn stored(bits: &mut Bits<'_>, output: &mut Vec<u8>, limit: usize) -> Result<(), GzipError> {
    bits.align();
    let length = bits.take(16)?;
    let complement = bits.take(16)?;
    if length != !complement & 0xffff {
        return Err(malformed("stored block length check failed"));
    }
    let length = length as usize;
    if output.len() + length > limit {
        return Err(GzipError::TooLarge(limit));
    }
    output.extend_from_slice(bits.bytes(length)?);
    Ok(())
}

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DISTANCE_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DISTANCE_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// The body of a compressed block: literals and back-references until the
/// end-of-block code (RFC 1951 §3.2.5).
fn codes(
    bits: &mut Bits<'_>,
    output: &mut Vec<u8>,
    start: usize,
    limit: usize,
    literals: &Huffman,
    distances: &Huffman,
) -> Result<(), GzipError> {
    loop {
        let symbol = literals.decode(bits)?;
        match symbol {
            0..=255 => {
                if output.len() >= limit {
                    return Err(GzipError::TooLarge(limit));
                }
                output.push(symbol as u8);
            }
            256 => return Ok(()),
            _ => {
                let index = usize::from(symbol - 257);
                let (Some(&base), Some(&extra)) = (LENGTH_BASE.get(index), LENGTH_EXTRA.get(index))
                else {
                    return Err(malformed("invalid length symbol"));
                };
                let length = usize::from(base) + bits.take(u32::from(extra))? as usize;
                let index = usize::from(distances.decode(bits)?);
                let (Some(&base), Some(&extra)) =
                    (DISTANCE_BASE.get(index), DISTANCE_EXTRA.get(index))
                else {
                    return Err(malformed("invalid distance symbol"));
                };
                let distance = usize::from(base) + bits.take(u32::from(extra))? as usize;
                if distance > output.len() - start {
                    return Err(malformed("back-reference before the start of the stream"));
                }
                if output.len() + length > limit {
                    return Err(GzipError::TooLarge(limit));
                }
                let from = output.len() - distance;
                if distance >= length {
                    output.extend_from_within(from..from + length);
                } else {
                    // Overlapping: the copy reads bytes it has just written,
                    // which is how a run is encoded. Byte by byte, on purpose.
                    for offset in 0..length {
                        let byte = output[from + offset];
                        output.push(byte);
                    }
                }
            }
        }
    }
}

/// The fixed codes of block type 1 (RFC 1951 §3.2.6), built once.
fn fixed_codes() -> (&'static Huffman, &'static Huffman) {
    static FIXED: OnceLock<(Huffman, Huffman)> = OnceLock::new();
    let (literals, distances) = FIXED.get_or_init(|| {
        let mut lengths = [0_u8; 288];
        for (symbol, length) in lengths.iter_mut().enumerate() {
            *length = match symbol {
                0..=143 => 8,
                144..=255 => 9,
                256..=279 => 7,
                _ => 8,
            };
        }
        (
            Huffman::new(&lengths).expect("the fixed literal code is valid"),
            Huffman::new(&[5; 30]).expect("the fixed distance code is valid"),
        )
    });
    (literals, distances)
}

/// Reads a dynamic block's code definitions (RFC 1951 §3.2.7).
fn dynamic_codes(bits: &mut Bits<'_>) -> Result<(Huffman, Huffman), GzipError> {
    const ORDER: [usize; 19] = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    let literal_count = bits.take(5)? as usize + 257;
    let distance_count = bits.take(5)? as usize + 1;
    let length_count = bits.take(4)? as usize + 4;
    if literal_count > 286 || distance_count > 30 {
        return Err(malformed("too many length or distance codes"));
    }
    let mut length_lengths = [0_u8; 19];
    for &symbol in &ORDER[..length_count] {
        length_lengths[symbol] = bits.take(3)? as u8;
    }
    let length_code = Huffman::new(&length_lengths)?;
    if length_code.unused != 0 {
        return Err(malformed("incomplete code-length code"));
    }

    let total = literal_count + distance_count;
    let mut lengths = vec![0_u8; total];
    let mut at = 0;
    while at < total {
        let (value, repeat) = match length_code.decode(bits)? {
            symbol @ 0..=15 => (symbol as u8, 1),
            16 => {
                let previous = *lengths
                    .get(at.wrapping_sub(1))
                    .ok_or_else(|| malformed("repeat with no previous length"))?;
                (previous, 3 + bits.take(2)? as usize)
            }
            17 => (0, 3 + bits.take(3)? as usize),
            18 => (0, 11 + bits.take(7)? as usize),
            _ => return Err(malformed("invalid code-length symbol")),
        };
        if at + repeat > total {
            return Err(malformed("code lengths overrun their count"));
        }
        lengths[at..at + repeat].fill(value);
        at += repeat;
    }
    if lengths[256] == 0 {
        return Err(malformed("no end-of-block code"));
    }
    let literals = Huffman::new(&lengths[..literal_count])?;
    let distances = Huffman::new(&lengths[literal_count..])?;
    if !literals.acceptable_incomplete() || !distances.acceptable_incomplete() {
        return Err(malformed("incomplete literal/length or distance code"));
    }
    Ok((literals, distances))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text the vectors below compress, rebuilt the way the generator
    /// (Python's zlib) was fed it.
    fn sample() -> Vec<u8> {
        (0..60)
            .map(|i: u64| {
                format!(
                    "line {i}: the quick brown fox jumps over the lazy dog {}\n",
                    i * i % 97
                )
            })
            .collect::<String>()
            .into_bytes()
    }

    fn unhex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&text[at..at + 2], 16).expect("hex"))
            .collect()
    }

    /// `gzip.compress(sample, compresslevel=9, mtime=0)`: dynamic Huffman.
    const DYNAMIC_GZIP: &str = "1f8b08000000000002139dd6595203310c45d17f56e125c4833cb01b860602210d8130ad9e827e5ac0f577ea9565eb489dc3feb884dd65787f58c2eb797ff314ae4febe731dcad5fe1f1fcfcf216d68fe5f4fff3e1eae73bdcaef7617771f84b45968a5b2ab154d95299a5c6962ab0c2bac58cc5926db1ca6259a735f820ba5b67b1aa871c2cd6d5b5088964c5a091a42a2355d294834e5a520e4af1189492fd352195eae7412b433223c492bd7d504bd36648504bd4fd12e462aa33412e43a3972097229e097219befba097e2f7835e86d709bd98f70f7ae98a412e553c33e49234ed1972e9aa33432ee61f21c8258975865cba567c865caa9865fa25f23e402ed1cfa35c347e997a51ae402f457d2fd04b56ff0af412fd3ce8c5af47b78bb644815cbada5e2097e6ffae2097e6e7412ecdef37e672b69babd3e2dcbb589aeb83e5b9be5b99626636a7daeadc14599b9b5aeb735bc206de4abf4070db2c0d0d0000";

    /// Raw deflate of the sample under zlib's `Z_FIXED` strategy: fixed
    /// Huffman blocks only.
    const FIXED_DEFLATE: &str = "cbc9cc4b5530b05228c94855282ccd4cce56482aca2fcf5348cbaf50c82acd2d2856c82f4b2d024be72456552aa4e4a72b1870e580741992a6cb10a2cb88345d26105dc6a4e9b284e83221d18566106da6a4693332856833234d9b31d43673120304ea370bd2b4994103d292346d16d058332431891843b59198468ca0ae342435959843f591984ecc8da0fa484c29306d24a614635868929854cc60f69198562ca129d390c4c4620c8b3e12538b39b464302231b51842fd674462723185bad388c4e46209cd7a462426171368f2342231b958c2ca3e12d38b09cc7f24a6174b983b494c2fa6b0f82331bd5840b591985ccca0c9d398c4e46204cdedc62426170ba83b8d494c2ea6b04a88c4e462044dd6c62426170b68116f4c62723183263363526b22583c90985c0c61f6919a5ca0d9cf98d4f402d56742627a3181c6bb0989e9c5181a7f2624a61743987d24a61798f7482d5da0a5840989c9c5021aed262426177358eb8ac4e4620eb38fc4e4620ef39f2579fa4c0dc873a7a92179e1626a445e3c981a9317efa6266425335353f252b5a91979b9c8d49cbc5c6b6a415e29616a4972a90400";

    /// Wraps a raw deflate stream as one gzip member, with optional header
    /// fields.
    fn member(deflate: &[u8], plain: &[u8], flags: u8) -> Vec<u8> {
        let mut out = vec![0x1f, 0x8b, 8, flags, 0, 0, 0, 0, 0, 255];
        if flags & 0x04 != 0 {
            out.extend_from_slice(&3_u16.to_le_bytes());
            out.extend_from_slice(b"xyz");
        }
        if flags & 0x08 != 0 {
            out.extend_from_slice(b"spans.json\0");
        }
        if flags & 0x10 != 0 {
            out.extend_from_slice(b"a comment\0");
        }
        if flags & 0x02 != 0 {
            let crc = crate::crc::crc32(&out) as u16;
            out.extend_from_slice(&crc.to_le_bytes());
        }
        out.extend_from_slice(deflate);
        out.extend_from_slice(&crate::crc::crc32(plain).to_le_bytes());
        out.extend_from_slice(&(plain.len() as u32).to_le_bytes());
        out
    }

    /// `plain` as stored blocks of at most `chunk` bytes.
    fn stored_deflate(plain: &[u8], chunk: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut pieces: Vec<&[u8]> = plain.chunks(chunk.max(1)).collect();
        if pieces.is_empty() {
            pieces.push(&[]);
        }
        for (index, piece) in pieces.iter().enumerate() {
            out.push(u8::from(index + 1 == pieces.len()));
            out.extend_from_slice(&(piece.len() as u16).to_le_bytes());
            out.extend_from_slice(&(!(piece.len() as u16)).to_le_bytes());
            out.extend_from_slice(piece);
        }
        out
    }

    #[test]
    fn dynamic_fixed_and_stored_blocks_all_decode() {
        let plain = sample();
        assert_eq!(gunzip(&unhex(DYNAMIC_GZIP), usize::MAX), Ok(plain.clone()));
        let fixed = member(&unhex(FIXED_DEFLATE), &plain, 0);
        assert_eq!(gunzip(&fixed, usize::MAX), Ok(plain.clone()));
        let stored = member(&stored_deflate(&plain, 1_000), &plain, 0);
        assert_eq!(gunzip(&stored, usize::MAX), Ok(plain.clone()));
        // Empty content is a valid member too.
        let empty = member(&stored_deflate(b"", 1), b"", 0);
        assert_eq!(gunzip(&empty, 16), Ok(Vec::new()));
    }

    #[test]
    fn optional_header_fields_and_concatenated_members_are_understood() {
        let plain = sample();
        let all_fields = member(&unhex(FIXED_DEFLATE), &plain, 0x02 | 0x04 | 0x08 | 0x10);
        assert_eq!(gunzip(&all_fields, usize::MAX), Ok(plain.clone()));
        let mut two = unhex(DYNAMIC_GZIP);
        two.extend_from_slice(&member(&stored_deflate(b"tail", 2), b"tail", 0x08));
        let mut expected = plain;
        expected.extend_from_slice(b"tail");
        assert_eq!(gunzip(&two, usize::MAX), Ok(expected));
    }

    #[test]
    fn the_cap_refuses_before_producing_past_it() {
        let plain = sample();
        let limit = plain.len() - 1;
        for body in [
            unhex(DYNAMIC_GZIP),
            member(&unhex(FIXED_DEFLATE), &plain, 0),
            member(&stored_deflate(&plain, 500), &plain, 0),
        ] {
            assert_eq!(gunzip(&body, limit), Err(GzipError::TooLarge(limit)));
            assert_eq!(
                gunzip(&body, plain.len()),
                Ok(plain.clone()),
                "exactly the cap fits"
            );
        }
    }

    #[test]
    fn damage_is_refused_by_name_and_never_panics() {
        let good = unhex(DYNAMIC_GZIP);
        let refusal = |body: &[u8]| match gunzip(body, 1 << 20) {
            Err(GzipError::Malformed(reason)) => reason,
            other => panic!("expected a refusal, got {other:?}"),
        };
        assert!(refusal(b"").contains("empty"));
        assert!(refusal(b"{\"spans\":[]}").contains("magic"));
        let mut method = good.clone();
        method[2] = 7;
        assert!(refusal(&method).contains("compression method"));
        let mut flags = good.clone();
        flags[3] = 0x20;
        assert!(refusal(&flags).contains("reserved"));
        let mut crc = good.clone();
        let at = crc.len() - 8;
        crc[at] ^= 1;
        assert!(refusal(&crc).contains("CRC"));
        let mut size = good.clone();
        let at = size.len() - 1;
        size[at] ^= 1;
        assert!(refusal(&size).contains("length trailer"));
        let mut trailing = good.clone();
        trailing.extend_from_slice(b"junk");
        assert!(refusal(&trailing).contains("trailing"));
        assert!(refusal(&[0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255, 0x07]).contains("reserved deflate"));
        let mut header_crc = member(b"\x03\x00", b"", 0x02);
        header_crc[10] ^= 0xff;
        assert!(refusal(&header_crc).contains("header CRC"));
        let mut bad_stored = member(&stored_deflate(b"abc", 3), b"abc", 0);
        bad_stored[13] ^= 0xff; // the NLEN complement
        assert!(refusal(&bad_stored).contains("stored block"));

        // Every truncation and every single-byte corruption of a valid body
        // is an error or (when the damage is in the ignorable header bytes)
        // the right answer — never a panic, never different content. The
        // small cap keeps a corruption that decodes to garbage cheap.
        let plain = sample();
        for cut in 0..good.len() {
            assert!(gunzip(&good[..cut], 1 << 16).is_err(), "truncated at {cut}");
        }
        for at in 0..good.len() {
            for flip in [0x01_u8, 0x80, 0xff] {
                let mut damaged = good.clone();
                damaged[at] ^= flip;
                if let Ok(decoded) = gunzip(&damaged, 1 << 16) {
                    assert_eq!(
                        decoded, plain,
                        "byte {at} ^ {flip:#x} decoded to other content"
                    );
                }
            }
        }
    }

    #[test]
    fn a_back_reference_before_the_stream_is_refused() {
        // Fixed block: a length-3 match at distance 1 with nothing written
        // yet. Literal/length symbol 257 is the 7-bit code 0000001; distance
        // symbol 0 is the 5-bit code 00000; then end-of-block (0000000).
        let mut writer = BitWriter::default();
        writer.put(1, 1); // final block
        writer.put(1, 2); // fixed Huffman
        writer.put_code(0b000_0001, 7);
        writer.put_code(0, 5);
        writer.put_code(0, 7);
        let body = member(&writer.finish(), b"", 0);
        assert!(matches!(
            gunzip(&body, 1 << 20),
            Err(GzipError::Malformed(reason)) if reason.contains("back-reference")
        ));
    }

    /// Codes of one through fifteen bits — the short ones through the lookup
    /// table, the long ones through the canonical walk — each decode to
    /// their own symbol. The canonical codes are assigned here straight from
    /// RFC 1951 §3.2.2, independently of `Huffman::new`.
    #[test]
    fn every_code_length_decodes_through_both_paths() {
        let mut lengths: Vec<u8> = (1..=15).collect();
        lengths.push(15); // completes the code: 2^-1 + … + 2^-15 + 2^-15 = 1
        let code = Huffman::new(&lengths).expect("builds");
        assert_eq!(code.unused, 0);
        let mut bl_count = [0_u32; 16];
        for &length in &lengths {
            bl_count[usize::from(length)] += 1;
        }
        let mut next = [0_u32; 16];
        let mut running = 0;
        for bits in 1..16 {
            running = (running + bl_count[bits - 1]) << 1;
            next[bits] = running;
        }
        let mut writer = BitWriter::default();
        for &length in &lengths {
            let length = usize::from(length);
            writer.put_code(next[length], length as u32);
            next[length] += 1;
        }
        let stream = writer.finish();
        let mut bits = Bits::new(&stream);
        for symbol in 0..lengths.len() {
            assert_eq!(code.decode(&mut bits), Ok(symbol as u16), "symbol {symbol}");
        }
    }

    #[test]
    fn huffman_construction_refuses_over_subscription_and_honours_incompleteness() {
        assert!(
            Huffman::new(&[1, 1, 1]).is_err(),
            "three codes of length one"
        );
        let single = Huffman::new(&[0, 1]).expect("builds");
        assert!(
            single.acceptable_incomplete(),
            "a lone length-one code is allowed"
        );
        let empty = Huffman::new(&[0, 0, 0]).expect("builds");
        assert!(
            empty.acceptable_incomplete(),
            "no distance codes at all is allowed"
        );
        let gappy = Huffman::new(&[2, 2, 2]).expect("builds");
        assert!(
            !gappy.acceptable_incomplete(),
            "a quarter of the code space unused"
        );
        assert!(Huffman::new(&[16]).is_err());
    }

    /// Enough of a deflate writer to build adversarial streams.
    #[derive(Default)]
    struct BitWriter {
        bytes: Vec<u8>,
        buffer: u64,
        count: u32,
    }

    impl BitWriter {
        /// `value`'s low `n` bits, LSB first (header fields, extra bits).
        fn put(&mut self, value: u32, n: u32) {
            self.buffer |= u64::from(value) << self.count;
            self.count += n;
            while self.count >= 8 {
                self.bytes.push(self.buffer as u8);
                self.buffer >>= 8;
                self.count -= 8;
            }
        }

        /// A Huffman code, MSB first.
        fn put_code(&mut self, code: u32, n: u32) {
            self.put(reverse(code, n), n);
        }

        fn finish(mut self) -> Vec<u8> {
            if self.count > 0 {
                self.bytes.push(self.buffer as u8);
            }
            self.bytes
        }
    }

    #[test]
    fn a_run_of_overlapping_copies_decodes_and_hits_the_cap_exactly() {
        // One literal 'z', then 10 matches of length 258 at distance 1: a
        // run of 2,581 bytes from a stream of a few dozen.
        let mut writer = BitWriter::default();
        writer.put(1, 1);
        writer.put(1, 2);
        writer.put_code(0x30 + u32::from(b'z'), 8); // literals 0-143: 00110000 + value
        for _ in 0..10 {
            writer.put_code(0b1100_0101, 8); // symbol 285 (length 258): 11000000 + 5
            writer.put_code(0, 5); // distance 1
        }
        writer.put_code(0, 7); // end of block
        let plain = vec![b'z'; 1 + 10 * 258];
        let body = member(&writer.finish(), &plain, 0);
        assert_eq!(gunzip(&body, plain.len()), Ok(plain.clone()));
        assert_eq!(
            gunzip(&body, plain.len() - 1),
            Err(GzipError::TooLarge(plain.len() - 1))
        );
    }
}
