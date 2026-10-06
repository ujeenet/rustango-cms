//! Admin-resource registry — single source of truth for the CRUD
//! permissions matrix on `/cms-admin/roles/{id}/edit` (#33).
//!
//! Each entry describes one sidebar resource (Pages, Library, Media,
//! Redirects, Users, …) with:
//! - a stable string `key` used in the matrix form (`perm[key][action]`),
//! - a human `label` shown in the row header,
//! - a `codename_prefix` mapping the row to the existing
//!   `rustango_role_permissions` codename system (so matrix toggles
//!   read/write the same column the framework already enforces),
//! - the `section` group used to bucket rows (Content / Media / Site /
//!   Audit / Identity / Custom),
//! - the subset of [`crate::permissions::Action`] columns that apply
//!   (read-only rows like Page types and History skip Add/Edit/Delete).
//!
//! Three sources contribute rows at runtime:
//! 1. The static built-in list ([`built_in_resources`]).
//! 2. Every registered [`crate::library::LibraryTypeHandler`] — each
//!    one auto-adds a `library_item:<type_name>` row so per-snippet
//!    CRUD is grantable independently.
//! 3. Every registered
//!    [`crate::admin::admin_page::AdminPageHandler`] whose
//!    `permission_resource()` returns `Some` — custom admin pages
//!    appear in the matrix without any role-editor code change.

use crate::permissions::Action;

/// One row in the role permissions matrix.
#[derive(Debug, Clone)]
pub struct AdminResource {
    /// Stable form key. Format: snake_case for built-ins
    /// (`pages`, `media`, `documents`); `library_item:<type_name>`
    /// for library-type rows; `custom:<slug>` for third-party
    /// admin pages.
    pub key: String,
    /// Sidebar / matrix label.
    pub label: String,
    /// Codename prefix used in `rustango_role_permissions.codename`.
    /// Combined with an action via `.` — `{prefix}.{action}` —
    /// matching the existing `cms_page.view` / `cms_media.add`
    /// convention.
    pub codename_prefix: String,
    /// Visual bucket used in the matrix grouping.
    pub section: ResourceSection,
    /// Which action columns apply to this resource. Cells outside
    /// this set render `—` and don't accept toggles.
    pub actions: &'static [Action],
}

/// Visual / logical grouping for the matrix rows. Rendered as the
/// row-header band above its members.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceSection {
    Access,
    Content,
    Media,
    Site,
    Audit,
    Identity,
    /// Public members-area access tiers (#members) — permission
    /// codenames a page/type restriction can require. Roles carry
    /// these; admins assign the roles to members.
    Membership,
    Custom,
}

impl ResourceSection {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Access => "Access",
            Self::Content => "Content",
            Self::Media => "Media & documents",
            Self::Site => "Site structure",
            Self::Audit => "Audit",
            Self::Identity => "Identity",
            Self::Membership => "Membership (public site)",
            Self::Custom => "Custom admin pages",
        }
    }

    #[must_use]
    pub fn order(self) -> u8 {
        match self {
            Self::Access => 0,
            Self::Content => 1,
            Self::Media => 2,
            Self::Site => 3,
            Self::Audit => 4,
            Self::Identity => 5,
            Self::Membership => 6,
            Self::Custom => 7,
        }
    }
}

const ALL_ACTIONS: &[Action] = &[
    Action::View,
    Action::Add,
    Action::Edit,
    Action::Publish,
    Action::Delete,
];
const CRUD_ACTIONS: &[Action] = &[Action::View, Action::Add, Action::Edit, Action::Delete];
const READ_ONLY: &[Action] = &[Action::View];
const VIEW_EDIT: &[Action] = &[Action::View, Action::Edit];
const ACCESS_ONLY: &[Action] = &[Action::Access];
const ACCESS_ADMIN_ONLY: &[Action] = &[Action::AccessAdmin];

/// Built-in sidebar resources, in matrix display order. Library-type
/// rows and custom-admin-page rows are appended dynamically by
/// [`all_resources`].
#[must_use]
pub fn built_in_resources() -> Vec<AdminResource> {
    vec![
        AdminResource {
            key: "cms_admin".to_owned(),
            label: "Access the CMS admin".to_owned(),
            codename_prefix: "cms_admin".to_owned(),
            section: ResourceSection::Access,
            actions: ACCESS_ONLY,
        },
        // #10 — framework admin access. The framework's
        // `permission_required` middleware (rustango#311) gates
        // `/__admin/` + `/admin/` on the `auth.access_admin`
        // codename. Surfacing the row here lets admins grant /
        // revoke framework-admin access from the same matrix that
        // controls CMS access, so they don't have to know two
        // different codename surfaces.
        AdminResource {
            key: "rustango_admin".to_owned(),
            label: "Access the framework admin (/admin/)".to_owned(),
            codename_prefix: "auth".to_owned(),
            section: ResourceSection::Access,
            actions: ACCESS_ADMIN_ONLY,
        },
        AdminResource {
            key: "pages".to_owned(),
            label: "Pages".to_owned(),
            codename_prefix: "cms_page".to_owned(),
            section: ResourceSection::Content,
            actions: ALL_ACTIONS,
        },
        AdminResource {
            key: "page_types".to_owned(),
            label: "Page types".to_owned(),
            codename_prefix: "cms_page_type".to_owned(),
            section: ResourceSection::Content,
            actions: READ_ONLY,
        },
        AdminResource {
            key: "library".to_owned(),
            label: "Library (all)".to_owned(),
            codename_prefix: "cms_library".to_owned(),
            section: ResourceSection::Content,
            actions: CRUD_ACTIONS,
        },
        AdminResource {
            key: "media".to_owned(),
            label: "Media".to_owned(),
            codename_prefix: "cms_media".to_owned(),
            section: ResourceSection::Media,
            actions: CRUD_ACTIONS,
        },
        AdminResource {
            key: "documents".to_owned(),
            label: "Documents".to_owned(),
            codename_prefix: "cms_document".to_owned(),
            section: ResourceSection::Media,
            actions: CRUD_ACTIONS,
        },
        AdminResource {
            key: "collections".to_owned(),
            label: "Media folders".to_owned(),
            codename_prefix: "cms_media_collection".to_owned(),
            section: ResourceSection::Media,
            actions: CRUD_ACTIONS,
        },
        AdminResource {
            key: "locales".to_owned(),
            label: "Locales".to_owned(),
            codename_prefix: "cms_locale".to_owned(),
            section: ResourceSection::Site,
            actions: CRUD_ACTIONS,
        },
        AdminResource {
            key: "redirects".to_owned(),
            label: "Redirects".to_owned(),
            codename_prefix: "cms_redirect".to_owned(),
            section: ResourceSection::Site,
            actions: CRUD_ACTIONS,
        },
        AdminResource {
            key: "navigation".to_owned(),
            label: "Navigation menus".to_owned(),
            codename_prefix: "cms_navigation_menu".to_owned(),
            section: ResourceSection::Site,
            actions: CRUD_ACTIONS,
        },
        AdminResource {
            key: "workflows".to_owned(),
            label: "Workflows".to_owned(),
            codename_prefix: "cms_workflow".to_owned(),
            section: ResourceSection::Site,
            actions: CRUD_ACTIONS,
        },
        AdminResource {
            key: "settings".to_owned(),
            label: "Settings".to_owned(),
            codename_prefix: "cms_settings".to_owned(),
            section: ResourceSection::Site,
            actions: VIEW_EDIT,
        },
        AdminResource {
            key: "history".to_owned(),
            label: "Revision history".to_owned(),
            codename_prefix: "cms_history".to_owned(),
            section: ResourceSection::Audit,
            actions: READ_ONLY,
        },
        AdminResource {
            key: "users".to_owned(),
            label: "Users".to_owned(),
            codename_prefix: "rustango_users".to_owned(),
            section: ResourceSection::Identity,
            actions: CRUD_ACTIONS,
        },
        AdminResource {
            key: "roles".to_owned(),
            label: "Roles".to_owned(),
            codename_prefix: "rustango_roles".to_owned(),
            section: ResourceSection::Identity,
            actions: CRUD_ACTIONS,
        },
        // #members — public members-area access tiers. Each is a single
        // "Access" permission codename a page/type view-restriction can
        // require (`members.access`, `members_pro.access`,
        // `members_moderator.access`). Ticking one grants that codename
        // to the role; a member holding the role then satisfies the
        // gate. These auto-surface as matrix checkboxes with no extra
        // wiring (the matrix is data-driven from `all_resources`).
        AdminResource {
            key: "members_area".to_owned(),
            label: "Access members area".to_owned(),
            codename_prefix: "members".to_owned(),
            section: ResourceSection::Membership,
            actions: ACCESS_ONLY,
        },
        AdminResource {
            key: "members_pro".to_owned(),
            label: "Pro member".to_owned(),
            codename_prefix: "members_pro".to_owned(),
            section: ResourceSection::Membership,
            actions: ACCESS_ONLY,
        },
        AdminResource {
            key: "members_moderator".to_owned(),
            label: "Moderator".to_owned(),
            codename_prefix: "members_moderator".to_owned(),
            section: ResourceSection::Membership,
            actions: ACCESS_ONLY,
        },
    ]
}

/// The membership permission codenames a page/type view-restriction can
/// require (#members), as `(codename, label)` — for the restriction
/// editor's permission picker. Derived from the built-in Membership
/// resources so the picker and the role matrix never drift.
#[must_use]
pub fn membership_codenames() -> Vec<(String, String)> {
    built_in_resources()
        .into_iter()
        .filter(|r| r.section == ResourceSection::Membership)
        .map(|r| (format!("{}.access", r.codename_prefix), r.label))
        .collect()
}

/// Full resource list = built-ins + library-type rows + custom
/// admin-page rows. Library-type rows let an admin grant CRUD on a
/// specific snippet kind (e.g. Author) without granting the whole
/// library tab. Custom-page rows surface third-party admin views.
#[must_use]
pub fn all_resources() -> Vec<AdminResource> {
    let mut out = built_in_resources();

    // Library-type rows — one per registered LibraryTypeHandler.
    for handler in crate::library::registered_handlers() {
        let type_name = handler.type_name();
        out.push(AdminResource {
            key: format!("library_item:{type_name}"),
            label: format!("Library — {}", handler.verbose_name()),
            codename_prefix: format!("cms_library_item__{type_name}"),
            section: ResourceSection::Content,
            actions: CRUD_ACTIONS,
        });
    }

    // Custom admin page rows — opt-in via `permission_resource()`.
    for handler in super::admin_page::registered_admin_pages() {
        if let Some(prefix) = handler.permission_resource() {
            out.push(AdminResource {
                key: format!("custom:{}", handler.slug()),
                label: handler.label().to_owned(),
                codename_prefix: prefix.to_owned(),
                section: ResourceSection::Custom,
                actions: VIEW_EDIT,
            });
        }
    }

    out
}

/// Group resources by section in display order, preserving the order
/// resources are returned by [`all_resources`].
#[must_use]
pub fn grouped() -> Vec<(ResourceSection, Vec<AdminResource>)> {
    let mut by_section: std::collections::BTreeMap<u8, (ResourceSection, Vec<AdminResource>)> =
        std::collections::BTreeMap::new();
    for r in all_resources() {
        let entry = by_section
            .entry(r.section.order())
            .or_insert_with(|| (r.section, Vec::new()));
        entry.1.push(r);
    }
    by_section.into_values().collect()
}

/// Build the canonical codename for `(resource, action)`. Always
/// `{prefix}.{action_str}` — e.g. `cms_page.view`,
/// `cms_library_item__author.edit`.
#[must_use]
pub fn codename(resource: &AdminResource, action: Action) -> String {
    format!("{}.{}", resource.codename_prefix, action.as_str())
}

/// Reverse mapping: given a codename, find the matching `(key,
/// action)` pair so existing role grants can be rendered as
/// pre-checked matrix cells. Returns `None` if the codename doesn't
/// match any registered resource (e.g. a legacy free-form codename
/// the admin hasn't migrated yet).
#[must_use]
pub fn parse_codename(codename: &str) -> Option<(String, Action)> {
    let (prefix, action_str) = codename.rsplit_once('.')?;
    let action = match action_str {
        "view" => Action::View,
        "add" => Action::Add,
        "edit" | "change" => Action::Edit,
        "publish" => Action::Publish,
        "delete" => Action::Delete,
        "access" => Action::Access,
        "access_admin" => Action::AccessAdmin,
        _ => return None,
    };
    let resources = all_resources();
    let r = resources.iter().find(|r| r.codename_prefix == prefix)?;
    Some((r.key.clone(), action))
}

/// The actions a role currently holds on `resource`, given the set of
/// granted `(role_id, codename)` pairs (#436). Only the resource's own
/// `actions` are considered, in declaration order. Drives each
/// `(resource × role)` cell of the tenant permission-overview matrix.
#[must_use]
pub fn granted_actions(
    resource: &AdminResource,
    role_id: i64,
    granted: &std::collections::HashSet<(i64, String)>,
) -> Vec<&'static str> {
    resource
        .actions
        .iter()
        .copied()
        .filter(|a| granted.contains(&(role_id, codename(resource, *a))))
        .map(Action::as_str)
        .collect()
}

#[cfg(test)]
mod overview_tests {
    use super::*;
    use std::collections::HashSet;

    fn pages_resource() -> AdminResource {
        AdminResource {
            key: "pages".to_owned(),
            label: "Pages".to_owned(),
            codename_prefix: "cms_page".to_owned(),
            section: ResourceSection::Content,
            actions: Action::all(),
        }
    }

    #[test]
    fn granted_actions_filters_to_held_codenames_in_order() {
        let res = pages_resource();
        let granted: HashSet<(i64, String)> = [
            (1, "cms_page.edit".to_owned()),
            (1, "cms_page.view".to_owned()),
            (2, "cms_page.view".to_owned()),
            // a grant for a different resource must not leak in
            (1, "cms_media.delete".to_owned()),
        ]
        .into_iter()
        .collect();
        // Role 1: view + edit, returned in Action::all() order (view before edit).
        assert_eq!(granted_actions(&res, 1, &granted), vec!["view", "edit"]);
        // Role 2: only view.
        assert_eq!(granted_actions(&res, 2, &granted), vec!["view"]);
        // Role 3: nothing.
        assert!(granted_actions(&res, 3, &granted).is_empty());
    }

    #[test]
    fn granted_actions_only_considers_resource_actions() {
        // A read-only resource (View only) never reports edit/delete even
        // if a stray codename grant exists.
        let res = AdminResource {
            key: "history".to_owned(),
            label: "History".to_owned(),
            codename_prefix: "cms_history".to_owned(),
            section: ResourceSection::Audit,
            actions: &[Action::View],
        };
        let granted: HashSet<(i64, String)> =
            [(1, "cms_history.delete".to_owned())].into_iter().collect();
        assert!(granted_actions(&res, 1, &granted).is_empty());
    }
}
