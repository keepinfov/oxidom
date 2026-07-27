//! Blocking rtnetlink facade for profile interfaces.

use std::net::Ipv4Addr;

use anyhow::{Context, Result, bail};
use futures_util::TryStreamExt;
use rtnetlink::packet_route::route::RouteScope;
use rtnetlink::packet_route::route::{RouteAddress, RouteAttribute, RouteMessage};
use rtnetlink::packet_route::rule::RuleAction;
use rtnetlink::{AddressMessageBuilder, Handle, LinkUnspec, RouteMessageBuilder};

use crate::tun::plan::{RouteSpec, Via};

pub struct Net {
    runtime: tokio::runtime::Runtime,
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
        let (connection, handle, _) =
            rtnetlink::new_connection().context("opening the rtnetlink connection")?;
        runtime.spawn(connection);
        Ok(Self { runtime, handle })
    }

    pub fn link_index(&self, name: &str) -> Result<u32> {
        self.runtime.block_on(async {
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
        self.runtime
            .block_on(self.handle.link().set(message).execute())
            .with_context(|| context)
    }

    pub fn address_add(&self, index: u32, address: Ipv4Addr, prefix: u8) -> Result<()> {
        validate_prefix(prefix)?;
        let result = self.runtime.block_on(
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
        let result = self
            .runtime
            .block_on(self.handle.address().del(message).execute());
        settle(
            result,
            Change::Delete,
            format!("deleting address {address}/{prefix} from interface index {index}"),
        )
    }

    pub fn route_add(&self, spec: &RouteSpec, device_index: u32) -> Result<()> {
        let message = route_message(spec, device_index);
        let result = self
            .runtime
            .block_on(self.handle.route().add(message).execute());
        settle(result, Change::Add, format!("adding route {spec:?}"))
    }

    pub fn route_del(&self, spec: &RouteSpec, device_index: u32) -> Result<()> {
        // Including the known output interface avoids removing a same-prefix
        // device route owned by somebody else.
        let message = route_message(spec, device_index);
        let result = self
            .runtime
            .block_on(self.handle.route().del(message).execute());
        settle(result, Change::Delete, format!("deleting route {spec:?}"))
    }

    pub fn rule_add(&self, mark: u32, table: u32, priority: u32) -> Result<()> {
        let result = self.runtime.block_on(
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
        let result = self
            .runtime
            .block_on(self.handle.rule().del(message).execute());
        settle(
            result,
            Change::Delete,
            format!("deleting IPv4 rule fwmark {mark:#x} table {table} priority {priority}"),
        )
    }

    pub fn default_gateway(&self) -> Result<Option<Ipv4Addr>> {
        self.runtime.block_on(async {
            let request = RouteMessageBuilder::<Ipv4Addr>::new().build();
            let mut routes = self.handle.route().get(request).execute();
            while let Some(route) = routes
                .try_next()
                .await
                .context("reading IPv4 routes for the default gateway")?
            {
                if route.header.destination_prefix_length != 0 || route_table(&route) != 254 {
                    continue;
                }
                if let Some(gateway) = route.attributes.iter().find_map(|attribute| {
                    if let RouteAttribute::Gateway(RouteAddress::Inet(address)) = attribute {
                        Some(*address)
                    } else {
                        None
                    }
                }) {
                    return Ok(Some(gateway));
                }
            }
            Ok(None)
        })
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
        Via::Gateway(gateway) => builder.gateway(gateway),
    };
    builder.build()
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Change {
    Add,
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
        Change::Delete => matches!(errno, libc::ESRCH | libc::ENOENT | libc::EADDRNOTAVAIL),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_contractual_errnos_are_idempotent() {
        assert!(super::ignored_errno(super::Change::Add, libc::EEXIST));
        assert!(!super::ignored_errno(super::Change::Add, libc::EPERM));
        for errno in [libc::ESRCH, libc::ENOENT, libc::EADDRNOTAVAIL] {
            assert!(super::ignored_errno(super::Change::Delete, errno));
        }
        assert!(!super::ignored_errno(super::Change::Delete, libc::EPERM));
    }
}
