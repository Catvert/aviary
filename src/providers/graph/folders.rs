use super::{Client, GraphList, BASE};
use crate::model::MailFolder;
use anyhow::Result;
use serde::Deserialize;
use serde_json::json;
use std::collections::{HashMap, VecDeque};

#[derive(Deserialize)]
struct GraphMailFolder {
    id: String,
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    #[serde(rename = "parentFolderId")]
    parent_id: Option<String>,
    #[serde(rename = "childFolderCount", default)]
    child_folder_count: u32,
    #[serde(rename = "totalItemCount")]
    total_item_count: Option<u32>,
    #[serde(rename = "unreadItemCount")]
    unread_item_count: Option<u32>,
}

impl From<GraphMailFolder> for MailFolder {
    fn from(f: GraphMailFolder) -> Self {
        Self {
            id: f.id,
            display_name: f.display_name.unwrap_or_default(),
            parent_id: f.parent_id,
            well_known_name: None,
            total_item_count: f.total_item_count.unwrap_or(0),
            unread_item_count: f.unread_item_count.unwrap_or(0),
        }
    }
}

// `mailFolder` in Graph v1.0 has no `wellKnownName` field — well-known
// folders are addressable by alias in the URL but the resource itself
// only exposes `id` + `displayName`. We resolve each alias to its real
// `id` by GETting `/me/mailFolders/{alias}` in parallel, then label
// matching folders in the list.
const WELL_KNOWN: &[&str] = &[
    "inbox",
    "drafts",
    "sentitems",
    "deleteditems",
    "junkemail",
    "outbox",
    "archive",
];

pub async fn list_folders(client: &Client<'_>) -> Result<Vec<MailFolder>> {
    let root_url = format!("{BASE}/me/mailFolders");

    #[derive(Deserialize)]
    struct IdOnly {
        id: String,
    }
    let well_known_handles: Vec<_> = WELL_KNOWN
        .iter()
        .map(|name| {
            let url = format!("{BASE}/me/mailFolders/{name}");
            client.get(&url).query(&[("$select", "id")]).send()
        })
        .collect();
    let mut well_known_resolved: Vec<(&'static str, String)> = Vec::new();
    for (name, fut) in WELL_KNOWN.iter().zip(well_known_handles) {
        if let Ok(resp) = fut.await {
            if resp.status().is_success() {
                if let Ok(f) = resp.json::<IdOnly>().await {
                    well_known_resolved.push((*name, f.id));
                }
            }
        }
    }

    // Graph only returns direct children from `/mailFolders`. Walk every
    // collection breadth-first so arbitrarily deep user folder trees are
    // represented in Aviary instead of being silently flattened away.
    let mut pending = VecDeque::from([root_url]);
    let mut graph_folders = Vec::new();
    while let Some(url) = pending.pop_front() {
        let collection = list_folder_collection(client, &url).await?;
        for folder in &collection {
            if folder.child_folder_count > 0 {
                pending.push_back(format!("{BASE}/me/mailFolders/{}/childFolders", folder.id));
            }
        }
        graph_folders.extend(collection);
    }
    let mut folders: Vec<MailFolder> = graph_folders.into_iter().map(Into::into).collect();

    let by_id: HashMap<String, &'static str> = well_known_resolved
        .into_iter()
        .map(|(name, id)| (id, name))
        .collect();
    for f in &mut folders {
        if let Some(name) = by_id.get(&f.id) {
            f.well_known_name = Some((*name).to_string());
            // System folders remain top-level navigation entries even when
            // Graph reports a technical parent.
            f.parent_id = None;
        }
    }
    Ok(folders)
}

async fn list_folder_collection(
    client: &Client<'_>,
    initial_url: &str,
) -> Result<Vec<GraphMailFolder>> {
    let mut next = Some(initial_url.to_string());
    let mut first = true;
    let mut folders = Vec::new();
    while let Some(url) = next.take() {
        let mut request = client.get(&url);
        if first {
            request = request.query(&[
                ("$top", "100"),
                ("includeHiddenFolders", "false"),
                (
                    "$select",
                    "id,displayName,parentFolderId,childFolderCount,totalItemCount,unreadItemCount",
                ),
            ]);
            first = false;
        }
        let resp = request.send().await?;
        if !resp.status().is_success() {
            return Err(crate::providers::http_error(resp, "graph list folders failed").await);
        }
        let page: GraphList<GraphMailFolder> = resp.json().await?;
        folders.extend(page.value);
        next = page.next_link;
    }
    Ok(folders)
}

/// Create a new mail folder. Top-level when `parent_id` is `None`, otherwise
/// a child of the given folder. Graph rejects empty/whitespace names with
/// 400; the call site validates before dispatching.
pub async fn create_folder(
    client: &Client<'_>,
    name: &str,
    parent_id: Option<&str>,
) -> Result<MailFolder> {
    let url = match parent_id {
        Some(p) => format!("{BASE}/me/mailFolders/{p}/childFolders"),
        None => format!("{BASE}/me/mailFolders"),
    };
    let resp = client
        .post(&url)
        .json(&json!({ "displayName": name }))
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(crate::providers::http_error(resp, "graph create folder failed").await);
    }
    let f: GraphMailFolder = resp.json().await?;
    let mut folder: MailFolder = f.into();
    folder.parent_id = parent_id.map(str::to_string);
    Ok(folder)
}

pub async fn rename_folder(client: &Client<'_>, id: &str, new_name: &str) -> Result<()> {
    let url = format!("{BASE}/me/mailFolders/{id}");
    let resp = client
        .patch(&url)
        .json(&json!({ "displayName": new_name }))
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(crate::providers::http_error(resp, "graph rename folder failed").await);
    }
    Ok(())
}

pub async fn delete_folder(client: &Client<'_>, id: &str) -> Result<()> {
    let url = format!("{BASE}/me/mailFolders/{id}");
    let resp = client.delete(&url).send().await?;
    if !resp.status().is_success() {
        return Err(crate::providers::http_error(resp, "graph delete folder failed").await);
    }
    Ok(())
}
