#include "detector.h"

tg_det_state_t tg_det_next(tg_det_state_t s, tg_det_event_t e, bool have_token) {
    switch (e) {
    case TG_EV_SCAN_DUE:
    case TG_EV_FORCED_RESCAN:
        return TG_DET_SCAN;
    case TG_EV_FOUND:
        return (s == TG_DET_SCAN) ? TG_DET_JOIN : s;
    case TG_EV_NONE_FOUND:
        return TG_DET_SCAN;
    case TG_EV_JOIN_OK:
        return TG_DET_VALIDATE;
    case TG_EV_JOIN_FAIL:
        return TG_DET_ERROR_WAIT;
    case TG_EV_AD_VALID:
        return have_token ? TG_DET_PAY : TG_DET_MONITOR;
    case TG_EV_AD_INVALID:
        return TG_DET_ERROR_WAIT;
    case TG_EV_PAY_OK:
        return TG_DET_MONITOR;
    case TG_EV_PAY_FAIL:
        return TG_DET_ERROR_WAIT;
    case TG_EV_SESSION_LOST:
        return TG_DET_SCAN;
    }
    return TG_DET_SCAN;
}

tg_led_mode_t tg_det_led(tg_det_state_t s) {
    switch (s) {
    case TG_DET_SCAN:     return TG_LED_SCANNING;
    case TG_DET_JOIN:     return TG_LED_SCANNING;
    case TG_DET_VALIDATE: return TG_LED_VALIDATING;
    case TG_DET_PAY:      return TG_LED_PAYING;
    case TG_DET_MONITOR:  return TG_LED_SESSION;
    case TG_DET_ERROR_WAIT: return TG_LED_ERROR;
    }
    return TG_LED_OFF;
}

const char *tg_det_tag(tg_det_event_t e) {
    switch (e) {
    case TG_EV_SCAN_DUE:     return "TG_SCAN_START";
    case TG_EV_FORCED_RESCAN: return "TG_SCAN_START";
    case TG_EV_FOUND:        return "TG_FOUND";
    case TG_EV_NONE_FOUND:   return "TG_SCAN_START";
    case TG_EV_JOIN_OK:      return "TG_CONNECTED";
    case TG_EV_JOIN_FAIL:    return "TG_DISCONNECTED";
    case TG_EV_AD_VALID:     return "TG_AD_VALID";
    case TG_EV_AD_INVALID:   return "TG_AD_INVALID";
    case TG_EV_PAY_OK:       return "TG_PAID";
    case TG_EV_PAY_FAIL:     return "TG_DISCONNECTED";
    case TG_EV_SESSION_LOST: return "TG_DISCONNECTED";
    }
    return "TG_SCAN_START";
}
