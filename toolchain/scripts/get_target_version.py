import os
import subprocess
import sys


def get_git_repo_root():
    return subprocess.getoutput("git rev-parse --show-toplevel")


def get_version_from_cargo_toml(repo_root):
    file_path = os.path.join(repo_root, "toolchain", "Cargo.toml")
    with open(file_path, 'r') as file:
        lines = file.readlines()
        in_workspace_package = False
        for line in lines:
            if "[workspace.package]" in line:
                in_workspace_package = True
            elif in_workspace_package and line.startswith("version"):
                return line.split('=')[1].strip().strip('"')


if __name__ == "__main__":
    repo_root = get_git_repo_root()
    tag = get_version_from_cargo_toml(repo_root)
    if os.environ.get('GITHUB_ENV'):
        with open(os.environ['GITHUB_ENV'], 'a') as f:
            f.write('ARTIFACT_VERSION=v' + tag)
    sys.stdout.write('v' + tag)
    sys.exit(0)
