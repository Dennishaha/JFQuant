//! Stable lowercase hexadecimal rendering, independent of digest library output types.
use std::fmt;

/// Render every byte as exactly two lowercase hex digits, retaining leading zeros.
pub struct Hex<T: AsRef<[u8]>>(pub T);
impl<T: AsRef<[u8]>> fmt::LowerHex for Hex<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0.as_ref() {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lowercase_hex_preserves_leading_zero_bytes() {
        assert_eq!(
            format!("sha256:{:x}", Hex([0, 1, 15, 16, 255])),
            "sha256:00010f10ff"
        );
        assert_eq!(format!("{:x}", Hex([])), "");
    }
}
