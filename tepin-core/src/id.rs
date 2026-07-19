//! Short, sortable document ids: 8 base36 chars of unix milliseconds
//! (zero-padded, so lexicographic order is creation order — good until
//! year ~5188) + 4 random base36 chars from the OS RNG. 12 lowercase
//! chars total. Collisions inside one millisecond are possible in bulk
//! inserts, so the insert path verifies and retries; ids are never
//! trusted to be unique by construction alone.

use std::time::{SystemTime, UNIX_EPOCH};

const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
const TIME_LEN: usize = 8;
const RAND_LEN: usize = 4;

pub fn generate() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut out = [b'0'; TIME_LEN + RAND_LEN];
    let mut t = millis;
    for slot in out[..TIME_LEN].iter_mut().rev() {
        *slot = ALPHABET[(t % 36) as usize];
        t /= 36;
    }
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("OS RNG");
    let mut rnd = u64::from_le_bytes(bytes);
    for slot in out[TIME_LEN..].iter_mut() {
        *slot = ALPHABET[(rnd % 36) as usize];
        rnd /= 36;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_short_lowercase_and_sortable() {
        let a = generate();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = generate();
        assert_eq!(a.len(), 12);
        assert!(a.bytes().all(|c| ALPHABET.contains(&c)));
        assert!(a < b, "{a} should sort before {b}");
    }
}
