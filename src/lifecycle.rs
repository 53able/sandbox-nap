//! 降格の判断(純粋関数)。

use crate::config::Config;
use crate::model::Tier;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// しきい値に達していない、または降格対象の段階ではない。
    NotDue,
    /// しきい値に達しているが、リースがあるので見送る。
    SkipLease,
    Stop,
    Archive,
}

/// 段階・最終アクセス・現在時刻・リース数から次の降格を決める。
///
/// - hot: アイドルが `idle_stop_secs` 以上なら Stop
/// - stopped: アイドルが `idle_archive_secs` 以上なら Archive
/// - しきい値に達していてもリースが 1 以上なら SkipLease
pub fn next_action(tier: Tier, last_access_ms: i64, now_ms: i64, lease_count: usize, cfg: &Config) -> Action {
    let idle_ms = now_ms.saturating_sub(last_access_ms).max(0) as u128;
    let due = |secs: u64| idle_ms >= u128::from(secs) * 1000;
    let candidate = match tier {
        Tier::Hot if due(cfg.idle_stop_secs) => Action::Stop,
        Tier::Stopped if due(cfg.idle_archive_secs) => Action::Archive,
        _ => Action::NotDue,
    };
    if candidate != Action::NotDue && lease_count > 0 {
        Action::SkipLease
    } else {
        candidate
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn cfg() -> Config {
        Config {
            idle_stop_secs: 300,
            idle_archive_secs: 86_400,
            tick_secs: 5,
            stop_timeout_secs: None,
            max_hot_memory_mb: 1024,
            socket_path: PathBuf::from("/tmp/x.sock"),
            db_path: PathBuf::from("/tmp/x.db"),
        }
    }

    const T0: i64 = 1_000_000_000;

    #[test]
    fn hot_below_threshold_is_not_due() {
        assert_eq!(next_action(Tier::Hot, T0, T0 + 299_999, 0, &cfg()), Action::NotDue);
    }

    #[test]
    fn hot_exactly_at_threshold_stops() {
        assert_eq!(next_action(Tier::Hot, T0, T0 + 300_000, 0, &cfg()), Action::Stop);
    }

    #[test]
    fn hot_with_lease_is_skipped() {
        assert_eq!(next_action(Tier::Hot, T0, T0 + 300_000, 1, &cfg()), Action::SkipLease);
        assert_eq!(next_action(Tier::Hot, T0, T0 + 10_000_000, 2, &cfg()), Action::SkipLease);
    }

    #[test]
    fn hot_with_lease_below_threshold_is_not_due() {
        assert_eq!(next_action(Tier::Hot, T0, T0 + 1_000, 1, &cfg()), Action::NotDue);
    }

    #[test]
    fn stopped_archive_threshold() {
        assert_eq!(next_action(Tier::Stopped, T0, T0 + 86_399_999, 0, &cfg()), Action::NotDue);
        assert_eq!(next_action(Tier::Stopped, T0, T0 + 86_400_000, 0, &cfg()), Action::Archive);
        assert_eq!(next_action(Tier::Stopped, T0, T0 + 86_400_000, 1, &cfg()), Action::SkipLease);
    }

    #[test]
    fn hot_past_archive_threshold_stops_first() {
        assert_eq!(next_action(Tier::Hot, T0, T0 + 90_000_000, 0, &cfg()), Action::Stop);
    }

    #[test]
    fn other_tiers_never_demote() {
        for t in [Tier::Archived, Tier::Lost, Tier::Orphan, Tier::Transient] {
            assert_eq!(next_action(t, T0, T0 + 999_999_999, 0, &cfg()), Action::NotDue);
        }
    }

    #[test]
    fn clock_going_backwards_is_not_due() {
        assert_eq!(next_action(Tier::Hot, T0, T0 - 5_000, 0, &cfg()), Action::NotDue);
    }
}
