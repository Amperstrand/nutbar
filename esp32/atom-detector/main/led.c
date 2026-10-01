#include "led.h"
#include "led_strip.h"
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"

#define TG_LED_GPIO      27   // M5 Atom SK6812 data
#define TG_LED_RMT_CH    0
#define TG_LED_TICK_MS   50

static led_strip_handle_t s_strip;
static volatile tg_led_mode_t s_mode = TG_LED_OFF;

static void render_task(void *arg);

static void put(uint8_t r, uint8_t g, uint8_t b) {
    led_strip_set_pixel(s_strip, 0, r, g, b);
    led_strip_refresh(s_strip);
}

void tg_led_init(void) {
    led_strip_config_t cfg = {
        .strip_gpio_num = TG_LED_GPIO,
        .max_leds = 1,
    };
    led_strip_rmt_config_t rmt = {
        .resolution_hz = 10 * 1000 * 1000,
        .mem_block_symbols = 64,
    };
    led_strip_new_rmt_device(&cfg, &rmt, &s_strip);
    led_strip_clear(s_strip);
    xTaskCreate(render_task, "led", 2048, NULL, 3, NULL);
}

void tg_led_set(tg_led_mode_t mode) {
    s_mode = mode;
}

static void render_task(void *arg) {
    int t = 0;
    for (;;) {
        switch (s_mode) {
        case TG_LED_SCANNING:
            put(32, 0, 0);              // red solid
            break;
        case TG_LED_VALIDATING:
            put(16, 16, 0);             // yellow dim
            break;
        case TG_LED_PAYING:
            put(0, 0, (t / 5) % 2 ? 64 : 0);   // blue blink 250ms
            break;
        case TG_LED_SESSION: {
            int wave = t % 40;          // green pulse ~2s
            int bright = wave < 20 ? wave : 40 - wave;   // 0..20 ramp
            put(0, 16 + bright * 3, 0);
            break;
        }
        case TG_LED_ERROR:
            put((t / 2) % 2 ? 64 : 0, 0, 0);   // red fast blink 100ms
            break;
        case TG_LED_OFF:
        default:
            put(0, 0, 0);
            break;
        }
        t++;
        vTaskDelay(pdMS_TO_TICKS(TG_LED_TICK_MS));
    }
}
