{ sources ? (import ./nix/sources.nix),
  cargoChannel ? "1.50.0",
  nixpkgs ? (import sources.nixpkgs {
    overlays = [
      (import "${sources.nixpkgs-mozilla}/rust-overlay.nix")
      (import "${sources.nixpkgs-mozilla}/rust-src-overlay.nix")
      (import "${sources.cargo2nix}/overlay/default.nix")
      (self: super: rec {
        channel = nixpkgs.rustChannelOf { channel = cargoChannel; };
        rustc = channel.rust;
        rust = rustc.overrideAttrs (attrs: {
            toRustTarget = platform: with platform.parsed; let
              cpu_ = {
                "armv7a" = "armv7";
                "armv7l" = "armv7";
                "armv6l" = "arm";
              }.${cpu.name} or platform.rustc.arch or cpu.name;
            in platform.rustc.config
              or "${cpu_}-${vendor.name}-${kernel.name}${super.lib.optionalString (abi.name != "unknown") "-${abi.name}"}";
        });
        inherit (channel) cargo rust-fmt rust-std clippy;
      })
    ];
  }),
  ... }:
let
  pkgs = nixpkgs.pkgs;
  cargo2nix = (nixpkgs.callPackage sources.cargo2nix {}).package;
in
pkgs.mkShell {
  buildInputs = with pkgs; [
    cargo
    cargo-make
    cargo2nix
    git
    less
    libgit2
    niv
    nix
    openssl
    pkg-config
    rustc
    rustfmt
    vim
  ];
  shellHook = ''
    if [ -n "$IN_NIX_SHELL" ]; then
      unset SOURCE_DATE_EPOCH
    fi
    export EDITOR=vim
    export SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
    export NIX_SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
    export GIT_SSL_CAINFO=/etc/ssl/certs/ca-certificates.crt
    export PS1="\n\[\033[1;32m\][git-submerge:\w]\$\[\033[0m\] "
    alias make=makers
    alias vi=vim
  '';
}
