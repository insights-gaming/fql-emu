/**
 * Lossless JSON for the differential runner.
 *
 * `JSON.parse` turns `9223372036854775807` into `9223372036854776000` and
 * `1.0` into `1`, and moves integer-like object keys to the front. Neither
 * is acceptable when replaying wire-format cases, so this module keeps
 * every number literal verbatim (`LNum`), keeps object keys in source order
 * (`Map`), and compares values the way the Rust conformance harness does
 * (`serde_json::Value` equality): integers by exact value, floats by f64
 * value, and an integer never equals a float (`1` != `1.0`). Object
 * equality ignores key order.
 */

export class LNum {
  constructor(readonly raw: string) {}

  /** Integer literal (no fraction or exponent): compared exactly. */
  get isInteger(): boolean {
    return !/[.eE]/.test(this.raw)
  }
}

export type LJ = null | boolean | string | LNum | LJ[] | LObj
export type LObj = Map<string, LJ>

export const isObj = (v: LJ | undefined): v is LObj => v instanceof Map
export const isNum = (v: LJ | undefined): v is LNum => v instanceof LNum

// ---------------------------------------------------------------------------
// parse

const STRING = /"(?:[^"\\\u0000-\u001f]|\\(?:["\\/bfnrt]|u[0-9a-fA-F]{4}))*"/y
const NUMBER = /-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?/y

export function parse(text: string): LJ {
  let i = 0
  const fail = (what: string): never => {
    throw new SyntaxError(`ljson: ${what} at offset ${i}: ${JSON.stringify(text.slice(i, i + 20))}`)
  }
  const ws = () => {
    while (i < text.length && (text[i] === ' ' || text[i] === '\n' || text[i] === '\r' || text[i] === '\t')) i++
  }
  const str = (): string => {
    STRING.lastIndex = i
    const m = STRING.exec(text)
    if (!m) return fail('bad string')
    i += m[0].length
    return JSON.parse(m[0]) as string
  }
  const value = (): LJ => {
    ws()
    const c = text[i]
    if (c === '{') {
      i++
      const o: LObj = new Map()
      ws()
      if (text[i] === '}') {
        i++
        return o
      }
      for (;;) {
        ws()
        if (text[i] !== '"') fail('expected key')
        const k = str()
        ws()
        if (text[i++] !== ':') fail('expected :')
        o.set(k, value())
        ws()
        if (text[i] === ',') {
          i++
          continue
        }
        if (text[i++] !== '}') fail('expected , or }')
        return o
      }
    }
    if (c === '[') {
      i++
      const a: LJ[] = []
      ws()
      if (text[i] === ']') {
        i++
        return a
      }
      for (;;) {
        a.push(value())
        ws()
        if (text[i] === ',') {
          i++
          continue
        }
        if (text[i++] !== ']') fail('expected , or ]')
        return a
      }
    }
    if (c === '"') return str()
    for (const [lit, v] of [
      ['true', true],
      ['false', false],
      ['null', null],
    ] as const) {
      if (text.startsWith(lit, i)) {
        i += lit.length
        return v
      }
    }
    NUMBER.lastIndex = i
    const m = NUMBER.exec(text)
    if (!m) return fail('unexpected token')
    i += m[0].length
    return new LNum(m[0])
  }
  const v = value()
  ws()
  if (i !== text.length) fail('trailing data')
  return v
}

// ---------------------------------------------------------------------------
// stringify

export function stringify(v: LJ): string {
  if (v === null) return 'null'
  if (typeof v === 'boolean') return String(v)
  if (typeof v === 'string') return JSON.stringify(v)
  if (v instanceof LNum) return v.raw
  if (Array.isArray(v)) return `[${v.map(stringify).join(',')}]`
  return `{${[...v].map(([k, x]) => `${JSON.stringify(k)}:${stringify(x)}`).join(',')}}`
}

// ---------------------------------------------------------------------------
// equality

function numEq(a: LNum, b: LNum): boolean {
  if (a.isInteger !== b.isInteger) return false
  if (a.isInteger) return BigInt(a.raw) === BigInt(b.raw)
  return Number(a.raw) === Number(b.raw)
}

export function equal(a: LJ | undefined, b: LJ | undefined): boolean {
  if (a === b) return true
  if (a === undefined || b === undefined || a === null || b === null) return false
  if (a instanceof LNum) return b instanceof LNum && numEq(a, b)
  if (Array.isArray(a)) return Array.isArray(b) && a.length === b.length && a.every((x, i) => equal(x, b[i]))
  if (a instanceof Map) {
    if (!(b instanceof Map) || a.size !== b.size) return false
    for (const [k, x] of a) if (!b.has(k) || !equal(x, b.get(k))) return false
    return true
  }
  return false
}

// ---------------------------------------------------------------------------
// helpers

/** `o[k]` for objects, `undefined` otherwise. */
export function get(o: LJ | undefined, k: string): LJ | undefined {
  return isObj(o) ? o.get(k) : undefined
}

export function obj(entries: Array<[string, LJ]>): LObj {
  return new Map(entries)
}

/** Deep map over every node, bottom-up. `key` is the object key the node sits under. */
export function mapTree(v: LJ, f: (v: LJ, key: string | undefined) => LJ, key?: string): LJ {
  let out: LJ = v
  if (Array.isArray(v)) out = v.map((x) => mapTree(x, f))
  else if (isObj(v)) out = new Map([...v].map(([k, x]) => [k, mapTree(x, f, k)]))
  return f(out, key)
}

/** Does any object in the tree have a key matching `pred`? */
export function someKey(v: LJ, pred: (k: string) => boolean): boolean {
  if (Array.isArray(v)) return v.some((x) => someKey(x, pred))
  if (isObj(v)) for (const [k, x] of v) if (pred(k) || someKey(x, pred)) return true
  return false
}
