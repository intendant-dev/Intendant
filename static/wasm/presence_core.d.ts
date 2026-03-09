/* tslint:disable */
/* eslint-disable */

/**
 * Browser-side presence state.
 *
 * Wraps `AgentStateSnapshot` and exposes tool dispatch, event formatting,
 * and state queries to JavaScript.
 */
export class WasmPresence {
    free(): void;
    [Symbol.dispose](): void;
    /**
     * Dispatch a tool call using local agent state.
     *
     * Returns a `PresenceAction` JS object:
     * - `{ type: "TextResult", data: "..." }` — resolved locally
     * - `{ type: "SubmitTask", data: { task, force_direct, context_hints } }`
     * - `{ type: "Approve", data: { id } }`
     * - `{ type: "Deny", data: { id } }`
     * - `{ type: "Skip", data: { id } }`
     * - `{ type: "Respond", data: { text } }`
     * - `{ type: "SetAutonomy", data: { level } }`
     * - `{ type: "NeedsIO", data: { tool_name, args } }` — needs server round-trip
     */
    dispatch(tool_name: string, args: any): any;
    /**
     * Get the current agent state as a JS object.
     */
    get_state(): any;
    /**
     * Check if there is a pending approval.
     */
    has_pending_approval(): boolean;
    /**
     * Create a new presence instance with default (empty) agent state.
     */
    constructor();
    /**
     * Get the current phase.
     */
    phase(): string;
    /**
     * Replace the entire agent state (e.g. from a bootstrap `state_snapshot`).
     */
    set_state(state: any): void;
    /**
     * Update state from a server-sent event (OutboundEvent JSON).
     *
     * Returns a formatted narration string if the event should be narrated
     * to the live model, or `null` if the event is not narration-worthy.
     */
    update_from_event(event: any): any;
}

/**
 * Return the compiled-in presence system prompt.
 */
export function get_presence_prompt(): string;

/**
 * Return all presence tool definitions as a JS array.
 */
export function get_presence_tools(): any;

/**
 * Unicode-safe string truncation (appends "..." if truncated).
 */
export function wasm_truncate(s: string, max: number): string;

export type InitInput = RequestInfo | URL | Response | BufferSource | WebAssembly.Module;

export interface InitOutput {
    readonly memory: WebAssembly.Memory;
    readonly __wbg_wasmpresence_free: (a: number, b: number) => void;
    readonly get_presence_prompt: () => [number, number];
    readonly get_presence_tools: () => any;
    readonly wasm_truncate: (a: number, b: number, c: number) => [number, number];
    readonly wasmpresence_dispatch: (a: number, b: number, c: number, d: any) => any;
    readonly wasmpresence_get_state: (a: number) => any;
    readonly wasmpresence_has_pending_approval: (a: number) => number;
    readonly wasmpresence_new: () => number;
    readonly wasmpresence_phase: (a: number) => [number, number];
    readonly wasmpresence_set_state: (a: number, b: any) => void;
    readonly wasmpresence_update_from_event: (a: number, b: any) => any;
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
