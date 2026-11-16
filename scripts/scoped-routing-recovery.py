"""Continues the scoped routing driver with automatic A-to-B recovery evidence."""


with subtest("automatic A-to-B recovery restores a retained owned interface's mark"):
    print("Scoped automatic recovery: retaining one owned interface across a live A-to-B transition")
    cli("down", PROFILE)
    cli("profile", "failover", PROFILE, "--enable", "--json")
    cli("up", PROFILE)
    wait_session(
        "s['state']=='connected' and s['runtime']['failover']['state']=='armed'"
        + " and s['runtime']['failover']['current']==" + repr(ids[0]),
        timeout=90,
    )
    interface = state_session()["interface"]
    owner = interface["ownership"]
    assert owner and interface["rule"] is False
    device = interface["device"]
    mark = interface["mark"]
    link = kernel_json("ip -j -d link show dev " + device)[0]
    assert link["ifindex"] == owner["device_index"] and "UP" in link["flags"]
    nft_snapshot()
    identity = core_identity()

    captures = start_capture("auto")
    controls = capture_controls(captures, "auto-before")
    begun = actor("begin", label="auto")
    window_marker = begun["first_success"]["path"]
    live_marker(captures, window_marker, "dst host " + CANDIDATES[0] + " and dst port 1080")
    injected_ns = kill_core(identity)
    assert begun["first_success"]["end_ns"] <= injected_ns
    wait_session(
        "s['state']=='connected' and s['server_id']==" + repr(ids[1])
        + " and s['runtime']['failover']['current']==" + repr(ids[1])
        + " and s['runtime']['failover']['state']=='armed'",
        timeout=90,
    )

    recovered = state_session()["interface"]
    recovered_owner = recovered["ownership"]
    assert recovered["device"] == device and recovered["mark"] == mark
    assert recovered_owner and recovered_owner["cookie"] == owner["cookie"]
    assert recovered_owner["device_index"] == owner["device_index"]
    assert not recovered_owner.get("cleanup_dirty", False)
    link = kernel_json("ip -j -d link show dev " + device)[0]
    assert link["ifindex"] == owner["device_index"] and "UP" in link["flags"]
    same_actor()
    # The restored chain is sampled after B confirms. The intermediate hold
    # chain is covered by the core-loss request window earlier in this driver,
    # not re-sampled here.
    nft_snapshot()

    replacement = core_identity()
    assert replacement["pid"] != identity["pid"] and replacement["starttime"] != identity["starttime"]
    assert replacement["argv"] == identity["argv"]

    b_marker = "/p15-auto-recovered-candidate-b"
    scoped_success(SINK4, b_marker)
    live_marker(captures, b_marker, "dst host " + CANDIDATES[1] + " and dst port 1080")

    stable_ns = guest_ns()
    window = actor("end", after_ns=stable_ns)
    assert window["first_start_ns"] <= injected_ns < stable_ns < window["last_start_ns"]
    assert window["attempts"] >= 2 and window["successes"] > 0 and window["failures"] > 0
    same_actor()
    controls.extend(capture_controls(captures, "auto-after"))
    finish_capture(captures, controls, [(CANDIDATES[0], window_marker), (CANDIDATES[1], b_marker)])
    cli("profile", "failover", PROFILE, "--disable", "--json")
    print("Scoped automatic recovery passed: retained device and mark, restored mark chain, and fresh forwarding through candidate B in one finite capture")
