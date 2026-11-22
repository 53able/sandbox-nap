//! 実状態(`container` CLI)と DB 行の照合(純粋関数)。計画 3.5 の表を実装する。
//!
//! | コンテナ                   | DB 行 | archive_ref | 扱い        |
//! |----------------------------|-------|-------------|-------------|
//! | 稼働中(managed)          | 有    | 任意        | Hot         |
//! | 停止(managed)            | 有    | 任意        | Stopped     |
//! | 無し                       | 有    | 有          | Archived    |
//! | 無し                       | 有    | 無          | Lost        |
//! | 有(managed)              | 無    | -           | Orphan      |
//! | 有(managed でない)       | 任意  | -           | Unmanaged   |

use crate::model::{Sandbox, Tier};
use crate::runtime::{ContainerInfo, ContainerState};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// sandbox-nap が扱うサンドボックス。
    Managed { tier: Tier, row: Option<Sandbox> },
    /// `asb.managed=true` を持たないコンテナ。表示も操作もしない。
    Unmanaged,
}

/// 1 つの名前について照合する。名前に対応するコンテナも DB 行も無ければ None。
pub fn resolve_one(container: Option<&ContainerInfo>, row: Option<&Sandbox>) -> Option<Resolved> {
    match (container, row) {
        (Some(c), _) if !c.managed => Some(Resolved::Unmanaged),
        (Some(c), Some(r)) => Some(Resolved::Managed { tier: tier_of_state(c.state), row: Some(r.clone()) }),
        (Some(_), None) => Some(Resolved::Managed { tier: Tier::Orphan, row: None }),
        (None, Some(r)) if r.archive_ref.is_some() => {
            Some(Resolved::Managed { tier: Tier::Archived, row: Some(r.clone()) })
        }
        (None, Some(r)) => Some(Resolved::Managed { tier: Tier::Lost, row: Some(r.clone()) }),
        (None, None) => None,
    }
}

fn tier_of_state(state: ContainerState) -> Tier {
    match state {
        ContainerState::Running => Tier::Hot,
        ContainerState::Stopped => Tier::Stopped,
        ContainerState::Other => Tier::Transient,
    }
}

/// 全件を照合する。結果は名前順。Unmanaged も含む(呼び出し側が除外する)。
pub fn resolve_all(containers: &[ContainerInfo], rows: &[Sandbox]) -> BTreeMap<String, Resolved> {
    let by_c: BTreeMap<&str, &ContainerInfo> = containers.iter().map(|c| (c.id.as_str(), c)).collect();
    let by_r: BTreeMap<&str, &Sandbox> = rows.iter().map(|r| (r.name.as_str(), r)).collect();
    let mut names: Vec<&str> = by_c.keys().chain(by_r.keys()).copied().collect();
    names.sort_unstable();
    names.dedup();
    names
        .into_iter()
        .filter_map(|n| resolve_one(by_c.get(n).copied(), by_r.get(n).copied()).map(|r| (n.to_string(), r)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::NetworkMode;

    fn c(id: &str, state: ContainerState, managed: bool) -> ContainerInfo {
        ContainerInfo { id: id.into(), state, managed }
    }

    fn row(name: &str, archive: Option<&str>) -> Sandbox {
        Sandbox {
            name: name.into(),
            image: "alpine:3.22".into(),
            cpus: None,
            memory_mb: 512,
            network: NetworkMode::Internal,
            last_access_ms: 0,
            archive_ref: archive.map(String::from),
            archive_gen: u32::from(archive.is_some()),
            created_at_ms: 0,
        }
    }

    fn tier(r: Option<Resolved>) -> Option<Tier> {
        match r {
            Some(Resolved::Managed { tier, .. }) => Some(tier),
            _ => None,
        }
    }

    #[test]
    fn running_with_row_is_hot() {
        assert_eq!(tier(resolve_one(Some(&c("a", ContainerState::Running, true)), Some(&row("a", None)))), Some(Tier::Hot));
        // archive_ref があっても稼働中なら hot(行 1 の「任意」)
        assert_eq!(
            tier(resolve_one(Some(&c("a", ContainerState::Running, true)), Some(&row("a", Some("asb-archive/a:g1"))))),
            Some(Tier::Hot)
        );
    }

    #[test]
    fn stopped_with_row_is_stopped() {
        assert_eq!(
            tier(resolve_one(Some(&c("a", ContainerState::Stopped, true)), Some(&row("a", Some("asb-archive/a:g1"))))),
            Some(Tier::Stopped)
        );
    }

    #[test]
    fn missing_container_with_archive_is_archived() {
        assert_eq!(tier(resolve_one(None, Some(&row("a", Some("asb-archive/a:g1"))))), Some(Tier::Archived));
    }

    #[test]
    fn missing_container_without_archive_is_lost() {
        assert_eq!(tier(resolve_one(None, Some(&row("a", None)))), Some(Tier::Lost));
    }

    #[test]
    fn managed_container_without_row_is_orphan() {
        assert_eq!(tier(resolve_one(Some(&c("a", ContainerState::Running, true)), None)), Some(Tier::Orphan));
    }

    #[test]
    fn unmanaged_container_is_unmanaged_even_with_row() {
        assert_eq!(resolve_one(Some(&c("a", ContainerState::Running, false)), Some(&row("a", None))), Some(Resolved::Unmanaged));
        assert_eq!(resolve_one(Some(&c("a", ContainerState::Stopped, false)), None), Some(Resolved::Unmanaged));
    }

    #[test]
    fn transient_state() {
        assert_eq!(tier(resolve_one(Some(&c("a", ContainerState::Other, true)), Some(&row("a", None)))), Some(Tier::Transient));
    }

    #[test]
    fn nothing_is_none() {
        assert_eq!(resolve_one(None, None), None);
    }

    #[test]
    fn resolve_all_merges_names() {
        let cs = [c("a", ContainerState::Running, true), c("buildkit", ContainerState::Running, false), c("o", ContainerState::Stopped, true)];
        let rs = [row("a", None), row("z", Some("asb-archive/z:g1"))];
        let m = resolve_all(&cs, &rs);
        assert_eq!(m.len(), 4);
        assert_eq!(tier(m.get("a").cloned()), Some(Tier::Hot));
        assert_eq!(m.get("buildkit"), Some(&Resolved::Unmanaged));
        assert_eq!(tier(m.get("o").cloned()), Some(Tier::Orphan));
        assert_eq!(tier(m.get("z").cloned()), Some(Tier::Archived));
    }
}
