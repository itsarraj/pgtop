//! Small pure formatters. Durations and query text are the two things this
//! tool prints over and over, and both have a failure mode worth pinning
//! down in tests: a duration that reads as `4512.0s` instead of `1h15m`, and
//! a multi-line query that turns one table row into forty.

/// Human duration from a float count of seconds.
///
/// Under a minute keeps one decimal (the resolution that matters when you
/// are deciding whether a query is *actually* slow); above that it switches
/// to `3m04s` / `2h05m`, which is far easier to eyeball than four or five
/// digits of seconds. Negative input — possible for a hair's breadth if the
/// server's `now()` lands before a `query_start` recorded microseconds
/// later — is clamped rather than printed as `-0.0s`.
pub fn duration(secs: f64) -> String {
    let s = if secs.is_finite() && secs > 0.0 {
        secs
    } else {
        0.0
    };
    if s < 60.0 {
        return format!("{s:.1}s");
    }
    let total = s as u64;
    if total < 3600 {
        return format!("{}m{:02}s", total / 60, total % 60);
    }
    format!("{}h{:02}m", total / 3600, (total % 3600) / 60)
}

/// Collapse a query to a single line: runs of whitespace (including the
/// newlines and indentation an ORM loves to emit) become one space.
pub fn one_line(query: &str) -> String {
    query.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncate to `width` *characters* (not bytes — a query with an emoji or a
/// non-ASCII literal in it must not panic on a byte-slice boundary), with
/// the last three characters given over to `...` when something was cut.
pub fn truncate(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if s.chars().count() <= width {
        return s.to_string();
    }
    if width <= 3 {
        return ".".repeat(width);
    }
    let kept: String = s.chars().take(width - 3).collect();
    format!("{kept}...")
}

/// The two together: what actually goes in a `QUERY` cell.
pub fn query_cell(query: &str, width: usize) -> String {
    let flat = one_line(query);
    if flat.is_empty() {
        return "-".to_string();
    }
    truncate(&flat, width)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_minute_durations_keep_one_decimal() {
        assert_eq!(duration(0.0), "0.0s");
        assert_eq!(duration(0.42), "0.4s");
        assert_eq!(duration(12.44), "12.4s");
        assert_eq!(duration(59.94), "59.9s");
    }

    #[test]
    fn minutes_and_hours_switch_units() {
        assert_eq!(duration(60.0), "1m00s");
        assert_eq!(duration(184.6), "3m04s");
        assert_eq!(duration(3599.0), "59m59s");
        assert_eq!(duration(3600.0), "1h00m");
        assert_eq!(duration(4512.0), "1h15m");
    }

    #[test]
    fn negative_and_nonfinite_durations_clamp_instead_of_printing_garbage() {
        assert_eq!(duration(-0.001), "0.0s");
        assert_eq!(duration(f64::NAN), "0.0s");
        assert_eq!(duration(f64::INFINITY), "0.0s");
    }

    #[test]
    fn multi_line_queries_collapse_to_one_line() {
        let q = "SELECT *\n  FROM accounts\n  WHERE id = 1\n";
        assert_eq!(one_line(q), "SELECT * FROM accounts WHERE id = 1");
        assert!(!one_line(q).contains('\n'), "a row must stay one row");
    }

    #[test]
    fn truncation_is_char_safe_and_marks_the_cut() {
        assert_eq!(truncate("select 1", 20), "select 1");
        assert_eq!(truncate("select * from accounts", 10), "select ...");
        assert_eq!(truncate("select ✓ from accounts", 10).chars().count(), 10);
        assert_eq!(truncate("abcdef", 2), "..");
        assert_eq!(truncate("abcdef", 0), "");
    }

    #[test]
    fn an_empty_query_renders_as_a_dash_not_blank() {
        assert_eq!(query_cell("", 40), "-");
        assert_eq!(query_cell("   \n  ", 40), "-");
    }

    #[test]
    fn query_cell_flattens_then_truncates() {
        let q = "UPDATE accounts\n   SET balance = balance - 10\n WHERE id = 1";
        assert_eq!(query_cell(q, 24), "UPDATE accounts SET b...");
        assert_eq!(query_cell(q, 24).chars().count(), 24);
    }
}
