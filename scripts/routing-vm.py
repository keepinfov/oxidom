"""Executed only by the NixOS VM driver; no host daemon/network is a fixture."""

import ipaddress
import json
import shlex


LAN_CLIENT = "10.42.0.2"
GATEWAY = "10.42.0.1"
CANDIDATES = ["192.0.2.10", "192.0.2.11"]
SINK = "203.0.113.10"
STATE = "/var/lib/oxidom/state.toml"


def quote(value):
    return shlex.quote(str(value))


def user_command(command):
    # No session bus/autostarter and no inherited proxy configuration. The
    # ordinary system-bus policy grants this non-root group member access.
    return "timeout --kill-after=5s 45s runuser -u tester -- env -i HOME=/home/tester PATH=/run/current-system/sw/bin OXIDOM_SESSION_DAEMON=external " + command


def cli(*args):
    return client.succeed(user_command("oxidom " + shlex.join(args))).strip()


def rpc(method, signature, *args):
    command = shlex.join([
        "busctl", "--system", "--timeout=30", "--json=short", "call",
        "dev.keepinfov.oxidom.Daemon", "/dev/keepinfov/oxidom/Daemon",
        "dev.keepinfov.oxidom1", method, signature, *args,
    ])
    response = client.succeed(user_command(command)).strip()
    return json.loads(response) if response else None


def kernel_json(command):
    return json.loads(client.succeed(command))


def saved_state():
    # Root is the VM observer, never the caller driving the application.
    return json.loads(client.succeed(
        "python3 -c " + quote(
            "import json,tomllib; print(json.dumps(tomllib.load(open(" + repr(STATE) + ", 'rb'))))"
        )
    ))


def route_get(address, mark=None):
    command = "ip -j -4 route get " + address
    if mark is not None:
        command += " mark " + str(mark)
    return kernel_json(command)[0]


def curl(url, *options):
    command = shlex.join([
        "curl", "--noproxy", "*", "--fail", "--silent", "--show-error",
        "--connect-timeout", "3", "--max-time", "10", *options, url,
    ])
    return json.loads(client.succeed(user_command(command)))


def start_capture(mode):
    unit = "p15-capture-" + mode
    path = "/var/tmp/" + unit + ".pcap"
    expression = "tcp and (host " + SINK + " or host " + GATEWAY + " or host " + CANDIDATES[0] + " or host " + CANDIDATES[1] + ")"
    client.succeed(shlex.join([
        "systemd-run", "--unit=" + unit, "--service-type=exec",
        "--property=KillSignal=SIGINT", "--property=TimeoutStopSec=10",
        "tcpdump", "--immediate-mode", "-i", "eth1", "-s", "0", "-U", "-w", path, expression,
    ]))
    client.wait_until_succeeds("journalctl -u " + unit + " --no-pager | grep -F 'listening on eth1'", timeout=30)
    client.succeed("systemctl is-active " + unit)
    return unit, path


def finish_capture(unit, path, marker):
    candidate_filter = "dst host 192.0.2.10 and dst port 1080"
    # Immediate mode drains kernel delivery without waiting for a buffer
    # timeout; -U flushes each delivered packet to pcap. Observe the actual
    # canary in that file before SIGINT, then recheck the closed file below.
    observed = shlex.join(["tcpdump", "-nn", "-A", "-r", path, candidate_filter]) + " | " + shlex.join(["grep", "-F", "--", marker])
    gate = "systemctl is-active --quiet " + quote(unit) + " && bash -o pipefail -c " + quote(observed)
    try:
        client.wait_until_succeeds(gate, timeout=30)
    finally:
        # Wait for SIGINT and output flush, including a failed observation.
        client.succeed("systemctl stop " + unit)
    assert client.succeed("systemctl show -p ExecMainStatus --value " + unit).strip() == "0"
    candidate = client.succeed(shlex.join(["tcpdump", "-nn", "-A", "-r", path, candidate_filter]))
    assert marker in candidate, "the same capture must contain this HTTP canary inside candidate A's real SOCKS stream"
    direct = client.succeed("tcpdump -nn -r " + path + " 'dst host 203.0.113.10 and dst port 18080'")
    assert not direct.strip(), "direct TCP packets to the sink HTTP endpoint escaped onto the physical data NIC"


def check_owned_plan(name, mode):
    state = saved_state()
    assert state["format_version"] == 1
    session = next(item for item in state["sessions"] if item["profile"] == name)
    interface = session["interface"]
    owner = interface["ownership"]
    assert not owner.get("cleanup_dirty", False)
    assert owner["metric"] > 0 and len(owner["cookie"]) == 32
    device = interface["device"]
    link = kernel_json("ip -j -d link show dev " + device)[0]
    assert link["ifindex"] == owner["device_index"]
    assert link["ifalias"] == "oxidom-tun-v1:" + owner["cookie"]
    assert "UP" in link["flags"]
    rule = owner["rule"]
    assert interface["mark"] == rule["mark"] and interface["table"] == rule["table"]
    rules = kernel_json("ip -j -4 rule show")
    matching = [item for item in rules if item.get("priority") == rule["priority"]]
    assert len(matching) == 1
    actual = matching[0]
    assert int(str(actual["fwmark"]), 0) == rule["mark"]
    # iproute2 omits an all-ones fwmark mask in its JSON presentation.
    assert int(str(actual.get("fwmask", "0xffffffff")), 0) == 0xFFFFFFFF
    assert int(actual["table"]) == rule["table"]
    assert str(actual["protocol"]) == "242"
    main = kernel_json("ip -j -4 route show table main")
    private = kernel_json("ip -j -4 route show table " + str(rule["table"]))
    for address in CANDIDATES:
        bypass = [item for item in main if item.get("dst") == address or item.get("dst") == address + "/32"]
        assert len(bypass) == 1
        assert bypass[0]["gateway"] == GATEWAY and bypass[0]["dev"] == "eth1"
        assert bypass[0]["metric"] == owner["metric"] and str(bypass[0]["protocol"]) == "242"
        assert not any(item.get("dst", "").split("/")[0] == address for item in private)
        assert route_get(address)["dev"] == "eth1"
        assert route_get(address, rule["mark"])["dev"] == device
    assert route_get(SINK)["dev"] == device
    captures = {"203.0.113.10"} if mode == "list" else {"0.0.0.0/1", "128.0.0.0/1"}
    for destination in captures:
        capture = [item for item in main if item.get("dst", "").removesuffix("/32") == destination]
        assert len(capture) == 1 and capture[0]["dev"] == device
        assert capture[0]["metric"] == owner["metric"] and str(capture[0]["protocol"]) == "242"
    # Complete durable selectors, rather than a reconstruction by profile name.
    assert owner["routes"] and all(item["output_interface"] > 0 for item in owner["routes"])
    return interface


def check_stopped(name, interface):
    cli("down", name)
    client.wait_until_fails("ip link show dev " + interface["device"])
    assert not any(item["profile"] == name for item in saved_state().get("sessions", []))
    owner = interface["ownership"]
    assert not kernel_json("ip -j -4 route show table all proto 242")
    assert not [item for item in kernel_json("ip -j -4 rule show") if str(item.get("protocol")) == "242"]
    # The helper/core must already be gone before VM teardown can mask a leak.
    client.fail("kill -0 " + str(interface["tun2socks_pid"]))
    assert route_get(SINK)["dev"] == "eth1"
    assert not [item for item in kernel_json("ip -j -4 route show table all") if str(item.get("table")) == str(owner["rule"]["table"])]


start_all()
client.wait_for_unit("oxidom.service", timeout=60)
peer.wait_for_unit("p15-http.service", timeout=60)
peer.wait_for_unit("p15-xray.service", timeout=60)
peer.wait_for_open_port(1080, addr="192.0.2.10", timeout=30)
peer.wait_for_open_port(1080, addr="192.0.2.11", timeout=30)
peer.wait_for_open_port(18080, addr=SINK, timeout=30)

with subtest("private topology and real unprivileged control"):
    assert client.succeed(user_command("id -u")).strip() == "1000"
    defaults = kernel_json("ip -j -4 route show default")
    assert len(defaults) == 1 and defaults[0]["gateway"] == GATEWAY and defaults[0]["dev"] == "eth1"
    assert not json.loads(peer.succeed("ip -j -4 route show default"))
    # iproute2 may omit a link entirely from an IPv4-only address dump when
    # that link has no IPv4 address. The command must still succeed.
    assert not any(item["addr_info"] for item in kernel_json("ip -j -4 addr show dev eth0"))
    assert client.succeed("xray version").startswith("Xray 26.3.27 ")
    assert peer.succeed("xray version").startswith("Xray 26.3.27 ")
    for address in [*CANDIDATES, SINK]:
        assert ipaddress.ip_address(address) not in ipaddress.ip_network("10.42.0.0/24")
        route = route_get(address)
        assert route["gateway"] == GATEWAY and route["dev"] == "eth1"
    baseline = curl("http://" + SINK + ":18080/baseline-direct")
    assert baseline == {"path": "/baseline-direct", "source": LAN_CLIENT}
    daemon_pid = int(client.succeed("systemctl show -p MainPID --value oxidom"))
    daemon_status = client.succeed("cat /proc/" + str(daemon_pid) + "/status")
    caps = int(next(line.split()[1] for line in daemon_status.splitlines() if line.startswith("CapEff:")), 16)
    assert caps == 1 << 12, "only CAP_NET_ADMIN is effective in the system daemon"
    cli_status = client.succeed(user_command("cat /proc/self/status"))
    assert int(next(line.split()[1] for line in cli_status.splitlines() if line.startswith("CapEff:")), 16) == 0
    imported = rpc("ImportLinks", "s", "socks://192.0.2.10:1080#CandidateA\nsocks://192.0.2.11:1080#CandidateB")
    assert imported["data"][0] == 2
    servers = json.loads(cli("list", "servers", "--json"))
    ids = [next(item["id"] for item in servers if item["address"] == address) for address in CANDIDATES]

for mode in ["list", "default"]:
    name = "route-" + mode
    with subtest(mode + " real TUN path, qualified plan and explicit cleanup"):
        profile = {
            "description": "Invented isolated routing fixture",
            "select": {"server": ids[0]},
            "proxy": {"socks_port": 10808, "http_port": 10809},
            "interface": {"enable": True, "device": "oxi-" + mode, "routes": mode, "list": [SINK + "/32"] if mode == "list" else []},
            "on_core_exit": "hold",
            "failover": {"enabled": True, "members": ids, "max_attempts": 1, "cooldown_secs": 1},
        }
        rpc("SaveProfile", "ss", name, json.dumps(profile))
        cli("up", name)
        connected = "import json,sys; assert any(s['profile'] == " + repr(name) + " and s['state'] == 'connected' for s in json.load(sys.stdin))"
        client.wait_until_succeeds(user_command("oxidom list sessions --json") + " | python3 -c " + quote(connected), timeout=60)
        session = next(item for item in json.loads(cli("list", "sessions", "--json")) if item["profile"] == name)
        assert session["state"] == "connected"
        assert session["runtime"]["failover"]["state"] == "armed"
        interface = check_owned_plan(name, mode)
        core_pid = next(item["xray_pid"] for item in saved_state()["sessions"] if item["profile"] == name)
        config = json.loads(client.succeed("cat /var/lib/oxidom/current-config-" + name + ".json"))
        assert {"type": "field", "ip": ["geoip:private"], "outboundTag": "direct"} in config["routing"]["rules"]
        marker = "/p15-" + mode + "-through-candidate-a"
        unit, capture = start_capture(mode)
        body = curl("http://" + SINK + ":18080" + marker)
        assert body["path"] == marker and body["source"] != LAN_CLIENT
        # A positive LAN control through the same generated Xray exercises the
        # still-active geoip:private direct rule, rather than deleting it.
        lan_marker = "/p15-" + mode + "-lan-direct"
        lan = curl("http://" + GATEWAY + ":18080" + lan_marker, "--proxy", "socks5h://" + session["address"] + ":" + str(session["socks_port"]), "--noproxy", "")
        assert lan == {"path": lan_marker, "source": LAN_CLIENT}
        finish_capture(unit, capture, marker)
        peer.succeed("grep -F " + quote(SINK + ":18080") + " /var/lib/p15-xray/access.log | grep -F 'candidate-192.0.2.10'")
        peer.succeed("journalctl -u p15-http --no-pager | grep -F " + quote(marker))
        check_stopped(name, interface)
        client.fail("kill -0 " + str(core_pid))
        # Explicit Stop releases this route scope. It is not a core-death hold.
        after = curl("http://" + SINK + ":18080/p15-" + mode + "-released-by-stop")
        assert after["source"] == LAN_CLIENT
        print("Routing VM baseline passed: " + mode + ", real candidate hop, no direct TCP to sink HTTP endpoint on data NIC, complete explicit cleanup")

print("P15 foundation passed; core/helper/daemon faults, nft/cgroups and the remaining matrix are separate acceptance")
