#include "ad_parser.h"
#include "cJSON.h"
#include <string.h>
#include <stdio.h>

esp_err_t tg_parse_advertisement(const char *json_body, tg_advertisement_t *ad) {
    memset(ad, 0, sizeof(*ad));
    if (!json_body || !*json_body) {
        snprintf(ad->error, sizeof(ad->error), "empty ad body");
        return ESP_ERR_INVALID_ARG;
    }

    cJSON *root = cJSON_Parse(json_body);
    if (!root) {
        snprintf(ad->error, sizeof(ad->error), "JSON parse failed");
        return ESP_ERR_INVALID_STATE;
    }

    // must be kind 10021
    cJSON *kind = cJSON_GetObjectItem(root, "kind");
    if (!kind || !cJSON_IsNumber(kind) || (int)kind->valuedouble != 10021) {
        snprintf(ad->error, sizeof(ad->error), "not kind 10021");
        cJSON_Delete(root);
        return ESP_ERR_INVALID_STATE;
    }

    cJSON *tags = cJSON_GetObjectItem(root, "tags");
    if (!tags || !cJSON_IsArray(tags)) {
        snprintf(ad->error, sizeof(ad->error), "no tags array");
        cJSON_Delete(root);
        return ESP_ERR_INVALID_STATE;
    }

    cJSON *tag;
    cJSON_ArrayForEach(tag, tags) {
        if (!cJSON_IsArray(tag) || cJSON_GetArraySize(tag) < 2) continue;
        cJSON *first = cJSON_GetArrayItem(tag, 0);
        if (!first || !cJSON_IsString(first)) continue;

        if (strcmp(first->valuestring, "metric") == 0) {
            cJSON *v = cJSON_GetArrayItem(tag, 1);
            if (v && cJSON_IsString(v))
                snprintf(ad->metric, sizeof(ad->metric), "%s", v->valuestring);
        } else if (strcmp(first->valuestring, "step_size") == 0) {
            cJSON *v = cJSON_GetArrayItem(tag, 1);
            if (v && cJSON_IsNumber(v))
                ad->step_size = (uint64_t)v->valuedouble;
        } else if (strcmp(first->valuestring, "price_per_step") == 0 &&
                   cJSON_GetArraySize(tag) >= 6) {
            // ["price_per_step", method, price, unit, mint_url, min_steps]
            cJSON *method = cJSON_GetArrayItem(tag, 1);
            cJSON *price  = cJSON_GetArrayItem(tag, 2);
            cJSON *unit   = cJSON_GetArrayItem(tag, 3);
            cJSON *mint   = cJSON_GetArrayItem(tag, 4);
            cJSON *min_st = cJSON_GetArrayItem(tag, 5);

            if (!method || !cJSON_IsString(method) ||
                strcmp(method->valuestring, "cashu") != 0)
                continue;

            if (!price || !cJSON_IsNumber(price) || price->valuedouble <= 0)
                continue;

            if (!unit || !cJSON_IsString(unit))
                continue;
            const char *u = unit->valuestring;
            if (strcmp(u, "sat") != 0 && strcmp(u, "sats") != 0)
                continue;

            if (!mint || !cJSON_IsString(mint))
                continue;

            // v1 contract: min_steps MUST be >= 1 (a purchase of <1 step is
            // meaningless; gateways emitting 0 have a bug — fixed upstream
            // in Amperstrand/tollgate-module-basic-go fix/min-steps-default-1)
            if (!min_st || !cJSON_IsNumber(min_st) || min_st->valuedouble < 1)
                continue;

            if (ad->pricing_count >= TG_MAX_MINTS)
                break;

            tg_pricing_t *p = &ad->pricing[ad->pricing_count++];
            snprintf(p->mint_url, sizeof(p->mint_url), "%s", mint->valuestring);
            p->price_per_step = (uint64_t)price->valuedouble;
            snprintf(p->unit, sizeof(p->unit), "%s", u);
            p->min_steps = (uint64_t)min_st->valuedouble;
            p->valid = true;
        }
    }

    cJSON_Delete(root);

    if (ad->pricing_count == 0) {
        snprintf(ad->error, sizeof(ad->error),
                 "advertisement has no valid Cashu/sat pricing option");
        return ESP_ERR_INVALID_STATE;
    }
    return ESP_OK;
}

const tg_pricing_t *tg_select_pricing(const tg_advertisement_t *ad, const char *mint_url) {
    const tg_pricing_t *best = NULL;
    for (int i = 0; i < ad->pricing_count; i++) {
        const tg_pricing_t *p = &ad->pricing[i];
        if (!p->valid) continue;
        if (mint_url && strcmp(p->mint_url, mint_url) != 0) continue;
        if (!best || p->price_per_step < best->price_per_step)
            best = p;
    }
    return best;
}

uint64_t tg_payment_cost(const tg_pricing_t *p) {
    return p ? p->price_per_step * p->min_steps : 0;
}
