# sapphire-agent-server

The app server of [sapphire-agent](https://github.com/sapphire-turtle/sapphire-agent): the
agent itself — its Matrix and Discord channels, HTTP API (`/rpc`, `/a2a`, `/acp`), tools,
memory and sessions — around sapphire-framework's app server, which owns the IPC endpoint
and the workspace.

It is a library, run as `sapphire-agent serve` by either edition
([`sapphire-agent-headless`](../../headless/README.md) or the desktop app). Up to 0.8 this
name was the server's binary crate. `config.example.toml` here lists every setting.

## License

MIT OR Apache-2.0.
