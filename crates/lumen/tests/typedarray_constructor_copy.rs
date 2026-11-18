use lumen::{bytecode::Tier, Completion, Engine};

#[test]
fn typedarray_constructor_copies_internal_elements() {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        let result = engine.eval(r#"
            function assert(v) { if (!v) throw new Error('typedarray constructor'); }
            function throws(f) {
                let caught=false;
                try { f(); } catch(e) { caught=e instanceof TypeError; }
                assert(caught);
            }
            const source=new Uint8Array([1,2,3,255]);
            Object.defineProperty(source, Symbol.iterator, {get(){throw new Error('iterator read');}});
            Object.defineProperty(source, 'length', {get(){throw new Error('length read');}});
            const copy=new Uint8Array(source);
            assert(copy.join()==='1,2,3,255' && copy.buffer!==source.buffer);
            copy[0]=99; assert(source[0]===1);
            class Bytes extends Uint8Array {}
            const child=new Bytes(source);
            assert(child instanceof Bytes && child.join()==='1,2,3,255');
            const bits=new Uint32Array([0x7fc12345,0xffc54321]);
            const floats=new Float32Array(bits.buffer);
            const cloned=new Float32Array(floats);
            const result=new Uint32Array(cloned.buffer);
            assert(result[0]===bits[0] && result[1]===bits[1]);
            assert(new Uint16Array(new Float64Array([1.9,65537,-1])).join()==='1,1,65535');
            assert(new BigUint64Array(new BigInt64Array([-1n,2n]))[0]===18446744073709551615n);
            throws(()=>new Uint8Array(new BigInt64Array(0)));
            throws(()=>new BigInt64Array(new Uint8Array(0)));
            const shared=new Uint8Array(new SharedArrayBuffer(3)); shared.set([7,8,9]);
            const sharedCopy=new Uint8Array(shared);
            assert(sharedCopy.join()==='7,8,9' && sharedCopy.buffer instanceof ArrayBuffer);
            const rab=new ArrayBuffer(8,{maxByteLength:16});
            const tracking=new Uint8Array(rab); tracking.set([3,4]); rab.resize(12);
            assert(new Uint8Array(tracking).length===12);
            assert(new Uint8Array(tracking)[0]===3);
            const fixed=new Uint8Array(rab,4,8); rab.resize(4);
            throws(()=>new Uint8Array(fixed));
            const detached=new Uint8Array(new ArrayBuffer(2)); detached.buffer.transfer();
            throws(()=>new Uint8Array(detached));
            const iterable={*[Symbol.iterator](){yield 7;yield 9;}};
            assert(new Uint8Array(iterable).join()==='7,9');
            'passed'
        "#, false).unwrap();
        match result {
            Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
            Completion::Throw {name,message} => panic!("{tier:?}: {name}: {message}"),
        }
    }
}
