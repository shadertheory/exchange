# ↔️ Exchange

A minimal, configuration-driven reverse proxy in Rust, built with `hyper`, `reqwest`, and `tokio`.

It reads a `proxy.toml` file at startup to configure routes and server settings, forwarding incoming requests to the appropriate upstream services.

-----

## 🚀 Features

  * **Configuration-driven:** All routes and settings are managed in a simple `proxy.toml` file.
  * **Path-based Routing:** Forward requests based on the incoming request path.
  * **Wildcard Matching:** Use `wildcard = true` to match all sub-paths of a given pattern.
  * **Connection Pooling:** Uses `reqwest`'s connection pool for efficient upstream connections.
  * **Configurable Limits:** Set maximum request body size and gateway timeouts.

-----

## 🔧 Configuration

Create a `proxy.toml` file in the same directory where you run the application.

### Example `proxy.toml`

```toml
# The port the proxy server will listen on
port = 8080

# --- Optional Settings ---

# Max request body size in bytes (default: 1048576, or 1MB)
max_body_bytes = 2097152 # 2MB

# Upstream request timeout (default: 5s)
# Uses serde's Duration format
gateway_timeout = { secs = 10, nanos = 0 }

# --- Routes ---

# An exact-match route
# Requests to "http://localhost:8080/api/users"
# will be proxied to "http://users-service:3000/api/users"
[[routes]]
pattern = "/api/users"
target = "http://users-service:3000"
# wildcard = false (this is the default)

# A wildcard route
# Requests to "http://localhost:8080/assets/main.css"
# will be proxied to "http://static-files:9000/assets/main.css"
[[routes]]
pattern = "/assets"
target = "http://static-files:9000"
wildcard = true
```

### Configuration Details

  * **`port`** (u16, *Required*): The port for the proxy to listen on.
  * **`max_body_bytes`** (u64, *Optional*): The maximum allowed request body size in bytes.
      * *Default:* `1048576` (1MB)
  * **`gateway_timeout`** (Duration, *Optional*): The time to wait for a response from the upstream service.
      * *Default:* `{ secs = 5, nanos = 0 }` (5 seconds)
  * **`connection_pool_size`** (usize, *Optional*): Max idle connections per upstream host.
      * *Default:* 2 \* (number of system cores)
  * **`[[routes]]`** (Array, *Required*): A list of route objects.
      * **`pattern`** (string): The incoming URI path to match.
      * **`target`** (string): The base URI of the upstream service to forward to.
      * **`wildcard`** (bool, *Optional*): If `true`, matches any path that *starts with* the `pattern`. If `false` (default), requires an *exact* path match.

-----

## 🛠️ How It Works

Exchange listens for incoming requests and matches them against its list of routes.

1.  A **Client** sends a request (e.g., `GET /assets/img.png`).
2.  **Exchange** receives the request on `port`.
3.  It searches its routes for a match.
      * It finds the `[[routes]]` entry with `pattern = "/assets"` and `wildcard = true`.
4.  It constructs the full target URL: `http://static-files:9000` (target) + `/assets/img.png` (original path).
5.  It forwards the request, headers, and body to the **Upstream Service**.
6.  The service's response is streamed back to the original **Client**.

If no route matches, a `404 Not Found` is returned. If the incoming request body exceeds `max_body_bytes`, a `413 Payload Too Large` is returned.

-----

## 📦 Running the Proxy

1.  Ensure you have Rust and Cargo installed.

2.  Create your `proxy.toml` file in the project's root directory.

3.  Run the server in release mode:

    ```sh
    cargo run --release
    ```

4.  The proxy will start on the port specified in your config file.

    ```
    Proxy server running on http://0.0.0.0:8080
    Press Ctrl+C to stop
    ```
