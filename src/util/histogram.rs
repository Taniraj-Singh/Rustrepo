//! A compact streaming histogram for latency and size distributions.
//!
//! [`Histogram`] records values into exponentially-spaced buckets and answers
//! summary questions — count, sum, min, max, mean, and estimated percentiles —
//! without retaining individual samples. It is used to characterize operation
//! costs (e.g. value sizes, block sizes) cheaply and with bounded memory.
//!
//! Buckets follow a base-`√2` progression so a modest bucket count spans a wide
//! dynamic range with a bounded relative error. Percentiles are interpolated
//! within the bucket that contains the target rank.

/// Number of exponential buckets. Bucket `i` covers roughly
/// `[2^(i/2), 2^((i+1)/2))`, so 64 buckets reach ~4 billion.
const BUCKET_COUNT: usize = 64;

/// Upper bound (exclusive) of each bucket, precomputed once.
fn bucket_bound(i: usize) -> f64 {
    // 2^((i+1)/2): every two buckets multiply the bound by 2.
    2f64.powf((i as f64 + 1.0) / 2.0)
}

/// Finds the bucket index a value falls into.
fn bucket_for(value: f64) -> usize {
    if value < 1.0 {
        return 0;
    }
    // index such that bucket_bound(index-1) <= value < bucket_bound(index)
    let idx = (value.log2() * 2.0).floor() as usize;
    idx.min(BUCKET_COUNT - 1)
}

/// A fixed-memory streaming histogram over non-negative values.
#[derive(Debug, Clone)]
pub struct Histogram {
    buckets: [u64; BUCKET_COUNT],
    count: u64,
    sum: f64,
    min: f64,
    max: f64,
}

impl Default for Histogram {
    fn default() -> Histogram {
        Histogram::new()
    }
}

impl Histogram {
    /// Creates an empty histogram.
    pub fn new() -> Histogram {
        Histogram {
            buckets: [0; BUCKET_COUNT],
            count: 0,
            sum: 0.0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        }
    }

    /// Records one observation. Negative values are clamped to zero.
    pub fn record(&mut self, value: f64) {
        let value = if value.is_finite() && value > 0.0 {
            value
        } else {
            0.0
        };
        let idx = bucket_for(value);
        self.buckets[idx] += 1;
        self.count += 1;
        self.sum += value;
        self.min = self.min.min(value);
        self.max = self.max.max(value);
    }

    /// Records an integer observation.
    pub fn record_u64(&mut self, value: u64) {
        self.record(value as f64);
    }

    /// Number of recorded observations.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Whether nothing has been recorded.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Sum of all observations.
    pub fn sum(&self) -> f64 {
        self.sum
    }

    /// Smallest observation, or `0.0` if empty.
    pub fn min(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.min
        }
    }

    /// Largest observation, or `0.0` if empty.
    pub fn max(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.max
        }
    }

    /// Arithmetic mean, or `0.0` if empty.
    pub fn mean(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.sum / self.count as f64
        }
    }

    /// Estimated value at the given percentile in `[0, 100]`.
    ///
    /// The result is interpolated within the bucket that holds the target rank,
    /// then clamped to the observed `[min, max]` range.
    pub fn percentile(&self, p: f64) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        let target = (p.clamp(0.0, 100.0) / 100.0) * self.count as f64;
        let mut cumulative = 0.0;
        for (i, &n) in self.buckets.iter().enumerate() {
            cumulative += n as f64;
            if cumulative >= target {
                let lower = if i == 0 { 0.0 } else { bucket_bound(i - 1) };
                let upper = bucket_bound(i);
                // Linear interpolation within the bucket.
                let bucket_start = cumulative - n as f64;
                let frac = if n == 0 {
                    0.0
                } else {
                    (target - bucket_start) / n as f64
                };
                let estimate = lower + (upper - lower) * frac;
                return estimate.clamp(self.min, self.max);
            }
        }
        self.max
    }

    /// The median (50th percentile).
    pub fn median(&self) -> f64 {
        self.percentile(50.0)
    }

    /// Folds another histogram's observations into this one.
    pub fn merge(&mut self, other: &Histogram) {
        for i in 0..BUCKET_COUNT {
            self.buckets[i] += other.buckets[i];
        }
        self.count += other.count;
        self.sum += other.sum;
        if other.count > 0 {
            self.min = self.min.min(other.min);
            self.max = self.max.max(other.max);
        }
    }

    /// Resets the histogram to empty.
    pub fn clear(&mut self) {
        *self = Histogram::new();
    }

    /// Renders a one-line summary of the distribution.
    pub fn summary(&self) -> String {
        format!(
            "count={} min={:.1} p50={:.1} p90={:.1} p99={:.1} max={:.1} mean={:.1}",
            self.count,
            self.min(),
            self.median(),
            self.percentile(90.0),
            self.percentile(99.0),
            self.max(),
            self.mean(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_histogram_is_well_defined() {
        let h = Histogram::new();
        assert!(h.is_empty());
        assert_eq!(h.count(), 0);
        assert_eq!(h.mean(), 0.0);
        assert_eq!(h.median(), 0.0);
        assert_eq!(h.min(), 0.0);
        assert_eq!(h.max(), 0.0);
    }

    #[test]
    fn basic_statistics() {
        let mut h = Histogram::new();
        for v in 1..=100u64 {
            h.record_u64(v);
        }
        assert_eq!(h.count(), 100);
        assert_eq!(h.min(), 1.0);
        assert_eq!(h.max(), 100.0);
        // Mean of 1..=100 is 50.5.
        assert!((h.mean() - 50.5).abs() < 0.001);
    }

    #[test]
    fn percentiles_are_ordered_and_bounded() {
        let mut h = Histogram::new();
        for v in 1..=1000u64 {
            h.record_u64(v);
        }
        let p10 = h.percentile(10.0);
        let p50 = h.percentile(50.0);
        let p90 = h.percentile(90.0);
        assert!(p10 <= p50 && p50 <= p90);
        assert!(p10 >= h.min() && p90 <= h.max());
        // The median of a uniform 1..=1000 should be near 500.
        assert!((p50 - 500.0).abs() < 200.0);
    }

    #[test]
    fn merge_combines_distributions() {
        let mut a = Histogram::new();
        let mut b = Histogram::new();
        for v in 1..=50u64 {
            a.record_u64(v);
        }
        for v in 51..=100u64 {
            b.record_u64(v);
        }
        a.merge(&b);
        assert_eq!(a.count(), 100);
        assert_eq!(a.min(), 1.0);
        assert_eq!(a.max(), 100.0);
    }

    #[test]
    fn handles_extreme_and_invalid_values() {
        let mut h = Histogram::new();
        h.record(-5.0); // clamped to 0
        h.record(f64::NAN); // clamped to 0
        h.record(f64::INFINITY); // clamped to 0
        h.record(1e9);
        assert_eq!(h.count(), 4);
        assert!(h.max() >= 1e9 - 1.0);
        assert!(!h.summary().is_empty());
    }

    #[test]
    fn bucket_monotonic() {
        // Larger values never land in an earlier bucket.
        let mut last = 0;
        for exp in 0..30 {
            let v = (1u64 << exp) as f64;
            let b = bucket_for(v);
            assert!(b >= last);
            last = b;
        }
    }
}
