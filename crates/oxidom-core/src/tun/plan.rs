//! Pure route planning, separate from privileged netlink application.

use std::fmt::{Display, Formatter};
use std::net::Ipv4Addr;
use std::str::FromStr;

use anyhow::{Context, Result, bail};

use crate::profile::RouteMode;

/// An IPv4 network and prefix length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    pub address: Ipv4Addr,
    pub prefix: u8,
}

impl FromStr for Cidr {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        let (address, prefix) = match value.split_once('/') {
            Some((address, prefix)) => {
                let prefix = prefix
                    .parse::<u8>()
                    .with_context(|| format!("invalid IPv4 CIDR prefix in {value:?}"))?;
                (address, prefix)
            }
            None => (value, 32),
        };
        if prefix > 32 {
            bail!("IPv4 CIDR prefix must be between 0 and 32 in {value:?}");
        }
        let address = address
            .parse::<Ipv4Addr>()
            .with_context(|| format!("invalid IPv4 address in CIDR {value:?}"))?;
        Ok(Self { address, prefix })
    }
}

impl Display for Cidr {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}/{}", self.address, self.prefix)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// The profile TUN, whose current ifindex is supplied at application time.
    Device,
    /// An existing system interface, captured while reading connected routes.
    Interface(u32),
    Gateway(Ipv4Addr),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteSpec {
    pub destination: Cidr,
    pub via: Via,
    /// 254 is Linux's main routing table.
    pub table: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleSpec {
    pub mark: u32,
    pub table: u32,
    pub priority: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectedRoute {
    pub destination: Cidr,
    pub interface: u32,
}

pub struct PlanInput<'a> {
    pub table: u32,
    pub mark: u32,
    pub mode: RouteMode,
    pub list: &'a [Cidr],
    pub server_address: Option<Ipv4Addr>,
    pub default_gateway: Option<Ipv4Addr>,
    /// Link-scope routes of the interface carrying the system default. They
    /// keep LAN hosts reachable after a cgroup mark selects the private table.
    pub connected: &'a [ConnectedRoute],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutePlan {
    pub private: Vec<RouteSpec>,
    pub system: Vec<RouteSpec>,
    pub rule: RuleSpec,
}

pub fn plan_routes(input: &PlanInput<'_>) -> Result<RoutePlan> {
    let mut private = input
        .connected
        .iter()
        .map(|route| RouteSpec {
            destination: route.destination,
            via: Via::Interface(route.interface),
            table: input.table,
        })
        .collect::<Vec<_>>();
    private.push(RouteSpec {
        destination: cidr(Ipv4Addr::UNSPECIFIED, 0),
        via: Via::Device,
        table: input.table,
    });
    let rule = RuleSpec {
        mark: input.mark,
        table: input.table,
        priority: input.mark,
    };
    let system = match input.mode {
        RouteMode::Manual => Vec::new(),
        RouteMode::List => {
            if input.list.is_empty() {
                bail!("routes = \"list\" requires at least one [interface] list CIDR");
            }
            input
                .list
                .iter()
                .copied()
                .map(|destination| RouteSpec {
                    destination,
                    via: Via::Device,
                    table: 254,
                })
                .collect()
        }
        RouteMode::Default => {
            let server = input
                .server_address
                .context("routes = \"default\" requires the server IPv4 address")?;
            let gateway = input
                .default_gateway
                .context("routes = \"default\" requires the current default IPv4 gateway")?;
            vec![
                RouteSpec {
                    destination: cidr(server, 32),
                    via: Via::Gateway(gateway),
                    table: 254,
                },
                RouteSpec {
                    destination: cidr(Ipv4Addr::UNSPECIFIED, 1),
                    via: Via::Device,
                    table: 254,
                },
                RouteSpec {
                    destination: cidr(Ipv4Addr::new(128, 0, 0, 0), 1),
                    via: Via::Device,
                    table: 254,
                },
            ]
        }
    };

    Ok(RoutePlan {
        private,
        system,
        rule,
    })
}

const fn cidr(address: Ipv4Addr, prefix: u8) -> Cidr {
    Cidr { address, prefix }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONNECTED: [ConnectedRoute; 1] = [ConnectedRoute {
        destination: Cidr {
            address: Ipv4Addr::new(192, 0, 2, 0),
            prefix: 24,
        },
        interface: 7,
    }];

    fn input(mode: RouteMode) -> PlanInput<'static> {
        PlanInput {
            table: 0x6f21,
            mark: 0x6f21,
            mode,
            list: &[],
            server_address: Some(Ipv4Addr::new(203, 0, 113, 7)),
            default_gateway: Some(Ipv4Addr::new(192, 0, 2, 1)),
            connected: &CONNECTED,
        }
    }

    fn common_is_present(plan: &RoutePlan) {
        assert_eq!(
            plan.private,
            [
                RouteSpec {
                    destination: cidr(Ipv4Addr::new(192, 0, 2, 0), 24),
                    via: Via::Interface(7),
                    table: 0x6f21,
                },
                RouteSpec {
                    destination: cidr(Ipv4Addr::UNSPECIFIED, 0),
                    via: Via::Device,
                    table: 0x6f21,
                },
            ]
        );
        assert_eq!(
            plan.rule,
            RuleSpec {
                mark: 0x6f21,
                table: 0x6f21,
                priority: 0x6f21,
            }
        );
    }

    #[test]
    fn manual_only_has_the_private_route_and_rule() {
        let plan = plan_routes(&input(RouteMode::Manual)).unwrap();
        common_is_present(&plan);
        assert!(plan.system.is_empty());
    }

    #[test]
    fn connected_routes_precede_the_private_default_and_keep_their_interfaces() {
        let connected = [
            ConnectedRoute {
                destination: cidr(Ipv4Addr::new(192, 168, 1, 0), 24),
                interface: 4,
            },
            ConnectedRoute {
                destination: cidr(Ipv4Addr::new(169, 254, 0, 0), 16),
                interface: 4,
            },
        ];
        let mut input = input(RouteMode::Manual);
        input.connected = &connected;

        let plan = plan_routes(&input).unwrap();

        assert_eq!(
            plan.private,
            [
                RouteSpec {
                    destination: connected[0].destination,
                    via: Via::Interface(4),
                    table: 0x6f21,
                },
                RouteSpec {
                    destination: connected[1].destination,
                    via: Via::Interface(4),
                    table: 0x6f21,
                },
                RouteSpec {
                    destination: cidr(Ipv4Addr::UNSPECIFIED, 0),
                    via: Via::Device,
                    table: 0x6f21,
                },
            ]
        );
    }

    #[test]
    fn list_preserves_cidr_order() {
        let list = [
            "10.0.0.0/8".parse().unwrap(),
            "192.168.0.0/16".parse().unwrap(),
        ];
        let mut input = input(RouteMode::List);
        input.list = &list;
        let plan = plan_routes(&input).unwrap();
        common_is_present(&plan);
        assert_eq!(
            plan.system
                .iter()
                .map(|route| route.destination)
                .collect::<Vec<_>>(),
            list
        );
        assert!(
            plan.system
                .iter()
                .all(|route| route.via == Via::Device && route.table == 254)
        );
    }

    #[test]
    fn empty_list_is_rejected() {
        assert!(
            plan_routes(&input(RouteMode::List))
                .unwrap_err()
                .to_string()
                .contains("at least one")
        );
    }

    #[test]
    fn default_route_plan_has_the_required_order() {
        let plan = plan_routes(&input(RouteMode::Default)).unwrap();
        common_is_present(&plan);
        assert_eq!(
            plan.system,
            [
                RouteSpec {
                    destination: cidr(Ipv4Addr::new(203, 0, 113, 7), 32),
                    via: Via::Gateway(Ipv4Addr::new(192, 0, 2, 1)),
                    table: 254,
                },
                RouteSpec {
                    destination: cidr(Ipv4Addr::UNSPECIFIED, 1),
                    via: Via::Device,
                    table: 254,
                },
                RouteSpec {
                    destination: cidr(Ipv4Addr::new(128, 0, 0, 0), 1),
                    via: Via::Device,
                    table: 254,
                },
            ]
        );
    }

    #[test]
    fn default_requires_server_and_gateway_separately() {
        let mut missing_server = input(RouteMode::Default);
        missing_server.server_address = None;
        let error = plan_routes(&missing_server).unwrap_err().to_string();
        assert!(error.contains("server IPv4 address"), "{error}");

        let mut missing_gateway = input(RouteMode::Default);
        missing_gateway.default_gateway = None;
        let error = plan_routes(&missing_gateway).unwrap_err().to_string();
        assert!(error.contains("default IPv4 gateway"), "{error}");
    }

    #[test]
    fn cidr_parsing_and_display_are_strictly_ipv4() {
        assert_eq!(
            "10.0.0.0/8".parse::<Cidr>().unwrap(),
            cidr(Ipv4Addr::new(10, 0, 0, 0), 8)
        );
        assert_eq!(
            "1.2.3.4".parse::<Cidr>().unwrap(),
            cidr(Ipv4Addr::new(1, 2, 3, 4), 32)
        );
        assert_eq!(
            cidr(Ipv4Addr::new(1, 2, 3, 4), 32).to_string(),
            "1.2.3.4/32"
        );
        for invalid in ["1.2.3.4/33", "garbage", "2001:db8::1/64"] {
            assert!(invalid.parse::<Cidr>().is_err(), "{invalid:?}");
        }
    }
}
