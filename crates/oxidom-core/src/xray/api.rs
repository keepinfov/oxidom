//! Read Xray's routing-balancer state through its bundled CLI.

use std::net::SocketAddrV4;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BalancerInfo {
    pub tag: String,
    /// `principleTarget.tag` verbatim. **Its meaning depends on the balancer's
    /// strategy**, which the response does not carry, so this type refuses to
    /// interpret it and the caller — which knows the strategy — must:
    ///
    /// - `roundRobin` / `random` have no single current node, and the core
    ///   answers with every tag it still considers eligible. Absence from this
    ///   list therefore means the observatory dropped that node.
    /// - `leastPing` / `leastLoad` answer with the one tag they picked.
    ///   Absence says nothing at all about health — verified on Xray 26.3.27
    ///   with two reachable outbounds, where only the faster one came back.
    pub principle: Vec<String>,
    /// A `xray api bo` override pins one target whatever the strategy says.
    pub override_target: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ApiResponse {
    balancer: ApiBalancer,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ApiBalancer {
    #[serde(rename = "override")]
    selection_override: ApiOverride,
    #[serde(rename = "principleTarget")]
    principle_target: PrincipleTarget,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ApiOverride {
    target: String,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct PrincipleTarget {
    tag: Vec<String>,
}

/// Ask the running core which outbound tags its balancer currently considers.
///
/// Xray 26.3.27 has an undocumented `--json` flag. Its response does not carry
/// strategy names or delays despite the command's help text claiming health
/// and strategy, so those values must not be invented here. The configured
/// strategy remains session state.
pub fn balancer_info(
    xray: &Path,
    api: SocketAddrV4,
    tag: &str,
    timeout: Duration,
) -> Result<BalancerInfo> {
    let timeout_secs = timeout.as_secs().max(1).to_string();
    let mut child = Command::new(xray)
        .args(["api", "bi", "--json", "--server"])
        .arg(api.to_string())
        .args(["--timeout", &timeout_secs])
        .arg(tag)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning {} api bi", xray.display()))?;

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().context("waiting for xray api bi")? {
            Some(_) => break,
            None if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            None => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("xray api bi timed out after {} ms", timeout.as_millis());
            }
        }
    }
    let output = child
        .wait_with_output()
        .context("collecting xray api bi output")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "xray api bi exited with {}: {}",
            output.status,
            stderr.trim()
        );
    }

    parse_json(tag, &output.stdout)
}

fn parse_json(tag: &str, bytes: &[u8]) -> Result<BalancerInfo> {
    let response: ApiResponse =
        serde_json::from_slice(bytes).context("parsing xray api bi JSON")?;
    let override_target =
        Some(response.balancer.selection_override.target).filter(|target| !target.is_empty());
    Ok(BalancerInfo {
        tag: tag.to_string(),
        principle: response.balancer.principle_target.tag,
        override_target,
    })
}

#[cfg(test)]
mod tests {
    use super::parse_json;

    // Captured verbatim from Xray 26.3.27 on 2026-07-28. Keep the parser tied
    // to what the bundled command actually emits, not to its stale help text.
    const LIVE_JSON: &str = r#"{
    "balancer": {
        "override": {},
        "principleTarget": {
            "tag": [
                "s-one",
                "s-two"
            ]
        }
    }
}
"#;

    /// Captured verbatim on the same core with `strategy = "leastPing"`, two
    /// reachable freedom outbounds and a live observatory. Both nodes were up,
    /// yet only the faster one comes back — which is why `principle` must never
    /// be read as "the healthy set" without knowing the strategy.
    const LIVE_JSON_LEAST_PING: &str = r#"{
    "balancer": {
        "override": {},
        "principleTarget": {
            "tag": [
                "s-two"
            ]
        }
    }
}
"#;

    #[test]
    fn parses_the_live_xray_26_3_27_json_shape() {
        let info = parse_json("pool", LIVE_JSON.as_bytes()).unwrap();

        assert_eq!(info.tag, "pool");
        assert_eq!(info.principle, ["s-one", "s-two"]);
        assert_eq!(info.override_target, None);
    }

    #[test]
    fn a_least_ping_core_answers_with_the_single_node_it_picked() {
        let info = parse_json("pool", LIVE_JSON_LEAST_PING.as_bytes()).unwrap();

        assert_eq!(info.principle, ["s-two"]);
        assert_eq!(info.override_target, None);
    }

    #[test]
    fn an_override_is_reported_apart_from_the_principle_set() {
        let info = parse_json(
            "pool",
            br#"{"balancer":{"override":{"target":"s-two"},"principleTarget":{"tag":["s-one"]}}}"#,
        )
        .unwrap();

        assert_eq!(info.principle, ["s-one"]);
        assert_eq!(info.override_target.as_deref(), Some("s-two"));
    }
}
