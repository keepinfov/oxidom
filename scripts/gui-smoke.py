#!/usr/bin/env python3
"""Isolated X11/GTK lifecycle smoke. This is not a GTK unit or AT test.

The Nix check supplies built binaries and pinned tools. No host buses, display,
settings, store or external destination are used. Actual forwarding is not tested.
"""

import argparse
import ctypes
import json
import os
from pathlib import Path
import select
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import tomllib


LIMIT = 20
PR_SET_CHILD_SUBREAPER = 36
PR_GET_CHILD_SUBREAPER = 37
APP = "dev.keepinfov.oxidom"
BUS = APP + ".Daemon"
OBJECT = "/dev/keepinfov/oxidom/Daemon"
INTERFACE = "dev.keepinfov.oxidom1"


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def receive_exact(stream, size):
    data = bytearray()
    while len(data) < size:
        block = stream.recv(size - len(data))
        if not block:
            raise EOFError("fake inbound closed")
        data.extend(block)
    return data


def answer(stream, socks):
    with stream:
        stream.settimeout(3)
        try:
            if not socks:
                return
            greeting = receive_exact(stream, 2)
            require(greeting[0] == 5, "fake only accepts SOCKS5")
            receive_exact(stream, greeting[1])
            stream.sendall(b"\x05\x00")
            request = receive_exact(stream, 4)
            lengths = {1: 4, 4: 16}
            size = (
                receive_exact(stream, 1)[0] if request[3] == 3 else lengths[request[3]]
            )
            receive_exact(stream, size + 2)
            stream.sendall(b"\x05\x00\x00\x01\x7f\x00\x00\x01\x00\x00")
            headers = bytearray()
            while not headers.endswith(b"\r\n\r\n") and len(headers) < 4096:
                headers.extend(receive_exact(stream, 1))
            stream.sendall(
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
        except (EOFError, OSError, KeyError):
            # Inbound readiness probes intentionally close without a request.
            pass


def serve(listener, socks):
    while True:
        stream, _ = listener.accept()
        threading.Thread(target=answer, args=(stream, socks), daemon=True).start()


def fake_core(arguments):
    if arguments == ["version"]:
        print("Xray 26.3.27 (offline GTK smoke fixture)")
        return
    require(arguments and arguments[0] == "run", "unsupported fake-core command")
    # RuntimeInfo probes asset compatibility with run -test -c. It must not
    # launch a long-lived child or bind the smoke session's inbounds.
    if "-test" in arguments:
        return
    config = json.loads(Path(arguments[arguments.index("-c") + 1]).read_text())
    signal.pthread_sigmask(signal.SIG_SETMASK, [])
    listeners = []
    for inbound in config["inbounds"]:
        address = inbound["listen"]
        require(address.startswith("127."), "fake listener must be loopback")
        listener = socket.socket()
        listener.bind((address, inbound["port"]))
        listener.listen()
        listeners.append(listener)
        threading.Thread(
            target=serve, args=(listener, inbound["protocol"] == "socks"), daemon=True
        ).start()
    while True:
        signal.pause()


# The fake needs only the standard library and deliberately imports no GTK/Gio.
if len(sys.argv) > 1 and sys.argv[1] == "--fake-core":
    fake_core(sys.argv[2:])
    sys.exit(0)

import gi

gi.require_version("Gio", "2.0")
from gi.repository import Gio, GLib
from Xlib import X, XK, display, error, protocol
from Xlib.ext import xtest


def bounded_gio(operation):
    cancellation = Gio.Cancellable.new()
    deadline = threading.Timer(LIMIT, cancellation.cancel)
    deadline.start()
    try:
        return operation(cancellation)
    finally:
        deadline.cancel()
        deadline.join()


class BusObserver:
    """Attribute successful GUI reads without becoming a control connection."""

    READS = frozenset(
        (
            "DaemonInfo",
            "RuntimeInfo",
            "ListSubscriptions",
            "ListProfiles",
            "Status",
            "GetSettings",
            "ProbeState",
            "LogsSince",
        )
    )
    HYDRATION = READS - {"LogsSince"}

    def __init__(self, address):
        self.lock = threading.Lock()
        self.pending = {}
        self.completed = []
        self.sequence = 0
        self.failure = None
        self.pids = {}
        self.filter_id = None
        self.connection = bounded_gio(
            lambda cancellation: Gio.DBusConnection.new_for_address_sync(
                address,
                Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT
                | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION,
                None,
                cancellation,
            )
        )
        self.unique = self.connection.get_unique_name()
        try:
            # Consume monitor copies before GDBus could dispatch another
            # client's method call and send an unsolicited reply. The monitor's
            # own setup reply still has to reach its pending call_sync.
            self.filter_id = self.connection.add_filter(self.message, None)
            self.connection.call_sync(
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus.Monitoring",
                "BecomeMonitor",
                GLib.Variant(
                    "(asu)",
                    (
                        [
                            "type='method_call',path='%s',interface='%s'"
                            % (OBJECT, INTERFACE),
                            "type='method_return'",
                            "type='error'",
                        ],
                        0,
                    ),
                ),
                None,
                Gio.DBusCallFlags.NO_AUTO_START,
                2000,
                None,
            )
        except BaseException:
            try:
                self.close()
            except BaseException as failure:
                print(
                    "additional observer cleanup failure: " + str(failure),
                    file=sys.stderr,
                )
            raise

    def message(self, connection, message, incoming, user_data=None):
        if not incoming:
            return message
        try:
            self.observe(message)
        except BaseException:
            # A Python exception must not silently disappear in a GI callback.
            # Payloads are never included in this diagnostic.
            with self.lock:
                self.failure = "private bus observer rejected a message"
        return message if message.get_destination() == self.unique else None

    def observe(self, message):
        kind = message.get_message_type()
        with self.lock:
            if (
                kind == Gio.DBusMessageType.METHOD_CALL
                and message.get_path() == OBJECT
                and message.get_interface() == INTERFACE
                and message.get_member() in self.READS
            ):
                require(len(self.pending) < 256, "private bus pending bound exceeded")
                self.pending[(message.get_sender(), message.get_serial())] = {
                    "sender": message.get_sender(),
                    "destination": message.get_destination(),
                    "member": message.get_member(),
                }
            elif kind in (
                Gio.DBusMessageType.METHOD_RETURN,
                Gio.DBusMessageType.ERROR,
            ):
                request = self.pending.pop(
                    (message.get_destination(), message.get_reply_serial()), None
                )
                if request is None:
                    return
                require(
                    len(self.completed) < 1024, "private bus history bound exceeded"
                )
                self.sequence += 1
                request.update(
                    sequence=self.sequence,
                    success=kind == Gio.DBusMessageType.METHOD_RETURN,
                    reply_sender=message.get_sender(),
                    sessions=(),
                )
                if request["success"] and request["member"] == "Status":
                    values = message.get_body().unpack()
                    require(len(values) == 1, "invalid status response shape")
                    status = json.loads(values[0])
                    request["sessions"] = tuple(
                        (
                            session.get("profile"),
                            session.get("state"),
                            session.get("server_id"),
                        )
                        for session in status["sessions"]
                    )
                self.completed.append(request)

    def cursor(self):
        with self.lock:
            require(self.failure is None, self.failure)
            return self.sequence

    def reads(self, smoke, pid, owner, after=0):
        with self.lock:
            require(self.failure is None, self.failure)
            records = [
                record.copy()
                for record in self.completed
                if record["sequence"] > after
                and record["destination"] == owner
                and record["reply_sender"] == owner
                and record["success"]
            ]
        # APP's GApplication owner is a different connection from zbus. Resolve
        # daemon-client senders outside the monitor callback on the separate
        # control connection; each recovery creates a new zbus sender.
        for sender in {record["sender"] for record in records}:
            if sender not in self.pids:
                try:
                    self.pids[sender] = smoke.call(
                        "org.freedesktop.DBus",
                        "/org/freedesktop/DBus",
                        "org.freedesktop.DBus",
                        "GetConnectionUnixProcessID",
                        "(s)",
                        (sender,),
                    )[0]
                except GLib.Error as failure:
                    if "NameHasNoOwner" not in str(failure):
                        raise
                    self.pids[sender] = None
        return [record for record in records if self.pids[record["sender"]] == pid]

    def polling_sender(self, smoke, pid, owner, after=0):
        records = self.reads(smoke, pid, owner, after)
        for sender in {record["sender"] for record in records}:
            first = {}
            for record in records:
                if record["sender"] == sender:
                    first.setdefault(record["member"], record["sequence"])
            if not self.HYDRATION.issubset(first):
                continue
            hydrated = max(first[member] for member in self.HYDRATION)
            if any(
                record["sender"] == sender
                and record["member"] == "LogsSince"
                and record["sequence"] > hydrated
                for record in records
            ):
                # load_recovered never calls LogsSince. Normal polling only
                # runs after the controller installs its hydrated candidate
                # and reopens admission, so read attempts alone cannot pass.
                return sender
        return None

    def saw_session(self, smoke, pid, owner, sender, server, after):
        return any(
            record["sender"] == sender
            and record["member"] == "Status"
            and ("default", "connected", server) in record["sessions"]
            for record in self.reads(smoke, pid, owner, after)
        )

    def close(self):
        try:
            if self.filter_id is not None:
                self.connection.remove_filter(self.filter_id)
                self.filter_id = None
        finally:
            bounded_gio(self.connection.close_sync)


class Process:
    """Keep the unreaped leader until private group cleanup prevents PID reuse."""

    def __init__(self, command, environment, root, name, pass_fds=(), stdout=None):
        self.output = root / (name + ".stdout")
        self.error = root / (name + ".stderr")
        self.reaped = False
        with self.output.open("wb") as out, self.error.open("wb") as err:
            self.child = subprocess.Popen(
                command,
                env=environment,
                cwd=root,
                stdin=subprocess.DEVNULL,
                stdout=out if stdout is None else stdout,
                stderr=err,
                start_new_session=True,
                pass_fds=pass_fds,
            )
        try:
            self.fd = os.pidfd_open(self.child.pid)
        except BaseException:
            try:
                self.kill_group()
            finally:
                self.child.wait()
                self.reap_group()
            raise

    def exited(self):
        return bool(select.select([self.fd], [], [], 0)[0])

    def logs(self):
        records = []
        for path in (self.output, self.error):
            with path.open(errors="replace") as stream:
                records.append(path.name + ":\n" + stream.read(65536))
        return "\n".join(records)

    def appended_notice(self, text):
        """Observe a new stderr marker without depending on an old log prefix."""
        needle = text.encode()
        require(bool(needle), "an empty log notice cannot witness an event")
        cursor = self.error.stat().st_size
        suffix = b""

        def arrived():
            nonlocal cursor, suffix
            with self.error.open("rb") as stream:
                stream.seek(cursor)
                data = stream.read(65536)
            cursor += len(data)
            combined = suffix + data
            keep = len(needle) - 1
            suffix = combined[-keep:] if keep else b""
            return needle in combined

        return arrived

    def signal(self, value):
        signal.pidfd_send_signal(self.fd, value)

    def wait_exit(self):
        require(
            select.select([self.fd], [], [], LIMIT)[0], "child timeout: " + self.logs()
        )

    def reap_group(self):
        deadline = time.monotonic() + LIMIT
        while True:
            try:
                child, _ = os.waitpid(-self.child.pid, os.WNOHANG)
                if child:
                    continue
                require(
                    time.monotonic() < deadline, "private process group did not exit"
                )
                os.sched_yield()
            except ChildProcessError:
                return
            except InterruptedError:
                continue

    def finish(self, expected=0):
        self.wait_exit()
        self.kill_group()
        code = self.child.wait()
        self.reaped = True
        self.reap_group()
        require(code == expected, "unexpected child exit %s: %s" % (code, self.logs()))
        return self.output.read_text()

    def kill_group(self):
        try:
            os.killpg(self.child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass

    def close(self):
        try:
            if not self.reaped:
                self.kill_group()
                self.child.wait(timeout=LIMIT)
                self.reaped = True
                self.reap_group()
        finally:
            os.close(self.fd)


class Smoke:
    def __init__(self, args):
        self.args = args
        self.children = []
        self.x = None
        self.connection = None
        self.observer = None
        self.libc = ctypes.CDLL(None, use_errno=True)
        self.previous_subreaper = ctypes.c_int()
        self.subreaper_installed = False
        # Short private paths also fit sockaddr_un in long Nix build roots.
        self.root = Path(tempfile.mkdtemp(prefix="oxidom-gui-smoke-", dir="/tmp"))
        self.env = {
            "PATH": args.tool_path,
            "HOME": str(self.root / "home"),
            "XDG_CONFIG_HOME": str(self.root / "config"),
            "XDG_DATA_HOME": str(self.root / "data"),
            "XDG_STATE_HOME": str(self.root / "state"),
            "XDG_CACHE_HOME": str(self.root / "cache"),
            "XDG_RUNTIME_DIR": str(self.root / "runtime"),
            "DBUS_SYSTEM_BUS_ADDRESS": "unix:path=" + str(self.root / "no-system-bus"),
            "OXIDOM_SESSION_DAEMON": "external",
            "OXIDOM_BIN": args.cli,
            "GSETTINGS_BACKEND": "memory",
            "GDK_BACKEND": "x11",
            "GSK_RENDERER": "cairo",
            "GTK_A11Y": "none",
            "LANG": "C.UTF-8",
        }

    def spawn(self, arguments, name, **kwargs):
        child = Process(arguments, self.env, self.root, name, **kwargs)
        self.children.append(child)
        return child

    def wait(self, predicate, label):
        deadline = time.monotonic() + LIMIT
        while True:
            while GLib.MainContext.default().pending():
                GLib.MainContext.default().iteration(False)
            if predicate():
                return
            require(time.monotonic() < deadline, "deadline waiting for " + label)
            # Only an observed state transition advances a scenario; there is
            # no elapsed-time assumption about startup or rendering readiness.
            os.sched_yield()

    def read_line(self, fd, label):
        deadline = time.monotonic() + LIMIT
        result = bytearray()
        while not result.endswith(b"\n"):
            require(
                select.select([fd], [], [], max(0, deadline - time.monotonic()))[0],
                label + " did not announce readiness",
            )
            byte = os.read(fd, 1)
            require(byte, label + " closed its readiness pipe")
            result.extend(byte)
            require(len(result) < 4096, label + " readiness record too large")
        return result.decode().strip()

    def setup(self):
        require(
            self.libc.prctl(
                PR_GET_CHILD_SUBREAPER, ctypes.byref(self.previous_subreaper), 0, 0, 0
            )
            == 0,
            "reading subreaper",
        )
        require(
            self.libc.prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) == 0,
            "setting subreaper",
        )
        self.subreaper_installed = True
        for directory in ("home", "config", "data", "state", "cache", "runtime"):
            (self.root / directory).mkdir(mode=0o700)
        bus_config = self.root / "bus.conf"
        bus_config.write_text(
            '<busconfig><type>session</type><listen>unix:path=%s</listen><auth>EXTERNAL</auth><policy context="default"><allow send_destination="*"/><allow receive_sender="*"/><allow own="*"/><allow user="*"/><allow eavesdrop="true"/></policy></busconfig>'
            % (self.root / "bus")
        )
        read, write = os.pipe()
        try:
            self.spawn(
                [
                    self.args.dbus,
                    "--config-file=" + str(bus_config),
                    "--nofork",
                    "--print-address=1",
                ],
                "bus",
                stdout=write,
            )
            os.close(write)
            write = None
            self.env["DBUS_SESSION_BUS_ADDRESS"] = self.read_line(read, "private bus")
        finally:
            os.close(read)
            if write is not None:
                os.close(write)
        self.connection = bounded_gio(
            lambda cancellation: Gio.DBusConnection.new_for_address_sync(
                self.env["DBUS_SESSION_BUS_ADDRESS"],
                Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT
                | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION,
                None,
                cancellation,
            )
        )
        # Local MIT cookie, with an empty display number so both Xvfb and
        # python-xlib can use it before -displayfd chooses the free display.
        authority = self.root / "Xauthority"
        cookie = os.urandom(16)
        fields = (socket.gethostname().encode(), b"", b"MIT-MAGIC-COOKIE-1", cookie)
        record = struct.pack(">H", 256) + b"".join(
            struct.pack(">H", len(field)) + field for field in fields
        )
        self.private_write(authority, record)
        self.env["XAUTHORITY"] = str(authority)
        read, write = os.pipe()
        try:
            self.spawn(
                [
                    self.args.xvfb,
                    "-displayfd",
                    str(write),
                    "-screen",
                    "0",
                    "1400x1000x24",
                    "-nolisten",
                    "tcp",
                    "-auth",
                    str(authority),
                ],
                "display",
                pass_fds=(write,),
            )
            os.close(write)
            write = None
            # Explicit transport/display syntax works in libxcb and python-xlib;
            # libxcb interprets the different "unix:" prefix as a socket path.
            self.env["DISPLAY"] = "unix/:" + self.read_line(read, "private Xvfb")
        finally:
            os.close(read)
            if write is not None:
                os.close(write)
        previous_authority = os.environ.get("XAUTHORITY")
        previous_timeout = socket.getdefaulttimeout()
        try:
            os.environ["XAUTHORITY"] = str(authority)
            socket.setdefaulttimeout(LIMIT)
            self.x = display.Display(self.env["DISPLAY"])
        finally:
            socket.setdefaulttimeout(previous_timeout)
            if previous_authority is None:
                os.environ.pop("XAUTHORITY", None)
            else:
                os.environ["XAUTHORITY"] = previous_authority
        self.x.screen().root.change_attributes(event_mask=X.SubstructureNotifyMask)
        self.x.sync()
        self.seed()

    def seed(self):
        directory = self.root / "config/oxidom"
        directory.mkdir(mode=0o700)
        (directory / "profiles").mkdir(mode=0o700)
        fake = self.root / "fake-xray"
        fake_arguments = [sys.executable, str(Path(__file__).resolve()), "--fake-core"]
        fake.write_text(
            "#!"
            + sys.executable
            + "\nimport os, sys\nos.execv("
            + repr(sys.executable)
            + ", "
            + repr(fake_arguments)
            + " + sys.argv[1:])\n"
        )
        fake.chmod(0o700)
        reservations = [socket.socket(), socket.socket()]
        try:
            for listener in reservations:
                listener.bind(("127.0.0.1", 0))
            ports = [listener.getsockname()[1] for listener in reservations]
            config = "\n".join(
                [
                    "socks_port = " + str(ports[0]),
                    "http_port = " + str(ports[1]),
                    "xray_binary = " + json.dumps(str(fake)),
                    'latency_test_url = "http://offline.example.invalid/confirm"',
                    'watchdog = "off"',
                    "system_proxy = false",
                    "standby_proxy = false",
                    "reconnect = false",
                    "",
                ]
            )
            self.private_write(directory / "config.toml", config)
            self.private_write(
                directory / "profiles/default.toml",
                "[proxy]\nsocks_port = %s\nhttp_port = %s\n" % tuple(ports),
            )
        finally:
            for listener in reservations:
                listener.close()

    @staticmethod
    def private_write(path, contents):
        fd = os.open(path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
        with os.fdopen(fd, "wb") as stream:
            stream.write(contents.encode() if isinstance(contents, str) else contents)

    def call(self, destination, path, interface, method, signature="()", arguments=()):
        value = self.connection.call_sync(
            destination,
            path,
            interface,
            method,
            GLib.Variant(signature, arguments),
            None,
            Gio.DBusCallFlags.NO_AUTO_START,
            2000,
            None,
        )
        return value.unpack()

    def name_owner(self, name):
        try:
            return self.call(
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "GetNameOwner",
                "(s)",
                (name,),
            )[0]
        except GLib.Error as failure:
            if "NameHasNoOwner" in str(failure):
                return None
            raise

    def json(self, method, signature="()", arguments=()):
        return json.loads(
            self.call(self.owner, OBJECT, INTERFACE, method, signature, arguments)[0]
        )

    def windows(self, pid):
        found = []
        root = self.x.screen().root
        while self.x.pending_events():
            self.x.next_event()
        for window in root.query_tree().children:
            try:
                property = window.get_full_property(
                    self.x.intern_atom("_NET_WM_PID"), X.AnyPropertyType
                )
                if (
                    property is not None
                    and list(property.value) == [pid]
                    and window.get_attributes().map_state == X.IsViewable
                ):
                    found.append(window)
            except error.BadWindow:
                pass
        return found

    def main_window(self, gui):
        require(not gui.exited(), "GUI exited during startup: " + gui.logs())
        candidates = []
        for window in self.windows(gui.child.pid):
            try:
                geometry = window.get_geometry()
                exported = window.get_full_property(
                    self.x.intern_atom("_GTK_WINDOW_OBJECT_PATH"), X.AnyPropertyType
                )
                # Source-pinned distinction: splash is fixed340x220; the main
                # ApplicationWindow defaults to1100x720 after startup reads finish.
                if (
                    exported is not None
                    and geometry.width >= 1000
                    and geometry.height >= 650
                ):
                    candidates.append(window)
            except (error.BadWindow, error.BadDrawable):
                # query_tree/windows is a snapshot: GTK can destroy the splash
                # before GetGeometry, which reports BadDrawable rather than
                # BadWindow. Retry the bounded main-window observation only.
                continue
        return (
            candidates[0]
            if len(candidates) == 1 and len(self.windows(gui.child.pid)) == 1
            else None
        )

    def shortcut(self, window, symbol):
        window.set_input_focus(X.RevertToParent, X.CurrentTime)
        control = self.x.keysym_to_keycode(XK.string_to_keysym("Control_L"))
        key = self.x.keysym_to_keycode(XK.string_to_keysym(symbol))
        require(control and key, "Xvfb keyboard lacks shortcut keys")
        xtest.fake_input(self.x, X.KeyPress, control)
        xtest.fake_input(self.x, X.KeyPress, key)
        xtest.fake_input(self.x, X.KeyRelease, key)
        xtest.fake_input(self.x, X.KeyRelease, control)
        self.x.sync()

    def cli(self, *arguments):
        return self.spawn(
            [self.args.cli, *arguments], "cli-" + str(len(self.children))
        ).finish()

    def start_daemon(self, name, previous=None):
        daemon = self.spawn([self.args.cli, "daemon"], name)
        observed = [None]

        def ready():
            observed[0] = self.name_owner(BUS)
            return daemon.exited() or (
                observed[0] is not None and observed[0] != previous
            )

        self.wait(ready, "owned daemon name")
        require(not daemon.exited(), "daemon failed: " + daemon.logs())
        owner = observed[0]
        pid = self.call(
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "GetConnectionUnixProcessID",
            "(s)",
            (owner,),
        )[0]
        require(pid == daemon.child.pid, "daemon name is owned by another process")
        self.owner = owner
        return daemon, self.json("DaemonInfo")

    def run(self):
        self.setup()
        daemon, identity = self.start_daemon("daemon")
        imported = self.call(
            self.owner,
            OBJECT,
            INTERFACE,
            "ImportLinks",
            "(s)",
            ("trojan://invented-smoke@smoke.example.invalid:443#Smoke",),
        )
        require(imported[0] == 1, "invented server was not imported")
        servers = json.loads(self.cli("list", "servers", "--json"))
        profile = self.json("GetProfile", "(s)", ("default",))
        profile["select"] = {"server": servers[0]["id"]}
        self.call(
            self.owner,
            OBJECT,
            INTERFACE,
            "SaveProfile",
            "(ss)",
            ("default", json.dumps(profile)),
        )
        self.cli("up")
        self.wait(
            lambda: any(
                session["state"] == "connected" for session in self.json("ListSessions")
            ),
            "confirmed fake session",
        )
        state = tomllib.loads((self.root / "data/oxidom/state.toml").read_text())
        core_pid = state["sessions"][0]["xray_pid"]
        core = os.pidfd_open(core_pid)
        try:
            self.observer = BusObserver(self.env["DBUS_SESSION_BUS_ADDRESS"])
            gui = self.spawn([self.args.gui, "--debug"], "gui")
            mapped = [None]

            def ready():
                mapped[0] = self.main_window(gui)
                return mapped[0] is not None

            self.wait(ready, "main window after startup hydration attempt")
            main = mapped[0]
            application_owner = self.name_owner(APP)
            require(application_owner is not None, "GUI has no application owner")
            application_pid = self.call(
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "GetConnectionUnixProcessID",
                "(s)",
                (application_owner,),
            )[0]
            require(
                application_pid == gui.child.pid,
                "application owner differs from owned GUI",
            )
            initial_sender = [None]

            def initial_polling():
                initial_sender[0] = self.observer.polling_sender(
                    self, gui.child.pid, self.owner
                )
                return initial_sender[0] is not None

            self.wait(initial_polling, "GUI startup hydration and admitted polling")
            self.wait(
                lambda: self.observer.saw_session(
                    self,
                    gui.child.pid,
                    self.owner,
                    initial_sender[0],
                    servers[0]["id"],
                    0,
                ),
                "GUI observed the initial confirmed session",
            )
            require(
                not select.select([core], [], [], 0)[0],
                "initial fake core exited before the daemon-loss fault",
            )

            # The GUI must observe loss on its own timer, then install a fresh
            # immutable client only after strict same-store rehydration. Keep
            # the process/window alive through this actual owner replacement.
            fault_cursor = self.observer.cursor()
            old_owner = self.owner
            loss_notice = gui.appended_notice(
                "daemon polling failed; tunnel state is unknown"
            )
            daemon.signal(signal.SIGTERM)
            daemon.wait_exit()
            require(
                select.select([core], [], [], LIMIT)[0],
                "daemon signal shutdown left its old core alive",
            )
            # The core-exit assertion precedes fallback group cleanup.
            daemon.finish()
            self.wait(
                loss_notice,
                "GUI accepted daemon-loss state",
            )
            require(not gui.exited(), "daemon loss terminated the GUI")
            current_window = self.main_window(gui)
            require(
                current_window is not None and current_window.id == main.id,
                "daemon loss replaced or hid the main window",
            )

            daemon, replacement = self.start_daemon("replacement-daemon", old_owner)
            require(
                replacement["store_id"] == identity["store_id"]
                and replacement["incarnation"] != identity["incarnation"],
                "replacement did not preserve store continuity and change incarnation",
            )
            require(
                self.json("ListSessions") == [],
                "daemon restart resumed a session without an explicit Up",
            )
            recovered_sender = [None]

            def recovered_polling():
                recovered_sender[0] = self.observer.polling_sender(
                    self, gui.child.pid, self.owner, fault_cursor
                )
                return recovered_sender[0] is not None

            self.wait(
                recovered_polling,
                "GUI strict replacement hydration and committed polling",
            )
            require(
                recovered_sender[0] != initial_sender[0],
                "GUI reused its retired daemon-client connection",
            )
            current_window = self.main_window(gui)
            require(
                not gui.exited()
                and self.name_owner(APP) == application_owner
                and current_window is not None
                and current_window.id == main.id,
                "owner recovery replaced the GUI process, application or main window",
            )

            renewed_cursor = self.observer.cursor()
            self.cli("up")
            self.wait(
                lambda: any(
                    session["state"] == "connected"
                    for session in self.json("ListSessions")
                ),
                "renewed confirmed fake session",
            )
            state = tomllib.loads((self.root / "data/oxidom/state.toml").read_text())
            previous_core = core
            core = os.pidfd_open(state["sessions"][0]["xray_pid"])
            os.close(previous_core)
            self.wait(
                lambda: self.observer.saw_session(
                    self,
                    gui.child.pid,
                    self.owner,
                    recovered_sender[0],
                    servers[0]["id"],
                    renewed_cursor,
                ),
                "recovered GUI observed the renewed confirmed session",
            )
            identity = replacement

            # Dispatch existing navigation shortcuts through real X11 events.
            # This checks event-loop wiring/liveness, not displayed text or AT.
            for page in ("1", "2", "3", "4", "5", "1"):
                self.shortcut(main, page)
            close = protocol.event.ClientMessage(
                window=main,
                client_type=self.x.intern_atom("WM_PROTOCOLS"),
                data=(
                    32,
                    [self.x.intern_atom("WM_DELETE_WINDOW"), X.CurrentTime, 0, 0, 0],
                ),
            )
            main.send_event(close, event_mask=X.NoEventMask)
            self.x.sync()
            self.wait(lambda: not self.windows(gui.child.pid), "main window hidden")
            require(not gui.exited(), "closing the window must retain the GUI host")
            self.spawn([self.args.gui, "--debug"], "second-gui").finish()
            self.wait(
                lambda: any(
                    window.id == main.id for window in self.windows(gui.child.pid)
                ),
                "same window reactivated",
            )
            require(
                not gui.exited(), "second invocation replaced the original GUI process"
            )
            require(
                len(self.windows(gui.child.pid)) == 1,
                "reactivation created another visible window",
            )
            require(
                self.name_owner(APP) == application_owner,
                "second invocation replaced the GUI application owner",
            )
            self.shortcut(main, "q")
            gui.finish()
            require(
                not select.select([core], [], [], 0)[0],
                "GUI quit stopped the daemon-owned fake core",
            )
            require(
                self.name_owner(BUS) == self.owner
                and self.json("DaemonInfo") == identity,
                "GUI lifecycle replaced the daemon identity",
            )
            sessions = self.json("ListSessions")
            require(
                len(sessions) == 1 and sessions[0]["state"] == "connected",
                "GUI lifecycle disturbed the active fake session",
            )
            self.cli("down")
            require(
                select.select([core], [], [], LIMIT)[0],
                "explicit Stop left the fake core alive",
            )
        finally:
            os.close(core)
        daemon.signal(signal.SIGTERM)
        daemon.finish()
        print(
            "GUI smoke passed: startup main, same-store owner recovery, navigation dispatch, hide/reactivate, quit preserves daemon and session"
        )

    def close(self):
        failures = []

        def cleanup(action):
            try:
                action()
            except BaseException as failure:
                failures.append(str(failure))

        if self.observer is not None:
            cleanup(self.observer.close)
        if self.x is not None:
            cleanup(self.x.close)
        if self.connection is not None:
            cleanup(lambda: bounded_gio(self.connection.close_sync))
        for child in reversed(self.children):
            cleanup(child.close)
        if self.subreaper_installed:
            cleanup(
                lambda: require(
                    self.libc.prctl(
                        PR_SET_CHILD_SUBREAPER, self.previous_subreaper.value, 0, 0, 0
                    )
                    == 0,
                    "restoring subreaper",
                )
            )
        if failures:
            for child in self.children:
                try:
                    print(child.logs(), file=sys.stderr)
                except OSError:
                    pass
            raise RuntimeError(
                "cleanup failures (private root retained at %s): %s"
                % (self.root, "; ".join(failures))
            )
        shutil.rmtree(self.root)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for argument in ("cli", "gui", "dbus", "xvfb", "tool-path"):
        parser.add_argument("--" + argument, required=True)
    smoke = Smoke(parser.parse_args())
    failure = None
    try:
        smoke.run()
    except BaseException as caught:
        failure = caught
        for child in smoke.children:
            print(child.logs(), file=sys.stderr)
        raise
    finally:
        try:
            smoke.close()
        except BaseException as cleanup_failure:
            if failure is None:
                raise
            print(
                "additional cleanup failure: " + str(cleanup_failure), file=sys.stderr
            )


if __name__ == "__main__":
    main()
