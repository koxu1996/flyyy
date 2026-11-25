//! Randomness from the system's generator (ring), for ids, tokens and address picks.
use ring::rand::{SecureRandom, SystemRandom};

/// `n` random bytes; fails rather than hand back anything predictable.
pub(crate) fn bytes(n: usize) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; n];
    SystemRandom::new().fill(&mut buf).map_err(|_| "the system's random generator failed".to_owned())?;
    Ok(buf)
}

/// `n` random bytes as lowercase hex (2n characters).
pub(crate) fn hex(n: usize) -> Result<String, String> {
    Ok(bytes(n)?.iter().map(|b| format!("{b:02x}")).collect())
}

/// A random u64, e.g. for picking among candidates (`% len`).
pub(crate) fn u64() -> Result<u64, String> {
    let b: [u8; 8] = bytes(8)?.try_into().expect("8 bytes");
    Ok(u64::from_le_bytes(b))
}

#[cfg(test)]
mod tests {
    #[test]
    fn hex_is_random_and_sized() {
        let (a, b) = (super::hex(16).unwrap(), super::hex(16).unwrap());
        assert!(a.len() == 32 && a.bytes().all(|c| c.is_ascii_hexdigit()) && a != b);
    }
}
