// Emit dist/index.cjs (and its .d.cts) next to tsc's ESM output.
//
// The source uses `createRequire(import.meta.url)` to load the CommonJS wasm
// glue; in a CJS bundle `import.meta` does not exist, so it is replaced with
// the file URL of `__filename`.
import { build } from 'esbuild'
import { copyFileSync } from 'node:fs'

await build({
  entryPoints: ['src/index.ts'],
  outfile: 'dist/index.cjs',
  bundle: true,
  format: 'cjs',
  platform: 'node',
  target: 'node18',
  external: ['faunadb'],
  define: { 'import.meta.url': '__fqlEmuImportMetaUrl' },
  banner: { js: "const __fqlEmuImportMetaUrl = require('node:url').pathToFileURL(__filename).href;" },
  logLevel: 'warning',
})
copyFileSync('dist/index.d.ts', 'dist/index.d.cts')
