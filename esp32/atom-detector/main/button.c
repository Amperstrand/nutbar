#include "button.h"
#include "driver/gpio.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"

#define TG_BTN_GPIO      39   // M5 Atom button
#define TG_BTN_DEBOUNCE_MS 20
#define TG_BTN_LONG_MS   3000

static bool s_down = false;
static int64_t s_down_since = 0;
static bool s_long_fired = false;

void tg_button_init(void) {
    gpio_config_t io = {
        .pin_bit_mask = 1ULL << TG_BTN_GPIO,
        .mode = GPIO_MODE_INPUT,
        .pull_up_en = GPIO_PULLUP_ENABLE,
        .intr_type = GPIO_INTR_DISABLE,
    };
    gpio_config(&io);
}

tg_btn_event_t tg_button_poll(void) {
    int level = gpio_get_level(TG_BTN_GPIO);
    if (level == 0 && !s_down) {          // press edge
        s_down = true;
        s_down_since = (int64_t)xTaskGetTickCount() * portTICK_PERIOD_MS;
        s_long_fired = false;
    } else if (level == 0 && s_down && !s_long_fired) {
        int64_t held = (int64_t)xTaskGetTickCount() * portTICK_PERIOD_MS - s_down_since;
        if (held >= TG_BTN_LONG_MS) {
            s_long_fired = true;
            return TG_BTN_LONG;           // fires while held
        }
    } else if (level == 1 && s_down) {    // release edge
        s_down = false;
        if (!s_long_fired) {
            int64_t held = (int64_t)xTaskGetTickCount() * portTICK_PERIOD_MS - s_down_since;
            if (held >= TG_BTN_DEBOUNCE_MS) return TG_BTN_SHORT;
        }
    }
    return TG_BTN_NONE;
}
