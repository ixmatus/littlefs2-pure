# verify_image

C-reference verifier for round-trip conformance: takes a LittleFS
image produced by `littlefs2-pure`, mounts it via the C reference,
and validates expected file contents. Exit status 0 means the image
is byte-compatible with the C reference; non-zero means a mismatch.

Companions `tools/gen_vectors/` (C-writes-Rust-reads direction).
Together they prove bit accuracy in both directions.

## Building

```sh
make
```

Produces `build/verify_image`. Reuses the vendored C littlefs at
`../gen_vectors/littlefs/`, so no additional vendoring.

## Running

```sh
build/verify_image path/to/image.bin <scenario>
```

Scenarios:

| Scenario | Expected entry |
|---|---|
| `inline` | `/cfg` with body `"hello, rust"` |
| `ctz`    | `/payload.bin` with 500 bytes of `i & 0xff` |
| `nested` | `/audit/log` with body `"entry-0001;"` |

`tests/roundtrip.rs` exercises every scenario: produces an image
with this crate's writer, saves to a temp file, invokes
`build/verify_image`. If the binary is missing the test is skipped
with a message pointing here.
