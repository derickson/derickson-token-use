{
  description = "token-use — per-call AI token-usage collector emitting NDJSON for Elasticsearch";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
  };

  outputs = { self, nixpkgs, ... }:
    let
      # The systems we build the package for. Add more if you ever need them.
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = f:
        nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      # Reproducible build of the collector binary. Cargo.lock is committed, so
      # `cargoLock.lockFile` pins every crate — no network fetch at build time.
      packages = forAllSystems (pkgs:
        let
          token-use = pkgs.rustPlatform.buildRustPackage {
            pname = "token-use";
            version = "0.1.0";
            src = self;
            cargoLock.lockFile = ./Cargo.lock;

            # notify uses inotify on Linux (no extra deps); on macOS it needs
            # the CoreServices framework for FSEvents.
            buildInputs = pkgs.lib.optionals pkgs.stdenv.isDarwin [
              pkgs.darwin.apple_sdk.frameworks.CoreServices
            ];

            meta = with pkgs.lib; {
              description = "Per-call AI token-usage collector emitting NDJSON for Elasticsearch";
              mainProgram = "token-use";
              license = licenses.mit;
              platforms = platforms.unix;
            };
          };
        in
        {
          inherit token-use;
          default = token-use;
        });

      # Expose the package as an overlay so consumers can pull it into their
      # own pkgs set (`pkgs.token-use`) if they prefer that to the flake output.
      overlays.default = final: prev: {
        token-use = self.packages.${final.stdenv.hostPlatform.system}.token-use;
      };

      # Per-user service (systemd --user). This is the NixOS analogue of what
      # install.sh registers on Ubuntu/macOS. Recommended entry point since the
      # daemon is inherently per-user (reads ~/.claude, writes ~/.local/share).
      homeModules.default = import ./nix/hm-module.nix self;

      # System-level variant: define the same service under systemd.user for a
      # chosen user, without home-manager. Use one OR the other, not both.
      nixosModules.default = import ./nix/nixos-module.nix self;

      # `nix run` / `nix develop` conveniences.
      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [ cargo rustc rustfmt clippy ];
        };
      });
    };
}
