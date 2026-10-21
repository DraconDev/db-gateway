#!/usr/bin/env python3
"""Read the address a `dg serve` logged, from its log file.

The gateway logs the address it bound as one JSON line. Reading it
from the log is how a caller finds an ephemeral port without asking
the gateway to report it twice.
"""

import json
import sys


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: read-gateway-address.py LOG_FILE", file=sys.stderr)
        return 2
    try:
        with open(sys.argv[1], errors="replace") as log:
            for line in log:
                try:
                    entry = json.loads(line)
                except ValueError:
                    continue
                address = entry.get("fields", {}).get("address")
                if address:
                    print("http://" + address)
                    return 0
    except OSError:
        return 1
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
