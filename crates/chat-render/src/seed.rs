//! The `seed` template filter: a stable number from any value, so themes
//! can give every chatter their own look (colour, shape, ...) from
//! `author.id`:
//!
//! ```jinja
//! style="--hue: {{ author.id | seed(360, 'hue') }}"
//! ```
//!
//! The numbers are a promise to themes: the same value and salt must give
//! the same number on every machine, after every restart and update.
//! Changing the algorithm would give every chatter a new look, so it's
//! written out here (FNV-1a, then MurmurHash3's finalizer) rather than
//! taken from Rust's standard hasher, which uses a random key per process
//! (against hash-flooding attacks) and may change between Rust versions.
//! The pinned test below guards it.

use minijinja::{Error, ErrorKind, Value};

/// `value | seed(n, salt="")`: a number from 0 to n-1. A different `salt`
/// gives an independent number for the same value, e.g. one for the colour
/// and one for the shape.
pub(crate) fn filter(value: &Value, n: u32, salt: Option<&str>) -> Result<u32, Error> {
    if n == 0 {
        return Err(Error::new(
            ErrorKind::InvalidOperation,
            "seed(n) needs n of at least 1",
        ));
    }
    // A string value's text is the string itself (no quotes); numbers and
    // other values hash their printed form.
    let value = value.to_string();
    let hash = fnv1a([
        salt.unwrap_or("").as_bytes(),
        &[SEPARATOR],
        value.as_bytes(),
    ]);
    Ok(reduce(fmix64(hash), n))
}

/// Between salt and value, so salt "ab" + value "c" and salt "a" + value
/// "bc" don't hash alike. 0xFF never occurs in UTF-8 text.
const SEPARATOR: u8 = 0xFF;

/// 64-bit FNV-1a over the concatenated parts (http://www.isthe.com/chongo/tech/comp/fnv/).
fn fnv1a<const N: usize>(parts: [&[u8]; N]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for byte in parts.into_iter().flatten() {
        hash ^= u64::from(*byte);
        // Overflow is part of the algorithm: wrap instead of panicking.
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// MurmurHash3's 64-bit finalizer: stirs every input bit into every output
/// bit. FNV-1a alone mixes poorly: the last bytes barely reach the top
/// bits, the lowest bit only depends on the lowest bit of each byte, so IDs
/// that differ only at the end (most of them) would bunch up on a few
/// values. The constants are MurmurHash3's published ones.
fn fmix64(mut hash: u64) -> u64 {
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xff51_afd7_ed55_8ccd);
    hash ^= hash >> 33;
    hash = hash.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    hash ^= hash >> 33;
    hash
}

/// Maps the hash to 0..n: multiplying its top 32 bits by n and keeping the
/// upper half spreads them evenly over 0..n (Lemire's "fast range"). Unlike
/// `hash % n` it needs no division, and it can't overflow: both factors are
/// below 2^32.
fn reduce(hash: u64, n: u32) -> u32 {
    let top = hash >> 32;
    ((top * u64::from(n)) >> 32) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed(value: &str, n: u32, salt: Option<&str>) -> u32 {
        filter(&Value::from(value), n, salt).unwrap()
    }

    #[test]
    fn hash_is_the_standard_fnv1a() {
        // Published FNV-1a 64-bit test vectors.
        assert_eq!(fnv1a([b""]), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a([b"a"]), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a([b"foobar"]), 0x8594_4171_f739_67e8);
        // Parts are simply concatenated.
        assert_eq!(fnv1a([b"foo", b"bar"]), fnv1a([b"foobar"]));
    }

    /// Themes rely on these staying the same forever: if this fails, every
    /// chatter's look would change. Don't update the numbers, fix the code.
    #[test]
    fn results_never_change() {
        assert_eq!(
            seed("UCExampleChannelId000001", 360, Some("hue")),
            PINNED[0]
        );
        assert_eq!(
            seed("UCExampleChannelId000001", 6, Some("shape")),
            PINNED[1]
        );
        assert_eq!(seed("123456789", 360, None), PINNED[2]);
    }
    // Computed independently (a Python version of the same steps).
    const PINNED: [u32; 3] = [190, 1, 159];

    #[test]
    fn hash_mixes_all_bits() {
        // Published MurmurHash3 fmix64 behaviour: 0 stays 0, and one flipped
        // input bit changes about half of the output bits.
        assert_eq!(fmix64(0), 0);
        let changed = (fmix64(1) ^ fmix64(0)).count_ones();
        assert!((20..=44).contains(&changed), "{changed} bits changed");
    }

    /// IDs that differ only in their last characters, like real ones, must
    /// still spread evenly (6000 draws over 6 values: 1000 each on average).
    #[test]
    fn results_stay_in_range_and_spread_evenly() {
        let mut counts = [0; 6];
        for i in 0..6000 {
            let n = seed(&format!("user-{i}"), 6, None);
            assert!(n < 6);
            counts[n as usize] += 1;
        }
        assert!(
            counts.iter().all(|c| (850..=1150).contains(c)),
            "{counts:?}"
        );
    }

    #[test]
    fn salts_give_independent_numbers() {
        let differ = (0..100)
            .filter(|i| {
                let id = format!("user-{i}");
                seed(&id, 1000, Some("hue")) != seed(&id, 1000, Some("shape"))
            })
            .count();
        assert!(differ > 95, "only {differ} of 100 differ");
    }

    #[test]
    fn salt_and_value_are_kept_apart() {
        assert_ne!(
            seed("c", u32::MAX, Some("ab")),
            seed("bc", u32::MAX, Some("a"))
        );
    }

    #[test]
    fn zero_is_refused() {
        assert!(filter(&Value::from("x"), 0, None).is_err());
    }
}
