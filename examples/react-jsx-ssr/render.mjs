// Run a TypeScript-JSX (.tsx) React component on lumen. Plain .jsx runs natively — just
// `lumen-cli app.jsx` — because the runtime lowers JSX itself. TypeScript *type* stripping is not
// built in, though, so a .tsx file is transpiled with esbuild first (which itself runs on lumen).
// esbuild's `tsx` loader strips the types and lowers JSX to React.createElement calls; the
// `import React`/`import { renderToString }` bindings stay ESM imports and resolve against
// node_modules like any other module.
import { readFileSync, writeFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { dirname, join } from 'node:path'

// lumen has no worker threads, so tell esbuild to use its subprocess service instead of its
// worker-thread service. This is esbuild's own documented opt-out; it must be set before esbuild
// is loaded, hence the dynamic import below.
process.env.ESBUILD_WORKER_THREADS = '0'
const esbuild = (await import('esbuild')).default

const here = dirname(fileURLToPath(import.meta.url))
const entry = process.argv[2] ?? 'app.tsx'
const source = readFileSync(join(here, entry), 'utf8')

// One loader handles both: `tsx` = TypeScript + JSX. Classic runtime → React.createElement, so the
// component's `import React from 'react'` is what supplies the factory.
const { code } = await esbuild.transform(source, {
  loader: 'tsx',
  jsx: 'transform',
  jsxFactory: 'React.createElement',
  jsxFragment: 'React.Fragment',
})

// Write the transpiled module next to node_modules so its bare imports resolve, then run it.
const out = join(here, '_app.mjs')
writeFileSync(out, code)
await import('./_app.mjs')
