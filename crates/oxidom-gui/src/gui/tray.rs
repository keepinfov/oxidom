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
    /// A tunnel is down and said why. Toasts do not reach a hidden window —
    /// they are recorded and dropped — so with a constant icon a tunnel that
    /// died in the background left no sign at all outside the tooltip nobody
    /// hovers.
    pub failed: bool,
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

    /// `NeedsAttention` rather than a second icon asset: it is the protocol's
    /// own word for this, and every StatusNotifier host already renders it
    /// distinctly without oxidom shipping an error variant of its logo.
    fn status(&self) -> ksni::Status {
        if self.failed {
            ksni::Status::NeedsAttention
        } else {
            ksni::Status::Active
        }
    }

    fn attention_icon_name(&self) -> String {
        "dialog-warning-symbolic".to_string()
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
