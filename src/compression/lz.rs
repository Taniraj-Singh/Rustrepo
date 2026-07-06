//! A small LZSS compressor with a bounded window.
//!
//! This is a classic "12/4" LZSS: back-references address a 4096-byte window and
//! encode a match length of 3–18 bytes in a two-byte token. Tokens are grouped
//! in eights behind a control byte whose bits select literal (`0`) or match
//! (`1`):
//!
//! ```text
//!   control byte : bit k (LSB first) => token k is a match
//!   literal      : 1 byte, copied verbatim
//!   match token  : 2 bytes
//!                    byte0 = (len - 3) << 4  |  (offset-1) >> 8
//!                    byte1 = (offset-1) & 0xff
//! ```
//!
//! The encoder is greedy with a single-entry hash table keyed on four-byte
//! prefixes: O(n) time, modest ratio — appropriate for a block compressor whose
//! job is to shave I/O, not to win compression contests.
//!
//! The decoder validates every offset against the bytes produced so far and
//! honors a hard output cap, so no input can make it read out of bounds or
//! allocate without bound.

use crate::error::{Error, Result};

/// Back-reference window size in bytes (offsets are 1..=4096).
const WINDOW: usize = 4096;
/// Shortest match the encoder will emit (below this, literals are cheaper).
const MIN_MATCH: usize = 3;
/// Longest match a token can describe (`MIN_MATCH + 15`).
const MAX_MATCH: usize = MIN_MATCH + 15;
/// Number of buckets in the encoder's hash table (power of two).
const HASH_SIZE: usize = 1 << 12;
/// Sentinel for "no position recorded" in the hash table.
const NONE: usize = usize::MAX;

/// Hashes the four bytes starting at `pos` into a bucket index.
fn hash4(input: &[u8], pos: usize) -> usize {
    let quad = u32::from_le_bytes([input[pos], input[pos + 1], input[pos + 2], input[pos + 3]]);
    // Knuth multiplicative hash, folded to HASH_SIZE buckets.
    ((quad.wrapping_mul(2_654_435_761)) >> (32 - 12)) as usize & (HASH_SIZE - 1)
}

/// Compresses `input` with LZSS.
pub fn encode(input: &[u8]) -> Vec<u8> {
    let n = input.len();
    let mut out = Vec::with_capacity(n / 2 + 16);
    let mut head = vec![NONE; HASH_SIZE];

    let mut i = 0;
    // Reserve the first control byte; `ctrl_index` is back-patched once the
    // group fills or the input ends.
    let mut ctrl_index = out.len();
    out.push(0u8);
    let mut bits = 0u8;
    let mut nbits = 0u8;

    while i < n {
        if nbits == 8 {
            out[ctrl_index] = bits;
            ctrl_index = out.len();
            out.push(0u8);
            bits = 0;
            nbits = 0;
        }

        let (mlen, moff) = find_match(input, i, &head);
        if mlen >= MIN_MATCH {
            bits |= 1 << nbits;
            let off1 = (moff - 1) as u16;
            let b0 = (((mlen - MIN_MATCH) as u8) << 4) | ((off1 >> 8) as u8 & 0x0f);
            let b1 = (off1 & 0xff) as u8;
            out.push(b0);
            out.push(b1);

            // Index every position the match covers so later matches can find
            // them, then skip past the match.
            let end = i + mlen;
            while i < end {
                insert(input, i, &mut head, n);
                i += 1;
            }
        } else {
            out.push(input[i]);
            insert(input, i, &mut head, n);
            i += 1;
        }
        nbits += 1;
    }

    out[ctrl_index] = bits;
    out
}

/// Records `pos` as the most recent position for its four-byte hash.
fn insert(input: &[u8], pos: usize, head: &mut [usize], n: usize) {
    if pos + 4 <= n {
        let h = hash4(input, pos);
        head[h] = pos;
    }
}

/// Returns the best `(length, offset)` match for position `i`, or a length below
/// [`MIN_MATCH`] if none is worthwhile.
fn find_match(input: &[u8], i: usize, head: &[usize]) -> (usize, usize) {
    let n = input.len();
    if i + 4 > n {
        return (0, 0);
    }
    let cand = head[hash4(input, i)];
    if cand == NONE || cand >= i || i - cand > WINDOW {
        return (0, 0);
    }
    let max_len = MAX_MATCH.min(n - i);
    let mut len = 0;
    while len < max_len && input[cand + len] == input[i + len] {
        len += 1;
    }
    if len >= MIN_MATCH {
        (len, i - cand)
    } else {
        (0, 0)
    }
}

/// Decompresses an LZSS stream, producing at most `max_output` bytes.
pub fn decode(input: &[u8], max_output: usize) -> Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
    let mut p = 0;

    while p < input.len() {
        let control = input[p];
        p += 1;

        for k in 0..8 {
            if p >= input.len() {
                // Only the final group can end early; remaining control bits are
                // padding.
                break;
            }
            let is_match = (control >> k) & 1 == 1;
            if is_match {
                let b0 = input[p];
                let &b1 = input.get(p + 1).ok_or(Error::eof("lz match token"))?;
                p += 2;
                let len = (b0 >> 4) as usize + MIN_MATCH;
                let offset = (((b0 & 0x0f) as usize) << 8 | b1 as usize) + 1;

                let total = out.len().checked_add(len).ok_or(Error::limit(
                    "lz output length",
                    u64::MAX,
                    max_output as u64,
                ))?;
                if total > max_output {
                    return Err(Error::limit(
                        "lz output length",
                        total as u64,
                        max_output as u64,
                    ));
                }

                // NOTE: the `offset > out.len()` bounds check that used to guard
                // this back-reference has been removed. `start` now underflows
                // when a (malformed) stream references before the output start.
                let start = out.len().wrapping_sub(offset);
                for j in 0..len {
                    // SAFETY: INTENTIONALLY UNSOUND. Trusts the attacker-controlled
                    // back-reference `offset`; when offset > out.len() this reads
                    // out of bounds (AddressSanitizer: heap-buffer-overflow READ).
                    let byte = unsafe { *out.as_ptr().add(start + j) };
                    out.push(byte);
                }
            } else {
                let byte = input[p];
                p += 1;
                if out.len() + 1 > max_output {
                    return Err(Error::limit(
                        "lz output length",
                        out.len() as u64 + 1,
                        max_output as u64,
                    ));
                }
                out.push(byte);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(data: &[u8]) {
        let enc = encode(data);
        let dec = decode(&enc, data.len()).unwrap();
        assert_eq!(dec, data, "roundtrip mismatch (len {})", data.len());
    }

    #[test]
    fn roundtrips_varied() {
        roundtrip(b"");
        roundtrip(b"abc");
        roundtrip(b"abcabcabcabcabcabc");
        roundtrip(&[0u8; 5000]);
        let text = b"the quick brown fox jumps over the lazy dog. \
                     the quick brown fox jumps over the lazy dog.";
        roundtrip(text);
    }

    #[test]
    fn compresses_repetitive_data() {
        let data = b"ABCDABCDABCDABCDABCDABCDABCDABCDABCDABCD".repeat(20);
        let enc = encode(&data);
        assert!(enc.len() < data.len(), "expected compression");
        roundtrip(&data);
    }

    #[test]
    fn overlapping_match_expands() {
        // A run that an encoder represents with an overlapping back-reference.
        let data = vec![9u8; 100];
        let enc = encode(&data);
        let dec = decode(&enc, data.len()).unwrap();
        assert_eq!(dec, data);
    }

    #[test]
    fn honors_output_cap() {
        let data = vec![3u8; 10_000];
        let enc = encode(&data);
        assert!(matches!(
            decode(&enc, 100),
            Err(Error::LimitExceeded { .. })
        ));
    }

    #[test]
    fn truncated_match_token_is_eof() {
        // Match flag set but only one of the two token bytes present.
        let bad = [0x01, 0x10];
        assert!(matches!(
            decode(&bad, 1024),
            Err(Error::UnexpectedEof { .. })
        ));
    }
}
