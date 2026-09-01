#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiOperationKind {
    Connect,
    Disconnect,
    AddSubscription,
    UpdateSubscription,
    UpdateAllSubscriptions,
    DeleteSubscription,
    ImportServers,
    CreateServer,
    DeleteServer,
    ApplySettings,
    SaveProfile,
    RemoveProfile,
    UpProfile,
    DownProfile,
    FindGeoAssets,
    InstallGeoAssets,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiOperation {
    pub kind: UiOperationKind,
    pub subscription_id: Option<String>,
    pub server_id: Option<String>,
    pub profile: Option<String>,
}

impl UiOperation {
    pub fn new(kind: UiOperationKind) -> Self {
        Self {
            kind,
            subscription_id: None,
            server_id: None,
            profile: None,
        }
    }

    pub fn for_subscription(kind: UiOperationKind, subscription_id: impl Into<String>) -> Self {
        Self {
            kind,
            subscription_id: Some(subscription_id.into()),
            server_id: None,
            profile: None,
        }
    }

    pub fn for_server(kind: UiOperationKind, server_id: impl Into<String>) -> Self {
        Self {
            kind,
            subscription_id: None,
            server_id: Some(server_id.into()),
            profile: None,
        }
    }

    pub fn for_profile(kind: UiOperationKind, profile: impl Into<String>) -> Self {
        Self {
            kind,
            subscription_id: None,
            server_id: None,
            profile: Some(profile.into()),
        }
    }

    pub fn label(&self) -> &'static str {
        match self.kind {
            UiOperationKind::Connect => "Connecting…",
            UiOperationKind::Disconnect => "Disconnecting…",
            UiOperationKind::AddSubscription => "Fetching subscription…",
            UiOperationKind::UpdateSubscription => "Updating subscription…",
            UiOperationKind::UpdateAllSubscriptions => "Updating subscriptions…",
            UiOperationKind::DeleteSubscription => "Removing subscription…",
            UiOperationKind::ImportServers => "Importing servers…",
            UiOperationKind::CreateServer => "Creating server…",
            UiOperationKind::DeleteServer => "Removing server…",
            UiOperationKind::ApplySettings => "Applying settings…",
            UiOperationKind::SaveProfile => "Saving profile…",
            UiOperationKind::RemoveProfile => "Removing profile…",
            UiOperationKind::UpProfile => "Connecting…",
            UiOperationKind::DownProfile => "Disconnecting…",
            // The download itself is not here: it runs on the daemon and
            // reports through the poll, so it must not hold the single
            // operation slot -- Cancel has to stay clickable throughout.
            UiOperationKind::FindGeoAssets => "Looking for geo data…",
            UiOperationKind::InstallGeoAssets => "Installing geo data…",
        }
    }
}
