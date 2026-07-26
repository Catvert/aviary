use super::{check_status, BASE};
use crate::model::MailFolder;
use anyhow::Result;
use futures::{stream, StreamExt};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;

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
    #[serde(rename = "labelListVisibility", default)]
    label_list_visibility: String,
}

#[derive(Deserialize)]
struct LabelDetails {
    #[serde(rename = "messagesTotal", default)]
    messages_total: u32,
    #[serde(rename = "messagesUnread", default)]
    messages_unread: u32,
}

/// Gmail's well-known label IDs that we surface as folders. The pair maps
/// the Gmail label ID to the Microsoft-Graph well-known alias that the
/// inbox UI already understands (lookup in `MailFolder::well_known_name`).
const WELL_KNOWN: &[(&str, &str)] = &[
    ("INBOX", "inbox"),
    ("CATEGORY_PERSONAL", "category-personal"),
    ("CATEGORY_SOCIAL", "category-social"),
    ("CATEGORY_PROMOTIONS", "category-promotions"),
    ("CATEGORY_UPDATES", "category-updates"),
    ("CATEGORY_FORUMS", "category-forums"),
    ("SENT", "sentitems"),
    ("DRAFT", "drafts"),
    ("TRASH", "deleteditems"),
    ("SPAM", "junkemail"),
];

fn well_known_name(id: &str) -> Option<&'static str> {
    WELL_KNOWN
        .iter()
        .find(|(known_id, _)| *known_id == id)
        .map(|(_, alias)| *alias)
}

fn should_surface(label: &Label) -> bool {
    if label.label_type == "system" {
        // Supported system folders are useful regardless of Gmail's sidebar
        // preference. In particular, `messageListVisibility` must not be
        // consulted here: it controls badges on messages, not the folder list.
        return well_known_name(&label.id).is_some();
    }
    !label
        .label_list_visibility
        .eq_ignore_ascii_case("labelHide")
}

pub async fn list_folders(client: &reqwest::Client, access_token: &str) -> Result<Vec<MailFolder>> {
    let url = format!("{BASE}/users/me/labels");
    let resp = client.get(&url).bearer_auth(access_token).send().await?;
    let resp = check_status(resp, "list_folders").await?;
    let list: LabelList = resp.json().await?;

    let mut keep: Vec<Label> = list.labels.into_iter().filter(should_surface).collect();
    keep.sort_by(|a, b| a.name.cmp(&b.name));

    // `labels.list` omits counters. Keep the follow-up `labels.get` fan-out
    // deliberately small: it runs alongside the first message page and Gmail
    // applies its concurrency limit to every inner batch request as well.
    let detail_ids: Vec<String> = keep.iter().map(|label| label.id.clone()).collect();
    let details: Vec<Option<LabelDetails>> = stream::iter(detail_ids)
        .map(|id| {
            let client = client.clone();
            let token = access_token.to_string();
            async move {
                let url = format!("{BASE}/users/me/labels/{id}");
                let resp = client.get(&url).bearer_auth(&token).send().await.ok()?;
                if !resp.status().is_success() {
                    return None;
                }
                resp.json::<LabelDetails>().await.ok()
            }
        })
        .buffered(3)
        .collect()
        .await;

    let ids_by_name: HashMap<String, String> = keep
        .iter()
        .map(|label| (label.name.clone(), label.id.clone()))
        .collect();
    let folders = keep
        .into_iter()
        .zip(details)
        .map(|(l, d)| {
            let well_known_name = well_known_name(&l.id).map(str::to_string);
            MailFolder {
                id: l.id,
                display_name: pretty_label_name(
                    l.name
                        .rsplit_once('/')
                        .map_or(l.name.as_str(), |(_, leaf)| leaf),
                ),
                parent_id: l
                    .name
                    .rsplit_once('/')
                    .and_then(|(parent, _)| ids_by_name.get(parent).cloned()),
                well_known_name,
                total_item_count: d.as_ref().map(|d| d.messages_total).unwrap_or(0),
                unread_item_count: d.as_ref().map(|d| d.messages_unread).unwrap_or(0),
            }
        })
        .collect();
    Ok(folders)
}

/// Create a new user label. Gmail labels are flat strings; nesting is encoded
/// by preserving the parent's full `Parent/Child` label name.
/// Returns the freshly-created label as a `MailFolder`.
pub async fn create_folder(
    client: &reqwest::Client,
    access_token: &str,
    name: &str,
    parent_id: Option<&str>,
) -> Result<MailFolder> {
    let full_name = match parent_id {
        Some(parent_id) => {
            let parent = get_label(client, access_token, parent_id).await?;
            format!("{}/{name}", parent.name)
        }
        None => name.to_string(),
    };
    let url = format!("{BASE}/users/me/labels");
    let payload = json!({
        "name": full_name,
        "labelListVisibility": "labelShow",
        "messageListVisibility": "show",
    });
    let resp = client
        .post(&url)
        .bearer_auth(access_token)
        .json(&payload)
        .send()
        .await?;
    let resp = check_status(resp, "create_folder").await?;
    #[derive(Deserialize)]
    struct Created {
        id: String,
        name: String,
    }
    let created: Created = resp.json().await?;
    Ok(MailFolder {
        id: created.id,
        display_name: pretty_label_name(
            created
                .name
                .rsplit_once('/')
                .map_or(created.name.as_str(), |(_, leaf)| leaf),
        ),
        parent_id: parent_id.map(str::to_string),
        well_known_name: None,
        total_item_count: 0,
        unread_item_count: 0,
    })
}

pub async fn rename_folder(
    client: &reqwest::Client,
    access_token: &str,
    id: &str,
    new_name: &str,
) -> Result<()> {
    let current = get_label(client, access_token, id).await?;
    let full_name = current.name.rsplit_once('/').map_or_else(
        || new_name.to_string(),
        |(parent, _)| format!("{parent}/{new_name}"),
    );
    let old_prefix = format!("{}/", current.name);
    let new_prefix = format!("{full_name}/");

    // Gmail expresses nesting solely in label names. Renaming only the
    // parent would strand `Parent/Child` as a root, so rewrite descendant
    // prefixes as part of the same runtime operation.
    let url = format!("{BASE}/users/me/labels");
    let resp = client.get(&url).bearer_auth(access_token).send().await?;
    let labels: LabelList = check_status(resp, "list_labels_for_rename")
        .await?
        .json()
        .await?;
    let mut renames = vec![(id.to_string(), full_name)];
    renames.extend(
        labels
            .labels
            .into_iter()
            .filter(|label| label.id != id && label.name.starts_with(&old_prefix))
            .map(|label| {
                (
                    label.id,
                    format!("{new_prefix}{}", &label.name[old_prefix.len()..]),
                )
            }),
    );
    for (label_id, label_name) in renames {
        let url = format!("{BASE}/users/me/labels/{label_id}");
        let resp = client
            .patch(&url)
            .bearer_auth(access_token)
            .json(&json!({ "name": label_name }))
            .send()
            .await?;
        let _ = check_status(resp, "rename_folder").await?;
    }
    Ok(())
}

async fn get_label(client: &reqwest::Client, access_token: &str, id: &str) -> Result<Label> {
    let url = format!("{BASE}/users/me/labels/{id}");
    let resp = client.get(&url).bearer_auth(access_token).send().await?;
    Ok(check_status(resp, "get_label").await?.json().await?)
}

pub async fn delete_folder(client: &reqwest::Client, access_token: &str, id: &str) -> Result<()> {
    let url = format!("{BASE}/users/me/labels/{id}");
    let resp = client.delete(&url).bearer_auth(access_token).send().await?;
    let _ = check_status(resp, "delete_folder").await?;
    Ok(())
}

fn pretty_label_name(raw: &str) -> String {
    match raw {
        "INBOX" => tr!("folder-inbox").to_string(),
        "CATEGORY_PERSONAL" => tr!("folder-category-primary").to_string(),
        "CATEGORY_SOCIAL" => tr!("folder-category-social").to_string(),
        "CATEGORY_PROMOTIONS" => tr!("folder-category-promotions").to_string(),
        "CATEGORY_UPDATES" => tr!("folder-category-updates").to_string(),
        "CATEGORY_FORUMS" => tr!("folder-category-forums").to_string(),
        "SENT" => tr!("folder-sent").to_string(),
        "DRAFT" => tr!("folder-drafts").to_string(),
        "TRASH" => tr!("folder-deleted").to_string(),
        "SPAM" => tr!("folder-junk").to_string(),
        "STARRED" => tr!("folder-starred").to_string(),
        "IMPORTANT" => tr!("folder-important").to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{should_surface, well_known_name, Label};

    fn label(id: &str, label_type: &str, visibility: &str) -> Label {
        Label {
            id: id.to_string(),
            name: id.to_string(),
            label_type: label_type.to_string(),
            label_list_visibility: visibility.to_string(),
        }
    }

    #[test]
    fn supported_system_labels_ignore_sidebar_visibility() {
        assert!(should_surface(&label("INBOX", "system", "labelHide")));
        assert!(should_surface(&label(
            "CATEGORY_PERSONAL",
            "system",
            "labelHide"
        )));
        assert_eq!(
            well_known_name("CATEGORY_PERSONAL"),
            Some("category-personal")
        );
    }

    #[test]
    fn unsupported_system_labels_stay_hidden() {
        assert!(!should_surface(&label("CHAT", "system", "labelShow")));
        assert!(!should_surface(&label("UNREAD", "system", "labelShow")));
    }

    #[test]
    fn user_labels_follow_label_list_visibility() {
        assert!(should_surface(&label("Label_1", "user", "labelShow")));
        assert!(should_surface(&label("Label_2", "user", "")));
        assert!(!should_surface(&label("Label_3", "user", "labelHide")));
    }
}
