use crate::model::{MessageHeader, MessageRef};
use notify_rust::NotificationResponse;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy)]
pub enum NotificationActionKind {
    Open,
    MarkRead,
    Archive,
    Reply,
    ShowInbox,
}

#[derive(Debug, Clone)]
pub struct NotificationAction {
    pub message: Option<MessageRef>,
    pub kind: NotificationActionKind,
}

pub type NotificationActionSender = mpsc::UnboundedSender<NotificationAction>;
pub type NotificationActionReceiver = mpsc::UnboundedReceiver<NotificationAction>;

/// L'action « clic sur le corps de la notification » de la spec freedesktop.
/// Un serveur n'émet `ActionInvoked("default")` que si le client a **déclaré**
/// cette action : sans elle, cliquer la notification ne fait rien, et la branche
/// `NotificationResponse::Default` ci-dessous n'est jamais atteinte. Les serveurs
/// conformes ne l'affichent pas comme bouton, d'où le bouton explicite qui suit.
const DEFAULT_ACTION: &str = "default";

pub fn channel() -> (NotificationActionSender, NotificationActionReceiver) {
    mpsc::unbounded_channel()
}

pub fn new_message(h: &MessageHeader, tx: NotificationActionSender) {
    let summary = tr!("notify-new-prefix", { subject: first_line(&h.subject, 60) });
    let body = first_line(&h.from, 80);
    let reference = MessageRef {
        account_id: h.account_id.clone(),
        id: h.id.clone(),
    };
    show(
        &summary,
        &body,
        &[
            (DEFAULT_ACTION, tr!("notify-action-open").to_string()),
            ("open", tr!("notify-action-open").to_string()),
            ("mark-read", tr!("notify-action-mark-read").to_string()),
            ("archive", tr!("notify-action-archive").to_string()),
            ("reply", tr!("notify-action-reply").to_string()),
        ],
        move |response| {
            let kind = match response {
                NotificationResponse::Default => Some(NotificationActionKind::Open),
                NotificationResponse::Action(action) if action == "open" => {
                    Some(NotificationActionKind::Open)
                }
                NotificationResponse::Action(action) if action == "mark-read" => {
                    Some(NotificationActionKind::MarkRead)
                }
                NotificationResponse::Action(action) if action == "archive" => {
                    Some(NotificationActionKind::Archive)
                }
                NotificationResponse::Action(action) if action == "reply" => {
                    Some(NotificationActionKind::Reply)
                }
                NotificationResponse::Action(_)
                | NotificationResponse::Reply(_)
                | NotificationResponse::Closed(_) => None,
            };
            if let Some(kind) = kind {
                let _ = tx.send(NotificationAction {
                    message: Some(reference),
                    kind,
                });
            }
        },
    );
}

pub fn new_message_aggregated(count: usize, tx: NotificationActionSender) {
    show(
        &tr!("notify-new-aggregated", { count: count }),
        &tr!("app-name"),
        &[
            (DEFAULT_ACTION, tr!("notify-action-show-inbox").to_string()),
            ("show-inbox", tr!("notify-action-show-inbox").to_string()),
        ],
        move |response| {
            let opens = match response {
                NotificationResponse::Default => true,
                NotificationResponse::Action(action) => action == "show-inbox",
                NotificationResponse::Reply(_) | NotificationResponse::Closed(_) => false,
            };
            if opens {
                let _ = tx.send(NotificationAction {
                    message: None,
                    kind: NotificationActionKind::ShowInbox,
                });
            }
        },
    );
}

fn show(
    summary: &str,
    body: &str,
    actions: &[(&str, String)],
    on_response: impl FnOnce(&NotificationResponse) + Send + 'static,
) {
    let mut notification = notify_rust::Notification::new();
    notification
        .summary(summary)
        .body(body)
        .icon("mail-unread")
        .timeout(notify_rust::Timeout::Default);
    for (id, label) in actions {
        notification.action(id, label);
    }
    match notification.show() {
        Ok(handle) => {
            std::thread::Builder::new()
                .name("notification-action".into())
                .spawn(move || {
                    if let Err(error) = handle.wait_for_response(on_response) {
                        log::warn!("waiting for notification action failed: {error}");
                    }
                })
                .ok();
        }
        Err(error) => log::warn!("notification failed: {error}"),
    }
}

fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("");
    if line.chars().count() <= max {
        line.to_string()
    } else {
        let head: String = line.chars().take(max).collect();
        format!("{head}…")
    }
}
