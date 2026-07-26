use crate::model::Contact;
use anyhow::Result;
use serde::Deserialize;

const PEOPLE_BASE: &str = "https://people.googleapis.com/v1";

#[derive(Deserialize)]
struct PersonList {
    #[serde(default)]
    connections: Vec<Person>,
    #[serde(default, rename = "otherContacts")]
    other_contacts: Vec<Person>,
}

#[derive(Deserialize)]
struct Person {
    #[serde(default)]
    names: Vec<NameField>,
    #[serde(rename = "emailAddresses", default)]
    email_addresses: Vec<EmailField>,
}

#[derive(Deserialize)]
struct NameField {
    #[serde(rename = "displayName", default)]
    display_name: String,
}

#[derive(Deserialize)]
struct EmailField {
    #[serde(default)]
    value: String,
}

pub async fn list_people(
    client: &reqwest::Client,
    access_token: &str,
    top: usize,
) -> Result<Vec<Contact>> {
    let top_s = top.to_string();
    // Real connections (the user's address book).
    let main_fut = client
        .get(format!("{PEOPLE_BASE}/people/me/connections"))
        .bearer_auth(access_token)
        .query(&[
            ("personFields", "names,emailAddresses"),
            ("pageSize", top_s.as_str()),
        ])
        .send();
    // "Other contacts" — auto-collected from people the user has emailed.
    // This is what gives us the recency-of-correspondence analogue Outlook
    // exposes via /me/people scoredEmailAddresses.
    let other_fut = client
        .get(format!("{PEOPLE_BASE}/otherContacts"))
        .bearer_auth(access_token)
        .query(&[
            ("readMask", "names,emailAddresses"),
            ("pageSize", top_s.as_str()),
        ])
        .send();

    let (main_resp, other_resp) = tokio::join!(main_fut, other_fut);
    let mut contacts = Vec::new();
    if let Ok(resp) = main_resp {
        if resp.status().is_success() {
            if let Ok(list) = resp.json::<PersonList>().await {
                contacts.extend(list.connections);
            }
        } else {
            return Err(crate::providers::http_error(resp, "google people failed").await);
        }
    }
    if let Ok(resp) = other_resp {
        if resp.status().is_success() {
            if let Ok(list) = resp.json::<PersonList>().await {
                contacts.extend(list.other_contacts);
            }
        }
    }

    let mut out: Vec<Contact> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for p in contacts {
        let name = p
            .names
            .first()
            .map(|n| n.display_name.clone())
            .unwrap_or_default();
        for e in p.email_addresses {
            if e.value.is_empty() {
                continue;
            }
            let key = e.value.to_lowercase();
            if !seen.insert(key) {
                continue;
            }
            out.push(Contact {
                name: name.clone(),
                email: e.value,
                score: 0.0,
            });
        }
    }
    Ok(out)
}
