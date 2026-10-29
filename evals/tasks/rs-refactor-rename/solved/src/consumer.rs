use crate::area_of_rectangle;

/// Sum of the areas of every `(width, height)` pair.
pub fn total_area(pairs: &[(f64, f64)]) -> f64 {
    pairs.iter().map(|(w, h)| area_of_rectangle(*w, *h)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_two_rectangles() {
        assert_eq!(total_area(&[(2.0, 3.0), (4.0, 5.0)]), 26.0);
    }
}
