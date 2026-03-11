# Extensibility

> Source: https://agentclientprotocol.com/protocol/extensibility

ACP provides three mechanisms for extending the protocol without breaking compatibility
with standard clients and agents.

---

## 1. The `_meta` Field

Every protocol type includes a `_meta` field. Implementations can use it to attach
custom information without modifying the core specification.

```json
{
  "sessionId": "sess_abc123",
  "_meta": {
    "clientVersion": "25.1.0",
    "traceparent": "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
  }
}
```

**Reserved keys** (W3C Trace Context interoperability):

| Key | Purpose |
|-----|---------|
| `traceparent` | Distributed tracing parent header |
| `tracestate` | Vendor-specific trace state |
| `baggage` | Propagated key-value pairs |

**Rule:** Implementations **MUST NOT** add custom fields at the **root level** of a spec
type. All extensions must go inside `_meta`.

---

## 2. Extension Methods

Custom methods use **underscore-prefixed names** with a namespace:

```
_<namespace>/<method>
```

Examples:
- `_zed.dev/workspace/buffers`
- `_anthropic.com/account/info`

### Custom Requests

Must include a JSON-RPC `id` field and expect a response. Recipients that don't
recognize the method should return the standard "Method not found" error:

```json
{
  "jsonrpc": "2.0",
  "id": 42,
  "error": {
    "code": -32601,
    "message": "Method not found"
  }
}
```

### Custom Notifications

Omit the `id` field. Recipients **SHOULD** silently ignore unrecognized notifications
rather than returning an error.

---

## 3. Advertising Custom Capabilities

Use the `_meta` field inside capability objects during `initialize` to advertise
extension support. This enables feature negotiation while remaining backward-compatible.

```json
{
  "agentCapabilities": {
    "loadSession": true,
    "_meta": {
      "_anthropic.com/account_info": true,
      "_anthropic.com/model_picker": true
    }
  }
}
```

Clients check these custom capability flags before calling the corresponding extension
methods.

---

## Helix Extension Example: `account/info`

Helix implements `account/info` as a custom ACP extension method. It returns the
authenticated user's account details.

Response type (from `helix-acp/src/types.rs`):

```rust
pub struct AccountInfo {
    pub email: Option<String>,
    pub name: Option<String>,
    pub account_uuid: Option<String>,
}
```

This is used by the Helix `whoami` command to display the current user's information.
