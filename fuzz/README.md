# Fuzz targets

Targets for what arrives from outside (ADR 0031): sync/1, blobs/1 and pair/1 frames, signed
versions and device certificates, path validation, pairing codes, metadata snapshots and engine
state files. They build with the nightly toolchain pinned in `rust-toolchain.toml`.

```sh
cargo install cargo-fuzz --locked
cd fuzz
cargo fuzz list
cargo fuzz run sync_frame -- -max_total_time=60
```

`corpus/<target>` holds seed inputs, such as the frames of the spec's test vectors. A scheduled
job on `main` runs every target for ten minutes each night. A crash becomes a regression test in
the crate it hit, and its input goes in `corpus/<target>`.
