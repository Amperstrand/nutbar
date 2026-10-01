#pragma once
#include <stdint.h>
#include <stdbool.h>
#include "esp_err.h"

#define TG_MAX_MINTS 8
#define TG_URL_MAX 256

typedef struct {
    char mint_url[TG_URL_MAX];
    uint64_t price_per_step;
    char unit[8];          // "sat" or "sats"
    uint64_t min_steps;
    bool valid;
} tg_pricing_t;

typedef struct {
    char metric[16];       // "milliseconds" or "bytes"
    uint64_t step_size;
    tg_pricing_t pricing[TG_MAX_MINTS];
    int pricing_count;
    char error[128];
} tg_advertisement_t;

// Parse a kind-10021 advertisement JSON body.
// Populates ad->pricing[] with entries where valid=true (price>0, min_steps>=1,
// method=="cashu", unit is "sat" or "sats"). Returns ESP_OK if at least one
// valid pricing option exist; ESP_ERR_INVALID_STATE otherwise (ad->error set).
esp_err_t tg_parse_advertisement(const char *json_body, tg_advertisement_t *ad);

// Select the cheapest valid pricing for a given mint URL (or any mint if
// mint_url is NULL). Returns NULL if none match.
const tg_pricing_t *tg_select_pricing(const tg_advertisement_t *ad, const char *mint_url);

// Compute the payment cost in sats: price_per_step * min_steps
uint64_t tg_payment_cost(const tg_pricing_t *p);
