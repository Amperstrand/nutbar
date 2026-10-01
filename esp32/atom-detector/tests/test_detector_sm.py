#!/usr/bin/env python3
"""State-machine tests for the M5 Atom TollGate detector.

Mirrors the C decision core in main/detector.c (tg_det_next, tg_det_led,
tg_det_tag) and the scan candidate predicate (tg_wifi_is_candidate).
Keep both in lockstep: a change here must be reflected in the C and
vice versa.
"""
import unittest

SCAN, JOIN, VALIDATE, PAY, MONITOR, ERROR_WAIT = range(6)

SCAN_DUE, FORCED_RESCAN, FOUND, NONE_FOUND, JOIN_OK, JOIN_FAIL, \
    AD_VALID, AD_INVALID, PAY_OK, PAY_FAIL, SESSION_LOST = range(11)


def det_next(s, e, have_token):
    if e in (SCAN_DUE, FORCED_RESCAN):
        return SCAN
    if e == FOUND:
        return JOIN if s == SCAN else s
    if e == NONE_FOUND:
        return SCAN
    if e == JOIN_OK:
        return VALIDATE
    if e == JOIN_FAIL:
        return ERROR_WAIT
    if e == AD_VALID:
        return PAY if have_token else MONITOR
    if e == AD_INVALID:
        return ERROR_WAIT
    if e == PAY_OK:
        return MONITOR
    if e == PAY_FAIL:
        return ERROR_WAIT
    if e == SESSION_LOST:
        return SCAN
    return SCAN


def det_led(s):
    return {
        SCAN: "red", JOIN: "red", VALIDATE: "yellow-dim", PAY: "blue-blink",
        MONITOR: "green-pulse", ERROR_WAIT: "red-fast",
    }[s]


def det_tag(e):
    return {
        SCAN_DUE: "TG_SCAN_START", FORCED_RESCAN: "TG_SCAN_START",
        FOUND: "TG_FOUND", NONE_FOUND: "TG_SCAN_START",
        JOIN_OK: "TG_CONNECTED", JOIN_FAIL: "TG_DISCONNECTED",
        AD_VALID: "TG_AD_VALID", AD_INVALID: "TG_AD_INVALID",
        PAY_OK: "TG_PAID", PAY_FAIL: "TG_DISCONNECTED",
        SESSION_LOST: "TG_DISCONNECTED",
    }[e]


def is_candidate(ssid, auth, wanted=None):
    if auth != "open":
        return False
    if wanted:
        return ssid == wanted
    return ssid.startswith("TollGate")


class Transitions(unittest.TestCase):
    def test_happy_path_paying(self):
        path = [(SCAN_DUE, SCAN), (FOUND, JOIN), (JOIN_OK, VALIDATE),
                (AD_VALID, PAY), (PAY_OK, MONITOR)]
        s = SCAN
        for e, want in path:
            s = det_next(s, e, have_token=True)
            self.assertEqual(s, want)

    def test_happy_path_monitor_only_skips_pay(self):
        # No token: valid ad goes straight to monitor.
        s = det_next(VALIDATE, AD_VALID, have_token=False)
        self.assertEqual(s, MONITOR)
        self.assertEqual(det_led(s), "green-pulse")

    def test_session_loss_restarts_scan(self):
        self.assertEqual(det_next(MONITOR, SESSION_LOST, True), SCAN)
        self.assertEqual(det_tag(SESSION_LOST), "TG_DISCONNECTED")

    def test_error_backoff_then_rescan(self):
        for e in (JOIN_FAIL, AD_INVALID, PAY_FAIL):
            self.assertEqual(det_next(JOIN, e, True), ERROR_WAIT)
        self.assertEqual(det_led(ERROR_WAIT), "red-fast")
        self.assertEqual(det_next(ERROR_WAIT, SCAN_DUE, True), SCAN)

    def test_none_found_stays_scanning(self):
        self.assertEqual(det_next(SCAN, NONE_FOUND, False), SCAN)
        self.assertEqual(det_led(SCAN), "red")

    def test_forced_rescan_always_returns_to_scan(self):
        for s in (JOIN, VALIDATE, PAY, MONITOR, ERROR_WAIT):
            self.assertEqual(det_next(s, FORCED_RESCAN, True), SCAN)

    def test_found_only_matters_while_scanning(self):
        # A stray FOUND in another state is ignored.
        self.assertEqual(det_next(MONITOR, FOUND, True), MONITOR)

    def test_led_covers_every_state(self):
        for s in (SCAN, JOIN, VALIDATE, PAY, MONITOR, ERROR_WAIT):
            self.assertTrue(det_led(s))


class Candidates(unittest.TestCase):
    def test_prefix_and_open_required(self):
        self.assertTrue(is_candidate("TollGate-326D", "open"))
        self.assertFalse(is_candidate("TollGate-326D", "wpa2"))
        self.assertFalse(is_candidate("HomeWifi", "open"))
        self.assertFalse(is_candidate("tollgate-lower", "open"))  # case-sensitive

    def test_wanted_ssid_locks_exact_match(self):
        self.assertTrue(is_candidate("TollGate-326D", "open", "TollGate-326D"))
        self.assertFalse(is_candidate("TollGate-9999", "open", "TollGate-326D"))

    def test_strongest_signal_wins(self):
        aps = [("TollGate-A", -80), ("TollGate-B", -55), ("Other", -10)]
        pick = max((a for a in aps if is_candidate(a[0], "open")), key=lambda a: a[1])
        self.assertEqual(pick[0], "TollGate-B")


class Tags(unittest.TestCase):
    def test_tag_format_strings(self):
        self.assertEqual(det_tag(FOUND), "TG_FOUND")
        self.assertEqual(det_tag(AD_VALID), "TG_AD_VALID")
        self.assertEqual(det_tag(AD_INVALID), "TG_AD_INVALID")
        self.assertEqual(det_tag(JOIN_OK), "TG_CONNECTED")
        self.assertEqual(det_tag(PAY_OK), "TG_PAID")
        self.assertEqual(det_tag(PAY_FAIL), "TG_DISCONNECTED")

    def test_failures_map_to_disconnected_or_invalid(self):
        # PRTA distinguishes error kinds: transport failures become
        # TG_DISCONNECTED, ad problems keep their own tag.
        for e in (JOIN_FAIL, PAY_FAIL, SESSION_LOST):
            self.assertEqual(det_tag(e), "TG_DISCONNECTED")
        self.assertEqual(det_tag(AD_INVALID), "TG_AD_INVALID")


if __name__ == "__main__":
    unittest.main()
