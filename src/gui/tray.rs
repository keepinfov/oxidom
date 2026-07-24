//! StatusNotifier tray icon. Lives in the GUI process; menu clicks are
//! forwarded to the GTK main loop through a channel drained by the tick.

use std::sync::mpsc::Sender;

use ksni::menu::{MenuItem, StandardItem};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    ShowWindow,
    Disconnect,
    Quit,
}

pub struct OxidomTray {
    pub connected: bool,
    pub status_text: String,
    pub commands: Sender<TrayCommand>,
}

impl ksni::Tray for OxidomTray {
    fn id(&self) -> String {
        crate::APP_ID.to_string()
    }

    fn title(&self) -> String {
        "oxidom".to_string()
    }

    fn icon_name(&self) -> String {
        crate::APP_ID.to_string()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "oxidom".to_string(),
            description: self.status_text.clone(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.commands.send(TrayCommand::ShowWindow);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: "Show oxidom".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.commands.send(TrayCommand::ShowWindow);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Disconnect".to_string(),
                enabled: self.connected,
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.commands.send(TrayCommand::Disconnect);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.commands.send(TrayCommand::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}
