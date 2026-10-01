#!/usr/bin/env python3
"""Unit test: validate a TollGate v1 advertisement against cashud's parser.

Ports the exact predicate from cashud's parse_advertisement (v1,
tollgate-module-basic-go wire format):
  ["price_per_step","cashu",<sats>,"sat"|"sats",<mint>,<min_steps>]
  accepted only if method=="cashu" and unit in {sat,sats} and
  price_per_step>0 and min_steps>0 and mint non-empty.

Usage: ad_validate.py [gateway-url|path-to-json]   (default http://192.168.1.1:2121/)
Exit 0 = at least one valid option; exit 1 = cashud would reject this ad.
"""
import json
import sys
import urllib.request

DEFAULT = "http://192.168.1.1:2121/"


def load(source: str):
    if source.startswith("http"):
        with urllib.request.urlopen(source, timeout=6) as r:
            return json.load(r)
    with open(source) as f:
        return json.load(f)


def validate(ad: dict) -> tuple[list, list]:
    problems = []
    if ad.get("kind") != 10021:
        problems.append(f"kind != 10021 (got {ad.get('kind')!r})")
    pubkey = ad.get("pubkey", "")
    if len(pubkey) != 64 or not all(c in "0123456789abcdefABCDEF" for c in pubkey):
        problems.append("pubkey not 32-byte hex")
    metric = None
    step_size = 0
    pricing = []
    for tag in ad.get("tags", []):
        parts = tag if isinstance(tag, list) else []
        head = parts[0] if parts else ""
        if head == "metric":
            metric = parts[1] if len(parts) > 1 else None
        elif head == "step_size":
            try:
                step_size = int(parts[1])
            except (ValueError, IndexError):
                step_size = 0
        elif head == "price_per_step" and len(parts) >= 5:
            method = parts[1] if len(parts) > 1 else ""
            try:
                price = int(parts[2])
            except (ValueError, IndexError):
                price = 0
            unit = parts[3] if len(parts) > 3 else ""
            mint = parts[4] if len(parts) > 4 else ""
            try:
                min_steps = int(parts[5])
            except (ValueError, IndexError):
                min_steps = 1
            why = []
            if method != "cashu":
                why.append(f"method={method!r}")
            if unit not in ("sat", "sats"):
                why.append(f"unit={unit!r}")
            if price <= 0:
                why.append(f"price={price}")
            if min_steps <= 0:
                why.append(f"min_steps={min_steps}")
            if not mint:
                why.append("empty mint")
            if why:
                problems.append(f"option {mint or '?'}: REJECTED ({', '.join(why)})")
            else:
                pricing.append({"price": price, "unit": unit, "mint": mint, "min_steps": min_steps})
    if metric not in ("milliseconds", "bytes"):
        problems.append(f"metric={metric!r}")
    if step_size == 0:
        problems.append("step_size=0")
    if not pricing:
        problems.append("NO VALID Cashu/sat pricing option (cashud error)")
    return pricing, problems


def main():
    source = sys.argv[1] if len(sys.argv) > 1 else DEFAULT
    ad = load(source)
    pricing, problems = validate(ad)
    print(f"ad from {source}")
    print(f"  valid options: {len(pricing)}")
    for p in pricing:
        print(f"    OK  {p['mint']}  {p['price']} {p['unit']}/step  min_steps={p['min_steps']}")
    for p in problems:
        print(f"    !!  {p}")
    sys.exit(0 if pricing and not [p for p in problems if p.startswith('option') or 'NO VALID' in p or 'metric' in p or 'step_size' in p or 'kind' in p or 'pubkey' in p] else 1)


if __name__ == "__main__":
    main()
