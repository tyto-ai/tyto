{ pkgs, lib, config, inputs, ... }:

{
  dotenv.enable = true;

  languages.rust.enable = true;

  env.MEMSO_BINARY_OVERRIDE = "${config.devenv.root}/target/release/memso";
  env.MEMSO_CHANNEL = "dev";

  packages = with pkgs; [
    act
    cargo-outdated
    gh
    python3
    sqld
    turso-cli
    upx
  ];
}
