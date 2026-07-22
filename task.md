# Task List

---

## 1. Switch TLS from native-tls to rustls

**Summary**
`sqlx` uses `runtime-tokio-native-tls` and `reqwest` pulls `native-tls`, tying builds to system OpenSSL (`libssl3` installed in the runtime image).

**Location**
- `Cargo.toml` lines 21, 23
- `Dockerfile` line 21 (`libssl3` install)

**Details & Impact**
`native-tls` complicates static/musl builds and adds a system dependency. Switching to `rustls` avoids the OpenSSL runtime requirement and shrinks the final image.

**Suggested Fix**
Switch to `rustls`-based feature flags for both `sqlx` and `reqwest`.

**Acceptance Criteria**
- TLS is handled via `rustls`
- No system OpenSSL required at runtime

---

## 2. Fail fast on invalid listener mode

**Summary**
An unrecognized `STELLAR_LISTENER_MODE` value logs a warning and silently defaults to `stream`.

**Location**
- `src/config.rs` lines 14–25

**Details & Impact**
An operator who intended `poll` but made a typo gets `stream` silently, opening an SSE connection they didn't want — surprising on constrained or proxied networks.

**Suggested Fix**
Fail fast on an invalid explicit value. An empty or unset variable may still default.

**Acceptance Criteria**
- A set-but-invalid listener mode aborts boot with a clear error

---

## 3. Reject placeholder secrets at boot

**Summary**
`.env.example` contains placeholder values like `STELLAR_GATEWAY_SECRET=SXXXX…` and `WEBHOOK_SECRET=your_webhook_signing_secret` that are easy to copy verbatim into production.

**Location**
- `.env.example` lines 9–13, 33

**Details & Impact**
Copy-paste deployments risk running with weak or placeholder secrets, especially if a default-secret fallback is also present.

**Suggested Fix**
Use obviously-invalid placeholders and add boot-time rejection of known placeholder values.

**Acceptance Criteria**
- Placeholder secrets are detected and rejected at boot with a clear error
