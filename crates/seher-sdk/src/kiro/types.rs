/// Parsed output from `kiro-cli chat --no-interactive "/usage"`.
///
/// Kiro reports usage as a simple text block that is parsed into structured
/// data.  The CLI provider does not use HTTP; status is obtained by running
/// a subprocess and parsing its stdout.
#[derive(Debug)]
pub struct KiroUsageInfo {
    /// Maximum requests allowed in the current window.
    pub limit: i64,
    /// Requests consumed so far.
    pub used: i64,
    /// Seconds remaining until the window resets, if applicable.
    pub reset_in_seconds: Option<i64>,
}

impl KiroUsageInfo {
    /// Returns `true` when usage has reached or exceeded the limit.
    #[must_use]
    pub fn is_limited(&self) -> bool {
        self.used >= self.limit
    }

    /// Returns usage as a percentage (0–100). Returns 100.0 when limit is
    /// zero or negative.
    #[must_use]
    #[expect(clippy::cast_precision_loss)]
    pub fn utilization(&self) -> f64 {
        if self.limit > 0 {
            self.used as f64 / self.limit as f64 * 100.0
        } else {
            100.0
        }
    }

    /// Parse usage output from the kiro CLI.
    ///
    /// Expected format:
    /// ```text
    /// Usage: 42/100 requests
    /// Resets in: 3600 seconds
    /// ```
    ///
    /// The "Resets in" line is optional.
    ///
    /// # Errors
    ///
    /// Returns an error if the output cannot be parsed.
    pub fn parse(output: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let output = output.trim();
        if output.is_empty() {
            return Err("empty kiro output".into());
        }

        let mut limit: Option<i64> = None;
        let mut used: Option<i64> = None;
        let mut reset_in_seconds: Option<i64> = None;

        for line in output.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("Usage:") {
                // Format: "Usage: 42/100 requests"
                let rest = rest.trim();
                let rest = rest.strip_suffix("requests").unwrap_or(rest).trim();
                let parts: Vec<&str> = rest.split('/').collect();
                if parts.len() != 2 {
                    return Err(format!("malformed usage line: {line}").into());
                }
                used = Some(parts[0].parse::<i64>()?);
                limit = Some(parts[1].parse::<i64>()?);
            } else if let Some(rest) = line.strip_prefix("Resets in:") {
                // Format: "Resets in: 3600 seconds"
                let rest = rest.trim();
                let rest = rest.strip_suffix("seconds").unwrap_or(rest).trim();
                reset_in_seconds = Some(rest.parse::<i64>()?);
            }
        }

        let limit = limit.ok_or("missing usage line in kiro output")?;
        let used = used.ok_or("missing usage line in kiro output")?;

        Ok(Self {
            limit,
            used,
            reset_in_seconds,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    // =======================================================================
    // Struct construction & predicates (no parsing)
    // =======================================================================

    #[test]
    fn test_is_not_limited_when_below_limit() {
        let info = KiroUsageInfo {
            limit: 100,
            used: 42,
            reset_in_seconds: Some(3600),
        };
        assert!(!info.is_limited());
    }

    #[test]
    fn test_is_limited_when_used_equals_limit() {
        let info = KiroUsageInfo {
            limit: 100,
            used: 100,
            reset_in_seconds: None,
        };
        assert!(info.is_limited());
    }

    #[test]
    fn test_is_limited_when_used_exceeds_limit() {
        let info = KiroUsageInfo {
            limit: 50,
            used: 60,
            reset_in_seconds: None,
        };
        assert!(info.is_limited());
    }

    #[test]
    fn test_utilization_computed_correctly() {
        let info = KiroUsageInfo {
            limit: 200,
            used: 50,
            reset_in_seconds: None,
        };
        assert!((info.utilization() - 25.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_utilization_returns_100_when_zero_limit() {
        let info = KiroUsageInfo {
            limit: 0,
            used: 0,
            reset_in_seconds: None,
        };
        assert!((info.utilization() - 100.0).abs() < f64::EPSILON);
    }

    // =======================================================================
    // Parsing tests
    // =======================================================================

    #[test]
    fn test_parse_full_output() -> TestResult {
        let output = "Usage: 42/100 requests\nResets in: 3600 seconds\n";

        let info = KiroUsageInfo::parse(output)?;
        assert_eq!(info.limit, 100);
        assert_eq!(info.used, 42);
        assert_eq!(info.reset_in_seconds, Some(3600));
        Ok(())
    }

    #[test]
    fn test_parse_output_without_reset() -> TestResult {
        let output = "Usage: 10/50 requests\n";

        let info = KiroUsageInfo::parse(output)?;
        assert_eq!(info.limit, 50);
        assert_eq!(info.used, 10);
        assert!(info.reset_in_seconds.is_none());
        Ok(())
    }

    #[test]
    fn test_parse_output_at_limit() -> TestResult {
        let output = "Usage: 100/100 requests\nResets in: 0 seconds\n";

        let info = KiroUsageInfo::parse(output)?;
        assert!(info.is_limited());
        Ok(())
    }

    #[test]
    fn test_parse_rejects_malformed_output() {
        let output = "this is not valid kiro output";

        let result = KiroUsageInfo::parse(output);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_rejects_empty_output() {
        let result = KiroUsageInfo::parse("");
        assert!(result.is_err());
    }
}
