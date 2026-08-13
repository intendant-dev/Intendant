/* tslint:disable */
/* eslint-disable */

/**
 * The XR surface's JS-facing handle. One per dashboard page, constructed
 * lazily by the `ui2-xr.js` fragment on first use.
 */
export class XrWeb {
    free(): void;
    [Symbol.dispose](): void;
    /**
     * Activate a scene target by hit-target id (`card:<agent>`,
     * `pill:<agent>:<op>`, `banner:<agent>`), the same
     * activation-by-name contract the other rendered surface gives the
     * validator and accessibility layers. Runs the exact dispatch path
     * a completed ray interaction runs — activation by name IS the
     * deliberate act, so approve/deny fire without the hold. Returns
     * true when the target existed and had an effect.
     */
    activate(name: string): boolean;
    /**
     * QA/introspection hook: JSON string of the facade + engine state.
     * Kept schema-stable for the validator probe (`--xr-probe`).
     */
    debugJson(): string;
    /**
     * Enter an immersive session ("immersive-ar" or "immersive-vr").
     * Resolves once the session is live and the frame loop is armed.
     */
    enter(mode: string): Promise<any>;
    /**
     * End the active immersive session, if any (cleanup and the
     * session-end callback run from the session's own 'end' event).
     */
    exit(): void;
    constructor();
    /**
     * Probe `navigator.xr` for immersive-ar / immersive-vr support.
     * Resolves to `{ ar: bool, vr: bool }` and caches the answer for
     * `debug_json`. Never rejects — an absent or throwing XR system
     * reads as unsupported.
     */
    probeSupport(): Promise<any>;
    /**
     * Register the dashboard's action router. Actions emitted by the XR
     * surface call this with one JSON-stringifiable object argument.
     */
    setActionCallback(callback: Function): void;
    /**
     * Register a no-argument callback fired whenever the immersive
     * session ends (user gesture, `exit()`, or runtime shutdown).
     */
    setOnSessionEnd(callback: Function): void;
    /**
     * Ingest one coalesced dashboard state snapshot (same feed schema the
     * other rendered surface consumes). Parse failures keep the previous
     * scene and count in `debug_json` — the feed must never take the
     * session down.
     */
    updateSnapshot(snapshot: any): void;
}

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly __wbg_xrweb_free: (a: number, b: number) => void;
    readonly xrweb_activate: (a: number, b: number, c: number) => number;
    readonly xrweb_debugJson: (a: number) => [number, number];
    readonly xrweb_enter: (a: number, b: number, c: number) => any;
    readonly xrweb_exit: (a: number) => void;
    readonly xrweb_new: () => number;
    readonly xrweb_probeSupport: (a: number) => any;
    readonly xrweb_setActionCallback: (a: number, b: any) => void;
    readonly xrweb_setOnSessionEnd: (a: number, b: any) => void;
    readonly xrweb_updateSnapshot: (a: number, b: any) => void;
    readonly wasm_bindgen__closure__destroy__h95fa55e82713b162: (a: number, b: number) => void;
    readonly wasm_bindgen__closure__destroy__hb9ef122cd6bafce1: (a: number, b: number) => void;
    readonly wasm_bindgen__convert__closures_____invoke__h80d27238e69792d0: (a: number, b: number, c: number, d: any) => void;
    readonly wasm_bindgen__convert__closures_____invoke__h8830e5bad9fe7bb6: (a: number, b: number, c: any, d: any) => void;
    readonly wasm_bindgen__convert__closures_____invoke__h5fa997591c1e8ef4: (a: number, b: number, c: any) => void;
    readonly wasm_bindgen__convert__closures_____invoke__h7dfd20110b18ff44: (a: number, b: number, c: any) => void;
    readonly __wbindgen_malloc: (a: number, b: number) => number;
    readonly __wbindgen_realloc: (a: number, b: number, c: number, d: number) => number;
    readonly __wbindgen_exn_store: (a: number) => void;
    readonly __externref_table_alloc: () => number;
    readonly __wbindgen_externrefs: WebAssembly.Table;
    readonly __wbindgen_free: (a: number, b: number, c: number) => void;
    readonly __wbindgen_start: () => void;
}

export type SyncInitInput = BufferSource | WebAssembly.Module;

/**
 * Instantiates the given `module`, which can either be bytes or
 * a precompiled `WebAssembly.Module`.
 *
 * @param {{ module: SyncInitInput }} module - Passing `SyncInitInput` directly is deprecated.
 *
 * @returns {InitOutput}
 */
export function initSync(module: { module: SyncInitInput } | SyncInitInput): InitOutput;

/**
 * If `module_or_path` is {RequestInfo} or {URL}, makes a request and
 * for everything else, calls `WebAssembly.instantiate` directly.
 *
 * @param {{ module_or_path: InitInput | Promise<InitInput> }} module_or_path - Passing `InitInput` directly is deprecated.
 *
 * @returns {Promise<InitOutput>}
 */
export default function __wbg_init (module_or_path?: { module_or_path: InitInput | Promise<InitInput> } | InitInput | Promise<InitInput>): Promise<InitOutput>;
