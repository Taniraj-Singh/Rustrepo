//! Byte-oriented run-length encoding.
//!
//! The stream is a sequence of tokens, each introduced by a control byte:
//!
//! ```text
//!   control & 0x80 == 0   literal run: (control + 1) bytes follow verbatim
//!   control & 0x80 != 0   repeat run:  one byte follows, repeated
//!                         (control & 0x7f) + 1 times
//! ```
//!
//! Runs and literal chunks are capped at 128 elements, so both counts fit the
//! seven payload bits. The decoder validates every length against the remaining
//! input and against the caller's output cap.

use crate::error::{Error, Result};

/// Minimum run of identical bytes worth encoding as a repeat token rather than
/// leaving in a literal run.
const MIN_RUN: usize = 3;
/// Maximum elements a single token can describe.
const MAX_TOKEN: usize = 128;

/// Compresses `input` with run-length encoding.
pub fn encode(input: &[u8]) -> Vec<u8> {
    let n = input.len();
    let mut out = Vec::with_capacity(n / 2 + 1);
    let mut i = 0;
    let mut literal_start = 0;

    while i < n {
        let byte = input[i];
        let mut run = 1;
        while i + run < n && input[i + run] == byte && run < MAX_TOKEN {
            run += 1;
        }

        if run >= MIN_RUN {
            flush_literals(&mut out, &input[literal_start..i]);
            out.push(0x80 | (run as u8 - 1));
            out.push(byte);
            i += run;
            literal_start = i;
        } else {
            i += 1;
            if i - literal_start == MAX_TOKEN {
                flush_literals(&mut out, &input[literal_start..i]);
                literal_start = i;
            }
        }
    }
    flush_literals(&mut out, &input[literal_start..i]);
    out
}

/// Emits `literals` as one or more literal tokens, splitting at [`MAX_TOKEN`].
fn flush_literals(out: &mut Vec<u8>, literals: &[u8]) {
    for chunk in literals.chunks(MAX_TOKEN) {
        // chunk.len() is in 1..=128, so (len - 1) fits in seven bits with the
        // high bit clear, marking a literal token.
        out.push(chunk.len() as u8 - 1);
        out.extend_from_slice(chunk);
    }
}

/// Decompresses an RLE stream, producing at most `max_output` bytes.
pub fn decode(input: &[u8], max_output: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut p = 0;

    while p < input.len() {
        let control = input[p];
        p += 1;

        if control & 0x80 == 0 {
            let count = control as usize + 1;
            let end = p.checked_add(count).ok_or(Error::eof("rle literal run"))?;
            let chunk = input.get(p..end).ok_or(Error::eof("rle literal run"))?;
            reserve_within_cap(out.len(), count, max_output)?;
            out.extend_from_slice(chunk);
            p = end;
        } else {
            let count = (control & 0x7f) as usize + 1;
            let &byte = input.get(p).ok_or(Error::eof("rle repeat value"))?;
            p += 1;
            reserve_within_cap(out.len(), count, max_output)?;
            out.resize(out.len() + count, byte);
        }
    }
    Ok(out)
}

/// Checks that `have + add` stays within `cap`, reporting a structured limit
/// error otherwise. Uses checked arithmetic so a colossal claimed length cannot
/// overflow the addition.
fn reserve_within_cap(have: usize, add: usize, cap: usize) -> Result<()> {
    let total =
        have.checked_add(add)
            .ok_or(Error::limit("rle output length", u64::MAX, cap as u64))?;
    if total > cap {
        return Err(Error::limit("rle output length", total as u64, cap as u64));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(data: &[u8]) {
        let enc = encode(data);
        let dec = decode(&enc, data.len()).unwrap();
        assert_eq!(dec, data);
    }

    #[test]
    fn roundtrips() {
        roundtrip(b"");
        roundtrip(b"a");
        roundtrip(b"abcdefg");
        roundtrip(&[0u8; 500]);
        roundtrip(b"aaabbbcccddd");
        roundtrip(&[5u8; 128]);
        roundtrip(&[5u8; 129]);
        let mixed: Vec<u8> = (0..300)
            .map(|i| if i % 7 == 0 { 0 } else { i as u8 })
            .collect();
        roundtrip(&mixed);
    }

    #[test]
    fn long_runs_compress() {
        let enc = encode(&[42u8; 1000]);
        assert!(enc.len() < 50);
    }

    #[test]
    fn truncated_input_is_rejected() {
        // Literal token claims 5 bytes, none follow.
        assert!(decode(&[0x04], usize::MAX).is_err());
        // Repeat token with no value byte.
        assert!(decode(&[0x82], usize::MAX).is_err());
    }

    #[test]
    fn output_cap_enforced() {
        let enc = encode(&[1u8; 1000]);
        assert!(matches!(
            decode(&enc, 100),
            Err(Error::LimitExceeded { .. })
        ));
    }
}
