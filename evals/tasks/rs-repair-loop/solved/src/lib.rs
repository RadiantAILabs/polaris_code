/// Clamps `x` into the inclusive range `[lo, hi]`.
pub fn clamp(x: i32, lo: i32, hi: i32) -> i32 {
    x.max(lo).min(hi)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_within_bounds() {
        assert_eq!(clamp(5, 0, 10), 5);
    }

    #[test]
    fn clamps_above_high_bound() {
        assert_eq!(clamp(15, 0, 10), 10);
    }

    #[test]
    fn clamps_below_low_bound() {
        assert_eq!(clamp(-5, 0, 10), 0);
    }
}
