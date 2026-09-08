# Numeric regions inside shared-closure inlines

The shared-closure implementation conservatively cleared JIT fast bit 15 for an
entire chunk with dynamic inline owners. That blocked numeric loops and ordinary
instruction templates, even when they never touched the hidden callee. It also
blocked all numeric field expressions. This follow-up narrows those restrictions.

`jit/numeric_loops.rs` owns selection between the existing numeric CFG and linear
loop emitters. Both planners reject calls, InlineGuard and slot resets. Supported
numeric writes prove that overwritten values are drop-free; receiver objects and
unmentioned locals retain their canonical owners. Thus a numeric loop can run
inside an active inline without moving or releasing its callee. Guard failures
and bounded backedges materialize numeric state before resuming ordinary bytecode
positions. Observable helpers then record the live inline frame as before.

Numeric field expressions are also restored. Their emitter borrows object roots
and publishes only the original terminal instruction's operands after all guards
succeed. It never consumes the hidden callee slot. The existing terminal remains
responsible for writes, returns and their observable behavior.

Complex regions that can consume call setup or perform speculative writes remain
restricted. Ordinary instruction fast paths use the user's original fast mask;
only complex region selection receives the restricted mask. The overall closure
feature remains opt-in through `LUMEN_JIT_INLINE_CLOSURES=1`.

Colocated tests exercise fresh inlined linear and branching loops, actual native
execution, 2048-iteration continuations, out-of-bounds side exits to prototype
getters, GC while a dynamic inline frame is active, exceptions and subsequent
calls. The side-exit getter verifies that a native loop was entered before it ran.
A separate numeric-expression test covers fresh closures and getter/throw fallback.
