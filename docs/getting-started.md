# Build and run

Run these commands from the repository root.

With Nix:

```sh
nix develop path:.
cargo build --locked
```

Or run `nix build path:.` for `result/bin/geata`. The other guides use `geata`
as the command name; substitute `./result/bin/geata` when using this build.
Without Nix, install the Rust version in `rust-toolchain.toml`, a C/C++ compiler,
CMake, pkg-config, and OpenSSL development libraries, then run
`OPENSSL_NO_VENDOR=1 cargo build --locked` to use your system OpenSSL.

Run the included Hello world example on unprivileged ports; no backend is needed:

```sh
cargo run -- run --config examples/Geatafile.local \
  --http-listen 127.0.0.1:8080 --https-listen 127.0.0.1:8443
```

In another terminal:

```sh
curl http://localhost:8080
```

The explicit `http://` site address disables certificate requests for that site.
Both listeners are opened so a reload can introduce HTTPS sites later.

For public domains, see [HTTPS and operation](operations.md). For site syntax,
see [configuration](configuration.md).

Back to [Geata](../README.md).
