#include "token_store.h"
#include "nvs_flash.h"
#include "nvs.h"
#include "esp_log.h"
#include <string.h>

static const char *TAG = "tg_store";
static const char *NS = "tollgate";
static const char *KEY_SSID = "ssid";
static const char *KEY_TOKEN = "token";
static const char *KEY_MINT = "mint";

esp_err_t tg_store_load(tg_config_t *cfg) {
    memset(cfg, 0, sizeof(*cfg));
    nvs_handle_t h;
    esp_err_t err = nvs_open(NS, NVS_READONLY, &h);
    if (err != ESP_OK) {
        ESP_LOGI(TAG, "no stored config (first boot?)");
        return ESP_OK;
    }
    size_t len;
    len = sizeof(cfg->ssid);
    nvs_get_str(h, KEY_SSID, cfg->ssid, &len);
    len = sizeof(cfg->token);
    nvs_get_str(h, KEY_TOKEN, cfg->token, &len);
    len = sizeof(cfg->mint_url);
    nvs_get_str(h, KEY_MINT, cfg->mint_url, &len);
    nvs_close(h);
    ESP_LOGI(TAG, "loaded: ssid=%s token=%d chars mint=%s",
             cfg->ssid, (int)strlen(cfg->token), cfg->mint_url);
    return ESP_OK;
}

esp_err_t tg_store_save(const tg_config_t *cfg) {
    nvs_handle_t h;
    ESP_ERROR_CHECK(nvs_open(NS, NVS_READWRITE, &h));
    if (cfg->ssid[0])
        ESP_ERROR_CHECK(nvs_set_str(h, KEY_SSID, cfg->ssid));
    if (cfg->token[0])
        ESP_ERROR_CHECK(nvs_set_str(h, KEY_TOKEN, cfg->token));
    if (cfg->mint_url[0])
        ESP_ERROR_CHECK(nvs_set_str(h, KEY_MINT, cfg->mint_url));
    ESP_ERROR_CHECK(nvs_commit(h));
    nvs_close(h);
    ESP_LOGI(TAG, "config saved");
    return ESP_OK;
}

esp_err_t tg_store_erase(void) {
    nvs_handle_t h;
    esp_err_t err = nvs_open(NS, NVS_READWRITE, &h);
    if (err == ESP_OK) {
        nvs_erase_all(h);
        nvs_commit(h);
        nvs_close(h);
        ESP_LOGI(TAG, "config erased");
    }
    return ESP_OK;
}
