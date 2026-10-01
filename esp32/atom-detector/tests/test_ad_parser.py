#!/usr/bin/env python3
"""Port of the M5StickC TollGate client's C ad parser for CI verification.
Mirrors main/ad_parser.c — same predicates, same rejection rules.
Run: python3 tests/test_ad_parser.py"""
import json
import sys
import os

sys.path.insert(0, os.path.dirname(__file__))

MAX_MINTS = 8


class Pricing:
    def __init__(self, mint_url, price_per_step, unit, min_steps):
        self.mint_url = mint_url
        self.price_per_step = price_per_step
        self.unit = unit
        self.min_steps = min_steps


def parse_advertisement(body):
    """Returns (metric, step_size, [Pricing]) or raises ValueError."""
    if not body:
        raise ValueError("empty ad body")
    ad = json.loads(body)
    if ad.get("kind") != 10021:
        raise ValueError("not kind 10021")
    tags = ad.get("tags", [])

    metric = ""
    step_size = 0
    pricing = []
    error = "advertisement has no valid Cashu/sat pricing option"

    for tag in tags:
        if not isinstance(tag, list) or len(tag) < 2:
            continue
        name = tag[0]
        if name == "metric":
            metric = tag[1]
        elif name == "step_size":
            step_size = int(tag[1])
        elif name == "price_per_step" and len(tag) >= 6:
            method, price, unit, mint_url, min_steps = tag[1], tag[2], tag[3], tag[4], tag[5]
            if method != "cashu":
                continue
            if not isinstance(price, (int, float)) or price <= 0:
                continue
            if unit not in ("sat", "sats"):
                continue
            if not isinstance(min_steps, (int, float)) or min_steps < 1:
                continue
            if len(pricing) >= MAX_MINTS:
                break
            pricing.append(Pricing(mint_url, int(price), unit, int(min_steps)))

    if not pricing:
        raise ValueError(error)
    return metric, step_size, pricing


def select_pricing(pricing, mint_url=None):
    best = None
    for p in pricing:
        if mint_url and p.mint_url != mint_url:
            continue
        if best is None or p.price_per_step < best.price_per_step:
            best = p
    return best


def payment_cost(p):
    return p.price_per_step * p.min_steps if p else 0


# --- tests ---

def test_valid_ad():
    ad = json.dumps({"kind": 10021, "tags": [
        ["metric", "milliseconds"], ["step_size", 60000],
        ["price_per_step", "cashu", 1, "sat", "https://mint.example", 1],
    ]})
    m, ss, p = parse_advertisement(ad)
    assert m == "milliseconds"
    assert ss == 60000
    assert len(p) == 1
    assert p[0].price_per_step == 1
    assert p[0].min_steps == 1
    assert payment_cost(p[0]) == 1


def test_min_steps_zero_rejected():
    ad = json.dumps({"kind": 10021, "tags": [
        ["price_per_step", "cashu", 1, "sat", "https://mint.example", 0],
    ]})
    try:
        parse_advertisement(ad)
        assert False, "should reject min_steps=0"
    except ValueError as e:
        assert "no valid" in str(e)


def test_min_steps_negative_rejected():
    ad = json.dumps({"kind": 10021, "tags": [
        ["price_per_step", "cashu", 1, "sat", "https://mint.example", -1],
    ]})
    try:
        parse_advertisement(ad)
        assert False, "should reject min_steps<0"
    except ValueError:
        pass


def test_price_zero_rejected():
    ad = json.dumps({"kind": 10021, "tags": [
        ["price_per_step", "cashu", 0, "sat", "https://mint.example", 1],
    ]})
    try:
        parse_advertisement(ad)
        assert False, "should reject price=0"
    except ValueError:
        pass


def test_wrong_method_rejected():
    ad = json.dumps({"kind": 10021, "tags": [
        ["price_per_step", "ln", 1, "sat", "https://mint.example", 1],
    ]})
    try:
        parse_advertisement(ad)
        assert False, "should reject non-cashu method"
    except ValueError:
        pass


def test_wrong_unit_rejected():
    ad = json.dumps({"kind": 10021, "tags": [
        ["price_per_step", "cashu", 1, "usd", "https://mint.example", 1],
    ]})
    try:
        parse_advertisement(ad)
        assert False, "should reject non-sat unit"
    except ValueError:
        pass


def test_sats_unit_accepted():
    ad = json.dumps({"kind": 10021, "tags": [
        ["price_per_step", "cashu", 2, "sats", "https://mint.example", 3],
    ]})
    _, _, p = parse_advertisement(ad)
    assert p[0].unit == "sats"
    assert payment_cost(p[0]) == 6


def test_wrong_kind_rejected():
    ad = json.dumps({"kind": 1022, "tags": []})
    try:
        parse_advertisement(ad)
        assert False, "should reject kind != 10021"
    except ValueError:
        pass


def test_empty_body_rejected():
    try:
        parse_advertisement("")
        assert False
    except ValueError:
        pass


def test_mixed_valid_invalid():
    ad = json.dumps({"kind": 10021, "tags": [
        ["price_per_step", "cashu", 1, "sat", "https://bad", 0],
        ["price_per_step", "cashu", 3, "sat", "https://good", 2],
        ["price_per_step", "ln", 1, "sat", "https://wrong-method", 1],
    ]})
    _, _, p = parse_advertisement(ad)
    assert len(p) == 1
    assert p[0].mint_url == "https://good"
    assert payment_cost(p[0]) == 6


def test_select_by_mint():
    ad = json.dumps({"kind": 10021, "tags": [
        ["price_per_step", "cashu", 5, "sat", "https://a.example", 1],
        ["price_per_step", "cashu", 2, "sat", "https://b.example", 1],
    ]})
    _, _, p = parse_advertisement(ad)
    sel = select_pricing(p, "https://b.example")
    assert sel.price_per_step == 2
    sel_any = select_pricing(p)
    assert sel_any.price_per_step == 2  # cheapest wins


def test_hardware_fixed_ad():
    # The actual ad shape from the MT3000 after the min-steps fix
    ad = json.dumps({"kind": 10021, "tags": [
        ["metric", "bytes"], ["step_size", 22020096],
        ["price_per_step", "cashu", 1, "sat", "https://testnut.cashu.space", 1],
        ["price_per_step", "cashu", 1, "sat", "https://mint.coinos.io", 1],
        ["price_per_step", "cashu", 1, "sat", "https://mint.minibits.cash/Bitcoin", 1],
    ]})
    m, ss, p = parse_advertisement(ad)
    assert m == "bytes"
    assert ss == 22020096
    assert len(p) == 3
    assert payment_cost(p[0]) == 1


def test_pre_fix_all_zero():
    # The original bug: every mint emitted min_steps=0
    ad = json.dumps({"kind": 10021, "tags": [
        ["price_per_step", "cashu", 1, "sat", "https://a", 0],
        ["price_per_step", "cashu", 1, "sat", "https://b", 0],
    ]})
    try:
        parse_advertisement(ad)
        assert False, "should reject all-zero min_steps"
    except ValueError as e:
        assert "no valid" in str(e)


if __name__ == "__main__":
    tests = [v for k, v in sorted(globals().items()) if k.startswith("test_")]
    passed = 0
    for t in tests:
        try:
            t()
            print(f"  PASS {t.__name__}")
            passed += 1
        except Exception as e:
            print(f"  FAIL {t.__name__}: {e}")
    print(f"\n{passed}/{len(tests)} passed")
    sys.exit(0 if passed == len(tests) else 1)
