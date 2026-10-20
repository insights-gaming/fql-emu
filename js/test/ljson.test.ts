// The differential runner's lossless JSON: number literals survive verbatim
// and compare like serde_json values in the Rust conformance harness.
import { describe, expect, it } from 'vitest'
import { equal, LNum, parse, stringify } from '../src/ljson.js'

describe('ljson', () => {
  it('round-trips number literals and key order verbatim', () => {
    const text = '{"b":9223372036854775807,"a":[1.0,-0.5e-3,2],"1":null,"s":"é\\n"}'
    expect(stringify(parse(text))).toBe(text)
  })

  it('compares integers exactly and never equal to floats', () => {
    expect(equal(parse('9223372036854775807'), parse('9223372036854775806'))).toBe(false)
    expect(equal(parse('1'), parse('1.0'))).toBe(false)
    expect(equal(parse('1.0'), parse('1.00'))).toBe(true)
    expect(equal(parse('100.0'), parse('1e2'))).toBe(true)
    expect(equal(new LNum('-0'), new LNum('0'))).toBe(true)
  })

  it('ignores object key order', () => {
    expect(equal(parse('{"a":1,"b":[true]}'), parse('{"b":[true],"a":1}'))).toBe(true)
    expect(equal(parse('{"a":1}'), parse('{"a":1,"b":2}'))).toBe(false)
  })

  it('rejects malformed input', () => {
    expect(() => parse('{"a":1,}')).toThrow(SyntaxError)
    expect(() => parse('[1] x')).toThrow(SyntaxError)
    expect(() => parse('01')).toThrow(SyntaxError)
  })
})
