#pragma once
#include <stdbool.h>

// Detector state machine (pure decision core — mirrored by
// tests/test_detector_sm.py; keep both in lockstep).

typedef enum {
    TG_DET_SCAN = 0,     // scanning for TollGate APs
    TG_DET_JOIN,         // joining the found AP
    TG_DET_VALIDATE,     // fetching + validating the kind-10021 ad
    TG_DET_PAY,          // posting the Cashu token (only when configured)
    TG_DET_MONITOR,      // connected, session/usage polled
    TG_DET_ERROR_WAIT,   // error shown, waiting out the backoff
} tg_det_state_t;

typedef enum {
    TG_EV_SCAN_DUE = 0,  // 30s timer fired (or boot)
    TG_EV_FORCED_RESCAN, // button short-press or `go`
    TG_EV_FOUND,         // candidate AP selected
    TG_EV_NONE_FOUND,    // scan produced no candidate
    TG_EV_JOIN_OK,
    TG_EV_JOIN_FAIL,
    TG_EV_AD_VALID,
    TG_EV_AD_INVALID,
    TG_EV_PAY_OK,
    TG_EV_PAY_FAIL,
    TG_EV_SESSION_LOST,  // /usage gone, AP dropped, or disconnect
} tg_det_event_t;

#define TG_DET_SCAN_PERIOD_SECS   30
#define TG_DET_ERROR_BACKOFF_SECS 30
#define TG_DET_MONITOR_POLL_SECS  10

// Next state for (state, event). have_token gates PAY: without a stored
// token an AD_VALID goes straight to MONITOR (monitor-only mode).
tg_det_state_t tg_det_next(tg_det_state_t s, tg_det_event_t e, bool have_token);

// LED mode for a state (see led.h for the color encoding).
typedef enum {
    TG_LED_SCANNING = 0,   // red solid
    TG_LED_VALIDATING,     // yellow dim
    TG_LED_PAYING,         // blue blink
    TG_LED_SESSION,        // green pulse
    TG_LED_ERROR,          // red fast blink
    TG_LED_OFF,
} tg_led_mode_t;

tg_led_mode_t tg_det_led(tg_det_state_t s);

// Serial tag for an event (line protocol consumed by PRTA).
const char *tg_det_tag(tg_det_event_t e);
