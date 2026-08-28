# Cloudflare deployment (vela-relay-cf)

> Stub — completed by task T024. The authoritative gates live in
> `specs/002-cf-worker-shell/quickstart.md`.

The second deployment target runs the same `vela-relay-core` decisions on
Cloudflare Workers (Rust/wasm): Durable Objects replace Redis, Queues replace
Iggy, KV holds caches only. **Workers Paid is required** (the Free plan's
10 ms CPU budget cannot host the signing/hashing paths).

## Build & dev

```sh
cd vela-relay-cf
cargo check -p vela-relay-cf --target wasm32-unknown-unknown --locked  # gate
npx wrangler dev        # local: workerd emulates DO + Queues + KV
```

`wrangler.jsonc` carries the bindings; `.dev.vars.example` documents local
vars. Secrets are set with `wrangler secret put` (OPERATOR_SECRET,
ALCHEMY_API_KEY, TELEGRAM_*) and never live in config files.

Behind an HTTP proxy, worker-build's own binary downloads (wasm-bindgen,
wasm-opt) may fail; point it at locally installed binaries instead:

```sh
WASM_BINDGEN_BIN=$(which wasm-bindgen) WASM_OPT_BIN=$(which wasm-opt) npx wrangler dev
```

## Chains and ownership

Chains are dynamic (no per-chain provisioning): admission resolves metadata
from the controlled chain directory, the queue is chain-agnostic, and Durable
Objects materialize on first use. `VELA_RELAY_EXECUTION_CHAINS` stays empty
for directory-driven execution; it becomes mandatory and globally disjoint
across deployments only when key material is shared (FR-010 — see
`specs/002-cf-worker-shell/research.md` R10, and R11 for the
`VELA_RELAY_RELAYER_COUNT` 1..=100 provisioning-time policy).
