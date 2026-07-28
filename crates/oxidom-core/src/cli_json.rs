//! Stable JSON schemas printed by the CLI.
//!
//! These deliberately omit subscription URLs and outbound credentials. CLI
//! output is often piped into logs, and listing a server must not disclose the
//! secret material needed to use it.

use serde::{Deserialize, Serialize};

use crate::ipc::{InterfaceInfo, ProfileEntry, SessionInfo};
use crate::model::{Server, Subscription, UserInfo};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusOutput {
    pub state: String,
    pub server: Option<ActiveServerOutput>,
    pub socks_port: u16,
    pub http_port: u16,
    pub latency_ms: Option<u32>,
    pub error: Option<String>,
    /// Local SOCKS bind address for the selected session.
    pub address: String,
}

impl StatusOutput {
    pub fn new(session: &SessionInfo, server: Option<&Server>, latency_ms: Option<u32>) -> Self {
        StatusOutput {
            state: session.state.clone(),
            server: server.map(ActiveServerOutput::from),
            socks_port: session.socks_port,
            http_port: session.http_port,
            latency_ms,
            error: session.error.clone(),
            address: session.address.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionOutput {
    pub profile: String,
    pub state: String,
    pub server_id: Option<String>,
    pub server_alias: Option<String>,
    pub server_name: Option<String>,
    pub address: String,
    pub socks_port: u16,
    pub http_port: u16,
    pub latency_ms: Option<u32>,
    pub error: Option<String>,
    pub owns_system_proxy: bool,
    pub interface: Option<InterfaceInfo>,
}

impl SessionOutput {
    pub fn new(session: &SessionInfo, latency_ms: Option<u32>) -> Self {
        Self {
            profile: session.profile.clone(),
            state: session.state.clone(),
            server_id: session.server_id.clone(),
            server_alias: session.server_alias.clone(),
            server_name: session.server_name.clone(),
            address: session.address.clone(),
            socks_port: session.socks_port,
            http_port: session.http_port,
            latency_ms,
            error: session.error.clone(),
            owns_system_proxy: session.owns_system_proxy,
            interface: session.interface.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveServerOutput {
    pub id: String,
    pub alias: Option<String>,
    pub name: String,
    pub address: String,
    pub port: u16,
    pub protocol: String,
}

impl From<&Server> for ActiveServerOutput {
    fn from(server: &Server) -> Self {
        ActiveServerOutput {
            id: server.id.clone(),
            alias: server.alias.clone(),
            name: server.name.clone(),
            address: server.address.clone(),
            port: server.port,
            protocol: server.protocol.as_str().to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerOutput {
    pub id: String,
    pub alias: Option<String>,
    pub name: String,
    pub protocol: String,
    pub address: String,
    pub port: u16,
    pub country: Option<String>,
    pub subscription_id: String,
    pub subscription: String,
}

impl ServerOutput {
    pub fn all(subscriptions: &[Subscription]) -> Vec<Self> {
        subscriptions
            .iter()
            .flat_map(|subscription| {
                subscription.servers.iter().map(|server| ServerOutput {
                    id: server.id.clone(),
                    alias: server.alias.clone(),
                    name: server.name.clone(),
                    protocol: server.protocol.as_str().to_string(),
                    address: server.address.clone(),
                    port: server.port,
                    country: server.country.clone(),
                    subscription_id: subscription.id.clone(),
                    subscription: subscription.name.clone(),
                })
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileOutput {
    pub name: String,
    pub description: String,
    pub server: String,
    pub socks_port: u16,
    pub http_port: u16,
}

impl From<&ProfileEntry> for ProfileOutput {
    fn from(profile: &ProfileEntry) -> Self {
        ProfileOutput {
            name: profile.name.clone(),
            description: profile.description.clone(),
            server: profile.server.clone(),
            socks_port: profile.socks_port,
            http_port: profile.http_port,
        }
    }
}

impl ProfileOutput {
    pub fn all(profiles: &[ProfileEntry]) -> Vec<Self> {
        profiles.iter().map(ProfileOutput::from).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionOutput {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub send_hwid: bool,
    pub server_count: usize,
    pub updated_at: Option<i64>,
    pub userinfo: Option<UserInfoOutput>,
}

impl SubscriptionOutput {
    pub fn all(subscriptions: &[Subscription]) -> Vec<Self> {
        subscriptions
            .iter()
            .map(|subscription| SubscriptionOutput {
                id: subscription.id.clone(),
                name: subscription.name.clone(),
                description: subscription.description.clone(),
                send_hwid: subscription.send_hwid,
                server_count: subscription.servers.len(),
                updated_at: subscription.updated_at,
                userinfo: subscription.userinfo.as_ref().map(UserInfoOutput::from),
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserInfoOutput {
    pub upload: u64,
    pub download: u64,
    pub total: u64,
    pub expire: Option<i64>,
}

impl From<&UserInfo> for UserInfoOutput {
    fn from(userinfo: &UserInfo) -> Self {
        UserInfoOutput {
            upload: userinfo.upload,
            download: userinfo.download,
            total: userinfo.total,
            expire: userinfo.expire,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EgressCache {
    pub server_id: String,
    pub ip: String,
    pub at_unix_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::SessionInfo;
    use crate::link::parse_link;
    use crate::model::{Subscription, UserInfo};

    #[test]
    fn status_json_shape_is_frozen() {
        let mut server =
            parse_link("trojan://secret@203.0.113.7:443#%F0%9F%87%A8%F0%9F%87%AD%20Trojan")
                .unwrap();
        server.id = "0123456789abcdef".to_string();
        server.alias = Some("ch-trojan".to_string());
        let session = SessionInfo {
            profile: "default".to_string(),
            state: "connected".to_string(),
            server_id: Some(server.id.clone()),
            server_alias: server.alias.clone(),
            server_name: Some(server.name.clone()),
            address: "127.0.0.1".to_string(),
            socks_port: 10808,
            http_port: 10809,
            ..SessionInfo::default()
        };
        let output = StatusOutput::new(&session, Some(&server), Some(84));

        assert_eq!(
            serde_json::to_string(&output).unwrap(),
            r#"{"state":"connected","server":{"id":"0123456789abcdef","alias":"ch-trojan","name":"🇨🇭 Trojan","address":"203.0.113.7","port":443,"protocol":"trojan"},"socks_port":10808,"http_port":10809,"latency_ms":84,"error":null,"address":"127.0.0.1"}"#
        );
    }

    #[test]
    fn session_list_json_shape_is_frozen() {
        let session = SessionInfo {
            profile: "work".to_string(),
            state: "connected".to_string(),
            server_id: Some("0123456789abcdef".to_string()),
            server_alias: Some("ch-trojan".to_string()),
            server_name: Some("Swiss".to_string()),
            address: "127.72.14.1".to_string(),
            socks_port: 10808,
            http_port: 10809,
            owns_system_proxy: true,
            interface: Some(InterfaceInfo {
                device: "oxi-work".to_string(),
                address: "198.18.9.7".to_string(),
                mtu: 1500,
                routes: "manual".to_string(),
                table: 28449,
                mark: 28449,
                up: true,
            }),
            ..SessionInfo::default()
        };

        assert_eq!(
            serde_json::to_string(&SessionOutput::new(&session, Some(84))).unwrap(),
            r#"{"profile":"work","state":"connected","server_id":"0123456789abcdef","server_alias":"ch-trojan","server_name":"Swiss","address":"127.72.14.1","socks_port":10808,"http_port":10809,"latency_ms":84,"error":null,"owns_system_proxy":true,"interface":{"device":"oxi-work","address":"198.18.9.7","mtu":1500,"routes":"manual","table":28449,"mark":28449,"up":true}}"#
        );
    }

    #[test]
    fn server_list_json_shape_is_frozen() {
        let mut server =
            parse_link("vless://uuid@example.com:443?security=reality#Berlin").unwrap();
        server.id = "fedcba9876543210".to_string();
        server.alias = Some("de-berlin".to_string());
        server.country = Some("de".to_string());
        let subscription = Subscription {
            id: "sub-id".to_string(),
            name: "Work".to_string(),
            url: "https://secret.example/sub".to_string(),
            description: None,
            userinfo: None,
            send_hwid: false,
            user_agent: None,
            servers: vec![server],
            updated_at: None,
        };

        assert_eq!(
            serde_json::to_string(&ServerOutput::all(&[subscription])).unwrap(),
            r#"[{"id":"fedcba9876543210","alias":"de-berlin","name":"Berlin","protocol":"vless","address":"example.com","port":443,"country":"de","subscription_id":"sub-id","subscription":"Work"}]"#
        );
    }

    #[test]
    fn profile_list_json_shape_is_frozen() {
        let profiles = [ProfileEntry {
            name: "work".to_string(),
            description: "Office".to_string(),
            server: "ch-trojan".to_string(),
            socks_port: 12080,
            http_port: 12081,
            interface: Default::default(),
        }];

        assert_eq!(
            serde_json::to_string(&ProfileOutput::all(&profiles)).unwrap(),
            r#"[{"name":"work","description":"Office","server":"ch-trojan","socks_port":12080,"http_port":12081}]"#
        );
    }

    #[test]
    fn subscription_output_never_contains_urls_or_credentials() {
        let subscription = Subscription {
            id: "sub-id".to_string(),
            name: "Provider".to_string(),
            url: "https://user:password@example.com/secret".to_string(),
            description: Some("Plan".to_string()),
            userinfo: Some(UserInfo {
                upload: 1,
                download: 2,
                total: 3,
                expire: Some(4),
            }),
            send_hwid: true,
            user_agent: Some("private override".to_string()),
            servers: Vec::new(),
            updated_at: Some(5),
        };
        let json = serde_json::to_string(&SubscriptionOutput::all(&[subscription])).unwrap();

        assert!(!json.contains("password"));
        assert!(!json.contains("private override"));
        assert!(json.contains(r#""server_count":0"#));
    }
}
