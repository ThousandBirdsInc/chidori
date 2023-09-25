{ pkgs, lib, ... }:

{
  # https://devenv.sh/packages/
  packages = [
      pkgs.curl
      pkgs.git
      pkgs.jq
      pkgs.kubectl
      pkgs.protobuf
      pkgs.pulumi
      pkgs.awscli2
      pkgs.protobuf3_21
      pkgs.kubernetes-helm
      pkgs.flyway
      pkgs.argo-rollouts
      pkgs.argocd
      pkgs.nodejs_20
      pkgs.tilt
      pkgs.aws-sam-cli
      pkgs.xdot
  ] ++ lib.optionals pkgs.stdenv.isDarwin (with pkgs.darwin.apple_sdk; [
       frameworks.Security
       frameworks.CoreFoundation
     ]);

  # https://devenv.sh/scripts/
  scripts.hello.exec = "echo Welcome to Thousand Birds";
  scripts.tbngrok.exec = "ngrok http --domain=thousandbirds.ngrok.dev 3002";
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

  services.temporal = {
     enable = true;

     port = 17233;

     namespaces = [ "mynamespace" ];

     state = {
       ephemeral = true;
       sqlite-pragma = {
         journal_mode = "wal";
       };
     };
  };

# See full reference at https://devenv.sh/reference/options/
}
