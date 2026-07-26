//! Gmail implementation of the `Tag` concept — user labels.
//!
//! Gmail's storage is flat-labels: any user-defined label can be both a
//! "folder" (the column on the left in the web UI) and a "tag" (a
//! classification overlaid on top of normal mail). We re-use the labels
//! API but filter the listing to **user labels only** — system labels
//! (`INBOX`, `SENT`, `DRAFT`, `TRASH`, `SPAM`, …) are surfaced as folders
//! elsewhere and don't make sense as tags.
//!
//! Add/remove tag operations reuse the existing `messages/{id}/modify`
//! endpoint via `add_tag_to_message` / `remove_tag_from_message`.

use super::{check_status, BASE};
use crate::model::{AccountId, MessageHeader, Tag};
use crate::providers::TagRename;
use anyhow::Result;
use serde::Deserialize;

#[derive(Deserialize)]
struct LabelList {
    labels: Vec<Label>,
}

#[derive(Deserialize)]
struct Label {
    id: String,
    name: String,
    #[serde(rename = "type", default)]
    label_type: String,
    #[serde(default)]
    color: Option<LabelColor>,
}

#[derive(Deserialize)]
struct LabelColor {
    #[serde(rename = "backgroundColor", default)]
    background_color: Option<String>,
}

/// Colors a Gmail label can take. The API rejects arbitrary values: both
/// `backgroundColor` and `textColor` must come from the documented allowed
/// list (<https://developers.google.com/workspace/gmail/api/reference/rest/v1/users.labels>).
/// This is a curated, vivid subset of that list, mirroring the size of
/// Outlook's preset palette.
pub const LABEL_PALETTE: [u32; 20] = [
    0xFB_4C_2F, // red
    0xCC_3A_21, // dark red
    0xFF_75_37, // orange
    0xFF_AD_46, // amber
    0xEA_A0_41, // ochre
    0xF2_C9_60, // yellow
    0x16_A7_66, // green
    0x14_9E_60, // dark green
    0x44_B9_84, // emerald
    0x2D_A2_BB, // teal
    0x4A_86_E8, // blue
    0x3C_78_D8, // medium blue
    0x28_5B_AC, // dark blue
    0xA4_79_E2, // purple
    0x8E_63_CE, // deep purple
    0xE0_77_98, // pink
    0xB6_57_75, // raspberry
    0xA4_6A_21, // brown
    0x66_66_66, // gray
    0x43_43_43, // dark gray
];

fn parse_hex_color(value: &str) -> Option<u32> {
    u32::from_str_radix(value.strip_prefix('#')?, 16).ok()
}

/// List user-created labels (those usable as tags). Drops system labels
/// like INBOX/SENT/CATEGORY_*/CHAT — those serve as folders or are Gmail-
/// internal classification we don't want to expose.
pub async fn list_tags(client: &reqwest::Client, access_token: &str) -> Result<Vec<Tag>> {
    let url = format!("{BASE}/users/me/labels");
    let resp = client.get(&url).bearer_auth(access_token).send().await?;
    let resp = check_status(resp, "list_tags").await?;
    let list: LabelList = resp.json().await?;
    let mut tags: Vec<Tag> = list
        .labels
        .into_iter()
        .filter(|l| l.label_type == "user")
        .map(|l| {
            // Gmail only sets `color` on labels the user colored (web UI or
            // us via `set_tag_color`); the UI derives one from the name
            // hash otherwise.
            let color = l
                .color
                .as_ref()
                .and_then(|c| c.background_color.as_deref())
                .and_then(parse_hex_color);
            Tag {
                id: l.id,
                display_name: l.name,
                color,
            }
        })
        .collect();
    tags.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    Ok(tags)
}

pub async fn create_tag(client: &reqwest::Client, access_token: &str, name: &str) -> Result<Tag> {
    super::labels::create_folder(client, access_token, name, None)
        .await
        .map(|f| Tag {
            id: f.id,
            display_name: f.display_name,
            color: None,
        })
}

pub async fn rename_tag(
    client: &reqwest::Client,
    access_token: &str,
    id: &str,
    new_name: &str,
) -> Result<TagRename> {
    super::labels::rename_folder(client, access_token, id, new_name).await?;
    Ok(TagRename::default())
}

pub async fn delete_tag(client: &reqwest::Client, access_token: &str, id: &str) -> Result<()> {
    super::labels::delete_folder(client, access_token, id).await
}

/// Set a label's color. `color` must come from [`LABEL_PALETTE`] (Gmail
/// rejects values outside its allowed list); the text color is picked by
/// luminance — both `#ffffff` and `#000000` are in the allowed list.
pub async fn set_tag_color(
    client: &reqwest::Client,
    access_token: &str,
    id: &str,
    color: u32,
) -> Result<()> {
    let (r, g, b) = ((color >> 16) & 0xFF, (color >> 8) & 0xFF, color & 0xFF);
    let luminance = 299 * r + 587 * g + 114 * b; // out of 255_000
    let text = if luminance > 150_000 {
        "#000000"
    } else {
        "#ffffff"
    };
    let payload = serde_json::json!({
        "color": {
            "backgroundColor": format!("#{color:06x}"),
            "textColor": text,
        }
    });
    let resp = client
        .patch(format!("{BASE}/users/me/labels/{id}"))
        .bearer_auth(access_token)
        .json(&payload)
        .send()
        .await?;
    let _ = check_status(resp, "set_tag_color").await?;
    Ok(())
}

pub async fn add_tag_to_message(
    client: &reqwest::Client,
    access_token: &str,
    message_id: &str,
    tag_id: &str,
) -> Result<()> {
    let payload = serde_json::json!({ "addLabelIds": [tag_id] });
    super::messages::modify_labels_public(client, access_token, message_id, &payload).await
}

pub async fn remove_tag_from_message(
    client: &reqwest::Client,
    access_token: &str,
    message_id: &str,
    tag_id: &str,
) -> Result<()> {
    let payload = serde_json::json!({ "removeLabelIds": [tag_id] });
    super::messages::modify_labels_public(client, access_token, message_id, &payload).await
}

#[derive(Deserialize)]
struct MessagesIds {
    #[serde(default)]
    messages: Vec<MessagesId>,
}

#[derive(Deserialize)]
struct MessagesId {
    id: String,
}

/// Gmail's `messages.list` returns ids only. We fan out per-id metadata
/// fetches (parallel via `tokio::spawn`) — same pattern the folder listing
/// uses.
pub async fn list_messages_tagged(
    client: &reqwest::Client,
    access_token: &str,
    tag_id: &str,
    top: usize,
) -> Result<Vec<MessageHeader>> {
    let limit_s = top.to_string();
    let resp = client
        .get(format!("{BASE}/users/me/messages"))
        .bearer_auth(access_token)
        .query(&[("labelIds", tag_id), ("maxResults", limit_s.as_str())])
        .send()
        .await?;
    let resp = check_status(resp, "list_messages_tagged").await?;
    let body: MessagesIds = resp.json().await?;
    let ids: Vec<String> = body.messages.into_iter().map(|m| m.id).collect();
    let headers = super::messages::fetch_metadata_batch_pub(client, access_token, &ids).await?;
    let mut headers: Vec<MessageHeader> = headers
        .into_iter()
        .map(|mut h| {
            h.account_id = AccountId::default();
            h
        })
        .collect();
    headers.sort_by_key(|h| std::cmp::Reverse(h.received));
    Ok(headers)
}
