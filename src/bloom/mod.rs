//! Bloom filters for skipping tables that cannot contain a key.
//!
//! Each SSTable stores a bloom filter over its keys. Before doing the work of a
//! block read, a point lookup asks the filter "could this key be here?". A
//! *no* is definitive and saves an I/O; a *yes* may be a false positive, in
//! which case the read proceeds as normal. Filters never produce false
//! negatives, so correctness never depends on them — only speed.
//!
//! The construction follows LevelDB: `k` probes derived from one base hash via
//! the double-hashing trick (a second hash is a bit-rotation of the first),
//! with the probe count stored in the filter's final byte. The false-positive
//! rate is roughly `0.6185 ^ bits_per_key`.
//!
//! Every reader is defensive: a filter that is too small or carries an
//! unrecognized probe count is treated as "might contain" (the safe answer),
//! never as a reason to panic or skip a real key.

/// Hashes a key into the 32-bit base value used for bloom probing.
///
/// This is LevelDB's `Hash`: a fast, non-cryptographic mixing function. It only
/// needs to distribute well, not resist attackers.
pub fn hash_key(data: &[u8]) -> u32 {
    const SEED: u32 = 0xbc9f_1d34;
    const M: u32 = 0xc6a4_a793;

    let mut h = SEED ^ (data.len() as u32).wrapping_mul(M);
    let mut chunks = data.chunks_exact(4);
    for c in chunks.by_ref() {
        let w = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
        h = h.wrapping_add(w);
        h = h.wrapping_mul(M);
        h ^= h >> 16;
    }
    let rem = chunks.remainder();
    if rem.len() >= 3 {
        h = h.wrapping_add((rem[2] as u32) << 16);
    }
    if rem.len() >= 2 {
        h = h.wrapping_add((rem[1] as u32) << 8);
    }
    if !rem.is_empty() {
        h = h.wrapping_add(rem[0] as u32);
        h = h.wrapping_mul(M);
        h ^= h >> 24;
    }
    h
}

/// The largest probe count we encode; clamps pathological `bits_per_key`.
const MAX_PROBES: usize = 30;
/// Minimum filter bit count, so tiny tables still get a usable filter.
const MIN_BITS: usize = 64;

/// Derives the probe count `k` from bits-per-key (`k = bits * ln2`).
fn probe_count(bits_per_key: usize) -> usize {
    // 0.69 ≈ ln(2). Clamp into a sane range.
    ((bits_per_key as f64 * 0.69) as usize).clamp(1, MAX_PROBES)
}

/// Builds a bloom filter over a set of pre-computed key hashes.
///
/// Taking hashes rather than keys lets the table builder hash each key once, as
/// it is added, without retaining the key bytes. Returns an empty vector when
/// `bits_per_key` is zero (filters disabled).
pub fn build_from_hashes(hashes: &[u32], bits_per_key: usize) -> Vec<u8> {
    if bits_per_key == 0 {
        return Vec::new();
    }
    let k = probe_count(bits_per_key);
    let want_bits = hashes.len().saturating_mul(bits_per_key).max(MIN_BITS);
    let nbytes = want_bits.div_ceil(8);
    let bits = nbytes * 8;

    // One trailing byte records k so the reader uses the same probe count.
    let mut filter = vec![0u8; nbytes + 1];
    for &base in hashes {
        let mut h = base;
        let delta = h.rotate_right(17);
        for _ in 0..k {
            let bitpos = (h as usize) % bits;
            filter[bitpos / 8] |= 1 << (bitpos % 8);
            h = h.wrapping_add(delta);
        }
    }
    filter[nbytes] = k as u8;
    filter
}

/// Convenience wrapper: builds a filter directly from keys.
pub fn build_from_keys<K: AsRef<[u8]>>(keys: &[K], bits_per_key: usize) -> Vec<u8> {
    let hashes: Vec<u32> = keys.iter().map(|k| hash_key(k.as_ref())).collect();
    build_from_hashes(&hashes, bits_per_key)
}

/// Tests whether `key` might be present according to `filter`.
///
/// Returns `false` only if the key is *definitely* absent. A malformed or
/// too-small filter conservatively returns `true`.
pub fn may_contain(filter: &[u8], key: &[u8]) -> bool {
    may_contain_hash(filter, hash_key(key))
}

/// Like [`may_contain`] but takes a precomputed [`hash_key`] value.
pub fn may_contain_hash(filter: &[u8], base_hash: u32) -> bool {
    // Need at least one data byte plus the trailing k byte.
    if filter.len() < 2 {
        return true;
    }
    let k = filter[filter.len() - 1] as usize;
    if k > MAX_PROBES {
        // Unknown encoding — do not risk a false negative.
        return true;
    }
    let bits = (filter.len() - 1) * 8;
    let mut h = base_hash;
    let delta = h.rotate_right(17);
    for _ in 0..k {
        let bitpos = (h as usize) % bits;
        if filter[bitpos / 8] & (1 << (bitpos % 8)) == 0 {
            return false;
        }
        h = h.wrapping_add(delta);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_false_negatives() {
        let keys: Vec<Vec<u8>> = (0..1000u32).map(|i| i.to_le_bytes().to_vec()).collect();
        let filter = build_from_keys(&keys, 10);
        for k in &keys {
            assert!(may_contain(&filter, k), "false negative for {k:?}");
        }
    }

    #[test]
    fn false_positive_rate_is_low() {
        let keys: Vec<Vec<u8>> = (0..1000u32).map(|i| i.to_le_bytes().to_vec()).collect();
        let filter = build_from_keys(&keys, 10);
        let mut fp = 0;
        for i in 1000..11000u32 {
            if may_contain(&filter, &i.to_le_bytes()) {
                fp += 1;
            }
        }
        // ~1% expected at 10 bits/key; allow generous headroom for variance.
        assert!(fp < 500, "false positive rate too high: {fp}/10000");
    }

    #[test]
    fn disabled_when_zero_bits() {
        assert!(build_from_keys(&[b"x".to_vec()], 0).is_empty());
    }

    #[test]
    fn malformed_filter_is_conservative() {
        assert!(may_contain(&[], b"key"));
        assert!(may_contain(&[0x00], b"key"));
        // Trailing byte claims 200 probes — unknown; must not reject.
        assert!(may_contain(&[0x00, 0x00, 200], b"key"));
    }

    #[test]
    fn empty_filter_rejects_everything() {
        // A filter built over no keys has all bits clear; lookups are negative.
        let filter = build_from_hashes(&[], 10);
        assert!(!may_contain(&filter, b"anything"));
    }
}
