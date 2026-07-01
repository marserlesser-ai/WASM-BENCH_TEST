//! BA-FB WASM Runtime Benchmark
//!
//! Measures the overhead of each WASM runtime vs a native baseline for the
//! kind of work BA-FB does: load a byte buffer, decode fields, emit JSON.
//!
//! Each runtime is feature-gated; build with all defaults or select a subset:
//!   cargo build --release --features runtime-wasmtime,runtime-cabi
//!
//! The WAT module at ../wat/extract.wat is used for every interpreter.
//! For the C ABI path a native cdylib must be compiled separately:
//!   cargo build --release -p plugin --target <host>   (non-wasm build)
//! and its path passed as the first CLI argument.

#![allow(unused_variables, unused_mut, dead_code)]

use std::{
    hint::black_box,
    time::{Duration, Instant},
};

// ── bench parameters ─────────────────────────────────────────────────────────

const ITERATIONS:  u64 = 500_000;
const WARMUP:      u64 =  10_000;
const COLD_ITERS:  u64 =     500;  // re-instantiate every call (cold-start cost)

// ── WAT source (compiled to wasm bytes by each runtime's loader) ─────────────

const WAT_SRC: &str = include_str!("../../wat/extract.wat");

// ── benchmark payload: simulates a FlatBuffer byte buffer ───────────────────

fn make_payload() -> Vec<u8> {
    let mut buf = vec![0u8; 100];
    for i in 0u64..10 {
        let val: i64 = (i as i64 + 1) * 1_000;
        let off = i as usize * 8;
        buf[off..off + 8].copy_from_slice(&val.to_le_bytes());
    }
    for i in 0u32..5 {
        let val: f32 = (i as f32 + 1.0) * 1.5;
        let off = 80 + i as usize * 4;
        buf[off..off + 4].copy_from_slice(&val.to_le_bytes());
    }
    buf
}

// ── result formatting ────────────────────────────────────────────────────────

fn fmt_ns(d: Duration, iters: u64) -> String {
    let ns = d.as_nanos() as f64 / iters as f64;
    if ns < 1_000.0       { format!("{ns:>8.1} ns") }
    else if ns < 1_000_000.0 { format!("{:>8.2} µs", ns / 1_000.0) }
    else                  { format!("{:>8.2} ms", ns / 1_000_000.0) }
}

fn fmt_ops(d: Duration, iters: u64) -> String {
    let ops = iters as f64 / d.as_secs_f64();
    if ops >= 1_000_000.0 { format!("{:>7.2}M/s", ops / 1_000_000.0) }
    else if ops >= 1_000.0 { format!("{:>7.1}K/s", ops / 1_000.0) }
    else                   { format!("{ops:>7.0}/s") }
}

fn overhead(baseline: Duration, other: Duration, iters: u64) -> String {
    let b = baseline.as_nanos() as f64 / iters as f64;
    let o = other.as_nanos()   as f64 / iters as f64;
    let x = o / b;
    format!("{x:>6.1}x")
}

fn print_row(label: &str, hot: Duration, cold: Option<Duration>, baseline: Duration) {
    let hot_str  = fmt_ns(hot, ITERATIONS);
    let ops_str  = fmt_ops(hot, ITERATIONS);
    let ovhd_str = overhead(baseline, hot, ITERATIONS);
    let cold_str = cold.map_or("         ".into(), |c| fmt_ns(c, COLD_ITERS));
    println!("  {label:<26}  {hot_str}  {ops_str}  {ovhd_str}  cold:{cold_str}");
}

// ── native inline baseline ───────────────────────────────────────────────────

#[inline(never)]
fn native_extract(buf: &[u8]) -> (i64, f64) {
    let mut si: i64 = 0;
    let mut sf: f64 = 0.0;
    for i in 0..10usize {
        si = si.wrapping_add(i64::from_le_bytes(buf[i*8..i*8+8].try_into().unwrap()));
    }
    for i in 0..5usize {
        sf += f32::from_le_bytes(buf[80+i*4..80+i*4+4].try_into().unwrap()) as f64;
    }
    (si, sf)
}

fn bench_native(payload: &[u8]) -> Duration {
    for _ in 0..WARMUP { black_box(native_extract(payload)); }
    let t = Instant::now();
    for _ in 0..ITERATIONS { black_box(native_extract(payload)); }
    t.elapsed()
}

// ── C ABI cdylib via libloading ──────────────────────────────────────────────

#[cfg(feature = "runtime-cabi")]
mod cabi {
    use super::*;
    use libloading::{Library, Symbol};

    type ExtractFn = unsafe extern "C" fn(*const u8, usize, u32) -> u64;

    pub fn bench(payload: &[u8], lib_path: &str) -> Option<(Duration, Duration)> {
        if !std::path::Path::new(lib_path).exists() {
            eprintln!("  [C ABI] .so not found at {lib_path}, skipping");
            return None;
        }
        let lib = unsafe { Library::new(lib_path) }.ok()?;
        let extract: Symbol<ExtractFn> = unsafe { lib.get(b"extract") }.ok()?;

        // warmup
        for _ in 0..WARMUP {
            black_box(unsafe { extract(payload.as_ptr(), payload.len(), 0) });
        }

        // hot bench
        let t = Instant::now();
        for _ in 0..ITERATIONS {
            black_box(unsafe { extract(payload.as_ptr(), payload.len(), 0) });
        }
        let hot = t.elapsed();

        // cold bench: libloading::Library::new every iteration (plugin reload cost)
        let t2 = Instant::now();
        for _ in 0..COLD_ITERS {
            let l2 = unsafe { Library::new(lib_path) }.unwrap();
            let ex: Symbol<ExtractFn> = unsafe { l2.get(b"extract") }.unwrap();
            black_box(unsafe { ex(payload.as_ptr(), payload.len(), 0) });
        }
        let cold = t2.elapsed();

        Some((hot, cold))
    }
}

// ── wasmi 1.1 ────────────────────────────────────────────────────────────────

#[cfg(feature = "runtime-wasmi")]
mod runtime_wasmi {
    use super::*;
    use wasmi::{Engine, Linker, Module, Store};

    fn wat_to_wasm(src: &str) -> Vec<u8> {
        // wasmi 1.x ships its own wat parser
        wat::parse_str(src).expect("WAT parse failed")
    }

    pub fn bench(payload: &[u8]) -> (Duration, Duration) {
        let wasm   = wat_to_wasm(WAT_SRC);
        let engine = Engine::default();
        let module = Module::new(&engine, &wasm[..]).expect("wasmi: module");
        let mut store: Store<()> = Store::new(&engine, ());
        let linker  = Linker::new(&engine);
        let instance = linker
            .instantiate(&mut store, &module)
            .and_then(|i| i.start(&mut store))
            .expect("wasmi: instantiate");

        let memory = instance
            .get_memory(&store, "memory")
            .expect("wasmi: memory");
        let extract = instance
            .get_typed_func::<(i32, i32, i32), i64>(&store, "extract")
            .expect("wasmi: extract fn");

        // write payload into wasm memory once
        memory.write(&mut store, 0, payload).expect("wasmi: write");

        // warmup
        for _ in 0..WARMUP {
            black_box(extract.call(&mut store, (0, payload.len() as i32, 0)).unwrap());
        }

        // hot
        let t = Instant::now();
        for _ in 0..ITERATIONS {
            black_box(extract.call(&mut store, (0, payload.len() as i32, 0)).unwrap());
        }
        let hot = t.elapsed();

        // cold (re-instantiate per call)
        let t2 = Instant::now();
        for _ in 0..COLD_ITERS {
            let mut s: Store<()> = Store::new(&engine, ());
            let inst = linker
                .instantiate(&mut s, &module)
                .and_then(|i| i.start(&mut s))
                .unwrap();
            let mem = inst.get_memory(&s, "memory").unwrap();
            let ex  = inst.get_typed_func::<(i32, i32, i32), i64>(&s, "extract").unwrap();
            mem.write(&mut s, 0, payload).unwrap();
            black_box(ex.call(&mut s, (0, payload.len() as i32, 0)).unwrap());
        }
        let cold = t2.elapsed();

        (hot, cold)
    }
}

// ── tinywasm 0.9 ─────────────────────────────────────────────────────────────

#[cfg(feature = "runtime-tinywasm")]
mod runtime_tinywasm {
    use super::*;
    use tinywasm::{Extern, Imports, Module, Store};
    use tinywasm_parser::Parser;

    fn wat_to_wasm(src: &str) -> Vec<u8> {
        wat::parse_str(src).expect("WAT parse failed")
    }

    pub fn bench(payload: &[u8]) -> (Duration, Duration) {
        let wasm   = wat_to_wasm(WAT_SRC);
        let module = Parser::default()
            .parse_module_bytes(&wasm)
            .and_then(Module::new)
            .expect("tinywasm: parse");

        let mut store = Store::default();
        let imports = Imports::default();
        let instance = module
            .instantiate(&mut store, Some(imports))
            .expect("tinywasm: instantiate");

        let memory = instance
            .exported_memory(&store, "memory")
            .expect("tinywasm: memory");
        let extract = instance
            .exported_func::<(i32, i32, i32), i64>(&store, "extract")
            .expect("tinywasm: extract fn");

        memory
            .store(&mut store, 0, 0, payload)
            .expect("tinywasm: write");

        // warmup
        for _ in 0..WARMUP {
            black_box(extract.call(&mut store, (0, payload.len() as i32, 0)).unwrap());
        }

        // hot
        let t = Instant::now();
        for _ in 0..ITERATIONS {
            black_box(extract.call(&mut store, (0, payload.len() as i32, 0)).unwrap());
        }
        let hot = t.elapsed();

        // cold
        let t2 = Instant::now();
        for _ in 0..COLD_ITERS {
            let mut s = Store::default();
            let inst = module.instantiate(&mut s, Some(Imports::default())).unwrap();
            let mem  = inst.exported_memory(&s, "memory").unwrap();
            let ex   = inst.exported_func::<(i32, i32, i32), i64>(&s, "extract").unwrap();
            mem.store(&mut s, 0, 0, payload).unwrap();
            black_box(ex.call(&mut s, (0, payload.len() as i32, 0)).unwrap());
        }
        let cold = t2.elapsed();

        (hot, cold)
    }
}

// ── stitch ───────────────────────────────────────────────────────────────────

#[cfg(feature = "runtime-stitch")]
mod runtime_stitch {
    use super::*;
    use makepad_stitch::{Engine, Linker, Module, Store, Val};
    use wast::{parser, parser::ParseBuffer, Wat};

    fn wat_to_wasm(src: &str) -> Vec<u8> {
        let buf = ParseBuffer::new(src).expect("stitch: ParseBuffer");
        let mut wat = parser::parse::<Wat>(&buf).expect("stitch: parse WAT");
        wat.encode().expect("stitch: encode")
    }

    pub fn bench(payload: &[u8]) -> (Duration, Duration) {
        let wasm     = wat_to_wasm(WAT_SRC);
        let engine   = Engine::new();
        let mut store = Store::new(engine);
        let module   = Module::new(store.engine(), &wasm).expect("stitch: module");
        let instance = Linker::new()
            .instantiate(&mut store, &module)
            .expect("stitch: instantiate");

        let memory  = instance.exported_mem("memory").expect("stitch: memory");
        let extract = instance.exported_func("extract").expect("stitch: extract fn");

        {
            let bytes = memory.bytes_mut(&mut store);
            bytes[..payload.len()].copy_from_slice(payload);
        }

        let mut results = [Val::I64(0)];
        let args = [Val::I32(0), Val::I32(payload.len() as i32), Val::I32(0)];

        // warmup
        for _ in 0..WARMUP {
            extract.call(&mut store, &args, &mut results).unwrap();
            black_box(results[0].clone());
        }

        // hot
        let t = Instant::now();
        for _ in 0..ITERATIONS {
            extract.call(&mut store, &args, &mut results).unwrap();
            black_box(results[0].clone());
        }
        let hot = t.elapsed();

        // cold
        let t2 = Instant::now();
        for _ in 0..COLD_ITERS {
            let eng = Engine::new();
            let mut s = Store::new(eng);
            let m   = Module::new(s.engine(), &wasm).unwrap();
            let inst = Linker::new().instantiate(&mut s, &m).unwrap();
            let mem  = inst.exported_mem("memory").unwrap();
            let ex   = inst.exported_func("extract").unwrap();
            {
                let bytes = mem.bytes_mut(&mut s);
                bytes[..payload.len()].copy_from_slice(payload);
            }
            let mut r = [Val::I64(0)];
            ex.call(&mut s, &args, &mut r).unwrap();
            black_box(r[0].clone());
        }
        let cold = t2.elapsed();

        (hot, cold)
    }
}

// ── wasm3 ────────────────────────────────────────────────────────────────────

#[cfg(feature = "runtime-wasm3")]
mod runtime_wasm3 {
    use super::*;
    use wasm3::{Environment, Module};

    fn wat_to_wasm(src: &str) -> Vec<u8> {
        wat::parse_str(src).expect("WAT parse failed")
    }

    pub fn bench(payload: &[u8]) -> (Duration, Duration) {
        let wasm = wat_to_wasm(WAT_SRC);
        let env  = Environment::new().expect("wasm3: env");

        // wasm3 stack size in slots
        let rt = env.create_runtime(1024).expect("wasm3: runtime");
        let module = Module::parse(&env, &wasm).expect("wasm3: parse");
        let mut module = rt.load_module(module).expect("wasm3: load");

        // wasm3 function signature: "(i i i)L" = (i32 i32 i32) -> i64
        let extract = module
            .find_function::<(i32, i32, i32), i64>("extract")
            .expect("wasm3: extract fn");

        // wasm3 exposes memory as a raw slice via the runtime
        // Write payload directly into linear memory at offset 0
        // wasm3-rs exposes rt.memory() for direct access
        {
            let mem = rt.memory();
            mem[..payload.len()].copy_from_slice(payload);
        }

        // warmup
        for _ in 0..WARMUP {
            black_box(extract.call(0, payload.len() as i32, 0).unwrap());
        }

        // hot
        let t = Instant::now();
        for _ in 0..ITERATIONS {
            black_box(extract.call(0, payload.len() as i32, 0).unwrap());
        }
        let hot = t.elapsed();

        // cold — wasm3 runtime creation is the instantiation cost
        let t2 = Instant::now();
        for _ in 0..COLD_ITERS {
            let e2  = Environment::new().unwrap();
            let rt2 = e2.create_runtime(1024).unwrap();
            let m2  = Module::parse(&e2, &wasm).unwrap();
            let mut m2 = rt2.load_module(m2).unwrap();
            let ex  = m2.find_function::<(i32, i32, i32), i64>("extract").unwrap();
            {
                let mem = rt2.memory();
                mem[..payload.len()].copy_from_slice(payload);
            }
            black_box(ex.call(0, payload.len() as i32, 0).unwrap());
        }
        let cold = t2.elapsed();

        (hot, cold)
    }
}

// ── wasmtime 46 ──────────────────────────────────────────────────────────────

#[cfg(feature = "runtime-wasmtime")]
mod runtime_wasmtime {
    use super::*;
    use wasmtime::{Config, Engine, Linker, Module, OptLevel, Store, Strategy};

    fn wat_to_wasm(src: &str) -> Vec<u8> {
        // wasmtime can parse WAT natively
        wat::parse_str(src).expect("WAT parse failed")
    }

    pub fn bench(payload: &[u8]) -> (Duration, Duration) {
        let wasm = wat_to_wasm(WAT_SRC);

        let mut cfg = Config::new();
        cfg.cranelift_opt_level(OptLevel::Speed);
        cfg.strategy(Strategy::Cranelift);
        let engine = Engine::new(&cfg).expect("wasmtime: engine");
        let module = Module::new(&engine, &wasm).expect("wasmtime: module");

        let mut store: Store<()> = Store::new(&engine, ());
        let linker = Linker::new(&engine);
        let instance = linker
            .instantiate(&mut store, &module)
            .expect("wasmtime: instantiate");

        let memory = instance
            .get_memory(&mut store, "memory")
            .expect("wasmtime: memory");
        let extract = instance
            .get_typed_func::<(i32, i32, i32), i64>(&mut store, "extract")
            .expect("wasmtime: extract fn");

        memory.write(&mut store, 0, payload).expect("wasmtime: write");

        // warmup
        for _ in 0..WARMUP {
            black_box(extract.call(&mut store, (0, payload.len() as i32, 0)).unwrap());
        }

        // hot
        let t = Instant::now();
        for _ in 0..ITERATIONS {
            black_box(extract.call(&mut store, (0, payload.len() as i32, 0)).unwrap());
        }
        let hot = t.elapsed();

        // cold
        let t2 = Instant::now();
        for _ in 0..COLD_ITERS {
            let mut s: Store<()> = Store::new(&engine, ());
            let inst = linker.instantiate(&mut s, &module).unwrap();
            let mem  = inst.get_memory(&mut s, "memory").unwrap();
            let ex   = inst.get_typed_func::<(i32, i32, i32), i64>(&mut s, "extract").unwrap();
            mem.write(&mut s, 0, payload).unwrap();
            black_box(ex.call(&mut s, (0, payload.len() as i32, 0)).unwrap());
        }
        let cold = t2.elapsed();

        (hot, cold)
    }
}

// ── silverfir-nano ────────────────────────────────────────────────────────────
// sf-nano-core doesn't expose a stable public Rust embedding API yet —
// the project is primarily a CLI tool + WASI runtime. This module is a
// placeholder that will compile once sf-nano-core stabilises its lib API.
// For now it emits a clear "not yet available" row in the results table
// rather than failing the build.

#[cfg(feature = "runtime-silverfir")]
mod runtime_silverfir {
    use super::*;

    pub fn bench(_payload: &[u8]) -> Option<(Duration, Duration)> {
        // TODO: wire up sf-nano-core embedding API once it's stable.
        // Silverfir-nano is primarily a CLI/WASI runtime right now;
        // the embedding API (sf-nano-core) exists but isn't documented
        // as a stable library surface. Track:
        //   https://github.com/mbbill/Silverfir-nano/tree/main/sf-nano-core
        None
    }
}

// ── main ─────────────────────────────────────────────────────────────────────

fn main() {
    let lib_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "../target/release/libnative_plugin.so".into());

    let payload = make_payload();

    println!();
    println!("┌─────────────────────────────────────────────────────────────────────────────────────┐");
    println!("│  BA-FB WASM Runtime Benchmark                                                       │");
    println!("│  Payload : 10×i64 + 5×f32 field reads → packed result  (FlatBuffer sim)            │");
    println!("│  Hot iters : {ITERATIONS:<10}  Cold iters: {COLD_ITERS:<10}  Warmup: {WARMUP:<10}              │");
    println!("├──────────────────────────────┬───────────┬──────────┬────────┬──────────────────────┤");
    println!("│  Runtime                     │  hot/call │  ops/sec │  vs①  │  cold/call           │");
    println!("├──────────────────────────────┼───────────┼──────────┼────────┼──────────────────────┤");

    // ① native baseline
    let native_d = bench_native(&payload);
    println!(
        "│  ① Native inline (baseline)  │ {} │ {} │    1.0x │  N/A                 │",
        fmt_ns(native_d, ITERATIONS),
        fmt_ops(native_d, ITERATIONS),
    );

    // ② C ABI cdylib
    #[cfg(feature = "runtime-cabi")]
    {
        match cabi::bench(&payload, &lib_path) {
            Some((hot, cold)) => println!(
                "│  ② C ABI cdylib              │ {} │ {} │ {} │  {}     │",
                fmt_ns(hot, ITERATIONS),
                fmt_ops(hot, ITERATIONS),
                overhead(native_d, hot, ITERATIONS),
                fmt_ns(cold, COLD_ITERS),
            ),
            None => println!("│  ② C ABI cdylib              │  [.so not found — pass path as argv[1]]                        │"),
        }
    }
    #[cfg(not(feature = "runtime-cabi"))]
    println!("│  ② C ABI cdylib              │  [feature disabled]                                            │");

    // ③ wasmi
    #[cfg(feature = "runtime-wasmi")]
    {
        let (hot, cold) = runtime_wasmi::bench(&payload);
        println!(
            "│  ③ wasmi 1.1                 │ {} │ {} │ {} │  {}     │",
            fmt_ns(hot, ITERATIONS),
            fmt_ops(hot, ITERATIONS),
            overhead(native_d, hot, ITERATIONS),
            fmt_ns(cold, COLD_ITERS),
        );
    }
    #[cfg(not(feature = "runtime-wasmi"))]
    println!("│  ③ wasmi 1.1                 │  [feature disabled]                                            │");

    // ④ tinywasm
    #[cfg(feature = "runtime-tinywasm")]
    {
        let (hot, cold) = runtime_tinywasm::bench(&payload);
        println!(
            "│  ④ tinywasm 0.9              │ {} │ {} │ {} │  {}     │",
            fmt_ns(hot, ITERATIONS),
            fmt_ops(hot, ITERATIONS),
            overhead(native_d, hot, ITERATIONS),
            fmt_ns(cold, COLD_ITERS),
        );
    }
    #[cfg(not(feature = "runtime-tinywasm"))]
    println!("│  ④ tinywasm 0.9              │  [feature disabled]                                            │");

    // ⑤ stitch
    #[cfg(feature = "runtime-stitch")]
    {
        let (hot, cold) = runtime_stitch::bench(&payload);
        println!(
            "│  ⑤ stitch (makepad)          │ {} │ {} │ {} │  {}     │",
            fmt_ns(hot, ITERATIONS),
            fmt_ops(hot, ITERATIONS),
            overhead(native_d, hot, ITERATIONS),
            fmt_ns(cold, COLD_ITERS),
        );
    }
    #[cfg(not(feature = "runtime-stitch"))]
    println!("│  ⑤ stitch (makepad)          │  [feature disabled]                                            │");

    // ⑥ wasm3
    #[cfg(feature = "runtime-wasm3")]
    {
        let (hot, cold) = runtime_wasm3::bench(&payload);
        println!(
            "│  ⑥ wasm3                     │ {} │ {} │ {} │  {}     │",
            fmt_ns(hot, ITERATIONS),
            fmt_ops(hot, ITERATIONS),
            overhead(native_d, hot, ITERATIONS),
            fmt_ns(cold, COLD_ITERS),
        );
    }
    #[cfg(not(feature = "runtime-wasm3"))]
    println!("│  ⑥ wasm3                     │  [feature disabled]                                            │");

    // ⑦ wasmtime
    #[cfg(feature = "runtime-wasmtime")]
    {
        let (hot, cold) = runtime_wasmtime::bench(&payload);
        println!(
            "│  ⑦ wasmtime 46 (Cranelift)   │ {} │ {} │ {} │  {}     │",
            fmt_ns(hot, ITERATIONS),
            fmt_ops(hot, ITERATIONS),
            overhead(native_d, hot, ITERATIONS),
            fmt_ns(cold, COLD_ITERS),
        );
    }
    #[cfg(not(feature = "runtime-wasmtime"))]
    println!("│  ⑦ wasmtime 46 (Cranelift)   │  [feature disabled]                                            │");

    // ⑧ silverfir-nano
    #[cfg(feature = "runtime-silverfir")]
    {
        match runtime_silverfir::bench(&payload) {
            Some((hot, cold)) => println!(
                "│  ⑧ silverfir-nano            │ {} │ {} │ {} │  {}     │",
                fmt_ns(hot, ITERATIONS),
                fmt_ops(hot, ITERATIONS),
                overhead(native_d, hot, ITERATIONS),
                fmt_ns(cold, COLD_ITERS),
            ),
            None => println!("│  ⑧ silverfir-nano            │  [embedding API not yet stable — CLI only]                     │"),
        }
    }
    #[cfg(not(feature = "runtime-silverfir"))]
    println!("│  ⑧ silverfir-nano            │  [feature disabled]                                            │");

    println!("└──────────────────────────────┴───────────┴──────────┴────────┴──────────────────────┘");
    println!();
    println!("  hot/call  = wall time per extract() call, warm instance reused");
    println!("  cold/call = wall time including fresh instantiation per call");
    println!("  vs①      = overhead multiplier relative to native inline");
    println!();
}
