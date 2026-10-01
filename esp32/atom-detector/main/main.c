#include <string.h>
#include <stdio.h>
#include <stdarg.h>
#include "freertos/FreeRTOS.h"
#include "freertos/task.h"
#include "esp_log.h"
#include "esp_system.h"
#include "nvs_flash.h"

#include "ad_parser.h"
#include "wifi_client.h"
#include "http_client.h"
#include "token_store.h"
#include "detector.h"
#include "led.h"
#include "button.h"

void tg_console_init(void);
char *tg_console_readline(int timeout_ms);
void tg_console_write(const char *s);

static const char *TAG = "atom_detector";
static tg_config_t g_cfg;
static volatile bool g_force_rescan = false;
static tg_det_state_t g_state = TG_DET_SCAN;
static char g_scan_target[33];
static tg_advertisement_t g_ad;
static char g_gw[16];

static void emit(const char *fmt, ...) __attribute__((format(printf, 1, 2)));
static void emit(const char *fmt, ...) {
    char line[256];
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(line, sizeof(line), fmt, ap);
    va_end(ap);
    tg_console_write(line);
}

static void print_banner(void) {
    tg_console_write("");
    emit("=== TollGate Detector Probe (M5 Atom) ===");
    emit("Commands:");
    emit("  ssid <name>      Lock onto one SSID (default: any TollGate*)");
    emit("  token <cashuA..> Set funded Cashu token (enables paying)");
    emit("  mint <url>       Restrict pricing to one mint (optional)");
    emit("  go               Force a rescan now");
    emit("  status           Show config + state");
    emit("  erase            Erase stored config");
    tg_console_write("");
    emit("Auto-scan every %d s. LED: red=scan, yellow=validating,", TG_DET_SCAN_PERIOD_SECS);
    emit("blue blink=paying, green pulse=monitoring, red fast blink=error.");
    emit("Button: short=force rescan, long (3 s)=erase config.");
}

static void cmd_status(void) {
    emit("ssid:   %s", g_cfg.ssid[0] ? g_cfg.ssid : "(any TollGate*)");
    emit("mint:   %s", g_cfg.mint_url[0] ? g_cfg.mint_url : "(any)");
    emit("token:  %s", g_cfg.token[0] ? "(set, hidden) - paying mode" : "(not set) - monitor-only");
    emit("state:  %d", (int)g_state);
}

static int pick_candidate(tg_scan_ap_t *aps, int n, tg_scan_ap_t *out) {
    int best = -1;
    for (int i = 0; i < n; i++) {
        if (!tg_wifi_is_candidate(aps[i].ssid, aps[i].authmode, g_cfg.ssid[0] ? g_cfg.ssid : NULL))
            continue;
        if (best < 0 || aps[i].rssi > aps[best].rssi) best = i;
    }
    if (best < 0) return -1;
    *out = aps[best];
    return best;
}

static tg_det_event_t do_scan(void) {
    emit("%s", tg_det_tag(TG_EV_SCAN_DUE));
    tg_scan_ap_t aps[20];
    int n = tg_wifi_scan(aps, 20);
    tg_scan_ap_t pick;
    if (n <= 0 || pick_candidate(aps, n, &pick) < 0)
        return TG_EV_NONE_FOUND;
    char bssid[18];
    snprintf(bssid, sizeof(bssid), "%02x:%02x:%02x:%02x:%02x:%02x",
             pick.bssid[0], pick.bssid[1], pick.bssid[2],
             pick.bssid[3], pick.bssid[4], pick.bssid[5]);
    emit("%s %s %s %d", tg_det_tag(TG_EV_FOUND), pick.ssid, bssid, pick.rssi);
    strlcpy(g_scan_target, pick.ssid, sizeof(g_scan_target));
    return TG_EV_FOUND;
}

static tg_det_event_t do_join(void) {
    esp_err_t err = tg_wifi_join(g_scan_target, 15);
    if (err != ESP_OK) {
        emit("%s join failed: %s", tg_det_tag(TG_EV_JOIN_FAIL), g_scan_target);
        return TG_EV_JOIN_FAIL;
    }
    tg_wifi_state_t st;
    tg_wifi_get_ip(&st);
    char ipstr[16];
    snprintf(ipstr, sizeof(ipstr), IPSTR, IP2STR(&st.ip));
    snprintf(g_gw, sizeof(g_gw), IPSTR, IP2STR(&st.gw));
    emit("%s %s", tg_det_tag(TG_EV_JOIN_OK), ipstr);
    return TG_EV_JOIN_OK;
}

static tg_det_event_t do_validate(void) {
    char body[TG_HTTP_MAX_RESPONSE];
    int status = tg_http_get(g_gw, 2121, "/", body, sizeof(body));
    if (status != 200) {
        emit("%s ad fetch HTTP %d", tg_det_tag(TG_EV_AD_INVALID), status);
        return TG_EV_AD_INVALID;
    }
    if (tg_parse_advertisement(body, &g_ad) != ESP_OK) {
        emit("%s %s", tg_det_tag(TG_EV_AD_INVALID), g_ad.error);
        return TG_EV_AD_INVALID;
    }
    const tg_pricing_t *p = tg_select_pricing(&g_ad, g_cfg.mint_url[0] ? g_cfg.mint_url : NULL);
    if (!p) {
        emit("%s no pricing matches configured mint", tg_det_tag(TG_EV_AD_INVALID));
        return TG_EV_AD_INVALID;
    }
    emit("%s %s %d %llu", tg_det_tag(TG_EV_AD_VALID), g_gw, g_ad.pricing_count,
         (unsigned long long)p->price_per_step);
    return TG_EV_AD_VALID;
}

static void session_id_from(const char *resp, char *out, int out_len) {
    const char *id = strstr(resp, "\"id\":\"");
    if (id) {
        id += strlen("\"id\":\"");
        int i = 0;
        while (id[i] && id[i] != '"' && i < out_len - 1) { out[i] = id[i]; i++; }
        out[i] = 0;
        return;
    }
    strlcpy(out, "sess", out_len);
}

static tg_det_event_t do_pay(void) {
    const tg_pricing_t *p = tg_select_pricing(&g_ad, g_cfg.mint_url[0] ? g_cfg.mint_url : NULL);
    uint64_t cost = p ? tg_payment_cost(p) : 0;
    char resp[TG_HTTP_MAX_RESPONSE];
    int status = tg_http_post(g_gw, 2121, "/", g_cfg.token, strlen(g_cfg.token),
                              resp, sizeof(resp));
    if (status != 200) {
        emit("%s payment HTTP %d", tg_det_tag(TG_EV_PAY_FAIL), status);
        return TG_EV_PAY_FAIL;
    }
    char sess[20];
    session_id_from(resp, sess, sizeof(sess));
    emit("%s %s %llu", tg_det_tag(TG_EV_PAY_OK), sess, (unsigned long long)cost);
    return TG_EV_PAY_OK;
}

static tg_det_event_t do_monitor(void) {
    int misses = 0;
    int since_internet = 60;
    for (;;) {
        if (g_force_rescan) { g_force_rescan = false; return TG_EV_SESSION_LOST; }
        vTaskDelay(pdMS_TO_TICKS(TG_DET_MONITOR_POLL_SECS * 1000));
        char usage[128];
        int status = tg_http_get(g_gw, 2121, "/usage", usage, sizeof(usage));
        if (status == 200 && usage[0]) {
            misses = 0;
        } else if (++misses >= 3) {
            return TG_EV_SESSION_LOST;
        }
        since_internet += TG_DET_MONITOR_POLL_SECS;
        if (since_internet >= 60) {
            since_internet = 0;
            char body[128];
            int code = tg_http_get("1.1.1.1", 80, "/", body, sizeof(body));
            emit("TG_INTERNET %d", code);
        }
    }
}

static void wait_or_force(int secs) {
    for (int i = 0; i < secs * 10 && !g_force_rescan; i++)
        vTaskDelay(pdMS_TO_TICKS(100));
    g_force_rescan = false;
}

static void detector_task(void *arg) {
    tg_wifi_init();
    tg_det_state_t s = TG_DET_SCAN;
    for (;;) {
        bool have_token = g_cfg.token[0] != 0;
        g_state = s;
        tg_led_set(tg_det_led(s));
        tg_det_event_t ev = TG_EV_SCAN_DUE;
        switch (s) {
        case TG_DET_SCAN:
            tg_wifi_resume();
            ev = do_scan();
            if (ev == TG_EV_NONE_FOUND) {
                emit("(no TollGate in range)");
                wait_or_force(TG_DET_SCAN_PERIOD_SECS);
            }
            break;
        case TG_DET_JOIN:       ev = do_join();      break;
        case TG_DET_VALIDATE:   ev = do_validate();  break;
        case TG_DET_PAY:        ev = do_pay();       break;
        case TG_DET_MONITOR:    ev = do_monitor();   break;
        case TG_DET_ERROR_WAIT:
            wait_or_force(TG_DET_ERROR_BACKOFF_SECS);
            tg_wifi_disconnect();
            ev = TG_EV_SCAN_DUE;
            break;
        }
        if (ev == TG_EV_SESSION_LOST) {
            emit("%s", tg_det_tag(TG_EV_SESSION_LOST));
            tg_wifi_disconnect();
        }
        s = tg_det_next(s, ev, have_token);
    }
}

void app_main(void) {
    ESP_LOGI(TAG, "TollGate detector probe starting");
    tg_led_init();
    tg_button_init();
    tg_console_init();
    print_banner();

    tg_store_load(&g_cfg);
    emit("%s", g_cfg.token[0]
        ? "config loaded: paying mode"
        : "config loaded: monitor-only (set a token to enable paying)");

    xTaskCreate(detector_task, "detector", 8192, NULL, 5, NULL);

    for (;;) {
        char *line = tg_console_readline(100);
        tg_btn_event_t btn = tg_button_poll();
        if (btn == TG_BTN_SHORT) {
            emit("button: rescan");
            g_force_rescan = true;
        } else if (btn == TG_BTN_LONG) {
            tg_store_erase();
            memset(&g_cfg, 0, sizeof(g_cfg));
            emit("button: config erased (monitor-only, any TollGate*)");
        }
        if (!line) continue;
        if (strncmp(line, "ssid ", 5) == 0) {
            strlcpy(g_cfg.ssid, line + 5, sizeof(g_cfg.ssid));
            tg_store_save(&g_cfg);
            g_force_rescan = true;
            emit("ssid locked: %s", g_cfg.ssid);
        } else if (strncmp(line, "token ", 6) == 0) {
            strlcpy(g_cfg.token, line + 6, sizeof(g_cfg.token));
            tg_store_save(&g_cfg);
            emit("token set (paying mode from next rescan)");
        } else if (strncmp(line, "mint ", 5) == 0) {
            strlcpy(g_cfg.mint_url, line + 5, sizeof(g_cfg.mint_url));
            tg_store_save(&g_cfg);
            emit("mint set");
        } else if (strcmp(line, "go") == 0) {
            g_force_rescan = true;
        } else if (strcmp(line, "status") == 0) {
            cmd_status();
        } else if (strcmp(line, "erase") == 0) {
            tg_store_erase();
            memset(&g_cfg, 0, sizeof(g_cfg));
            emit("config erased");
        } else if (strcmp(line, "help") == 0) {
            print_banner();
        } else if (line[0] != 0) {
            emit("unknown command (try 'help')");
        }
    }
}
