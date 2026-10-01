#pragma once
#include "esp_err.h"
#include <stdint.h>

#define TG_HTTP_MAX_RESPONSE 4096

// Minimal HTTP client for the TollGate v1 protocol.
// All requests go to http://<host>:<port><path> on the local network.

// GET a URL, return the body in buf. Returns HTTP status code, or -1 on error.
int tg_http_get(const char *host, int port, const char *path, char *buf, int buf_len);

// POST a raw body (Content-Type: text/plain). Returns HTTP status code or -1.
int tg_http_post(const char *host, int port, const char *path,
                 const char *body, int body_len, char *resp_buf, int resp_buf_len);
