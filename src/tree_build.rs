//! Turn a flat, parent-linked row set into a nested tree.
//!
//! Four places in this crate had grown their own version of this —
//! `auto_menu` (page-derived menus), `navigation` (curated menus), the
//! admin page list, and `menu-builder.js` in the browser. The shape is
//! always the same: bucket rows by `parent_id`, then walk down from a
//! root, bounded by a depth.
//!
//! Both functions here are **pure and query-free**. That is the whole
//! point: the caller does exactly one bounded fetch, and the nesting
//! costs nothing more. Building a tree by asking the database for each
//! node's children is the N+1 that this module exists to prevent —
//! and on `cms_page` it would be an unindexed lookup per node, since
//! only `path`, `status`, `url_path` and `page_type_id` are indexed.

use std::collections::HashMap;

/// Bucket rows by their parent id.
///
/// Order within each bucket is the order rows arrive in, so a caller
/// that fetched with `ORDER BY path` (tree pre-order) or
/// `ORDER BY sort_order, id` gets a stable, correctly-ordered tree for
/// free.
pub fn index_by_parent<T, F>(rows: &[T], parent_of: F) -> HashMap<Option<i64>, Vec<&T>>
where
    F: Fn(&T) -> Option<i64>,
{
    let mut by_parent: HashMap<Option<i64>, Vec<&T>> = HashMap::new();
    for r in rows {
        by_parent.entry(parent_of(r)).or_default().push(r);
    }
    by_parent
}

/// Materialize the nodes under `parent`, at most `depth` levels deep.
///
/// `depth = 1` yields immediate children with empty child lists; a
/// depth of `0` or less yields nothing. `make` receives each row with
/// its already-built children, so the caller decides the node shape
/// without this module knowing anything about it.
pub fn build<'a, T, N, I, M>(
    by_parent: &HashMap<Option<i64>, Vec<&'a T>>,
    parent: Option<i64>,
    depth: i64,
    id_of: &I,
    make: &M,
) -> Vec<N>
where
    I: Fn(&T) -> i64,
    M: Fn(&'a T, Vec<N>) -> N,
{
    if depth <= 0 {
        return Vec::new();
    }
    let Some(rows) = by_parent.get(&parent) else {
        return Vec::new();
    };
    rows.iter()
        .map(|r| {
            let children = if depth > 1 {
                build(by_parent, Some(id_of(r)), depth - 1, id_of, make)
            } else {
                Vec::new()
            };
            make(r, children)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Row {
        id: i64,
        parent: Option<i64>,
        label: &'static str,
    }

    #[derive(Debug, PartialEq)]
    struct Node {
        label: &'static str,
        children: Vec<Node>,
    }

    fn rows() -> Vec<Row> {
        // 1 ─ 2 ─ 4
        //   └ 3
        // 5 (second root)
        vec![
            Row { id: 1, parent: None, label: "root-a" },
            Row { id: 2, parent: Some(1), label: "child-a1" },
            Row { id: 3, parent: Some(1), label: "child-a2" },
            Row { id: 4, parent: Some(2), label: "grandchild" },
            Row { id: 5, parent: None, label: "root-b" },
        ]
    }

    fn tree(depth: i64) -> Vec<Node> {
        let rows = rows();
        let idx = index_by_parent(&rows, |r| r.parent);
        build(
            &idx,
            None,
            depth,
            &|r: &Row| r.id,
            &|r: &Row, children| Node { label: r.label, children },
        )
    }

    #[test]
    fn nests_children_under_their_parent() {
        let t = tree(10);
        assert_eq!(t.len(), 2, "two roots");
        assert_eq!(t[0].label, "root-a");
        assert_eq!(t[0].children.len(), 2);
        assert_eq!(t[0].children[0].children[0].label, "grandchild");
        assert!(t[1].children.is_empty(), "root-b is childless");
    }

    #[test]
    fn depth_bounds_the_walk() {
        // The bound is what keeps a whole-site tree from being an
        // unbounded response.
        let t = tree(1);
        assert_eq!(t.len(), 2);
        assert!(t[0].children.is_empty(), "depth 1 = roots only");

        let t = tree(2);
        assert_eq!(t[0].children.len(), 2);
        assert!(
            t[0].children[0].children.is_empty(),
            "depth 2 stops before grandchildren"
        );
    }

    #[test]
    fn a_zero_or_negative_depth_yields_nothing() {
        assert!(tree(0).is_empty());
        assert!(tree(-1).is_empty());
    }

    #[test]
    fn bucket_order_follows_input_order() {
        // Callers rely on this: they sort in SQL (`ORDER BY path`) and
        // expect the tree to come out in that order.
        let rows = rows();
        let idx = index_by_parent(&rows, |r| r.parent);
        let kids: Vec<&str> = idx[&Some(1)].iter().map(|r| r.label).collect();
        assert_eq!(kids, vec!["child-a1", "child-a2"]);
    }

    #[test]
    fn an_orphan_row_is_simply_absent() {
        // A row whose parent isn't in the set (filtered out by status or
        // a view restriction) must not surface at the root — that would
        // leak a page out of a subtree its parent gates.
        let rows = vec![
            Row { id: 1, parent: None, label: "root" },
            Row { id: 9, parent: Some(404), label: "orphan" },
        ];
        let idx = index_by_parent(&rows, |r| r.parent);
        let t: Vec<Node> = build(
            &idx,
            None,
            10,
            &|r: &Row| r.id,
            &|r: &Row, children| Node { label: r.label, children },
        );
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].label, "root");
    }
}
