# Native build and baseline verification

Use native entry points below. Build tooling and test selection are distinct from product requirements. Target task-specific cases and nearby regressions, record selected/filtered counts and exit codes. Zero selected tests is not success.

Pinned Rust 1.98.1; dependencies already cached locally. On a fresh machine run cargo fetch --locked once before offline tests. C compiler must be /usr/bin/gcc.

```sh
cargo test --locked --offline --lib --features bundled,backup,blob,hooks,functions,serde_json,vtab
```

## Selected dependency scope

This preparation targets bundled SQLite, not optional SQLCipher builds. The SQLCipher amalgamation is omitted to avoid supplying two complete database engines to the task author. SQLite source, Rust APIs, SQLite FFI and all selected features remain intact. Do not enable bundled-sqlcipher or propose SQLCipher-specific tasks. The initial preparation commit retains the complete upstream snapshot in Git history.
