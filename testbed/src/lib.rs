//! testbed — RDOSCli Phase 0 supervision-loop test crate.
//!
//! `parse_ranges` parses a comma-separated spec of numbers and inclusive
//! ranges, e.g. `"1-3,7,10-12"` → `[1, 2, 3, 7, 10, 11, 12]`.
//! `format_ranges` is the inverse: `[1, 2, 3, 7]` → `"1-3,7"`.

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    /// Input was empty or only whitespace.
    Empty,
    /// A segment contained something that is not a non-negative integer.
    InvalidNumber(String),
    /// A range was malformed (missing endpoint, or end < start).
    InvalidRange(String),
}

/// Parse a spec like `"1-3,7,10-12"` into the full list of numbers,
/// expanding each inclusive range. Whitespace around segments is allowed.
pub fn parse_ranges(input: &str) -> Result<Vec<usize>, ParseError> {
    if input.trim().is_empty() {
        return Err(ParseError::Empty);
    }
    let mut out = Vec::new();
    for seg in input.split(',') {
        let seg = seg.trim();
        if input.contains('-') {
            let Some((lo, hi)) = seg.split_once('-') else {
                return Err(ParseError::InvalidRange(seg.to_string()));
            };
            let lo = parse_num(lo)?;
            let hi = parse_num(hi)?;
            if hi < lo {
                return Err(ParseError::InvalidRange(seg.to_string()));
            }
            for n in lo..hi {
                out.push(n);
            }
        } else {
            out.push(parse_num(seg)?);
        }
    }
    Ok(out)
}

fn parse_num(s: &str) -> Result<usize, ParseError> {
    s.trim()
        .parse()
        .map_err(|_| ParseError::InvalidNumber(s.trim().to_string()))
}

/// Inverse of `parse_ranges`: compress a sorted, de-duplicated slice into
/// the shortest spec string, e.g. `[1, 2, 3, 7, 10, 11, 12]` → `"1-3,7,10-12"`.
/// Runs of length 1 render as a single number; runs of length ≥ 2 render as
/// `lo-hi`. An empty slice renders as the empty string.
pub fn format_ranges(_nums: &[usize]) -> String {
    todo!("implement for Phase 0 task T3")
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- 基线（4 个，必须始终通过；防 agent 把好的改坏）--

    #[test]
    fn parses_single_number() {
        assert_eq!(parse_ranges("7"), Ok(vec![7]));
    }

    #[test]
    fn parses_list_of_singles() {
        assert_eq!(parse_ranges("3, 5, 9"), Ok(vec![3, 5, 9]));
    }

    #[test]
    fn rejects_empty_input() {
        assert_eq!(parse_ranges("   "), Err(ParseError::Empty));
    }

    #[test]
    fn rejects_non_numeric() {
        assert_eq!(
            parse_ranges("a-b"),
            Err(ParseError::InvalidNumber("a".to_string()))
        );
    }

    // -- 浅 bug 目标（T2）--

    #[test]
    fn expands_inclusive_range() {
        assert_eq!(parse_ranges("10-12"), Ok(vec![10, 11, 12]));
    }

    // -- 深 bug 目标（T3）--

    #[test]
    fn parses_mixed_singles_and_ranges() {
        assert_eq!(
            parse_ranges("1-3,7,10-12"),
            Ok(vec![1, 2, 3, 7, 10, 11, 12])
        );
    }

    // -- 未实现目标（T3；cargo test -- --include-ignored 才运行）--

    #[test]
    #[ignore = "format_ranges not implemented yet"]
    fn compresses_sorted_numbers() {
        assert_eq!(format_ranges(&[1, 2, 3, 7, 10, 11, 12]), "1-3,7,10-12");
        assert_eq!(format_ranges(&[5]), "5");
        assert_eq!(format_ranges(&[]), "");
    }
}
