var logicalAsyncDone = false, logicalAsyncError = '';
async function compiled_async_object(key, promise) { return {[key]: await promise}; }
async function checkComputedAsync() {
    for (let round = 0; round < 24; round++) {
        const symbol = Symbol('key');
        let calls = 0;
        const key = {[Symbol.toPrimitive]() { calls++; return symbol; }};
        let resolve;
        const promise = new Promise(r => { resolve = r; });
        const result = compiled_async_object(key, promise);
        if (calls !== 1) throw new Error('key must precede await');
        key[Symbol.toPrimitive] = () => { throw new Error('key coerced twice'); };
        resolve(19);
        const object = await result;
        if (calls !== 1 || object[symbol] !== 19) throw new Error('retained symbol key');
        let rejected = false;
        try { await compiled_async_object('key', Promise.reject('sentinel')); }
        catch (error) { rejected = error === 'sentinel'; }
        if (!rejected) throw new Error('await rejection');
    }
    logicalAsyncDone = true;
}
checkComputedAsync().catch(error => { logicalAsyncError = String(error); });
