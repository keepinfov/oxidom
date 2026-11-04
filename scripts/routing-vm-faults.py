"""Continues routing-vm.py inside the same disposable NixOS test driver."""

from decimal import Decimal


FAULT_PROFILE = "fault-work"
PEER_PROFILE = "fault-peer"
OUTSIDE = "192.0.2.12"
PEER_PORT = 1082
BLOCKED_PORTS = [22001, 22002]


def fault_python(machine, source, timeout=30):
    return machine.succeed(
        "timeout --kill-after=5s " + str(timeout) + "s python3 -c " + quote(source)
    ).strip()


def fault_write_text(machine, path, value):
    source = "import pathlib; p=pathlib.Path(" + repr(path) + "); "
    source += "p.write_text(" + repr(value) + "); p.chmod(0o644)"
    fault_python(machine, source)


def fault_write_json(machine, path, value):
    # These upstream-only configs contain invented, unauthenticated fixture
    # listeners. They are not application configuration or credential storage.
    fault_write_text(machine, path, json.dumps(value))


def fault_rpc_json(method, signature="", *args):
    response = rpc(method, signature, *args)
    assert response["type"] == "s" and len(response["data"]) == 1
    return json.loads(response["data"][0])


def fault_session(name):
    return next(item for item in json.loads(cli("list", "sessions", "--json")) if item["profile"] == name)


def fault_wait_session(name, predicate, timeout=60):
    source = "import json,sys; s=next(s for s in json.load(sys.stdin) if s['profile']==" + repr(name) + "); assert " + predicate
    command = user_command("oxidom list sessions --json") + " | python3 -c " + quote(source)
    client.wait_until_succeeds(command, timeout=timeout)
    return fault_session(name)


def fault_current_predicate(index, state="armed"):
    return (
        "s['state']=='connected' and s['server_id']==" + repr(ids[index])
        + " and s['runtime']['failover']['state']==" + repr(state)
        + " and s['runtime']['failover']['current']==" + repr(ids[index])
    )


def fault_state_session(name):
    return next(item for item in saved_state()["sessions"] if item["profile"] == name)


def fault_identity(machine, pid, config, uid, parent):
    # The pidfd is opened before /proc is read; stat is checked on both sides
    # of cmdline/status collection. A replacement numeric PID cannot silently
    # become the identity accepted by a later fault operation.
    source = r"""
import json, os, pathlib, select
pid, config, uid, parent = PARAMETERS
descriptor = os.pidfd_open(pid)
try:
    def read():
        root = pathlib.Path('/proc') / str(pid)
        before = (root / 'stat').read_text().rsplit(')', 1)[1].split()
        argv = (root / 'cmdline').read_bytes().rstrip(b'\0').decode().split('\0')
        status = (root / 'status').read_text().splitlines()
        actual_uid = int(next(line.split()[1] for line in status if line.startswith('Uid:')))
        after = (root / 'stat').read_text().rsplit(')', 1)[1].split()
        assert before[19] == after[19]
        assert actual_uid == uid and int(after[1]) == parent
        assert argv[-2:] in [['-c', config], ['-config', config]]
        assert 'run' in argv and 'xray' in pathlib.Path(os.readlink(root / 'exe')).name
        return {'pid': pid, 'starttime': after[19], 'uid': actual_uid,
                'parent': int(after[1]), 'argv': argv, 'config': config}
    identity = read()
    observer = select.poll()
    observer.register(descriptor, select.POLLIN)
    assert not observer.poll(0), 'the pinned process exited during identity capture'
    print(json.dumps(identity))
finally:
    os.close(descriptor)
""".replace("PARAMETERS", repr((pid, config, uid, parent)))
    return json.loads(fault_python(machine, source))


def fault_core_identity(name):
    daemon = int(client.succeed("systemctl show -p MainPID --value oxidom"))
    uid = int(client.succeed("id -u oxidom"))
    return fault_identity(
        client, fault_state_session(name)["xray_pid"],
        "/var/lib/oxidom/current-config-" + name + ".json", uid, daemon,
    )


def fault_unit_identity(unit):
    pid = int(peer.succeed("systemctl show -p MainPID --value " + unit))
    uid = int(peer.succeed("awk '/^Uid:/ {print $2}' /proc/" + str(pid) + "/status"))
    return fault_identity(peer, pid, "/run/" + unit + ".json", uid, 1)


def fault_assert_core_unchanged(name, identity):
    assert fault_state_session(name)["xray_pid"] == identity["pid"]
    assert fault_core_identity(name) == identity


def fault_signal(machine, identity, number, exits=False):
    source = r"""
import json, os, pathlib, select, signal, time
expected, number, exits = PARAMETERS
pid = expected['pid']
descriptor = os.pidfd_open(pid)
try:
    root = pathlib.Path('/proc') / str(pid)
    stat = (root / 'stat').read_text().rsplit(')', 1)[1].split()
    argv = (root / 'cmdline').read_bytes().rstrip(b'\0').decode().split('\0')
    uid = int(next(line.split()[1] for line in (root / 'status').read_text().splitlines() if line.startswith('Uid:')))
    assert stat[19] == expected['starttime'] and int(stat[1]) == expected['parent']
    assert uid == expected['uid'] and argv == expected['argv']
    observer = select.poll()
    observer.register(descriptor, select.POLLIN)
    assert not observer.poll(0), 'refusing a fault against an exited identity'
    injected_ns = time.time_ns()
    signal.pidfd_send_signal(descriptor, number)
    if exits:
        assert observer.poll(5000), 'the signalled core did not exit'
    print(json.dumps({'injected_ns': injected_ns}))
finally:
    os.close(descriptor)
""".replace("PARAMETERS", repr((identity, number, exits)))
    return json.loads(fault_python(machine, source))["injected_ns"]


def fault_assert_gone(machine, identity):
    source = r"""
import pathlib
expected = PARAMETERS
root = pathlib.Path('/proc') / str(expected['pid'])
try:
    fields = (root / 'stat').read_text().rsplit(')', 1)[1].split()
except FileNotFoundError:
    pass
else:
    assert fields[19] != expected['starttime'], 'the recorded process is still present'
""".replace("PARAMETERS", repr(identity))
    fault_python(machine, source)


def fault_envelope():
    interface = check_owned_plan(FAULT_PROFILE, "default")
    # Core PID changes are expected. The observed helper/routing snapshots must
    # match initial activation; this alone does not monitor every transition.
    return json.dumps(interface, sort_keys=True)


def fault_profile_hash():
    return client.succeed("sha256sum /var/lib/oxidom/profiles/" + FAULT_PROFILE + ".toml").split()[0]


def fault_start_endpoint(unit, address, port):
    config = {
        "log": {"loglevel": "info", "access": "/var/lib/" + unit + "/access.log"},
        "inbounds": [{"listen": address, "port": port, "protocol": "socks", "tag": unit,
                      "settings": {"auth": "noauth", "udp": False}}],
        "outbounds": [{"protocol": "freedom", "tag": "sink"}],
    }
    fault_write_json(peer, "/run/" + unit + ".json", config)
    # A runtime unit file remains loadable after Stop. A transient unit can be
    # garbage-collected while inactive and would make a later Start ambiguous.
    definition = "\n".join([
        "[Unit]", "Description=Invented P15 upstream " + unit,
        "[Service]", "Type=exec", "DynamicUser=yes", "StateDirectory=" + unit,
        "StateDirectoryMode=0700", "NoNewPrivileges=yes", "CapabilityBoundingSet=",
        "AmbientCapabilities=", "TimeoutStopSec=10",
        "ExecStart=/run/current-system/sw/bin/xray run -config /run/" + unit + ".json", "",
    ])
    fault_write_text(peer, "/run/systemd/system/" + unit + ".service", definition)
    peer.succeed("systemctl daemon-reload")
    peer.succeed("systemctl start " + unit)
    peer.wait_for_open_port(port, addr=address, timeout=30)
    peer.succeed("systemctl is-active " + unit)
    return fault_unit_identity(unit)


def fault_capture(label):
    captures = {}
    for suffix, device in [("nic", "eth1"), ("all", "any")]:
        unit = "p15-fault-cap-" + label + "-" + suffix
        path = "/var/tmp/" + unit + ".pcap"
        expression = "tcp and (host " + SINK + " or host " + GATEWAY + " or net 192.0.2.0/24)"
        definition = "\n".join([
            "[Unit]", "Description=P15 bounded packet capture " + unit,
            "[Service]", "Type=exec", "KillSignal=SIGINT", "TimeoutStopSec=10",
            "Environment=LC_ALL=C", "ExecStart=/run/current-system/sw/bin/" + shlex.join([
                "tcpdump", "--immediate-mode", "-i", device, "-s", "0", "-U", "-w", path, expression,
            ]), "",
        ])
        fault_write_text(client, "/run/systemd/system/" + unit + ".service", definition)
        client.succeed("systemctl daemon-reload")
        client.succeed("systemctl start " + unit)
        client.wait_until_succeeds("journalctl -u " + unit + " --no-pager | grep -F " + quote("listening on " + device), timeout=30)
        captures[suffix] = (unit, path)
    return captures


def fault_close_capture(captures, peer_markers, candidate_markers=(), tun_canary_ports=(), endpoint_syn_addresses=()):
    def wait_for_packet(suffix, expression, marker=None):
        unit, path = captures[suffix]
        command = ["tcpdump", "-nn", "-tt", "-r", path]
        if marker is not None:
            command.append("-A")
        command.append(expression)
        predicate = ["grep", "-F", "--", marker] if marker is not None else ["grep", "-E", "."]
        observed = shlex.join(command) + " | " + shlex.join(predicate)
        gate = "systemctl is-active --quiet " + quote(unit) + " && bash -o pipefail -c " + quote(observed)
        client.wait_until_succeeds(gate, timeout=30)

    try:
        assert peer_markers, "each capture needs a unique live independent-peer control"
        # Immediate mode drains kernel delivery; -U flushes delivered packets.
        # Require the actual controls in both live files before SIGINT, not a
        # kernel-filter count or a guessed buffer-delay sleep.
        for suffix in ["nic", "all"]:
            for marker in peer_markers:
                wait_for_packet(suffix, "dst host " + GATEWAY + " and dst port " + str(PEER_PORT), marker)
            for address, marker in candidate_markers:
                wait_for_packet(suffix, "dst host " + address + " and dst port 1080", marker)
            for address in endpoint_syn_addresses:
                wait_for_packet(suffix, "dst host " + address + " and dst port 1080 and tcp[tcpflags] & tcp-syn != 0")
        for source_port in tun_canary_ports:
            wait_for_packet("all", "dst host " + SINK + " and dst port 18080 and src port " + str(source_port) + " and tcp[tcpflags] & tcp-syn != 0")
    finally:
        failures = []
        for unit, _path in captures.values():
            try:
                client.succeed("systemctl stop " + unit, timeout=20)
            except Exception as error:
                failures.append(unit + ": " + str(error))
        if failures:
            raise RuntimeError("capture cleanup failed: " + "; ".join(failures))
    for unit, _path in captures.values():
        assert client.succeed("systemctl show -p ExecMainStatus --value " + unit).strip() == "0"
        assert "0 packets dropped by kernel" in client.succeed("journalctl -u " + unit + " --no-pager")


def fault_packets(captures, expression, all_interfaces=False, ascii_payload=False):
    path = captures["all" if all_interfaces else "nic"][1]
    command = ["tcpdump", "-nn", "-tt", "-r", path]
    if ascii_payload:
        command.append("-A")
    command.append(expression)
    return client.succeed(shlex.join(command))


def fault_capture_assertions(captures, peer_markers, candidate_markers=(), no_a_since_ns=None, tun_canary_ports=()):
    for all_interfaces in [False, True]:
        for marker in peer_markers:
            packets = fault_packets(captures, "dst host " + GATEWAY + " and dst port " + str(PEER_PORT), all_interfaces=all_interfaces, ascii_payload=True)
            assert marker in packets, "a capture missed its live independent-peer control"
        for address, marker in candidate_markers:
            packets = fault_packets(captures, "dst host " + address + " and dst port 1080", all_interfaces=all_interfaces, ascii_payload=True)
            assert marker in packets, "a capture missed the canary through its expected real candidate"
    assert not fault_packets(captures, "dst host " + SINK + " and dst port 18080").strip()
    # The all-interface capture also sees a forbidden dial trapped by the TUN,
    # not just a forbidden endpoint that managed to escape onto the data NIC.
    assert not fault_packets(captures, "dst host " + OUTSIDE + " and dst port 1080", all_interfaces=True).strip()
    for source_port in tun_canary_ports:
        assert route_get(SINK)["dev"] == fault_state_session(FAULT_PROFILE)["interface"]["device"]
        packets = fault_packets(captures, "dst host " + SINK + " and dst port 18080 and src port " + str(source_port) + " and tcp[tcpflags] & tcp-syn != 0", all_interfaces=True)
        assert packets.strip(), "the all-interface capture did not observe this blocked canary entering the TUN route"
    if no_a_since_ns is not None:
        packets = fault_packets(captures, "dst host 192.0.2.10 and dst port 1080 and tcp[tcpflags] & tcp-syn != 0", all_interfaces=True)
        timestamps = [Decimal(line.split()[0]) for line in packets.splitlines() if line.strip()]
        assert not any(timestamp * 1_000_000_000 >= no_a_since_ns for timestamp in timestamps)


def fault_canary(marker, session_name=None):
    options = []
    if session_name is not None:
        session = fault_session(session_name)
        options = ["--proxy", "socks5h://" + session["address"] + ":" + str(session["socks_port"]), "--noproxy", ""]
    body = curl("http://" + SINK + ":18080" + marker, *options)
    assert body["path"] == marker and body["source"] != LAN_CLIENT
    receipt = peer.succeed("journalctl -u p15-http --no-pager")
    assert marker in receipt
    return marker


def fault_peer_canary(marker, identity):
    fault_assert_core_unchanged(PEER_PROFILE, identity)
    session = fault_session(PEER_PROFILE)
    assert session["state"] == "connected" and session["server_id"] == peer_id
    assert session.get("runtime", {}).get("failover") is None
    return fault_canary(marker, PEER_PROFILE)


def fault_blocked_canary(marker, source_port):
    assert not client.succeed("ss -H -tan " + quote("sport = :" + str(source_port))).strip()
    command = shlex.join([
        "curl", "--noproxy", "*", "--fail", "--silent", "--show-error",
        "--connect-timeout", "3", "--max-time", "8",
        "--local-port", str(source_port),
        "http://" + SINK + ":18080" + marker,
    ])
    status, output = client.execute(user_command(command))
    assert status in {7, 28, 52, 56}, "expected a network-path refusal/timeout, not a tool/configuration failure: " + output
    assert marker not in peer.succeed("journalctl -u p15-http --no-pager")


def fault_cross_observed_deadline(remaining_ms):
    assert 0 < remaining_ms <= 60_000
    source = r"""
import os, time
descriptor = os.timerfd_create(time.CLOCK_MONOTONIC, flags=os.TFD_CLOEXEC)
try:
    os.timerfd_settime(descriptor, initial=SECONDS)
    assert len(os.read(descriptor, 8)) == 8
finally:
    os.close(descriptor)
""".replace("SECONDS", repr(remaining_ms / 1000 + 1))
    fault_python(client, source, timeout=70)


def fault_save_work(cooldown=1):
    profile = {
        "description": "Invented real-core routing fault fixture",
        "select": {"server": ids[0]},
        "proxy": {"socks_port": 10808, "http_port": 10809},
        "interface": {"enable": True, "device": "oxi-fault", "routes": "default"},
        "on_core_exit": "hold",
        "failover": {"enabled": True, "members": ids, "max_attempts": 1, "cooldown_secs": cooldown},
    }
    rpc("SaveProfile", "ss", FAULT_PROFILE, json.dumps(profile))
    cli("up", FAULT_PROFILE)
    fault_wait_session(FAULT_PROFILE, fault_current_predicate(0))
    assert fault_rpc_json("GetProfile", "s", FAULT_PROFILE)["select"]["server"] == ids[0]
    return fault_core_identity(FAULT_PROFILE), fault_envelope(), fault_profile_hash()


def fault_stop_work():
    interface = fault_state_session(FAULT_PROFILE)["interface"]
    check_stopped(FAULT_PROFILE, interface)


def fault_recover_to_b(label, peer_identity):
    initial = fault_core_identity(FAULT_PROFILE)
    envelope, saved = fault_envelope(), fault_profile_hash()
    capture = fault_capture(label)
    before = fault_peer_canary("/p15-" + label + "-peer-before", peer_identity)
    through_a = fault_canary("/p15-" + label + "-candidate-a")
    fault_signal(client, initial, 9, exits=True)
    fault_wait_session(FAULT_PROFILE, fault_current_predicate(1), timeout=45)
    replacement = fault_core_identity(FAULT_PROFILE)
    assert replacement != initial
    through_b = fault_canary("/p15-" + label + "-candidate-b")
    after = fault_peer_canary("/p15-" + label + "-peer-after", peer_identity)
    assert fault_envelope() == envelope and fault_profile_hash() == saved
    assert fault_session(FAULT_PROFILE)["runtime"]["failover"]["attempts"] == 0
    fault_assert_core_unchanged(FAULT_PROFILE, replacement)
    fault_close_capture(capture, [before, after], [(CANDIDATES[0], through_a), (CANDIDATES[1], through_b)])
    fault_capture_assertions(capture, [before, after], [(CANDIDATES[0], through_a), (CANDIDATES[1], through_b)])
    fault_assert_gone(client, initial)
    return replacement, envelope


with subtest("separate real upstreams and an unauthorized outside catalogue member"):
    assert not saved_state().get("sessions", [])
    ephemeral_low, _ephemeral_high = map(int, client.succeed("cat /proc/sys/net/ipv4/ip_local_port_range").split())
    assert all(1024 < port < ephemeral_low for port in BLOCKED_PORTS)
    peer.succeed("systemctl stop p15-xray.service")
    peer.succeed("ip address add " + OUTSIDE + "/32 dev lo")
    endpoint_units = ["p15-fault-a", "p15-fault-b", "p15-fault-c", "p15-fault-peer"]
    for unit, address, port in zip(endpoint_units, [*CANDIDATES, OUTSIDE, GATEWAY], [1080, 1080, 1080, PEER_PORT]):
        fault_start_endpoint(unit, address, port)
    added = rpc("ImportLinks", "s", "socks://" + OUTSIDE + ":1080#OutsideC\nsocks://" + GATEWAY + ":" + str(PEER_PORT) + "#IndependentPeer")
    assert added["data"][0] == 2
    servers = json.loads(cli("list", "servers", "--json"))
    outside_id = next(item["id"] for item in servers if item["address"] == OUTSIDE)
    peer_id = next(item["id"] for item in servers if item["address"] == GATEWAY and item["port"] == PEER_PORT)
    assert outside_id not in ids and peer_id not in ids
    rpc("SaveProfile", "ss", PEER_PROFILE, json.dumps({
        "description": "Independent invented LAN proxy fixture",
        "select": {"server": peer_id},
        "proxy": {"socks_port": 10818, "http_port": 10819},
    }))
    cli("up", PEER_PROFILE)
    fault_wait_session(PEER_PROFILE, "s['state']=='connected' and s['server_id']==" + repr(peer_id))
    pinned_peer = fault_core_identity(PEER_PROFILE)


with subtest("core death reaches B with matching owned-plan and independent-peer observations"):
    fault_save_work()
    fault_recover_to_b("switch", pinned_peer)
    fault_stop_work()
    fault_assert_core_unchanged(PEER_PROFILE, pinned_peer)


with subtest("an unavailable authorized B exhausts one admission and reports an installed hold"):
    initial, envelope, saved = fault_save_work()
    capture = fault_capture("exhaust")
    before = fault_peer_canary("/p15-exhaust-peer-before", pinned_peer)
    peer.succeed("systemctl stop p15-fault-b")
    peer.fail("systemctl is-active p15-fault-b")
    listeners = peer.succeed("ss -H -ltn")
    assert CANDIDATES[1] + ":1080" not in listeners
    assert CANDIDATES[0] + ":1080" in listeners and OUTSIDE + ":1080" in listeners
    assert GATEWAY + ":" + str(PEER_PORT) in listeners
    injected_ns = fault_signal(client, initial, 9, exits=True)
    exhausted = fault_wait_session(FAULT_PROFILE, "s['runtime']['failover']['state']=='exhausted' and s['runtime']['retry']['state']=='exhausted'", timeout=90)
    progress = exhausted["runtime"]["failover"]
    assert progress["current"] == ids[1] and progress["attempts"] == 1
    assert progress["next"] is None and progress["members"] == ids
    assert exhausted["holding_traffic"]
    assert fault_envelope() == envelope and fault_profile_hash() == saved
    fault_blocked_canary("/p15-exhaust-blocked", BLOCKED_PORTS[0])
    after = fault_peer_canary("/p15-exhaust-peer-after", pinned_peer)
    fault_close_capture(capture, [before, after], tun_canary_ports=[BLOCKED_PORTS[0]], endpoint_syn_addresses=[CANDIDATES[1]])
    fault_capture_assertions(capture, [before, after], no_a_since_ns=injected_ns, tun_canary_ports=[BLOCKED_PORTS[0]])
    assert fault_packets(capture, "dst host 192.0.2.11 and dst port 1080 and tcp[tcpflags] & tcp-syn != 0").strip()
    assert fault_session(FAULT_PROFILE)["runtime"]["failover"] == progress
    fault_stop_work()
    peer.succeed("systemctl start p15-fault-b")
    peer.wait_for_open_port(1080, addr=CANDIDATES[1], timeout=30)


with subtest("disable and reenable cannot resurrect a revoked cooldown activation"):
    fault_save_work(cooldown=60)
    replacement, envelope = fault_recover_to_b("revoke-switch", pinned_peer)
    capture = fault_capture("revoke")
    before = fault_peer_canary("/p15-revoke-peer-before", pinned_peer)
    injected_ns = fault_signal(client, replacement, 9, exits=True)
    waiting = fault_wait_session(FAULT_PROFILE, "s['runtime']['retry']['state']=='backoff' and s['runtime']['retry']['remaining_ms']>0 and s['runtime']['failover']['next']==" + repr(ids[0]), timeout=10)
    remaining = waiting["runtime"]["retry"]["remaining_ms"]
    during = fault_peer_canary("/p15-revoke-peer-during-backoff", pinned_peer)
    still_waiting = fault_session(FAULT_PROFILE)
    assert still_waiting["runtime"]["retry"]["state"] == "backoff"
    assert still_waiting["runtime"]["retry"]["remaining_ms"] > 0
    assert still_waiting["holding_traffic"] and fault_envelope() == envelope
    cli("profile", "failover", FAULT_PROFILE, "--disable", "--json")
    cli("profile", "failover", FAULT_PROFILE, "--enable", "--json")
    disarmed = fault_wait_session(FAULT_PROFILE, "s['runtime']['failover']['state']=='disarmed'")
    assert disarmed["holding_traffic"] and fault_envelope() == envelope
    assert fault_rpc_json("GetProfile", "s", FAULT_PROFILE)["failover"]["enabled"]
    fault_cross_observed_deadline(remaining)
    fault_blocked_canary("/p15-revoke-blocked-after-deadline", BLOCKED_PORTS[1])
    after = fault_peer_canary("/p15-revoke-peer-after", pinned_peer)
    assert fault_session(FAULT_PROFILE)["runtime"]["failover"]["state"] == "disarmed"
    assert fault_envelope() == envelope
    fault_close_capture(capture, [before, during, after], tun_canary_ports=[BLOCKED_PORTS[1]])
    fault_capture_assertions(capture, [before, during, after], no_a_since_ns=injected_ns, tun_canary_ports=[BLOCKED_PORTS[1]])
    fault_stop_work()
    fault_save_work()
    assert fault_canary("/p15-revoke-explicit-new-activation")
    fault_stop_work()


with subtest("SIGTERM cleanup releases its scope and restart does not resume authorization"):
    initial, envelope, _saved = fault_save_work()
    interface = fault_state_session(FAULT_PROFILE)["interface"]
    helper = interface["tun2socks_pid"]
    before = fault_rpc_json("DaemonInfo")
    fault_peer_canary("/p15-terminal-peer-before", pinned_peer)
    client.succeed("systemctl stop oxidom.service")
    # No oxidom/busctl call is allowed while stopped: the packaged D-Bus
    # activation policy could otherwise turn an observation into a restart.
    client.fail("systemctl is-active oxidom.service")
    client.fail("ip link show dev " + interface["device"])
    assert not kernel_json("ip -j -4 route show table all proto 242")
    assert not [rule for rule in kernel_json("ip -j -4 rule show") if str(rule.get("protocol")) == "242"]
    assert not saved_state().get("sessions", [])
    fault_assert_gone(client, initial)
    fault_assert_gone(client, pinned_peer)
    client.fail("kill -0 " + str(helper))
    released = curl("http://" + SINK + ":18080/p15-terminal-deliberate-release")
    assert released["source"] == LAN_CLIENT
    client.succeed("systemctl start oxidom.service")
    client.wait_for_unit("oxidom.service", timeout=60)
    after = fault_rpc_json("DaemonInfo")
    assert after["store_id"] == before["store_id"] and after["incarnation"] != before["incarnation"]
    assert not json.loads(cli("list", "sessions", "--json"))
    assert fault_rpc_json("GetProfile", "s", FAULT_PROFILE)["failover"]["enabled"]
    client.fail("ip link show dev " + interface["device"])
    fault_save_work()
    fault_canary("/p15-terminal-explicit-new-activation")
    fault_stop_work()


for endpoint_unit in endpoint_units:
    peer.succeed("systemctl stop " + endpoint_unit)
    peer.fail("systemctl is-active " + endpoint_unit)

print("P15 fault slice passed: real A-to-B confirmation, held endpoint observations at exhaustion/revocation, peer independence, and explicit terminal cleanup; transition release windows remain unmeasured")
