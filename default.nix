# Non-flake entry point, for users still on `nix-channel`/`import <nixpkgs>`
# workflows. Usage:
#
#   nix-build
#   # or, in configuration.nix:
#   let greeter = pkgs.callPackage /path/to/greetd-mini-greeter { }; in ...
#
{ pkgs ? import <nixpkgs> { } }:

pkgs.rustPlatform.buildRustPackage {
  pname = "greeter";
  version = "0.2.0";

  src = ./.;

  cargoLock = {
    lockFile = ./Cargo.lock;
  };

  doCheck = true;

  meta = with pkgs.lib; {
    description = "A minimal CLI greeter for greetd that just works out of the box";
    license = licenses.mit;
    platforms = platforms.linux;
    mainProgram = "greeter";
  };
}
