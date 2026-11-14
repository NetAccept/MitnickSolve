(module
  (memory (export "memory") 16)
  (func (export "run") (param i32 i32) (result i64)
    (local $n i32)
    (local $i i32)
    (local $sum i64)
    (local.set $n (i32.load (i32.const 0)))
    (block $done
      (loop $l
        (br_if $done (i32.ge_u (local.get $i) (local.get $n)))
        (local.set $sum
          (i64.add (local.get $sum) (i64.extend_i32_u (local.get $i))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $l)))
    (i64.store (i32.const 4096) (local.get $sum))
    (i64.or
      (i64.shl (i64.const 4096) (i64.const 32))
      (i64.const 8))))
