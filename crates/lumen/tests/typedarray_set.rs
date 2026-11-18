use lumen::{bytecode::Tier, Completion, Engine};

#[test]
fn typedarray_set_preserves_bytes_overlap_and_validation() {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        let result = engine
            .eval(
                r#"
            function assert(v) { if (!v) throw new Error('typedarray set mismatch'); }
            function throws(f, type) {
                let caught=false;
                try { f(); } catch(e) { caught=e instanceof type; }
                assert(caught);
            }
            let a=new Uint8Array([1,2,3,4,5]);
            a.set(a.subarray(0,4),1);
            assert(a.join()==='1,1,2,3,4');
            a.set(a.subarray(1),0);
            assert(a.join()==='1,2,3,4,4');
            let words=new Uint32Array([0x7fc12345,0xffc54321]);
            let floats=new Float32Array(words.buffer);
            let copied=new Float32Array(4);
            copied.set(floats,1);
            let bits=new Uint32Array(copied.buffer);
            assert(bits[0]===0 && bits[1]===words[0] && bits[2]===words[1] && bits[3]===0);
            let converted=new Uint16Array(2);
            converted.set(new Float32Array([1.9,65537]));
            assert(converted.join()==='1,1');
            let big=new BigInt64Array(2);
            big.set(new BigInt64Array([-1n,2n]));
            assert(big[0]===-1n && big[1]===2n);
            throws(()=>a.set(new Uint8Array(1),5),RangeError);
            throws(()=>a.set(new Uint8Array(0),Infinity),RangeError);
            throws(()=>a.set(new BigInt64Array(0)),TypeError);
            let buffer=new ArrayBuffer(8,{maxByteLength:16});
            let tracking=new Uint8Array(buffer);
            tracking.set(new Uint8Array([7,8]));
            buffer.resize(12);
            let target=new Uint8Array(12);
            target.set(tracking);
            assert(target[0]===7 && target[1]===8 && target[11]===0);
            let fixed=new Uint8Array(buffer,4,8);
            buffer.resize(4);
            throws(()=>target.set(fixed),TypeError);
            let detached=new Uint8Array(new ArrayBuffer(4));
            detached.buffer.transfer();
            throws(()=>target.set(detached),TypeError);
            let shared=new Uint8Array(new SharedArrayBuffer(4));
            shared.set(new Uint8Array([1,2,3,4]));
            shared.set(shared.subarray(0,3),1);
            assert(shared.join()==='1,1,2,3');
            target.set(shared);
            assert(target.slice(0,4).join()==='1,1,2,3');
            'passed'
        "#,
                false,
            )
            .unwrap();
        match result {
            Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
            Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
        }
    }
}
