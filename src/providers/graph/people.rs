use super::{Client, GraphList, BASE};
use crate::model::Contact;
use anyhow::Result;
use serde::Deserialize;

#[derive(Deserialize)]
struct GraphPerson {
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    #[serde(rename = "scoredEmailAddresses")]
    scored_email_addresses: Option<Vec<GraphScoredEmail>>,
}

#[derive(Deserialize)]
struct GraphScoredEmail {
    address: Option<String>,
    #[serde(rename = "relevanceScore")]
    relevance_score: Option<f32>,
}

pub async fn list_people(client: &Client<'_>, top: usize) -> Result<Vec<Contact>> {
    let top_str = top.to_string();
    let url = format!("{BASE}/me/people");
    let resp = client
        .get(&url)
        .query(&[
            ("$top", top_str.as_str()),
            ("$select", "displayName,scoredEmailAddresses"),
        ])
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(crate::providers::http_error(resp, "graph people failed").await);
    }
    let list: GraphList<GraphPerson> = resp.json().await?;
    let mut out: Vec<Contact> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for p in list.value {
        let name = p.display_name.clone().unwrap_or_default();
        let Some(emails) = p.scored_email_addresses else {
            continue;
        };
        for e in emails {
            let Some(addr) = e.address else { continue };
            if addr.is_empty() {
                continue;
            }
            let key = addr.to_lowercase();
            if !seen.insert(key) {
                continue;
            }
            out.push(Contact {
                name: name.clone(),
                email: addr,
                score: e.relevance_score.unwrap_or(0.0),
            });
        }
    }
    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(out)
}
