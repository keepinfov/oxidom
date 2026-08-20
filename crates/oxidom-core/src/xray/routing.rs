//! The routing block a profile carries verbatim.
//!
//! A subscription that came with good routing — advertising blocked, one
//! country direct, the rest through the proxy — is imported as a list of
//! servers and its rules are dropped: `subscription_format::parse` reads a
//! provider's `routing` only for the balancer tag. Until oxidom models rules of
//! its own, a profile can hold the block as written and the generator will
//! carry it through, the way `noises` are already carried.
//!
//! Verbatim is not the same as unchecked. Everything here is refused rather
//! than passed on, because each of these produces either a core that will not
//! start or, worse, one that starts and routes somewhere the interface does not
//! admit to:
//!
//! - **`balancers`, and any rule with a `balancerTag`.** Balancing is oxidom's:
//!   a selector is a prefix match over outbound tags, and one that resolved to
//!   `direct` would send the whole tunnel out in the clear while the interface
//!   still said Connected. `xray/config.rs` re-tags every provider-supplied
//!   outbound for exactly this reason.
//! - **`domainStrategy`.** `[core] domain_strategy` already owns that setting,
//!   at two levels with a defined precedence. A second spelling that silently
//!   won would make the editor lie about what is in force.
//! - **An `outboundTag` naming an outbound that will not exist.** The tags
//!   oxidom emits are `direct`, `block`, and — for a single-server session only
//!   — `proxy`. A pool has one outbound per member instead, so a rule aimed at
//!   `proxy` there is refused with that as the reason rather than becoming a
//!   config the core rejects at spawn.

use anyhow::{Result, bail};
use serde_json::Value;

/// Outbound tags a rule may send traffic to, whatever the session.
const ALWAYS_PRESENT: [&str; 2] = ["direct", "block"];

/// The tag a single-server session gives its one proxy outbound.
const SINGLE_SERVER_TAG: &str = "proxy";

/// What the session will offer this block, which decides whether a rule can
/// name `proxy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// One server, one `proxy` outbound.
    SingleServer,
    /// A pool: one outbound per member, reached through the balancer, so no
    /// outbound is called `proxy`.
    Pool,
}

/// Check a raw `routing` block and return it parsed.
///
/// Called where a person can still be told about the problem — saving a profile
/// and bringing one up — so that nothing further down has to decide what to do
/// with a block it cannot use.
pub fn validate(raw: &str, shape: Shape) -> Result<Value> {
    let value: Value = serde_json::from_str(raw.trim())
        .map_err(|error| anyhow::anyhow!("the routing block is not JSON: {error}"))?;
    let Some(object) = value.as_object() else {
        bail!("the routing block must be a JSON object, the way it appears in an Xray config");
    };
    if object.contains_key("balancers") {
        bail!(
            "a routing block may not carry balancers: oxidom builds the balancer for a pool \
             itself, and a selector from elsewhere can resolve to an outbound that leaves the \
             tunnel"
        );
    }
    if object.contains_key("domainStrategy") {
        bail!(
            "set domainStrategy through [core] domain_strategy instead: two places for one \
             setting means the editor cannot say which is in force"
        );
    }
    let rules = match object.get("rules") {
        None => &Vec::new(),
        Some(Value::Array(rules)) => rules,
        Some(_) => bail!("routing.rules must be an array"),
    };
    for (index, rule) in rules.iter().enumerate() {
        check_rule(rule, index, shape)?;
    }
    Ok(value)
}

fn check_rule(rule: &Value, index: usize, shape: Shape) -> Result<()> {
    let position = index + 1;
    let Some(object) = rule.as_object() else {
        bail!("routing rule {position} is not an object");
    };
    if object.contains_key("balancerTag") {
        bail!(
            "routing rule {position} sends traffic to a balancer, which oxidom owns: drop the \
             balancerTag and let the profile's pool decide"
        );
    }
    let Some(tag) = object.get("outboundTag") else {
        bail!("routing rule {position} names no outboundTag, so it decides nothing");
    };
    let Some(tag) = tag.as_str() else {
        bail!("routing rule {position} has an outboundTag that is not a string");
    };
    if ALWAYS_PRESENT.contains(&tag) {
        return Ok(());
    }
    if tag == SINGLE_SERVER_TAG {
        return match shape {
            Shape::SingleServer => Ok(()),
            Shape::Pool => bail!(
                "routing rule {position} sends traffic to {SINGLE_SERVER_TAG:?}, which a pool has \
                 no outbound for — its members are reached through the balancer, so send this to \
                 direct or block, or point the profile at a single server"
            ),
        };
    }
    bail!(
        "routing rule {position} sends traffic to {tag:?}, which is not an outbound oxidom \
         generates: the choices are \"direct\", \"block\", and \"proxy\" for a profile on a \
         single server"
    )
}

/// The rules to splice ahead of the generated ones, and the block's other keys.
///
/// Split here rather than in the generator so that the two positions that are
/// binding — the api rule first, a pool's balancer rule last — stay visible in
/// one place instead of being reconstructed around a merge.
pub fn parts(block: &Value) -> (Vec<Value>, Vec<(String, Value)>) {
    let Some(object) = block.as_object() else {
        return (Vec::new(), Vec::new());
    };
    let rules = object
        .get("rules")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let rest = object
        .iter()
        .filter(|(key, _)| key.as_str() != "rules")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    (rules, rest)
}

/// How many rules a block holds, for an interface that reports a count rather
/// than showing them.
///
/// `None` when the text is not a routing object at all — a row can then say
/// that instead of claiming a number it does not have. Deliberately more
/// forgiving than [`validate`]: this describes what is stored, and what is
/// stored may be something a save is about to refuse.
pub fn rule_count(raw: &str) -> Option<usize> {
    let value: Value = serde_json::from_str(raw.trim()).ok()?;
    value.as_object()?;
    Some(
        value
            .get("rules")
            .and_then(Value::as_array)
            .map_or(0, Vec::len),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(json: &str) -> Result<Value> {
        validate(json, Shape::SingleServer)
    }

    #[test]
    fn a_block_that_is_not_json_says_so_without_quoting_itself() {
        let error = block("{ rules: [] }").unwrap_err().to_string();
        assert!(error.contains("not JSON"), "got: {error}");
    }

    #[test]
    fn a_block_that_is_not_an_object_is_refused() {
        let error = block("[]").unwrap_err().to_string();
        assert!(error.contains("must be a JSON object"), "got: {error}");
    }

    #[test]
    fn a_carried_balancer_is_refused_because_a_selector_can_leave_the_tunnel() {
        let error = block(r#"{"balancers":[{"tag":"b","selector":["direct"]}]}"#)
            .unwrap_err()
            .to_string();
        assert!(error.contains("may not carry balancers"), "got: {error}");
    }

    #[test]
    fn a_rule_aimed_at_a_balancer_is_refused() {
        let error = block(r#"{"rules":[{"type":"field","network":"tcp","balancerTag":"b"}]}"#)
            .unwrap_err()
            .to_string();
        assert!(error.contains("balancerTag"), "got: {error}");
    }

    /// One setting, one place. The `[core]` key resolves over two levels and
    /// the editor reports which won; a second spelling here would win silently.
    #[test]
    fn a_domain_strategy_in_the_block_points_at_the_core_setting_that_owns_it() {
        let error = block(r#"{"domainStrategy":"IPIfNonMatch"}"#)
            .unwrap_err()
            .to_string();
        assert!(error.contains("[core] domain_strategy"), "got: {error}");
    }

    #[test]
    fn a_rule_naming_an_outbound_that_will_not_exist_names_the_choices() {
        let error = block(r#"{"rules":[{"domain":["example.com"],"outboundTag":"warp"}]}"#)
            .unwrap_err()
            .to_string();
        assert!(error.contains("\"warp\""), "got: {error}");
        assert!(error.contains("\"direct\""), "got: {error}");
    }

    #[test]
    fn a_rule_deciding_nothing_is_refused() {
        let error = block(r#"{"rules":[{"domain":["example.com"]}]}"#)
            .unwrap_err()
            .to_string();
        assert!(error.contains("names no outboundTag"), "got: {error}");
    }

    /// A pool reaches its members through the balancer, so nothing is tagged
    /// `proxy` there and a rule aimed at it would dangle.
    #[test]
    fn a_pool_refuses_a_rule_aimed_at_the_single_server_outbound() {
        let raw = r#"{"rules":[{"domain":["example.com"],"outboundTag":"proxy"}]}"#;
        assert!(validate(raw, Shape::SingleServer).is_ok());
        let error = validate(raw, Shape::Pool).unwrap_err().to_string();
        assert!(error.contains("no outbound for"), "got: {error}");
    }

    #[test]
    fn a_count_is_available_even_for_a_block_a_save_would_refuse() {
        assert_eq!(
            rule_count(r#"{"rules":[{"outboundTag":"block"}]}"#),
            Some(1)
        );
        assert_eq!(rule_count("{}"), Some(0));
        // Refused by `validate`, but a row still has to describe it.
        assert_eq!(
            rule_count(r#"{"balancers":[],"rules":[{"outboundTag":"warp"}]}"#),
            Some(1)
        );
        assert_eq!(rule_count("not json"), None);
        assert_eq!(rule_count("[]"), None);
    }

    #[test]
    fn the_rules_come_back_separately_from_the_rest_of_the_block() {
        let value = block(
            r#"{"domainMatcher":"hybrid","rules":[
                 {"domain":["geosite:category-ads-all"],"outboundTag":"block"}
               ]}"#,
        )
        .unwrap();
        let (rules, rest) = parts(&value);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0]["outboundTag"], "block");
        assert_eq!(
            rest,
            vec![("domainMatcher".to_string(), Value::from("hybrid"))]
        );
    }
}
