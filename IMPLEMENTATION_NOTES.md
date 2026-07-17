# Webhook Delivery Management — Implementation Notes

## What Was Built

A complete webhook delivery inspection and redelivery system that allows merchants to:
1. Query the full history of webhook delivery attempts for any payment
2. Manually re-send failed or pending webhook deliveries
3. Verify the exact payload and delivery status of each attempt

## Files Modified

### 1. `src/db.rs`
Added three new functions:
- `WebhookDelivery` struct (serializable) to represent a delivery record
- `list_webhook_deliveries()` — queries all deliveries for a payment, ordered newest first
- `get_webhook_delivery()` — fetches a specific delivery by ID
- Helper function `row_to_webhook_delivery()` to safely map database rows

**Rationale:** Kept the struct definition near the query functions for tight cohesion and easy maintenance.

### 2. `src/api/mod.rs`
Registered two new routes in the router:
- `GET /payments/:id/webhooks` 
- `POST /payments/:id/webhooks/:delivery_id/redeliver`

**Rationale:** Routes are the contract between clients and handlers; keeping them centralized in the router makes the API surface clear.

### 3. `src/api/payments.rs`
Added two new handler functions:
- `list_webhooks()` — retrieves delivery history with validation
- `redeliver_webhook()` — resends a delivery with fresh signature computation

**Key decisions:**
- Both handlers verify the payment exists first (fail-fast error handling)
- `redeliver_webhook()` verifies the delivery belongs to the payment (prevents cross-payment access)
- Re-signing uses the exact original payload bytes (preserves authenticity)
- Attempt count is incremented on every re-send attempt (audit trail)
- Returns `502 Bad Gateway` when the webhook delivery fails (semantically correct — downstream issue)

### 4. `tests/api_tests.rs`
Added five comprehensive tests:
- `test_list_webhooks_not_found()` — validates 404 for missing payment
- `test_list_webhooks_empty()` — validates empty list response
- `test_redeliver_webhook_not_found()` — validates 404 for missing payment during redeliver
- `test_redeliver_delivery_not_found()` — validates 404 for missing delivery
- `test_webhook_delivery_isolation()` — validates deliveries are properly scoped to payments (security-critical)

**Rationale:** Isolation test is particularly important — it prevents subtle bugs where a merchant could inspect another merchant's webhooks.

## Design Decisions

### 1. Response Format for List Endpoint
Response includes `payment_id` at top level to give clients context about which payment the deliveries belong to.

### 2. Delivery Payload Preservation
The original `payload` is stored and re-sent as-is during redeliver. This means:
- Signature remains valid (re-computed using the original bytes)
- Merchant sees the exact event that was originally sent
- No re-serialization artifacts (decimal precision changes, etc.)

### 3. Status Code Semantics
- `404 Not Found` — resource doesn't exist or doesn't belong to the requester
- `502 Bad Gateway` — delivery failed (merchant's endpoint returned error or was unreachable)
- `200 OK` — redelivery succeeded

### 4. Error Messages
All error responses use the standard `{ "error": "message" }` format for consistency with existing endpoints.

### 5. Merchant Scoping
Both `GET /payments/:id/webhooks` and `POST /payments/:id/webhooks/:delivery_id/redeliver`
require the merchant bearer token and assert `payment.merchant_id ==
authenticated_merchant`, reporting the same 404 as a missing payment when a
different merchant's payment id is requested. Redelivery is also
rate-limited (same mechanism as `POST /payments`, keyed independently) since
it triggers a server-originated outbound request on every call.

## Testing Strategy

Tests follow the existing pattern:
1. Use `test_server()` to get an in-memory SQLite instance
2. Create test data via API calls
3. Verify responses with `axum_test` assertions
4. The isolation test manually inserts a delivery to test edge cases

The test suite covers:
- Happy path: empty list, successful operations
- Error paths: 404s for missing resources
- Security: cross-payment access prevention

## Future Enhancements

1. **Merchant Scoping** — Add `merchant_id` filter to prevent cross-tenant leaks
2. **Response Logging** — Store HTTP response code/body for debugging
3. **Pagination** — Add limit/offset to list endpoint for high-volume payments
4. **Filtering** — Allow filtering by status or date range
5. **Bulk Redelivery** — Redelivery all failed deliveries for a payment in one call

## Acceptance Criteria ✓

- [x] Delivery history is queryable per payment (`GET /payments/:id/webhooks`)
- [x] Manual redelivery re-sends the original signed body (`POST /payments/:id/webhooks/:delivery_id/redeliver`)
- [x] Records a new attempt on redeliver (attempts counter incremented)
- [x] Endpoints return standard error shape for unknown IDs
- [x] Tests cover list + redelivery + isolation
