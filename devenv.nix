{ pkgs, lib, config, inputs, ... }:

{
  dotenv.enable = true;

  languages.rust.enable = true;

  env.TYTO_BINARY_OVERRIDE = "${config.devenv.root}/target/release/tyto";
  env.TYTO_CHANNEL = "dev";

  packages = with pkgs; [
    act
    cargo-outdated
    gh
    python3
    sqld
    sqlite
    turso-cli
    upx
  ];
}
