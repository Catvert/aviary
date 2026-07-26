//! Outlook-style contact completion in message bodies.
//!
//! Typing `@` at a token boundary searches the shared address book. Accepting
//! a suggestion inserts the display name and adds the contact to the To field.

use super::super::addresses::{mention_name, AddressBook, RecipientInput};
use super::super::components::block_input::{BlockCompletionItem, BlockCompletionProvider};
use gpui::Entity;
use std::rc::Rc;

const MAX_SUGGESTIONS: usize = 8;
const MAX_QUERY_BYTES: usize = 160;

pub(super) fn completion_provider(
    address_book: AddressBook,
    recipient: Entity<RecipientInput>,
) -> BlockCompletionProvider {
    Rc::new(move |source, offset| {
        let mut offset = offset.min(source.len());
        while !source.is_char_boundary(offset) {
            offset = offset.saturating_sub(1);
        }
        let Some((start, query)) = active_mention(&source[..offset]) else {
            return Vec::new();
        };

        address_book
            .search(query, MAX_SUGGESTIONS)
            .into_iter()
            .map(|contact| {
                let name = mention_name(&contact);
                let detail = tr!("compose-contact-completion-detail", {
                    name: &name,
                    email: &contact.email
                });
                let replacement = format!("@{name} ");
                let recipient = recipient.clone();
                let mentioned = contact.clone();
                BlockCompletionItem {
                    range: start..offset,
                    label: "@".into(),
                    detail,
                    replacement: replacement.into(),
                    on_accept: Some(Rc::new(move |cx| {
                        recipient.update(cx, |input, cx| {
                            input.add_mentioned_contact(&mentioned, cx);
                        });
                    })),
                }
            })
            .collect()
    })
}

fn active_mention(before_cursor: &str) -> Option<(usize, &str)> {
    let start = before_cursor.rfind('@')?;
    if start > 0 {
        let previous = before_cursor[..start].chars().next_back()?;
        if previous.is_alphanumeric() || matches!(previous, '_' | '.' | '-' | '+') {
            return None;
        }
    }
    let query = &before_cursor[start + 1..];
    if query.len() > MAX_QUERY_BYTES
        || query.contains(['\n', '\r', '@'])
        || query.starts_with(char::is_whitespace)
    {
        return None;
    }
    Some((start, query))
}

#[cfg(test)]
mod tests {
    use super::active_mention;

    #[test]
    fn mention_opens_at_a_text_boundary_and_supports_full_names() {
        assert_eq!(active_mention("@Contact A"), Some((0, "Contact A")));
        assert_eq!(active_mention("Bonjour @Contact A"), Some((8, "Contact A")));
    }

    #[test]
    fn mention_does_not_open_inside_an_email_address() {
        assert_eq!(active_mention("contact@example.test"), None);
        assert_eq!(active_mention("prefix-@Contact"), None);
    }

    #[test]
    fn mention_stops_after_a_newline_or_another_at_sign() {
        assert_eq!(active_mention("@Contact\nsuite"), None);
        assert_eq!(active_mention("@Contact @Other"), Some((9, "Other")));
    }
}
