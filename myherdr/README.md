# myherdr

A personal Herdr fork, installed as `myherdr` next to the official `herdr`. It adds
automatic port forwarding from saved machines (branch `feat/remote-port-forwarding`).

Files in this directory are fork-only. Upstream never changes them, so upstream merges
do not conflict here.

## Setup (once per machine)

Requirements: `git`, a C toolchain, and Zig 0.16.0. The script finds Zig in
`~/.local/share/zig-*-0.16.0` and Rust in `~/.cargo/bin`.

```sh
# Rust (does not edit your shell profile) and Zig 0.16.0
curl -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --default-toolchain none --profile minimal
os=$(uname -s | tr A-Z a-z | sed 's/darwin/macos/'); arch=$(uname -m | sed 's/arm64/aarch64/')
mkdir -p ~/.local/share && curl -sSfL "https://ziglang.org/download/0.16.0/zig-$arch-$os-0.16.0.tar.xz" | tar xJ -C ~/.local/share

# Checkout used only for builds (use the https URL on machines that cannot push)
git clone -b feat/remote-port-forwarding git@github.com:bhoov/herdr.git ~/Projects/myherdr

# Put myherdr-update on your PATH
mkdir -p ~/.local/bin
ln -sf ~/Projects/myherdr/myherdr/update.sh ~/.local/bin/myherdr-update
```

If `~/.local/bin` is not on your `PATH` yet, add it to your shell startup file and open
a new shell:

```sh
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc    # zsh (macOS default)
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc   # bash (most Linux hosts)
command -v myherdr-update                                   # prints ~/.local/bin/myherdr-update
```

Then build and install `myherdr`:

```sh
myherdr-update
```

## Update

```sh
myherdr-update                    # personal computer
myherdr-update --restart-server   # remote machine: also restart its server on the new build
```

Each run merges your pushed branch and upstream `master`, pushes the merge (unless
`--no-push`), and rebuilds only when the commit changed. Machines can update in any
order. On a conflict, the script stops; merge `upstream/master` by hand in
`~/Projects/myherdr` and run it again.

`--restart-server` stops the running Herdr server, which ends the programs in its panes.
Without it, the old server keeps running until it restarts.

Do not run `myherdr update`. That is Herdr's own updater, and it installs an official
release.

## Remote machines

Saved machines start the remote Herdr found as `herdr` on `PATH` or `~/.local/bin/herdr`.
Point that name at this build on the remote, once:

```sh
[ -e ~/.local/bin/herdr ] && [ ! -L ~/.local/bin/herdr ] && mv ~/.local/bin/herdr ~/.local/bin/herdr.stable
ln -sf myherdr ~/.local/bin/herdr
```

Undo: `mv ~/.local/bin/herdr.stable ~/.local/bin/herdr && herdr server stop`.
