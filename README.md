# Apollo Updated

Apollo Updated is a standalone, Linux-first Rust service and command-line client for managing signed package releases. It uses TUF metadata verification, immutable package generations, durable update state, and a local Unix-socket API.

The repository includes the `apollo-updated` daemon, `apollo-updatectl` client, an in-process fixture supervisor, systemd packaging, and qualification tests.

## Build and test

Rust 1.96 or newer is required.

```sh
cargo build --bins
cargo test --all-targets
```

The qualification test that creates a signed TUF repository uses Python 3 and OpenSSL to generate a disposable test key in a temporary directory.

## Configuration and packaging

Start from [`config.example.toml`](config.example.toml). Installation and trust-root setup are documented in [`packaging/README.md`](packaging/README.md); the private qualification key is not part of the public source repository or runtime package.

## License

MIT. See [`LICENSE`](LICENSE).
