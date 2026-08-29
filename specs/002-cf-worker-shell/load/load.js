// SC-004 load harness: sustained submits + status reads against a DEPLOYED
// vela-relay-cf environment. Run one instance per region (three regions for
// the gate) and sum the rates.
//
//   k6 run -e BASE_URL=https://<worker-host> -e RECIPIENT=<derived-vault-hex40> \
//          -e CHAIN_ID=42161 -e SUBMIT_RATE=334 -e READ_RATE=3334 \
//          -e DURATION=30m -e REGION=a load.js
//
// POSTURE (mandatory): the target deployment runs
// VELA_RELAY_EXECUTOR_ENABLED=false — SC-004 measures the intake/read
// surface; accepted operations must not reach a real chain. RECIPIENT is the
// deployment's own derived settlement vault (40 hex chars, no 0x): the
// admission minimum-payment check must pass for a submission to be ACCEPTED,
// and only accepted submissions count.
//
// Per-region default rates: 1,000/3 submits/s and 10,000/3 reads/s.

import http from "k6/http";
import { check } from "k6";
import { Counter } from "k6/metrics";

const BASE_URL = __ENV.BASE_URL;
const CHAIN_ID = __ENV.CHAIN_ID || "42161";
const RECIPIENT = (__ENV.RECIPIENT || "").replace(/^0x/, "").toLowerCase();
const REGION = __ENV.REGION || "x";
const SUBMIT_RATE = Number(__ENV.SUBMIT_RATE || 334);
const READ_RATE = Number(__ENV.READ_RATE || 3334);
const DURATION = __ENV.DURATION || "30m";

const TRUSTED = "38869bf66a61cf6bdb996a6ae40d5853fd43b526";
const ENTRY_POINT = "0x0000000071727De22E5E9d8BAf0edAc6f37da032";
// 0.00001 ETH — the admission minimum for an 18-decimal native coin.
const AMOUNT_WORD = "000000000000000000000000000000000000000000000000000009184e72a000";

const accepted = new Counter("accepted_submissions");
const refused = new Counter("refused_submissions");

function word(hex) {
  return hex.padStart(64, "0");
}

// mkop.py's Safe executeUserOp -> MultiSend(delegatecall) -> CALL(vault, min)
function paymentCalldata() {
  let packed = "00" + RECIPIENT + AMOUNT_WORD + word("");
  let ms = "8d80ff0a" + word("20") + word((packed.length / 2).toString(16)) + packed;
  const pad = (32 - (ms.length / 2) % 32) % 32;
  ms += "00".repeat(pad);
  return (
    "0x7bb37428" +
    word(TRUSTED) +
    word("") +
    word("80") +
    word("1") +
    word((ms.length / 2).toString(16)) +
    ms
  );
}

const CALL_DATA = paymentCalldata();

// Unique hash per submission without BigInt: nonce = region byte | VU | iter.
function uniqueNonce(vu, iter) {
  const regionByte = (REGION.charCodeAt(0) & 0xff).toString(16).padStart(2, "0");
  return (
    "0x" +
    regionByte +
    (vu >>> 0).toString(16).padStart(8, "0") +
    (iter >>> 0).toString(16).padStart(8, "0")
  );
}

function operation(nonce) {
  return {
    sender: "0x00000000000000000000000000000000000000aa",
    nonce,
    callData: CALL_DATA,
    callGasLimit: "0x186a0",
    verificationGasLimit: "0x186a0",
    preVerificationGas: "0x5208",
    maxFeePerGas: "0x0",
    maxPriorityFeePerGas: "0x0",
    signature: "0x" + "11".repeat(65),
  };
}

export const options = {
  scenarios: {
    submits: {
      executor: "constant-arrival-rate",
      exec: "submit",
      rate: SUBMIT_RATE,
      timeUnit: "1s",
      duration: DURATION,
      preAllocatedVUs: 200,
      maxVUs: 2000,
    },
    reads: {
      executor: "constant-arrival-rate",
      exec: "read",
      rate: READ_RATE,
      timeUnit: "1s",
      duration: DURATION,
      preAllocatedVUs: 400,
      maxVUs: 4000,
    },
  },
  thresholds: {
    // SC-004: p95 submission ack < 500 ms, p95 read < 200 ms, zero
    // capacity-caused failures.
    "http_req_duration{scenario:submits}": ["p(95)<500"],
    "http_req_duration{scenario:reads}": ["p(95)<200"],
    "http_req_failed": ["rate<0.001"],
  },
};

const PARAMS = { headers: { "Content-Type": "application/json" } };
// Per-VU ring of recently accepted hashes for realistic reads.
const recent = [];

export function submit() {
  const nonce = uniqueNonce(__VU, __ITER);
  const body = JSON.stringify({
    jsonrpc: "2.0",
    id: 1,
    method: "eth_sendUserOperation",
    params: [operation(nonce), ENTRY_POINT],
  });
  const res = http.post(`${BASE_URL}/${CHAIN_ID}`, body, PARAMS);
  const parsed = res.status === 200 ? res.json() : {};
  const ok = check(res, {
    "submit 200": (r) => r.status === 200,
    "submit accepted": () => typeof parsed.result === "string",
  });
  if (ok && typeof parsed.result === "string") {
    accepted.add(1);
    recent.push(parsed.result);
    if (recent.length > 32) recent.shift();
  } else {
    refused.add(1);
  }
}

export function read() {
  // 3:1 known-hash status reads vs unknown-hash reads (cache-miss path).
  const known = recent.length > 0 && __ITER % 4 !== 3;
  const hash = known
    ? recent[__ITER % recent.length]
    : "0x" + (__ITER >>> 0).toString(16).padStart(64, "e");
  const body = JSON.stringify({
    jsonrpc: "2.0",
    id: 2,
    method: "pimlico_getUserOperationStatus",
    params: [hash],
  });
  const res = http.post(`${BASE_URL}/${CHAIN_ID}`, body, PARAMS);
  check(res, { "read 200": (r) => r.status === 200 });
}
