let asyncDone = false, asyncError = '';
async function later(head = 10, ...tail) {
    const read = () => tail;
    await Promise.resolve(0);
    tail.push(head);
    await Promise.resolve(0);
    return [tail, read];
}
(async () => {
    const marker = {id: 42};
    for (let call = 0; call < 100; call++) {
        const empty = await later();
        const values = await later(9, marker, 2);
        if (empty[0].length !== 1 || empty[0][0] !== 10 || empty[0] !== empty[1]()) throw new Error('empty async rest');
        if (values[0].length !== 3 || values[0][0] !== marker || values[0][2] !== 9 || values[0] !== values[1]()) throw new Error('captured async rest');
        if (empty[0] === values[0]) throw new Error('async rest alias');
    }
    asyncDone = true;
})().catch(error => { asyncError = String(error); });
