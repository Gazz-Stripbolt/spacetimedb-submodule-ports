# auth: accounts, sign-in and sessions

Port of [`@spacetimedb/auth`](https://github.com/clockworklabs/SpacetimeDB/pull/5905) at
`37ee2ad627568f95df8229647cff5a482d629c25`. It needs the [crypto](../crypto) and [rate-limit](../rate-limit) ports
next to it (Rust: `crate::crypto`, `crate::rate_limit`; C#: `Crypto.cs` and `RateLimit.cs` in the same project), as the
TS package nests rate-limit.

**Schema:**
- **Tables:** `auth_user`, `auth_session`, `auth_account`, `auth_verification`, `auth_oauth_state`, `auth_config`,
  `auth_connection_binding`, `auth_admin_identity` (all private) and `auth_sweeper_tick` (every 60 s). The rate-limit
  tables come from the rate-limit port.
- **Reducers:** `set_auth_config`, `add_auth_admin`, `remove_auth_admin`, `auth_link_connection`,
  `auth_unlink_connection`, `auth_update_profile`, `auth_revoke_session`, `auth_revoke_my_session` and the scheduled
  `auth_sweep`.
- **Procedures:** `get_auth_public_key`, `auth_list_my_sessions`, `auth_whoami`.
- **View:** `my_auth_user`.
- **Renamed** to carry the package (TS mounts them under a namespace): `link_connection`, `unlink_connection`,
  `update_profile`, `revoke_session`, `revoke_my_session`, `list_my_sessions` and `whoami` get the `auth_` prefix.

**Same bytes as TS.** Given the same signing key, every value matches the TS package exactly
([`tests/vectors/auth.json`](../tests/vectors/auth.json), generated from the real code):
- **Keys:** PKCS#8/SPKI PEMs and the RFC 7638 key id.
- **Sessions:** ES256 JWTs (RFC 6979 nonces and low-S like noble, so signatures are byte-identical) and `verifyJwt`
  outcomes, including high-S and tampered tokens.
- **Passwords:** `scrypt$16384$8$1$salt$hash` strings.
- **Secrets:** the HMAC-derived secrets behind session ids, tokens, salts and UUIDv7 ids.
- **Helpers:** PKCE challenges, redirect checks and query parsing.

```rust
// set_auth_config(issuer_url, base_url, cookie_name, session_ttl_seconds, es256_private_key_pem, google…, github…)
// once as an admin, with a key from `openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256`.
const AUTH_HTTP: auth::AuthHttpOptions = auth::AuthHttpOptions { app_name: Some("Notes"), ..auth::AuthHttpOptions::DEFAULT };

#[handler]
fn auth_password_signup(ctx: &mut HandlerContext, req: Request) -> Response { auth::password_signup(ctx, req, &AUTH_HTTP) }
// ... then a #[router] with .post("/auth/password/signup", auth_password_signup), etc.

#[spacetimedb::reducer]
pub fn create_note(ctx: &ReducerContext, title: String) -> Result<(), String> {
    let user_id = auth::require_caller_user_id(ctx)?;   // the connection's bound session
    // ...
}
```

```csharp
static readonly AuthHttpOptions AuthHttp = new() { AppName = "Notes" };

[HttpHandler]
public static HttpResponse AuthPasswordSignupRoute(HandlerContext ctx, HttpRequest req) => AuthPasswordSignup(ctx, req, AuthHttp);

[Reducer]
public static void CreateNote(ReducerContext ctx, string title) { var userId = AuthRequireCallerUserId(ctx); /* ... */ }
```

| TypeScript | Rust | C# |
|---|---|---|
| `install(ctx)` | `auth::install(ctx)` | `AuthInstall(ctx)` |
| `client(options).passwordSignup` … `githubCallback` | `password_signup`, `password_login`, `me`, `refresh`, `logout`, `forgot_password`, `reset_password`, `email_verify_request`, `email_verify`, `google_start/_callback`, `github_start/_callback` (each `(ctx, req, &AuthHttpOptions)`) | `AuthPasswordSignup` … `AuthGithubCallback` (each `(ctx, req, AuthHttpOptions)`) |
| `oauthStart(provider)` / `oauthCallback(provider)` | `oauth_start(ctx, req, &opts, &OAuthProviderSpec)` / `oauth_callback` | `AuthOauthStart(ctx, req, opts, OAuthProviderSpec)` / `AuthOauthCallback` |
| `getCallerUserId` / `requireCallerUserId` / `findCallerUser` | same names, snake_case (`find_caller_user_in_view` for views) | `AuthGetCallerUserId` / `AuthRequireCallerUserId` / `AuthFindCallerUser` |
| `requestUserId(tx, req)` | `request_user_id(tx, &req)` | `AuthRequestUserId(AuthTx.Of(tx), req)` |
| `errors` | `auth::errors` | `AuthErrors` |

**Differences:**
- **Procedure results:** procedures return `Result<T, String>`; TS's throw a `SenderError`.
- **C# crypto:** C# on wasi has no `System.Security.Cryptography`, so P-256 ECDSA and scrypt are implemented in
  `Auth.cs` (managed `BigInteger` arithmetic). They match the vectors byte for byte but are **not constant-time**.
  Signing only ever uses the operator's key on the server; verification handles public data. Rust uses the RustCrypto
  `p256` and `scrypt` crates (`p256 = { version = "0.13", default-features = false, features = ["ecdsa", "arithmetic"] }`,
  `scrypt = { version = "0.11", default-features = false }`, plus `serde_json` and `http`).
- **OAuth providers:** providers are `OAuthProviderSpec` values. The GitHub resolver reads `/user` and `/user/emails`
  relative to the spec's user-info URL (the built-in spec points at `https://api.github.com/user`, as TS), so a test
  double can stand in for GitHub. In C#, `AuthGoogle`/`AuthGithub` are properties: a static field would race the
  host's own static initializers (their order across partial files is undefined).
- **Request bodies:** non-string `email`/`password`/`name` fields are `invalid_request`; TS throws a TypeError (a 500).
  Unparseable token or userinfo responses are 502s with the nearest TS code; TS throws.
- **Password hashes:** `verify_password` returns false for hashes whose scrypt parameters are invalid or need more than
  1 GiB; TS throws.
- **Scheduler-only:** `auth_sweep` rejects callers other than the scheduler.

**Tests:** every vector above plus the upstream cases (keys, JWT, crypto, request trust), and the HTTP and reducer
surface end to end: signup, login, sessions, binding, profile, refresh, logout, rate limits, email verification,
password reset, and the OAuth flows (PKCE, state bound to the browser, account linking only between verified
addresses). A mock provider stands in for Google and GitHub. See [`tests/auth.test.mjs`](../tests/auth.test.mjs).
Not covered: real Google/GitHub sign-ins and the 60 s expiry sweep.
