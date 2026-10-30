//! Blocking rtnetlink facade for profile interfaces.

use std::future::Future;
use std::net::Ipv4Addr;

use anyhow::{Context, Result, bail};
use futures_util::TryStreamExt;
use rtnetlink::packet_route::link::LinkAttribute;
use rtnetlink::packet_route::route::{
    RouteAddress, RouteAttribute, RouteMessage, RouteProtocol, RouteScope,
};
use rtnetlink::packet_route::rule::{RuleAction, RuleAttribute, RuleMessage};
use rtnetlink::{AddressMessageBuilder, Handle, LinkUnspec, RouteMessageBuilder};

use crate::state::{InterfaceOwnership, OWNED_ROUTE_PROTOCOL, OwnedRoute, OwnedRule};
use crate::tun::plan::{Cidr, ConnectedRoute, RoutePlan, RouteSpec, Via};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultNetwork {
    pub interface: u32,
    pub gateway: Option<Ipv4Addr>,
    pub connected: Vec<ConnectedRoute>,
}

pub struct Net {
    /// Always `Some` until `Drop` takes it; see the `Drop` impl below.
    runtime: Option<tokio::runtime::Runtime>,
    handle: Handle,
}

/// Netlink application is deliberately idempotent because cleanup starts from
/// uncertain state after crashes: adds accept EEXIST, while deletes accept
/// ESRCH, ENOENT, and EADDRNOTAVAIL. Every other kernel error retains operation
/// and object context.
impl Net {
    pub fn new() -> Result<Net> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .context("creating the netlink runtime")?;
        // `new_connection` registers the netlink socket with whatever reactor is
        // current on this thread, so it has to run inside ours. Without the
        // guard a plain thread panics with "there is no reactor running", and a
        // thread that happens to be a zbus worker is worse: the socket lands on
        // the D-Bus reactor while the connection task waits on ours, and no
        // exchange ever completes.
        let (connection, handle, _) = {
            let _guard = runtime.enter();
            rtnetlink::new_connection().context("opening the rtnetlink connection")?
        };
        runtime.spawn(connection);
        Ok(Self {
            runtime: Some(runtime),
            handle,
        })
    }

    /// Drive one netlink exchange to completion.
    ///
    /// The blocking has to happen on a thread that is not already driving a
    /// runtime. The daemon answers D-Bus on zbus's tokio workers, and calling
    /// `Runtime::block_on` there aborts the worker with "Cannot start a runtime
    /// from within a runtime" — the method then never replies and the client
    /// hangs forever. A scoped plain thread is never a worker.
    fn block<T: Send>(&self, future: impl Future<Output = T> + Send) -> T {
        let runtime = self
            .runtime
            .as_ref()
            .expect("the netlink runtime outlives every exchange");
        std::thread::scope(|scope| {
            scope
                .spawn(|| runtime.block_on(future))
                .join()
                .expect("the netlink worker thread panicked")
        })
    }

    pub fn link_index(&self, name: &str) -> Result<u32> {
        self.block(async {
            let mut links = self
                .handle
                .link()
                .get()
                .match_name(name.to_string())
                .execute();
            links
                .try_next()
                .await
                .with_context(|| format!("looking up network interface {name:?}"))?
                .map(|link| link.header.index)
                .with_context(|| format!("network interface {name:?} does not exist"))
        })
    }

    pub fn link_up(&self, index: u32) -> Result<()> {
        self.set_link(
            LinkUnspec::new_with_index(index).up().build(),
            format!("bringing network interface index {index} up"),
        )
    }

    pub fn set_alias(&self, index: u32, alias: &str) -> Result<()> {
        let mut message = LinkUnspec::new_with_index(index).build();
        message
            .attributes
            .push(LinkAttribute::IfAlias(alias.into()));
        self.set_link(message, "setting exclusive TUN identity".into())
    }

    /// Name lookup is never a fallback for an absent/mismatched owner identity.
    pub fn owned_device_index(
        &self,
        name: &str,
        owner: &InterfaceOwnership,
    ) -> Result<Option<u32>> {
        owner.validate()?;
        self.block(async {
            let mut links = self.handle.link().get().execute();
            let mut found = None;
            while let Some(link) = links.try_next().await.context("checking exclusive TUN identity")? {
                if Some(link.header.index) == owner.device_index || link.attributes.iter().any(|attribute| matches!(attribute, LinkAttribute::IfName(current) if current == name)) {
                    found = Some(link);
                    break;
                }
            }
            let Some(link) = found else { return Ok(None); };
            let alias = link
                .attributes
                .iter()
                .find_map(|attribute| match attribute {
                    LinkAttribute::IfAlias(alias) => Some(alias.as_str()),
                    _ => None,
                });
            anyhow::ensure!(
                owned_device_matches(link.header.index, alias, owner)
                    && link.attributes.iter().any(|attribute| matches!(attribute, LinkAttribute::IfName(current) if current == name)),
                "refusing to touch a device whose TUN recovery identity does not match"
            );
            Ok(Some(link.header.index))
        })
    }

    pub fn delete_owned_device(&self, name: &str, owner: &InterfaceOwnership) -> Result<()> {
        if let Some(index) = self.owned_device_index(name, owner)? {
            self.block(self.handle.link().del(index).execute())
                .context("deleting exclusively owned TUN")?;
        }
        Ok(())
    }

    pub fn owned_route_add(&self, route: &OwnedRoute, owner: &InterfaceOwnership) -> Result<()> {
        let message = owned_route_message(route, owner)?;
        self.block(self.handle.route().add(message).execute())
            .context("exclusively adding owned route")
    }

    pub fn owned_route_del(&self, route: &OwnedRoute, owner: &InterfaceOwnership) -> Result<()> {
        let message = owned_route_message(route, owner)?;
        settle(
            self.block(self.handle.route().del(message).execute()),
            Change::Delete,
            "deleting ownership-qualified route".into(),
        )
    }

    pub fn owned_rule_add(&self, rule: &OwnedRule) -> Result<()> {
        let mut request = self.handle.rule().add();
        *request.message_mut() = owned_rule_message(rule)?;
        self.block(request.execute())
            .context("exclusively adding owned rule")
    }

    pub fn owned_rule_del(&self, rule: &OwnedRule) -> Result<()> {
        settle(
            self.block(self.handle.rule().del(owned_rule_message(rule)?).execute()),
            Change::Delete,
            "deleting ownership-qualified rule".into(),
        )
    }

    pub fn link_down(&self, index: u32) -> Result<()> {
        self.set_link(
            LinkUnspec::new_with_index(index).down().build(),
            format!("bringing network interface index {index} down"),
        )
    }

    pub fn set_mtu(&self, index: u32, mtu: u32) -> Result<()> {
        self.set_link(
            LinkUnspec::new_with_index(index).mtu(mtu).build(),
            format!("setting MTU {mtu} on network interface index {index}"),
        )
    }

    fn set_link(
        &self,
        message: rtnetlink::packet_route::link::LinkMessage,
        context: String,
    ) -> Result<()> {
        self.block(self.handle.link().set(message).execute())
            .with_context(|| context)
    }

    pub fn address_add(&self, index: u32, address: Ipv4Addr, prefix: u8) -> Result<()> {
        validate_prefix(prefix)?;
        let result = self.block(
            self.handle
                .address()
                .add(index, address.into(), prefix)
                .execute(),
        );
        settle(
            result,
            Change::Add,
            format!("adding address {address}/{prefix} to interface index {index}"),
        )
    }

    pub fn address_del(&self, index: u32, address: Ipv4Addr, prefix: u8) -> Result<()> {
        validate_prefix(prefix)?;
        let message = AddressMessageBuilder::<Ipv4Addr>::new()
            .index(index)
            .address(address, prefix)
            .build();
        let result = self.block(self.handle.address().del(message).execute());
        settle(
            result,
            Change::Delete,
            format!("deleting address {address}/{prefix} from interface index {index}"),
        )
    }

    pub fn route_add(&self, spec: &RouteSpec, device_index: u32) -> Result<()> {
        let message = route_message(spec, device_index);
        let result = self.block(self.handle.route().add(message).execute());
        settle(result, Change::Add, format!("adding route {spec:?}"))
    }

    /// An existing route is never evidence of ownership by this activation.
    pub fn route_add_strict(&self, spec: &RouteSpec, device_index: u32) -> Result<()> {
        let result = self.block(
            self.handle
                .route()
                .add(route_message(spec, device_index))
                .execute(),
        );
        settle(
            result,
            Change::ExclusiveAdd,
            format!("exclusively adding route {spec:?}"),
        )
    }

    /// Refuse stale private tables/rules and colliding main-table prefixes
    /// before any capture. Strict adds still close the route-add race.
    pub fn ensure_plan_available(&self, plan: &RoutePlan) -> Result<()> {
        self.ensure_plan_available_except(plan, None)
    }

    /// Recheck immediately before capture, exempting only exact qualified
    /// intents already installed by this activation. Privileged route writers
    /// must still coordinate: netlink has no atomic multi-object transaction.
    pub fn ensure_capture_available(
        &self,
        plan: &RoutePlan,
        owner: &InterfaceOwnership,
    ) -> Result<()> {
        self.ensure_plan_available_except(plan, Some(owner))
    }

    fn ensure_plan_available_except(
        &self,
        plan: &RoutePlan,
        owner: Option<&InterfaceOwnership>,
    ) -> Result<()> {
        self.block(async {
            let mut routes = self
                .handle
                .route()
                .get(RouteMessageBuilder::<Ipv4Addr>::new().build())
                .execute();
            while let Some(route) = routes
                .try_next()
                .await
                .context("checking TUN failover route ownership")?
            {
                if let Some(owner) = owner
                    && owner
                        .routes
                        .iter()
                        .any(|intent| owned_route_matches(&route, intent, owner))
                {
                    continue;
                }
                if route_table(&route) == plan.rule.table {
                    bail!("TUN failover private routing table is already in use");
                }
                for spec in &plan.system {
                    let destination = route
                        .attributes
                        .iter()
                        .find_map(|attribute| match attribute {
                            RouteAttribute::Destination(RouteAddress::Inet(address)) => {
                                Some(*address)
                            }
                            _ => None,
                        })
                        .unwrap_or(Ipv4Addr::UNSPECIFIED);
                    let prefix = spec.destination.prefix;
                    let mask = if prefix == 0 {
                        0
                    } else {
                        u32::MAX << (32 - prefix)
                    };
                    if route_table(&route) == spec.table
                        && route.header.destination_prefix_length == prefix
                        && (u32::from(destination) & mask)
                            == (u32::from(spec.destination.address) & mask)
                    {
                        bail!("TUN failover route prefix is already in use");
                    }
                }
            }
            let mut rules = self.handle.rule().get(rtnetlink::IpVersion::V4).execute();
            while let Some(rule) = rules
                .try_next()
                .await
                .context("checking TUN failover rule ownership")?
            {
                if let Some(rule_intent) = owner.and_then(|owner| owner.rule.as_ref())
                    && owned_rule_message(rule_intent).is_ok_and(|expected| {
                        expected.header.family == rule.header.family
                            && expected.header.action == rule.header.action
                            && expected.header.src_len == rule.header.src_len
                            && expected.header.dst_len == rule.header.dst_len
                            && expected.header.tos == rule.header.tos
                            && expected
                                .attributes
                                .iter()
                                .all(|attribute| rule.attributes.contains(attribute))
                    })
                {
                    continue;
                }
                if u32::from(rule.header.table) == plan.rule.table
                    || rule.attributes.iter().any(|attribute| match attribute {
                        RuleAttribute::Table(table) => *table == plan.rule.table,
                        RuleAttribute::Priority(priority) => *priority == plan.rule.priority,
                        RuleAttribute::FwMark(mark) => *mark == plan.rule.mark,
                        _ => false,
                    })
                {
                    bail!("TUN failover routing rule is already in use");
                }
            }
            Ok(())
        })
    }

    pub fn route_del(&self, spec: &RouteSpec, device_index: u32) -> Result<()> {
        // Including the known output interface avoids removing a same-prefix
        // device route owned by somebody else.
        let message = route_message(spec, device_index);
        let result = self.block(self.handle.route().del(message).execute());
        settle(result, Change::Delete, format!("deleting route {spec:?}"))
    }

    pub fn rule_add(&self, mark: u32, table: u32, priority: u32) -> Result<()> {
        let result = self.block(
            self.handle
                .rule()
                .add()
                .v4()
                .fw_mark(mark)
                .table_id(table)
                .priority(priority)
                .action(RuleAction::ToTable)
                .execute(),
        );
        settle(
            result,
            Change::Add,
            format!("adding IPv4 rule fwmark {mark:#x} table {table} priority {priority}"),
        )
    }

    pub fn rule_add_strict(&self, mark: u32, table: u32, priority: u32) -> Result<()> {
        self.block(
            self.handle
                .rule()
                .add()
                .v4()
                .fw_mark(mark)
                .table_id(table)
                .priority(priority)
                .action(RuleAction::ToTable)
                .execute(),
        )
        .context("exclusively adding TUN failover routing rule")
    }

    pub fn rule_del(&self, mark: u32, table: u32, priority: u32) -> Result<()> {
        let mut request = self
            .handle
            .rule()
            .add()
            .v4()
            .fw_mark(mark)
            .table_id(table)
            .priority(priority)
            .action(RuleAction::ToTable);
        let message = request.message_mut().clone();
        let result = self.block(self.handle.rule().del(message).execute());
        settle(
            result,
            Change::Delete,
            format!("deleting IPv4 rule fwmark {mark:#x} table {table} priority {priority}"),
        )
    }

    pub fn default_gateway(&self) -> Result<Option<Ipv4Addr>> {
        Ok(self.default_network()?.and_then(|network| network.gateway))
    }

    /// Link-scope IPv4 routes of the interface carrying main-table default.
    /// These are copied into a profile's private table so a process selected
    /// by fwmark does not lose its LAN or gateway.
    pub fn default_network(&self) -> Result<Option<DefaultNetwork>> {
        self.block(async {
            let request = RouteMessageBuilder::<Ipv4Addr>::new().build();
            let mut routes = self.handle.route().get(request).execute();
            let mut messages = Vec::new();
            while let Some(route) = routes
                .try_next()
                .await
                .context("reading IPv4 routes for the default network")?
            {
                messages.push(route);
            }
            let Some(default) = messages.iter().find(|route| {
                route.header.destination_prefix_length == 0 && route_table(route) == 254
            }) else {
                return Ok(None);
            };
            let interface = route_oif(default)
                .context("the current default IPv4 route has no output interface")?;
            let gateway = default.attributes.iter().find_map(|attribute| {
                if let RouteAttribute::Gateway(RouteAddress::Inet(address)) = attribute {
                    Some(*address)
                } else {
                    None
                }
            });
            let connected = messages
                .iter()
                .filter(|route| {
                    route_table(route) == 254
                        && route.header.scope == RouteScope::Link
                        && route.header.destination_prefix_length > 0
                        && route_oif(route) == Some(interface)
                })
                .filter_map(|route| {
                    route.attributes.iter().find_map(|attribute| {
                        if let RouteAttribute::Destination(RouteAddress::Inet(address)) = attribute
                        {
                            Some(ConnectedRoute {
                                destination: Cidr {
                                    address: *address,
                                    prefix: route.header.destination_prefix_length,
                                },
                                interface,
                            })
                        } else {
                            None
                        }
                    })
                })
                .collect();
            Ok(Some(DefaultNetwork {
                interface,
                gateway,
                connected,
            }))
        })
    }
}

fn owned_device_matches(index: u32, alias: Option<&str>, owner: &InterfaceOwnership) -> bool {
    Some(index) == owner.device_index && alias.is_some_and(|alias| alias == owner.alias())
}

/// Retire the runtime without blocking.
///
/// Dropping a `Runtime` normally waits for its threads, which tokio forbids
/// inside an async context — and the daemon builds a `Net` on a zbus worker,
/// where the plain drop panics with "Cannot drop a runtime in a context where
/// blocking is not allowed". Nothing here outlives the connection task, so
/// letting it go unawaited is exactly right.
impl Drop for Net {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

fn validate_prefix(prefix: u8) -> Result<()> {
    if prefix > 32 {
        bail!("IPv4 prefix length must be between 0 and 32, got {prefix}");
    }
    Ok(())
}

fn route_message(spec: &RouteSpec, device_index: u32) -> RouteMessage {
    let mut builder = RouteMessageBuilder::<Ipv4Addr>::new()
        .destination_prefix(spec.destination.address, spec.destination.prefix)
        .table_id(spec.table);
    builder = match spec.via {
        Via::Device => builder
            .output_interface(device_index)
            .scope(RouteScope::Link),
        Via::Interface(index) => builder.output_interface(index).scope(RouteScope::Link),
        Via::Gateway(gateway) => builder.gateway(gateway),
    };
    builder.build()
}

fn owned_route_message(route: &OwnedRoute, owner: &InterfaceOwnership) -> Result<RouteMessage> {
    owner.validate()?;
    anyhow::ensure!(
        route.output_interface != 0 && route.route.prefix <= 32,
        "invalid owned route selector"
    );
    let mut message = route_message(&route.route.to_spec(), route.output_interface);
    message.header.protocol = RouteProtocol::from(OWNED_ROUTE_PROTOCOL);
    message.attributes.retain(|attribute| {
        !matches!(
            attribute,
            RouteAttribute::Oif(_) | RouteAttribute::Priority(_)
        )
    });
    message
        .attributes
        .push(RouteAttribute::Oif(route.output_interface));
    message
        .attributes
        .push(RouteAttribute::Priority(owner.metric));
    Ok(message)
}

fn owned_route_matches(
    message: &RouteMessage,
    intent: &OwnedRoute,
    owner: &InterfaceOwnership,
) -> bool {
    let Ok(expected) = owned_route_message(intent, owner) else {
        return false;
    };
    message.header.protocol == expected.header.protocol
        && message.header.destination_prefix_length == expected.header.destination_prefix_length
        && message.header.scope == expected.header.scope
        && message.header.kind == expected.header.kind
        && route_table(message) == intent.route.table
        && message.attributes.iter().find_map(|attribute| match attribute {
            RouteAttribute::Destination(RouteAddress::Inet(address)) => Some(*address),
            _ => None,
        }).unwrap_or(Ipv4Addr::UNSPECIFIED) == intent.route.address
        && expected.attributes.iter().filter(|attribute| !matches!(attribute, RouteAttribute::Destination(RouteAddress::Inet(address)) if address.is_unspecified()))
            .all(|attribute| message.attributes.contains(attribute))
        && !message.attributes.iter().any(|attribute| matches!(attribute, RouteAttribute::MultiPath(_) | RouteAttribute::Via(_) | RouteAttribute::Encap(_) | RouteAttribute::EncapType(_)))
}

fn owned_rule_message(rule: &OwnedRule) -> Result<RuleMessage> {
    anyhow::ensure!(
        rule.mark != 0 && rule.table != 0 && (1..32766).contains(&rule.priority),
        "invalid owned rule selector"
    );
    let mut message = RuleMessage::default();
    message.header.family = rtnetlink::packet_route::AddressFamily::Inet;
    message.header.action = RuleAction::ToTable;
    message.attributes = vec![
        RuleAttribute::Table(rule.table),
        RuleAttribute::FwMark(rule.mark),
        RuleAttribute::FwMask(u32::MAX),
        RuleAttribute::Priority(rule.priority),
        RuleAttribute::Protocol(RouteProtocol::from(OWNED_ROUTE_PROTOCOL)),
    ];
    Ok(message)
}

fn route_table(route: &RouteMessage) -> u32 {
    route
        .attributes
        .iter()
        .find_map(|attribute| {
            if let RouteAttribute::Table(table) = attribute {
                Some(*table)
            } else {
                None
            }
        })
        .unwrap_or(u32::from(route.header.table))
}

fn route_oif(route: &RouteMessage) -> Option<u32> {
    route.attributes.iter().find_map(|attribute| {
        if let RouteAttribute::Oif(index) = attribute {
            Some(*index)
        } else {
            None
        }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    Add,
    ExclusiveAdd,
    Delete,
}

fn settle(result: Result<(), rtnetlink::Error>, change: Change, context: String) -> Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(error) if netlink_errno(&error).is_some_and(|errno| ignored_errno(change, errno)) => {
            Ok(())
        }
        Err(error) => Err(error).with_context(|| context),
    }
}

fn netlink_errno(error: &rtnetlink::Error) -> Option<i32> {
    match error {
        rtnetlink::Error::NetlinkError(message) => Some(message.raw_code().abs()),
        _ => None,
    }
}

fn ignored_errno(change: Change, errno: i32) -> bool {
    match change {
        Change::Add => errno == libc::EEXIST,
        Change::ExclusiveAdd => false,
        Change::Delete => matches!(errno, libc::ESRCH | libc::ENOENT | libc::EADDRNOTAVAIL),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_messages_preserve_every_nonwildcard_recovery_selector() -> Result<()> {
        let owner = InterfaceOwnership {
            cookie: "0123456789abcdef0123456789abcdef".into(),
            metric: 1234567,
            gateway_interface: Some(7),
            device_index: Some(42),
            routes: Vec::new(),
            rule: None,
            cleanup_dirty: false,
        };
        let route = OwnedRoute {
            route: crate::state::RouteRecord {
                address: Ipv4Addr::new(192, 0, 2, 10),
                prefix: 32,
                table: 254,
                gateway: Some(Ipv4Addr::new(192, 0, 2, 1)),
                interface: None,
            },
            output_interface: 7,
        };
        let message = owned_route_message(&route, &owner)?;
        assert!(owned_route_matches(&message, &route, &owner));
        let mut raced = message.clone();
        raced
            .attributes
            .retain(|attribute| !matches!(attribute, RouteAttribute::Priority(_)));
        raced
            .attributes
            .push(RouteAttribute::Priority(owner.metric + 1));
        assert!(
            !owned_route_matches(&raced, &route, &owner),
            "a same-prefix different-metric route is not exempt from capture preflight"
        );
        assert_eq!(u8::from(message.header.protocol), OWNED_ROUTE_PROTOCOL);
        assert_eq!(
            message.attributes,
            vec![
                RouteAttribute::Destination(RouteAddress::Inet(Ipv4Addr::new(192, 0, 2, 10))),
                RouteAttribute::Gateway(RouteAddress::Inet(Ipv4Addr::new(192, 0, 2, 1))),
                RouteAttribute::Oif(7),
                RouteAttribute::Priority(1234567)
            ]
        );
        let rule = OwnedRule {
            mark: 2147483649,
            table: 2147483649,
            priority: 12000,
        };
        assert_eq!(
            owned_rule_message(&rule)?.attributes,
            vec![
                RuleAttribute::Table(2147483649),
                RuleAttribute::FwMark(2147483649),
                RuleAttribute::FwMask(u32::MAX),
                RuleAttribute::Priority(12000),
                RuleAttribute::Protocol(RouteProtocol::from(OWNED_ROUTE_PROTOCOL))
            ]
        );
        assert!(owned_device_matches(42, Some(&owner.alias()), &owner));
        assert!(!owned_device_matches(43, Some(&owner.alias()), &owner));
        assert!(!owned_device_matches(42, Some("unrelated-owner"), &owner));
        assert!(!owned_device_matches(42, None, &owner));
        let mut invalid = owner.clone();
        invalid.metric = 0;
        assert!(owned_route_message(&route, &invalid).is_err());
        Ok(())
    }
    #[test]
    fn only_contractual_errnos_are_idempotent() {
        assert!(super::ignored_errno(super::Change::Add, libc::EEXIST));
        assert!(!super::ignored_errno(super::Change::Add, libc::EPERM));
        for errno in [libc::ESRCH, libc::ENOENT, libc::EADDRNOTAVAIL] {
            assert!(super::ignored_errno(super::Change::Delete, errno));
        }
        assert!(!super::ignored_errno(super::Change::Delete, libc::EPERM));
    }

    #[test]
    fn exclusive_route_additions_never_claim_existing_kernel_objects() {
        for errno in [libc::EEXIST, libc::EPERM, libc::ESRCH, libc::ENOENT] {
            assert!(!super::ignored_errno(super::Change::ExclusiveAdd, errno));
        }
        assert!(super::settle(Ok(()), super::Change::ExclusiveAdd, "exclusive add".into()).is_ok());
    }
}
