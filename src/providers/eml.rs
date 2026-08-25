//! Lecture d'un courriel joint (« pièce jointe .eml »).
//!
//! Les trois backends livrent un message joint sous forme d'octets RFC 822
//! (Graph : `itemAttachment` via `/$value`, Gmail : partie `message/rfc822`,
//! IMAP : partie imbriquée). Ce module les reconstruit en [`Message`]
//! affichable par le lecteur, sans passer par aucun provider : l'identifiant
//! est synthétique et toutes les pièces jointes internes sont embarquées,
//! puisqu'aucun backend ne saurait les servir plus tard.

use crate::model::{AccountId, Message, MessageHeader};
use crate::providers::html::extract_cids_from_html;
use crate::providers::imap::{
    collect_attachments, nested_rfc822_bytes, render_address, render_address_list, render_body,
};
use chrono::{DateTime, Utc};
use mail_parser::{MessageParser, PartType};

/// Préfixe des identifiants de message synthétiques issus d'un `.eml` joint.
/// Il ne correspond à aucun format d'identifiant provider (Graph, Gmail, ni
/// l'encodage `dossier:uid` d'IMAP — les deux-points du préfixe l'excluent) :
/// l'UI s'en sert pour ne jamais confier ces messages au runtime (persistance
/// de session, rechargements).
pub const SYNTHETIC_EML_ID_PREFIX: &str = "aviary-eml:";

/// Parse un `.eml` en [`Message`] autonome. `account_id` est le compte du
/// message porteur (routage des vues), `synthetic_id` l'identifiant d'onglet
/// construit par l'appelant (préfixé par [`SYNTHETIC_EML_ID_PREFIX`]).
pub fn parse_attached_eml(
    raw: &[u8],
    account_id: AccountId,
    synthetic_id: String,
) -> Option<Message> {
    let parsed = MessageParser::default().parse(raw)?;
    let (body, format, inline_images, raw_body) = render_body(&parsed);
    let html_cids = raw_body
        .as_deref()
        .map(extract_cids_from_html)
        .unwrap_or_default();
    let mut attachments = collect_attachments(&parsed, &html_cids);
    // Embarquer les octets de chaque pièce jointe interne : le message ne
    // vivant que dans l'onglet, il n'y a pas de chemin de récupération
    // paresseuse. L'id est vidé pour que l'UI ne tente jamais un
    // `Cmd::FetchAttachment` avec l'identifiant synthétique.
    for attachment in &mut attachments {
        let index = attachment
            .id
            .strip_prefix("part:")
            .and_then(|value| value.parse::<usize>().ok());
        attachment.id = String::new();
        let Some(part) = index.and_then(|index| parsed.attachments().nth(index)) else {
            continue;
        };
        let bytes = match &part.body {
            PartType::Binary(bytes) | PartType::InlineBinary(bytes) => bytes.to_vec(),
            PartType::Text(text) | PartType::Html(text) => text.as_bytes().to_vec(),
            PartType::Message(nested) => nested_rfc822_bytes(&parsed, part, nested).to_vec(),
            _ => continue,
        };
        attachment.size = bytes.len() as u64;
        attachment.bytes = Some(bytes);
    }
    let received = parsed
        .date()
        .and_then(|date| DateTime::from_timestamp(date.to_timestamp(), 0))
        .unwrap_or_else(Utc::now);
    let header = MessageHeader {
        id: synthetic_id,
        account_id,
        subject: parsed.subject().unwrap_or("").to_string(),
        from: render_address(parsed.from()),
        received,
        preview: parsed
            .body_text(0)
            .map(|text| text.chars().take(200).collect::<String>())
            .unwrap_or_default(),
        // Lu : le message affiché n'a pas d'état côté serveur à refléter.
        is_read: true,
        is_flagged: false,
        has_attachments: !attachments.is_empty(),
        tags: Vec::new(),
        last_action: None,
        last_action_at: None,
        // Pas de fil : un conversation_id déclencherait un Cmd::LoadThread
        // que le provider ne saurait pas résoudre.
        conversation_id: None,
        internet_message_id: parsed.message_id().map(str::to_string),
    };
    Some(Message {
        header,
        body,
        format,
        inline_images,
        attachments,
        tags: Vec::new(),
        raw_body,
        to: render_address_list(parsed.to()),
        cc: render_address_list(parsed.cc()),
        bcc: render_address_list(parsed.bcc()),
        draft_id: None,
        invitation: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AccountId;

    const OUTER_WITH_NESTED_EML: &str = "From: Contact A <a@example.com>\r\n\
To: Contact B <b@example.com>\r\n\
Subject: Message transmis\r\n\
Date: Mon, 3 Feb 2025 10:00:00 +0000\r\n\
Message-ID: <outer@example.com>\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"outer\"\r\n\
\r\n\
--outer\r\n\
Content-Type: text/plain\r\n\
\r\n\
Voir le message joint.\r\n\
--outer\r\n\
Content-Type: message/rfc822\r\n\
Content-Disposition: attachment\r\n\
\r\n\
From: Contact C <c@example.com>\r\n\
To: Contact A <a@example.com>\r\n\
Subject: Rapport interne\r\n\
Date: Sun, 2 Feb 2025 09:00:00 +0000\r\n\
Content-Type: text/plain\r\n\
\r\n\
Contenu du rapport.\r\n\
--outer--\r\n";

    #[test]
    fn attached_eml_parses_into_a_standalone_message() {
        let message = parse_attached_eml(
            OUTER_WITH_NESTED_EML.as_bytes(),
            AccountId::default(),
            "aviary-eml:test:0".to_string(),
        )
        .expect("le .eml doit se parser");
        assert_eq!(message.header.subject, "Message transmis");
        assert_eq!(message.header.id, "aviary-eml:test:0");
        assert!(message.header.conversation_id.is_none());
        assert_eq!(message.attachments.len(), 1);
        let nested = &message.attachments[0];
        assert_eq!(nested.mime, "message/rfc822");
        assert_eq!(nested.filename, "Rapport interne.eml");
        // Les octets sont embarqués (aucun provider ne peut les fournir) et
        // l'id vidé pour qu'aucune récupération paresseuse ne soit tentée.
        assert!(nested.id.is_empty());
        let bytes = nested.bytes.as_deref().expect("octets embarqués");
        let nested_parsed = parse_attached_eml(
            bytes,
            AccountId::default(),
            "aviary-eml:test:0:Rapport interne.eml".to_string(),
        )
        .expect("le .eml imbriqué doit se parser à son tour");
        assert_eq!(nested_parsed.header.subject, "Rapport interne");
        assert!(nested_parsed.body.contains("Contenu du rapport."));
    }
}
