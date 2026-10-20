/* tslint:disable */
/* eslint-disable */

export class FqlEmulator {
    free(): void;
    [Symbol.dispose](): void;
    fork(): FqlEmulator;
    /**
     * Load a schema bundle (see `Emulator::load_schema`). Returns `null`
     * on success or a JSON array of errors.
     */
    load_schema(bundle_json: string): string;
    constructor();
    /**
     * Run one query. `expr_json` is the serialised query expression.
     * Returns `{"status": 200, "body": {"resource": ...}, "txn_time_micros": n}`
     * or `{"status": 4xx, "body": {"errors": [...]}, "txn_time_micros": n}`
     * (`txn_time_micros` is `null` when the body was not valid JSON).
     */
    query(expr_json: string): string;
    set_auto_clock_from_micros(micros: number): void;
    /**
     * `identity_json` is a `{"@ref": ...}` value, or `null`.
     */
    set_identity(identity_json: string): string;
    set_max_documents(n: number): void;
    /**
     * `id` is a JS `BigInt`: Fauna ids (~3e17) do not fit in an f64.
     */
    set_next_id(id: bigint): void;
    set_now_micros(micros: number): void;
    stats(): string;
    /**
     * JSON array of `[name, implemented]` pairs.
     */
    static supported_forms(): string;
}
