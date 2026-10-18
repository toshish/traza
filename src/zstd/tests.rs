//! Test vectors were produced by the reference `zstd` CLI from synthetic
//! inputs that [`lines`], [`noise`] and friends regenerate here byte for
//! byte, so a vector checks the decoder against the reference encoder.

use super::*;

const WORDS: [&str; 20] = [
    "agent", "tool", "call", "span", "trace", "run", "edit", "patch", "model", "token", "cache",
    "read", "write", "file", "line", "test", "pass", "fail", "retry", "plan",
];

fn lcg(x: u64) -> u64 {
    x.wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407)
}

/// 2000 JSON lines of pseudo-random words (~190 KB: more than one block).
fn lines() -> Vec<u8> {
    let mut x = 7u64;
    let mut out = String::new();
    for n in 0..2000 {
        let mut words = Vec::new();
        for _ in 0..12 {
            x = lcg(x);
            words.push(WORDS[((x >> 33) % 20) as usize]);
        }
        out.push_str(&format!(
            "{{\"type\":\"response_item\",\"seq\":{n},\"text\":\"{}\"}}\n",
            words.join(" ")
        ));
    }
    out.into_bytes()
}

/// 20000 pseudo-random bytes: incompressible, so stored as raw blocks.
fn noise() -> Vec<u8> {
    let mut x = 11u64;
    (0..20000)
        .map(|_| {
            x = lcg(x);
            (x >> 56) as u8
        })
        .collect()
}

#[test]
fn reference_vectors_decode_to_their_inputs() {
    let lines = lines();
    assert_eq!(
        crate::payload::sha256_hex(&lines),
        "df7b40635ead31b1bb6b9448f353a30fbbd3532e26a22193a9d8d36072516569",
        "the generator must match the one the vectors were built from"
    );
    for (name, vector) in [
        (
            "level 1",
            &include_bytes!("../../tests/fixtures/zstd/lines-l1.zst")[..],
        ),
        (
            "level 19",
            &include_bytes!("../../tests/fixtures/zstd/lines-l19.zst")[..],
        ),
        (
            "no checksum",
            &include_bytes!("../../tests/fixtures/zstd/lines-nocheck.zst")[..],
        ),
    ] {
        assert!(is_zstd(vector), "{name}");
        let out = decompress(vector, 1 << 20).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(out == lines, "{name}: decoded bytes differ from the input");
    }
    assert_eq!(
        decompress(
            include_bytes!("../../tests/fixtures/zstd/noise.zst"),
            1 << 20
        ),
        Ok(noise())
    );
    assert_eq!(
        decompress(include_bytes!("../../tests/fixtures/zstd/rle.zst"), 1 << 20),
        Ok(vec![b'a'; 100_000])
    );
    assert_eq!(
        decompress(
            include_bytes!("../../tests/fixtures/zstd/small.zst"),
            1 << 20
        ),
        Ok(b"hello, traza\n".to_vec())
    );
    let mut both = b"hello, traza\n".to_vec();
    both.extend(vec![b'a'; 100_000]);
    assert_eq!(
        decompress(
            include_bytes!("../../tests/fixtures/zstd/two-frames.zst"),
            1 << 20
        ),
        Ok(both),
        "concatenated frames decode in order"
    );
}

#[test]
fn the_output_cap_holds() {
    let vector = include_bytes!("../../tests/fixtures/zstd/rle.zst");
    assert_eq!(decompress(vector, 99_999), Err(ZstdError::TooLarge(99_999)));
    let lines = include_bytes!("../../tests/fixtures/zstd/lines-l19.zst");
    assert_eq!(decompress(lines, 1000), Err(ZstdError::TooLarge(1000)));
}

#[test]
fn damage_is_refused_and_never_panics() {
    let vector = include_bytes!("../../tests/fixtures/zstd/lines-l19.zst").to_vec();
    // Every truncation is an error, not a panic or a short success.
    for cut in (0..vector.len()).step_by(97) {
        assert!(decompress(&vector[..cut], 1 << 20).is_err(), "cut at {cut}");
    }
    // Flipping bytes inside the compressed body is caught — by the decoder's
    // own checks or, at the latest, by the content checksum.
    let mut caught = 0;
    for at in (20..vector.len() - 8).step_by(131) {
        let mut damaged = vector.clone();
        damaged[at] ^= 0x5A;
        match decompress(&damaged, 1 << 20) {
            Err(_) => caught += 1,
            Ok(out) => assert_ne!(out, lines(), "a flip at {at} cannot decode to the original"),
        }
    }
    assert!(caught > 0);
    assert!(decompress(b"not zstd at all", 1 << 20).is_err());
    assert!(decompress(&[], 1 << 20).is_err());
}

#[test]
fn a_bad_checksum_is_refused() {
    let mut vector = include_bytes!("../../tests/fixtures/zstd/small.zst").to_vec();
    let last = vector.len() - 1;
    vector[last] ^= 1;
    assert_eq!(
        decompress(&vector, 1 << 20),
        Err(ZstdError::Malformed("content checksum mismatch".into()))
    );
}

#[test]
fn xxh64_matches_the_published_vectors() {
    assert_eq!(xxh64(b"", 0), 0xEF46_DB37_51D8_E999);
    // The 32-byte lane path and the tail paths, checked through the frames
    // above (whose checksums are the reference implementation's); here, a
    // seed changes the answer.
    assert_ne!(xxh64(b"abc", 0), xxh64(b"abc", 1));
}

#[test]
fn skippable_frames_are_skipped() {
    let mut stream = vec![0x50, 0x2A, 0x4D, 0x18, 3, 0, 0, 0, 1, 2, 3];
    stream.extend_from_slice(include_bytes!("../../tests/fixtures/zstd/small.zst"));
    assert_eq!(decompress(&stream, 1 << 20), Ok(b"hello, traza\n".to_vec()));
}

/// Cross-checks the decoder against the `zstd` CLI over a directory of real
/// `.zst` files, printing only counts. Run by hand:
/// `TRAZA_ZSTD_CORPUS=~/.codex/sessions cargo test --lib zstd::tests::corpus -- --ignored`.
#[test]
#[ignore]
fn corpus_agrees_with_the_reference_cli() {
    let Ok(root) = std::env::var("TRAZA_ZSTD_CORPUS") else {
        return;
    };
    let mut stack = vec![std::path::PathBuf::from(root)];
    let (mut agree, mut disagree, mut failed) = (0, 0, 0);
    let limit: usize = std::env::var("TRAZA_ZSTD_CORPUS_LIMIT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(usize::MAX);
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("zst")
                || agree + disagree + failed >= limit
            {
                continue;
            }
            let bytes = std::fs::read(&path).expect("read");
            let reference = std::process::Command::new("zstd")
                .args(["-dc", "--"])
                .arg(&path)
                .output()
                .expect("zstd runs");
            match decompress(&bytes, 4 << 30) {
                Ok(out) if out == reference.stdout => agree += 1,
                Ok(_) => disagree += 1,
                Err(_) => failed += 1,
            }
        }
    }
    eprintln!("zstd corpus: {agree} agree, {disagree} disagree, {failed} failed");
    assert_eq!((disagree, failed), (0, 0));
}
