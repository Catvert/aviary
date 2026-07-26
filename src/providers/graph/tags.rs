//! Graph implementation of the `Tag` concept — Outlook categories.
//!
//! Microsoft maintains a per-user "master list" of categories at
//! `/me/outlook/masterCategories`, each with a `displayName` and a preset
//! color (`preset0` … `preset24`). Messages reference categories by name
//! (`message.categories: string[]`), not by id, so add/remove operations
//! mutate that array via PATCH /me/messages/{id}.

use super::{from_label, patch_json, Client, GraphList, GraphRecipient, BASE};
use crate::model::{AccountId, MessageHeader, Tag};
use crate::providers::TagRename;
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use futures::{stream, StreamExt};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
struct GraphCategory {
    id: String,
    #[serde(rename = "displayName")]
    display_name: String,
    /// `preset0` .. `preset24` — Outlook's fixed palette. Stored as a
    /// string by Graph; we map it onto a packed sRGB value for the UI.
    #[serde(default)]
    color: Option<String>,
}

impl From<GraphCategory> for Tag {
    fn from(c: GraphCategory) -> Self {
        Self {
            id: c.id,
            display_name: c.display_name,
            color: c.color.as_deref().and_then(preset_to_rgb),
        }
    }
}

/// Outlook's fixed palette (`preset0` … `preset24`), as the approximate
/// sRGB values shown in Outlook on the web. The list is fixed and
/// documented at
/// <https://learn.microsoft.com/graph/api/resources/outlookcategory>.
/// Index = preset number.
pub const PRESET_PALETTE: [u32; 25] = [
    0xE7_4C_3C, // preset0 Red
    0xE6_7E_22, // preset1 Orange
    0xC0_82_3C, // preset2 Brown
    0xF1_C4_0F, // preset3 Yellow
    0x16_A0_85, // preset4 Green
    0x2E_CC_71, // preset5 Teal
    0x27_AE_60, // preset6 Olive
    0x3B_8B_8C, // preset7 Blue
    0x34_98_DB, // preset8 Purple
    0x9B_59_B6, // preset9 Cranberry
    0x95_A5_A6, // preset10 Steel
    0x7F_8C_8D, // preset11 Dark Steel
    0xBD_C3_C7, // preset12 Gray
    0x60_60_60, // preset13 Dark Gray
    0x2C_3E_50, // preset14 Black
    0xC0_39_2B, // preset15 Dark Red
    0xD3_5A_05, // preset16 Dark Orange
    0xA0_4C_00, // preset17 Dark Brown
    0xC4_9F_07, // preset18 Dark Yellow
    0x0E_85_55, // preset19 Dark Green
    0x1A_AA_85, // preset20 Dark Teal
    0x1E_8E_43, // preset21 Dark Olive
    0x21_67_82, // preset22 Dark Blue
    0x6C_3A_8E, // preset23 Dark Purple
    0x77_2A_5C, // preset24 Dark Cranberry
];

fn preset_to_rgb(preset: &str) -> Option<u32> {
    let index: usize = preset.strip_prefix("preset")?.parse().ok()?;
    PRESET_PALETTE.get(index).copied()
}

fn rgb_to_preset(rgb: u32) -> String {
    // Pick the closest preset by Euclidean distance in sRGB. Categories that
    // round-trip lose nothing; ones we typed by hand land on the nearest
    // entry. Cheap enough — 25 entries.
    let target = ((rgb >> 16) & 0xFF, (rgb >> 8) & 0xFF, rgb & 0xFF);
    let mut best = (4usize, u32::MAX);
    for (index, p) in PRESET_PALETTE.iter().enumerate() {
        let dr = (((p >> 16) & 0xFF) as i32 - target.0 as i32).pow(2);
        let dg = (((p >> 8) & 0xFF) as i32 - target.1 as i32).pow(2);
        let db = ((p & 0xFF) as i32 - target.2 as i32).pow(2);
        let dist = (dr + dg + db) as u32;
        if dist < best.1 {
            best = (index, dist);
        }
    }
    format!("preset{}", best.0)
}

pub async fn list_tags(client: &Client<'_>) -> Result<Vec<Tag>> {
    let url = format!("{BASE}/me/outlook/masterCategories");
    let resp = client.get(&url).send().await?;
    if !resp.status().is_success() {
        return Err(crate::providers::http_error(resp, "graph list categories failed").await);
    }
    let list: GraphList<GraphCategory> = resp.json().await?;
    Ok(list.value.into_iter().map(Into::into).collect())
}

/// Translate a category identifier — either the master id (GUID) or the
/// display name — into the display name that Graph uses on individual
/// messages. Callers that filter/PATCH `message.categories` need the name,
/// but the kanban stores `Tag::id` (GUID) on columns to keep rename/delete
/// addressable. Falls back to the input untouched when no match — lets a
/// freshly-typed name still go through, and keeps the error surfaced by
/// the downstream call rather than masking it here.
async fn resolve_category_name(client: &Client<'_>, input: &str) -> String {
    let Ok(tags) = list_tags(client).await else {
        return input.to_string();
    };
    if let Some(t) = tags.iter().find(|t| t.id == input) {
        return t.display_name.clone();
    }
    if tags.iter().any(|t| t.display_name == input) {
        return input.to_string();
    }
    input.to_string()
}

pub async fn create_tag(client: &Client<'_>, name: &str, color: Option<u32>) -> Result<Tag> {
    let url = format!("{BASE}/me/outlook/masterCategories");
    let preset = color
        .map(rgb_to_preset)
        .unwrap_or_else(|| "preset4".to_string());
    let resp = client
        .post(&url)
        .json(&json!({ "displayName": name, "color": preset }))
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(crate::providers::http_error(resp, "graph create category failed").await);
    }
    let cat: GraphCategory = resp.json().await?;
    Ok(cat.into())
}

pub async fn rename_tag(client: &Client<'_>, id: &str, new_name: &str) -> Result<TagRename> {
    // Graph deliberately makes outlookCategory.displayName immutable. A
    // successful PATCH containing displayName leaves the old name untouched,
    // so emulate Outlook's rename semantics by replacing the master category
    // and migrating every message that stores the old display name.
    let old = list_tags(client)
        .await?
        .into_iter()
        .find(|tag| tag.id == id || tag.display_name == id)
        .with_context(|| format!("graph category not found: {id}"))?;
    if old.display_name == new_name {
        return Ok(TagRename::default());
    }

    // Finish all reads before creating anything. If listing fails, the
    // mailbox remains untouched.
    let assignments = list_category_assignments(client, &old.display_name).await?;
    let replacement = create_tag(client, new_name, old.color).await?;

    let (migrated, migration_error) =
        migrate_category_assignments(client, assignments, &old.display_name, new_name).await;
    if let Some(error) = migration_error {
        let rollback_failures = rollback_category_rename(client, &replacement.id, &migrated).await;
        bail!(
            "graph category replacement failed: {error:#}; \
             rollback failures: {rollback_failures}"
        );
    }

    if let Err(error) = delete_tag(client, &old.id).await {
        let rollback_failures = rollback_category_rename(client, &replacement.id, &migrated).await;
        bail!(
            "graph old category deletion failed: {error:#}; \
             rollback failures: {rollback_failures}"
        );
    }

    Ok(TagRename {
        new_id: Some(replacement.id),
        message_tag_rename: Some((old.display_name, new_name.to_string())),
    })
}

#[derive(Clone, Deserialize)]
struct CategoryAssignment {
    id: String,
    #[serde(default)]
    categories: Vec<String>,
}

async fn list_category_assignments(
    client: &Client<'_>,
    category_name: &str,
) -> Result<Vec<CategoryAssignment>> {
    let escaped = category_name.replace('\'', "''");
    let filter = format!("categories/any(c:c eq '{escaped}')");
    let mut next = Some(format!("{BASE}/me/messages"));
    let mut first_page = true;
    let mut assignments = Vec::new();

    while let Some(url) = next {
        let mut request = client.get(url);
        if first_page {
            request = request.query(&[
                ("$top", "500"),
                ("$filter", filter.as_str()),
                ("$select", "id,categories"),
            ]);
            first_page = false;
        }
        let resp = request.send().await?;
        if !resp.status().is_success() {
            return Err(crate::providers::http_error(
                resp,
                "graph list category assignments failed",
            )
            .await);
        }
        let page: GraphList<CategoryAssignment> = resp.json().await?;
        assignments.extend(page.value);
        next = page.next_link;
    }

    Ok(assignments)
}

fn replace_category(categories: &mut Vec<String>, old_name: &str, new_name: &str) -> bool {
    let mut replaced = false;
    for category in categories.iter_mut() {
        if category == old_name {
            category.clear();
            category.push_str(new_name);
            replaced = true;
        }
    }
    if replaced {
        let mut seen = Vec::with_capacity(categories.len());
        categories.retain(|category| {
            if seen.iter().any(|known| known == category) {
                false
            } else {
                seen.push(category.clone());
                true
            }
        });
    }
    replaced
}

async fn migrate_category_assignments(
    client: &Client<'_>,
    assignments: Vec<CategoryAssignment>,
    old_name: &str,
    new_name: &str,
) -> (Vec<CategoryAssignment>, Option<anyhow::Error>) {
    let results = stream::iter(assignments.into_iter().map(|assignment| async move {
        let mut updated = assignment.categories.clone();
        let result = if replace_category(&mut updated, old_name, new_name) {
            set_categories(client, &assignment.id, &updated).await
        } else {
            Ok(())
        };
        (assignment, result)
    }))
    .buffer_unordered(8)
    .collect::<Vec<_>>()
    .await;

    let mut migrated = Vec::new();
    let mut first_error = None;
    for (assignment, result) in results {
        match result {
            Ok(()) => migrated.push(assignment),
            Err(error) if first_error.is_none() => first_error = Some(error),
            Err(_) => {}
        }
    }
    (migrated, first_error)
}

async fn rollback_category_rename(
    client: &Client<'_>,
    replacement_id: &str,
    migrated: &[CategoryAssignment],
) -> usize {
    let results = stream::iter(migrated.iter().cloned().map(|assignment| async move {
        set_categories(client, &assignment.id, &assignment.categories).await
    }))
    .buffer_unordered(8)
    .collect::<Vec<_>>()
    .await;
    let failures = results.iter().filter(|result| result.is_err()).count();
    if failures != 0 {
        // Some messages still reference the replacement name, so retain its
        // master-category entry instead of leaving an orphaned category.
        return failures;
    }
    usize::from(delete_tag(client, replacement_id).await.is_err())
}

/// Change a category's color. Unlike `displayName`, `color` is freely
/// updatable on the master list; messages referencing the category pick the
/// new color up immediately (they store the name only).
pub async fn set_tag_color(client: &Client<'_>, id: &str, color: u32) -> Result<()> {
    let url = format!("{BASE}/me/outlook/masterCategories/{id}");
    patch_json(
        client,
        &url,
        &json!({ "color": rgb_to_preset(color) }),
        "set category color",
    )
    .await
}

pub async fn delete_tag(client: &Client<'_>, id: &str) -> Result<()> {
    let url = format!("{BASE}/me/outlook/masterCategories/{id}");
    let resp = client.delete(&url).send().await?;
    if !resp.status().is_success() {
        return Err(crate::providers::http_error(resp, "graph delete category failed").await);
    }
    Ok(())
}

/// Add or remove a category by *display name* — that's what messages
/// actually reference. The caller may pass either the master id (GUID, what
/// `Tag::id` carries) or the display name; we normalize via
/// `resolve_category_name` first so the kanban — which stores GUIDs — and
/// the viewer's tag-picker — which stores display names — both work. We
/// GET the current array first because Graph's PATCH fully replaces it.
pub async fn add_tag_to_message(
    client: &Client<'_>,
    message_id: &str,
    tag_name: &str,
) -> Result<()> {
    let name = resolve_category_name(client, tag_name).await;
    let mut current = fetch_categories(client, message_id).await?;
    if !current.iter().any(|c| c == &name) {
        current.push(name);
    }
    set_categories(client, message_id, &current).await
}

pub async fn remove_tag_from_message(
    client: &Client<'_>,
    message_id: &str,
    tag_name: &str,
) -> Result<()> {
    let name = resolve_category_name(client, tag_name).await;
    let mut current = fetch_categories(client, message_id).await?;
    current.retain(|c| c != &name);
    set_categories(client, message_id, &current).await
}

async fn fetch_categories(client: &Client<'_>, message_id: &str) -> Result<Vec<String>> {
    let url = format!("{BASE}/me/messages/{message_id}");
    let resp = client
        .get(&url)
        .query(&[("$select", "categories")])
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(crate::providers::http_error(resp, "graph get categories failed").await);
    }
    #[derive(Deserialize)]
    struct M {
        #[serde(default)]
        categories: Vec<String>,
    }
    let m: M = resp.json().await.context("decode categories")?;
    Ok(m.categories)
}

async fn set_categories(
    client: &Client<'_>,
    message_id: &str,
    categories: &[String],
) -> Result<()> {
    let url = format!("{BASE}/me/messages/{message_id}");
    patch_json(
        client,
        &url,
        &json!({ "categories": categories }),
        "set categories",
    )
    .await
}

/// Header-shape parser, scoped to this module. Mirrors the private
/// `GraphMessage` in `messages.rs` but only decodes what `MessageHeader`
/// needs — the `$select` clause keeps the wire bytes minimal.
#[derive(Deserialize)]
struct GraphHeader {
    id: String,
    subject: Option<String>,
    from: Option<GraphRecipient>,
    #[serde(rename = "receivedDateTime")]
    received_date_time: DateTime<Utc>,
    #[serde(rename = "bodyPreview")]
    body_preview: Option<String>,
    #[serde(rename = "isRead")]
    is_read: bool,
    flag: Option<GraphFlag>,
    #[serde(default, rename = "hasAttachments")]
    has_attachments: bool,
    #[serde(default)]
    categories: Vec<String>,
}

#[derive(Deserialize)]
struct GraphFlag {
    #[serde(rename = "flagStatus")]
    flag_status: Option<String>,
}

impl From<GraphHeader> for MessageHeader {
    fn from(mut m: GraphHeader) -> Self {
        let is_flagged = m
            .flag
            .as_ref()
            .and_then(|f| f.flag_status.as_deref())
            .is_some_and(|s| s.eq_ignore_ascii_case("flagged"));
        let tags = std::mem::take(&mut m.categories);
        Self {
            id: m.id,
            account_id: AccountId::default(),
            subject: m.subject.unwrap_or_default(),
            from: from_label(m.from),
            received: m.received_date_time,
            preview: m.body_preview.unwrap_or_default(),
            is_read: m.is_read,
            is_flagged,
            has_attachments: m.has_attachments,
            tags,
            // Tag listings (kanban) do not include extended properties, so
            // the latest reply/forward verb is unavailable here.
            last_action: None,
            last_action_at: None,
            // The kanban is flat by design, so this listing pays for neither
            // `conversationId` nor `internetMessageId`.
            conversation_id: None,
            internet_message_id: None,
        }
    }
}

const HEADER_SELECT: &str =
    "id,subject,from,receivedDateTime,bodyPreview,isRead,flag,hasAttachments,categories";

/// List messages tagged with `tag_name`, newest first. Searches across the
/// user's mailbox (no folder constraint) so a kanban column gathers every
/// tagged message regardless of where it lives. Accepts either the master
/// category id (GUID) or the display name — see `resolve_category_name` for
/// why both are in play.
pub async fn list_messages_tagged(
    client: &Client<'_>,
    tag_name: &str,
    top: usize,
) -> Result<Vec<MessageHeader>> {
    let name = resolve_category_name(client, tag_name).await;
    let escaped = name.replace('\'', "''");
    let filter = format!("categories/any(c:c eq '{escaped}')");
    let top_s = top.to_string();
    let resp = client
        .get(format!("{BASE}/me/messages"))
        .query(&[
            ("$top", top_s.as_str()),
            ("$orderby", "receivedDateTime desc"),
            ("$filter", filter.as_str()),
            ("$select", HEADER_SELECT),
        ])
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(crate::providers::http_error(resp, "graph list tagged failed").await);
    }
    let list: GraphList<GraphHeader> = resp.json().await?;
    Ok(list.value.into_iter().map(Into::into).collect())
}

#[cfg(test)]
mod tests {
    use super::replace_category;

    #[test]
    fn category_replacement_preserves_order_and_removes_duplicates() {
        let mut categories = vec![
            "Étiquette A".to_string(),
            "Étiquette B".to_string(),
            "Étiquette C".to_string(),
        ];

        assert!(replace_category(
            &mut categories,
            "Étiquette A",
            "Étiquette B"
        ));
        assert_eq!(categories, vec!["Étiquette B", "Étiquette C"]);
    }

    #[test]
    fn category_replacement_ignores_non_matching_assignments() {
        let mut categories = vec!["Étiquette B".to_string()];

        assert!(!replace_category(
            &mut categories,
            "Étiquette A",
            "Étiquette C"
        ));
        assert_eq!(categories, vec!["Étiquette B"]);
    }
}
