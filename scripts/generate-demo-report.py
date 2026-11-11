#!/usr/bin/env python3
"""Generate a synthetic LidlessEye report for demos and smoke tests.

Usage:
    python3 scripts/generate-demo-report.py > /tmp/report.json
    curl -X POST localhost:4040/ingest --data-binary @/tmp/report.json
"""

import json
import time
import uuid


def main() -> None:
    now = time.time_ns()
    start = now - 10_000_000_000
    strings = [
        "/usr/local/bin/checkout",  # 0 module
        "main",                     # 1
        "handle_request",           # 2
        "encode_json",              # 3
        "charge_card",              # 4
        "validate_cart",            # 5
        "/lib/x86_64-linux-gnu/libc.so.6",  # 6
        "memcpy",                   # 7
        "[kernel]",                 # 8
        "entry_SYSCALL_64_after_hwframe",  # 9
        "do_syscall_64",            # 10
        "vfs_write",                # 11
        "src/http.rs",              # 12
    ]
    stacks = [
        ([[0, 1, 12, 42]], 900_000_000),
        ([[0, 2, 12, 118], [0, 1, 12, 42]], 700_000_000),
        ([[0, 3, 12, 240], [0, 2, 12, 118], [0, 1, 12, 42]], 300_000_000),
        ([[0, 4, 12, 312], [0, 2, 12, 118], [0, 1, 12, 42]], 200_000_000),
        ([[0, 5, 12, 355], [0, 2, 12, 118], [0, 1, 12, 42]], 120_000_000),
        ([[6, 7, 0, 0], [0, 3, 12, 240], [0, 2, 12, 118], [0, 1, 12, 42]], 80_000_000),
        (
            [
                [8, 11, 0, 0],
                [8, 10, 0, 0],
                [8, 9, 0, 0],
                [6, 7, 0, 0],
                [0, 3, 12, 240],
                [0, 2, 12, 118],
                [0, 1, 12, 42],
            ],
            40_000_000,
        ),
    ]
    report = {
        "service_name": "checkout",
        "labels": {
            "namespace": "shop",
            "pod": f"checkout-{uuid.uuid4().hex[:6]}",
            "node": "demo-node",
        },
        "profile_type": "cpu",
        "start_ns": start,
        "end_ns": now,
        "sample_rate_hz": 99,
        "strings": strings,
        "stacks": [{"frames": frames, "value": value} for frames, value in stacks],
    }
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
