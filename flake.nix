{
  description = "Loom runner controller for NixOS container workers";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems =
        f:
        nixpkgs.lib.genAttrs systems (
          system:
          f (import nixpkgs {
            inherit system;
          })
        );
    in
    {
      packages = forAllSystems (
        pkgs:
        let
          package = pkgs.rustPlatform.buildRustPackage {
            pname = "runner-controller";
            version = "0.1.0";

            src = pkgs.lib.cleanSource ./.;
            cargoLock.lockFile = ./Cargo.lock;

            nativeBuildInputs = with pkgs; [
              cmake
              pkg-config
            ];

            meta = with pkgs.lib; {
              description = "Loom worker advertisement controller for NixOS containers";
              license = licenses.mit;
              mainProgram = "runner-controller";
              platforms = platforms.linux;
            };
          };
        in
        {
          runner-controller = package;
          default = package;
        }
      );

      overlays.default = final: _prev: {
        runner-controller = self.packages.${final.system}.runner-controller;
      };

      nixosModules.runner-controller = import ./nix/module.nix { inherit self; };
      nixosModules.default = self.nixosModules.runner-controller;
    };
}
