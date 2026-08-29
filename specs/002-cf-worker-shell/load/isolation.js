// SC-007 per-chain isolation: saturate one chain's intake while measuring a
// second, lightly loaded chain. Run the light profile alone first (baseline),
// then both together; the victim chain's p95s must degrade by less than 10%.
//
//   # baseline (light traffic only):
//   k6 run -e BASE_URL=… -e RECIPIENT=… -e VICTIM_CHAIN=8453 \
//          -e SATURATE_RATE=0 -e DURATION=5m isolation.js
//   # experiment (saturator on):
//   k6 run -e BASE_URL=… -e RECIPIENT=… -e VICTIM_CHAIN=8453 \
//          -e SATURATED_CHAIN=42161 -e SATURATE_RATE=1500 -e DURATION=5m isolation.js
//
// Compare `http_req_duration{chain:victim}` p95 between the two runs.
// Same posture as load.js: executor disabled, RECIPIENT = derived vault.

import http from "k6/http";
import { check } from "k6";

const BASE_URL = __ENV.BASE_URL;
const RECIPIENT = (__ENV.RECIPIENT || "").replace(/^0x/, "").toLowerCase();
const SATURATED_CHAIN = __ENV.SATURATED_CHAIN || "42161";
const VICTIM_CHAIN = __ENV.VICTIM_CHAIN || "8453";
const SATURATE_RATE = Number(__ENV.SATURATE_RATE || 1500);
const VICTIM_SUBMIT_RATE = Number(__ENV.VICTIM_SUBMIT_RATE || 20);
const VICTIM_READ_RATE = Number(__ENV.VICTIM_READ_RATE || 200);
const DURATION = __ENV.DURATION || "5m";

const TRUSTED = "38869bf66a61cf6bdb996a6ae40d5853fd43b526";
const ENTRY_POINT = "0x0000000071727De22E5E9d8BAf0edAc6f37da032";
const AMOUNT_WORD = "000000000000000000000000000000000000000000000000000009184e72a000";

function word(hex) {
  return hex.padStart(64, "0");
}

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
const PARAMS = { headers: { "Content-Type": "application/json" } };

function operation(prefix) {
  return {
    sender: "0x00000000000000000000000000000000000000aa",
    nonce:
      "0x" +
      prefix +
      (__VU >>> 0).toString(16).padStart(8, "0") +
      (__ITER >>> 0).toString(16).padStart(8, "0"),
    callData: CALL_DATA,
    callGasLimit: "0x186a0",
    verificationGasLimit: "0x186a0",
    preVerificationGas: "0x5208",
    maxFeePerGas: "0x0",
    maxPriorityFeePerGas: "0x0",
    signature: "0x" + "11".repeat(65),
  };
}

function submitTo(chain, prefix, tag) {
  const body = JSON.stringify({
    jsonrpc: "2.0",
    id: 1,
    method: "eth_sendUserOperation",
    params: [operation(prefix), ENTRY_POINT],
  });
  const res = http.post(`${BASE_URL}/${chain}`, body, {
    ...PARAMS,
    tags: { chain: tag },
  });
  check(res, { [`${tag} submit 200`]: (r) => r.status === 200 });
}

const scenarios = {
  victim_submits: {
    executor: "constant-arrival-rate",
    exec: "victimSubmit",
    rate: VICTIM_SUBMIT_RATE,
    timeUnit: "1s",
    duration: DURATION,
    preAllocatedVUs: 20,
    maxVUs: 200,
  },
  victim_reads: {
    executor: "constant-arrival-rate",
    exec: "victimRead",
    rate: VICTIM_READ_RATE,
    timeUnit: "1s",
    duration: DURATION,
    preAllocatedVUs: 40,
    maxVUs: 400,
  },
};
if (SATURATE_RATE > 0) {
  scenarios.saturator = {
    executor: "constant-arrival-rate",
    exec: "saturate",
    rate: SATURATE_RATE,
    timeUnit: "1s",
    duration: DURATION,
    preAllocatedVUs: 300,
    maxVUs: 3000,
  };
}

export const options = { scenarios };

export function saturate() {
  submitTo(SATURATED_CHAIN, "5a", "saturated");
}

export function victimSubmit() {
  submitTo(VICTIM_CHAIN, "1b", "victim");
}

export function victimRead() {
  const body = JSON.stringify({
    jsonrpc: "2.0",
    id: 2,
    method: "pimlico_getUserOperationStatus",
    params: ["0x" + (__ITER >>> 0).toString(16).padStart(64, "d")],
  });
  const res = http.post(`${BASE_URL}/${VICTIM_CHAIN}`, body, {
    ...PARAMS,
    tags: { chain: "victim" },
  });
  check(res, { "victim read 200": (r) => r.status === 200 });
}
