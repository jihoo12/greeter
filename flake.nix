{
  description = "greeter: a minimal, zero-config CLI greeter for greetd";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "greeter";
          version = "0.3.0";

          src = ./.;

          cargoLock = {
            lockFile = ./Cargo.lock;
          };

          # No C library deps beyond libc, and no build-time codegen: keep
          # this fast and simple.
          doCheck = true;

          meta = with pkgs.lib; {
            description = "A minimal CLI greeter for greetd that just works out of the box";
            homepage = "https://github.com/jihoo12/greeter";
            license = licenses.mit;
            platforms = platforms.linux;
            mainProgram = "greeter";
          };
        };

        apps.default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/greeter";
        };

        devShells.default = pkgs.mkShell {
          packages = [ pkgs.cargo pkgs.rustc pkgs.rust-analyzer pkgs.clippy ];
        };
      }
    ) // {
      # NixOS module, shared across systems (not per-system).
      nixosModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.services.greetd-mini-greeter;
          system = pkgs.stdenv.hostPlatform.system;
          package =
            if self.packages ? ${system}
            then self.packages.${system}.default
            else pkgs.callPackage ./default.nix { };
        in
        {
          options.services.greetd-mini-greeter = {
            enable = lib.mkEnableOption "greeter as the greetd greeter";

            user = lib.mkOption {
              type = lib.types.str;
              default = "greeter";
              description = "User account the greeter itself runs as.";
            };
          };

          config = lib.mkIf cfg.enable {
            services.greetd = {
              enable = true;
              settings = {
                default_session = {
                  command = "${package}/bin/greeter";
                  user = cfg.user;
                };
              };
            };
          };
        };
    };
}
