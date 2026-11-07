//! Shared pagination context for admin list pages.
//!
//! Three list pages had each grown their own pagination: different
//! context variable names (`page` vs `current_page`), different labels
//! ("Prev" vs "Previous"), one hand-building query strings and one
//! using the `querystring` filter — and none of them offered a page
//! size or a way to jump to a page.
//!
//! This builds the context that `rcms_admin/_pagination.html` renders,
//! so every list gets the same controls from one place. The page
//! arithmetic itself comes from [`rustango::pagination::Paginator`].

/// Rows per page when the request doesn't say.
pub const DEFAULT_PER_PAGE: usize = 50;
/// Ceiling for `?per_page=`. A hostile or fat-fingered value must not
/// turn a list page into a full-table fetch.
pub const MAX_PER_PAGE: usize = 100;
/// Offered in the size selector. Kept at/below [`MAX_PER_PAGE`].
pub const PER_PAGE_CHOICES: [usize; 3] = [25, 50, 100];

/// Snap a requested page size to one of [`PER_PAGE_CHOICES`].
///
/// Snapping, rather than merely capping at [`MAX_PER_PAGE`]: the size
/// selector can only display a value it offers, so an off-list size like
/// `?per_page=2` left the select showing "25" while the server served 2
/// — the control lied about the state it was in. Rounding up to the
/// nearest offered size keeps the two in agreement.
#[must_use]
pub fn clamp_per_page(requested: Option<usize>) -> usize {
    match requested {
        None | Some(0) => DEFAULT_PER_PAGE,
        Some(n) => PER_PAGE_CHOICES
            .into_iter()
            .find(|&choice| n <= choice)
            .unwrap_or(MAX_PER_PAGE),
    }
}

/// Build the template context for the shared pagination partial.
///
/// `page` is 1-based and clamped into range, so an out-of-range `?page=`
/// lands on the last page instead of rendering an empty list with dead
/// prev/next links.
#[must_use]
pub fn context(page: i64, per_page: usize, total: usize) -> serde_json::Value {
    let paginator = rustango::pagination::Paginator::new(total, per_page);
    let total_pages = paginator.num_pages().max(1) as i64;
    let current = page.clamp(1, total_pages);
    let start = if total == 0 {
        0
    } else {
        (current - 1) * per_page as i64 + 1
    };
    let end = (current * per_page as i64).min(total as i64);

    serde_json::json!({
        "page": current,
        "per_page": per_page,
        "total": total,
        "total_pages": total_pages,
        "start_index": start,
        "end_index": end,
        "has_prev": current > 1,
        "has_next": current < total_pages,
        "prev_page": (current - 1).max(1),
        "next_page": (current + 1).min(total_pages),
        "pages": (1..=total_pages).collect::<Vec<i64>>(),
        "per_page_choices": PER_PAGE_CHOICES,
        // Show the row when there is paging to do, when the list is
        // big enough that the size matters — or when the size is not
        // the default. That last clause matters: without it, picking a
        // size large enough to fit everything on one page hid the row,
        // and with it the only control that could put the size back.
        "show": total_pages > 1
            || total > PER_PAGE_CHOICES[0]
            || per_page != DEFAULT_PER_PAGE,
    })
}

/// Zero-based offset for the current page, for `.offset()`.
#[must_use]
pub fn offset(page: i64, per_page: usize) -> i64 {
    // Saturating: `?page=` is user input, and a wrapped (negative)
    // OFFSET is an error on some backends.
    (page.max(1) - 1).saturating_mul(i64::try_from(per_page).unwrap_or(i64::MAX))
}

/// `?page=&per_page=` for a list with no other query parameters.
#[derive(Debug, Default, serde::Deserialize)]
pub struct PageQuery {
    #[serde(default)]
    pub page: Option<i64>,
    #[serde(default)]
    pub per_page: Option<usize>,
}

/// One page of a list: what to fetch and what the partial renders.
#[derive(Debug)]
pub struct Window {
    pub limit: i64,
    pub offset: i64,
    /// The `pagination` context for `_pagination.html`.
    pub context: serde_json::Value,
}

impl PageQuery {
    /// The page window over `total` rows; an out-of-range `?page=`
    /// lands on the last page.
    #[must_use]
    pub fn window(&self, total: usize) -> Window {
        let per_page = clamp_per_page(self.per_page);
        let context = context(self.page.unwrap_or(1), per_page, total);
        let page = context["page"].as_i64().unwrap_or(1);
        Window {
            limit: per_page as i64,
            offset: offset(page, per_page),
            context,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_page_is_clamped_to_the_ceiling() {
        assert_eq!(clamp_per_page(None), DEFAULT_PER_PAGE);
        assert_eq!(clamp_per_page(Some(0)), DEFAULT_PER_PAGE);
        assert_eq!(clamp_per_page(Some(25)), 25);
        // The point of the ceiling: a big `?per_page=` must not become a
        // full-table fetch.
        assert_eq!(clamp_per_page(Some(100_000)), MAX_PER_PAGE);
    }

    #[test]
    fn odd_sizes_snap_to_an_offered_choice() {
        // The selector can only show a size it offers; serving a size it
        // cannot display makes the control misreport the current state.
        assert_eq!(clamp_per_page(Some(2)), 25);
        assert_eq!(clamp_per_page(Some(26)), 50);
        assert_eq!(clamp_per_page(Some(51)), 100);
        for n in PER_PAGE_CHOICES {
            assert_eq!(clamp_per_page(Some(n)), n, "offered sizes pass through");
        }
    }

    #[test]
    fn the_size_control_never_hides_itself() {
        // A non-default size must keep the row visible even when
        // everything fits on one page — otherwise there is no way back.
        let c = context(1, 100, 6);
        assert_eq!(c["total_pages"], 1);
        assert!(
            c["show"].as_bool().unwrap(),
            "picking a large size must not remove the size selector"
        );
        // Default size on a short list still needs no controls.
        assert!(!context(1, DEFAULT_PER_PAGE, 6)["show"].as_bool().unwrap());
    }

    #[test]
    fn out_of_range_page_clamps_to_the_last_one() {
        let c = context(99, 50, 120);
        assert_eq!(c["page"], 3, "120 items / 50 = 3 pages");
        assert!(!c["has_next"].as_bool().unwrap());
        assert!(c["has_prev"].as_bool().unwrap());
    }

    #[test]
    fn indices_describe_the_visible_slice() {
        let c = context(2, 50, 120);
        assert_eq!(c["start_index"], 51);
        assert_eq!(c["end_index"], 100);
        // Last page is short: the end index is the total, not page*size.
        let last = context(3, 50, 120);
        assert_eq!(last["start_index"], 101);
        assert_eq!(last["end_index"], 120);
    }

    #[test]
    fn an_empty_list_is_still_page_one() {
        let c = context(1, 50, 0);
        assert_eq!(c["page"], 1);
        assert_eq!(c["total_pages"], 1);
        assert_eq!(c["start_index"], 0);
        assert_eq!(c["end_index"], 0);
        assert!(
            !c["show"].as_bool().unwrap(),
            "no controls for an empty list"
        );
    }

    #[test]
    fn offsets_follow_the_page() {
        assert_eq!(offset(1, 50), 0);
        assert_eq!(offset(3, 50), 100);
        assert_eq!(offset(0, 50), 0, "page 0 is treated as page 1");
    }

    #[test]
    fn a_window_past_the_end_shows_the_last_page() {
        let q = PageQuery { page: Some(99), per_page: Some(25) };
        let w = q.window(60);
        assert_eq!((w.limit, w.offset), (25, 50));
        assert_eq!(w.context["page"], 3);
    }
}
