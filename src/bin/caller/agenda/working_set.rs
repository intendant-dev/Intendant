//! The served WORKING SET (agenda ontology P3): the territory an item
//! and its placed subtree point at — file/dir refs only, deduped to the
//! newest attach per `(type, locator)`, recency-ordered, capped. A
//! serving-seam derivation exactly like placed-children counts: computed
//! on demand from the fold, never stored. This lane aggregates what
//! agents DECLARED at park time; observed-territory mining is a
//! separate, provenance-labeled concern and never writes here.

use super::types::{AgendaItem, AgendaRefType, AgendaStatus};
use std::collections::{BTreeMap, BTreeSet};

/// Served row cap: a hub's aggregate is a working set, not an index —
/// `total` carries the distinct-locator count so truncation stays
/// honest.
pub(crate) const WORKING_SET_MAX_ROWS: usize = 48;

/// One territory row: a file/dir ref somewhere in the requested item's
/// subtree, with the carrying item named so "via which child" stays
/// visible to whoever picks the item up.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub(crate) struct WorkingSetRow {
    pub(crate) ref_type: AgendaRefType,
    pub(crate) locator: String,
    pub(crate) item_id: String,
    pub(crate) item_title: String,
    pub(crate) added_ms: u64,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) must_read: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) label: Option<String>,
}

/// The served block: capped rows plus the honest distinct total.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub(crate) struct WorkingSet {
    pub(crate) rows: Vec<WorkingSetRow>,
    pub(crate) total: usize,
}

/// Compute `root_id`'s working set over a fold snapshot. The subtree is
/// the `part_of` descendant closure (BFS, cycle-guarded so a foreign
/// log's cycle stays total). Retired items are hidden items and
/// contribute no refs — but their children are still walked, since
/// retiring a hub never hides its children; done items DO contribute
/// (finished work's territory is exactly the affinity signal an
/// adopting session wants). `None` only when `root_id` names no item.
pub(crate) fn working_set(items: &[AgendaItem], root_id: &str) -> Option<WorkingSet> {
    let by_id: BTreeMap<&str, &AgendaItem> =
        items.iter().map(|item| (item.id.as_str(), item)).collect();
    by_id.get(root_id)?;
    let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for item in items {
        if let Some(placement) = &item.part_of {
            children
                .entry(placement.parent_id.as_str())
                .or_default()
                .push(item.id.as_str());
        }
    }
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut queue: Vec<&str> = vec![root_id];
    let mut best: BTreeMap<(&'static str, String), (&AgendaItem, usize)> = BTreeMap::new();
    while let Some(id) = queue.pop() {
        if !seen.insert(id) {
            continue;
        }
        let Some(item) = by_id.get(id) else { continue };
        if item.status != AgendaStatus::Retired || item.id == root_id {
            for (index, r) in item.refs.iter().enumerate() {
                if !matches!(r.ref_type, AgendaRefType::File | AgendaRefType::Dir) {
                    continue;
                }
                let key = (r.ref_type.as_str(), r.locator.clone());
                let newer = best
                    .get(&key)
                    .is_none_or(|(cur, cur_index)| r.added_ms > cur.refs[*cur_index].added_ms);
                if newer {
                    best.insert(key, (item, index));
                }
            }
        }
        if let Some(kids) = children.get(id) {
            queue.extend(kids);
        }
    }
    let total = best.len();
    let mut rows: Vec<WorkingSetRow> = best
        .into_values()
        .map(|(item, index)| {
            let r = &item.refs[index];
            WorkingSetRow {
                ref_type: r.ref_type,
                locator: r.locator.clone(),
                item_id: item.id.clone(),
                item_title: item.title.clone(),
                added_ms: r.added_ms,
                must_read: r.must_read,
                label: r.label.clone(),
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        b.added_ms
            .cmp(&a.added_ms)
            .then_with(|| a.locator.cmp(&b.locator))
    });
    rows.truncate(WORKING_SET_MAX_ROWS);
    Some(WorkingSet { rows, total })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agenda::types::{AgendaKind, AgendaPlacement, AgendaProvenance, AgendaRef};

    fn item(
        id: &str,
        parent: Option<&str>,
        status: AgendaStatus,
        refs: Vec<(AgendaRefType, &str, u64)>,
    ) -> AgendaItem {
        AgendaItem {
            id: id.into(),
            kind: AgendaKind::Task,
            title: format!("{id} title"),
            body: String::new(),
            tags: Vec::new(),
            due_ms: None,
            provenance: AgendaProvenance {
                principal: None,
                session_id: None,
                kind: None,
                source: None,
                created_ms: 1,
            },
            status,
            updated_ms: 1,
            completed_ms: None,
            answer: None,
            effects: Vec::new(),
            ask: None,
            dismissed: None,
            annotations: Vec::new(),
            blockers: Vec::new(),
            relies_on: Vec::new(),
            refs: refs
                .into_iter()
                .map(|(ref_type, locator, added_ms)| AgendaRef {
                    ref_type,
                    locator: locator.into(),
                    digest: None,
                    must_read: false,
                    label: None,
                    added_ms,
                    principal: None,
                    session_id: None,
                    kind: None,
                    source: None,
                })
                .collect(),
            part_of: parent.map(|parent_id| AgendaPlacement {
                parent_id: parent_id.into(),
                added_ms: 1,
                principal: None,
                session_id: None,
                kind: None,
                source: None,
            }),
            relates_to: Vec::new(),
            deferred_until: None,
            watched_by: None,
        }
    }

    #[test]
    fn subtree_refs_aggregate_with_carrying_item_named() {
        let items = vec![
            item(
                "01HUB",
                None,
                AgendaStatus::Open,
                vec![(AgendaRefType::Dir, "/repo/src", 10)],
            ),
            item(
                "01CHILD",
                Some("01HUB"),
                AgendaStatus::Open,
                vec![(AgendaRefType::File, "/repo/a.rs", 20)],
            ),
            item(
                "01GRAND",
                Some("01CHILD"),
                AgendaStatus::Done,
                vec![(AgendaRefType::File, "/repo/b.rs", 30)],
            ),
            item(
                "01ELSEWHERE",
                None,
                AgendaStatus::Open,
                vec![(AgendaRefType::File, "/repo/unrelated.rs", 40)],
            ),
        ];
        let ws = working_set(&items, "01HUB").unwrap();
        assert_eq!(ws.total, 3, "unplaced strangers never contribute");
        assert_eq!(ws.rows[0].locator, "/repo/b.rs");
        assert_eq!(ws.rows[0].item_id, "01GRAND", "done items contribute");
        assert_eq!(ws.rows[2].item_id, "01HUB");
        // A narrower root scopes to its own subtree.
        let ws = working_set(&items, "01CHILD").unwrap();
        assert_eq!(ws.total, 2);
    }

    #[test]
    fn newest_attach_wins_per_locator_and_territory_types_only() {
        let items = vec![
            item(
                "01A",
                None,
                AgendaStatus::Open,
                vec![
                    (AgendaRefType::File, "/repo/hot.rs", 10),
                    (AgendaRefType::Url, "https://example.com/pr/1", 99),
                    (AgendaRefType::Memory, "abc123abc123", 99),
                    (AgendaRefType::Session, "sess-1", 99),
                ],
            ),
            item(
                "01B",
                Some("01A"),
                AgendaStatus::Open,
                vec![(AgendaRefType::File, "/repo/hot.rs", 25)],
            ),
        ];
        let ws = working_set(&items, "01A").unwrap();
        assert_eq!(ws.total, 1, "one distinct territory locator");
        assert_eq!(ws.rows[0].item_id, "01B", "newest attach wins");
        assert_eq!(ws.rows[0].added_ms, 25);
    }

    #[test]
    fn retired_items_contribute_nothing_but_their_children_still_do() {
        let items = vec![
            item("01ROOT", None, AgendaStatus::Open, Vec::new()),
            item(
                "01GONE",
                Some("01ROOT"),
                AgendaStatus::Retired,
                vec![(AgendaRefType::File, "/repo/stale.rs", 50)],
            ),
            item(
                "01ALIVE",
                Some("01GONE"),
                AgendaStatus::Open,
                vec![(AgendaRefType::File, "/repo/live.rs", 60)],
            ),
        ];
        let ws = working_set(&items, "01ROOT").unwrap();
        assert_eq!(ws.total, 1);
        assert_eq!(ws.rows[0].locator, "/repo/live.rs");
        // The retired item itself, asked directly, still owns its refs.
        let ws = working_set(&items, "01GONE").unwrap();
        assert_eq!(ws.total, 2);
    }

    #[test]
    fn rows_cap_with_honest_total_and_missing_root_is_none() {
        let mut items = vec![item("01ROOT", None, AgendaStatus::Open, Vec::new())];
        for i in 0..(WORKING_SET_MAX_ROWS + 2) {
            items.push(item(
                &format!("01C{i:03}"),
                Some("01ROOT"),
                AgendaStatus::Open,
                vec![(AgendaRefType::File, &format!("/repo/f{i:03}.rs"), i as u64)],
            ));
        }
        let ws = working_set(&items, "01ROOT").unwrap();
        assert_eq!(ws.rows.len(), WORKING_SET_MAX_ROWS);
        assert_eq!(ws.total, WORKING_SET_MAX_ROWS + 2);
        assert!(working_set(&items, "01NOSUCH").is_none());
    }
}
