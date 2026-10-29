# sapphire-agent-headless

The headless edition of [sapphire-agent](https://github.com/sapphire-turtle/sapphire-agent) — a personal AI
assistant agent that works on file-based, local-first sapphire-framework workspaces — for hosts
without a GUI. Its one binary, `sapphire-agent`, is the agent server (`serve`) and its CLI (`chat`,
`acp`, `init`, `verify`, and sapphire-framework's `status`, `service`, `workspace`, …). The
desktop app's binary runs the same commands and is put on PATH under the same name.

## Installation

```sh
# Install script (Linux, macOS), with sapphire-bridge
curl -fsSL https://raw.githubusercontent.com/sapphire-turtle/sapphire-framework/main/install/install.sh | sh -s -- agent

cargo binstall sapphire-agent-headless   # the release file
cargo install sapphire-agent-headless    # or build it
```

Either installs the `sapphire-agent` binary. The desktop app's binary runs the same
commands; install only one of the two. Up to 0.11 this edition was published as the
`sapphire-agent` crate.

See the [repository's README](https://github.com/sapphire-turtle/sapphire-agent#readme) for
configuration and usage.

## License

MIT OR Apache-2.0.
