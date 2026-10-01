#pragma once
#include "esp_err.h"

#define TG_TOKEN_MAX 2048
#define TG_SSID_MAX 32

typedef struct {
    char ssid[TG_SSID_MAX];
    char token[TG_TOKEN_MAX];
    char mint_url[256];
} tg_config_t;

// Load config from NVS (returns defaults if not set).
esp_err_t tg_store_load(tg_config_t *cfg);

// Save config to NVS.
esp_err_t tg_store_save(const tg_config_t *cfg);

// Erase stored config.
esp_err_t tg_store_erase(void);
