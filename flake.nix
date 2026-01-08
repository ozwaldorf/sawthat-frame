{
  inputs = {
    nixpkgs.url = "github:ozwaldorf/nixpkgs/espup/patchelf";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    { self, nixpkgs, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
      in
      {
        packages = {
          server = pkgs.rustPlatform.buildRustPackage {
            pname = "concert-display-server";
            version = "0.1.0";

            src = ./server;

            cargoLock.lockFile = ./server/Cargo.lock;

            meta = {
              description = "Concert display server for e-paper widgets";
              mainProgram = "concert-display-server";
            };
          };

          default = self.packages.${system}.server;
        };

        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            rustup
            espup
            espflash
            fastly
          ];
        };
      }
    )
    // {
      nixosModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.services.concert-display-server;
        in
        {
          options.services.concert-display-server = {
            enable = lib.mkEnableOption "Concert Display server";

            port = lib.mkOption {
              type = lib.types.port;
              default = 3000;
              description = "Port to listen on";
            };

            logLevel = lib.mkOption {
              type = lib.types.str;
              default = "info";
              description = "RUST_LOG filter string";
            };

            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.system}.server;
              description = "The concert-display-server package to use";
            };

            openFirewall = lib.mkOption {
              type = lib.types.bool;
              default = false;
              description = "Whether to open the firewall port";
            };
          };

          config = lib.mkIf cfg.enable {
            networking.firewall.allowedTCPPorts = lib.mkIf cfg.openFirewall [ cfg.port ];
            systemd.services.concert-display-server = {
              description = "Concert Display Server";
              wantedBy = [ "multi-user.target" ];
              after = [ "network.target" ];

              environment = {
                PORT = toString cfg.port;
                RUST_LOG = cfg.logLevel;
              };

              serviceConfig = {
                Type = "simple";
                ExecStart = "${cfg.package}/bin/concert-display-server";
                Restart = "on-failure";
                RestartSec = 5;

                # Hardening
                DynamicUser = true;
                NoNewPrivileges = true;
                ProtectSystem = "strict";
                ProtectHome = true;
                PrivateTmp = true;
                PrivateDevices = true;
                ProtectKernelTunables = true;
                ProtectKernelModules = true;
                ProtectControlGroups = true;
                RestrictNamespaces = true;
                RestrictSUIDSGID = true;
              };
            };
          };
        };
    };
}
