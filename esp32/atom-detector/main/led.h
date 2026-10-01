#pragma once
#include "detector.h"

// SK6812 RGB LED on the M5 Atom (GPIO27), driven via the led_strip
// component. tg_led_set publishes a mode; a background task renders
// the animation (pulse/blink) so callers never block.

void tg_led_init(void);
void tg_led_set(tg_led_mode_t mode);
