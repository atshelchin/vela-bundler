#!/usr/bin/env python3
"""Build the JSON-RPC request battery. Mirrors the core tests' Safe
executeUserOp -> MultiSend(delegatecall) -> CALL(recipient, value) encoding."""
import json, os

RECIPIENT = "00000000000000000000000000000000000000fe"
TRUSTED = "38869bf66a61cf6bdb996a6ae40d5853fd43b526"
ENTRY_POINT = "0x0000000071727De22E5E9d8BAf0edAc6f37da032"
MIN_NATIVE = 10**13  # 0.00001 ETH at 18 decimals


def word(value: int) -> bytes:
    return value.to_bytes(32, "big")


def native_payment_calldata(amount: int) -> str:
    packed = bytes([0]) + bytes.fromhex(RECIPIENT) + word(amount) + word(0)
    multisend = bytes.fromhex("8d80ff0a") + word(32) + word(len(packed)) + packed
    multisend += bytes((32 - len(multisend) % 32) % 32)
    call = bytes.fromhex("7bb37428")
    call += bytes(12) + bytes.fromhex(TRUSTED)
    call += word(0) + word(128) + word(1) + word(len(multisend)) + multisend
    return "0x" + call.hex()


def op(call_data: str, max_fee: str = "0x0") -> dict:
    return {
        "sender": "0x00000000000000000000000000000000000000aa",
        "nonce": "0x0",
        "callData": call_data,
        "callGasLimit": "0x186a0",
        "verificationGasLimit": "0x186a0",
        "preVerificationGas": "0x5208",
        "maxFeePerGas": max_fee,
        "maxPriorityFeePerGas": max_fee,
        "signature": "0x" + "11" * 65,
    }


def rpc(idv, method, params) -> str:
    return json.dumps(
        {"jsonrpc": "2.0", "id": idv, "method": method, "params": params},
        separators=(",", ":"),
    )

UNKNOWN_HASH = "0x" + "de" * 32

# (name, safe_for_prod, body_or_None_for_raw, raw_body)
BATTERY = [
    ("sep", True, rpc(1, "eth_supportedEntryPoints", [])),
    ("unknown-method", True, rpc(2, "eth_chainId", [])),
    ("malformed-json", True, '{"jsonrpc":"2.0", broken'),
    ("wrong-version", True, '{"jsonrpc":"1.0","id":3,"method":"eth_supportedEntryPoints","params":[]}'),
    ("send-nonzero-fee", True, rpc(4, "eth_sendUserOperation", [op(native_payment_calldata(MIN_NATIVE), "0x3b9aca00"), ENTRY_POINT])),
    ("send-empty-calldata", True, rpc(5, "eth_sendUserOperation", [op("0x"), ENTRY_POINT])),
    ("send-under-minimum", True, rpc(6, "eth_sendUserOperation", [op(native_payment_calldata(MIN_NATIVE - 1)), ENTRY_POINT])),
    ("send-valid", False, rpc(7, "eth_sendUserOperation", [op(native_payment_calldata(MIN_NATIVE)), ENTRY_POINT])),
    ("send-duplicate", False, rpc(8, "eth_sendUserOperation", [op(native_payment_calldata(MIN_NATIVE)), ENTRY_POINT])),
    ("status-accepted", False, rpc(20, "pimlico_getUserOperationStatus", ["0x9d699d70fbf28253b0e463d3bc2f60ebdf217b5ddf256adef46c0dc05f23ce95"])),
    ("byhash-accepted", False, rpc(21, "eth_getUserOperationByHash", ["0x9d699d70fbf28253b0e463d3bc2f60ebdf217b5ddf256adef46c0dc05f23ce95"])),
    ("receipt-accepted", False, rpc(22, "eth_getUserOperationReceipt", ["0x9d699d70fbf28253b0e463d3bc2f60ebdf217b5ddf256adef46c0dc05f23ce95"])),
    ("status-unknown", True, rpc(9, "pimlico_getUserOperationStatus", [UNKNOWN_HASH])),
    ("byhash-unknown", True, rpc(10, "eth_getUserOperationByHash", [UNKNOWN_HASH])),
    ("receipt-unknown", True, rpc(11, "eth_getUserOperationReceipt", [UNKNOWN_HASH])),
    ("bad-entrypoint", True, rpc(12, "eth_sendUserOperation", [op(native_payment_calldata(MIN_NATIVE)), "0x1111111111111111111111111111111111111111"])),
]

out = os.path.join(os.path.dirname(__file__), "battery")
os.makedirs(out, exist_ok=True)
manifest = []
for index, (name, safe, body) in enumerate(BATTERY):
    fname = f"{index:02d}-{name}.json"
    with open(os.path.join(out, fname), "w") as f:
        f.write(body)
    manifest.append({"file": fname, "name": name, "safe_for_prod": safe})
with open(os.path.join(out, "manifest.json"), "w") as f:
    json.dump(manifest, f, indent=1)
print(f"wrote {len(BATTERY)} bodies to {out}")
