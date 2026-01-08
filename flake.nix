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
    );
}
