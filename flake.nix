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
          server =
            let
              fontconfig = pkgs.makeFontsConf {
                fontDirectories = [ pkgs.ibm-plex ];
              };
              unwrapped = pkgs.rustPlatform.buildRustPackage {
                pname = "sawthat-frame-server";
                version = "0.1.0";

                src = ./server;

                cargoLock.lockFile = ./server/Cargo.lock;

                meta = {
                  description = "SawThat Frame server for e-paper widgets";
                  mainProgram = "sawthat-frame-server";
                };
              };
            in
            pkgs.runCommand "sawthat-frame-server" {
              nativeBuildInputs = [ pkgs.makeWrapper ];
              inherit (unwrapped) meta;
            } ''
              mkdir -p $out/bin
              makeWrapper ${unwrapped}/bin/sawthat-frame-server $out/bin/sawthat-frame-server \
                --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.fontconfig ]} \
                --set FONTCONFIG_FILE ${fontconfig}
            '';

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
          cfg = config.services.sawthat-frame-server;
        in
        {
          options.services.sawthat-frame-server = {
            enable = lib.mkEnableOption "SawThat Frame server";

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
              description = "The sawthat-frame-server package to use";
            };

            openFirewall = lib.mkOption {
              type = lib.types.bool;
              default = false;
              description = "Whether to open the firewall port";
            };
          };

          config = lib.mkIf cfg.enable {
            networking.firewall.allowedTCPPorts = lib.mkIf cfg.openFirewall [ cfg.port ];
            systemd.services.sawthat-frame-server = {
              description = "SawThat Frame Server";
              wantedBy = [ "multi-user.target" ];
              after = [ "network.target" ];

              environment = {
                PORT = toString cfg.port;
                RUST_LOG = cfg.logLevel;
              };

              serviceConfig = {
                Type = "simple";
                ExecStart = "${cfg.package}/bin/sawthat-frame-server";
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
