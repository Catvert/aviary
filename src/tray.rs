use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub enum TrayAction {
    Show,
    Hide,
    Refresh,
    Quit,
}

struct AviaryTray {
    tx: mpsc::UnboundedSender<TrayAction>,
    unread: u32,
}

impl ksni::Tray for AviaryTray {
    fn id(&self) -> String {
        "aviary".into()
    }

    fn title(&self) -> String {
        if self.unread > 0 {
            format!("Aviary ({} {})", self.unread, tr!("tray-unread-suffix"))
        } else {
            tr!("app-name").to_string()
        }
    }

    fn icon_name(&self) -> String {
        if self.unread > 0 {
            "mail-unread".into()
        } else {
            "mail-read".into()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.send(TrayAction::Show);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        vec![
            StandardItem {
                label: tr!("tray-show").to_string(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send(TrayAction::Show);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: tr!("tray-hide").to_string(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send(TrayAction::Hide);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: tr!("tray-refresh").to_string(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send(TrayAction::Refresh);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: tr!("tray-quit").to_string(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send(TrayAction::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

pub struct TrayHandle {
    handle: ksni::blocking::Handle<AviaryTray>,
}

impl TrayHandle {
    pub fn refresh_i18n(&self) {
        let _ = self.handle.update(|_| {});
    }

    pub fn set_unread(&self, count: u32) {
        let _ = self.handle.update(|t| t.unread = count);
    }

    pub fn shutdown(&self) {
        let _ = self.handle.shutdown();
    }
}

pub fn spawn() -> (TrayHandle, mpsc::UnboundedReceiver<TrayAction>) {
    use ksni::blocking::TrayMethods;
    let (tx, rx) = mpsc::unbounded_channel();
    let tray = AviaryTray { tx, unread: 0 };
    let handle = tray.spawn().expect("ksni tray spawn");
    (TrayHandle { handle }, rx)
}
