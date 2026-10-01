#include "wifi_client.h"
#include <string.h>
#include "esp_log.h"
#include "freertos/FreeRTOS.h"
#include "freertos/event_groups.h"
#include "nvs_flash.h"

static const char *TAG = "tg_wifi";
static EventGroupHandle_t s_wifi_event;
#define WIFI_CONNECTED_BIT BIT0
#define WIFI_FAIL_BIT BIT1
#define WIFI_GOT_IP_BIT BIT2
static int s_retry_count = 0;
static tg_wifi_state_t s_state = {0};

static void event_handler(void *arg, esp_event_base_t base, int32_t id, void *data) {
    if (base == WIFI_EVENT && id == WIFI_EVENT_STA_START) {
        esp_wifi_connect();
    } else if (base == WIFI_EVENT && id == WIFI_EVENT_STA_DISCONNECTED) {
        if (s_retry_count < 10) {
            esp_wifi_connect();
            s_retry_count++;
            ESP_LOGI(TAG, "retry %d", s_retry_count);
        } else {
            xEventGroupSetBits(s_wifi_event, WIFI_FAIL_BIT);
        }
        s_state.connected = false;
    } else if (base == IP_EVENT && id == IP_EVENT_STA_GOT_IP) {
        ip_event_got_ip_t *e = (ip_event_got_ip_t *)data;
        s_state.ip.addr = e->ip_info.ip.addr;
        s_state.netmask.addr = e->ip_info.netmask.addr;
        s_state.gw.addr = e->ip_info.gw.addr;
        s_state.connected = true;
        ESP_LOGI(TAG, "got ip " IPSTR " gw " IPSTR,
                 IP2STR(&e->ip_info.ip), IP2STR(&e->ip_info.gw));
        xEventGroupSetBits(s_wifi_event, WIFI_GOT_IP_BIT);
    }
}

esp_err_t tg_wifi_init(void) {
    ESP_ERROR_CHECK(nvs_flash_init());
    s_wifi_event = xEventGroupCreate();
    ESP_ERROR_CHECK(esp_netif_init());
    ESP_ERROR_CHECK(esp_event_loop_create_default());
    esp_netif_create_default_wifi_sta();
    wifi_init_config_t cfg = WIFI_INIT_CONFIG_DEFAULT();
    ESP_ERROR_CHECK(esp_wifi_init(&cfg));
    ESP_ERROR_CHECK(esp_event_handler_register(WIFI_EVENT, ESP_EVENT_ANY_ID, &event_handler, NULL));
    ESP_ERROR_CHECK(esp_event_handler_register(IP_EVENT, IP_EVENT_STA_GOT_IP, &event_handler, NULL));
    ESP_ERROR_CHECK(esp_wifi_set_mode(WIFI_MODE_STA));
    ESP_ERROR_CHECK(esp_wifi_start());
    return ESP_OK;
}

esp_err_t tg_wifi_join(const char *ssid, int timeout_secs) {
    wifi_config_t cfg = {0};
    strlcpy((char *)cfg.sta.ssid, ssid, sizeof(cfg.sta.ssid));
    cfg.sta.channel = 0; // any
    cfg.sta.password[0] = 0; // open
    cfg.sta.scan_method = WIFI_FAST_SCAN;
    cfg.sta.sort_method = WIFI_CONNECT_AP_BY_SIGNAL;

    s_retry_count = 0;
    xEventGroupClearBits(s_wifi_event, WIFI_CONNECTED_BIT | WIFI_FAIL_BIT | WIFI_GOT_IP_BIT);
    ESP_ERROR_CHECK(esp_wifi_set_config(WIFI_IF_STA, &cfg));
    ESP_ERROR_CHECK(esp_wifi_connect());

    EventBits_t bits = xEventGroupWaitBits(s_wifi_event,
        WIFI_GOT_IP_BIT | WIFI_FAIL_BIT, pdFALSE, pdFALSE,
        pdMS_TO_TICKS(timeout_secs * 1000));

    if (bits & WIFI_GOT_IP_BIT) {
        ESP_LOGI(TAG, "connected to %s", ssid);
        return ESP_OK;
    }
    ESP_LOGE(TAG, "failed to connect to %s", ssid);
    return ESP_ERR_TIMEOUT;
}

esp_err_t tg_wifi_get_ip(tg_wifi_state_t *state) {
    if (!s_state.connected) return ESP_ERR_INVALID_STATE;
    *state = s_state;
    return ESP_OK;
}

void tg_wifi_disconnect(void) {
    esp_wifi_disconnect();
    esp_wifi_stop();
    s_state.connected = false;
}

// --- detector additions ---

esp_err_t tg_wifi_resume(void) {
    if (s_state.connected) return ESP_OK;
    // esp_wifi_stop() in tg_wifi_disconnect requires a fresh start
    // before scanning or connecting again.
    return esp_wifi_start();
}

int tg_wifi_scan(tg_scan_ap_t *aps, int max) {
    enum { TG_SCAN_CAP = 20 };
    if (max > TG_SCAN_CAP) max = TG_SCAN_CAP;
    wifi_scan_config_t cfg = { .show_hidden = false };
    esp_err_t err = esp_wifi_scan_start(&cfg, true);
    if (err != ESP_OK) return -1;
    uint16_t n = 0;
    esp_wifi_scan_get_ap_num(&n);
    if (n > (uint16_t)max) n = (uint16_t)max;
    static wifi_ap_record_t recs[TG_SCAN_CAP];
    esp_wifi_scan_get_ap_records(&n, recs);
    for (int i = 0; i < (int)n; i++) {
        memcpy(aps[i].ssid, recs[i].ssid, 32);
        aps[i].ssid[32] = 0;
        memcpy(aps[i].bssid, recs[i].bssid, 6);
        aps[i].rssi = recs[i].rssi;
        aps[i].authmode = recs[i].authmode;
        aps[i].channel = recs[i].primary;
    }
    return (int)n;
}

bool tg_wifi_is_candidate(const char *ssid, wifi_auth_mode_t auth, const char *wanted) {
    if (auth != WIFI_AUTH_OPEN) return false;
    if (wanted && wanted[0]) return strcmp(ssid, wanted) == 0;
    return strncmp(ssid, "TollGate", strlen("TollGate")) == 0;
}
