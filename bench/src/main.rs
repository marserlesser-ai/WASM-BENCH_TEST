//! BA-FB WASM Runtime Benchmark
//!
//! Measures overhead of each WASM runtime vs native for the exact workload
//! BA-FB does: receive a byte buffer → decode FlatBuffer fields → emit JSON.
//!
//! Build a single runtime:
//!   cargo build --release -p bench --no-default-features --features runtime-wasmtime

#![allow(unused_variables, unused_mut, dead_code)]

use std::{hint::black_box, time::{Duration, Instant}};

const ITERATIONS: u64 = 500_000;
const WARMUP:     u64 =  10_000;
const COLD_ITERS: u64 =     500;

const WAT_SRC: &str = include_str!("../../wat/extract.wat");

// ── payload: simulates a FlatBuffer byte buffer ───────────────────────────────

fn make_payload() -> Vec<u8> {
    let mut buf = vec![0u8; 100];
    for i in 0u64..10 {
        let val: i64 = (i as i64 + 1) * 1_000;
        buf[i as usize * 8..i as usize * 8 + 8].copy_from_slice(&val.to_le_bytes());
    }
    for i in 0u32..5 {
        let val: f32 = (i as f32 + 1.0) * 1.5;
        let off = 80 + i as usize * 4;
        buf[off..off + 4].copy_from_slice(&val.to_le_bytes());
    }
    buf
}

// ── formatting ────────────────────────────────────────────────────────────────

fn fmt_ns(d: Duration, iters: u64) -> String {
    let ns = d.as_nanos() as f64 / iters as f64;
    if ns < 1_000.0          { format!("{ns:>8.1} ns") }
    else if ns < 1_000_000.0 { format!("{:>8.2} µs", ns / 1_000.0) }
    else                     { format!("{:>8.2} ms", ns / 1_000_000.0) }
}

fn fmt_ops(d: Duration, iters: u64) -> String {
    let ops = iters as f64 / d.as_secs_f64();
    if ops >= 1_000_000.0 { format!("{:>7.2}M/s", ops / 1_000_000.0) }
    else if ops >= 1_000.0 { format!("{:>7.1}K/s", ops / 1_000.0) }
    else                   { format!("{ops:>7.0}/s") }
}

fn overhead(base: Duration, other: Duration, iters: u64) -> String {
    let b = base.as_nanos()  as f64 / iters as f64;
    let o = other.as_nanos() as f64 / iters as f64;
    format!("{:>5.1}x", o / b)
}

fn row(label: &str, hot: Duration, cold: Option<Duration>, base: Duration) {
    let cold_s = cold.map_or("        N/A".into(), |c| fmt_ns(c, COLD_ITERS));
    println!(
        "  {label:<28}  {}  {}  {}  cold: {}",
        fmt_ns(hot, ITERATIONS),
        fmt_ops(hot, ITERATIONS),
        overhead(base, hot, ITERATIONS),
        cold_s,
    );
}

// ── 1. native inline baseline ─────────────────────────────────────────────────

#[inline(never)]
fn native_extract(buf: &[u8]) -> (i64, f64) {
    let mut si: i64 = 0;
    let mut sf: f64 = 0.0;
    for i in 0..10usize {
        si = si.wrapping_add(i64::from_le_bytes(buf[i*8..i*8+8].try_into().unwrap()));
    }
    for i in 0..5usize {
        sf += f32::from_le_bytes(buf[80+i*4..84+i*4].try_into().unwrap()) as f64;
    }
    (si, sf)
}

fn bench_native(p: &[u8]) -> Duration {
    for _ in 0..WARMUP      { black_box(native_extract(p)); }
    let t = Instant::now();
    for _ in 0..ITERATIONS  { black_box(native_extract(p)); }
    t.elapsed()
}

// ── 2. C ABI cdylib via libloading ───────────────────────────────────────────

#[cfg(feature = "runtime-cabi")]
mod runtime_cabi {
    use super::*;
    use libloading::{Library, Symbol};

    type ExtractFn = unsafe extern "C" fn(*const u8, usize, u32) -> u64;

    pub fn bench(payload: &[u8], lib_path: &str) -> Option<(Duration, Duration)> {
        if !std::path::Path::new(lib_path).exists() {
            eprintln!("    [C ABI] library not found at {lib_path}");
            return None;
        }
        let lib = unsafe { Library::new(lib_path) }.ok()?;
        let extract: Symbol<ExtractFn> = unsafe { lib.get(b"extract") }.ok()?;

        for _ in 0..WARMUP {
            black_box(unsafe { extract(payload.as_ptr(), payload.len(), 0) });
        }
        let t = Instant::now();
        for _ in 0..ITERATIONS {
            black_box(unsafe { extract(payload.as_ptr(), payload.len(), 0) });
        }
        let hot = t.elapsed();

        // cold = Library::new + symbol lookup + one call
        let t2 = Instant::now();
        for _ in 0..COLD_ITERS {
            let l  = unsafe { Library::new(lib_path) }.unwrap();
            let ex: Symbol<ExtractFn> = unsafe { l.get(b"extract") }.unwrap();
            black_box(unsafe { ex(payload.as_ptr(), payload.len(), 0) });
        }
        Some((hot, t2.elapsed()))
    }
}

// ── 3. wasmi 1.1 ─────────────────────────────────────────────────────────────
// wasmi 1.x: Linker::instantiate_and_start (not .instantiate().start())
// wasmi has built-in WAT parsing via the `wat` feature — pass WAT string directly

#[cfg(feature = "runtime-wasmi")]
mod runtime_wasmi {
    use super::*;
    use wasmi::{Engine, Linker, Module, Store};

    pub fn bench(payload: &[u8]) -> (Duration, Duration) {
        let engine   = Engine::default();
        // wasmi with `wat` feature accepts WAT text directly in Module::new
        let module   = Module::new(&engine, WAT_SRC).expect("wasmi: module");
        let linker   = Linker::<()>::new(&engine);
        let mut store: Store<()> = Store::new(&engine, ());

        let instance = linker
            .instantiate_and_start(&mut store, &module)
            .expect("wasmi: instantiate");

        let memory  = instance.get_memory(&store, "memory").expect("wasmi: memory");
        let extract = instance
            .get_typed_func::<(i32, i32, i32), i64>(&store, "extract")
            .expect("wasmi: extract fn");

        memory.write(&mut store, 0, payload).expect("wasmi: write");

        for _ in 0..WARMUP {
            black_box(extract.call(&mut store, (0, payload.len() as i32, 0)).unwrap());
        }
        let t = Instant::now();
        for _ in 0..ITERATIONS {
            black_box(extract.call(&mut store, (0, payload.len() as i32, 0)).unwrap());
        }
        let hot = t.elapsed();

        let t2 = Instant::now();
        for _ in 0..COLD_ITERS {
            let mut s: Store<()> = Store::new(&engine, ());
            let inst = linker.instantiate_and_start(&mut s, &module).unwrap();
            let mem  = inst.get_memory(&s, "memory").unwrap();
            let ex   = inst.get_typed_func::<(i32, i32, i32), i64>(&s, "extract").unwrap();
            mem.write(&mut s, 0, payload).unwrap();
            black_box(ex.call(&mut s, (0, payload.len() as i32, 0)).unwrap());
        }
        (hot, t2.elapsed())
    }
}

// ── 4. tinywasm 0.9 ──────────────────────────────────────────────────────────
// API: tinywasm::parse_bytes → Module
//      ModuleInstance::instantiate(&mut store, &module, None)
//      instance.exported_memory(&store, "memory")  → MemoryRef
//      instance.func::<(i32,i32,i32), i64>(&mut store, "extract") → TypedFunc
//      func.call(&mut store, args)

#[cfg(feature = "runtime-tinywasm")]
mod runtime_tinywasm {
    use super::*;
    use tinywasm::{ModuleInstance, Store};

    pub fn bench(payload: &[u8]) -> (Duration, Duration) {
        // tinywasm::parse_bytes expects raw wasm bytes; use `wat` crate to convert
        let wasm   = wat::parse_str(WAT_SRC).expect("tinywasm: WAT parse");
        let module = tinywasm::parse_bytes(&wasm).expect("tinywasm: parse_bytes");

        let mut store = Store::default();
        let instance  = ModuleInstance::instantiate(&mut store, &module, None)
            .expect("tinywasm: instantiate");

        // Memory API: copy_from_slice(&mut store, offset, &[u8])
        let memory  = instance.exported_memory(&mut store, "memory")
            .expect("tinywasm: memory");
        memory.copy_from_slice(&mut store, 0, payload)
            .expect("tinywasm: write");

        // func::<Params, Results>(&mut store, name) -> FunctionTyped
        let extract = instance
            .func::<(i32, i32, i32), i64>(&mut store, "extract")
            .expect("tinywasm: extract fn");

        for _ in 0..WARMUP {
            black_box(extract.call(&mut store, (0, payload.len() as i32, 0)).unwrap());
        }
        let t = Instant::now();
        for _ in 0..ITERATIONS {
            black_box(extract.call(&mut store, (0, payload.len() as i32, 0)).unwrap());
        }
        let hot = t.elapsed();

        let t2 = Instant::now();
        for _ in 0..COLD_ITERS {
            let mut s  = Store::default();
            let inst   = ModuleInstance::instantiate(&mut s, &module, None).unwrap();
            let mem    = inst.exported_memory(&mut s, "memory").unwrap();
            mem.copy_from_slice(&mut s, 0, payload).unwrap();
            let ex     = inst.func::<(i32, i32, i32), i64>(&mut s, "extract").unwrap();
            black_box(ex.call(&mut s, (0, payload.len() as i32, 0)).unwrap());
        }
        (hot, t2.elapsed())
    }
}

// ── 5. stitch ─────────────────────────────────────────────────────────────────

#[cfg(feature = "runtime-stitch")]
mod runtime_stitch {
    use super::*;
    use makepad_stitch::{Engine, Linker, Module, Store, Val};
    use wast::{parser, parser::ParseBuffer, Wat};

    fn to_wasm(src: &str) -> Vec<u8> {
        let buf = ParseBuffer::new(src).expect("stitch: ParseBuffer");
        parser::parse::<Wat>(&buf).expect("stitch: parse WAT")
            .encode().expect("stitch: encode")
    }

    pub fn bench(payload: &[u8]) -> (Duration, Duration) {
        let wasm      = to_wasm(WAT_SRC);
        let engine    = Engine::new();
        let mut store = Store::new(engine);
        let module    = Module::new(store.engine(), &wasm).expect("stitch: module");
        let instance  = Linker::new()
            .instantiate(&mut store, &module)
            .expect("stitch: instantiate");

        let memory  = instance.exported_mem("memory").expect("stitch: memory");
        let extract = instance.exported_func("extract").expect("stitch: fn");

        { let b = memory.bytes_mut(&mut store); b[..payload.len()].copy_from_slice(payload); }

        let args    = [Val::I32(0), Val::I32(payload.len() as i32), Val::I32(0)];
        let mut res = [Val::I64(0)];

        for _ in 0..WARMUP {
            extract.call(&mut store, &args, &mut res).unwrap();
            black_box(res[0].clone());
        }
        let t = Instant::now();
        for _ in 0..ITERATIONS {
            extract.call(&mut store, &args, &mut res).unwrap();
            black_box(res[0].clone());
        }
        let hot = t.elapsed();

        let t2 = Instant::now();
        for _ in 0..COLD_ITERS {
            let eng   = Engine::new();
            let mut s = Store::new(eng);
            let m     = Module::new(s.engine(), &wasm).unwrap();
            let inst  = Linker::new().instantiate(&mut s, &m).unwrap();
            let mem   = inst.exported_mem("memory").unwrap();
            let ex    = inst.exported_func("extract").unwrap();
            { let b = mem.bytes_mut(&mut s); b[..payload.len()].copy_from_slice(payload); }
            let mut r = [Val::I64(0)];
            ex.call(&mut s, &args, &mut r).unwrap();
            black_box(r[0].clone());
        }
        (hot, t2.elapsed())
    }
}

// ── 6. wasm3 ─────────────────────────────────────────────────────────────────

#[cfg(feature = "runtime-wasm3")]
mod runtime_wasm3 {
    use super::*;
    use wasm3::{Environment, Module};

    pub fn bench(payload: &[u8]) -> (Duration, Duration) {
        let wasm = wat::parse_str(WAT_SRC).expect("wasm3: WAT parse");
        let env  = Environment::new().expect("wasm3: env");
        let rt   = env.create_runtime(1024).expect("wasm3: runtime");
        let module = Module::parse(&env, &wasm).expect("wasm3: parse");
        let mut module = rt.load_module(module).expect("wasm3: load");

        let extract = module
            .find_function::<(i32, i32, i32), i64>("extract")
            .expect("wasm3: extract fn");

        // wasm3 memory is exposed as &mut [u8] via runtime.memory()
        // We write the payload in before calling; wasm3 reads it via in_ptr arg
        rt.memory()[..payload.len()].copy_from_slice(payload);

        for _ in 0..WARMUP {
            black_box(extract.call(0, payload.len() as i32, 0).unwrap());
        }
        let t = Instant::now();
        for _ in 0..ITERATIONS {
            black_box(extract.call(0, payload.len() as i32, 0).unwrap());
        }
        let hot = t.elapsed();

        let t2 = Instant::now();
        for _ in 0..COLD_ITERS {
            let e2     = Environment::new().unwrap();
            let rt2    = e2.create_runtime(1024).unwrap();
            let m2     = Module::parse(&e2, &wasm).unwrap();
            let mut m2 = rt2.load_module(m2).unwrap();
            rt2.memory()[..payload.len()].copy_from_slice(payload);
            let ex = m2.find_function::<(i32, i32, i32), i64>("extract").unwrap();
            black_box(ex.call(0, payload.len() as i32, 0).unwrap());
        }
        (hot, t2.elapsed())
    }
}

// ── 7. wasmtime 46 ───────────────────────────────────────────────────────────

#[cfg(feature = "runtime-wasmtime")]
mod runtime_wasmtime {
    use super::*;
    use wasmtime::{Config, Engine, Linker, Module, OptLevel, Store, Strategy};

    pub fn bench(payload: &[u8]) -> (Duration, Duration) {
        let mut cfg = Config::new();
        cfg.cranelift_opt_level(OptLevel::Speed);
        cfg.strategy(Strategy::Cranelift);
        let engine = Engine::new(&cfg).expect("wasmtime: engine");
        // wasmtime parses WAT natively
        let module = Module::new(&engine, WAT_SRC).expect("wasmtime: module");
        let linker = Linker::<()>::new(&engine);
        let mut store: Store<()> = Store::new(&engine, ());

        let instance = linker
            .instantiate(&mut store, &module)
            .expect("wasmtime: instantiate");

        let memory  = instance.get_memory(&mut store, "memory").expect("wasmtime: memory");
        let extract = instance
            .get_typed_func::<(i32, i32, i32), i64>(&mut store, "extract")
            .expect("wasmtime: extract fn");

        memory.write(&mut store, 0, payload).expect("wasmtime: write");

        for _ in 0..WARMUP {
            black_box(extract.call(&mut store, (0, payload.len() as i32, 0)).unwrap());
        }
        let t = Instant::now();
        for _ in 0..ITERATIONS {
            black_box(extract.call(&mut store, (0, payload.len() as i32, 0)).unwrap());
        }
        let hot = t.elapsed();

        let t2 = Instant::now();
        for _ in 0..COLD_ITERS {
            let mut s: Store<()> = Store::new(&engine, ());
            let inst = linker.instantiate(&mut s, &module).unwrap();
            let mem  = inst.get_memory(&mut s, "memory").unwrap();
            let ex   = inst.get_typed_func::<(i32, i32, i32), i64>(&mut s, "extract").unwrap();
            mem.write(&mut s, 0, payload).unwrap();
            black_box(ex.call(&mut s, (0, payload.len() as i32, 0)).unwrap());
        }
        (hot, t2.elapsed())
    }
}

// ── 8. silverfir-nano ─────────────────────────────────────────────────────────

#[cfg(feature = "runtime-silverfir")]
mod runtime_silverfir {
    use super::*;

    pub fn bench(_payload: &[u8]) -> Option<(Duration, Duration)> {
        // sf-nano-core embedding API is not yet stable (CLI-only project).
        // This slot will light up once the library surface is documented.
        // Track: https://github.com/mbbill/Silverfir-nano/tree/main/sf-nano-core
        None
    }
}

// ── main ──────────────────────────────────────────────────────────────────────

fn main() {
    let lib_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "../target/release/libplugin.so".into());

    let payload = make_payload();

    println!();
    println!("┌──────────────────────────────────────────────────────────────────────────────────────────┐");
    println!("│  BA-FB WASM Runtime Benchmark                                                            │");
    println!("│  Payload: 10×i64 + 5×f32 field reads → packed result  (FlatBuffer sim)                  │");
    println!("│  Hot: {ITERATIONS}  Cold: {COLD_ITERS}  Warmup: {WARMUP}                                          │");
    println!("├──────────────────────┬───────────┬──────────┬────────┬──────────────┤");
    println!("│  Runtime             │  hot/call │  ops/sec │  vs①  │  cold/call   │");
    println!("├──────────────────────┼───────────┼──────────┼────────┼──────────────┤");

    let native_d = bench_native(&payload);
    println!("│  ① Native (baseline) │ {} │ {} │   1.0x │  N/A         │",
        fmt_ns(native_d, ITERATIONS), fmt_ops(native_d, ITERATIONS));

    #[cfg(feature = "runtime-cabi")]
    match runtime_cabi::bench(&payload, &lib_path) {
        Some((h, c)) => println!("│  ② C ABI cdylib      │ {} │ {} │ {} │  {}  │",
            fmt_ns(h, ITERATIONS), fmt_ops(h, ITERATIONS), overhead(native_d, h, ITERATIONS), fmt_ns(c, COLD_ITERS)),
        None => println!("│  ② C ABI cdylib      │  [.so not found — pass path as argv[1]]                       │"),
    }
    #[cfg(not(feature = "runtime-cabi"))]
    println!("│  ② C ABI cdylib      │  [feature disabled]                                                │");

    #[cfg(feature = "runtime-wasmi")]
    { let (h, c) = runtime_wasmi::bench(&payload);
      println!("│  ③ wasmi 1.1         │ {} │ {} │ {} │  {}  │",
        fmt_ns(h, ITERATIONS), fmt_ops(h, ITERATIONS), overhead(native_d, h, ITERATIONS), fmt_ns(c, COLD_ITERS)); }
    #[cfg(not(feature = "runtime-wasmi"))]
    println!("│  ③ wasmi 1.1         │  [feature disabled]                                                │");

    #[cfg(feature = "runtime-tinywasm")]
    { let (h, c) = runtime_tinywasm::bench(&payload);
      println!("│  ④ tinywasm 0.9      │ {} │ {} │ {} │  {}  │",
        fmt_ns(h, ITERATIONS), fmt_ops(h, ITERATIONS), overhead(native_d, h, ITERATIONS), fmt_ns(c, COLD_ITERS)); }
    #[cfg(not(feature = "runtime-tinywasm"))]
    println!("│  ④ tinywasm 0.9      │  [feature disabled]                                                │");

    #[cfg(feature = "runtime-stitch")]
    { let (h, c) = runtime_stitch::bench(&payload);
      println!("│  ⑤ stitch (makepad)  │ {} │ {} │ {} │  {}  │",
        fmt_ns(h, ITERATIONS), fmt_ops(h, ITERATIONS), overhead(native_d, h, ITERATIONS), fmt_ns(c, COLD_ITERS)); }
    #[cfg(not(feature = "runtime-stitch"))]
    println!("│  ⑤ stitch (makepad)  │  [feature disabled]                                                │");

    #[cfg(feature = "runtime-wasm3")]
    { let (h, c) = runtime_wasm3::bench(&payload);
      println!("│  ⑥ wasm3             │ {} │ {} │ {} │  {}  │",
        fmt_ns(h, ITERATIONS), fmt_ops(h, ITERATIONS), overhead(native_d, h, ITERATIONS), fmt_ns(c, COLD_ITERS)); }
    #[cfg(not(feature = "runtime-wasm3"))]
    println!("│  ⑥ wasm3             │  [feature disabled]                                                │");

    #[cfg(feature = "runtime-wasmtime")]
    { let (h, c) = runtime_wasmtime::bench(&payload);
      println!("│  ⑦ wasmtime 46       │ {} │ {} │ {} │  {}  │",
        fmt_ns(h, ITERATIONS), fmt_ops(h, ITERATIONS), overhead(native_d, h, ITERATIONS), fmt_ns(c, COLD_ITERS)); }
    #[cfg(not(feature = "runtime-wasmtime"))]
    println!("│  ⑦ wasmtime 46       │  [feature disabled]                                                │");

    #[cfg(feature = "runtime-silverfir")]
    match runtime_silverfir::bench(&payload) {
        Some((h, c)) => println!("│  ⑧ silverfir-nano    │ {} │ {} │ {} │  {}  │",
            fmt_ns(h, ITERATIONS), fmt_ops(h, ITERATIONS), overhead(native_d, h, ITERATIONS), fmt_ns(c, COLD_ITERS)),
        None => println!("│  ⑧ silverfir-nano    │  [embedding API not yet stable]                               │"),
    }
    #[cfg(not(feature = "runtime-silverfir"))]
    println!("│  ⑧ silverfir-nano    │  [feature disabled]                                                │");

    println!("└──────────────────────┴───────────┴──────────┴────────┴──────────────┘");
    println!();
    println!("  hot  = warm instance, call only");
    println!("  cold = includes fresh instantiation per call");
    println!("  vs①  = overhead multiplier vs native inline");
    println!();
}
