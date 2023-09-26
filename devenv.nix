{ pkgs, lib, ... }:

{
  # https://devenv.sh/packages/
  packages = [
      pkgs.curl
      pkgs.git
      pkgs.jq
      pkgs.protobuf
      pkgs.protobuf3_21
      pkgs.nodejs_20
      pkgs.xdot
  ] ++ lib.optionals pkgs.stdenv.isDarwin (with pkgs.darwin.apple_sdk; [
       frameworks.Security
       frameworks.CoreFoundation
     ]);

  # https://devenv.sh/scripts/
  scripts.hello.exec = "echo Welcome to the Chidori dev enviroment";
  scripts.run-ui.exec = "(cd toolchain/prompt-graph-ui && yarn run tauri dev";

  enterShell = ''
    REPO_ROOT=`git rev-parse --show-toplevel`
    hello
  '';

  # https://devenv.sh/languages/
  languages.nix.enable = true;

  languages.python = {
      enable = true;
      poetry.enable = true;
      venv.enable = true;
    };

  languages.rust = {
      enable = true;
  };

  # https://devenv.sh/pre-commit-hooks/
  pre-commit.hooks.shellcheck.enable = true;

  services.clickhouse = {
     enable = true;
     config = ''
       http_port: 9000
       listen_host: 127.0.0.1
     '';
  };

# See full reference at https://devenv.sh/reference/options/
}
