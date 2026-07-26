//! Keyboard settings and shortcut reference.

use super::super::app::AviaryApp;
use gpui::{div, prelude::*, px, AnyElement, Context, Keystroke};
use gpui_component::{h_flex, kbd::Kbd, switch::Switch, v_flex, ActiveTheme};

impl AviaryApp {
    pub(super) fn render_settings_keyboard(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let vim_enabled = self.settings.global.vim_keybindings;
        let mut standard = v_flex().gap_1();
        for (label, bindings) in [
            (tr!("shortcut-new-message"), &["secondary-n"][..]),
            (tr!("shortcut-refresh"), &["f5"][..]),
            (tr!("shortcut-search"), &["secondary-f"][..]),
            (
                tr!("shortcut-views"),
                &[
                    "secondary-1",
                    "secondary-2",
                    "secondary-3",
                    "secondary-4",
                    "secondary-5",
                ][..],
            ),
            (tr!("shortcut-next-previous"), &["up", "down"][..]),
            (tr!("shortcut-first-last"), &["home", "end"][..]),
            (tr!("shortcut-reply"), &["secondary-r"][..]),
            (tr!("shortcut-reply-all"), &["secondary-shift-r"][..]),
            (tr!("shortcut-forward"), &["secondary-shift-f"][..]),
            (tr!("shortcut-archive"), &["secondary-e"][..]),
            (tr!("shortcut-delete"), &["delete"][..]),
            (tr!("shortcut-close"), &["secondary-w"][..]),
            (tr!("shortcut-save-draft"), &["secondary-s"][..]),
            (tr!("shortcut-send"), &["secondary-enter"][..]),
        ] {
            standard = standard.child(self.shortcut_row(label.to_string(), bindings, cx));
        }

        let mut vim = v_flex().gap_1();
        for (label, bindings) in [
            (tr!("shortcut-next-previous"), &["k", "j"][..]),
            (tr!("shortcut-first-last"), &["g g", "shift-g"][..]),
            (tr!("shortcut-previous-next-view"), &["h", "l"][..]),
            (tr!("shortcut-new-message"), &["c"][..]),
            (tr!("shortcut-search"), &["/"][..]),
            (tr!("shortcut-reply"), &["r"][..]),
            (tr!("shortcut-reply-all"), &["shift-r"][..]),
            (tr!("shortcut-forward"), &["f"][..]),
            (tr!("shortcut-quick-actions"), &["a"][..]),
            (tr!("shortcut-archive"), &["e"][..]),
            (tr!("shortcut-delete"), &["d"][..]),
            (tr!("shortcut-toggle-flag"), &["s"][..]),
            (tr!("shortcut-mark-unread"), &["u"][..]),
            (tr!("shortcut-close"), &["q"][..]),
        ] {
            vim = vim.child(self.shortcut_row(label.to_string(), bindings, cx));
        }

        v_flex()
            .gap_4()
            .child(
                self.section(&tr!("settings-keyboard-standard"), cx)
                    .child(standard),
            )
            .child(
                self.section(&tr!("settings-keyboard-vim"), cx)
                    .child(
                        Switch::new("vim-keybindings")
                            .checked(vim_enabled)
                            .label(tr!("settings-keyboard-vim-enable"))
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.settings.global.vim_keybindings = *checked;
                                this.settings.save();
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(tr!("settings-keyboard-vim-help")),
                    )
                    .child(vim),
            )
    }

    fn shortcut_row(&self, label: String, bindings: &[&str], cx: &Context<Self>) -> AnyElement {
        let mut keys = h_flex()
            .flex_1()
            .min_w_0()
            .flex_wrap()
            .justify_end()
            .gap_1();
        for (index, binding) in bindings.iter().enumerate() {
            if index > 0 {
                keys = keys.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(tr!("shortcut-or")),
                );
            }
            for stroke in binding.split_whitespace() {
                let stroke = Keystroke::parse(stroke).expect("valid built-in shortcut");
                keys = keys.child(Kbd::new(stroke));
            }
        }
        h_flex()
            .w_full()
            .min_w_0()
            .items_start()
            .justify_between()
            .gap_3()
            .py_1()
            .child(div().w(px(250.)).flex_none().text_sm().child(label))
            .child(keys)
            .into_any_element()
    }
}
