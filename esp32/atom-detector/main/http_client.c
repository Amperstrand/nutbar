#include "http_client.h"
#include "esp_log.h"
#include "esp_http_client.h"
#include "esp_tls.h"

static const char *TAG = "tg_http";

typedef struct {
    char *buf;
    int buf_len;
    int bytes_read;
} tg_http_ctx_t;

static esp_err_t _handle_chunk(esp_http_client_event_t *evt) {
    tg_http_ctx_t *ctx = (tg_http_ctx_t *)evt->user_data;
    if (evt->event_id == HTTP_EVENT_ON_DATA && ctx->buf && ctx->bytes_read < ctx->buf_len - 1) {
        int copy = evt->data_len;
        if (ctx->bytes_read + copy > ctx->buf_len - 1)
            copy = ctx->buf_len - 1 - ctx->bytes_read;
        memcpy(ctx->buf + ctx->bytes_read, evt->data, copy);
        ctx->bytes_read += copy;
        ctx->buf[ctx->bytes_read] = 0;
    }
    return ESP_OK;
}

static int _do_request(esp_http_client_config_t *config, tg_http_ctx_t *ctx) {
    config->event_handler = _handle_chunk;
    esp_http_client_handle_t client = esp_http_client_init(config);
    if (!client) return -1;

    esp_err_t err = esp_http_client_perform(client);
    int status = esp_http_client_get_status_code(client);
    esp_http_client_cleanup(client);

    if (err != ESP_OK) {
        ESP_LOGE(TAG, "request failed: %s", esp_err_to_name(err));
        return -1;
    }
    return status;
}

int tg_http_get(const char *host, int port, const char *path, char *buf, int buf_len) {
    tg_http_ctx_t ctx = {.buf = buf, .buf_len = buf_len, .bytes_read = 0};
    buf[0] = 0;
    esp_http_client_config_t config = {
        .host = host,
        .port = port,
        .path = path,
        .method = HTTP_METHOD_GET,
        .timeout_ms = 10000,
        .user_data = &ctx,
        .disable_auto_redirect = false,
    };
    int status = _do_request(&config, &ctx);
    if (status > 0 && ctx.bytes_read > 0)
        ESP_LOGI(TAG, "GET %s:%d%s -> %d (%d bytes)", host, port, path, status, ctx.bytes_read);
    return status;
}

int tg_http_post(const char *host, int port, const char *path,
                 const char *body, int body_len, char *resp_buf, int resp_buf_len) {
    tg_http_ctx_t ctx = {.buf = resp_buf, .buf_len = resp_buf_len, .bytes_read = 0};
    if (resp_buf) resp_buf[0] = 0;
    esp_http_client_config_t config = {
        .host = host,
        .port = port,
        .path = path,
        .method = HTTP_METHOD_POST,
        .timeout_ms = 15000,
        .user_data = &ctx,
        .event_handler = _handle_chunk,
    };
    esp_http_client_handle_t client = esp_http_client_init(&config);
    if (!client) return -1;
    esp_http_client_set_header(client, "Content-Type", "text/plain");
    esp_http_client_set_post_field(client, body, body_len);

    esp_err_t err = esp_http_client_perform(client);
    int status = esp_http_client_get_status_code(client);
    esp_http_client_cleanup(client);

    if (err != ESP_OK) {
        ESP_LOGE(TAG, "POST failed: %s", esp_err_to_name(err));
        return -1;
    }
    ESP_LOGI(TAG, "POST %s:%d%s -> %d (%d bytes)", host, port, path, status, ctx.bytes_read);
    return status;
}
