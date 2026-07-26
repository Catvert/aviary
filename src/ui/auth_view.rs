//! Authentication screens: provider selection, device code,
//! Microsoft, redirection Google, formulaire IMAP.

use super::app::AviaryApp;
use super::state::AuthState;
use crate::auth::{ImapConfig, NetSecurity};
use crate::runtime::Cmd;
use gpui::{div, prelude::*, px, ClipboardItem, Context, Entity, Window};
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex,
    input::{Input, InputState},
    spinner::Spinner,
    v_flex, ActiveTheme, IconName, Sizable, StyledExt,
};

/// Input entities for the IMAP form (the equivalent of `ImapFormState`).
pub struct ImapFormUi {
    pub email: Entity<InputState>,
    pub display_name: Entity<InputState>,
    pub imap_host: Entity<InputState>,
    pub imap_port: Entity<InputState>,
    pub imap_security: NetSecurity,
    pub imap_username: Entity<InputState>,
    pub smtp_host: Entity<InputState>,
    pub smtp_port: Entity<InputState>,
    pub smtp_security: NetSecurity,
    pub smtp_username: Entity<InputState>,
    pub password: Entity<InputState>,
}

impl ImapFormUi {
    fn new(window: &mut Window, cx: &mut Context<AviaryApp>) -> Self {
        let mk = |window: &mut Window, cx: &mut Context<AviaryApp>, placeholder: &str| {
            let p = placeholder.to_string();
            cx.new(|cx| InputState::new(window, cx).placeholder(p))
        };
        Self {
            email: mk(window, cx, &tr!("login-imap-form-email-hint")),
            display_name: mk(window, cx, &tr!("login-imap-display-name-placeholder")),
            imap_host: mk(window, cx, &tr!("login-imap-form-imap-host-hint")),
            imap_port: { cx.new(|cx| InputState::new(window, cx).default_value("993")) },
            imap_security: NetSecurity::Tls,
            imap_username: mk(window, cx, &tr!("login-imap-username-placeholder")),
            smtp_host: mk(window, cx, &tr!("login-imap-form-smtp-host-hint")),
            smtp_port: { cx.new(|cx| InputState::new(window, cx).default_value("465")) },
            smtp_security: NetSecurity::Tls,
            smtp_username: mk(window, cx, &tr!("login-smtp-username-placeholder")),
            password: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(tr!("login-imap-form-password-hint"))
                    .masked(true)
            }),
        }
    }
}

fn security_label(s: NetSecurity) -> gpui::SharedString {
    match s {
        NetSecurity::Plain => tr!("security-plain"),
        NetSecurity::StartTls => tr!("security-starttls"),
        NetSecurity::Tls => tr!("security-tls"),
    }
}

impl AviaryApp {
    pub fn open_imap_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.imap_form = Some(ImapFormUi::new(window, cx));
        cx.notify();
    }

    fn submit_imap_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(form) = &self.imap_form else { return };
        let read = |e: &Entity<InputState>| e.read(cx).value().trim().to_string();
        let email = read(&form.email);
        let password = form.password.read(cx).value().to_string();
        let imap_username = {
            let u = read(&form.imap_username);
            if u.is_empty() {
                email.clone()
            } else {
                u
            }
        };
        let smtp_username = {
            let u = read(&form.smtp_username);
            if u.is_empty() {
                imap_username.clone()
            } else {
                u
            }
        };
        let config = ImapConfig {
            email: email.clone(),
            display_name: read(&form.display_name),
            imap_host: read(&form.imap_host),
            imap_port: read(&form.imap_port).parse().unwrap_or(993),
            imap_security: form.imap_security,
            imap_username,
            smtp_host: read(&form.smtp_host),
            smtp_port: read(&form.smtp_port).parse().unwrap_or(465),
            smtp_security: form.smtp_security,
            smtp_username,
        };
        if config.email.is_empty() || config.imap_host.is_empty() || password.is_empty() {
            self.notify_error(tr!("login-imap-required-fields"), window, cx);
            return;
        }
        self.auth = AuthState::AwaitingImap { email };
        self.imap_form = None;
        self.send(Cmd::StartImapLogin { config, password });
        cx.notify();
    }

    pub fn render_auth_view(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // The form can be opened from either the initial login screen (`Idle`)
        // or Settings while another account is already authenticated. It must
        // therefore take precedence over `AuthState`.
        if self.imap_form.is_some() {
            return div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(self.render_imap_form(window, cx))
                .into_any_element();
        }

        let card = v_flex()
            .gap_4()
            .p_8()
            .w(px(460.))
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .rounded(cx.theme().radius_lg);

        let card = match &self.auth {
            AuthState::Idle => card
                .child(div().text_xl().font_bold().child(tr!("login-welcome")))
                .child(
                    div()
                        .text_color(cx.theme().muted_foreground)
                        .child(tr!("login-connect-prompt")),
                )
                .child(
                    Button::new("ms")
                        .primary()
                        .label(tr!("login-connect-microsoft"))
                        .icon(IconName::CircleUser)
                        .on_click(cx.listener(|this, _, _, cx| this.start_microsoft_login(cx))),
                )
                .child(
                    Button::new("google")
                        .label(tr!("login-connect-gmail"))
                        .icon(super::icons::app_icon("mail"))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.start_google_login(window, cx)),
                        ),
                )
                .child(
                    Button::new("imap")
                        .label(tr!("login-connect-imap"))
                        .icon(super::icons::app_icon("building"))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.open_imap_form(window, cx)),
                        ),
                ),
            AuthState::StartingMicrosoft => card
                .child(
                    div()
                        .text_xl()
                        .font_bold()
                        .child(tr!("login-microsoft-title")),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(Spinner::new().small())
                        .child(
                            div()
                                .text_color(cx.theme().muted_foreground)
                                .child(tr!("login-microsoft-starting")),
                        ),
                ),
            AuthState::AwaitingCode {
                user_code,
                verification_uri,
            } => {
                let code = user_code.clone();
                let uri = verification_uri.clone();
                card.child(
                    div()
                        .text_xl()
                        .font_bold()
                        .child(tr!("login-microsoft-title")),
                )
                .child(div().child(tr!("login-microsoft-code-prompt")))
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .font_family("JetBrains Mono")
                                .text_2xl()
                                .font_bold()
                                .child(code.clone()),
                        )
                        .child(
                            Button::new("copy-code")
                                .ghost()
                                .small()
                                .icon(IconName::Copy)
                                .on_click({
                                    let code = code.clone();
                                    move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            code.clone(),
                                        ));
                                    }
                                }),
                        ),
                )
                .child(
                    Button::new("open-uri")
                        .primary()
                        .label(tr!("login-microsoft-open"))
                        .icon(IconName::ExternalLink)
                        .on_click({
                            let uri = uri.clone();
                            move |_, _, _| {
                                let _ = open::that(&uri);
                            }
                        }),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(Spinner::new().small())
                        .child(
                            div()
                                .text_color(cx.theme().muted_foreground)
                                .child(tr!("login-microsoft-waiting")),
                        ),
                )
                .child(
                    Button::new("cancel")
                        .ghost()
                        .label(tr!("cancel"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.auth = if this.accounts.is_empty() {
                                AuthState::Idle
                            } else {
                                AuthState::Authenticated
                            };
                            cx.notify();
                        })),
                )
            }
            AuthState::AwaitingGoogle { auth_url } => {
                let url = auth_url.clone();
                card.child(
                    div()
                        .text_xl()
                        .font_bold()
                        .child(tr!("login-google-window-title")),
                )
                .child(div().child(tr!("login-google-fallback")))
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("open-google")
                                .primary()
                                .label(tr!("login-google-open"))
                                .icon(IconName::ExternalLink)
                                .on_click({
                                    let url = url.clone();
                                    move |_, _, _| {
                                        let _ = open::that(&url);
                                    }
                                }),
                        )
                        .child(
                            Button::new("copy-url")
                                .ghost()
                                .icon(IconName::Copy)
                                .on_click({
                                    let url = url.clone();
                                    move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            url.clone(),
                                        ));
                                    }
                                }),
                        ),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(Spinner::new().small())
                        .child(
                            div()
                                .text_color(cx.theme().muted_foreground)
                                .child(tr!("login-google-waiting")),
                        ),
                )
                .child(
                    Button::new("cancel")
                        .ghost()
                        .label(tr!("cancel"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.auth = if this.accounts.is_empty() {
                                AuthState::Idle
                            } else {
                                AuthState::Authenticated
                            };
                            cx.notify();
                        })),
                )
            }
            AuthState::AwaitingImap { email } => card
                .child(div().text_xl().font_bold().child(tr!("login-imap-title")))
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(Spinner::new().small())
                        .child(tr!("login-imap-testing", { email: email })),
                ),
            AuthState::Authenticated => card.child(tr!("status-connected")),
        };

        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(card)
            .into_any_element()
    }

    fn render_imap_form(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let form = self.imap_form.as_ref().expect("formulaire IMAP ouvert");
        let muted = cx.theme().muted_foreground;
        let field = move |label: &str, input: &Entity<InputState>| {
            v_flex()
                .gap_1()
                .flex_1()
                .child(div().text_sm().text_color(muted).child(label.to_string()))
                .child(Input::new(input))
        };
        let sec_button =
            |id: &'static str, current: NetSecurity, which_imap: bool, cx: &mut Context<Self>| {
                Button::new(id)
                    .ghost()
                    .small()
                    .label(security_label(current))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(form) = &mut this.imap_form {
                            let slot = if which_imap {
                                &mut form.imap_security
                            } else {
                                &mut form.smtp_security
                            };
                            *slot = match *slot {
                                NetSecurity::Tls => NetSecurity::StartTls,
                                NetSecurity::StartTls => NetSecurity::Plain,
                                NetSecurity::Plain => NetSecurity::Tls,
                            };
                            cx.notify();
                        }
                    }))
            };

        v_flex()
            .gap_3()
            .p_6()
            .w(px(560.))
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .rounded(cx.theme().radius_lg)
            .child(
                div()
                    .text_xl()
                    .font_bold()
                    .child(tr!("login-imap-form-title")),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(field(&tr!("login-imap-email-label"), &form.email))
                    .child(field(
                        &tr!("login-imap-display-name-label"),
                        &form.display_name,
                    )),
            )
            .child(
                div()
                    .font_semibold()
                    .child(tr!("login-imap-form-imap-section")),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_end()
                    .child(field(&tr!("login-imap-server-label"), &form.imap_host))
                    .child(
                        div()
                            .w(px(90.))
                            .child(field(&tr!("login-imap-port-label"), &form.imap_port)),
                    )
                    .child(sec_button("imap-sec", form.imap_security, true, cx)),
            )
            .child(field(&tr!("login-imap-user-label"), &form.imap_username))
            .child(
                div()
                    .font_semibold()
                    .child(tr!("login-imap-form-smtp-section")),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_end()
                    .child(field(&tr!("login-imap-server-label"), &form.smtp_host))
                    .child(
                        div()
                            .w(px(90.))
                            .child(field(&tr!("login-imap-port-label"), &form.smtp_port)),
                    )
                    .child(sec_button("smtp-sec", form.smtp_security, false, cx)),
            )
            .child(field(&tr!("login-smtp-user-label"), &form.smtp_username))
            .child(field(&tr!("login-imap-form-password"), &form.password))
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("cancel-imap")
                            .ghost()
                            .label(tr!("cancel"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.imap_form = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("submit-imap")
                            .primary()
                            .label(tr!("login-imap-submit"))
                            .on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.submit_imap_form(window, cx)
                                }),
                            ),
                    ),
            )
    }
}
