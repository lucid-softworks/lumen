# JIT scalar condition lowering

ARM64 now consumes straight-line numeric results for `JumpIfFalse` in generated code. The
existing compact Bool path is unchanged. Number truthiness handles signed zero and NaN directly;
BigInt and every refcounted value retain the canonical helper, including HTMLDDA handling and
operand destruction.

The larger template is emitted only when an untargeted condition immediately follows an
operation that produces a Number or BigInt. `Add` is deliberately excluded because it may
produce a string. Branch targets and merged control flow retain the generic checked path.

## Evidence

The focused fixture performs 100 million calls and verifies a checksum. Five fresh-process,
rotated pairs on the final rebased tree improved from a 1,976 ms disabled median to 1,890 ms
enabled, a 4.4% reduction; all five pairs improved. A Bool-producing control, whose emitted hot
path is unchanged, was flat at 2,147 vs 2,144 ms across three rotated pairs.

Matched five-second samples used 400 million calls. The disabled sample had 4,292 main-thread
samples, including 167 exclusive `jit_cond` and 170 generic `Value` drop-glue samples. The
enabled sample had 4,290 samples; `jit_cond` fell below `sample`'s five-hit report threshold and
drop glue had 126 samples. Instrumented elapsed times are diagnostic, not benchmark evidence.

Five rotated complete V8-v7 pairs on the final rebased tree were flat within host variation:
8,818 disabled vs 8,817 enabled (-0.01%), with mixed directions. Three selected Octane pairs
were also flat: 14,935 disabled vs 14,931 enabled (-0.03%), with mixed directions. Splay's noisy
movement cannot come from this feature: its bytecode has no eligible producer/branch pair and
its enabled and disabled JIT allocation layouts are identical.

[Raw samples, profile counts, commands, hashes, and validation](benchmarks/jit-scalar-conditions-2026-09-24.json)

## Correctness boundary

The fast path performs no coercion and cannot execute JavaScript. It moves the operand-stack
pointer only after reading the scalar payload. Refcounted values and BigInt go through the
existing helper, so destruction, HTMLDDA, exception state, bailout PCs, GC/interrupt behavior,
and live-state reconstruction are unchanged. The x86-64 backend retains its existing helper
lowering.
