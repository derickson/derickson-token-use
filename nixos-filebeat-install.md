# token-use standalone Filebeat on NixOS

The NixOS counterpart to [`mac-filebeat-install.sh`](mac-filebeat-install.sh): a
standalone **Filebeat** that tails the collector's NDJSON and ships it to the
*same* Elastic data streams as the Fleet integration — for NixOS hosts that
**cannot (or shouldn't) be enrolled in Fleet**.

On macOS the installer downloads a self-contained Filebeat, writes a local
`filebeat-token-use.yml`, and hand-registers a launchd agent. **None of that is
NixOS-like** — a downloaded binary won't even run (wrong dynamic linker), and the
hand-registered service is imperative drift. NixOS already ships Filebeat as a
package with a first-class **`services.filebeat`** module, so you don't download
anything and you don't hand-write a unit: you declare the same config the mac
`filebeat-token-use.yml` holds, and `nixos-rebuild` builds + supervises it.

> **Why not the Fleet-managed Elastic Agent?** There is no `elastic-agent` package
> or `services.elastic-agent` module in nixpkgs, and `elastic-agent install` copies
> a self-updating binary into `/opt/Elastic` with its own out-of-store systemd unit
> — imperative, mutable, invisible to `nixos-rebuild`. Standalone Filebeat via the
> packaged module is the declarative path, and it lands documents in exactly the
> same place (see [Data stream routing](README.md#data-stream-routing)).

This is the shipper. It assumes the **collector** is already running (writing
NDJSON to `~/.local/share/token-use/logs`), which on NixOS you set up with the
home-manager or NixOS module in [README-nix.md](README-nix.md).

## The config

Add this to the NixOS configuration for the host (a host module, or inline in the
host's `configuration.nix`). It is a 1:1 translation of
[`deploy/filebeat-token-use.example.yml`](deploy/filebeat-token-use.example.yml)
into the `services.filebeat` module — same filestream input, same `ndjson` parser,
same direct data-stream routing:

```nix
{ ... }:
let
  # The token-use collector writes NDJSON here (services.token-use default
  # outDir for user "dave"). Filebeat runs as root and reads it directly.
  user   = "dave";
  outDir = "/home/${user}/.local/share/token-use/logs";
in
{
  services.filebeat = {
    enable = true;

    # One filestream input — the module's `inputs.<name>` attrset becomes an
    # entry in `filebeat.inputs`. Mirrors the example yml exactly.
    inputs.token-use = {
      type    = "filestream";
      id      = "token-use";
      enabled = true;
      paths   = [ "${outDir}/*.ndjson" ];
      parsers = [
        # Decode each line and promote its fields to the top level — the same
        # preprocessing the Fleet integration's ndjson parser does.
        { ndjson = { target = ""; overwrite_keys = true; add_error_key = true; }; }
      ];
    };

    settings = {
      output.elasticsearch = {
        # (1) FILL ME — your Elastic Cloud deployment endpoint. Read it from a
        # Fleet-managed host's `elastic-agent inspect` (output.elasticsearch.hosts)
        # or the Cloud console. Form: https://<id>.<region>.<csp>.elastic.cloud:443
        hosts = [ "https://REPLACE_ME.us-east-1.aws.elastic.cloud:443" ];

        # (2) FILL ME — a dedicated API key in "id:key" form, kept OUT of git and
        # the Nix store. The module substitutes the file's contents into the
        # generated filebeat.yml at service start (ExecStartPre), so only the
        # *path* is in your config. See "Minting the API key" below.
        api_key = { _secret = "/etc/filebeat/token-use-api-key"; };

        # Route each record straight to its final data stream by event.dataset,
        # e.g. claude_code.turn -> logs-claude_code.turn-dericksontokenuse.
        index = "logs-%{[event.dataset]}-dericksontokenuse";
      };

      # Standalone Filebeat must not manage templates/ILM for Fleet-owned data
      # streams; a custom `index` also requires ILM off. (Mirrors how the managed
      # agent runs filebeat.)
      setup.template.enabled = false;
      setup.ilm.enabled      = false;

      logging.level = "info";
    };
  };
}
```

The module runs Filebeat as a **system** service (root, `StateDirectory
=/var/lib/filebeat`) — so it reads any user's `~/.local/share/token-use/logs`
regardless of home permissions. If you run the collector for several users, add
their globs to `paths`.

## The two values you supply

Both are kept out of git, exactly as on the mac path (the example yml carries only
`__ES_HOST__` / `__ID__:__API_KEY__` placeholders):

### 1. The Elasticsearch host

Your deployment endpoint, `https://<id>.<region>.<csp>.elastic.cloud:443`. This is
an endpoint, not a credential — inline it as shown. (If you'd rather keep it out of
a public repo too, put the whole host module in a git-ignored file, or template it
in with sops-nix / agenix.)

### 2. The API key (kept out of the store via `_secret`)

The `services.filebeat` module resolves any `{ _secret = "/path"; }` value by
reading that file at service start and splicing it into the generated
`filebeat.yml` — so the key never enters your Nix expressions or `/nix/store`.
Create the file out-of-band, root-only, **no trailing newline**:

```bash
sudo install -Dm600 /dev/stdin /etc/filebeat/token-use-api-key <<<'__ID__:__API_KEY__'
# or, to avoid the trailing newline heredoc adds:
printf '%s' '__ID__:__API_KEY__' | sudo install -Dm600 /dev/stdin /etc/filebeat/token-use-api-key
```

If the file is absent the service fails at ExecStartPre (can't read the secret) —
create it, then `systemctl restart filebeat`.

#### Minting the API key

Do **not** reuse the Fleet agent's key. In Kibana → *Stack Management → API keys*,
create one whose role descriptor allows writing the token-use data streams:

```json
{ "token-use-filebeat": {
  "cluster": ["monitor"],
  "indices": [ {
    "names": ["logs-claude_code.*-dericksontokenuse"],
    "privileges": ["auto_configure", "create_doc"] } ] } }
```

`cluster: ["monitor"]` is required — Filebeat calls `GET /` for a version check at
startup; without it you get `403 ... action [cluster:monitor/main] is
unauthorized`. Use the key in **`id:key`** form (not the base64 header form).

## Rebuild and verify

```bash
sudo nixos-rebuild switch --flake .#<host>

systemctl status filebeat            # up?
journalctl -u filebeat -f            # operational logs

# Confirm connectivity + config the way the mac script does, but against the
# generated config the module wrote:
sudo /run/current-system/sw/bin/filebeat \
  -c /var/lib/filebeat/filebeat.yml --path.data /var/lib/filebeat test output
```

Documents land in `logs-claude_code.turn-dericksontokenuse`,
`logs-claude_code.token_usage-dericksontokenuse`, etc. — the same targets as the
Fleet integration and the mac standalone shipper (see
[Data stream routing](README.md#data-stream-routing)).

## Re-shipping already-sent lines

The mac `mac-reprocess.sh` clears the Filebeat registry so every line is re-read.
The NixOS equivalent is to wipe the module's data dir and restart:

```bash
sudo systemctl stop filebeat
sudo rm -rf /var/lib/filebeat/registry
sudo systemctl start filebeat
```

Same caveat as the mac path: data streams reject a client-set `_id`, so re-shipped
lines are **not** deduped — you'll get duplicates for anything re-read.
