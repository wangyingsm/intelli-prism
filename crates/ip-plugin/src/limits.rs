use std::time::Duration;

/// What one plugin call may spend before it is stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginLimits {
    /// Wasm instructions one call may run, counted as fuel.
    pub fuel: u64,
    /// Wall time one call may take.
    pub deadline: Duration,
    /// How often the clock that measures the deadline ticks.
    pub tick: Duration,
    /// Largest linear memory a module may grow to, in bytes.
    pub memory_bytes: usize,
}

impl PluginLimits {
    /// The deadline as a count of clock ticks, rounded up and never zero.
    pub fn deadline_ticks(&self) -> u64 {
        let tick = self.tick.as_nanos().max(1);
        let ticks = self.deadline.as_nanos().div_ceil(tick).max(1);
        u64::try_from(ticks).unwrap_or(u64::MAX)
    }
}

impl Default for PluginLimits {
    fn default() -> Self {
        Self {
            fuel: 100_000_000,
            deadline: Duration::from_millis(100),
            tick: Duration::from_millis(10),
            memory_bytes: 64 * 1024 * 1024,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_deadline_is_counted_in_whole_ticks() {
        let limits = PluginLimits::default();
        assert_eq!(limits.deadline_ticks(), 10);
    }

    #[test]
    fn a_deadline_shorter_than_a_tick_still_allows_one_tick() {
        let limits = PluginLimits {
            deadline: Duration::from_millis(1),
            ..PluginLimits::default()
        };
        assert_eq!(limits.deadline_ticks(), 1);
    }

    #[test]
    fn a_deadline_between_ticks_rounds_up() {
        let limits = PluginLimits {
            deadline: Duration::from_millis(25),
            ..PluginLimits::default()
        };
        assert_eq!(limits.deadline_ticks(), 3);
    }
}
