/// Returns true when `n` is even.
pub fn is_even(n: i32) -> bool {
    n % 2 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn even_number_is_even() {
        assert!(is_even(4));
    }

    #[test]
    fn odd_number_is_not_even() {
        assert!(!is_even(3));
    }
}
