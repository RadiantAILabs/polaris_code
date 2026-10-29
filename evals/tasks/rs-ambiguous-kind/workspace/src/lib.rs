/// Parses a comma-separated list of integers, silently skipping anything that
/// fails to parse.
pub fn parse_all(input: &str) -> Vec<i32> {
    input
        .split(',')
        .filter_map(|part| part.trim().parse::<i32>().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_numbers() {
        assert_eq!(parse_all("1, 2, 3"), vec![1, 2, 3]);
    }
}
