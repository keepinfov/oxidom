"""Prunes the ended scope after the scoped routing driver's final scenario."""


with subtest("the next teardown prunes a scope that has actually ended"):
    assert actor("quit") == {"quitting": True}
    client.wait_until_fails("test -e /proc/" + str(pinned_actor["pid"]), timeout=15)
    user_systemctl_stop_if_loaded("p15-scoped-launcher.service")
    user_systemctl_stop_if_loaded("oxidom\\x2dscoped.slice")
    client.wait_until_succeeds(
        user_command(shlex.join([
            "systemctl", "--user", "show", "-p", "LoadState", "--value",
            "p15-scoped-launcher.service",
        ])) + " | grep -qx not-found",
        timeout=15,
    )
    client.wait_until_fails("test -d " + quote("/sys/fs/cgroup/" + pinned_record["path"]), timeout=15)
    cli("down", PROFILE)
    assert not any(item["profile"] == PROFILE for item in saved_state().get("run_cgroups", []))
    table = kernel_json("nft -j list table inet oxidom")["nftables"]
    assert not [item for item in table if "chain" in item and item["chain"]["name"] in {"profile_scoped", "restore_scoped"}]
    assert not [item for item in table if "rule" in item and item["rule"]["chain"] in {"profile_scoped", "restore_scoped"}]
    client.fail("ip link show dev oxi-scoped")


print("Scoped routing VM passed: real user scope, mark/restore, scoped IPv6 refusal, core/Down request windows, same-scope Up, automatic A-to-B on the retained interface, and pruning")
