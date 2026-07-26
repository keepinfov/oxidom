#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiOperationKind {
    Connect,
    Disconnect,
    AddSubscription,
    UpdateSubscription,
    UpdateAllSubscriptions,
    DeleteSubscription,
    ImportServers,
    DeleteServer,
    ApplySettings,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiOperation {
    pub kind: UiOperationKind,
    pub subscription_id: Option<String>,
    pub server_id: Option<String>,
}

impl UiOperation {
    pub fn new(kind: UiOperationKind) -> Self {
        Self {
            kind,
            subscription_id: None,
            server_id: None,
        }
    }

    pub fn for_subscription(kind: UiOperationKind, subscription_id: impl Into<String>) -> Self {
        Self {
            kind,
            subscription_id: Some(subscription_id.into()),
            server_id: None,
        }
    }

    pub fn for_server(kind: UiOperationKind, server_id: impl Into<String>) -> Self {
        Self {
            kind,
            subscription_id: None,
            server_id: Some(server_id.into()),
        }
    }

    pub fn label(&self) -> &'static str {
        match self.kind {
            UiOperationKind::Connect => "Connecting…",
            UiOperationKind::Disconnect => "Disconnecting…",
            UiOperationKind::AddSubscription => "Fetching subscription…",
            UiOperationKind::UpdateSubscription => "Updating subscription…",
            UiOperationKind::UpdateAllSubscriptions => "Updating subscriptions…",
            UiOperationKind::DeleteSubscription => "Deleting subscription…",
            UiOperationKind::ImportServers => "Importing servers…",
            UiOperationKind::DeleteServer => "Removing server…",
            UiOperationKind::ApplySettings => "Applying settings…",
        }
    }
}
