const fs = require('node:fs');
const path = require('node:path');
const os = require('node:os');
const crypto = require('node:crypto');
const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'lumen-pgo-io-'));
(async () => {
    try {
        const payload = Buffer.from('profile training\n'.repeat(128));
        let total = 0;
        for (let n = 0; n < 8; n++) {
            const file = path.join(directory, 'file-' + n);
            await fs.promises.writeFile(file, payload);
            const bytes = await fs.promises.readFile(file);
            total += bytes.length + fs.statSync(file).size;
            if (crypto.createHash('sha256').update(bytes).digest('hex') !== crypto.createHash('sha256').update(payload).digest('hex')) throw new Error('IO checksum');
        }
        const lengths = await Promise.all([1, 2, 3].map(async n => (await fs.promises.readFile(path.join(directory, 'file-' + n))).length));
        console.log('async-io', total + lengths.reduce((a, b) => a + b, 0));
    } finally {
        fs.rmSync(directory, {recursive: true, force: true});
    }
})().catch(error => { console.error(error); process.exitCode = 1; });
