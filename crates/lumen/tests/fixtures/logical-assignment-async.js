var logicalAsyncDone = false, logicalAsyncError = '';
async function compiled_async_property(base, key, promise) { return base[key] ??= await promise; }
async function compiled_async_local(value, promise) { return value ||= await promise; }
async function runLogicalAsync() {
    for (let round = 0; round < 24; round++) {
        const events = [];
        let stored;
        const original = {
            get value() { events.push('get'); return stored; },
            set value(v) { events.push('set'); stored = v; }
        };
        const key = {[Symbol.toPrimitive]() { events.push('key'); return 'value'; }};
        let resolve;
        const pending = new Promise(r => { resolve = r; });
        const result = compiled_async_property(original, key, pending);
        if (events.join(',') !== 'key,get') throw new Error('initial read order');
        key[Symbol.toPrimitive] = () => { throw new Error('key converted twice'); };
        resolve(17);
        if (await result !== 17 || stored !== 17 || events.join(',') !== 'key,get,set') throw new Error('suspended reference');
        if (await compiled_async_property(original, 'value', new Promise(() => {})) !== 17) throw new Error('skipped await');
        if (await compiled_async_local(0, Promise.resolve(19)) !== 19) throw new Error('local awaited value');
        let rejected = false;
        try { await compiled_async_local(0, Promise.reject('sentinel')); }
        catch (error) { rejected = error === 'sentinel'; }
        if (!rejected) throw new Error('await rejection');
    }
    logicalAsyncDone = true;
}
runLogicalAsync().catch(error => { logicalAsyncError = String(error); });
