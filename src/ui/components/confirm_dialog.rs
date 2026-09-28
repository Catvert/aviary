//! Confirmation dialogs with labels in the interface language.
//!
//! `AlertDialog::confirm` draws OK and Cancel, but labels them from
//! gpui-component's own catalog, which has no French: a French interface got
//! an English "Cancel". Every confirmation goes through `confirmation` so the
//! two buttons speak Aviary's language; a site that needs a more specific OK
//! (`ok_text`) sets it after.

use gpui_kit::component::dialog::AlertDialog;

pub(crate) trait ConfirmationDialog {
    /// OK and Cancel buttons, labelled from Aviary's catalogs.
    fn confirmation(self) -> Self;
}

impl ConfirmationDialog for AlertDialog {
    fn confirmation(self) -> Self {
        self.confirm().ok_text(tr!("ok")).cancel_text(tr!("cancel"))
    }
}
