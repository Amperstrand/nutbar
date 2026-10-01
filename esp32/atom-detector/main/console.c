#include "esp_log.h"
#include "driver/uart.h"
#include <string.h>
#include <stdio.h>

static const char *TAG = "tg_console";
#define RX_BUF_SIZE 4096
static char rx_line[4096];
static int rx_pos = 0;

void tg_console_init(void) {
    uart_config_t cfg = {
        .baud_rate = 115200,
        .data_bits = UART_DATA_8_BITS,
        .parity = UART_PARITY_DISABLE,
        .stop_bits = UART_STOP_BITS_1,
        .flow_ctrl = UART_HW_FLOWCTRL_DISABLE,
        .source_clk = UART_SCLK_DEFAULT,
    };
    uart_driver_install(UART_NUM_0, RX_BUF_SIZE, 0, 0, NULL, 0);
    uart_param_config(UART_NUM_0, &cfg);
}

// Read one line (blocking, strips newline). Returns NULL on timeout.
char *tg_console_readline(int timeout_ms) {
    uint8_t ch;
    rx_pos = 0;
    TickType_t deadline = xTaskGetTickCount() + pdMS_TO_TICKS(timeout_ms);
    while (xTaskGetTickCount() < deadline) {
        int n = uart_read_bytes(UART_NUM_0, &ch, 1, pdMS_TO_TICKS(50));
        if (n == 1) {
            if (ch == '\n' || ch == '\r') {
                if (rx_pos > 0) {
                    rx_line[rx_pos] = 0;
                    return rx_line;
                }
            } else if (rx_pos < (int)sizeof(rx_line) - 1) {
                rx_line[rx_pos++] = ch;
            }
        }
    }
    return NULL;
}

void tg_console_write(const char *s) {
    uart_write_bytes(UART_NUM_0, s, strlen(s));
    uart_write_bytes(UART_NUM_0, "\r\n", 2);
}
