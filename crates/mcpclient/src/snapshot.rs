//! Turning a live session into a [`schemadiff::Snapshot`].

use std::collections::BTreeMap;
use std::sync::Arc;

use rmcp::model::{ErrorData, PaginatedRequestParams, Prompt, Resource, ServerPeerInfo, Tool};
use rmcp::service::{Peer, RoleClient, ServiceError};
use schemadiff::Snapshot;
use serde::Serialize;
use serde_json::Value;

use crate::{Error, Result};

pub(crate) async fn collect(
    peer: &Peer<RoleClient>,
    info: Option<Arc<ServerPeerInfo>>,
) -> Result<Snapshot> {
    let info = info.ok_or_else(|| Error::Connect("the server completed no handshake".into()))?;
    let capabilities = &info.capabilities;

    // Only ask for what the server said it has. Calling `resources/list` on a
    // server without the capability is an error, not an empty list, and would
    // otherwise fail the whole snapshot.
    let (tools, resources, prompts) = tokio::join!(
        maybe(capabilities.tools.is_some(), all_tools(peer)),
        maybe(capabilities.resources.is_some(), all_resources(peer)),
        maybe(capabilities.prompts.is_some(), all_prompts(peer)),
    );

    Ok(Snapshot {
        protocol_version: as_string(&info.protocol_version),
        // Optional: a discovery response is not required to identify itself.
        server_name: info
            .server_info
            .as_ref()
            .map(|i| i.name.clone())
            .unwrap_or_default(),
        server_version: info
            .server_info
            .as_ref()
            .map(|i| i.version.clone())
            .unwrap_or_default(),
        capabilities: to_value(capabilities),
        tools: keyed(tools?, |t| t.name.to_string()),
        resources: keyed(resources?, |r| r.uri.clone()),
        prompts: keyed(prompts?, |p| p.name.clone()),
    })
}

/// Pages followed per list before giving up. rmcp's `list_all_*` follow
/// `next_cursor` for as long as the server hands one out, so a server that
/// always does would keep a hosted caller listing until memory ran out.
const MAX_PAGES: usize = 100;

/// Items kept per list, across every page.
const MAX_ITEMS: usize = 10_000;

/// Bytes kept per list, across every page, as the items serialize. One page
/// is already capped on a hosted connection, but a hundred of them are not.
const MAX_LIST_BYTES: usize = 16 * 1024 * 1024;

/// Follow `next_cursor` the way rmcp's `list_all_*` do, under both caps.
async fn paged<T, F, Fut>(mut page: F) -> std::result::Result<Vec<T>, ServiceError>
where
    T: Serialize,
    F: FnMut(PaginatedRequestParams) -> Fut,
    Fut: Future<Output = std::result::Result<(Vec<T>, Option<String>), ServiceError>>,
{
    let mut items = Vec::new();
    let mut bytes = 0;
    let mut cursor = None;
    for _ in 0..MAX_PAGES {
        let (batch, next) = page(PaginatedRequestParams::default().with_cursor(cursor)).await?;
        bytes += serde_json::to_vec(&batch).map_or(0, |b| b.len());
        items.extend(batch);
        if items.len() > MAX_ITEMS || bytes > MAX_LIST_BYTES {
            break;
        }
        match next {
            None => return Ok(items),
            next => cursor = next,
        }
    }
    Err(ServiceError::McpError(ErrorData::internal_error(
        format!(
            "the server listed more than {MAX_ITEMS} items, {} MiB or {MAX_PAGES} pages",
            MAX_LIST_BYTES / (1024 * 1024)
        ),
        None,
    )))
}

pub(crate) async fn all_tools(
    peer: &Peer<RoleClient>,
) -> std::result::Result<Vec<Tool>, ServiceError> {
    paged(|params| async move {
        let page = peer.list_tools(Some(params)).await?;
        Ok((page.tools, page.next_cursor))
    })
    .await
}

pub(crate) async fn all_resources(
    peer: &Peer<RoleClient>,
) -> std::result::Result<Vec<Resource>, ServiceError> {
    paged(|params| async move {
        let page = peer.list_resources(Some(params)).await?;
        Ok((page.resources, page.next_cursor))
    })
    .await
}

pub(crate) async fn all_prompts(
    peer: &Peer<RoleClient>,
) -> std::result::Result<Vec<Prompt>, ServiceError> {
    paged(|params| async move {
        let page = peer.list_prompts(Some(params)).await?;
        Ok((page.prompts, page.next_cursor))
    })
    .await
}

/// Await `fut` only when `supported`, otherwise yield an empty list.
async fn maybe<T>(
    supported: bool,
    fut: impl Future<Output = std::result::Result<Vec<T>, rmcp::service::ServiceError>>,
) -> Result<Vec<T>> {
    if !supported {
        return Ok(Vec::new());
    }
    fut.await.map_err(|e| Error::Request(e.to_string()))
}

/// Index items by their identity, serialising each one whole.
///
/// Keeping the full object rather than a typed subset means a field added by a
/// future MCP revision still shows up in a diff instead of being dropped.
fn keyed<T: Serialize>(items: Vec<T>, key: impl Fn(&T) -> String) -> BTreeMap<String, Value> {
    items
        .into_iter()
        .map(|item| (key(&item), to_value(&item)))
        .collect()
}

fn to_value<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn as_string<T: Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

#[cfg(test)]
mod paging_tests {
    use super::*;

    /// A server that always has another page of `item`.
    async fn endless(item: String) -> std::result::Result<Vec<String>, ServiceError> {
        paged(|_| {
            let item = item.clone();
            async move { Ok((vec![item], Some("more".to_string()))) }
        })
        .await
    }

    #[tokio::test]
    async fn an_endless_cursor_stops_at_the_page_cap() {
        assert!(endless("x".into()).await.is_err());
    }

    #[tokio::test]
    async fn large_pages_stop_at_the_byte_cap_before_the_page_cap() {
        let mut pages = 0;
        let result = paged(|_| {
            pages += 1;
            async { Ok((vec!["x".repeat(1024 * 1024)], Some("more".to_string()))) }
        })
        .await;
        assert!(result.is_err());
        assert!(pages <= 17, "{pages}");
    }

    #[tokio::test]
    async fn a_finite_list_is_returned_whole() {
        let mut left = 3;
        let items = paged(|_| {
            left -= 1;
            let next = (left > 0).then(|| "more".to_string());
            async move { Ok((vec![1], next)) }
        })
        .await
        .unwrap();
        assert_eq!(items, [1, 1, 1]);
    }
}
