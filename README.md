# BA-FB WASM Runtime Benchmark

Benchmarks several WASM runtimes against a native C ABI cdylib for the exact
workload that matters to BA-FB: **load a byte buffer → decode FlatBuffer fields
→ emit JSON**.

## Runtimes compared

| # | Runtime | Kind | Notes |
|---|---------|------|-------|
| ① | Native inline | baseline | plain Rust fn, no FFI |
| ② | C ABI cdylib | native | `libloading` dlopen, same-process |
| ③ | [wasmi 1.1](https://github.com/wasmi-labs/wasmi) | interpreter | pure Rust, production-stable |
| ④ | [tinywasm 0.9](https://github.com/explodingcamera/tinywasm) | interpreter | pure Rust, minimal |
| ⑤ | [stitch](https://github.com/makepad/stitch) | threaded interp | sibling-call dispatch, very fast for an interpreter |
| ⑥ | [wasm3 0.3](https://github.com/wasm3/wasm3) | interpreter | C-based, fast |
| ⑦ | [wasmtime 46](https://github.com/bytecodealliance/wasmtime) | JIT (Cranelift) | near-native after warmup |
| ⑧ | [silverfir-nano](https://github.com/mbbill/Silverfir-nano) | JIT | embedding API in progress |

## Metrics

- **hot/call** — wall time per `extract()` call with a warm, reused instance
- **cold/call** — wall time including fresh instantiation (plugin reload cost)
- **vs①** — overhead multiplier vs the native inline baseline

## Project layout

```
ba-fb-bench/
├── bench/               # benchmark harness (host side)
│   └── src/main.rs      # one module per runtime, feature-gated
├── plugin/              # the WASM plugin (per-game-version artifact)
│   └── src/lib.rs       # alloc_buffer / dealloc_buffer / extract exports
├── wat/
│   └── extract.wat      # hand-written WAT used for all interpreter benches
└── .github/workflows/
    └── bench.yml        # CI: build × 3 OS, bench each runtime, summarize
```

## Running locally

```bash
# Build everything
cargo build --release

# Run all runtimes (pass path to native .so for the C ABI bench)
./target/release/bench ./target/release/libplugin.so

# Run a single runtime
cargo build --release -p bench --no-default-features --features runtime-wasmtime
./target/release/bench

# Build the wasm32 plugin (needs wasm32 target)
rustup target add wasm32-unknown-unknown
cargo build --release -p plugin --target wasm32-unknown-unknown
# output: target/wasm32-unknown-unknown/release/plugin.wasm
```

## Integrating with your BA-FB extractor

The `plugin` crate is the template for your per-game-version artifact:

1. Drop the flatc-generated `table.rs` into `plugin/src/`
2. Update `DISPATCH_TABLE` in `plugin/src/lib.rs` to add one entry per table
   (or use `build.rs` to generate it automatically — see comments in the file)
3. `cargo build --release -p plugin --target wasm32-unknown-unknown`
4. Ship `plugin.wasm` — the host extractor binary never changes

The host picks the runtime that wins this benchmark for your target platform
and loads the `.wasm` with the same `extract(in_ptr, in_len, type_id) -> u64`
ABI regardless of which runtime is used.

## Notes on silverfir-nano

Silverfir-nano (`sf-nano-core`) is listed as a git dependency but its
embedding API is not yet documented as a stable library surface — the project
is primarily a WASI CLI runtime right now. The bench slot is wired up and will
light up automatically once the API stabilises. Track progress at
<https://github.com/mbbill/Silverfir-nano/tree/main/sf-nano-core>.
