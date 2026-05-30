# runner-controller

Loom worker pool controller for NixOS containers. Workers advertise available software and per-second Cashu pricing over Nostr, accept paid job requests, dispatch them to warm worker containers, and publish job results with billing metadata.

## Payment Setup

The controller uses the external `cdk-cli` v0.16.0 binary as the Cashu wallet boundary. It does not link CDK crates directly.

Install the static `cdk-cli` v0.16.0 release binary on the host and make it executable:

```sh
install -m 0755 cdk-cli /usr/local/bin/cdk-cli
cdk-cli --version
```

Configure the controller wallet and pricing with environment variables:

```sh
export CDK_CLI_PATH=/usr/local/bin/cdk-cli
export STATE_DIR=/var/lib/runner-controller
export CDK_WORK_DIR=/var/lib/runner-controller/cdk-cli
export CDK_ENGINE=redb

export WORKER_PRICES=https://mint.example:10:sat
export WORKER_MIN_DURATION=5
export WORKER_MAX_DURATION=120
```

`CDK_CLI_PATH` defaults to `cdk-cli`, `CDK_WORK_DIR` defaults to `$STATE_DIR/cdk-cli`, `CDK_ENGINE` defaults to `redb`, `NIXOS_CONTAINER_BIN` defaults to `nixos-container`, `CONTAINER_TEMPLATE` defaults to `/etc/nixos/ci-container-template.nix`, and `HTTP_ADDR` defaults to `127.0.0.1`. `WORKER_PRICES` entries use `mint_url:price_per_second:unit`; `price_per_second` must be a positive integer.

On startup the controller runs `cdk-cli check-pending` for each configured payment unit using:

```sh
cdk-cli --work-dir "$CDK_WORK_DIR" --engine "$CDK_ENGINE" --unit sat check-pending
```

Job requests must include exactly one Nostr tag:

```json
["payment", "<cashu_token>"]
```

Before dispatching a job, the controller decodes the token, verifies that its mint and unit match a configured `WORKER_PRICES` entry, receives it with `cdk-cli receive`, and computes prepaid runtime as `amount / price_per_second`. For worker-pubkey-locked tokens, the worker slot secret is passed to `cdk-cli receive --signing-key` as raw hex. Payments below `WORKER_MIN_DURATION * price_per_second` are rejected.

The worker dispatch timeout is `min(prepaid_seconds, WORKER_MAX_DURATION)`. If that timeout expires, the controller treats the job as timed out, destroys the running worker container for that slot, and respawns a fresh worker so unpaid compute cannot continue in the background.

After completion or timeout, billing is `max(elapsed_seconds, WORKER_MIN_DURATION) * price_per_second`, capped at the prepaid amount. For controller-enforced timeouts, `elapsed_seconds` is the paid timeout that was enforced. If there is unused payment, the controller creates a change token with `cdk-cli send --mint-url <mint_url> --amount <amount>` and includes it in the result tags.

Result events include:

```json
["duration", "<elapsed_seconds>"]
["billable_duration", "<billable_seconds>"]
["cost", "<amount>"]
["mint", "<mint_url>"]
["unit", "<unit>"]
["change", "<cashu_token>"]
```

The `change` tag is omitted when no refund is due. If change creation fails after the job completes, the result is still published with an `error` tag; funds remain in the worker wallet for manual recovery.

## Required Runtime Configuration

At minimum, set:

```sh
export NOSTR_RELAYS=wss://relay.example
export WORKER_SOFTWARE=nix:2.24:/run/current-system/sw/bin/nix
export WORKER_PRICES=https://mint.example:10:sat
```

Useful optional settings:

```sh
export MAX_CONCURRENT=7
export POLL_INTERVAL=10
export ADVERTISE_INTERVAL=300
export JOB_TIMEOUT=7200
export WORKER_NAME=loom-worker
export WORKER_DEFAULT_SHELL=/bin/bash
export WORKER_WORK_DIR=/var/lib/loom-worker/work
export CONTAINER_TEMPLATE=/etc/nixos/ci-container-template.nix
```

## Running

For local development, run the binary with Cargo after setting the required environment variables:

```sh
cargo run
```

For a Nix-built binary:

```sh
nix build .#
./result/bin/runner-controller
```

For a NixOS service, import the module in your host configuration, rebuild, and inspect the unit:

```sh
sudo nixos-rebuild switch
sudo systemctl status runner-controller
journalctl -u runner-controller -f
```

To test the NixOS configuration without making it the boot default:

```sh
sudo nixos-rebuild test
```

Real operation should run on a NixOS host with `nixos-container`, systemd, a valid worker container template, and a configured `cdk-cli` v0.16.0 binary.

## Just Commands

Common development and operations commands are available through `just`:

```sh
just              # list commands
just fmt
just test
just check        # fmt + test
just run          # cargo run
just post-job     # publish a paid test job
just nix-check
just nix-build
just nix-run
just nixos-test
just nixos-switch
```

The `justfile` loads `.env` automatically if present, so test-job variables such as `NOSTR_RELAYS`, `REQUESTER_NSEC`, `WORKER_PUBKEY`, and `PAYMENT_TOKEN` can live there during development.

## Posting A Test Job

For manual end-to-end testing, the repo includes a helper binary that publishes an encrypted Nostr job request with a payment tag:

```sh
export NOSTR_RELAYS=wss://relay.example
export REQUESTER_NSEC=nsec1...
export WORKER_PUBKEY=<worker_hex_pubkey>
export PAYMENT_TOKEN=<cashu_token>

export JOB_REPO=nostr://_@example.com/repo
export JOB_REF=main
export JOB_WORKFLOW=.github/workflows/ci.yml
export JOB_NAME=test

just post-job
```

Optional fields:

```sh
export JOB_EVENT=push
export JOB_EVENT_PAYLOAD='{"after":"abc"}'
```

The helper publishes kind `5100`, encrypts the JSON payload to `WORKER_PUBKEY` with NIP-44, signs the request with `REQUESTER_NSEC`, and adds:

```json
["p", "<worker_hex_pubkey>"]
["payment", "<cashu_token>"]
```

## NixOS Installation

This repository exposes a flake package and NixOS module:

- `packages.${system}.default`
- `nixosModules.default`

Example host configuration:

```nix
{
  inputs.runner-controller.url = "path:/path/to/runner-controller";

  outputs = { self, nixpkgs, runner-controller, ... }: {
    nixosConfigurations.worker-host = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        runner-controller.nixosModules.default
        ({ pkgs, ... }: {
          services.runner-controller = {
            enable = true;

            relays = [ "wss://relay.example" ];
            workerSoftware = [
              {
                name = "nix";
                version = "2.24";
                path = "/run/current-system/sw/bin/nix";
              }
            ];
            workerPrices = [
              {
                mintUrl = "https://mint.example";
                pricePerSecond = 10;
                unit = "sat";
              }
            ];

            cdkCliPath = "/usr/local/bin/cdk-cli";
            cdkEngine = "redb";
            workerMinDuration = 5;
            workerMaxDuration = 120;

            # Optional: install the worker container template at the path
            # currently expected by the controller.
            containerTemplate = ./ci-container-template.nix;
          };
        })
      ];
    };
  };
}
```

The module runs `runner-controller` as root because it manages NixOS containers, systemd units, and network interfaces. It creates `STATE_DIR`, `CDK_WORK_DIR`, and `WORKER_WORK_DIR` with systemd-tmpfiles.

The module does not package `cdk-cli` v0.16.0. Install and verify that binary separately, then set `services.runner-controller.cdkCliPath` to its absolute path. The module passes the NixOS-generated `nixos-container` executable by default; override `services.runner-controller.nixosContainerBin` only for custom host layouts. If `containerTemplate` is not set, `/etc/nixos/ci-container-template.nix` must already exist on the host.

## Tests

Run the normal unit test suite with:

```sh
cargo test
```

Ignored payment integration tests are placeholders for environments with a real `cdk-cli` v0.16.0 binary and funded test mint:

```sh
CDK_CLI_PATH=/usr/local/bin/cdk-cli cargo test -- --ignored
```
