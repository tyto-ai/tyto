{ pkgs, lib, config, inputs, ... }:

{
  dotenv.enable = true;

  languages.rust.enable = true;

  env.TYTO_BINARY_OVERRIDE = "${config.devenv.root}/target/release/tyto";
  env.TYTO_CHANNEL = "dev";
  env.RUST_LOG = "tyto=debug";

  packages = with pkgs; [
    act
    cargo-bloat
    cargo-outdated
    gh
    openssl
    pkg-config
    python3
    sqld
    sqlite
    turso-cli
    upx
  ];
}
