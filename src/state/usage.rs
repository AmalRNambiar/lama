//! What a session is using up: how full its context window is and what it
//! has cost. Measured inside the provider boundary (`provider::usage`, which
//! reads transcripts natively) and handed to the plane as plain numbers, so
//! nothing here knows a format or touches the disk.

/// One session's context and spend, as of its latest refresh.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UsageSummary {
    /// Tokens in the context window on the latest model call.
    pub context_used: u64,
    /// The window's size, when the transcript states it or the model implies it.
    pub context_window: Option<u64>,
    /// Lifetime cost in USD; `None` while any call ran on an unpriced model,
    /// since a partial sum would read as the whole bill.
    pub cost_usd: Option<f64>,
    /// Cost of calls made on today's local date; same `None` rule.
    pub cost_today_usd: Option<f64>,
}

impl UsageSummary {
    /// How full the context window is, 0.0–1.0, when its size is known.
    pub fn fill(&self) -> Option<f64> {
        let window = self.context_window.filter(|&w| w > 0)?;
        Some((self.context_used as f64 / window as f64).clamp(0.0, 1.0))
    }

    /// `(used, window)` for a card's gauge, when the window is known.
    pub fn context(&self) -> Option<(u64, u64)> {
        Some((self.context_used, self.context_window?))
    }
}

/// Sum optional costs: known if any part is known. Unpriced sessions are
/// already `None`, so this adds up what can be priced rather than hiding a
/// project's total behind one Codex session.
pub fn sum_costs(costs: impl IntoIterator<Item = Option<f64>>) -> Option<f64> {
    costs.into_iter().flatten().reduce(|a, b| a + b)
}

/// A dollar amount for a gauge: cents below $100, whole dollars above.
pub fn fmt_usd(usd: f64) -> String {
    if usd < 100.0 {
        format!("${usd:.2}")
    } else {
        format!("${usd:.0}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_needs_a_window() {
        let mut u = UsageSummary {
            context_used: 50_000,
            ..Default::default()
        };
        assert_eq!(u.fill(), None);
        u.context_window = Some(200_000);
        assert_eq!(u.fill(), Some(0.25));
        assert_eq!(u.context(), Some((50_000, 200_000)));
    }

    #[test]
    fn costs_sum_what_is_known() {
        assert_eq!(sum_costs([None, None]), None);
        assert_eq!(sum_costs([Some(1.0), None, Some(0.5)]), Some(1.5));
        assert_eq!(fmt_usd(4.1), "$4.10");
        assert_eq!(fmt_usd(123.4), "$123");
    }
}
