//! StatusNotifier tray icon. Lives in the GUI process; menu clicks are
//! forwarded to the GTK main loop through a channel drained by the tick.

use std::sync::mpsc::Sender;

use ksni::menu::{CheckmarkItem, MenuItem, StandardItem};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayCommand {
    ShowWindow,
    Toggle(String),
    Quit,
}

pub struct OxidomTray {
    pub sessions: Vec<(String, bool)>,
    pub status_text: String,
    pub commands: Sender<TrayCommand>,
}

impl ksni::Tray for OxidomTray {
    fn id(&self) -> String {
        oxidom_core::APP_ID.to_string()
    }

    fn title(&self) -> String {
        "oxidom".to_string()
    }

    fn icon_name(&self) -> String {
        oxidom_core::APP_ID.to_string()
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
        let mut menu = self
            .sessions
            .iter()
            .map(|(profile, running)| {
                let profile = profile.clone();
                CheckmarkItem {
                    label: profile.clone(),
                    checked: *running,
                    activate: Box::new(move |tray: &mut Self| {
                        let _ = tray.commands.send(TrayCommand::Toggle(profile.clone()));
                    }),
                    ..Default::default()
                }
                .into()
            })
            .collect::<Vec<_>>();
        // Nothing above it to separate from when no profile exists yet.
        if !menu.is_empty() {
            menu.push(MenuItem::Separator);
        }
        menu.extend([
            StandardItem {
                label: "Show oxidom".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.commands.send(TrayCommand::ShowWindow);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Quit".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.commands.send(TrayCommand::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]);
        menu
    }
}
