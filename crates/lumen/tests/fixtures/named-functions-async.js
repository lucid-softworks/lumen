let asyncDone = false, asyncError = '';
const marker = {};
const compiled_async = async function finish(value) {
    const again = await Promise.resolve(value);
    return () => again;
};
const fallback_async = async function asyncSelf() {
    await Promise.resolve();
    return asyncSelf;
};
async function driveAsync() {
    for(let call=0;call<30;call++) {
        const read = await compiled_async(marker);
        if(read() !== marker) throw new Error('async capture');
        if(await fallback_async() !== fallback_async) throw new Error('async self binding');
    }
    asyncDone = true;
}
driveAsync().catch(error => { asyncError = String(error); });
