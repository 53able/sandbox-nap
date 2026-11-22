//! 受付判定(純粋関数)。

/// 判定に使う hot サンドボックスの情報。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotEntry {
    pub name: String,
    pub memory_mb: u64,
    pub last_access_ms: i64,
    /// 退去できるか(リース 0 かつロックを取れた)。
    pub evictable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Admit,
    /// 列挙した hot を停止すれば受け付けられる(古い順)。
    Evict(Vec<String>),
    Reject { limit_mb: u64, current_mb: u64, request_mb: u64 },
}

/// `request_mb` を hot に加えてよいかを判定する。
///
/// hot 合計 + 要求 が上限以下なら Admit。超えるなら、退去可能な hot を
/// 最終アクセスの古い順に選び、収まるまで足す。足りなければ Reject。
pub fn decide(hot: &[HotEntry], request_mb: u64, limit_mb: u64) -> Decision {
    let current: u64 = hot.iter().map(|h| h.memory_mb).sum();
    if current.saturating_add(request_mb) <= limit_mb {
        return Decision::Admit;
    }
    let mut candidates: Vec<&HotEntry> = hot.iter().filter(|h| h.evictable).collect();
    candidates.sort_by(|a, b| a.last_access_ms.cmp(&b.last_access_ms).then_with(|| a.name.cmp(&b.name)));
    let mut remaining = current;
    let mut chosen = Vec::new();
    for c in candidates {
        if remaining.saturating_add(request_mb) <= limit_mb {
            break;
        }
        remaining -= c.memory_mb;
        chosen.push(c.name.clone());
    }
    if remaining.saturating_add(request_mb) <= limit_mb {
        Decision::Evict(chosen)
    } else {
        Decision::Reject { limit_mb, current_mb: current, request_mb }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(name: &str, mb: u64, last: i64, evictable: bool) -> HotEntry {
        HotEntry { name: name.into(), memory_mb: mb, last_access_ms: last, evictable }
    }

    #[test]
    fn admit_when_room() {
        assert_eq!(decide(&[h("a", 512, 1, true)], 256, 1024), Decision::Admit);
    }

    #[test]
    fn admit_exactly_at_limit() {
        assert_eq!(decide(&[h("a", 512, 1, true)], 512, 1024), Decision::Admit);
    }

    #[test]
    fn evict_oldest_first() {
        let hot = [h("new", 512, 20, true), h("old", 512, 10, true)];
        assert_eq!(decide(&hot, 512, 1024), Decision::Evict(vec!["old".into()]));
    }

    #[test]
    fn evict_multiple_until_fits() {
        let hot = [h("a", 300, 1, true), h("b", 300, 2, true), h("c", 300, 3, true)];
        assert_eq!(decide(&hot, 600, 1000), Decision::Evict(vec!["a".into(), "b".into()]));
    }

    #[test]
    fn skip_non_evictable() {
        let hot = [h("busy-old", 512, 1, false), h("idle-new", 512, 9, true)];
        assert_eq!(decide(&hot, 512, 1024), Decision::Evict(vec!["idle-new".into()]));
    }

    #[test]
    fn reject_when_candidates_insufficient() {
        let hot = [h("a", 512, 1, false), h("b", 512, 2, false)];
        assert_eq!(decide(&hot, 512, 1024), Decision::Reject { limit_mb: 1024, current_mb: 1024, request_mb: 512 });
    }

    #[test]
    fn reject_when_request_alone_exceeds_limit() {
        let hot = [h("a", 100, 1, true)];
        assert_eq!(decide(&hot, 2000, 1024), Decision::Reject { limit_mb: 1024, current_mb: 100, request_mb: 2000 });
    }

    #[test]
    fn empty_hot_admits_within_limit() {
        assert_eq!(decide(&[], 1024, 1024), Decision::Admit);
    }

    #[test]
    fn ties_break_by_name() {
        let hot = [h("b", 512, 5, true), h("a", 512, 5, true)];
        assert_eq!(decide(&hot, 512, 1024), Decision::Evict(vec!["a".into()]));
    }
}
