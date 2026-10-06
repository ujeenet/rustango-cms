//! Materialized-path encoding for the page tree.
//!
//! A page's `path` is its ancestors' row ids as hex segments
//! followed by its own, so `ORDER BY path` is a depth-first walk and a
//! subtree is one prefix range. This module only builds and parses
//! paths; tree mutations live in [`crate::tree_ops`].

use thiserror::Error;

/// Width of a materialized-path segment for ids up to 65 535: 4 hex chars.
///
/// A segment is the node's **row id**, not its position under its parent,
/// so this bounds ids per tenant, not children per parent. Larger ids use
/// [`WIDE_MARKER`] + [`WIDE_WIDTH`] hex chars instead (#678).
pub const SEGMENT_WIDTH: usize = 4;

/// Marks a segment for an id above 65 535. `~` sorts after every hex
/// digit, so wide siblings order after narrow ones and by id among
/// themselves; like any segment it ends in [`SEPARATOR`], which sorts
/// below every digit, so a subtree stays contiguous under `ORDER BY path`.
pub const WIDE_MARKER: char = '~';

/// Hex width of a wide segment: ids up to `u32::MAX`.
pub const WIDE_WIDTH: usize = 8;
pub const SEPARATOR: char = '/';

#[derive(Debug, Error)]
pub enum PathError {
    #[error("tree id {id} is outside 1..={max}", max = u32::MAX)]
    SegmentOverflow { id: i64 },
    #[error("path segment {segment:?} is not a valid {SEGMENT_WIDTH}-wide hex id")]
    BadSegment { segment: String },
}

/// Helper for building, parsing, and querying materialized paths.
///
/// A path is a sequence of hex id segments joined by `/` with a trailing
/// `/`. Example for ids 1, 3, 8: `"0001/0003/0008/"`; an id above 65 535
/// is written wide, e.g. 70 000 → `"~00011170/"`.
///
/// Trailing slash matters — it makes prefix queries (`path LIKE 'parent/%'`)
/// safe against false matches like `0001/00012/` matching `0001/0001/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializedPath(String);

impl MaterializedPath {
    pub fn root(id: i64) -> Result<Self, PathError> {
        let mut s = String::with_capacity(SEGMENT_WIDTH + 1);
        push_segment(&mut s, id)?;
        Ok(Self(s))
    }

    pub fn child_of(parent: &str, id: i64) -> Result<Self, PathError> {
        let mut s = String::with_capacity(parent.len() + SEGMENT_WIDTH + 1);
        s.push_str(parent);
        push_segment(&mut s, id)?;
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }

    pub fn depth(&self) -> i32 {
        depth_of(&self.0)
    }

    /// Returns the parent's path, or None if this is a root.
    pub fn parent(&self) -> Option<String> {
        parent_of(&self.0).map(str::to_owned)
    }

    /// All ancestor paths from root → immediate parent (excludes self).
    pub fn ancestors(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut acc = String::new();
        for seg in self.0.split(SEPARATOR).filter(|s| !s.is_empty()) {
            acc.push_str(seg);
            acc.push(SEPARATOR);
            if acc != self.0 {
                out.push(acc.clone());
            }
        }
        out
    }
}

/// Compute depth from a raw path string. Trailing-slash safe.
pub fn depth_of(path: &str) -> i32 {
    path.split(SEPARATOR).filter(|s| !s.is_empty()).count() as i32
}

/// Strip the last segment, returning the parent prefix (with trailing `/`),
/// or None if the path was already a single segment.
pub fn parent_of(path: &str) -> Option<&str> {
    let trimmed = path.strip_suffix(SEPARATOR).unwrap_or(path);
    let cut = trimmed.rfind(SEPARATOR)?;
    Some(&path[..=cut])
}

/// SQL `LIKE` pattern matching every descendant of `parent_path` — and the
/// parent itself, since `%` matches the empty string. Callers that want
/// descendants only add `path != parent_path`.
pub fn descendants_like(parent_path: &str) -> String {
    format!("{parent_path}%")
}

fn push_segment(buf: &mut String, id: i64) -> Result<(), PathError> {
    use std::fmt::Write;
    if id <= 0 || id > i64::from(u32::MAX) {
        return Err(PathError::SegmentOverflow { id });
    }
    // Writing into a String cannot fail.
    if id <= i64::from(u16::MAX) {
        let _ = write!(buf, "{:0width$x}{}", id, SEPARATOR, width = SEGMENT_WIDTH);
    } else {
        let _ = write!(buf, "{WIDE_MARKER}{:0width$x}{}", id, SEPARATOR, width = WIDE_WIDTH);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_and_child() {
        let root = MaterializedPath::root(1).unwrap();
        assert_eq!(root.as_str(), "0001/");
        let child = MaterializedPath::child_of(root.as_str(), 3).unwrap();
        assert_eq!(child.as_str(), "0001/0003/");
        assert_eq!(child.depth(), 2);
        assert_eq!(child.parent().as_deref(), Some("0001/"));
    }

    #[test]
    fn ancestors_excludes_self() {
        let p = MaterializedPath::child_of("0001/0003/", 8).unwrap();
        assert_eq!(
            p.ancestors(),
            vec!["0001/".to_string(), "0001/0003/".to_string()]
        );
    }

    #[test]
    fn descendants_like_pattern() {
        assert_eq!(descendants_like("0001/"), "0001/%");
    }

    #[test]
    fn segment_overflow_rejected() {
        assert!(matches!(
            MaterializedPath::root(0),
            Err(PathError::SegmentOverflow { id: 0 })
        ));
        assert!(matches!(
            MaterializedPath::root(i64::from(u32::MAX) + 1),
            Err(PathError::SegmentOverflow { .. })
        ));
    }

    #[test]
    fn ids_past_65535_get_a_wide_segment() {
        // #678 — a tenant's 65 536th page used to be impossible to create.
        assert_eq!(MaterializedPath::root(0xffff).unwrap().as_str(), "ffff/");
        assert_eq!(MaterializedPath::root(0x1_0000).unwrap().as_str(), "~00010000/");
        let child = MaterializedPath::child_of("0001/", 70_000).unwrap();
        assert_eq!(child.as_str(), "0001/~00011170/");
        assert_eq!(child.depth(), 2);
        assert_eq!(child.parent().as_deref(), Some("0001/"));
    }

    #[test]
    fn mixed_widths_keep_sibling_order_and_subtree_contiguity() {
        let parent = "0001/";
        let narrow = MaterializedPath::child_of(parent, 0xfffe).unwrap().into_string();
        let narrow_kid = MaterializedPath::child_of(&narrow, 2).unwrap().into_string();
        let wide = MaterializedPath::child_of(parent, 0x1_0001).unwrap().into_string();
        let wider = MaterializedPath::child_of(parent, 0x2_0000).unwrap().into_string();
        let mut sorted = vec![wider.clone(), wide.clone(), narrow_kid.clone(), narrow.clone()];
        sorted.sort();
        // Pre-order: each node before its subtree, subtrees contiguous,
        // siblings in id order across the width boundary.
        assert_eq!(sorted, vec![narrow, narrow_kid, wide, wider]);
    }
}
