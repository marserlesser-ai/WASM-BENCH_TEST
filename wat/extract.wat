;; Benchmark payload — mirrors what a flatc-generated extract fn does:
;;   • Read 10 × i64  from offsets  0, 8, 16 … 72   (field reads via vtable)
;;   • Read  5 × f32  from offsets 80, 84, 88, 92, 96
;;   • Write 16 bytes of packed result to output region
;;   • Return packed (out_ptr | out_len<<32) as i64
;;
;; Memory layout (2 pages = 128 KB):
;;   [0  ..  99]  — input  (written by host before calling extract)
;;   [4096 .. 4111] — output (i64 sum_i + f64 sum_f)
;;
;; Exported API matches the plugin/src/lib.rs contract exactly so the
;; host harness code is identical for every runtime.

(module
  (memory (export "memory") 2)

  ;; alloc/dealloc stubs — in the WAT benchmark we use fixed regions,
  ;; so these are no-ops; they exist so the host harness code doesn't
  ;; need a special case for WAT vs real .wasm.
  (func (export "alloc_buffer")   (param i32) (result i32) i32.const 0)
  (func (export "dealloc_buffer") (param i32) (param i32))
  (func (export "table_count")    (result i32) i32.const 1)

  ;; extract(in_ptr, in_len, type_id) -> i64
  (func (export "extract")
    (param $in_ptr i32)
    (param $in_len i32)
    (param $type_id i32)
    (result i64)

    (local $sum_i i64)
    (local $sum_f f64)
    (local $i     i32)
    (local $out   i32)

    (local.set $out   (i32.const 4096))
    (local.set $sum_i (i64.const 0))
    (local.set $sum_f (f64.const 0.0))

    ;; ── sum 10 × i64 ─────────────────────────────────────────────────
    (local.set $i (i32.const 0))
    (block $brk
      (loop $lp
        (br_if $brk (i32.ge_u (local.get $i) (i32.const 10)))
        (local.set $sum_i
          (i64.add (local.get $sum_i)
            (i64.load
              (i32.add (local.get $in_ptr)
                       (i32.mul (local.get $i) (i32.const 8))))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $lp)
      )
    )

    ;; ── sum 5 × f32 → f64 ────────────────────────────────────────────
    (local.set $i (i32.const 0))
    (block $brk2
      (loop $lp2
        (br_if $brk2 (i32.ge_u (local.get $i) (i32.const 5)))
        (local.set $sum_f
          (f64.add (local.get $sum_f)
            (f64.promote_f32
              (f32.load
                (i32.add (local.get $in_ptr)
                         (i32.add (i32.const 80)
                                  (i32.mul (local.get $i) (i32.const 4))))))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $lp2)
      )
    )

    ;; ── write results ─────────────────────────────────────────────────
    (i64.store (local.get $out)
               (local.get $sum_i))
    (f64.store (i32.add (local.get $out) (i32.const 8))
               (local.get $sum_f))

    ;; ── return packed (out_ptr=4096 | out_len=16 << 32) ──────────────
    (i64.or
      (i64.extend_i32_u (local.get $out))
      (i64.shl (i64.const 16) (i64.const 32))
    )
  )
)
