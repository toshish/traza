//! Zstandard decompression (RFC 8878), dependency-free, with a hard cap on
//! the decompressed size.
//!
//! The Codex CLI compresses a session's rollout to `rollout-….jsonl.zst` once
//! it is no longer live, so on a real machine most of an agent's history is
//! zstd frames. The crate's dependency budget rules out a zstd library, and —
//! as with [`crate::gzip`]'s inflate — the decoder is the small half of the
//! codec: frames of raw, RLE and compressed blocks; Huffman-coded literals;
//! and three FSE-coded sequence streams executed against the output. This is
//! that, written for clarity first.
//!
//! **Untrusted input, so every step is bounded.** The output is capped by the
//! caller and checked before bytes are produced; every slice is checked, so
//! malformed input is a [`ZstdError::Malformed`] naming the defect, never a
//! panic. When a frame carries a content checksum (XXH64, also implemented
//! here) it is verified, so a decoding bug or a damaged file cannot reach the
//! importer as plausible-looking lines. Dictionaries are not supported (a
//! frame naming one is [`ZstdError::Unsupported`]); Codex does not use them.
//!
//! **Speed is adequate, not tuned.** Bit reads are word loads; no throughput
//! is claimed.

use std::fmt;

/// Why a zstd stream was refused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ZstdError {
    /// Not a valid zstd stream; the message names what is wrong with it.
    Malformed(String),
    /// Valid, but uses a feature this decoder does not implement.
    Unsupported(String),
    /// The decompressed content would exceed this many bytes.
    TooLarge(usize),
}

impl fmt::Display for ZstdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(reason) => write!(f, "invalid zstd stream: {reason}"),
            Self::Unsupported(reason) => write!(f, "unsupported zstd stream: {reason}"),
            Self::TooLarge(limit) => {
                write!(f, "zstd stream decompresses to more than {limit} bytes")
            }
        }
    }
}

impl std::error::Error for ZstdError {}

type Result<T> = std::result::Result<T, ZstdError>;

fn malformed<T>(reason: &str) -> Result<T> {
    Err(ZstdError::Malformed(reason.to_owned()))
}

const FRAME_MAGIC: u32 = 0xFD2F_B528;
const SKIPPABLE_MASK: u32 = 0xFFFF_FFF0;
const SKIPPABLE_MAGIC: u32 = 0x184D_2A50;
const MAX_BLOCK: usize = 128 * 1024;

/// Whether `input` begins with a zstd frame (or a skippable frame).
pub fn is_zstd(input: &[u8]) -> bool {
    input.len() >= 4 && {
        let magic = u32::from_le_bytes([input[0], input[1], input[2], input[3]]);
        magic == FRAME_MAGIC || magic & SKIPPABLE_MASK == SKIPPABLE_MAGIC
    }
}

/// Decompresses every frame of `input`, refusing to produce more than
/// `limit` bytes.
pub fn decompress(input: &[u8], limit: usize) -> Result<Vec<u8>> {
    if input.is_empty() {
        return malformed("the stream is empty");
    }
    let mut out = Vec::new();
    let mut rest = input;
    while !rest.is_empty() {
        let magic = match rest.get(..4) {
            Some(bytes) => u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            None => return malformed("trailing bytes after the last frame"),
        };
        if magic & SKIPPABLE_MASK == SKIPPABLE_MAGIC {
            let size = rest
                .get(4..8)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
                .ok_or_else(|| ZstdError::Malformed("truncated skippable frame".into()))?;
            rest = rest
                .get(8 + size..)
                .ok_or_else(|| ZstdError::Malformed("truncated skippable frame".into()))?;
            continue;
        }
        if magic != FRAME_MAGIC {
            return malformed("bad frame magic");
        }
        let used = decode_frame(&rest[4..], &mut out, limit)?;
        rest = &rest[4 + used..];
    }
    Ok(out)
}

/// The first `want` bytes (or more — whole blocks) of what `input`
/// decompresses to, as far as `input` holds complete blocks of the first
/// frame: for reading a file's first line without decompressing all of it.
/// Best effort — damage or truncation ends the prefix early, never panics.
pub fn decompress_prefix(input: &[u8], want: usize) -> Vec<u8> {
    let mut out = Vec::new();
    if input.len() >= 4
        && u32::from_le_bytes([input[0], input[1], input[2], input[3]]) == FRAME_MAGIC
    {
        let _ = decode_frame_until(
            &input[4..],
            &mut out,
            want.saturating_add(MAX_BLOCK),
            Some(want),
        );
    }
    out
}

/// Decodes one frame (after its magic) into `out`; returns bytes consumed.
fn decode_frame(input: &[u8], out: &mut Vec<u8>, limit: usize) -> Result<usize> {
    decode_frame_until(input, out, limit, None)
}

/// [`decode_frame`], stopping once `stop` bytes are out (skipping the
/// checksum, which covers the whole content).
fn decode_frame_until(
    input: &[u8],
    out: &mut Vec<u8>,
    limit: usize,
    stop: Option<usize>,
) -> Result<usize> {
    let descriptor = *input
        .first()
        .ok_or_else(|| ZstdError::Malformed("truncated frame header".into()))?;
    let fcs_flag = descriptor >> 6;
    let single_segment = descriptor & 0x20 != 0;
    if descriptor & 0x08 != 0 {
        return malformed("reserved frame header bit set");
    }
    let checksum = descriptor & 0x04 != 0;
    let dict_bytes = [0usize, 1, 2, 4][(descriptor & 3) as usize];
    let mut at = 1;
    if !single_segment {
        at += 1; // window descriptor: the whole frame is kept in memory anyway
    }
    let dictionary = read_le(input, at, dict_bytes)?;
    if dictionary != 0 {
        return Err(ZstdError::Unsupported(
            "the frame needs a dictionary".into(),
        ));
    }
    at += dict_bytes;
    let fcs_bytes = match (fcs_flag, single_segment) {
        (0, true) => 1,
        (0, false) => 0,
        (1, _) => 2,
        (2, _) => 4,
        _ => 8,
    };
    let declared = read_le(input, at, fcs_bytes)?;
    let declared = if fcs_bytes == 2 {
        declared + 256
    } else {
        declared
    };
    at += fcs_bytes;
    if fcs_bytes > 0 && declared > limit as u64 && stop.is_none() {
        return Err(ZstdError::TooLarge(limit));
    }
    let frame_start = out.len();
    let mut state = FrameState::default();
    loop {
        let header = read_le(input, at, 3)? as u32;
        at += 3;
        let last = header & 1 != 0;
        let kind = (header >> 1) & 3;
        let size = (header >> 3) as usize;
        match kind {
            0 => {
                let raw = input
                    .get(at..at + size)
                    .ok_or_else(|| ZstdError::Malformed("truncated raw block".into()))?;
                room(out, size, limit)?;
                out.extend_from_slice(raw);
                at += size;
            }
            1 => {
                let byte = *input
                    .get(at)
                    .ok_or_else(|| ZstdError::Malformed("truncated RLE block".into()))?;
                room(out, size, limit)?;
                out.resize(out.len() + size, byte);
                at += 1;
            }
            2 => {
                if size > MAX_BLOCK {
                    return malformed("a compressed block is larger than 128 KiB");
                }
                let block = input
                    .get(at..at + size)
                    .ok_or_else(|| ZstdError::Malformed("truncated compressed block".into()))?;
                decode_block(block, out, frame_start, &mut state, limit)?;
                at += size;
            }
            _ => return malformed("reserved block type"),
        }
        if last {
            break;
        }
        if stop.is_some_and(|stop| out.len() - frame_start >= stop) {
            return Ok(at);
        }
    }
    if fcs_bytes > 0 && (out.len() - frame_start) as u64 != declared {
        return malformed("the frame's content size does not match its header");
    }
    if checksum {
        let expected = read_le(input, at, 4)? as u32;
        at += 4;
        let actual = xxh64(&out[frame_start..], 0) as u32;
        if actual != expected {
            return malformed("content checksum mismatch");
        }
    }
    Ok(at)
}

/// A little-endian integer of `count` bytes at `at`.
fn read_le(input: &[u8], at: usize, count: usize) -> Result<u64> {
    let bytes = input
        .get(at..at + count)
        .ok_or_else(|| ZstdError::Malformed("truncated header".into()))?;
    Ok(bytes
        .iter()
        .rev()
        .fold(0u64, |value, byte| (value << 8) | u64::from(*byte)))
}

fn room(out: &[u8], more: usize, limit: usize) -> Result<()> {
    if out.len().saturating_add(more) > limit {
        Err(ZstdError::TooLarge(limit))
    } else {
        Ok(())
    }
}

/// What persists between the blocks of one frame.
struct FrameState {
    huffman: Option<HufTable>,
    literal_lengths: Option<FseTable>,
    offsets: Option<FseTable>,
    match_lengths: Option<FseTable>,
    repeats: [usize; 3],
}

impl Default for FrameState {
    fn default() -> Self {
        Self {
            huffman: None,
            literal_lengths: None,
            offsets: None,
            match_lengths: None,
            repeats: [1, 4, 8],
        }
    }
}

// --------------------------------------------------------------- bit reads

/// Little-endian word at `byte`, zero-padded past the end.
fn load(data: &[u8], byte: usize) -> u64 {
    let mut word = [0u8; 8];
    if let Some(tail) = data.get(byte..) {
        let take = tail.len().min(8);
        word[..take].copy_from_slice(&tail[..take]);
    }
    u64::from_le_bytes(word)
}

fn mask(bits: u32) -> u64 {
    if bits >= 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    }
}

/// A forward, LSB-first bit reader (FSE table descriptions).
struct ForwardBits<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> ForwardBits<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    fn peek(&self, bits: u32) -> u64 {
        (load(self.data, self.position / 8) >> (self.position % 8)) & mask(bits)
    }

    fn consume(&mut self, bits: u32) -> Result<()> {
        self.position += bits as usize;
        if self.position > self.data.len() * 8 {
            return malformed("an FSE table description runs past its data");
        }
        Ok(())
    }

    fn read(&mut self, bits: u32) -> Result<u64> {
        let value = self.peek(bits);
        self.consume(bits)?;
        Ok(value)
    }

    fn bytes_used(&self) -> usize {
        self.position.div_ceil(8)
    }
}

/// A backward bit reader: the stream is read from its end, past a 1-bit
/// marker in its last byte, highest bits first. Reads past the start yield
/// zeros, and [`Self::overflowed`] reports them.
struct BackwardBits<'a> {
    data: &'a [u8],
    /// Unread bits are positions `[0, remaining)`.
    remaining: isize,
}

impl<'a> BackwardBits<'a> {
    fn new(data: &'a [u8]) -> Result<Self> {
        let last = match data.last() {
            Some(byte) if *byte != 0 => *byte,
            _ => return malformed("a bitstream lacks its end marker"),
        };
        let marker = 7 - last.leading_zeros() as isize;
        Ok(Self {
            data,
            remaining: (data.len() as isize - 1) * 8 + marker,
        })
    }

    fn peek(&self, bits: u32) -> u64 {
        if bits == 0 {
            return 0;
        }
        let low = self.remaining - bits as isize;
        if low >= 0 {
            let low = low as usize;
            (load(self.data, low / 8) >> (low % 8)) & mask(bits)
        } else if self.remaining <= 0 {
            0
        } else {
            let available = self.remaining as u32;
            (load(self.data, 0) & mask(available)) << (bits - available)
        }
    }

    fn skip(&mut self, bits: u32) {
        self.remaining -= bits as isize;
    }

    fn read(&mut self, bits: u32) -> u64 {
        let value = self.peek(bits);
        self.skip(bits);
        value
    }

    fn overflowed(&self) -> bool {
        self.remaining < 0
    }
}

// ---------------------------------------------------------------------- FSE

#[derive(Clone, Copy, Debug, Default)]
struct FseEntry {
    symbol: u8,
    bits: u8,
    baseline: u16,
}

#[derive(Clone, Debug)]
struct FseTable {
    log: u32,
    entries: Vec<FseEntry>,
}

impl FseTable {
    fn rle(symbol: u8) -> Self {
        Self {
            log: 0,
            entries: vec![FseEntry {
                symbol,
                bits: 0,
                baseline: 0,
            }],
        }
    }

    fn entry(&self, state: usize) -> Result<FseEntry> {
        self.entries
            .get(state)
            .copied()
            .ok_or_else(|| ZstdError::Malformed("an FSE state is out of range".into()))
    }

    fn update(&self, state: usize, bits: &mut BackwardBits<'_>) -> Result<usize> {
        let entry = self.entry(state)?;
        Ok(usize::from(entry.baseline) + bits.read(u32::from(entry.bits)) as usize)
    }
}

/// Reads an FSE table description; returns the normalized counts and the
/// accuracy log.
fn read_distribution(
    bits: &mut ForwardBits<'_>,
    max_symbol: usize,
    max_log: u32,
) -> Result<(Vec<i32>, u32)> {
    let log = bits.read(4)? as u32 + 5;
    if log > max_log {
        return malformed("an FSE accuracy log is too large");
    }
    let mut norm = Vec::new();
    let mut remaining: i32 = (1 << log) + 1;
    let mut threshold: i32 = 1 << log;
    let mut nb_bits = log + 1;
    let mut previous_zero = false;
    while remaining > 1 && norm.len() <= max_symbol {
        if previous_zero {
            loop {
                let repeat = bits.read(2)? as usize;
                norm.resize(norm.len() + repeat, 0);
                if repeat != 3 {
                    break;
                }
            }
            if norm.len() > max_symbol {
                return malformed("an FSE description names too many symbols");
            }
        }
        let max = (2 * threshold - 1) - remaining;
        let low = bits.peek(nb_bits - 1) as i32;
        let mut count = if low < max {
            bits.consume(nb_bits - 1)?;
            low
        } else {
            let mut value = bits.peek(nb_bits) as i32;
            if value >= threshold {
                value -= max;
            }
            bits.consume(nb_bits)?;
            value
        };
        count -= 1;
        remaining -= count.abs();
        norm.push(count);
        previous_zero = count == 0;
        while remaining < threshold {
            nb_bits -= 1;
            threshold >>= 1;
        }
    }
    if remaining != 1 || norm.len() > max_symbol + 1 {
        return malformed("an FSE distribution does not sum to its table size");
    }
    Ok((norm, log))
}

fn build_fse(norm: &[i32], log: u32) -> Result<FseTable> {
    let size = 1usize << log;
    let mut entries = vec![FseEntry::default(); size];
    let mut next = vec![0u32; norm.len()];
    let mut high = size;
    for (symbol, &count) in norm.iter().enumerate() {
        if count == -1 {
            high = high
                .checked_sub(1)
                .ok_or_else(|| ZstdError::Malformed("an FSE distribution overflows".into()))?;
            entries[high].symbol = symbol as u8;
            next[symbol] = 1;
        } else if count > 0 {
            next[symbol] = count as u32;
        }
    }
    let spread: i64 = norm
        .iter()
        .filter(|count| **count > 0)
        .map(|count| i64::from(*count))
        .sum();
    if spread != high as i64 {
        return malformed("an FSE distribution does not fill its table");
    }
    let step = (size >> 1) + (size >> 3) + 3;
    let table_mask = size - 1;
    let mut position = 0usize;
    for (symbol, &count) in norm.iter().enumerate() {
        for _ in 0..count.max(0) {
            entries[position].symbol = symbol as u8;
            loop {
                position = (position + step) & table_mask;
                if position < high {
                    break;
                }
            }
        }
    }
    if position != 0 {
        return malformed("an FSE distribution does not fill its table");
    }
    for entry in &mut entries {
        let state = next[usize::from(entry.symbol)];
        next[usize::from(entry.symbol)] += 1;
        let high_bit = 31 - state.leading_zeros();
        let bits = log - high_bit;
        entry.bits = bits as u8;
        entry.baseline = ((state << bits) as usize - size) as u16;
    }
    Ok(FseTable { log, entries })
}

// ------------------------------------------------------------------ Huffman

#[derive(Clone, Debug)]
struct HufTable {
    max_bits: u32,
    /// `(symbol, bits)` for every `max_bits`-bit prefix.
    entries: Vec<(u8, u8)>,
}

/// Reads a Huffman tree description; returns the table and bytes used.
fn read_huffman(data: &[u8]) -> Result<(HufTable, usize)> {
    let header = *data
        .first()
        .ok_or_else(|| ZstdError::Malformed("missing Huffman tree".into()))?;
    let mut weights: Vec<u8> = Vec::new();
    let used = if header < 128 {
        let size = usize::from(header);
        let compressed = data
            .get(1..1 + size)
            .ok_or_else(|| ZstdError::Malformed("truncated Huffman weights".into()))?;
        let mut forward = ForwardBits::new(compressed);
        let (norm, log) = read_distribution(&mut forward, 255, 6)?;
        let table = build_fse(&norm, log)?;
        let stream = compressed
            .get(forward.bytes_used()..)
            .ok_or_else(|| ZstdError::Malformed("truncated Huffman weights".into()))?;
        let mut bits = BackwardBits::new(stream)?;
        let mut first = bits.read(log) as usize;
        let mut second = bits.read(log) as usize;
        loop {
            weights.push(table.entry(first)?.symbol);
            first = table.update(first, &mut bits)?;
            if bits.overflowed() {
                weights.push(table.entry(second)?.symbol);
                break;
            }
            weights.push(table.entry(second)?.symbol);
            second = table.update(second, &mut bits)?;
            if bits.overflowed() {
                weights.push(table.entry(first)?.symbol);
                break;
            }
            if weights.len() > 255 {
                return malformed("too many Huffman weights");
            }
        }
        1 + size
    } else {
        let count = usize::from(header - 127);
        let packed = data
            .get(1..1 + count.div_ceil(2))
            .ok_or_else(|| ZstdError::Malformed("truncated Huffman weights".into()))?;
        for index in 0..count {
            let byte = packed[index / 2];
            weights.push(if index % 2 == 0 { byte >> 4 } else { byte & 15 });
        }
        1 + count.div_ceil(2)
    };
    let mut sum: u32 = 0;
    for &weight in &weights {
        if weight > 11 {
            return malformed("a Huffman weight is too large");
        }
        if weight > 0 {
            sum += 1 << (weight - 1);
        }
    }
    if sum == 0 {
        return malformed("a Huffman tree has no symbols");
    }
    let max_bits = 32 - sum.leading_zeros();
    let rest = (1u32 << max_bits) - sum;
    if !rest.is_power_of_two() || max_bits > 11 {
        return malformed("Huffman weights do not complete a tree");
    }
    weights.push((31 - rest.leading_zeros() + 1) as u8);
    if weights.len() > 256 {
        return malformed("too many Huffman symbols");
    }
    let mut order: Vec<(u8, usize)> = weights
        .iter()
        .enumerate()
        .filter(|(_, weight)| **weight > 0)
        .map(|(symbol, weight)| (*weight, symbol))
        .collect();
    order.sort_unstable();
    let size = 1usize << max_bits;
    let mut entries = Vec::with_capacity(size);
    for (weight, symbol) in order {
        let bits = max_bits + 1 - u32::from(weight);
        let span = 1usize << (max_bits - bits);
        for _ in 0..span {
            entries.push((symbol as u8, bits as u8));
        }
    }
    if entries.len() != size {
        return malformed("Huffman codes do not fill their table");
    }
    Ok((HufTable { max_bits, entries }, used))
}

fn huffman_stream(table: &HufTable, data: &[u8], count: usize, out: &mut Vec<u8>) -> Result<()> {
    let mut bits = BackwardBits::new(data)?;
    for _ in 0..count {
        let (symbol, length) = table.entries[bits.peek(table.max_bits) as usize];
        bits.skip(u32::from(length));
        out.push(symbol);
    }
    if bits.remaining != 0 {
        return malformed("a Huffman stream was not consumed exactly");
    }
    Ok(())
}

// ------------------------------------------------------------------- blocks

fn decode_block(
    block: &[u8],
    out: &mut Vec<u8>,
    frame_start: usize,
    state: &mut FrameState,
    limit: usize,
) -> Result<()> {
    let (literals, used) = read_literals(block, state)?;
    let sequences = &block[used..];
    decode_sequences(sequences, &literals, out, frame_start, state, limit)
}

fn read_literals(block: &[u8], state: &mut FrameState) -> Result<(Vec<u8>, usize)> {
    let first = *block
        .first()
        .ok_or_else(|| ZstdError::Malformed("empty compressed block".into()))?;
    let kind = first & 3;
    let format = (first >> 2) & 3;
    let byte = |index: usize| -> Result<usize> {
        block
            .get(index)
            .map(|b| usize::from(*b))
            .ok_or_else(|| ZstdError::Malformed("truncated literals header".into()))
    };
    match kind {
        0 | 1 => {
            let (size, header) = match format {
                0 | 2 => (usize::from(first >> 3), 1),
                1 => (usize::from(first >> 4) + (byte(1)? << 4), 2),
                _ => (
                    usize::from(first >> 4) + (byte(1)? << 4) + (byte(2)? << 12),
                    3,
                ),
            };
            if size > MAX_BLOCK {
                return malformed("literals are larger than a block");
            }
            if kind == 0 {
                let raw = block
                    .get(header..header + size)
                    .ok_or_else(|| ZstdError::Malformed("truncated raw literals".into()))?;
                Ok((raw.to_vec(), header + size))
            } else {
                let value = byte(header)? as u8;
                Ok((vec![value; size], header + 1))
            }
        }
        _ => {
            let (header, four, width) = match format {
                0 => (3, false, 10),
                1 => (3, true, 10),
                2 => (4, true, 14),
                _ => (5, true, 18),
            };
            let packed = read_le(block, 0, header)?;
            let regenerated = ((packed >> 4) & mask(width)) as usize;
            let compressed = ((packed >> (4 + width)) & mask(width)) as usize;
            if regenerated > MAX_BLOCK {
                return malformed("literals are larger than a block");
            }
            let data = block
                .get(header..header + compressed)
                .ok_or_else(|| ZstdError::Malformed("truncated compressed literals".into()))?;
            let (table, used) = if kind == 2 {
                let (table, used) = read_huffman(data)?;
                state.huffman = Some(table);
                (state.huffman.as_ref(), used)
            } else {
                (state.huffman.as_ref(), 0)
            };
            let table = table
                .ok_or_else(|| ZstdError::Malformed("treeless literals with no tree".into()))?;
            let streams = &data[used..];
            let mut literals = Vec::with_capacity(regenerated);
            if four {
                let jump = streams
                    .get(..6)
                    .ok_or_else(|| ZstdError::Malformed("truncated jump table".into()))?;
                let sizes = [
                    usize::from(u16::from_le_bytes([jump[0], jump[1]])),
                    usize::from(u16::from_le_bytes([jump[2], jump[3]])),
                    usize::from(u16::from_le_bytes([jump[4], jump[5]])),
                ];
                let total: usize = sizes.iter().sum::<usize>() + 6;
                if total > streams.len() {
                    return malformed("a jump table runs past its literals");
                }
                let segment = regenerated.div_ceil(4);
                if 3 * segment > regenerated {
                    return malformed("too few literals for four streams");
                }
                let mut start = 6;
                for (index, size) in sizes
                    .iter()
                    .copied()
                    .chain(std::iter::once(streams.len() - total))
                    .enumerate()
                {
                    let count = if index < 3 {
                        segment
                    } else {
                        regenerated - 3 * segment
                    };
                    huffman_stream(table, &streams[start..start + size], count, &mut literals)?;
                    start += size;
                }
            } else {
                huffman_stream(table, streams, regenerated, &mut literals)?;
            }
            Ok((literals, header + compressed))
        }
    }
}

const LL_BASE: [u32; 36] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24, 28, 32, 40, 48, 64,
    128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536,
];
const LL_BITS: [u8; 36] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10, 11,
    12, 13, 14, 15, 16,
];
const ML_BASE: [u32; 53] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27,
    28, 29, 30, 31, 32, 33, 34, 35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 131, 259, 515, 1027,
    2051, 4099, 8195, 16387, 32771, 65539,
];
const ML_BITS: [u8; 53] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
];
const LL_DEFAULT: [i32; 36] = [
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];
const ML_DEFAULT: [i32; 53] = [
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];
const OF_DEFAULT: [i32; 29] = [
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
];

/// The sequence table for one stream, by its compression mode.
fn sequence_table(
    mode: u8,
    data: &[u8],
    at: &mut usize,
    previous: &Option<FseTable>,
    defaults: (&[i32], u32),
    max_symbol: usize,
    max_log: u32,
) -> Result<FseTable> {
    match mode {
        0 => build_fse(defaults.0, defaults.1),
        1 => {
            let symbol = *data
                .get(*at)
                .ok_or_else(|| ZstdError::Malformed("truncated RLE sequence table".into()))?;
            *at += 1;
            if usize::from(symbol) > max_symbol {
                return malformed("an RLE sequence symbol is out of range");
            }
            Ok(FseTable::rle(symbol))
        }
        2 => {
            let mut bits = ForwardBits::new(
                data.get(*at..)
                    .ok_or_else(|| ZstdError::Malformed("truncated sequence table".into()))?,
            );
            let (norm, log) = read_distribution(&mut bits, max_symbol, max_log)?;
            *at += bits.bytes_used();
            build_fse(&norm, log)
        }
        _ => previous.clone().ok_or_else(|| {
            ZstdError::Malformed("a repeated sequence table has no predecessor".into())
        }),
    }
}

fn decode_sequences(
    data: &[u8],
    literals: &[u8],
    out: &mut Vec<u8>,
    frame_start: usize,
    state: &mut FrameState,
    limit: usize,
) -> Result<()> {
    let first = data.first().map_or(0, |b| usize::from(*b));
    let (count, mut at) = match first {
        0 => (0, 1),
        1..=127 => (first, 1),
        128..=254 => {
            let second = usize::from(
                *data
                    .get(1)
                    .ok_or_else(|| ZstdError::Malformed("truncated sequence count".into()))?,
            );
            (((first - 128) << 8) + second, 2)
        }
        _ => (read_le(data, 1, 2)? as usize + 0x7F00, 3),
    };
    if count == 0 {
        room(out, literals.len(), limit)?;
        out.extend_from_slice(literals);
        return Ok(());
    }
    let modes = *data
        .get(at)
        .ok_or_else(|| ZstdError::Malformed("truncated sequence modes".into()))?;
    at += 1;
    if modes & 3 != 0 {
        return malformed("reserved sequence mode bits set");
    }
    let ll = sequence_table(
        modes >> 6,
        data,
        &mut at,
        &state.literal_lengths,
        (&LL_DEFAULT, 6),
        35,
        9,
    )?;
    let of = sequence_table(
        (modes >> 4) & 3,
        data,
        &mut at,
        &state.offsets,
        (&OF_DEFAULT, 5),
        31,
        8,
    )?;
    let ml = sequence_table(
        (modes >> 2) & 3,
        data,
        &mut at,
        &state.match_lengths,
        (&ML_DEFAULT, 6),
        52,
        9,
    )?;
    let stream = data
        .get(at..)
        .ok_or_else(|| ZstdError::Malformed("truncated sequence stream".into()))?;
    let mut bits = BackwardBits::new(stream)?;
    let mut ll_state = bits.read(ll.log) as usize;
    let mut of_state = bits.read(of.log) as usize;
    let mut ml_state = bits.read(ml.log) as usize;
    let mut literal_at = 0usize;
    for index in 0..count {
        let of_code = u32::from(of.entry(of_state)?.symbol);
        let ml_code = usize::from(ml.entry(ml_state)?.symbol);
        let ll_code = usize::from(ll.entry(ll_state)?.symbol);
        if of_code > 31 || ml_code > 52 || ll_code > 35 {
            return malformed("a sequence code is out of range");
        }
        let offset_value = (1u64 << of_code) + bits.read(of_code);
        let match_length =
            ML_BASE[ml_code] as usize + bits.read(u32::from(ML_BITS[ml_code])) as usize;
        let literal_length =
            LL_BASE[ll_code] as usize + bits.read(u32::from(LL_BITS[ll_code])) as usize;
        let offset = resolve_offset(&mut state.repeats, offset_value, literal_length)?;
        if index + 1 < count {
            ll_state = ll.update(ll_state, &mut bits)?;
            ml_state = ml.update(ml_state, &mut bits)?;
            of_state = of.update(of_state, &mut bits)?;
        }
        let copied = literals
            .get(literal_at..literal_at + literal_length)
            .ok_or_else(|| ZstdError::Malformed("a sequence runs past its literals".into()))?;
        room(out, literal_length + match_length, limit)?;
        out.extend_from_slice(copied);
        literal_at += literal_length;
        if offset == 0 || offset > out.len() - frame_start {
            return malformed("a match reaches before the start of the frame");
        }
        let from = out.len() - offset;
        if offset >= match_length {
            out.extend_from_within(from..from + match_length);
        } else {
            for position in 0..match_length {
                let byte = out[from + position];
                out.push(byte);
            }
        }
    }
    if bits.remaining != 0 {
        return malformed("a sequence stream was not consumed exactly");
    }
    let rest = &literals[literal_at..];
    room(out, rest.len(), limit)?;
    out.extend_from_slice(rest);
    state.literal_lengths = Some(ll);
    state.offsets = Some(of);
    state.match_lengths = Some(ml);
    Ok(())
}

/// The match offset for an offset value, maintaining the repeat history.
fn resolve_offset(repeats: &mut [usize; 3], value: u64, literal_length: usize) -> Result<usize> {
    if value > 3 {
        let offset = (value - 3) as usize;
        repeats[2] = repeats[1];
        repeats[1] = repeats[0];
        repeats[0] = offset;
        return Ok(offset);
    }
    // With no literals the repeat codes shift by one.
    let index = if literal_length == 0 {
        value
    } else {
        value - 1
    };
    let offset = match index {
        0 => repeats[0],
        1 => repeats[1],
        2 => repeats[2],
        _ => repeats[0]
            .checked_sub(1)
            .ok_or_else(|| ZstdError::Malformed("a repeat offset underflows".into()))?,
    };
    match index {
        0 => {}
        1 => {
            repeats[1] = repeats[0];
            repeats[0] = offset;
        }
        _ => {
            repeats[2] = repeats[1];
            repeats[1] = repeats[0];
            repeats[0] = offset;
        }
    }
    Ok(offset)
}

// -------------------------------------------------------------------- XXH64

const P1: u64 = 0x9E37_79B1_85EB_CA87;
const P2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const P3: u64 = 0x1656_67B1_9E37_79F9;
const P4: u64 = 0x85EB_CA77_C2B2_AE63;
const P5: u64 = 0x27D4_EB2F_1656_67C5;

fn round(accumulator: u64, input: u64) -> u64 {
    accumulator
        .wrapping_add(input.wrapping_mul(P2))
        .rotate_left(31)
        .wrapping_mul(P1)
}

fn merge(accumulator: u64, value: u64) -> u64 {
    (accumulator ^ round(0, value))
        .wrapping_mul(P1)
        .wrapping_add(P4)
}

/// XXH64 of `data` (the zstd content checksum is its low 32 bits).
pub fn xxh64(data: &[u8], seed: u64) -> u64 {
    let word = |at: usize| u64::from_le_bytes(data[at..at + 8].try_into().unwrap_or([0; 8]));
    let mut at = 0;
    let mut hash = if data.len() >= 32 {
        let mut v = [
            seed.wrapping_add(P1).wrapping_add(P2),
            seed.wrapping_add(P2),
            seed,
            seed.wrapping_sub(P1),
        ];
        while at + 32 <= data.len() {
            for (lane, value) in v.iter_mut().enumerate() {
                *value = round(*value, word(at + lane * 8));
            }
            at += 32;
        }
        let mut hash = v[0]
            .rotate_left(1)
            .wrapping_add(v[1].rotate_left(7))
            .wrapping_add(v[2].rotate_left(12))
            .wrapping_add(v[3].rotate_left(18));
        for value in v {
            hash = merge(hash, value);
        }
        hash
    } else {
        seed.wrapping_add(P5)
    };
    hash = hash.wrapping_add(data.len() as u64);
    while at + 8 <= data.len() {
        hash ^= round(0, word(at));
        hash = hash.rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
        at += 8;
    }
    if at + 4 <= data.len() {
        let half = u32::from_le_bytes(data[at..at + 4].try_into().unwrap_or([0; 4]));
        hash ^= u64::from(half).wrapping_mul(P1);
        hash = hash.rotate_left(23).wrapping_mul(P2).wrapping_add(P3);
        at += 4;
    }
    while at < data.len() {
        hash ^= u64::from(data[at]).wrapping_mul(P5);
        hash = hash.rotate_left(11).wrapping_mul(P1);
        at += 1;
    }
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(P2);
    hash ^= hash >> 29;
    hash = hash.wrapping_mul(P3);
    hash ^ (hash >> 32)
}

#[cfg(test)]
mod tests;
