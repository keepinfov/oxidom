"""Continues the isolated routing driver with transition and recovery evidence."""

import copy


RECOVERY_PROFILE = "recovery-work"
RECOVERY_DEVICE = "oxi-recovery"
RECOVERY_OBSERVER = "/var/lib/oxidom/p15-helper-observer"
RECOVERY_ACTOR_ROOT = "/run/p15-recovery-actor"
RECOVERY_FOREIGN_TABLE = 65000
RECOVERY_FOREIGN_MARK = 0x51515151


def recovery_now():
    return int(fault_python(client, "import time; print(time.monotonic_ns())"))


def recovery_session_state():
    return fault_state_session(RECOVERY_PROFILE)


def recovery_session():
    return fault_session(RECOVERY_PROFILE)


def recovery_plan():
    interface = copy.deepcopy(recovery_session_state()["interface"])
    interface.pop("tun2socks_pid", None)
    return interface


def recovery_kernel_plan():
    return {
        "routes": kernel_json("ip -j -4 route show table all proto 242"),
        "rules": [
            rule for rule in kernel_json("ip -j -4 rule show")
            if str(rule.get("protocol")) == "242"
        ],
    }


def recovery_generic_identity(pid, executable, parent, required=()):
    source = r"""
import json, os, pathlib, select
pid, executable, parent, required = PARAMETERS
descriptor = os.pidfd_open(pid)
try:
    root = pathlib.Path('/proc') / str(pid)
    before = (root / 'stat').read_text().rsplit(')', 1)[1].split()
    argv = (root / 'cmdline').read_bytes().rstrip(b'\0').decode().split('\0')
    uid = int(next(line.split()[1] for line in (root / 'status').read_text().splitlines() if line.startswith('Uid:')))
    expected_uid = int(__import__('pwd').getpwnam('oxidom').pw_uid)
    assert uid == expected_uid and int(before[1]) == parent
    assert executable in pathlib.Path(os.readlink(root / 'exe')).name
    assert all(value in argv for value in required)
    after = (root / 'stat').read_text().rsplit(')', 1)[1].split()
    assert before[19] == after[19]
    observer = select.poll()
    observer.register(descriptor, select.POLLIN)
    assert not observer.poll(0)
    print(json.dumps({'pid': pid, 'starttime': after[19], 'uid': uid,
                      'parent': int(after[1]), 'argv': argv, 'exe': os.readlink(root / 'exe')}))
finally:
    os.close(descriptor)
""".replace("PARAMETERS", repr((pid, executable, parent, list(required))))
    return json.loads(fault_python(client, source))


def recovery_daemon_identity():
    pid = int(client.succeed("systemctl show -p MainPID --value oxidom"))
    identity = recovery_generic_identity(pid, "oxidom", 1, ("daemon", "--system"))
    group = client.succeed("cat /proc/" + str(pid) + "/cgroup")
    assert "0::/system.slice/oxidom.service" in group.splitlines()
    return identity


def recovery_helper_identity():
    session = recovery_session_state()
    daemon = int(client.succeed("systemctl show -p MainPID --value oxidom"))
    interface = session["interface"]
    pid = interface["tun2socks_pid"]
    real_executable = client.succeed("readlink -f " + quote(real_helper_path)).strip()
    # Durable spawn PID is not sustained or even completed exec readiness.
    # Observe the real executable before attributing this fixture canary to it.
    actual_exec = "import os; assert os.path.realpath('/proc/" + str(pid) + "/exe') == " + repr(real_executable)
    client.wait_until_succeeds("python3 -c " + quote(actual_exec), timeout=15)
    identity = recovery_generic_identity(
        pid, "tun2socks", daemon,
        ("--device", interface["device"], "--proxy",
         "socks5://" + session["address"] + ":" + str(session["socks_port"])),
    )
    position = identity["argv"].index("--device")
    assert identity["argv"][position + 1] == interface["device"]
    assert identity["exe"] == real_executable
    return identity


def recovery_signal(identity):
    source = r"""
import json, os, pathlib, select, signal, time
expected = PARAMETERS
descriptor = os.pidfd_open(expected['pid'])
try:
    root = pathlib.Path('/proc') / str(expected['pid'])
    before = (root / 'stat').read_text().rsplit(')', 1)[1].split()
    argv = (root / 'cmdline').read_bytes().rstrip(b'\0').decode().split('\0')
    uid = int(next(line.split()[1] for line in (root / 'status').read_text().splitlines() if line.startswith('Uid:')))
    after = (root / 'stat').read_text().rsplit(')', 1)[1].split()
    assert before[19] == after[19] == expected['starttime']
    assert int(after[1]) == expected['parent'] and uid == expected['uid']
    assert argv == expected['argv']
    observer = select.poll()
    observer.register(descriptor, select.POLLIN)
    assert not observer.poll(0)
    injected_ns = time.monotonic_ns()
    signal.pidfd_send_signal(descriptor, signal.SIGKILL)
    assert observer.poll(5000), 'the pinned fault target did not exit'
    print(json.dumps({'injected_ns': injected_ns}))
finally:
    os.close(descriptor)
""".replace("PARAMETERS", repr(identity))
    return json.loads(fault_python(client, source))["injected_ns"]


def recovery_assert_alive(identity, parent=None):
    actual = recovery_generic_identity(
        identity["pid"],
        "tun2socks" if "--device" in identity["argv"] else "xray",
        identity["parent"] if parent is None else parent,
    )
    assert actual["starttime"] == identity["starttime"]
    assert actual["argv"] == identity["argv"] and actual["uid"] == identity["uid"]


def recovery_wait_gone(identity, timeout=15):
    source = (
        "import pathlib; p=pathlib.Path('/proc')/" + repr(str(identity["pid"]))
        + "; assert not p.exists() or p.joinpath('stat').read_text().rsplit(')',1)[1].split()[19] != "
        + repr(identity["starttime"])
    )
    client.wait_until_succeeds("python3 -c " + quote(source), timeout=timeout)


def recovery_observer_records():
    source = r"""
import json, pathlib
path = pathlib.Path(PARAMETER)
data = path.read_bytes()
assert len(data) <= 65536
print(json.dumps([json.loads(line) for line in data.splitlines()]))
""".replace("PARAMETER", repr(RECOVERY_OBSERVER + "/starts.jsonl"))
    return json.loads(fault_python(client, source))


def recovery_install_helper_observer():
    # Binary-path choice is a root-admin fixture operation performed only with
    # the daemon and every session stopped. System-bus SetSettings stays guarded.
    assert not saved_state().get("sessions", [])
    original = fault_rpc_json("GetSettings")
    real_helper = original["tun2socks_binary"]
    assert real_helper.startswith("/nix/store/") and real_helper.endswith("/bin/tun2socks")
    client.succeed("systemctl stop oxidom.service")
    client.fail("systemctl is-active oxidom.service")
    uid = int(client.succeed("id -u oxidom"))
    gid = int(client.succeed("id -g oxidom"))
    client.succeed("install -d -m0700 -o " + str(uid) + " -g " + str(gid) + " " + RECOVERY_OBSERVER)
    wrapper = r"""#!/run/current-system/sw/bin/python3
import fcntl, json, os, stat, sys
REAL = REAL_VALUE
JOURNAL = JOURNAL_VALUE
args = sys.argv[1:]
if args[:1] == ['--device']:
    descriptor = os.open(JOURNAL, os.O_WRONLY | os.O_APPEND | os.O_CLOEXEC | os.O_NOFOLLOW)
    try:
        fcntl.flock(descriptor, fcntl.LOCK_EX)
        info = os.fstat(descriptor)
        assert stat.S_ISREG(info.st_mode) and info.st_uid == os.getuid()
        assert stat.S_IMODE(info.st_mode) == 0o600 and info.st_nlink == 1
        record = (json.dumps({'pid': os.getpid(), 'uid': os.getuid(), 'argv': args, 'real': REAL}) + '\n').encode()
        assert len(record) < 4096 and info.st_size + len(record) <= 65536
        assert os.write(descriptor, record) == len(record)
        os.fsync(descriptor)
    finally:
        os.close(descriptor)
os.execv(REAL, [REAL, *args])
""".replace("REAL_VALUE", repr(real_helper)).replace("JOURNAL_VALUE", repr(RECOVERY_OBSERVER + "/starts.jsonl"))
    fault_write_text(client, RECOVERY_OBSERVER + "/tun2socks-observer", wrapper)
    client.succeed("chown " + str(uid) + ":" + str(gid) + " " + RECOVERY_OBSERVER + "/tun2socks-observer")
    client.succeed("chmod 0700 " + RECOVERY_OBSERVER + "/tun2socks-observer")
    setup = r"""
import json, os, pathlib, re, stat, tempfile, tomllib
directory, uid, gid = PARAMETERS
journal = pathlib.Path(directory) / 'starts.jsonl'
descriptor = os.open(journal, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_CLOEXEC, 0o600)
os.fchown(descriptor, uid, gid)
os.fsync(descriptor)
os.close(descriptor)
parent = os.open(directory, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
try:
    os.fsync(parent)
finally:
    os.close(parent)
path = pathlib.Path('/var/lib/oxidom/config.toml')
before_stat = path.stat()
assert stat.S_IMODE(before_stat.st_mode) == 0o600
text = path.read_text()
before = tomllib.loads(text)
replacement = 'tun2socks_binary = ' + json.dumps(directory + '/tun2socks-observer')
updated, count = re.subn(r'^tun2socks_binary\s*=\s*".*"\s*$', replacement, text, flags=re.MULTILINE)
assert count == 1
after = tomllib.loads(updated)
expected = dict(before)
expected['tun2socks_binary'] = directory + '/tun2socks-observer'
assert after == expected
descriptor, temporary = tempfile.mkstemp(prefix='.p15-helper-', dir=path.parent)
try:
    os.fchmod(descriptor, 0o600)
    os.fchown(descriptor, before_stat.st_uid, before_stat.st_gid)
    data = updated.encode()
    with os.fdopen(descriptor, 'wb') as output:
        output.write(data)
        output.flush()
        os.fsync(output.fileno())
    os.replace(temporary, path)
    parent = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        os.fsync(parent)
    finally:
        os.close(parent)
finally:
    if os.path.exists(temporary):
        os.unlink(temporary)
actual = path.stat()
assert actual.st_uid == before_stat.st_uid and actual.st_gid == before_stat.st_gid
assert stat.S_IMODE(actual.st_mode) == 0o600
print(json.dumps({'real_helper': before['tun2socks_binary']}))
""".replace("PARAMETERS", repr((RECOVERY_OBSERVER, uid, gid)))
    result = json.loads(fault_python(client, setup))
    assert result["real_helper"] == real_helper
    client.succeed("systemctl start oxidom.service")
    client.wait_for_unit("oxidom.service", timeout=60)
    settings = fault_rpc_json("GetSettings")
    expected = dict(original)
    expected["tun2socks_binary"] = RECOVERY_OBSERVER + "/tun2socks-observer"
    assert settings == expected
    return real_helper


def recovery_save(mode="default"):
    profile = {
        "description": "Invented real routing recovery fixture",
        "select": {"server": ids[0]},
        "proxy": {"socks_port": 10808, "http_port": 10809},
        "interface": {"enable": True, "device": RECOVERY_DEVICE, "routes": mode,
                      "list": [SINK + "/32"] if mode == "list" else []},
        "on_core_exit": "hold",
        "failover": {"enabled": True, "members": ids, "max_attempts": 1, "cooldown_secs": 1},
    }
    rpc("SaveProfile", "ss", RECOVERY_PROFILE, json.dumps(profile))
    cli("up", RECOVERY_PROFILE)
    fault_wait_session(RECOVERY_PROFILE, fault_current_predicate(0))
    interface = check_owned_plan(RECOVERY_PROFILE, mode)
    return interface, fault_core_identity(RECOVERY_PROFILE), recovery_helper_identity()


def recovery_canary(label):
    marker = "/p15-recovery-" + label
    body = curl("http://" + SINK + ":18080" + marker)
    assert body["path"] == marker and body["source"] != LAN_CLIENT
    return marker


def recovery_lan_control(label):
    marker = "/p15-recovery-" + label
    assert curl("http://" + GATEWAY + ":18080" + marker) == {"path": marker, "source": LAN_CLIENT}
    return marker


def recovery_capture(label):
    result = {}
    for suffix, device in [("nic", "eth1"), ("all", "any")]:
        unit = "p15-recovery-cap-" + label + "-" + suffix
        path = "/var/tmp/" + unit + ".pcap"
        expression = "tcp and (host " + SINK + " or host " + GATEWAY + " or net 192.0.2.0/24)"
        definition = "\n".join([
            "[Unit]", "Description=Isolated P15 recovery capture",
            "[Service]", "Type=exec", "KillSignal=SIGINT", "TimeoutStopSec=10",
            "Environment=LC_ALL=C", "ExecStart=/run/current-system/sw/bin/" + shlex.join([
                "tcpdump", "--immediate-mode", "-i", device, "-B", "4096",
                "-s", "0", "-U", "-w", path, expression,
            ]), "",
        ])
        fault_write_text(client, "/run/systemd/system/" + unit + ".service", definition)
        client.succeed("systemctl daemon-reload")
        client.succeed("systemctl start " + unit)
        client.wait_until_succeeds(
            "journalctl -u " + unit + " --no-pager | grep -F " + quote("listening on " + device),
            timeout=30,
        )
        result[suffix] = (unit, path)
    return result


def recovery_packet_text(captures, suffix, expression):
    return client.succeed(shlex.join([
        "tcpdump", "-nn", "-A", "-r", captures[suffix][1], expression,
    ]))


def recovery_capture_gate(captures, expression, marker):
    for unit, path in captures.values():
        observed = shlex.join(["tcpdump", "-nn", "-A", "-r", path, expression])
        observed += " | " + shlex.join(["grep", "-F", "--", marker])
        command = "systemctl is-active --quiet " + quote(unit)
        command += " && bash -o pipefail -c " + quote(observed)
        client.wait_until_succeeds(command, timeout=30)


def recovery_close_capture(captures, controls, no_direct=True):
    try:
        for expression, marker in controls:
            recovery_capture_gate(captures, expression, marker)
    finally:
        errors = []
        for unit, _path in captures.values():
            try:
                client.succeed("systemctl stop " + unit, timeout=20)
            except Exception as error:
                errors.append(unit + ": " + str(error))
        if errors:
            raise RuntimeError("capture cleanup failed: " + "; ".join(errors))
    for unit, _path in captures.values():
        assert client.succeed("systemctl show -p ExecMainStatus --value " + unit).strip() == "0"
        journal = client.succeed("journalctl -u " + unit + " --no-pager -o cat")
        assert fault_capture_reports_zero_kernel_drops(journal)
        statistics = [line for line in journal.splitlines() if line.partition(" ")[0].isdecimal() and " packets " in line]
        print("Routing recovery capture statistics: " + unit + " " + json.dumps(statistics))
    for suffix in ["nic", "all"]:
        for expression, marker in controls:
            assert marker in recovery_packet_text(captures, suffix, expression)
    if no_direct:
        direct = client.succeed(shlex.join([
            "tcpdump", "-nn", "-r", captures["nic"][1],
            "dst host " + SINK + " and dst port 18080",
        ]))
        assert not direct.strip(), "direct sink TCP appeared in the observed recovery window"
    outside = client.succeed(shlex.join([
        "tcpdump", "-nn", "-r", captures["all"][1],
        "dst host " + OUTSIDE + " and dst port 1080",
    ]))
    assert not outside.strip(), "an unauthorized candidate dial appeared in the observed recovery window"


RECOVERY_ACTOR_SOURCE = r"""
import http.client
import json
import os
import pathlib
import socket
import threading
import time

ROOT = pathlib.Path(__file__).parent
CONTROL = ROOT / 'control.sock'
JOURNAL = ROOT / 'requests.jsonl'
SINK = SINK_VALUE
MAX_REQUESTS = 16384
WINDOW_SECONDS = 45
records = []
active = None
guard = threading.Lock()
done = threading.Event()


def request(label, ordinal):
    marker = '/p15-recovery-window-' + label + '-' + str(ordinal)
    started = time.monotonic_ns()
    result = {'marker': marker, 'started_ns': started}
    connection = http.client.HTTPConnection(SINK, 18080, timeout=1)
    try:
        connection.request('GET', marker, headers={'Host': SINK + ':18080', 'Connection': 'close'})
        response = connection.getresponse()
        body = json.loads(response.read(4097))
        assert response.status == 200 and body['path'] == marker
        result.update(ok=True, source=body['source'])
    except (OSError, http.client.HTTPException) as error:
        result.update(ok=False, error=type(error).__name__)
    finally:
        connection.close()
    result['finished_ns'] = time.monotonic_ns()
    return result


def produce(state):
    descriptor = os.open(JOURNAL, os.O_WRONLY | os.O_CREAT | os.O_TRUNC | os.O_CLOEXEC, 0o600)
    try:
        os.fchmod(descriptor, 0o600)
        for ordinal in range(MAX_REQUESTS):
            if time.monotonic() >= state['deadline']:
                raise RuntimeError('producer exceeded its finite observation window')
            item = request(state['label'], ordinal)
            data = (json.dumps(item) + '\n').encode()
            assert len(data) < 4096
            assert os.write(descriptor, data) == len(data)
            with guard:
                records.append(item)
                if len(records) == 1:
                    if not item['ok']:
                        raise RuntimeError('initial window canary did not complete successfully')
                    state['first'].set()
                stable = state.get('stable_ns')
                expected = state.get('expected')
                elapsed = state.get('minimum_after_ms', 0) * 1_000_000
                if stable is not None and item['started_ns'] >= stable and item['finished_ns'] >= stable + elapsed:
                    if expected == 'blocked' and not item['ok'] or expected == 'success' and item['ok']:
                        state['last'] = item
                        return
        raise RuntimeError('producer exceeded its request bound')
    except BaseException as error:
        state['error'] = str(error)
        state['first'].set()
    finally:
        os.close(descriptor)
        done.set()


CONTROL.unlink(missing_ok=True)
listener = socket.socket(socket.AF_UNIX)
listener.bind(str(CONTROL))
listener.listen(1)
listener.settimeout(90)
while True:
    connection, _address = listener.accept()
    with connection:
        connection.settimeout(60)
        incoming = connection.makefile('rb').readline(65537)
        assert len(incoming) <= 65536 and incoming.endswith(b'\n')
        command = json.loads(incoming)
        if command['command'] == 'hello':
            caps = next(line.split()[1] for line in pathlib.Path('/proc/self/status').read_text().splitlines() if line.startswith('CapEff:'))
            answer = {'pid': os.getpid(), 'uid': os.getuid(), 'cap_eff': int(caps, 16)}
        elif command['command'] == 'begin':
            assert active is None
            done.clear()
            records.clear()
            active = {'label': command['label'], 'deadline': time.monotonic() + WINDOW_SECONDS, 'first': threading.Event()}
            threading.Thread(target=produce, args=(active,), daemon=True).start()
            assert active['first'].wait(10), 'first canary deadline expired'
            if 'error' in active:
                raise RuntimeError(active['error'])
            answer = records[0]
        elif command['command'] == 'end':
            assert active is not None
            with guard:
                active['stable_ns'] = command['stable_ns']
                active['expected'] = command['expected']
                active['minimum_after_ms'] = command.get('minimum_after_ms', 0)
            assert done.wait(50), 'producer failed to finish'
            if 'error' in active:
                raise RuntimeError(active['error'])
            assert active.get('last') is not None
            answer = {'first': records[0], 'last': active['last'], 'count': len(records)}
            active = None
        elif command['command'] == 'shutdown':
            assert active is None
            connection.sendall(b'{"stopped":true}\n')
            break
        else:
            raise ValueError('unknown producer command')
        connection.sendall((json.dumps(answer) + '\n').encode())
listener.close()
CONTROL.unlink()
""".replace("SINK_VALUE", repr(SINK))


def recovery_actor_call(command, timeout=20):
    source = r"""
import json, socket
command, path, timeout = PARAMETERS
with socket.socket(socket.AF_UNIX) as connection:
    connection.settimeout(timeout)
    connection.connect(path)
    connection.sendall((json.dumps(command) + '\n').encode())
    response = connection.makefile('rb').readline(65537)
    assert len(response) <= 65536 and response.endswith(b'\n')
    print(json.dumps(json.loads(response)))
""".replace("PARAMETERS", repr((command, RECOVERY_ACTOR_ROOT + "/control.sock", timeout)))
    return json.loads(fault_python(client, source, timeout=timeout + 5))


def recovery_actor_start():
    client.succeed("install -d -m0700 -o tester -g users " + RECOVERY_ACTOR_ROOT)
    fault_write_text(client, RECOVERY_ACTOR_ROOT + "/actor.py", RECOVERY_ACTOR_SOURCE)
    definition = "\n".join([
        "[Unit]", "Description=Unprivileged finite P15 request producer",
        "[Service]", "Type=exec", "User=tester", "Group=users",
        "NoNewPrivileges=yes", "CapabilityBoundingSet=", "AmbientCapabilities=",
        "TimeoutStopSec=10", "RuntimeMaxSec=600",
        "ExecStart=/run/current-system/sw/bin/python3 -u " + RECOVERY_ACTOR_ROOT + "/actor.py", "",
    ])
    fault_write_text(client, "/run/systemd/system/p15-recovery-producer.service", definition)
    client.succeed("systemctl daemon-reload")
    client.succeed("systemctl start p15-recovery-producer.service")
    client.wait_until_succeeds("test -S " + RECOVERY_ACTOR_ROOT + "/control.sock", timeout=30)
    identity = recovery_actor_call({"command": "hello"})
    assert identity["uid"] == 1000 and identity["cap_eff"] == 0
    return identity


def recovery_actor_begin(label, captures):
    first = recovery_actor_call({"command": "begin", "label": label})
    assert first["ok"] and first["source"] != LAN_CLIENT
    expression = "dst host " + CANDIDATES[0] + " and dst port 1080"
    recovery_capture_gate(captures, expression, first["marker"])
    return first


def recovery_actor_end(stable_ns, injected_ns, expected="success", minimum_after_ms=0):
    result = recovery_actor_call({
        "command": "end", "stable_ns": stable_ns, "expected": expected,
        "minimum_after_ms": minimum_after_ms,
    }, timeout=60)
    assert result["last"]["started_ns"] >= stable_ns
    assert result["last"]["finished_ns"] >= stable_ns + minimum_after_ms * 1_000_000
    assert result["last"]["ok"] == (expected == "success")
    source = r"""
import json, pathlib
path, injected_ns, stable_ns = PARAMETERS
data = pathlib.Path(path).read_bytes()
assert len(data) <= 16 * 1024 * 1024
records = [json.loads(line) for line in data.splitlines()]
assert records and len(records) <= 16384
assert any(injected_ns <= item['started_ns'] < stable_ns for item in records), 'no fresh socket was observed inside the fault-to-stable window'
assert not [item for item in records if item.get('ok') and item['source'] == '10.42.0.2'], 'the producer received a direct sink response'
print(json.dumps({'count': len(records), 'failed': sum(not item['ok'] for item in records)}))
""".replace("PARAMETERS", repr((RECOVERY_ACTOR_ROOT + "/requests.jsonl", injected_ns, stable_ns)))
    summary = json.loads(fault_python(client, source))
    assert summary["count"] == result["count"]
    result["summary"] = summary
    print("Routing active producer evidence: " + json.dumps({
        "requests": summary["count"], "failed": summary["failed"],
        "first_started_ns": result["first"]["started_ns"],
        "fault_injected_ns": injected_ns, "stable_ns": stable_ns,
        "last_started_ns": result["last"]["started_ns"],
        "last_finished_ns": result["last"]["finished_ns"],
        "minimum_after_ms": minimum_after_ms,
    }, sort_keys=True))
    return result


def recovery_assert_owned_absent(interface):
    owner = interface["ownership"]
    client.fail("ip link show dev " + interface["device"])
    assert not [
        item for item in kernel_json("ip -j -4 route show table all proto 242")
        if item.get("metric") == owner["metric"]
    ]
    assert not [
        item for item in kernel_json("ip -j -4 rule show")
        if item.get("priority") == owner["rule"]["priority"]
        and int(str(item.get("fwmark", "0")), 0) == owner["rule"]["mark"]
        and str(item.get("table")) == str(owner["rule"]["table"])
    ]
    records = saved_state().get("sessions", [])
    assert not [item for item in records if item["profile"] == RECOVERY_PROFILE and item.get("interface")]


def recovery_down(interface, core, helper):
    cli("down", RECOVERY_PROFILE)
    recovery_assert_owned_absent(interface)
    recovery_wait_gone(core)
    recovery_wait_gone(helper)


with subtest("real upstreams and a bounded helper exec observer are isolated"):
    assert not saved_state().get("sessions", [])
    peer.succeed("systemctl start p15-xray.service")
    for address in CANDIDATES:
        peer.wait_for_open_port(1080, addr=address, timeout=30)
    real_helper_path = recovery_install_helper_observer()
    producer_identity = recovery_actor_start()


with subtest("fresh sockets cross real A-to-B recovery without direct sink TCP in the observed window"):
    interface, initial, helper = recovery_save()
    plan = recovery_plan()
    profile_hash = client.succeed("sha256sum /var/lib/oxidom/profiles/" + RECOVERY_PROFILE + ".toml").split()[0]
    capture = recovery_capture("transition")
    before_lan = recovery_lan_control("transition-lan-before")
    first = recovery_actor_begin("transition", capture)
    injected = recovery_signal(initial)
    fault_wait_session(RECOVERY_PROFILE, fault_current_predicate(1), timeout=45)
    stable = recovery_now()
    series = recovery_actor_end(stable, injected)
    replacement = fault_core_identity(RECOVERY_PROFILE)
    assert replacement["starttime"] != initial["starttime"] or replacement["pid"] != initial["pid"]
    assert recovery_plan() == plan and recovery_helper_identity() == helper
    assert client.succeed("sha256sum /var/lib/oxidom/profiles/" + RECOVERY_PROFILE + ".toml").split()[0] == profile_hash
    after_lan = recovery_lan_control("transition-lan-after")
    controls = [
        ("dst host " + CANDIDATES[0] + " and dst port 1080", first["marker"]),
        ("dst host " + CANDIDATES[1] + " and dst port 1080", series["last"]["marker"]),
        ("dst host " + GATEWAY + " and dst port 18080", before_lan),
        ("dst host " + GATEWAY + " and dst port 18080", after_lan),
    ]
    recovery_close_capture(capture, controls)
    recovery_wait_gone(initial)
    recovery_down(interface, replacement, helper)
    print("Routing recovery window passed: fresh sockets span A-to-B, same owned plan/helper, no direct sink TCP in observed captures")


with subtest("one real helper replacement is durable and repeated failure admits no third helper"):
    start_records = len(recovery_observer_records())
    interface, core, first_helper = recovery_save()
    assert len(recovery_observer_records()) == start_records + 1
    initial_record = recovery_observer_records()[-1]
    assert initial_record["pid"] == first_helper["pid"]
    assert initial_record["uid"] == first_helper["uid"] and initial_record["real"] == real_helper_path
    assert initial_record["argv"] == first_helper["argv"][1:]
    plan = recovery_plan()
    capture = recovery_capture("helper")
    before_lan = recovery_lan_control("helper-lan-before")
    first = recovery_actor_begin("helper", capture)
    first_injected = recovery_signal(first_helper)
    changed = (
        "import tomllib; s=next(s for s in tomllib.load(open('/var/lib/oxidom/state.toml','rb'))['sessions'] if s['profile']=="
        + repr(RECOVERY_PROFILE)
        + "); assert s['interface']['tun2socks_pid'] != " + str(first_helper["pid"])
    )
    client.wait_until_succeeds("python3 -c " + quote(changed), timeout=15)
    second_helper = recovery_helper_identity()
    assert second_helper["pid"] != first_helper["pid"] or second_helper["starttime"] != first_helper["starttime"]
    fault_assert_core_unchanged(RECOVERY_PROFILE, core)
    assert recovery_plan() == plan
    assert len(recovery_observer_records()) == start_records + 2
    replacement_record = recovery_observer_records()[-1]
    assert replacement_record["pid"] == second_helper["pid"]
    assert replacement_record["uid"] == second_helper["uid"] and replacement_record["real"] == real_helper_path
    assert replacement_record["argv"] == second_helper["argv"][1:]
    restored = recovery_canary("helper-restored")
    admitted_records = recovery_observer_records()
    second_injected = recovery_signal(second_helper)
    fault_wait_session(RECOVERY_PROFILE, "s['runtime']['failover']['state']=='disarmed' and s['holding_traffic']", timeout=15)
    recovery_wait_gone(core)
    stable = recovery_now()
    series = recovery_actor_end(stable, first_injected, expected="blocked", minimum_after_ms=5000)
    assert second_injected < stable
    assert recovery_observer_records() == admitted_records
    assert recovery_plan() == plan
    assert recovery_session()["runtime"]["failover"]["attempts"] == 0
    after_lan = recovery_lan_control("helper-lan-after")
    recovery_close_capture(capture, [
        ("dst host " + CANDIDATES[0] + " and dst port 1080", first["marker"]),
        ("dst host " + CANDIDATES[0] + " and dst port 1080", restored),
        ("dst host " + GATEWAY + " and dst port 18080", before_lan),
        ("dst host " + GATEWAY + " and dst port 18080", after_lan),
    ])
    forbidden_b = client.succeed(shlex.join([
        "tcpdump", "-nn", "-r", capture["all"][1], "dst host " + CANDIDATES[1] + " and dst port 1080",
    ]))
    assert not forbidden_b.strip()
    recovery_down(interface, core, second_helper)
    recovery_wait_gone(first_helper)
    print("Routing helper budget passed: one durable real replacement forwards, second fault disarms and no third exec occurs in the bounded window")


with subtest("qualified cleanup preserves coordinated foreign protocol-242 resources"):
    interface, core, helper = recovery_save(mode="list")
    owner = interface["ownership"]
    foreign_metric = owner["metric"] ^ 0x40000000
    if foreign_metric == 0:
        foreign_metric = 1
    assert foreign_metric != owner["metric"] and RECOVERY_FOREIGN_TABLE != owner["rule"]["table"]
    foreign_route = shlex.join([
        "ip", "-4", "route", "add", SINK + "/32", "via", GATEWAY, "dev", "eth1",
        "proto", "242", "metric", str(foreign_metric),
    ])
    client.succeed(foreign_route)
    client.succeed("ip -4 route add table " + str(RECOVERY_FOREIGN_TABLE) + " 10.42.0.0/24 dev eth1 proto 242 metric " + str(foreign_metric))
    client.succeed("ip -4 route add table " + str(RECOVERY_FOREIGN_TABLE) + " default via " + GATEWAY + " dev eth1 proto 242 metric " + str(foreign_metric))
    foreign_rule = (
        "ip -4 rule add priority " + str(owner["rule"]["priority"])
        + " fwmark " + hex(RECOVERY_FOREIGN_MARK) + "/0xffffffff table " + str(RECOVERY_FOREIGN_TABLE) + " protocol 242"
    )
    client.succeed(foreign_rule)

    def foreign_snapshot():
        routes = [
            item for item in kernel_json("ip -j -4 route show table all proto 242")
            if item.get("metric") == foreign_metric
        ]
        rules = [
            item for item in kernel_json("ip -j -4 rule show")
            if item.get("priority") == owner["rule"]["priority"]
            and int(str(item.get("fwmark", "0")), 0) == RECOVERY_FOREIGN_MARK
            and str(item.get("table")) == str(RECOVERY_FOREIGN_TABLE)
            and str(item.get("protocol")) == "242"
        ]
        assert len(routes) == 3 and len(rules) == 1
        assert all(item["dev"] == "eth1" for item in routes)
        return json.dumps({"routes": sorted(routes, key=lambda item: json.dumps(item, sort_keys=True)),
                           "rules": sorted(rules, key=lambda item: json.dumps(item, sort_keys=True))}, sort_keys=True)

    foreign_before = foreign_snapshot()
    # No application traffic is generated while the administrative fixture has
    # deliberately inserted a competing route. This tests selector cleanup.
    recovery_down(interface, core, helper)
    assert foreign_snapshot() == foreign_before
    client.succeed(foreign_rule.replace(" rule add ", " rule del ", 1))
    client.succeed(foreign_route.replace(" route add ", " route del ", 1))
    client.succeed("ip -4 route del table " + str(RECOVERY_FOREIGN_TABLE) + " default via " + GATEWAY + " dev eth1 proto 242 metric " + str(foreign_metric))
    client.succeed("ip -4 route del table " + str(RECOVERY_FOREIGN_TABLE) + " 10.42.0.0/24 dev eth1 proto 242 metric " + str(foreign_metric))
    print("Routing foreign ownership passed: exact own cleanup preserves different route/rule selectors sharing protocol 242")


with subtest("dirty identity refuses reuse and explicit cleanup remains retryable"):
    interface, core, helper = recovery_save()
    original_alias = "oxidom-tun-v1:" + interface["ownership"]["cookie"]
    kernel_before = recovery_kernel_plan()
    before_records = len(recovery_observer_records())
    capture = recovery_capture("dirty")
    before_lan = recovery_lan_control("dirty-lan-before")
    first = recovery_actor_begin("dirty", capture)
    injected = recovery_now()
    client.succeed("ip link set dev " + interface["device"] + " alias p15-invented-foreign-alias")
    code, output = client.execute(user_command("oxidom down " + RECOVERY_PROFILE))
    assert code != 0 and "identity" in output
    dirty = recovery_session_state()["interface"]["ownership"]
    assert dirty["cleanup_dirty"]
    expected_owner = copy.deepcopy(interface["ownership"])
    expected_owner["cleanup_dirty"] = True
    assert dirty == expected_owner
    assert recovery_kernel_plan() == kernel_before
    assert recovery_generic_identity(helper["pid"], "tun2socks", helper["parent"])["starttime"] == helper["starttime"]
    recovery_wait_gone(core)
    stable = recovery_now()
    series = recovery_actor_end(stable, injected, expected="blocked", minimum_after_ms=2000)
    assert recovery_kernel_plan() == kernel_before
    code, _output = client.execute(user_command("oxidom up " + RECOVERY_PROFILE))
    assert code != 0
    assert len(recovery_observer_records()) == before_records
    assert recovery_session_state()["interface"]["ownership"] == dirty
    link = kernel_json("ip -j -d link show dev " + interface["device"])[0]
    assert link["ifindex"] == interface["ownership"]["device_index"]
    assert link["ifalias"] == "p15-invented-foreign-alias" and "UP" in link["flags"]
    after_lan = recovery_lan_control("dirty-lan-after")
    recovery_close_capture(capture, [
        ("dst host " + CANDIDATES[0] + " and dst port 1080", first["marker"]),
        ("dst host " + GATEWAY + " and dst port 18080", before_lan),
        ("dst host " + GATEWAY + " and dst port 18080", after_lan),
    ])
    client.succeed("ip link set dev " + interface["device"] + " alias " + quote(original_alias))
    recovery_down(interface, core, helper)
    assert curl("http://" + SINK + ":18080/p15-recovery-dirty-explicit-release")["source"] == LAN_CLIENT
    print("Routing dirty retry passed: failed Down retains qualified intent/kernel plan, reuse refuses, exact-identity cleanup retry succeeds")


with subtest("daemon crash leaves identifiable orphans before explicit non-pool recovery cleanup"):
    interface, core, helper = recovery_save()
    before_info = fault_rpc_json("DaemonInfo")
    daemon = recovery_daemon_identity()
    original_restart = client.succeed("systemctl show -p Restart --value oxidom").strip()
    client.succeed("install -d -m0755 /run/systemd/system/oxidom.service.d")
    dropin = "/run/systemd/system/oxidom.service.d/90-p15-recovery-gate.conf"
    fault_write_text(client, dropin, "[Service]\nRestart=no\n")
    client.succeed("systemctl daemon-reload")
    assert client.succeed("systemctl show -p Restart --value oxidom").strip() == "no"
    assert client.succeed("systemctl show -p KillMode --value oxidom").strip() == "process"
    capture = recovery_capture("crash")
    before_lan = recovery_lan_control("crash-lan-before")
    first = recovery_actor_begin("crash", capture)
    injected = recovery_signal(daemon)
    # Observe only kernel/proc/store state until explicit replacement. A bus
    # request here could activate the packaged service and erase this boundary.
    recovery_assert_alive(core, parent=1)
    recovery_assert_alive(helper, parent=1)
    link = kernel_json("ip -j -d link show dev " + interface["device"])[0]
    assert link["ifindex"] == interface["ownership"]["device_index"]
    assert link["ifalias"] == "oxidom-tun-v1:" + interface["ownership"]["cookie"]
    assert recovery_session_state()["interface"]["ownership"] == interface["ownership"]
    held_alive = recovery_canary("crash-orphan-still-forwards")
    stable = recovery_now()
    series = recovery_actor_end(stable, injected)
    after_lan = recovery_lan_control("crash-lan-after")
    recovery_close_capture(capture, [
        ("dst host " + CANDIDATES[0] + " and dst port 1080", first["marker"]),
        ("dst host " + CANDIDATES[0] + " and dst port 1080", held_alive),
        ("dst host " + CANDIDATES[0] + " and dst port 1080", series["last"]["marker"]),
        ("dst host " + GATEWAY + " and dst port 18080", before_lan),
        ("dst host " + GATEWAY + " and dst port 18080", after_lan),
    ])
    client.succeed("systemctl start oxidom.service")
    client.wait_for_unit("oxidom.service", timeout=60)
    after_info = fault_rpc_json("DaemonInfo")
    assert before_info["store_id"] == after_info["store_id"]
    assert before_info["incarnation"] != after_info["incarnation"]
    recovery_wait_gone(core)
    recovery_wait_gone(helper)
    recovery_assert_owned_absent(interface)
    sessions = json.loads(cli("list", "sessions", "--json"))
    assert not [item for item in sessions if item["state"] in ["connected", "connecting"]]
    assert not [item for item in sessions if item.get("runtime", {}).get("failover") is not None]
    assert fault_rpc_json("GetProfile", "s", RECOVERY_PROFILE)["failover"]["enabled"]
    assert curl("http://" + SINK + ":18080/p15-recovery-crash-deliberate-release")["source"] == LAN_CLIENT
    new_interface, new_core, new_helper = recovery_save()
    assert new_interface["ownership"]["cookie"] != interface["ownership"]["cookie"]
    recovery_canary("crash-explicit-new-activation")
    recovery_down(new_interface, new_core, new_helper)
    client.succeed("rm -- " + dropin)
    client.succeed("systemctl daemon-reload")
    assert client.succeed("systemctl show -p Restart --value oxidom").strip() == original_restart
    print("Routing daemon recovery passed: live identifiable orphans precede explicit cleanup, same store/new incarnation, no grant resumption")


assert recovery_actor_call({"command": "hello"}) == producer_identity
assert recovery_actor_call({"command": "shutdown"}) == {"stopped": True}
client.succeed("systemctl stop p15-recovery-producer.service")
client.fail("systemctl is-active p15-recovery-producer.service")
peer.succeed("systemctl stop p15-xray.service")
print("P15 recovery slice passed: bounded active IPv4 transition, helper budget, qualified cleanup and explicit daemon crash recovery; scoped nft/IPv6 and final release acceptance remain separate")
