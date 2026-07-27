# token-use on NixOS

Special install instructions for NixOS. On NixOS you **don't** run `install.sh`
(it drops files into `~/.local/bin` and hand-registers a systemd/launchd unit —
imperative state NixOS doesn't want). Instead this repo is a **flake**: it builds
the collector reproducibly and ships a home-manager module that registers the
same `systemd --user` service declaratively. See the main [README](README.md) for
what the daemon does and its runtime configuration.

## What the flake provides

| Output | Purpose |
|---|---|
| `packages.<system>.token-use` (`.default`) | The collector binary, built with `rustPlatform.buildRustPackage` from the committed `Cargo.lock` — fully pinned, no network fetch at build time. |
| `overlays.default` | Same package as `pkgs.token-use`, if you prefer pulling it into your own package set. |
| `homeModules.default` | **Recommended.** home-manager module that runs token-use as a per-user `systemd --user` service. Per-user is the natural fit: the daemon reads `~/.claude` and writes `~/.local/share`. |
| `nixosModules.default` | System-level variant that defines the same `systemd.user` service without home-manager (plus `lingerUsers`). Use *either* this *or* the home-manager module, not both. |
| `devShells.<system>.default` | `cargo`/`rustc`/`clippy`/`rustfmt` for hacking on the source. |

## Quick try (no install)

```bash
nix run github:derickson/derickson-token-use          # run the collector once
nix build github:derickson/derickson-token-use        # just build the binary -> ./result/bin/token-use
```

## Install via home-manager (recommended)

Add the input to your flake, making it follow your `nixpkgs` so the build uses
your pinned package set (no version drift):

```nix
# flake.nix
inputs = {
  # ... your existing inputs (nixpkgs, home-manager, ...)
  token-use = {
    # Public repo: github:derickson/derickson-token-use
    # Private repo: reuse your SSH key instead of a token:
    url = "git+ssh://git@github.com/derickson/derickson-token-use.git";
    inputs.nixpkgs.follows = "nixpkgs";
  };
};
```

Import the module wherever your home-manager config lives and enable the service:

```nix
imports = [ inputs.token-use.homeModules.default ];
services.token-use.enable = true;
```

If you drive home-manager as a NixOS module (the `home-manager.nixosModules.home-manager`
setup), the tidiest way to expose the module to every user is `sharedModules`:

```nix
# in your nixosSystem modules, alongside home-manager.useGlobalPkgs etc.
home-manager.sharedModules = [ inputs.token-use.homeModules.default ];
```

…then each user opts in with `services.token-use.enable = true;` in their own
home config. Because the service is a `systemd --user` unit, put the `enable`
line in a **Linux-only** file if your home config is shared with macOS.

Rebuild:

```bash
sudo nixos-rebuild switch --flake .#<host>     # first run locks the token-use input
```

## Options

All under `services.token-use`:

| Option | Default | Maps to |
|---|---|---|
| `enable` | `false` | register the systemd user service |
| `package` | this flake's `token-use` | the binary to run |
| `outDir` | `~/.local/share/token-use/logs` | `TOKEN_USE_OUT_DIR` (NDJSON output; a Filebeat/Elastic Agent shipper tails this) |
| `stateDir` | `~/.local/state/token-use` | `TOKEN_USE_STATE_DIR` (durable checkpoint) |
| `logLevel` | `"info"` | `RUST_LOG` |
| `extraEnvironment` | `{}` | any other [config var](README.md#configuration-environment) — `TOKEN_USE_DEBOUNCE_MS`, `TOKEN_USE_TICK_SECS`, `TOKEN_USE_HOME` |

Example with overrides:

```nix
services.token-use = {
  enable = true;
  logLevel = "debug";
  extraEnvironment = {
    TOKEN_USE_TICK_SECS = "120";
    TOKEN_USE_DEBOUNCE_MS = "1000";
  };
};
```

## System-level module (alternative)

If you'd rather not use home-manager, import the NixOS module instead. It defines
the same `systemd.user` service (paths use `%h`, so it works for any user) and can
enable lingering so it runs without an active login:

```nix
imports = [ inputs.token-use.nixosModules.default ];
services.token-use = {
  enable = true;
  lingerUsers = [ "dave" ];   # keep the user service alive at boot, no login needed
};
```

## Managing the service

```bash
systemctl --user status token-use      # is it up?
systemctl --user restart token-use
journalctl --user -u token-use -f      # operational logs (stderr)
```

Output NDJSON still lands in `~/.local/share/token-use/logs/` — the shipper setup
(standalone Filebeat / Fleet Elastic Agent) in the main [README](README.md) is
unchanged.

## Local development against an uncommitted checkout

The flake input points at a remote, so a rebuild fetches the last **pushed**
commit. To test local, uncommitted changes without pushing, override the input to
your working tree (git-tracked files only, so `target/` isn't copied):

```bash
sudo nixos-rebuild build --flake .#<host> \
  --override-input token-use "git+file:///absolute/path/to/derickson-token-use"
```

Or hack on the source directly:

```bash
nix develop github:derickson/derickson-token-use   # cargo/rustc/clippy shell
```
