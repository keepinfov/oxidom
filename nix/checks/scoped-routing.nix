{
  self,
  pkgs,
  lib,
  oxidom-cli,
}: let
  # Keep the generated private-address rule while making the documentation
  # destinations non-private in this explicitly declared packet fixture.
  geoAssets =
    pkgs.runCommand "oxidom-scoped-geo-fixture" {
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
  httpServer = pkgs.writeText "oxidom-scoped-http.py" ''
    import http.server
    import json
    import socket
    import threading

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

    class V6Server(http.server.ThreadingHTTPServer):
        address_family = socket.AF_INET6

        def server_bind(self):
            self.socket.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 1)
            super().server_bind()

    ipv4 = http.server.ThreadingHTTPServer(("0.0.0.0", 18080), Handler)
    ipv6 = V6Server(("::", 18080), Handler)
    threading.Thread(target=ipv4.serve_forever, daemon=True).start()
    ipv6.serve_forever()
  '';
  peerConfig = pkgs.writeText "oxidom-scoped-peer.json" (builtins.toJSON {
    log = {
      loglevel = "info";
      access = "/var/lib/p15-scoped-xray/access.log";
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
  daemonConfig = pkgs.writeText "oxidom-scoped-config.toml" ''
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
    networking.enableIPv6 = true;
    networking.firewall.enable = false;
    networking.interfaces.eth0.useDHCP = lib.mkForce false;
    boot.kernel.sysctl = {
      "net.ipv6.conf.all.accept_ra" = 0;
      "net.ipv6.conf.default.accept_ra" = 0;
      "net.ipv6.conf.all.autoconf" = 0;
      "net.ipv6.conf.default.autoconf" = 0;
      "net.ipv6.conf.eth0.disable_ipv6" = 1;
      "net.ipv4.conf.all.rp_filter" = 1;
      "net.ipv4.conf.default.rp_filter" = 1;
    };
    virtualisation.restrictNetwork = true;
    virtualisation.vlans = [];
    virtualisation.interfaces.eth1 = {
      vlan = 1;
      assignIP = false;
    };
    virtualisation.memorySize = 1024;
    environment.systemPackages = with pkgs; [
      curl
      iproute2
      nftables
      python3
      tcpdump
      xray
    ];
    system.stateVersion = "26.05";
  };
in
  assert lib.assertMsg (pkgs.xray.version == "26.3.27")
  "The scoped routing VM must use oxidom's exact supported real Xray release";
    pkgs.testers.runNixOSTest {
      name = "oxidom-scoped-routing";
      nodes = {
        client = {
          imports = [common self.nixosModules.default];
          environment.systemPackages = [oxidom-cli];
          networking.interfaces.eth1 = {
            ipv4.addresses = [
              {
                address = "10.42.0.2";
                prefixLength = 24;
              }
            ];
            ipv6.addresses = [
              {
                address = "2001:db8:42::2";
                prefixLength = 64;
              }
            ];
            ipv6.routes = [
              {
                address = "2001:db8:113::10";
                prefixLength = 128;
                via = "2001:db8:42::1";
              }
            ];
          };
          networking.defaultGateway = {
            address = "10.42.0.1";
            interface = "eth1";
          };
          networking.defaultGateway6 = null;
          users.users.tester = {
            isNormalUser = true;
            uid = 1000;
            linger = true;
          };
          services.oxidom = {
            enable = true;
            package = oxidom-cli;
            tun.enable = true;
            users = ["tester"];
          };
          systemd.services.oxidom = {
            environment.XRAY_LOCATION_ASSET = toString geoAssets;
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
          networking.defaultGateway6 = null;
          networking.interfaces.eth1 = {
            ipv4.addresses = [
              {
                address = "10.42.0.1";
                prefixLength = 24;
              }
            ];
            ipv6.addresses = [
              {
                address = "2001:db8:42::1";
                prefixLength = 64;
              }
              {
                address = "fe80::1";
                prefixLength = 64;
              }
            ];
          };
          networking.interfaces.lo = {
            ipv4.addresses = map (address: {
              inherit address;
              prefixLength = 32;
            }) ["192.0.2.10" "192.0.2.11" "203.0.113.10"];
            ipv6.addresses = [
              {
                address = "2001:db8:113::10";
                prefixLength = 128;
              }
            ];
          };
          boot.kernel.sysctl = {
            "net.ipv4.ip_forward" = 0;
            "net.ipv6.conf.all.forwarding" = 0;
          };
          systemd.services.p15-scoped-http = {
            wantedBy = ["multi-user.target"];
            requires = ["network-addresses-lo.service"];
            after = ["network.target" "network-addresses-lo.service"];
            serviceConfig = {
              ExecStart = "${pkgs.python3}/bin/python3 -u ${httpServer}";
              DynamicUser = true;
              NoNewPrivileges = true;
              CapabilityBoundingSet = "";
              AmbientCapabilities = "";
            };
          };
          systemd.services.p15-scoped-xray = {
            wantedBy = ["multi-user.target"];
            requires = ["network-addresses-lo.service"];
            after = ["network.target" "network-addresses-lo.service"];
            environment.XRAY_LOCATION_ASSET = toString geoAssets;
            serviceConfig = {
              ExecStart = "${pkgs.xray}/bin/xray run -config ${peerConfig}";
              DynamicUser = true;
              StateDirectory = "p15-scoped-xray";
              NoNewPrivileges = true;
              CapabilityBoundingSet = "";
              AmbientCapabilities = "";
            };
          };
        };
      };
      testScript = builtins.readFile ../../scripts/scoped-routing-vm.py;
    }
