"""Real scoped routing in disposable NixOS guests; executed by the VM driver."""

import json
import shlex


PROFILE = "scoped"
LAN4 = "10.42.0.2"
GATEWAY4 = "10.42.0.1"
GATEWAY6 = "2001:db8:42::1"
SINK4 = "203.0.113.10"
SINK6 = "2001:db8:113::10"
CANDIDATES = ["192.0.2.10", "192.0.2.11"]
ACTOR_PATH = "/run/user/1000/p15-scoped-actor.py"
ACTOR_SOCKET = "/run/user/1000/p15-scoped-actor.sock"
STATE = "/var/lib/oxidom/state.toml"


ACTOR_SOURCE = r'''
import errno
import json
import os
import pathlib
import socket
import threading
import time

SOCKET_PATH = "/run/user/1000/p15-scoped-actor.sock"
NETWORK_ERRORS = {
    errno.EPERM, errno.EACCES,
    errno.ECONNREFUSED, errno.ECONNRESET, errno.ETIMEDOUT,
    errno.EHOSTUNREACH, errno.ENETUNREACH, errno.EPIPE,
}
window = None

def identity():
    status = pathlib.Path("/proc/self/status").read_text().splitlines()
    capabilities = int(next(line.split()[1] for line in status if line.startswith("CapEff:")), 16)
    cgroup = pathlib.Path("/proc/self/cgroup").read_text().strip()
    return {"pid": os.getpid(), "uid": os.getuid(), "cap_eff": capabilities, "cgroup": cgroup}

def request(host, path, family=4, timeout=2.0):
    assert path.startswith("/p15-") and "\r" not in path and "\n" not in path
    address_family = socket.AF_INET if family == 4 else socket.AF_INET6
    connection = socket.socket(address_family, socket.SOCK_STREAM)
    connection.settimeout(timeout)
    started = time.monotonic_ns()
    try:
        scope = socket.if_nametoindex("eth1") if host.startswith("fe80:") else 0
        address = (host, 18080) if family == 4 else (host, 18080, 0, scope)
        authority = host + ":18080" if family == 4 else "[" + host + "]:18080"
        connection.connect(address)
        connection.sendall(("GET " + path + " HTTP/1.1\r\nHost: " + authority + "\r\nConnection: close\r\n\r\n").encode())
        response = bytearray()
        while b"\r\n\r\n" not in response:
            chunk = connection.recv(4096)
            assert chunk
            response.extend(chunk)
            assert len(response) <= 32768
        headers, body = bytes(response).split(b"\r\n\r\n", 1)
        lengths = [
            line.split(b":", 1)[1].strip()
            for line in headers.split(b"\r\n")
            if line.lower().startswith(b"content-length:")
        ]
        assert len(lengths) == 1
        length = int(lengths[0])
        while len(body) < length:
            chunk = connection.recv(4096)
            assert chunk
            body += chunk
        assert len(body) == length
        assert headers.split(b"\r\n", 1)[0] == b"HTTP/1.1 200 OK"
        value = json.loads(body)
        assert value["path"] == path
        return {"ok": True, "body": value, "start_ns": started, "end_ns": time.monotonic_ns()}
    except TimeoutError:
        return {"ok": False, "error": "timeout", "start_ns": started, "end_ns": time.monotonic_ns()}
    except (ValueError, AssertionError):
        return {"ok": False, "error": "truncated", "start_ns": started, "end_ns": time.monotonic_ns()}
    except OSError as error:
        if error.errno not in NETWORK_ERRORS:
            raise
        return {"ok": False, "error": error.errno, "start_ns": started, "end_ns": time.monotonic_ns()}
    finally:
        connection.close()

def produce(current, label):
    deadline = time.monotonic() + 45
    try:
        for ordinal in range(16384):
            if current["stop"].is_set():
                return
            if time.monotonic() >= deadline:
                current["bounded_out"] = True
                return
            path = "/p15-window-" + label + "/" + str(ordinal)
            result = request("203.0.113.10", path, timeout=1.0)
            if result["ok"]:
                assert result["body"]["source"] != "10.42.0.2", "window canary arrived directly"
            with current["condition"]:
                if not current["first_start_ns"]:
                    current["first_start_ns"] = result["start_ns"]
                current["last_start_ns"] = result["start_ns"]
                current["attempts"] += 1
                if result["ok"]:
                    current["successes"] += 1
                    if current["first_success"] is None:
                        current["first_success"] = {
                            "path": path, "start_ns": result["start_ns"], "end_ns": result["end_ns"],
                        }
                else:
                    current["failures"] += 1
                current["condition"].notify_all()
            if not result["ok"] and (result["end_ns"] - result["start_ns"]) < 20_000_000:
                time.sleep(0.02)
        current["bounded_out"] = True
    except BaseException as error:
        current["error"] = repr(error)
    finally:
        with current["condition"]:
            current["finished"] = True
            current["condition"].notify_all()

def command(value):
    global window
    operation = value["command"]
    if operation == "info":
        return identity()
    if operation == "request":
        return request(value["host"], value["path"], value.get("family", 4))
    if operation == "begin":
        assert window is None
        current = {
            "stop": threading.Event(), "condition": threading.Condition(),
            "first_start_ns": 0, "last_start_ns": 0, "attempts": 0,
            "successes": 0, "failures": 0, "bounded_out": False, "error": None,
            "first_success": None, "finished": False,
        }
        current["thread"] = threading.Thread(target=produce, args=(current, value["label"]), daemon=True)
        window = current
        current["thread"].start()
        with current["condition"]:
            assert current["condition"].wait_for(
                lambda: current["first_success"] is not None or current["finished"], timeout=10,
            ), "no successful window canary before the transition"
            assert current["first_success"] is not None, current["error"] or "window ended without a successful canary"
            assert not current["bounded_out"] and current["error"] is None, current["error"]
            return {"first_start_ns": current["first_start_ns"], "first_success": current["first_success"]}
    if operation == "end":
        assert window is not None
        current = window
        with current["condition"]:
            assert current["condition"].wait_for(
                lambda: current["last_start_ns"] > value["after_ns"] or current["finished"],
                timeout=3,
            )
        assert current["last_start_ns"] > value["after_ns"], "no fresh request followed the observed transition"
        current["stop"].set()
        current["thread"].join(timeout=3)
        assert not current["thread"].is_alive()
        assert not current["bounded_out"] and current["error"] is None, current["error"]
        result = {key: current[key] for key in [
            "first_start_ns", "last_start_ns", "attempts", "successes", "failures",
        ]}
        window = None
        return result
    if operation == "quit":
        assert window is None
        return {"quitting": True}
    raise AssertionError("unknown actor operation")

os.umask(0o077)
server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
server.bind(SOCKET_PATH)
server.listen(4)
server.settimeout(300)
try:
    while True:
        connection, _address = server.accept()
        connection.settimeout(15)
        try:
            message = bytearray()
            while b"\n" not in message:
                piece = connection.recv(4096)
                assert piece
                message.extend(piece)
                assert len(message) <= 65536
            value = json.loads(message)
            result = command(value)
            connection.sendall(json.dumps(result).encode() + b"\n")
        finally:
            connection.close()
        if value["command"] == "quit":
            break
finally:
    server.close()
    pathlib.Path(SOCKET_PATH).unlink(missing_ok=True)
'''


def quote(value):
    return shlex.quote(str(value))


def user_command(command):
    environment = [
        "HOME=/home/tester", "USER=tester", "PATH=/run/current-system/sw/bin",
        "XDG_RUNTIME_DIR=/run/user/1000",
        "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus",
        "OXIDOM_SESSION_DAEMON=external",
    ]
    return "timeout --kill-after=5s 45s runuser -u tester -- env -i " + shlex.join(environment) + " " + command


def cli(*arguments):
    return client.succeed(user_command(shlex.join(["oxidom", *arguments]))).strip()


def user_systemctl(*arguments):
    return client.succeed(user_command(shlex.join(["systemctl", "--user", *arguments]))).strip()


def rpc(method, signature, *arguments):
    command = [
        "busctl", "--system", "--timeout=30", "--json=short", "call",
        "dev.keepinfov.oxidom.Daemon", "/dev/keepinfov/oxidom/Daemon",
        "dev.keepinfov.oxidom1", method, signature, *arguments,
    ]
    response = client.succeed(user_command(shlex.join(command))).strip()
    return json.loads(response) if response else None


def guest_python(machine, source, timeout=30):
    return machine.succeed("timeout --kill-after=5s " + str(timeout) + "s python3 -c " + quote(source)).strip()


def write_guest(machine, path, contents, mode=0o600, uid=None):
    source = "import os,pathlib; p=pathlib.Path(" + repr(path) + "); p.write_text(" + repr(contents) + "); p.chmod(" + repr(mode) + ")"
    if uid is not None:
        source += "; os.chown(p," + str(uid) + "," + str(uid) + ")"
    guest_python(machine, source)


def kernel_json(command):
    return json.loads(client.succeed(command))


def saved_state():
    return json.loads(guest_python(client, "import json,tomllib; print(json.dumps(tomllib.load(open(" + repr(STATE) + ",'rb'))))"))


def state_session():
    return next(item for item in saved_state()["sessions"] if item["profile"] == PROFILE)


def session():
    return next(item for item in json.loads(cli("list", "sessions", "--json")) if item["profile"] == PROFILE)


def wait_session(predicate):
    source = "import json,sys; s=next(s for s in json.load(sys.stdin) if s['profile']==" + repr(PROFILE) + "); assert " + predicate
    client.wait_until_succeeds(user_command("oxidom list sessions --json") + " | python3 -c " + quote(source), timeout=30)
    return session()


def guest_ns():
    return int(guest_python(client, "import time; print(time.monotonic_ns())"))


def actor(command, **arguments):
    value = {"command": command, **arguments}
    source = r'''
import json, socket
value = VALUE
connection = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
connection.settimeout(15)
connection.connect("/run/user/1000/p15-scoped-actor.sock")
try:
    connection.sendall(json.dumps(value).encode() + b"\n")
    response = bytearray()
    while b"\n" not in response:
        chunk = connection.recv(4096)
        assert chunk, "actor closed without a reply"
        response.extend(chunk)
        assert len(response) <= 65536
    print(json.dumps(json.loads(response)))
finally:
    connection.close()
'''.replace("VALUE", repr(value))
    return json.loads(guest_python(client, source))


def unscoped(host, path, family=4):
    address = "http://[" + host + "]:18080" + path if family == 6 else "http://" + host + ":18080" + path
    command = [
        "curl", "--noproxy", "*", "--fail", "--silent", "--show-error",
        "--connect-timeout", "3", "--max-time", "10", address,
    ]
    value = json.loads(client.succeed(user_command(shlex.join(command))))
    assert value["path"] == path
    return value


def record_and_identity():
    value = actor("info")
    assert value["uid"] == 1000 and value["cap_eff"] == 0
    cgroup = value["cgroup"].splitlines()
    assert len(cgroup) == 1 and cgroup[0].startswith("0::/")
    actual = cgroup[0][4:]
    record = next(item for item in saved_state()["run_cgroups"] if item["profile"] == PROFILE)
    expected = "user.slice/user-1000.slice/user@1000.service/oxidom\\x2dscoped.slice"
    assert record == {"profile": PROFILE, "path": expected, "level": 4}
    assert actual.startswith(expected + "/")
    source = "import json,pathlib; p=pathlib.Path('/proc')/" + repr(str(value["pid"])) + "; "
    source += "print(json.dumps({'cgroup':(p/'cgroup').read_text().strip(),'inode':pathlib.Path(" + repr("/sys/fs/cgroup/" + expected) + ").stat().st_ino}))"
    observed = json.loads(guest_python(client, source))
    assert observed["cgroup"] == value["cgroup"]
    return value, record, observed["inode"]


def same_actor():
    value, record, inode = record_and_identity()
    assert value == pinned_actor and record == pinned_record and inode == pinned_inode


def number(value):
    return int(value, 0) if isinstance(value, str) else int(value)


def rule_counter(rule):
    counters = [item["counter"] for item in rule["expr"] if "counter" in item]
    assert len(counters) == 1
    assert isinstance(counters[0]["packets"], int) and counters[0]["packets"] >= 0
    return counters[0]["packets"]


def matches_cgroup(rule):
    matches = [item["match"] for item in rule["expr"] if "match" in item and "socket" in item["match"].get("left", {})]
    assert len(matches) == 1
    match = matches[0]
    # nftables may emit the cgroup constant either as its resolved path string
    # or as its numeric kernel inode depending on the nftables version.
    assert match["op"] == "==" and match["left"]["socket"].get("key") == "cgroupv2"
    right = match["right"]
    if isinstance(right, str) and not right.startswith(("0x", "0X")) and not right.isdecimal():
        assert right == pinned_record["path"]
    else:
        assert number(right) == pinned_inode


def nft_snapshot(blocked=False):
    objects = kernel_json("nft -j list table inet oxidom")["nftables"]
    chains = {item["chain"]["name"]: item["chain"] for item in objects if "chain" in item}
    output = [item["rule"] for item in objects if "rule" in item and item["rule"]["chain"] == "profile_scoped"]
    restore = [item["rule"] for item in objects if "rule" in item and item["rule"]["chain"] == "restore_scoped"]
    assert chains["profile_scoped"]["type"] == "route" and chains["profile_scoped"]["hook"] == "output"
    assert chains["restore_scoped"]["type"] == "filter" and chains["restore_scoped"]["hook"] == "prerouting"
    assert number(chains["profile_scoped"]["prio"]) == -150 and number(chains["restore_scoped"]["prio"]) == -150
    cgroup_rules = []
    connection_rules = []
    for rule in output:
        if any("match" in item and "socket" in item["match"].get("left", {}) for item in rule["expr"]):
            matches_cgroup(rule)
            cgroup_rules.append(rule)
        else:
            connection_rules.append(rule)
    rendered = client.succeed("nft list chain inet oxidom profile_scoped")
    selectors = [line for line in rendered.splitlines() if "socket cgroupv2" in line]
    assert len(selectors) == len(cgroup_rules)
    expected_selector = 'socket cgroupv2 level 4 "' + pinned_record["path"] + '"'
    assert all(expected_selector in line for line in selectors)
    if blocked:
        assert len(cgroup_rules) == 1 and not connection_rules and not restore
        assert any("drop" in item for item in cgroup_rules[0]["expr"])
        assert not any("mangle" in item for item in cgroup_rules[0]["expr"])
        return {"drop": rule_counter(cgroup_rules[0])}
    assert len(cgroup_rules) == 4 and len(connection_rules) == 1 and len(restore) == 1
    mark = state_session()["interface"]["mark"]
    connection = connection_rules[0]
    connection_matches = [item["match"] for item in connection["expr"] if "match" in item]
    assert connection_matches == [{"op": "==", "left": {"ct": {"key": "mark"}}, "right": mark}]
    connection_marks = [item["mangle"] for item in connection["expr"] if "mangle" in item]
    assert connection_marks == [{"key": {"meta": {"key": "mark"}}, "value": mark}]
    marks = [item["mangle"] for item in output[0]["expr"] if "mangle" in item]
    assert len(marks) == 2
    assert marks[0]["key"] == {"meta": {"key": "mark"}} and number(marks[0]["value"]) == mark
    assert marks[1]["key"] == {"ct": {"key": "mark"}} and number(marks[1]["value"]) == mark
    assert any("reject" in item for item in output[3]["expr"])
    assert any(item.get("match", {}).get("left") == {"meta": {"key": "nfproto"}} and item["match"]["right"] == "ipv6" for item in output[3]["expr"])
    for index, address, prefix in [(1, "fe80::", 10), (2, "ff00::", 8)]:
        assert any(item.get("match", {}).get("right") == {"prefix": {"addr": address, "len": prefix}} for item in output[index]["expr"])
        assert any("accept" in item for item in output[index]["expr"])
    restore_matches = [item["match"] for item in restore[0]["expr"] if "match" in item]
    assert restore_matches == [{"op": "==", "left": {"ct": {"key": "mark"}}, "right": mark}]
    restore_marks = [item["mangle"] for item in restore[0]["expr"] if "mangle" in item]
    assert restore_marks == [{"key": {"meta": {"key": "mark"}}, "value": mark}]
    return {
        "mark": rule_counter(cgroup_rules[0]), "link_local": rule_counter(cgroup_rules[1]),
        "reject": rule_counter(cgroup_rules[3]), "restore": rule_counter(restore[0]),
        "connection": rule_counter(connection),
    }


def start_capture(label):
    captures = {}
    for suffix, device in [("nic", "eth1"), ("all", "any")]:
        unit = "p15-scoped-cap-" + label + "-" + suffix
        path = "/var/tmp/" + unit + ".pcap"
        command = [
            "tcpdump", "--immediate-mode", "-i", device, "-B", "16384",
            "-s", "0", "-U", "-w", path, "tcp",
        ]
        definition = "\n".join([
            "[Unit]", "Description=Private P15 scoped capture",
            "[Service]", "Type=exec", "KillSignal=SIGINT", "TimeoutStopSec=10",
            "Environment=LC_ALL=C", "ExecStart=/run/current-system/sw/bin/" + shlex.join(command), "",
        ])
        write_guest(client, "/run/systemd/system/" + unit + ".service", definition, mode=0o644)
        client.succeed("systemctl daemon-reload")
        client.succeed("systemctl start " + unit)
        client.wait_until_succeeds("journalctl -u " + unit + " --no-pager | grep -F " + quote("listening on " + device), timeout=30)
        captures[suffix] = (unit, path)
    return captures


def capture_read(path, expression, ascii_payload=False):
    command = ["tcpdump", "-nn", "-r", path]
    if ascii_payload:
        command.append("-A")
    return client.succeed(shlex.join([*command, expression]))


def live_marker(captures, marker, expression):
    for _unit, path in captures.values():
        source = "import subprocess; p=subprocess.run(" + repr(["tcpdump", "-nn", "-A", "-r", path, expression]) + ",capture_output=True,timeout=3); "
        source += "assert p.returncode==0; assert " + repr(marker.encode()) + " in p.stdout"
        client.wait_until_succeeds("python3 -c " + quote(source), timeout=20)


def capture_controls(captures, label):
    marker4 = "/p15-control-" + label + "-v4"
    marker6 = "/p15-control-" + label + "-v6"
    assert unscoped(GATEWAY4, marker4)["source"] == LAN4
    assert unscoped(GATEWAY6, marker6, family=6)["source"] == "2001:db8:42::2"
    live_marker(captures, marker4, "dst host " + GATEWAY4 + " and dst port 18080")
    live_marker(captures, marker6, "dst host " + GATEWAY6 + " and dst port 18080")
    return [(GATEWAY4, marker4), (GATEWAY6, marker6)]


def fault_capture_reports_zero_kernel_drops(journal):
    import re

    reports = []
    for line in journal.splitlines():
        if "packets dropped by kernel" not in line:
            continue
        if line.count("packets dropped by kernel") != 1:
            return False
        report = re.fullmatch(
            r"(?:(?:.*\s)?tcpdump\[[0-9]+\]:\s+)?([0-9]+) packets dropped by kernel",
            line.strip(),
        )
        if report is None:
            return False
        reports.append(report[1])
    # One unique capture unit must produce one canonical terminal zero count.
    # Missing, malformed and repeated summaries cannot establish completeness.
    return reports == ["0"]


def finish_capture(captures, controls, candidate_markers=()):
    for unit, _path in captures.values():
        client.succeed("systemctl stop " + unit)
        assert client.succeed("systemctl show -p ExecMainStatus --value " + unit).strip() == "0"
        journal = client.succeed("journalctl -u " + unit + " --no-pager")
        assert fault_capture_reports_zero_kernel_drops(journal), journal
        statistics = [line for line in journal.splitlines() if line.partition(" ")[0].isdecimal() and " packets " in line]
        print("Scoped routing capture statistics: " + unit + " " + json.dumps(statistics))
    for _unit, path in captures.values():
        for address, marker in [*controls, *candidate_markers]:
            port = 1080 if address in CANDIDATES else 18080
            assert marker in capture_read(path, "dst host " + address + " and dst port " + str(port), ascii_payload=True)
    nic = captures["nic"][1]
    for address in [SINK4, SINK6, CANDIDATES[0]]:
        expression = "dst host " + address + " and dst port 18080"
        escaped = capture_read(nic, expression).strip()
        print("Scoped data-NIC check " + address + ": " + repr(escaped))
        assert not escaped, "direct scoped HTTP escaped on the data NIC: " + escaped


def scoped_success(host, path, family=4):
    result = actor("request", host=host, path=path, family=family)
    assert result["ok"], result
    if family == 4:
        assert result["body"]["source"] != LAN4
    assert result["body"]["path"] == path
    assert path in peer.succeed("journalctl -u p15-scoped-http --no-pager")
    return result


def scoped_blocked(host, path, family=4):
    result = actor("request", host=host, path=path, family=family)
    assert not result["ok"], result
    assert path not in peer.succeed("journalctl -u p15-scoped-http --no-pager")
    return result


def core_identity():
    pid = state_session()["xray_pid"]
    parent = int(client.succeed("systemctl show -p MainPID --value oxidom"))
    uid = int(client.succeed("id -u oxidom"))
    source = r'''
import json, os, pathlib, select
pid, parent, uid = PARAMETERS
descriptor = os.pidfd_open(pid)
try:
    root = pathlib.Path("/proc") / str(pid)
    before = (root / "stat").read_text().rsplit(")", 1)[1].split()
    argv = (root / "cmdline").read_bytes().rstrip(b"\0").decode().split("\0")
    actual_uid = int(next(line.split()[1] for line in (root / "status").read_text().splitlines() if line.startswith("Uid:")))
    after = (root / "stat").read_text().rsplit(")", 1)[1].split()
    assert before[19] == after[19] and int(after[1]) == parent and actual_uid == uid
    assert argv[-2:] == ["-c", "/var/lib/oxidom/current-config-scoped.json"]
    assert "xray" in pathlib.Path(os.readlink(root / "exe")).name
    observer = select.poll()
    observer.register(descriptor, select.POLLIN)
    assert not observer.poll(0)
    print(json.dumps({"pid": pid, "parent": parent, "uid": uid, "starttime": after[19], "argv": argv}))
finally:
    os.close(descriptor)
'''.replace("PARAMETERS", repr((pid, parent, uid)))
    return json.loads(guest_python(client, source))


def kill_core(identity):
    source = r'''
import json, os, pathlib, select, signal, time
expected = PARAMETERS
pid = expected["pid"]
descriptor = os.pidfd_open(pid)
try:
    root = pathlib.Path("/proc") / str(pid)
    stat = (root / "stat").read_text().rsplit(")", 1)[1].split()
    argv = (root / "cmdline").read_bytes().rstrip(b"\0").decode().split("\0")
    uid = int(next(line.split()[1] for line in (root / "status").read_text().splitlines() if line.startswith("Uid:")))
    assert stat[19] == expected["starttime"] and int(stat[1]) == expected["parent"]
    assert argv == expected["argv"] and uid == expected["uid"]
    observer = select.poll()
    observer.register(descriptor, select.POLLIN)
    assert not observer.poll(0)
    injected_ns = time.monotonic_ns()
    signal.pidfd_send_signal(descriptor, signal.SIGKILL)
    assert observer.poll(5000)
    print(injected_ns)
finally:
    os.close(descriptor)
'''.replace("PARAMETERS", repr(identity))
    return int(guest_python(client, source))


def up_without_automatic_recovery():
    cli("profile", "failover", PROFILE, "--enable", "--json")
    cli("up", PROFILE)
    wait_session("s['state']=='connected'")
    cli("profile", "failover", PROFILE, "--disable", "--json")
    wait_session("s['runtime']['failover']['state']=='disarmed'")
    interface = state_session()["interface"]
    assert interface["ownership"] and interface["rule"] is False
    device = interface["device"]
    assert client.succeed("cat /proc/sys/net/ipv4/conf/" + device + "/src_valid_mark").strip() == "1"
    assert client.succeed("cat /proc/sys/net/ipv4/conf/" + device + "/rp_filter").strip() == "2"
    assert kernel_json("ip -j -4 route get " + SINK4)[0]["dev"] == "eth1"
    assert kernel_json("ip -j -4 route get " + SINK4 + " mark " + str(interface["mark"]))[0]["dev"] == device
    for address in CANDIDATES:
        assert kernel_json("ip -j -4 route get " + address + " mark " + str(interface["mark"]))[0]["dev"] == device
    return interface


start_all()
client.wait_for_unit("oxidom.service", timeout=60)
client.wait_for_unit("user@1000.service", timeout=60)
client.wait_until_succeeds("test -S /run/user/1000/systemd/private && test -S /run/user/1000/bus", timeout=30)
peer.wait_for_unit("p15-scoped-http.service", timeout=60)
peer.wait_for_unit("p15-scoped-xray.service", timeout=60)
for address in CANDIDATES:
    peer.wait_for_open_port(1080, addr=address, timeout=30)
peer.wait_for_open_port(18080, addr=SINK4, timeout=30)
peer.wait_for_open_port(18080, addr=SINK6, timeout=30)

with subtest("real IPv6 and unprivileged user manager are available"):
    assert client.succeed(user_command("id -u")).strip() == "1000"
    assert client.succeed("xray version").startswith("Xray 26.3.27 ")
    assert peer.succeed("xray version").startswith("Xray 26.3.27 ")
    assert not any(item["addr_info"] for item in kernel_json("ip -j -4 addr show dev eth0"))
    client.succeed("ip -6 route flush default dev eth0 || true")
    peer.succeed("ip -6 route flush default dev eth0 || true")
    assert not [r for r in kernel_json("ip -j -6 route show default") if r.get("gateway")]
    assert not json.loads(peer.succeed("ip -j -4 route show default"))
    assert not [r for r in json.loads(peer.succeed("ip -j -6 route show default")) if r.get("gateway")]
    assert unscoped(SINK4, "/p15-unscoped-baseline-v4")["source"] == LAN4
    assert unscoped(SINK6, "/p15-unscoped-baseline-v6", family=6)["source"] == "2001:db8:42::2"
    imported = rpc("ImportLinks", "s", "socks://192.0.2.10:1080#ScopedA\nsocks://192.0.2.11:1080#ScopedB")
    assert imported["data"][0] == 2
    servers = json.loads(cli("list", "servers", "--json"))
    ids = [next(item["id"] for item in servers if item["address"] == address) for address in CANDIDATES]
    profile = {
        "description": "Invented isolated per-application routing fixture",
        "select": {"server": ids[0]},
        "proxy": {"socks_port": 10808, "http_port": 10809},
        "interface": {"enable": True, "device": "oxi-scoped", "routes": "manual"},
        "on_core_exit": "hold",
        "failover": {"enabled": True, "members": ids, "max_attempts": 1, "cooldown_secs": 60},
    }
    rpc("SaveProfile", "ss", PROFILE, json.dumps(profile))
    up_without_automatic_recovery()

with subtest("one real oxidom run scope marks traffic and refuses only scoped global IPv6"):
    write_guest(client, ACTOR_PATH, ACTOR_SOURCE, mode=0o700, uid=1000)
    launcher = [
        "systemd-run", "--user", "--unit=p15-scoped-launcher", "--service-type=exec",
        "--property=RuntimeMaxSec=600",
        "--setenv=HOME=/home/tester", "--setenv=PATH=/run/current-system/sw/bin",
        "--setenv=XDG_RUNTIME_DIR=/run/user/1000",
        "--setenv=DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus",
        "--setenv=OXIDOM_SESSION_DAEMON=external",
        "oxidom", "run", "--profile", PROFILE, "--", "python3", "-u", ACTOR_PATH,
    ]
    client.succeed(user_command(shlex.join(launcher)))
    client.wait_until_succeeds("test -S " + ACTOR_SOCKET, timeout=30)
    pinned_actor, pinned_record, pinned_inode = record_and_identity()
    captures = start_capture("healthy")
    controls = capture_controls(captures, "healthy-before")
    counters = nft_snapshot()
    marker = "/p15-scoped-candidate-a"
    scoped_success(SINK4, marker)
    live_marker(captures, marker, "dst host 192.0.2.10 and dst port 1080")
    after = nft_snapshot()
    assert after["mark"] > counters["mark"] and after["restore"] > counters["restore"]
    candidate_marker = "/p15-scoped-no-main-bypass"
    scoped_success(CANDIDATES[0], candidate_marker)
    live_marker(captures, candidate_marker, "dst host 192.0.2.10 and dst port 1080")
    before_v6 = nft_snapshot()
    scoped_blocked(SINK6, "/p15-scoped-global-v6-refused", family=6)
    after_v6 = nft_snapshot()
    assert after_v6["reject"] > before_v6["reject"]
    ll_before = nft_snapshot()
    scoped_success("fe80::1", "/p15-scoped-link-local-v6", family=6)
    ll_after = nft_snapshot()
    assert ll_after["link_local"] > ll_before["link_local"] and ll_after["reject"] == ll_before["reject"]
    same_actor()
    controls.extend(capture_controls(captures, "healthy-after"))
    finish_capture(captures, controls, [(CANDIDATES[0], marker), (CANDIDATES[0], candidate_marker)])

with subtest("core loss blocks the same scope throughout an observed request window"):
    captures = start_capture("core")
    controls = capture_controls(captures, "core-before")
    identity = core_identity()
    begun = actor("begin", label="core")
    window_marker = begun["first_success"]["path"]
    live_marker(captures, window_marker, "dst host 192.0.2.10 and dst port 1080")
    injected_ns = kill_core(identity)
    assert begun["first_success"]["end_ns"] <= injected_ns
    wait_session("s['holding_traffic'] and s['runtime']['failover']['state']=='disarmed'")
    blocked = nft_snapshot(blocked=True)
    scoped_blocked(SINK4, "/p15-core-held-v4")
    scoped_blocked(SINK6, "/p15-core-held-v6", family=6)
    assert nft_snapshot(blocked=True)["drop"] > blocked["drop"]
    stable_ns = guest_ns()
    window = actor("end", after_ns=stable_ns)
    assert window["first_start_ns"] <= injected_ns < stable_ns < window["last_start_ns"]
    assert window["attempts"] >= 2 and window["successes"] > 0 and window["failures"] > 0
    same_actor()
    controls.extend(capture_controls(captures, "core-after"))
    finish_capture(captures, controls, [(CANDIDATES[0], window_marker)])
    cli("down", PROFILE)
    up_without_automatic_recovery()
    same_actor()
    nft_snapshot()
    scoped_success(SINK4, "/p15-core-explicit-up-same-scope")

with subtest("explicit Down blocks a live scope before releasing its IPv4 routing domain"):
    captures = start_capture("down")
    controls = capture_controls(captures, "down-before")
    interface = state_session()["interface"]
    begun = actor("begin", label="down")
    window_marker = begun["first_success"]["path"]
    live_marker(captures, window_marker, "dst host 192.0.2.10 and dst port 1080")
    invoked_ns = guest_ns()
    cli("down", PROFILE)
    assert begun["first_success"]["end_ns"] <= invoked_ns
    client.fail("ip link show dev " + interface["device"])
    assert not any(item["profile"] == PROFILE for item in saved_state().get("sessions", []))
    blocked = nft_snapshot(blocked=True)
    scoped_blocked(SINK4, "/p15-down-held-v4")
    scoped_blocked(SINK6, "/p15-down-held-v6", family=6)
    assert nft_snapshot(blocked=True)["drop"] > blocked["drop"]
    stable_ns = guest_ns()
    window = actor("end", after_ns=stable_ns)
    assert window["first_start_ns"] <= invoked_ns < stable_ns < window["last_start_ns"]
    assert window["attempts"] >= 2 and window["successes"] > 0 and window["failures"] > 0
    same_actor()
    controls.extend(capture_controls(captures, "down-after"))
    finish_capture(captures, controls, [(CANDIDATES[0], window_marker)])
    assert unscoped(SINK4, "/p15-down-unscoped-v4")["source"] == LAN4
    assert unscoped(SINK6, "/p15-down-unscoped-v6", family=6)["source"] == "2001:db8:42::2"
    up_without_automatic_recovery()
    same_actor()
    nft_snapshot()
    scoped_success(SINK4, "/p15-down-explicit-up-same-scope")

with subtest("the next teardown prunes a scope that has actually ended"):
    assert actor("quit") == {"quitting": True}
    client.wait_until_fails("test -e /proc/" + str(pinned_actor["pid"]), timeout=15)
    user_systemctl("stop", "p15-scoped-launcher.service")
    user_systemctl("stop", "oxidom\\x2dscoped.slice")
    client.wait_until_fails("test -d " + quote("/sys/fs/cgroup/" + pinned_record["path"]), timeout=15)
    cli("down", PROFILE)
    assert not any(item["profile"] == PROFILE for item in saved_state().get("run_cgroups", []))
    table = kernel_json("nft -j list table inet oxidom")["nftables"]
    assert not [item for item in table if "chain" in item and item["chain"]["name"] in {"profile_scoped", "restore_scoped"}]
    assert not [item for item in table if "rule" in item and item["rule"]["chain"] in {"profile_scoped", "restore_scoped"}]
    client.fail("ip link show dev oxi-scoped")

print("Scoped routing VM passed: real user scope, mark/restore, scoped IPv6 refusal, core/Down request windows, same-scope Up, and pruning")
