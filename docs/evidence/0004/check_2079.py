#!/usr/bin/env python3
"""Independent cross-check for the relic 2079 showcase (RFC-0004 §12.4).

    SHADOW_RPC_BEARER=... python3 -I check_2079.py https://rpc.graphops.xyz/v1/evm/1

Shares no code with the Shadow or Burrmill. It asks the RPC directly and sums with Python's
unbounded integers, so its numbers are exact by construction. It does share a provider with the
Shadow when pointed at the same endpoint, which is stated wherever its output is shown.
"""

import json
import os
import sys
import threading
import time
import urllib.request
from concurrent.futures import ThreadPoolExecutor

START, END = 17031168, 17039359
STETH = "0xae7ab96520de3a18e5e111b5eaab095312d7fe84"
TRANSFER = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
APPROVAL = "0x8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925"
DEC38_MAX = 10**38 - 1

URL = sys.argv[1]
BEARER = os.environ.get("SHADOW_RPC_BEARER")
calls = 0
lock = threading.Lock()
bucket = threading.Semaphore(8)


def rpc(method, params):
    global calls
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    # GraphOps answers urllib's default User-Agent with 403; any honest name is served.
    headers = {"content-type": "application/json", "user-agent": "the-legacy-crosscheck/0.1"}
    if BEARER:
        headers["authorization"] = f"Bearer {BEARER}"
    for attempt in range(6):
        with lock:
            calls += 1
        try:
            req = urllib.request.Request(URL, data=body, headers=headers)
            with bucket, urllib.request.urlopen(req, timeout=60) as r:
                reply = json.load(r)
            if "error" in reply:
                raise RuntimeError(reply["error"])
            return reply["result"]
        except Exception:
            if attempt == 5:
                raise
            time.sleep(1 + attempt * 2)


def withdrawals():
    def one(n):
        b = rpc("eth_getBlockByNumber", [hex(n), False])
        return n, b.get("withdrawals") or []

    count = gwei = 0
    first = None
    with ThreadPoolExecutor(8) as pool:
        for n, ws in pool.map(one, range(START, END + 1)):
            count += len(ws)
            gwei += sum(int(w["amount"], 16) for w in ws)
            if ws and first is None:
                first = (n, min(int(w["index"], 16) for w in ws))
    return {"withdrawals": count, "gwei_total": gwei, "first_block": first[0], "first_index": first[1]}


def logs(topic):
    out = []
    for lo in range(START, END + 1, 1000):
        hi = min(lo + 999, END)
        out += rpc("eth_getLogs", [{"address": STETH, "topics": [topic], "fromBlock": hex(lo), "toBlock": hex(hi)}])
    return [int(l["data"][2:66], 16) for l in out]


def main():
    started = time.time()
    w = withdrawals()
    transfers = logs(TRANSFER)
    approvals = logs(APPROVAL)
    result = {
        "1-withdrawals": w,
        "2-steth-transfers": {
            "transfers": len(transfers),
            "wei_total": sum(transfers),
            "overflow_rows": sum(1 for v in transfers if v > DEC38_MAX),
        },
        "3-steth-approvals": {
            "unlimited_or_oversized": sum(1 for v in approvals if v > DEC38_MAX),
            "representable": sum(1 for v in approvals if v <= DEC38_MAX),
            "representable_total": sum(v for v in approvals if v <= DEC38_MAX),
            "largest_representable": max((v for v in approvals if v <= DEC38_MAX), default=None),
        },
        "rpc_calls": calls,
        "seconds": round(time.time() - started, 1),
        "provider": "the endpoint given on the command line; the same one the Shadow used",
    }
    print(json.dumps(result, indent=1))


if __name__ == "__main__":
    main()
