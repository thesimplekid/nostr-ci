set dotenv-load

default:
    just --list

fmt:
    cargo fmt

test:
    cargo test

check: fmt test

run:
    cargo run

post-job:
    cargo run --bin post_job

nix-check:
    nix flake check

nix-build:
    nix build .#

nix-run:
    nix run .#

nixos-test:
    sudo nixos-rebuild test

nixos-switch:
    sudo nixos-rebuild switch
