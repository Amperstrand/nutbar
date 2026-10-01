#pragma once

// M5 Atom button (GPIO39, active-low, internal pullup).
// Short press (<1s): force rescan. Long press (>=3s): erase config.

typedef enum {
    TG_BTN_NONE = 0,
    TG_BTN_SHORT,
    TG_BTN_LONG,
} tg_btn_event_t;

void tg_button_init(void);

// Poll the debounced button (non-blocking). Returns at most one event
// per completed press.
tg_btn_event_t tg_button_poll(void);
