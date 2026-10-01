#pragma once
#include "esp_err.h"
#include "esp_wifi.h"
#include "esp_event.h"
#include "lwip/ip4_addr.h"

typedef struct {
    ip4_addr_t ip;
    ip4_addr_t netmask;
    ip4_addr_t gw;
    bool connected;
} tg_wifi_state_t;

// Initialize WiFi in STA mode and event loop.
esp_err_t tg_wifi_init(void);

// Join an open network (no WPA). Blocks until got IP or timeout.
esp_err_t tg_wifi_join(const char *ssid, int timeout_secs);

// Get current IP info (only valid after tg_wifi_join returns ESP_OK).
esp_err_t tg_wifi_get_ip(tg_wifi_state_t *state);

// Disconnect and deinit.
void tg_wifi_disconnect(void);

// --- detector additions ---

typedef struct {
    char ssid[33];
    uint8_t bssid[6];
    int8_t rssi;
    wifi_auth_mode_t authmode;
    uint8_t channel;
} tg_scan_ap_t;

// Restart the STA after a disconnect (tg_wifi_disconnect stops the radio).
esp_err_t tg_wifi_resume(void);

// Blocking scan (all channels). Returns record count, or -1 on error.
int tg_wifi_scan(tg_scan_ap_t *aps, int max);

// Candidate predicate: SSID matches "TollGate" prefix (or the exact
// wanted SSID when configured) and the network is open.
bool tg_wifi_is_candidate(const char *ssid, wifi_auth_mode_t auth, const char *wanted);
