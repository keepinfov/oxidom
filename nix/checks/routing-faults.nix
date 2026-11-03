{
  self,
  pkgs,
  lib,
  oxidom-cli,
}: let
  # Documentation addresses must not accidentally match a distributor's
  # geoip:private dataset and bypass the candidate. These literal, deliberately
  # tiny protobuf fixtures leave the generated rule intact and classify only
  # the test LAN as private. This check does not verify production geo data.
  geoAssets =
    pkgs.runCommand "oxidom-routing-geo-fixture" {
      nativeBuildInputs = [pkgs.python3];
    } ''
      mkdir -p "$out"
      python3 - "$out" <<'PY'
      import pathlib
      import sys

      def field(number, value):
          assert len(value) < 128
          return bytes([number << 3 | 2, len(value)]) + value

      root = pathlib.Path(sys.argv[1])
      cidr = field(1, bytes([10, 0, 0, 0])) + bytes([0x10, 8])
      geoip = field(1, b"PRIVATE") + field(2, cidr)
      domain = bytes([0x08, 3]) + field(2, b"p15-lan.invalid")
      geosite = field(1, b"PRIVATE") + field(2, domain)
      (root / "geoip.dat").write_bytes(field(1, geoip))
      (root / "geosite.dat").write_bytes(field(1, geosite))
      PY
    '';
  fixtureServer = pkgs.writeText "oxidom-routing-http.py" ''
    import http.server
    import json

    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def do_GET(self):
            body = json.dumps({"path": self.path, "source": self.client_address[0]}).encode()
            print(body.decode(), flush=True)
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(body)
            self.close_connection = True

        def log_message(self, *_args):
            pass

    http.server.ThreadingHTTPServer(("0.0.0.0", 18080), Handler).serve_forever()
  '';
  peerConfig = pkgs.writeText "oxidom-routing-peer.json" (builtins.toJSON {
    log = {
      loglevel = "info";
      access = "/var/lib/p15-xray/access.log";
    };
    inbounds = map (address: {
      listen = address;
      port = 1080;
      protocol = "socks";
      tag = "candidate-${address}";
      settings = {
        auth = "noauth";
        udp = false;
      };
    }) ["192.0.2.10" "192.0.2.11"];
    outbounds = [
      {
        protocol = "freedom";
        tag = "sink";
      }
    ];
  });
  daemonConfig = pkgs.writeText "oxidom-routing-config.toml" ''
    system_proxy = false
    standby_proxy = false
    reconnect = false
    watchdog = "off"
    on_core_exit = "hold"
    latency_method = "http_get"
    latency_test_url = "http://203.0.113.10:18080/confirmation"
    xray_binary = "${pkgs.xray}/bin/xray"
    tun2socks_binary = "${pkgs.tun2socks}/bin/tun2socks"
    nft_binary = "${pkgs.nftables}/bin/nft"

    [core]
    log_level = "info"
  '';
  common = {
    networking.useDHCP = lib.mkForce false;
    networking.enableIPv6 = false;
    networking.firewall.enable = false;
    # The driver communicates over a serial backdoor, not this data network.
    # No DHCP address/default on QEMU's management NIC may reach its user NAT.
    networking.interfaces.eth0.useDHCP = lib.mkForce false;
    virtualisation.restrictNetwork = true;
    virtualisation.vlans = [];
    virtualisation.interfaces.eth1 = {
      vlan = 1;
      assignIP = false;
    };
    virtualisation.memorySize = 1024;
    environment.systemPackages = with pkgs; [curl iproute2 python3 tcpdump xray];
    boot.kernel.sysctl."net.ipv4.conf.all.rp_filter" = 0;
    boot.kernel.sysctl."net.ipv4.conf.default.rp_filter" = 0;
    system.stateVersion = "26.05";
  };
in
  assert lib.assertMsg (pkgs.xray.version == "26.3.27")
  "The routing VM must use oxidom's exact supported real Xray release";
    pkgs.testers.runNixOSTest {
      name = "oxidom-routing-faults";
      nodes = {
        client = {
          imports = [common self.nixosModules.default];
          environment.systemPackages = [oxidom-cli];
          networking.interfaces.eth1.ipv4.addresses = [
            {
              address = "10.42.0.2";
              prefixLength = 24;
            }
          ];
          networking.defaultGateway = {
            address = "10.42.0.1";
            interface = "eth1";
          };
          users.users.tester = {
            isNormalUser = true;
            uid = 1000;
          };
          services.oxidom = {
            enable = true;
            package = oxidom-cli;
            tun.enable = true;
            users = ["tester"];
          };
          systemd.services.oxidom = {
            environment.XRAY_LOCATION_ASSET = toString geoAssets;
            # Only the disposable daemon's private StateDirectory is seeded.
            # Every profile/catalog operation thereafter uses its public bus.
            preStart = ''
              if [ ! -e /var/lib/oxidom/config.toml ]; then
                install -m600 ${daemonConfig} /var/lib/oxidom/config.toml
              fi
            '';
          };
        };
        peer = {
          imports = [common];
          networking.defaultGateway = lib.mkForce null;
          networking.interfaces.eth1.ipv4.addresses = [
            {
              address = "10.42.0.1";
              prefixLength = 24;
            }
          ];
          networking.interfaces.lo.ipv4.addresses = map (address: {
            inherit address;
            prefixLength = 32;
          }) ["192.0.2.10" "192.0.2.11" "203.0.113.10"];
          # The peer terminates fixture connections, never forwards externally.
          boot.kernel.sysctl."net.ipv4.ip_forward" = 0;
          systemd.services.p15-http = {
            wantedBy = ["multi-user.target"];
            after = ["network.target"];
            serviceConfig = {
              ExecStart = "${pkgs.python3}/bin/python3 -u ${fixtureServer}";
              DynamicUser = true;
              NoNewPrivileges = true;
              CapabilityBoundingSet = "";
              AmbientCapabilities = "";
            };
          };
          systemd.services.p15-xray = {
            wantedBy = ["multi-user.target"];
            after = ["network.target"];
            environment.XRAY_LOCATION_ASSET = toString geoAssets;
            serviceConfig = {
              ExecStart = "${pkgs.xray}/bin/xray run -config ${peerConfig}";
              DynamicUser = true;
              StateDirectory = "p15-xray";
              NoNewPrivileges = true;
              CapabilityBoundingSet = "";
              AmbientCapabilities = "";
            };
          };
        };
      };
      testScript = builtins.readFile ../../scripts/routing-vm.py;
    }
