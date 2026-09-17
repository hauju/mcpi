//! Collapsing the server library into one entry per product.
//!
//! A site's MCP endpoint and its WebMCP page are two rows with two contract
//! histories — they must be, since one digest stream fed two different
//! contracts would read as a wholesale replacement on every scan. But they are
//! one product, and a library that lists "SeggWat" twice makes the user carry
//! that distinction for no benefit.
//!
//! So the split stays in the store and the join happens here: purely a matter
//! of presentation, over `ServerRow`s the store hands back unchanged.

use mcpstore::{ServerId, ServerRow};

/// One sidebar entry: a product, and every surface saved for it.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    /// The row the entry is named after, and what it selects by default.
    pub lead: ServerRow,
    /// Further surfaces, in library order.
    pub rest: Vec<ServerRow>,
}

impl Group {
    pub fn rows(&self) -> impl Iterator<Item = &ServerRow> {
        std::iter::once(&self.lead).chain(self.rest.iter())
    }

    /// Whether `id` is one of this entry's surfaces.
    pub fn contains(&self, id: ServerId) -> bool {
        self.rows().any(|row| row.id == id)
    }

    /// The surface this entry is currently showing.
    ///
    /// Falls back to the lead, so an entry always has something to show even
    /// before anything in it has been selected.
    pub fn active(&self, selected: Option<ServerId>) -> &ServerRow {
        selected
            .and_then(|id| self.rows().find(|row| row.id == id))
            .unwrap_or(&self.lead)
    }

    /// Whether this entry has more than one surface to switch between.
    pub fn is_split(&self) -> bool {
        !self.rest.is_empty()
    }
}

/// Collapse a library into one entry per product, preserving library order.
///
/// A row pointing at a group leader joins it; anything else leads its own
/// entry. A row whose leader is missing, or is itself grouped, leads its own
/// entry rather than disappearing — a dangling link must never cost the user a
/// server they can see in the database.
pub fn group(rows: &[ServerRow]) -> Vec<Group> {
    let leads: Vec<&ServerRow> = rows.iter().filter(|row| row.group_id.is_none()).collect();

    let joins = |row: &ServerRow| -> Option<ServerId> {
        let target = row.group_id?;
        leads.iter().any(|lead| lead.id == target).then_some(target)
    };

    let mut groups: Vec<Group> = rows
        .iter()
        .filter(|row| joins(row).is_none())
        .map(|row| Group {
            lead: row.clone(),
            rest: Vec::new(),
        })
        .collect();

    for row in rows {
        let Some(target) = joins(row) else {
            continue;
        };
        if let Some(entry) = groups.iter_mut().find(|g| g.lead.id == target) {
            entry.rest.push(row.clone());
        }
    }

    groups
}

/// The entry a new surface at `url` belongs to, if the library already has one
/// on the same origin.
///
/// This is why grouping costs no typing: a site's MCP endpoint and its WebMCP
/// page almost always share an origin (`seggwat.com/mcp` and `seggwat.com`).
/// It is only ever a *suggestion* — an endpoint at `mcp.example.com` beside an
/// app at `example.com` is common enough that a rule here would be a blind
/// spot, so the dialog pre-fills and the user can always say otherwise.
///
/// Only group leaders are offered, so accepting a suggestion never builds a
/// chain that [`group`] would have to break apart again.
pub fn suggest(rows: &[ServerRow], url: &str, editing: Option<ServerId>) -> Option<ServerId> {
    let origin = crate::config::origin(url)?;
    rows.iter()
        .filter(|row| row.group_id.is_none() && Some(row.id) != editing)
        .find(|row| {
            crate::config::origin_of(row.transport_kind, &row.config).as_deref() == Some(&origin)
        })
        .map(|row| row.id)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use mcpstore::TransportKind;

    use super::*;

    fn row(id: ServerId, name: &str, group_id: Option<ServerId>) -> ServerRow {
        ServerRow {
            id,
            name: name.into(),
            transport_kind: TransportKind::Http,
            config: serde_json::json!({}),
            created_at: Utc::now(),
            last_connected_at: None,
            group_id,
        }
    }

    #[test]
    fn ungrouped_rows_each_lead_their_own_entry() {
        let rows = [row(1, "A", None), row(2, "B", None)];
        let groups = group(&rows);

        assert_eq!(groups.len(), 2);
        assert!(groups.iter().all(|g| !g.is_split()));
    }

    #[test]
    fn a_page_joins_the_endpoint_it_points_at() {
        let rows = [row(1, "SeggWat", None), row(2, "SeggWat", Some(1))];
        let groups = group(&rows);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].lead.id, 1);
        assert_eq!(groups[0].rest.len(), 1);
        assert!(groups[0].contains(2));
    }

    #[test]
    fn a_dangling_link_still_shows_its_server() {
        // The store sets `group_id` to NULL when a leader is deleted, so this
        // should not arise — but losing a row the user can see in the database
        // is the one outcome worth being defensive about.
        let rows = [row(2, "Orphan", Some(404))];
        let groups = group(&rows);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].lead.id, 2);
    }

    #[test]
    fn a_chain_does_not_swallow_the_middle_row() {
        // b points at a, c points at b. Only b joins a; c leads its own entry
        // rather than vanishing behind a row that is not a leader.
        let rows = [
            row(1, "A", None),
            row(2, "B", Some(1)),
            row(3, "C", Some(2)),
        ];
        let groups = group(&rows);

        assert_eq!(groups.len(), 2);
        assert_eq!(groups.iter().flat_map(Group::rows).count(), 3);
    }

    #[test]
    fn the_active_surface_follows_the_selection() {
        let rows = [row(1, "SeggWat", None), row(2, "SeggWat", Some(1))];
        let groups = group(&rows);
        let entry = &groups[0];

        assert_eq!(entry.active(None).id, 1);
        assert_eq!(entry.active(Some(2)).id, 2);
        // A selection elsewhere in the library leaves this entry on its lead.
        assert_eq!(entry.active(Some(99)).id, 1);
    }
}

#[cfg(test)]
mod suggestions {
    use chrono::Utc;
    use mcpstore::TransportKind;

    use super::*;

    fn url_row(
        id: ServerId,
        kind: TransportKind,
        url: &str,
        group_id: Option<ServerId>,
    ) -> ServerRow {
        ServerRow {
            id,
            name: format!("row-{id}"),
            transport_kind: kind,
            config: serde_json::json!({ "url": url }),
            created_at: Utc::now(),
            last_connected_at: None,
            group_id,
        }
    }

    fn library() -> Vec<ServerRow> {
        vec![
            url_row(1, TransportKind::Http, "https://seggwat.com/mcp", None),
            url_row(2, TransportKind::Http, "https://stepshots.com/mcp", None),
            url_row(3, TransportKind::Http, "https://trustmrr.com/api/mcp", None),
        ]
    }

    #[test]
    fn a_page_finds_the_endpoint_on_its_own_site() {
        // The real library: every endpoint is same-origin with its site, so
        // adding the page suggests the right entry without being told.
        assert_eq!(suggest(&library(), "https://seggwat.com", None), Some(1));
        assert_eq!(suggest(&library(), "https://trustmrr.com", None), Some(3));
    }

    #[test]
    fn a_different_host_suggests_nothing() {
        // The case the origin cannot see. Nothing is guessed; the user points
        // it at the right entry themselves.
        assert_eq!(suggest(&library(), "https://mcp.seggwat.com", None), None);
        assert_eq!(suggest(&library(), "https://example.com", None), None);
    }

    #[test]
    fn a_row_never_suggests_itself() {
        // Editing an existing row must not offer to group it with itself.
        assert_eq!(
            suggest(&library(), "https://seggwat.com/mcp", Some(1)),
            None
        );
    }

    #[test]
    fn only_group_leaders_are_offered() {
        // Accepting a suggestion must never build a chain.
        let mut rows = library();
        rows.push(url_row(
            4,
            TransportKind::WebMcp,
            "https://seggwat.com",
            Some(1),
        ));

        assert_eq!(suggest(&rows, "https://seggwat.com", None), Some(1));
    }

    #[test]
    fn a_local_command_is_not_a_site() {
        let rows = vec![ServerRow {
            id: 1,
            name: "Local".into(),
            transport_kind: TransportKind::Stdio,
            config: serde_json::json!({ "command": "npx" }),
            created_at: Utc::now(),
            last_connected_at: None,
            group_id: None,
        }];

        assert_eq!(suggest(&rows, "https://example.com", None), None);
    }
}
