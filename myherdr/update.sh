#!/usr/bin/env bash
# myherdr-update: merge the latest upstream Herdr into this fork, then rebuild and install
# `myherdr`. Run it on every machine that uses the fork. It merges instead of rebasing, so
# machines can run it in any order: the first one pushes the merge, the others fast-forward.
#
# Setup, once per machine (full steps, including PATH, in myherdr/README.md):
#   git clone -b feat/remote-port-forwarding git@github.com:bhoov/herdr.git ~/src/myherdr
#   ln -sf ~/src/myherdr/myherdr/update.sh ~/.local/bin/myherdr-update
#
# Do not run `myherdr update`: that is Herdr's own updater, which installs official releases.
set -euo pipefail

usage() {
    cat <<'EOF'
usage: myherdr-update [--no-push] [--restart-server]

  --no-push         do not push the merged branch to origin
  --restart-server  stop the running Herdr server so the next connection starts the new build
                    (this ends the processes in its panes)

environment:
  MYHERDR_BRANCH    fork branch to build (default: feat/remote-port-forwarding)
  MYHERDR_BIN_DIR   install directory (default: ~/.local/bin)
EOF
}

push=1
restart=0
for arg in "$@"; do
    case "$arg" in
        --no-push) push=0 ;;
        --restart-server) restart=1 ;;
        -h | --help) usage; exit 0 ;;
        *) usage >&2; exit 2 ;;
    esac
done

die() { echo "myherdr-update: $*" >&2; exit 1; }
step() { echo "==> $*"; }

src=$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.." && pwd)
branch=${MYHERDR_BRANCH:-feat/remote-port-forwarding}
bin_dir=${MYHERDR_BIN_DIR:-$HOME/.local/bin}
upstream_url=https://github.com/herdrdev/herdr.git

cd "$src"
[ -z "$(git status --porcelain --untracked-files=no)" ] || die "$src has uncommitted changes"

step "fetching upstream and origin"
git remote get-url upstream >/dev/null 2>&1 || git remote add upstream "$upstream_url"
git fetch --quiet upstream master
git fetch --quiet origin "$branch"
git switch --quiet "$branch"

step "merging origin/$branch and upstream/master"
git merge --quiet --no-edit "origin/$branch"
if ! git merge --quiet --no-edit upstream/master; then
    git merge --abort
    die "upstream/master conflicts with $branch; merge it by hand in $src, then rerun"
fi

if [ "$push" = 1 ] && [ -n "$(git rev-list "origin/$branch..HEAD")" ]; then
    step "pushing $branch"
    git push --quiet origin "$branch" || echo "warning: push failed; continuing with the local merge" >&2
fi

stamp="$bin_dir/.myherdr.commit"
if [ -x "$bin_dir/myherdr" ] && [ "$(cat "$stamp" 2>/dev/null)" = "$(git rev-parse HEAD)" ]; then
    step "already up to date ($(git rev-parse --short HEAD))"
else
    step "building"
    zig_dir=$(ls -d "$HOME"/.local/share/zig-*-0.16.0 2>/dev/null | head -n 1 || true)
    export PATH="$HOME/.cargo/bin${zig_dir:+:$zig_dir}:$PATH"
    cargo build --release --locked

    step "installing $bin_dir/myherdr"
    mkdir -p "$bin_dir"
    # Replace by rename, so a running myherdr keeps its old file.
    install -m 755 target/release/herdr "$bin_dir/.myherdr.new"
    mv -f "$bin_dir/.myherdr.new" "$bin_dir/myherdr"
    git rev-parse HEAD >"$stamp"
fi
"$bin_dir/myherdr" --version

if [ "$restart" = 1 ]; then
    step "stopping the running server"
    "$bin_dir/myherdr" server stop || true
else
    echo "note: a running server keeps its old build until it restarts (--restart-server)"
fi
